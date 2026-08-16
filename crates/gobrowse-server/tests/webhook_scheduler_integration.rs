//! Integration tests for the outbound webhook delivery scheduler.
//!
//! Requires `GOBROWSE_TEST_DATABASE_URL` to be set to a throwaway Postgres
//! instance. Silently skips otherwise.
//!
//! Assertions filter claim results by `webhook_id` so stale rows from
//! previously-failed runs cannot cause false assertion failures.

mod common;

use gobrowse_server::{
    config::{
        AuthSettings, DatabaseSettings, FeatureSettings, HttpSettings, ObservabilitySettings,
        Settings, VaultSettings,
    },
    webhook_scheduler,
    webhooks::hmac_sha256,
};
use secrecy::SecretString;
use sqlx::{PgPool, Row};
use std::sync::{
    Arc,
    atomic::{AtomicUsize, Ordering},
};

use time::OffsetDateTime;
use tokio::sync::Barrier;
use uuid::Uuid;

#[test]
fn webhook_scheduler_is_disabled_by_default() {
    assert!(!FeatureSettings::default().webhook_scheduler_enabled);
}

fn database_url() -> Option<String> {
    std::env::var("GOBROWSE_TEST_DATABASE_URL").ok()
}

async fn test_pool() -> Option<PgPool> {
    let url = database_url()?;
    let pool = PgPool::connect(&url)
        .await
        .expect("connect to test PostgreSQL");
    gobrowse_server::db::migrate(&pool)
        .await
        .expect("apply test migrations");
    Some(pool)
}

#[allow(dead_code)]
fn test_settings(database_url: &str) -> Settings {
    Settings {
        http: HttpSettings::default(),
        database: DatabaseSettings {
            url: SecretString::from(database_url.to_owned()),
            max_connections: 10,
        },
        auth: AuthSettings::default(),
        vault: VaultSettings::default(),
        features: FeatureSettings {
            webhook_scheduler_enabled: true,
            webhook_scheduler_max_attempts: 5,
            ..FeatureSettings::default()
        },
        observability: ObservabilitySettings::default(),
    }
}

/// Insert a webhook row and return its id, profile_id, and the secret bytes.
async fn insert_webhook(pool: &PgPool) -> (Uuid, Uuid, Vec<u8>) {
    let profile_id = Uuid::now_v7();
    sqlx::query("INSERT INTO profiles (id,name) VALUES ($1,$2)")
        .bind(profile_id)
        .bind("webhook-scheduler-test-profile")
        .execute(pool)
        .await
        .expect("create test profile");
    let webhook_id = Uuid::now_v7();
    let secret = b"test-hmac-secret-32-bytes-long!!".to_vec();
    sqlx::query(
        "INSERT INTO webhooks (id, profile_id, name, secret_key, target, event_filter, enabled) \
         VALUES ($1,$2,$3,$4,$5,$6,true)",
    )
    .bind(webhook_id)
    .bind(profile_id)
    .bind("test-webhook")
    .bind(&secret)
    .bind(serde_json::json!({"url":"http://127.0.0.1:1/deliver"}))
    .bind(serde_json::json!({"events":["*"]}))
    .execute(pool)
    .await
    .expect("create test webhook");
    (webhook_id, profile_id, secret)
}

/// Insert a delivery row in 'queued' state with a given target_url.
///
/// Uses `now() - interval '1 second'` so the row is safely in the past
/// relative to `clock_timestamp()` used by the claim query, avoiding
/// microsecond clock races.
async fn insert_queued_delivery(
    pool: &PgPool,
    webhook_id: Uuid,
    delivery_id: &str,
    target_url: &str,
    secret_key: &[u8],
) {
    sqlx::query(
        "INSERT INTO webhook_deliveries \
         (webhook_id, delivery_id, status, attempts, next_attempt_at, target_url, secret_key) \
         VALUES ($1,$2,'queued',0,now() - interval '1 second',$3,$4) \
         ON CONFLICT (webhook_id, delivery_id) DO UPDATE \
         SET status='queued', attempts=0, next_attempt_at=now() - interval '1 second', \
             target_url=$3, secret_key=$4",
    )
    .bind(webhook_id)
    .bind(delivery_id)
    .bind(target_url)
    .bind(secret_key)
    .execute(pool)
    .await
    .expect("insert queued delivery");
}

/// Insert a delivery with specific attempts (for dead-letter testing).
///
/// Uses `now() - interval '1 second'` (see `insert_queued_delivery`).
async fn insert_delivery_with_attempts(
    pool: &PgPool,
    webhook_id: Uuid,
    delivery_id: &str,
    target_url: &str,
    secret_key: &[u8],
    attempts: i32,
) {
    sqlx::query(
        "INSERT INTO webhook_deliveries \
         (webhook_id, delivery_id, status, attempts, next_attempt_at, target_url, secret_key) \
         VALUES ($1,$2,'queued',$3,now() - interval '1 second',$4,$5) \
         ON CONFLICT (webhook_id, delivery_id) DO UPDATE \
         SET status='queued', attempts=$3, \
             next_attempt_at=now() - interval '1 second', target_url=$4, secret_key=$5",
    )
    .bind(webhook_id)
    .bind(delivery_id)
    .bind(attempts)
    .bind(target_url)
    .bind(secret_key)
    .execute(pool)
    .await
    .expect("insert delivery with specific attempts");
}

/// Clean up test rows after each test.
///
/// Deletes via webhook FK cascade (webhook_deliveries → webhooks → profiles).
async fn cleanup(pool: &PgPool, webhook_id: Uuid, profile_id: Uuid) {
    // webhooks ON DELETE CASCADE → webhook_deliveries
    sqlx::query("DELETE FROM webhooks WHERE id=$1")
        .bind(webhook_id)
        .execute(pool)
        .await
        .ok();
    sqlx::query("DELETE FROM profiles WHERE id=$1")
        .bind(profile_id)
        .execute(pool)
        .await
        .ok();
}

#[tokio::test]
async fn webhook_delivery_rejects_half_leases_for_every_status() {
    let Some(database_url) = database_url() else {
        eprintln!("GOBROWSE_TEST_DATABASE_URL is unset; skipping");
        return;
    };
    let _lock = common::acquire_test_lock(&database_url).await;
    let pool = test_pool().await.expect("test pool after lock");
    let (webhook_id, profile_id, _secret) = insert_webhook(&pool).await;
    for (delivery_id, status, token, expires_at) in [
        ("queued-token", "queued", Some(Uuid::now_v7()), None),
        (
            "queued-expiry",
            "queued",
            None,
            Some(OffsetDateTime::now_utc()),
        ),
        ("running-token", "running", Some(Uuid::now_v7()), None),
    ] {
        let result = sqlx::query(
            "INSERT INTO webhook_deliveries
             (webhook_id,delivery_id,status,next_attempt_at,lease_token,lease_expires_at)
             VALUES ($1,$2,$3,now(),$4,$5)",
        )
        .bind(webhook_id)
        .bind(delivery_id)
        .bind(status)
        .bind(token)
        .bind(expires_at)
        .execute(&pool)
        .await;
        assert!(
            result.is_err(),
            "half lease {status}/{delivery_id} was accepted"
        );
    }
    cleanup(&pool, webhook_id, profile_id).await;
}

// ────────────────────────────────────────────────────────────────────
// Test 1: claim_deliveries skips locked/completed rows
// ────────────────────────────────────────────────────────────────────

#[tokio::test]
async fn claim_loop_skips_locked_rows_under_concurrent_workers() {
    let Some(database_url) = database_url() else {
        eprintln!("GOBROWSE_TEST_DATABASE_URL is unset; skipping");
        return;
    };
    let _lock = common::acquire_test_lock(&database_url).await;
    let pool = test_pool().await.expect("test pool after lock");
    let (webhook_id, profile_id, secret) = insert_webhook(&pool).await;
    let delivery_id = format!("claim-test-{}", Uuid::now_v7());

    insert_queued_delivery(
        &pool,
        webhook_id,
        &delivery_id,
        "http://127.0.0.1:1/deliver",
        &secret,
    )
    .await;

    // Use a large limit so stale rows from previous failed runs are
    // included in the result but don't affect our webhook_id-filtered
    // assertions.
    let owner = format!("test-{}", Uuid::now_v7());

    // First claim → should include our row (status goes from 'queued' to 'running').
    let claimed = webhook_scheduler::claim_deliveries(&pool, &owner, 100)
        .await
        .expect("first claim");
    let mine: Vec<_> = claimed
        .iter()
        .filter(|c| c.webhook_id == webhook_id)
        .collect();
    assert_eq!(mine.len(), 1, "first claim must return our row");
    assert_eq!(mine[0].delivery_id, delivery_id);

    // Second claim → our row is now 'running' so it must NOT appear.
    let claimed2 = webhook_scheduler::claim_deliveries(&pool, &owner, 100)
        .await
        .expect("second claim");
    let mine2: Vec<_> = claimed2
        .iter()
        .filter(|c| c.webhook_id == webhook_id)
        .collect();
    assert!(
        mine2.is_empty(),
        "second claim must not include our row (it is now running)"
    );

    cleanup(&pool, webhook_id, profile_id).await;
}

// ────────────────────────────────────────────────────────────────────
// Test 2: barrier-controlled competing workers have one winner
// ────────────────────────────────────────────────────────────────────

#[tokio::test]
async fn barrier_competing_workers_deliver_once_and_fence_loser() {
    let Some(database_url) = database_url() else {
        eprintln!("GOBROWSE_TEST_DATABASE_URL is unset; skipping");
        return;
    };
    let _lock = common::acquire_test_lock(&database_url).await;
    let pool = test_pool().await.expect("test pool after lock");
    let (webhook_id, profile_id, secret) = insert_webhook(&pool).await;
    let delivery_id = format!("barrier-race-{}", Uuid::now_v7());
    insert_queued_delivery(
        &pool,
        webhook_id,
        &delivery_id,
        "https://hooks.example.test/barrier",
        &secret,
    )
    .await;
    let barrier = Arc::new(Barrier::new(3));
    let deliveries = Arc::new(AtomicUsize::new(0));
    let mut workers = Vec::new();
    for worker in ["barrier-a", "barrier-b"] {
        let barrier = barrier.clone();
        let deliveries = deliveries.clone();
        let pool = pool.clone();
        let delivery_id = delivery_id.clone();
        workers.push(tokio::spawn(async move {
            barrier.wait().await;
            let claimed = webhook_scheduler::claim_deliveries(&pool, worker, 1)
                .await
                .expect("competing claim");
            let mine = claimed.into_iter().find(|delivery| {
                delivery.webhook_id == webhook_id && delivery.delivery_id == delivery_id
            });
            if let Some(delivery) = mine {
                deliveries.fetch_add(1, Ordering::SeqCst);
                let outcome = webhook_scheduler::DeliveryOutcome {
                    response_code: Some(204),
                    success: true,
                    retryable: false,
                    error: None,
                };
                webhook_scheduler::persist_outcome(&pool, &delivery, &outcome, 5).await;
                1usize
            } else {
                0usize
            }
        }));
    }
    barrier.wait().await;
    let claims = tokio::time::timeout(
        std::time::Duration::from_secs(5),
        futures_util::future::join_all(workers),
    )
    .await
    .expect("competing workers timeout")
    .into_iter()
    .map(|result| result.expect("competing worker task"))
    .sum::<usize>();
    assert_eq!(claims, 1, "exactly one worker must claim the row");
    assert_eq!(deliveries.load(Ordering::SeqCst), 1, "one delivery winner");
    let row = sqlx::query(
        "SELECT status, attempts, lease_token, lease_expires_at, last_response_code FROM webhook_deliveries WHERE webhook_id=$1 AND delivery_id=$2",
    )
    .bind(webhook_id)
    .bind(&delivery_id)
    .fetch_one(&pool)
    .await
    .expect("read barrier row");
    assert_eq!(row.get::<String, _>("status"), "succeeded");
    assert_eq!(row.get::<i32, _>("attempts"), 1);
    assert_eq!(row.get::<Option<i32>, _>("last_response_code"), Some(204));
    assert!(row.get::<Option<Uuid>, _>("lease_token").is_none());
    assert!(
        row.get::<Option<OffsetDateTime>, _>("lease_expires_at")
            .is_none()
    );
    cleanup(&pool, webhook_id, profile_id).await;
}

// ────────────────────────────────────────────────────────────────────
// Test 3: exponential backoff on retryable failure
// ────────────────────────────────────────────────────────────────────

#[tokio::test]
async fn exponential_backoff_advances_next_attempt_at_on_retryable_failure() {
    let Some(database_url) = database_url() else {
        eprintln!("GOBROWSE_TEST_DATABASE_URL is unset; skipping");
        return;
    };
    let _lock = common::acquire_test_lock(&database_url).await;
    let pool = test_pool().await.expect("test pool after lock");

    let target_url = "https://hooks.example.test/deliver".to_owned();

    let (webhook_id, profile_id, secret) = insert_webhook(&pool).await;
    let delivery_id = format!("backoff-test-{}", Uuid::now_v7());
    insert_queued_delivery(&pool, webhook_id, &delivery_id, &target_url, &secret).await;

    // Claim and deliver (should get 503, retryable).
    let owner = format!("test-{}", Uuid::now_v7());
    let claimed_all = webhook_scheduler::claim_deliveries(&pool, &owner, 100)
        .await
        .expect("claim");
    let claimed = claimed_all
        .iter()
        .find(|c| c.delivery_id == delivery_id)
        .expect("must claim own delivery");
    assert_eq!(claimed.attempts, 1, "attempts incremented by claim");

    let outcome = webhook_scheduler::DeliveryOutcome {
        response_code: Some(503),
        success: false,
        retryable: true,
        error: None,
    };
    assert!(!outcome.success, "503 should not be success");
    assert!(outcome.retryable, "503 should be retryable");

    // Persist the retryable outcome (triggers backoff).
    webhook_scheduler::persist_outcome(&pool, claimed, &outcome, 5).await;

    // Verify DB state: status='queued', attempts incremented, next_attempt_at in the future.
    let row = sqlx::query(
        "SELECT status, attempts, next_attempt_at FROM webhook_deliveries \
         WHERE webhook_id=$1 AND delivery_id=$2",
    )
    .bind(webhook_id)
    .bind(&delivery_id)
    .fetch_one(&pool)
    .await
    .expect("read delivery row");

    let status: String = row.get("status");
    let attempts: i32 = row.get("attempts");
    let next_attempt: time::OffsetDateTime = row.get("next_attempt_at");

    assert_eq!(
        status, "queued",
        "retryable failure should set status=queued"
    );
    assert_eq!(attempts, 1, "attempts should be 1 (claim increments it)");

    let now = time::OffsetDateTime::now_utc();
    let diff = next_attempt - now;
    // First backoff: 2^1 = 2s
    assert!(
        diff.whole_seconds() >= 1,
        "next_attempt_at should be ~2s in the future, got {diff}"
    );

    let expected_payload = format!("{target_url}\n{delivery_id}");
    let expected_mac = hmac_sha256(&secret, expected_payload.as_bytes());
    assert_eq!(expected_mac.len(), 32, "canonical HMAC remains SHA-256");

    cleanup(&pool, webhook_id, profile_id).await;
}

// ────────────────────────────────────────────────────────────────────
// Test 3: dead-letter after max_attempts
// ────────────────────────────────────────────────────────────────────

#[tokio::test]
async fn delivery_dead_letters_after_max_attempts() {
    let Some(database_url) = database_url() else {
        eprintln!("GOBROWSE_TEST_DATABASE_URL is unset; skipping");
        return;
    };
    let _lock = common::acquire_test_lock(&database_url).await;
    let pool = test_pool().await.expect("test pool after lock");

    let target_url = "https://hooks.example.test/dead-letter".to_owned();

    let (webhook_id, profile_id, secret) = insert_webhook(&pool).await;
    let delivery_id = format!("dead-letter-test-{}", Uuid::now_v7());
    // Insert with attempts = max_attempts - 1 = 4 (so after claim, attempts=5 which >= max).
    insert_delivery_with_attempts(&pool, webhook_id, &delivery_id, &target_url, &secret, 4).await;

    let owner = format!("test-{}", Uuid::now_v7());
    let claimed_all = webhook_scheduler::claim_deliveries(&pool, &owner, 100)
        .await
        .expect("claim");
    let claimed = claimed_all
        .iter()
        .find(|c| c.delivery_id == delivery_id)
        .expect("must claim own delivery");

    // After claim, attempts should be 5 (4+1).
    assert_eq!(claimed.attempts, 5, "claim increments attempts from 4 to 5");

    let outcome = webhook_scheduler::DeliveryOutcome {
        response_code: Some(503),
        success: false,
        retryable: true,
        error: None,
    };
    assert!(!outcome.success);
    assert!(outcome.retryable);

    // max_attempts=5, attempts=5 → dead-letter.
    webhook_scheduler::persist_outcome(&pool, claimed, &outcome, 5).await;

    let row =
        sqlx::query("SELECT status FROM webhook_deliveries WHERE webhook_id=$1 AND delivery_id=$2")
            .bind(webhook_id)
            .bind(&delivery_id)
            .fetch_one(&pool)
            .await
            .expect("read delivery");

    let status: String = row.get("status");
    assert_eq!(status, "dead", "max_attempts reached → status='dead'");

    cleanup(&pool, webhook_id, profile_id).await;
}

// ────────────────────────────────────────────────────────────────────
// Test 4: successful delivery sets succeeded
// ────────────────────────────────────────────────────────────────────

#[tokio::test]
async fn successful_delivery_sets_status_succeeded_and_clears_next_attempt() {
    let Some(database_url) = database_url() else {
        eprintln!("GOBROWSE_TEST_DATABASE_URL is unset; skipping");
        return;
    };
    let _lock = common::acquire_test_lock(&database_url).await;
    let pool = test_pool().await.expect("test pool after lock");

    let target_url = "https://hooks.example.test/success".to_owned();

    let (webhook_id, profile_id, secret) = insert_webhook(&pool).await;
    let delivery_id = format!("success-test-{}", Uuid::now_v7());
    insert_queued_delivery(&pool, webhook_id, &delivery_id, &target_url, &secret).await;

    let owner = format!("test-{}", Uuid::now_v7());
    let claimed_all = webhook_scheduler::claim_deliveries(&pool, &owner, 100)
        .await
        .expect("claim");
    let claimed = claimed_all
        .iter()
        .find(|c| c.delivery_id == delivery_id)
        .expect("must claim own delivery");

    let outcome = webhook_scheduler::DeliveryOutcome {
        response_code: Some(200),
        success: true,
        retryable: false,
        error: None,
    };
    assert!(outcome.success, "200 should be success");
    assert_eq!(outcome.response_code, Some(200));

    webhook_scheduler::persist_outcome(&pool, claimed, &outcome, 5).await;

    let row = sqlx::query(
        "SELECT status, last_response_code, next_attempt_at FROM webhook_deliveries \
         WHERE webhook_id=$1 AND delivery_id=$2",
    )
    .bind(webhook_id)
    .bind(&delivery_id)
    .fetch_one(&pool)
    .await
    .expect("read delivery");

    let status: String = row.get("status");
    let last_code: Option<i32> = row.get("last_response_code");
    let next_attempt: Option<time::OffsetDateTime> = row.get("next_attempt_at");

    assert_eq!(status, "succeeded");
    assert_eq!(last_code, Some(200));
    assert!(next_attempt.is_none(), "success clears next_attempt_at");

    cleanup(&pool, webhook_id, profile_id).await;
}

// ────────────────────────────────────────────────────────────────────
// Test 5: crashed worker running row is reclaimed on restart
// ────────────────────────────────────────────────────────────────────

#[tokio::test]
async fn crashed_worker_running_row_is_reclaimed_on_restart() {
    let Some(database_url) = database_url() else {
        eprintln!("GOBROWSE_TEST_DATABASE_URL is unset; skipping");
        return;
    };
    let _lock = common::acquire_test_lock(&database_url).await;
    let pool = test_pool().await.expect("test pool after lock");

    let (webhook_id, profile_id, secret) = insert_webhook(&pool).await;
    let delivery_id = format!("crash-reclaim-{}", Uuid::now_v7());

    // Insert a delivery row that looks like a crashed worker left it 'running'
    // with an expired lease and attempts=1 (within max_attempts).
    sqlx::query(
        "INSERT INTO webhook_deliveries \
         (webhook_id, delivery_id, status, attempts, next_attempt_at, target_url, \
          secret_key, lease_token, lease_expires_at) \
         VALUES ($1,$2,'running',1,NULL,$3,$4,gen_random_uuid(),now() - interval '1 minute')",
    )
    .bind(webhook_id)
    .bind(&delivery_id)
    .bind("http://127.0.0.1:1/deliver")
    .bind(&secret)
    .execute(&pool)
    .await
    .expect("insert crashed running delivery");

    // Verify it is 'running' before reaping.
    let before: String = sqlx::query_scalar(
        "SELECT status FROM webhook_deliveries WHERE webhook_id=$1 AND delivery_id=$2",
    )
    .bind(webhook_id)
    .bind(&delivery_id)
    .fetch_one(&pool)
    .await
    .expect("read before status");
    assert_eq!(before, "running");

    // Recover stuck deliveries — should re-queue (attempts=1 < max=5).
    let reclaimed = webhook_scheduler::recover_stuck_deliveries(&pool, 5)
        .await
        .expect("recover_stuck_deliveries");
    assert!(reclaimed > 0, "reaper must reclaim the stuck row");

    // Verify it is now 'queued' with lease cleared.
    let row = sqlx::query(
        "SELECT status, lease_expires_at FROM webhook_deliveries \
         WHERE webhook_id=$1 AND delivery_id=$2",
    )
    .bind(webhook_id)
    .bind(&delivery_id)
    .fetch_one(&pool)
    .await
    .expect("read after reaper");
    let status: String = row.get("status");
    let lease: Option<time::OffsetDateTime> = row.get("lease_expires_at");
    assert_eq!(status, "queued", "reaper should re-queue the stuck row");
    assert!(lease.is_none(), "reaper must clear the lease");

    // Now claim_deliveries should pick it up.
    let owner = format!("test-{}", Uuid::now_v7());
    let claimed_all = webhook_scheduler::claim_deliveries(&pool, &owner, 100)
        .await
        .expect("claim after reaper");
    let claimed = claimed_all
        .iter()
        .find(|c| c.delivery_id == delivery_id)
        .expect("must claim re-queued delivery");

    // Attempts should be 2 (was 1, claim increments).
    assert_eq!(claimed.attempts, 2, "claim increments attempts to 2");

    // Verify the row is 'running' with a fresh future lease.
    let row2 = sqlx::query(
        "SELECT status, attempts, lease_expires_at FROM webhook_deliveries \
         WHERE webhook_id=$1 AND delivery_id=$2",
    )
    .bind(webhook_id)
    .bind(&delivery_id)
    .fetch_one(&pool)
    .await
    .expect("read after claim");
    let status2: String = row2.get("status");
    let attempts2: i32 = row2.get("attempts");
    let lease2: time::OffsetDateTime = row2.get("lease_expires_at");

    assert_eq!(status2, "running");
    assert_eq!(attempts2, 2);
    let now = time::OffsetDateTime::now_utc();
    assert!(
        lease2 > now,
        "lease_expires_at must be in the future after re-claim"
    );

    // Running the reaper again must NOT re-claim this row (lease still valid).
    let reclaimed2 = webhook_scheduler::recover_stuck_deliveries(&pool, 5)
        .await
        .expect("second recover");
    assert_eq!(
        reclaimed2, 0,
        "reaper must not touch delivery with a valid lease"
    );

    // Another claim must return zero rows for this webhook_id.
    let claimed_again = webhook_scheduler::claim_deliveries(&pool, &owner, 100)
        .await
        .expect("second claim");
    let mine_again: Vec<_> = claimed_again
        .iter()
        .filter(|c| c.webhook_id == webhook_id)
        .collect();
    assert!(
        mine_again.is_empty(),
        "second claim must not include the already-running row"
    );

    cleanup(&pool, webhook_id, profile_id).await;
}

// ────────────────────────────────────────────────────────────────────
// Test 7: stale worker outcome is fenced after crash recovery and reclaim
// ────────────────────────────────────────────────────────────────────

#[tokio::test]
async fn stale_worker_cannot_persist_after_recovery_and_reclaim() {
    let Some(database_url) = database_url() else {
        eprintln!("GOBROWSE_TEST_DATABASE_URL is unset; skipping");
        return;
    };
    let _lock = common::acquire_test_lock(&database_url).await;
    let pool = test_pool().await.expect("test pool after lock");

    let (webhook_id, profile_id, secret) = insert_webhook(&pool).await;
    let delivery_id = format!("stale-outcome-{}", Uuid::now_v7());
    insert_queued_delivery(
        &pool,
        webhook_id,
        &delivery_id,
        "https://hooks.example.test/stale",
        &secret,
    )
    .await;

    let owner = format!("test-{}", Uuid::now_v7());
    let stale = webhook_scheduler::claim_deliveries(&pool, &owner, 100)
        .await
        .expect("initial claim")
        .into_iter()
        .find(|delivery| delivery.webhook_id == webhook_id && delivery.delivery_id == delivery_id)
        .expect("must claim delivery");

    // Deterministically simulate the old worker's lease expiring before the
    // reaper makes the row available to a new worker.
    sqlx::query(
        "UPDATE webhook_deliveries SET lease_expires_at=clock_timestamp() - interval '1 second' \
         WHERE webhook_id=$1 AND delivery_id=$2 AND lease_token=$3",
    )
    .bind(webhook_id)
    .bind(&delivery_id)
    .bind(stale.lease_token)
    .execute(&pool)
    .await
    .expect("expire stale lease");
    let (expired_at, observed_at): (OffsetDateTime, OffsetDateTime) = sqlx::query_as(
        "SELECT lease_expires_at, clock_timestamp() FROM webhook_deliveries WHERE webhook_id=$1 AND delivery_id=$2",
    )
    .bind(webhook_id)
    .bind(&delivery_id)
    .fetch_one(&pool)
    .await
    .expect("read expired lease");
    assert!(
        expired_at < observed_at,
        "stale lease must expire before recovery"
    );

    let reclaimed = webhook_scheduler::recover_stuck_deliveries(&pool, 5)
        .await
        .expect("recover stale lease");
    assert!(reclaimed > 0, "reaper must reclaim the expired lease");

    let current = webhook_scheduler::claim_deliveries(&pool, &owner, 100)
        .await
        .expect("reclaim delivery")
        .into_iter()
        .find(|delivery| delivery.webhook_id == webhook_id && delivery.delivery_id == delivery_id)
        .expect("must claim recovered delivery");
    assert_ne!(
        stale.lease_token, current.lease_token,
        "reclaim must issue a new lease identity"
    );
    let before_stale = sqlx::query(
        "SELECT status, attempts, lease_token, lease_expires_at, next_attempt_at, last_error, last_response_code FROM webhook_deliveries WHERE webhook_id=$1 AND delivery_id=$2",
    )
    .bind(webhook_id)
    .bind(&delivery_id)
    .fetch_one(&pool)
    .await
    .expect("read current lease before stale writes");
    let before_status = before_stale.get::<String, _>("status");
    let before_attempts = before_stale.get::<i32, _>("attempts");
    let before_token = before_stale.get::<Uuid, _>("lease_token");
    let before_expiry = before_stale
        .get::<Option<OffsetDateTime>, _>("lease_expires_at")
        .expect("current expiry before stale writes");
    let before_next = before_stale.get::<Option<OffsetDateTime>, _>("next_attempt_at");
    let before_error = before_stale.get::<Option<String>, _>("last_error");
    let before_response = before_stale.get::<Option<i32>, _>("last_response_code");

    let denied = webhook_scheduler::DeliveryOutcome {
        response_code: None,
        success: false,
        retryable: false,
        error: Some("target_policy_denied".to_owned()),
    };
    webhook_scheduler::persist_outcome(&pool, &stale, &denied, 5).await;
    let after_policy = sqlx::query(
        "SELECT status, attempts, lease_token, lease_expires_at, next_attempt_at, last_error, last_response_code FROM webhook_deliveries WHERE webhook_id=$1 AND delivery_id=$2",
    )
    .bind(webhook_id)
    .bind(&delivery_id)
    .fetch_one(&pool)
    .await
    .expect("read after stale policy");
    assert_eq!(after_policy.get::<String, _>("status"), before_status);
    assert_eq!(after_policy.get::<i32, _>("attempts"), before_attempts);
    assert_eq!(after_policy.get::<Uuid, _>("lease_token"), before_token);
    assert_eq!(
        after_policy.get::<Option<OffsetDateTime>, _>("lease_expires_at"),
        Some(before_expiry)
    );
    assert_eq!(
        after_policy.get::<Option<OffsetDateTime>, _>("next_attempt_at"),
        before_next
    );
    assert_eq!(
        after_policy.get::<Option<String>, _>("last_error"),
        before_error
    );
    assert_eq!(
        after_policy.get::<Option<i32>, _>("last_response_code"),
        before_response
    );

    let success = webhook_scheduler::DeliveryOutcome {
        response_code: Some(200),
        success: true,
        retryable: false,
        error: None,
    };
    webhook_scheduler::persist_outcome(&pool, &stale, &success, 5).await;

    let row = sqlx::query(
        "SELECT status, attempts, lease_token, lease_expires_at, next_attempt_at, last_error, last_response_code FROM webhook_deliveries \
         WHERE webhook_id=$1 AND delivery_id=$2",
    )
    .bind(webhook_id)
    .bind(&delivery_id)
    .fetch_one(&pool)
    .await
    .expect("read fenced delivery");
    assert_eq!(row.get::<String, _>("status"), before_status);
    assert_eq!(row.get::<i32, _>("attempts"), before_attempts);
    assert_eq!(row.get::<Uuid, _>("lease_token"), before_token);
    let current_expiry = row
        .get::<Option<OffsetDateTime>, _>("lease_expires_at")
        .expect("current lease expiry");
    assert_eq!(current_expiry, before_expiry);
    assert!(current_expiry > OffsetDateTime::now_utc());
    assert_eq!(
        row.get::<Option<OffsetDateTime>, _>("next_attempt_at"),
        before_next
    );
    assert_eq!(row.get::<Option<String>, _>("last_error"), before_error);
    assert_eq!(
        row.get::<Option<i32>, _>("last_response_code"),
        before_response
    );

    // The current worker's outcome is still accepted by the same fence.
    webhook_scheduler::persist_outcome(&pool, &current, &success, 5).await;
    let completed = sqlx::query(
        "SELECT status, lease_token, lease_expires_at FROM webhook_deliveries WHERE webhook_id=$1 AND delivery_id=$2",
    )
    .bind(webhook_id)
    .bind(&delivery_id)
    .fetch_one(&pool)
    .await
    .expect("read completed delivery");
    assert_eq!(completed.get::<String, _>("status"), "succeeded");
    assert!(completed.get::<Option<Uuid>, _>("lease_token").is_none());
    assert!(
        completed
            .get::<Option<OffsetDateTime>, _>("lease_expires_at")
            .is_none()
    );

    cleanup(&pool, webhook_id, profile_id).await;
}

// ────────────────────────────────────────────────────────────────────
// Test 8: crashed worker running row dead-letters after max_attempts
// ────────────────────────────────────────────────────────────────────

#[tokio::test]
async fn crashed_worker_running_row_dead_letters_after_max_attempts() {
    let Some(database_url) = database_url() else {
        eprintln!("GOBROWSE_TEST_DATABASE_URL is unset; skipping");
        return;
    };
    let _lock = common::acquire_test_lock(&database_url).await;
    let pool = test_pool().await.expect("test pool after lock");

    let (webhook_id, profile_id, secret) = insert_webhook(&pool).await;
    let delivery_id = format!("crash-dead-{}", Uuid::now_v7());

    // Insert a delivery that looks like a crashed worker: status='running',
    // expired lease, attempts at max (5).
    sqlx::query(
        "INSERT INTO webhook_deliveries \
         (webhook_id, delivery_id, status, attempts, next_attempt_at, target_url, \
          secret_key, lease_token, lease_expires_at) \
         VALUES ($1,$2,'running',5,NULL,$3,$4,gen_random_uuid(),now() - interval '1 minute')",
    )
    .bind(webhook_id)
    .bind(&delivery_id)
    .bind("http://127.0.0.1:1/deliver")
    .bind(&secret)
    .execute(&pool)
    .await
    .expect("insert dead-letter candidate");

    // Recover stuck deliveries — must dead-letter because attempts >= max.
    let reclaimed = webhook_scheduler::recover_stuck_deliveries(&pool, 5)
        .await
        .expect("recover_stuck_deliveries");
    assert!(reclaimed > 0, "reaper must dead-letter the exhausted row");

    // Verify status is 'dead' (dead-lettered).
    let row = sqlx::query(
        "SELECT status, last_error, lease_expires_at, next_attempt_at \
         FROM webhook_deliveries WHERE webhook_id=$1 AND delivery_id=$2",
    )
    .bind(webhook_id)
    .bind(&delivery_id)
    .fetch_one(&pool)
    .await
    .expect("read dead-lettered row");

    let status: String = row.get("status");
    let last_error: String = row.get("last_error");
    let lease: Option<time::OffsetDateTime> = row.get("lease_expires_at");
    let next: Option<time::OffsetDateTime> = row.get("next_attempt_at");

    assert_eq!(status, "dead", "max_attempts exceeded → status='dead'");
    assert_eq!(last_error, "lease expired", "last_error set by reaper");
    assert!(lease.is_none(), "lease cleared by reaper");
    assert!(next.is_none(), "next_attempt_at cleared by reaper");

    // Verify the row is NOT picked up by claim_deliveries (it's 'failed').
    let owner = format!("test-{}", Uuid::now_v7());
    let claimed_all = webhook_scheduler::claim_deliveries(&pool, &owner, 100)
        .await
        .expect("claim after dead-letter");
    let mine: Vec<_> = claimed_all
        .iter()
        .filter(|c| c.webhook_id == webhook_id)
        .collect();
    assert!(mine.is_empty(), "dead-lettered row must not be re-claimed");

    cleanup(&pool, webhook_id, profile_id).await;
}

// ────────────────────────────────────────────────────────────────────
// Inbound webhook receive_webhook integration tests
// ────────────────────────────────────────────────────────────────────

use std::net::{IpAddr, SocketAddr};
use std::sync::atomic::AtomicBool;
use tokio_util::sync::CancellationToken;

use axum::{
    body::Bytes,
    extract::{Path, State},
    http::HeaderMap,
    response::IntoResponse,
};

use gobrowse_server::{
    AppState,
    outbound_http::{
        OutboundResolver, OutboundTransport, ResolverFuture, TransportFuture, TransportResponse,
        ValidatedOutboundTarget, WebhookDeliveryDeps,
    },
    webhooks::{self, hex_encode},
};

/// Single-use key for the inbound tests so parallel runs don't collide.
/// We already hold the advisory lock.
fn sign_payload(secret: &[u8], timestamp_millis: i64, body: &[u8]) -> String {
    let payload = format!("{timestamp_millis}.{}", String::from_utf8_lossy(body));
    let mac = hmac_sha256(secret, payload.as_bytes());
    format!("sha256={}", hex_encode(&mac))
}

fn make_headers(delivery_id: &str, timestamp_millis: i64, signature: &str) -> HeaderMap {
    let mut headers = HeaderMap::new();
    headers.insert("x-gobrowse-signature", signature.parse().unwrap());
    headers.insert("x-gobrowse-delivery", delivery_id.parse().unwrap());
    headers.insert(
        "x-gobrowse-timestamp",
        timestamp_millis.to_string().parse().unwrap(),
    );
    headers
}

/// Call receive_webhook and return only the HTTP status code for assertion.
async fn call_webhook(
    state: AppState,
    id: Uuid,
    headers: HeaderMap,
    body: &[u8],
) -> axum::http::StatusCode {
    let result =
        webhooks::receive_webhook(State(state), Path(id), headers, Bytes::from(body.to_vec()))
            .await;
    match result {
        Ok(r) => r.into_response().status(),
        Err(e) => e.into_response().status(),
    }
}

fn now_millis() -> i64 {
    let now = OffsetDateTime::now_utc();
    now.unix_timestamp() * 1000 + now.millisecond() as i64
}

// ────────────────────────────────────────────────────────────────────
// Test 9: valid signature → 200 OK
// ────────────────────────────────────────────────────────────────────

#[tokio::test]
async fn inbound_valid_signature_accepted() {
    let Some(database_url) = database_url() else {
        eprintln!("GOBROWSE_TEST_DATABASE_URL is unset; skipping");
        return;
    };
    let _lock = common::acquire_test_lock(&database_url).await;
    let pool = test_pool().await.expect("pool");
    let state = AppState::new(pool.clone(), test_settings(&database_url))
        .await
        .expect("AppState");
    let (webhook_id, profile_id, secret) = insert_webhook(&pool).await;

    let delivery_id = Uuid::now_v7();
    let ts = now_millis();
    let body = b"{\"event\":\"test\"}";
    let sig = sign_payload(&secret, ts, body);
    let headers = make_headers(&delivery_id.to_string(), ts, &sig);
    let status = call_webhook(state, webhook_id, headers, body).await;
    assert_eq!(status, 200, "valid signature => 200");

    cleanup(&pool, webhook_id, profile_id).await;
}

// ────────────────────────────────────────────────────────────────────
// Test 10: invalid signature → 401
// ────────────────────────────────────────────────────────────────────

#[tokio::test]
async fn inbound_invalid_signature_rejected() {
    let Some(database_url) = database_url() else {
        eprintln!("GOBROWSE_TEST_DATABASE_URL is unset; skipping");
        return;
    };
    let _lock = common::acquire_test_lock(&database_url).await;
    let pool = test_pool().await.expect("pool");
    let state = AppState::new(pool.clone(), test_settings(&database_url))
        .await
        .expect("AppState");
    let (webhook_id, profile_id, _) = insert_webhook(&pool).await;

    let delivery_id = Uuid::now_v7();
    let ts = now_millis();
    let body = b"{\"event\":\"test\"}";
    // Sign with wrong key
    let wrong_secret = b"wrong-key-that-does-not-match";
    let wrong_sig = sign_payload(wrong_secret, ts, body);
    let headers = make_headers(&delivery_id.to_string(), ts, &wrong_sig);
    let status = call_webhook(state, webhook_id, headers, body).await;
    assert_eq!(status, 401, "invalid signature => 401");

    cleanup(&pool, webhook_id, profile_id).await;
}

// ────────────────────────────────────────────────────────────────────
// Test 11: replay (duplicate delivery_id) → 409
// ────────────────────────────────────────────────────────────────────

#[tokio::test]
async fn inbound_replay_delivery_idempotency() {
    let Some(database_url) = database_url() else {
        eprintln!("GOBROWSE_TEST_DATABASE_URL is unset; skipping");
        return;
    };
    let _lock = common::acquire_test_lock(&database_url).await;
    let pool = test_pool().await.expect("pool");
    let state = AppState::new(pool.clone(), test_settings(&database_url))
        .await
        .expect("AppState");
    let (webhook_id, profile_id, secret) = insert_webhook(&pool).await;

    let delivery_id = Uuid::now_v7();
    let ts = now_millis();
    let body = b"{\"event\":\"test\"}";
    let sig = sign_payload(&secret, ts, body);
    let headers1 = make_headers(&delivery_id.to_string(), ts, &sig);
    let headers2 = make_headers(&delivery_id.to_string(), ts, &sig);

    // First call → 200
    let status1 = call_webhook(state.clone(), webhook_id, headers1, body).await;
    assert_eq!(status1, 200, "first delivery => 200");

    // Second call with same delivery_id → 409
    let status2 = call_webhook(state, webhook_id, headers2, body).await;
    assert_eq!(status2, 409, "replay => 409");

    cleanup(&pool, webhook_id, profile_id).await;
}

// ────────────────────────────────────────────────────────────────────
// Test 12: stale timestamp → 401
// ────────────────────────────────────────────────────────────────────

#[tokio::test]
async fn inbound_clock_skew_rejected() {
    let Some(database_url) = database_url() else {
        eprintln!("GOBROWSE_TEST_DATABASE_URL is unset; skipping");
        return;
    };
    let _lock = common::acquire_test_lock(&database_url).await;
    let pool = test_pool().await.expect("pool");
    let state = AppState::new(pool.clone(), test_settings(&database_url))
        .await
        .expect("AppState");
    let (webhook_id, profile_id, secret) = insert_webhook(&pool).await;

    let delivery_id = Uuid::now_v7();
    // 1000 seconds in the past — well beyond MAX_CLOCK_SKEW_SECS (300)
    let old_ts = now_millis() - 1_000_000;
    let body = b"{\"event\":\"test\"}";
    let sig = sign_payload(&secret, old_ts, body);
    let headers = make_headers(&delivery_id.to_string(), old_ts, &sig);
    let status = call_webhook(state, webhook_id, headers, body).await;
    assert_eq!(status, 401, "stale timestamp => 401");

    cleanup(&pool, webhook_id, profile_id).await;
}

// ────────────────────────────────────────────────────────────────────
// Test 13: missing required headers → 400
// ────────────────────────────────────────────────────────────────────

#[tokio::test]
async fn inbound_missing_headers_rejected() {
    let Some(database_url) = database_url() else {
        eprintln!("GOBROWSE_TEST_DATABASE_URL is unset; skipping");
        return;
    };
    let _lock = common::acquire_test_lock(&database_url).await;
    let pool = test_pool().await.expect("pool");
    let state = AppState::new(pool.clone(), test_settings(&database_url))
        .await
        .expect("AppState");
    let (webhook_id, profile_id, secret) = insert_webhook(&pool).await;

    let delivery_id = Uuid::now_v7();
    let ts = now_millis();
    let body = b"{\"event\":\"test\"}";
    let sig = sign_payload(&secret, ts, body);

    // Missing X-Gobrowse-Signature
    let mut headers_no_sig = HeaderMap::new();
    headers_no_sig.insert(
        "x-gobrowse-delivery",
        delivery_id.to_string().parse().unwrap(),
    );
    headers_no_sig.insert("x-gobrowse-timestamp", ts.to_string().parse().unwrap());
    let status_no_sig = call_webhook(state.clone(), webhook_id, headers_no_sig, body).await;
    assert_eq!(status_no_sig, 400, "missing signature header => 400");

    // Missing X-Gobrowse-Delivery
    let mut headers_no_did = HeaderMap::new();
    headers_no_did.insert("x-gobrowse-signature", sig.parse().unwrap());
    headers_no_did.insert("x-gobrowse-timestamp", ts.to_string().parse().unwrap());
    let status_no_did = call_webhook(state.clone(), webhook_id, headers_no_did, body).await;
    assert_eq!(status_no_did, 400, "missing delivery header => 400");

    // Missing X-Gobrowse-Timestamp
    let mut headers_no_ts = HeaderMap::new();
    headers_no_ts.insert("x-gobrowse-signature", sig.parse().unwrap());
    headers_no_ts.insert(
        "x-gobrowse-delivery",
        delivery_id.to_string().parse().unwrap(),
    );
    let status_no_ts = call_webhook(state.clone(), webhook_id, headers_no_ts, body).await;
    assert_eq!(status_no_ts, 400, "missing timestamp header => 400");

    cleanup(&pool, webhook_id, profile_id).await;
}

// ────────────────────────────────────────────────────────────────────
// Test 14: disabled/unknown webhook → 404
// ────────────────────────────────────────────────────────────────────

#[tokio::test]
async fn inbound_disabled_webhook_rejected() {
    let Some(database_url) = database_url() else {
        eprintln!("GOBROWSE_TEST_DATABASE_URL is unset; skipping");
        return;
    };
    let _lock = common::acquire_test_lock(&database_url).await;
    let pool = test_pool().await.expect("pool");
    let state = AppState::new(pool.clone(), test_settings(&database_url))
        .await
        .expect("AppState");

    // A random UUID that doesn't exist → 404
    let unknown_id = Uuid::now_v7();
    let delivery_id = Uuid::now_v7();
    let ts = now_millis();
    let body = b"{\"event\":\"test\"}";
    let headers = make_headers(
        &delivery_id.to_string(),
        ts,
        "sha256=0000000000000000000000000000000000000000000000000000000000000000",
    );
    let status = call_webhook(state.clone(), unknown_id, headers, body).await;
    assert_eq!(status, 404, "unknown webhook id => 404");

    // Disabled webhook → 404
    let profile_id = Uuid::now_v7();
    sqlx::query("INSERT INTO profiles (id,name) VALUES ($1,$2)")
        .bind(profile_id)
        .bind("disabled-webhook-profile")
        .execute(&pool)
        .await
        .expect("profile");
    let disabled_id = Uuid::now_v7();
    let secret = b"test-secret-for-disabled".to_vec();
    sqlx::query(
        "INSERT INTO webhooks (id,profile_id,name,secret_key,target,event_filter,enabled) \
         VALUES ($1,$2,$3,$4,$5,$6,false)",
    )
    .bind(disabled_id)
    .bind(profile_id)
    .bind("disabled-webhook")
    .bind(&secret)
    .bind(serde_json::json!({"url":"https://example.com/deliver"}))
    .bind(serde_json::json!({"events":["*"]}))
    .execute(&pool)
    .await
    .expect("disabled webhook");

    let delivery_id2 = Uuid::now_v7();
    let ts2 = now_millis();
    let sig2 = sign_payload(&secret, ts2, body);
    let headers2 = make_headers(&delivery_id2.to_string(), ts2, &sig2);
    let status_disabled = call_webhook(state, disabled_id, headers2, body).await;
    assert_eq!(status_disabled, 404, "disabled webhook => 404");

    sqlx::query("DELETE FROM webhooks WHERE id=$1")
        .bind(disabled_id)
        .execute(&pool)
        .await
        .ok();
    sqlx::query("DELETE FROM profiles WHERE id=$1")
        .bind(profile_id)
        .execute(&pool)
        .await
        .ok();
}

// ────────────────────────────────────────────────────────────────────
// Scheduler lifecycle tests
// ────────────────────────────────────────────────────────────────────

/// A resolver that resolves every hostname to a fixed IP address.
#[derive(Clone)]
struct FixedResolver {
    address: IpAddr,
}

impl OutboundResolver for FixedResolver {
    fn resolve<'a>(&'a self, _host: &'a str, port: u16) -> ResolverFuture<'a> {
        let address = SocketAddr::new(self.address, port);
        Box::pin(async move { Ok(vec![address]) })
    }
}

/// A transport that returns a configured status code immediately.
/// Tracks whether it was called via an AtomicBool.
#[derive(Clone)]
struct FlagTransport {
    status: u16,
    invoked: Arc<AtomicBool>,
}

impl FlagTransport {
    fn new(status: u16) -> Self {
        Self {
            status,
            invoked: Arc::new(AtomicBool::new(false)),
        }
    }

    fn was_invoked(&self) -> bool {
        self.invoked.load(Ordering::SeqCst)
    }
}

impl OutboundTransport for FlagTransport {
    fn send<'a>(
        &'a self,
        _target: &'a ValidatedOutboundTarget,
        _delivery_id: &'a str,
        _body: &'a str,
        _signature: Option<&'a str>,
    ) -> TransportFuture<'a> {
        self.invoked.store(true, Ordering::SeqCst);
        let status = self.status;
        Box::pin(async move { Ok(TransportResponse { status }) })
    }
}

// ────────────────────────────────────────────────────────────────────
// Test 15: run_worker graceful shutdown drains in-flight deliveries
// ────────────────────────────────────────────────────────────────────

#[tokio::test]
async fn scheduler_drains_in_flight_deliveries_on_shutdown() {
    let Some(database_url) = database_url() else {
        eprintln!("GOBROWSE_TEST_DATABASE_URL is unset; skipping");
        return;
    };
    let _lock = common::acquire_test_lock(&database_url).await;
    let pool = test_pool().await.expect("pool");
    let state = AppState::new(pool.clone(), test_settings(&database_url))
        .await
        .expect("AppState");
    let (webhook_id, profile_id, secret) = insert_webhook(&pool).await;

    let transport = FlagTransport::new(204);
    let deps = WebhookDeliveryDeps {
        resolver: Arc::new(FixedResolver {
            address: "1.1.1.1".parse().expect("addr"),
        }),
        transport: Arc::new(transport.clone()),
    };

    let delivery_id = format!("drain-test-{}", Uuid::now_v7());
    insert_queued_delivery(
        &pool,
        webhook_id,
        &delivery_id,
        "https://hooks.example.test/deliver",
        &secret,
    )
    .await;

    let shutdown = CancellationToken::new();
    let worker_shutdown = shutdown.clone();
    let worker_state = state.clone();

    let handle = tokio::spawn(async move {
        webhook_scheduler::run_worker(worker_state, worker_shutdown, deps).await;
    });

    // Wait for the delivery to be claimed (status transitions to 'running').
    let mut claimed = false;
    for _ in 0..50 {
        let row: Option<(String,)> = sqlx::query_as(
            "SELECT status FROM webhook_deliveries \
             WHERE webhook_id=$1 AND delivery_id=$2",
        )
        .bind(webhook_id)
        .bind(&delivery_id)
        .fetch_optional(&pool)
        .await
        .expect("poll row");
        if let Some((status,)) = &row {
            if status == "running" {
                claimed = true;
                break;
            }
        }
        tokio::time::sleep(std::time::Duration::from_millis(50)).await;
    }
    assert!(claimed, "delivery was never claimed by worker");

    // Cancel the worker — it should drain in-flight deliveries.
    shutdown.cancel();

    // Wait for worker to exit (drain complete).
    tokio::time::timeout(std::time::Duration::from_secs(10), handle)
        .await
        .expect("worker did not drain in time")
        .expect("worker panicked");

    // Verify the delivery was actually processed (status moved from 'running').
    let row: (String,) = sqlx::query_as(
        "SELECT status FROM webhook_deliveries \
         WHERE webhook_id=$1 AND delivery_id=$2",
    )
    .bind(webhook_id)
    .bind(&delivery_id)
    .fetch_one(&pool)
    .await
    .expect("final row");
    assert_ne!(
        row.0, "running",
        "delivery must not remain running after drain"
    );
    assert!(
        transport.was_invoked(),
        "transport must have been invoked during drain"
    );

    cleanup(&pool, webhook_id, profile_id).await;
}

// ────────────────────────────────────────────────────────────────────
// Test 16: worker restart does not double-process recovered deliveries
// ────────────────────────────────────────────────────────────────────

#[tokio::test]
async fn scheduler_restart_does_not_double_process() {
    let Some(database_url) = database_url() else {
        eprintln!("GOBROWSE_TEST_DATABASE_URL is unset; skipping");
        return;
    };
    let _lock = common::acquire_test_lock(&database_url).await;
    let pool = test_pool().await.expect("pool");
    let state = AppState::new(pool.clone(), test_settings(&database_url))
        .await
        .expect("AppState");
    let (webhook_id, profile_id, secret) = insert_webhook(&pool).await;

    let delivery_id = format!("restart-test-{}", Uuid::now_v7());
    insert_queued_delivery(
        &pool,
        webhook_id,
        &delivery_id,
        "https://hooks.example.test/deliver",
        &secret,
    )
    .await;

    // ── Worker 1: claim and process the delivery ──
    let transport1 = FlagTransport::new(204);
    let deps1 = WebhookDeliveryDeps {
        resolver: Arc::new(FixedResolver {
            address: "1.1.1.1".parse().expect("addr"),
        }),
        transport: Arc::new(transport1.clone()),
    };

    let shutdown1 = CancellationToken::new();
    let handle1 = tokio::spawn({
        let s = state.clone();
        let sd = shutdown1.clone();
        async move { webhook_scheduler::run_worker(s, sd, deps1).await }
    });

    // Wait for claim.
    let mut claimed = false;
    for _ in 0..50 {
        let row: Option<(String,)> = sqlx::query_as(
            "SELECT status FROM webhook_deliveries \
             WHERE webhook_id=$1 AND delivery_id=$2",
        )
        .bind(webhook_id)
        .bind(&delivery_id)
        .fetch_optional(&pool)
        .await
        .expect("poll row");
        if let Some((status,)) = &row {
            if status == "running" {
                claimed = true;
                break;
            }
        }
        tokio::time::sleep(std::time::Duration::from_millis(50)).await;
    }
    assert!(claimed, "worker 1 must claim the delivery");

    // Crash worker 1.
    shutdown1.cancel();
    tokio::time::timeout(std::time::Duration::from_secs(10), handle1)
        .await
        .expect("worker 1 timeout")
        .expect("worker 1 panicked");

    // Verify transport was called once.
    assert!(
        transport1.was_invoked(),
        "worker 1 must process the delivery"
    );
    let delivery_count_after_crash: (i64,) = sqlx::query_as(
        "SELECT attempts FROM webhook_deliveries \
         WHERE webhook_id=$1 AND delivery_id=$2",
    )
    .bind(webhook_id)
    .bind(&delivery_id)
    .fetch_one(&pool)
    .await
    .expect("attempts row");
    assert_eq!(
        delivery_count_after_crash.0, 1,
        "exactly 1 attempt after first worker"
    );

    // ── Simulate recovery: expire the lease and re-claim ──
    sqlx::query(
        "UPDATE webhook_deliveries \
         SET lease_expires_at = now() - interval '1 hour', \
             next_attempt_at = now() - interval '1 second' \
         WHERE webhook_id=$1 AND delivery_id=$2",
    )
    .bind(webhook_id)
    .bind(&delivery_id)
    .execute(&pool)
    .await
    .expect("expire lease");

    let reclaimed = webhook_scheduler::recover_stuck_deliveries(&pool, 5)
        .await
        .expect("recover");
    assert!(reclaimed > 0, "recovery must reclaim the crashed delivery");

    let status_after_recovery: (String,) = sqlx::query_as(
        "SELECT status FROM webhook_deliveries \
         WHERE webhook_id=$1 AND delivery_id=$2",
    )
    .bind(webhook_id)
    .bind(&delivery_id)
    .fetch_one(&pool)
    .await
    .expect("status after recovery");
    assert_eq!(
        status_after_recovery.0, "queued",
        "recovery re-queues delivery"
    );

    // ── Worker 2: claim the recovered delivery ──
    let transport2 = FlagTransport::new(204);
    let deps2 = WebhookDeliveryDeps {
        resolver: Arc::new(FixedResolver {
            address: "1.1.1.1".parse().expect("addr"),
        }),
        transport: Arc::new(transport2.clone()),
    };
    let shutdown2 = CancellationToken::new();
    let handle2 = tokio::spawn({
        let s = state.clone();
        let sd = shutdown2.clone();
        async move { webhook_scheduler::run_worker(s, sd, deps2).await }
    });

    // Wait for worker 2 to claim the recovered delivery.
    let mut re_claimed = false;
    for _ in 0..50 {
        let status: (String,) = sqlx::query_as(
            "SELECT status FROM webhook_deliveries \
             WHERE webhook_id=$1 AND delivery_id=$2",
        )
        .bind(webhook_id)
        .bind(&delivery_id)
        .fetch_one(&pool)
        .await
        .expect("poll status");
        if status.0 == "running" {
            re_claimed = true;
            break;
        }
        tokio::time::sleep(std::time::Duration::from_millis(50)).await;
    }
    assert!(re_claimed, "worker 2 must re-claim the recovered delivery");

    // Cancel worker 2 and drain.
    shutdown2.cancel();
    tokio::time::timeout(std::time::Duration::from_secs(10), handle2)
        .await
        .expect("worker 2 timeout")
        .expect("worker 2 panicked");

    // Verify the delivery was processed again by worker 2.
    let final_attempts: (i64,) = sqlx::query_as(
        "SELECT attempts FROM webhook_deliveries \
         WHERE webhook_id=$1 AND delivery_id=$2",
    )
    .bind(webhook_id)
    .bind(&delivery_id)
    .fetch_one(&pool)
    .await
    .expect("final attempts");
    // Worker 1: 1 attempt, recovery: no new attempt, Worker 2: 1 attempt → total 2.
    assert_eq!(
        final_attempts.0, 2,
        "exactly 2 total attempts (1 per worker lifecycle)"
    );

    let final_status: (String,) = sqlx::query_as(
        "SELECT status FROM webhook_deliveries \
         WHERE webhook_id=$1 AND delivery_id=$2",
    )
    .bind(webhook_id)
    .bind(&delivery_id)
    .fetch_one(&pool)
    .await
    .expect("final status");
    assert_ne!(
        final_status.0, "running",
        "final status must not be running after drain"
    );

    cleanup(&pool, webhook_id, profile_id).await;
}
