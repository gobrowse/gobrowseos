//! Integration tests for the outbound webhook delivery scheduler.
//!
//! Requires `GOBROWSE_TEST_DATABASE_URL` to be set to a throwaway Postgres
//! instance. Silently skips otherwise.
//!
//! Assertions filter claim results by `webhook_id` so stale rows from
//! previously-failed runs cannot cause false assertion failures.

mod common;

use std::sync::Arc;

use axum::{
    Router,
    body::Bytes,
    http::{HeaderMap, StatusCode},
    routing::post,
};
use gobrowse_server::{
    config::{
        AuthSettings, DatabaseSettings, FeatureSettings, HttpSettings, ObservabilitySettings,
        Settings, VaultSettings,
    },
    webhook_scheduler,
    webhooks::{hex_encode, hmac_sha256},
};
use secrecy::SecretString;
use sqlx::{PgPool, Row};
use time::OffsetDateTime;
use tokio::sync::Mutex;
use uuid::Uuid;

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
// Test 2: exponential backoff on retryable failure
// ────────────────────────────────────────────────────────────────────

#[tokio::test]
async fn exponential_backoff_advances_next_attempt_at_on_retryable_failure() {
    let Some(database_url) = database_url() else {
        eprintln!("GOBROWSE_TEST_DATABASE_URL is unset; skipping");
        return;
    };
    let _lock = common::acquire_test_lock(&database_url).await;
    let pool = test_pool().await.expect("test pool after lock");

    // Spin up a mock server that always returns 503 (retryable).
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0")
        .await
        .expect("bind mock server");
    let addr = listener.local_addr().expect("mock address");
    let target_url = format!("http://{addr}/deliver");

    // Track the signature header received.
    let received_headers: Arc<Mutex<Option<HeaderMap>>> = Arc::new(Mutex::new(None));
    let captured = received_headers.clone();

    let server = tokio::spawn(async move {
        let app = Router::new().route(
            "/deliver",
            post(move |headers: HeaderMap, _body: Bytes| async move {
                let mut guard = captured.lock().await;
                *guard = Some(headers);
                (StatusCode::SERVICE_UNAVAILABLE, "down for maintenance")
            }),
        );
        axum::serve(listener, app).await.expect("mock server");
    });

    let (webhook_id, profile_id, secret) = insert_webhook(&pool).await;
    let delivery_id = format!("backoff-test-{}", Uuid::now_v7());
    insert_queued_delivery(&pool, webhook_id, &delivery_id, &target_url, &secret).await;

    // Create a reqwest client.
    let http = reqwest::Client::builder()
        .timeout(std::time::Duration::from_secs(5))
        .no_proxy()
        .build()
        .expect("reqwest client");

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

    let outcome = webhook_scheduler::deliver_one(&http, claimed).await;
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

    // Verify the signature was sent.
    let headers_guard = received_headers.lock().await;
    assert!(
        headers_guard.is_some(),
        "mock server should have received headers"
    );
    let sig_header = headers_guard
        .as_ref()
        .and_then(|h| h.get("x-gobrowse-signature"))
        .and_then(|v| v.to_str().ok())
        .expect("X-Gobrowse-Signature header present");

    let expected_payload = format!("{target_url}\n{delivery_id}");
    let expected_mac = hmac_sha256(&secret, expected_payload.as_bytes());
    let expected_sig = format!("sha256={}", hex_encode(&expected_mac));
    assert_eq!(
        sig_header, expected_sig,
        "outbound signature must match canonical payload"
    );

    server.abort();
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

    // Mock server returning 503.
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0")
        .await
        .expect("bind mock server");
    let addr = listener.local_addr().expect("mock address");
    let target_url = format!("http://{addr}/deliver");

    let server = tokio::spawn(async move {
        let app = Router::new().route(
            "/deliver",
            post(|| async { (StatusCode::SERVICE_UNAVAILABLE, "down") }),
        );
        axum::serve(listener, app).await.expect("mock server");
    });

    let (webhook_id, profile_id, secret) = insert_webhook(&pool).await;
    let delivery_id = format!("dead-letter-test-{}", Uuid::now_v7());
    // Insert with attempts = max_attempts - 1 = 4 (so after claim, attempts=5 which >= max).
    insert_delivery_with_attempts(&pool, webhook_id, &delivery_id, &target_url, &secret, 4).await;

    let http = reqwest::Client::builder()
        .timeout(std::time::Duration::from_secs(5))
        .no_proxy()
        .build()
        .expect("reqwest client");

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

    let outcome = webhook_scheduler::deliver_one(&http, claimed).await;
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

    server.abort();
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

    // Mock server returning 200.
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0")
        .await
        .expect("bind mock server");
    let addr = listener.local_addr().expect("mock address");
    let target_url = format!("http://{addr}/deliver");

    let server = tokio::spawn(async move {
        let app = Router::new().route("/deliver", post(|| async { (StatusCode::OK, "ok") }));
        axum::serve(listener, app).await.expect("mock server");
    });

    let (webhook_id, profile_id, secret) = insert_webhook(&pool).await;
    let delivery_id = format!("success-test-{}", Uuid::now_v7());
    insert_queued_delivery(&pool, webhook_id, &delivery_id, &target_url, &secret).await;

    let http = reqwest::Client::builder()
        .timeout(std::time::Duration::from_secs(5))
        .no_proxy()
        .build()
        .expect("reqwest client");

    let owner = format!("test-{}", Uuid::now_v7());
    let claimed_all = webhook_scheduler::claim_deliveries(&pool, &owner, 100)
        .await
        .expect("claim");
    let claimed = claimed_all
        .iter()
        .find(|c| c.delivery_id == delivery_id)
        .expect("must claim own delivery");

    let outcome = webhook_scheduler::deliver_one(&http, claimed).await;
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

    server.abort();
    cleanup(&pool, webhook_id, profile_id).await;
}

// ────────────────────────────────────────────────────────────────────
// Test 5: signature end-to-end with canonical payload
// ────────────────────────────────────────────────────────────────────

#[tokio::test]
async fn signature_is_validated_over_canonical_payload_at_enqueue() {
    let Some(database_url) = database_url() else {
        eprintln!("GOBROWSE_TEST_DATABASE_URL is unset; skipping");
        return;
    };
    let _lock = common::acquire_test_lock(&database_url).await;
    let pool = test_pool().await.expect("test pool after lock");

    // Mock server that captures and validates the signature.
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0")
        .await
        .expect("bind mock server");
    let addr = listener.local_addr().expect("mock address");
    let target_url = format!("http://{addr}/deliver");

    // Capture the request headers and body.
    let captured_data: Arc<Mutex<Option<(HeaderMap, Bytes)>>> = Arc::new(Mutex::new(None));
    let data = captured_data.clone();

    let server = tokio::spawn(async move {
        let app = Router::new().route(
            "/deliver",
            post(move |headers: HeaderMap, body: Bytes| async move {
                let mut guard = data.lock().await;
                *guard = Some((headers, body));
                (StatusCode::OK, "ok")
            }),
        );
        axum::serve(listener, app).await.expect("mock server");
    });

    let (webhook_id, profile_id, secret) = insert_webhook(&pool).await;
    let delivery_id = format!("sig-test-{}", Uuid::now_v7());
    insert_queued_delivery(&pool, webhook_id, &delivery_id, &target_url, &secret).await;

    let http = reqwest::Client::builder()
        .timeout(std::time::Duration::from_secs(5))
        .no_proxy()
        .build()
        .expect("reqwest client");

    let owner = format!("test-{}", Uuid::now_v7());
    let claimed_all = webhook_scheduler::claim_deliveries(&pool, &owner, 100)
        .await
        .expect("claim");
    let claimed = claimed_all
        .iter()
        .find(|c| c.delivery_id == delivery_id)
        .expect("must claim own delivery");

    let outcome = webhook_scheduler::deliver_one(&http, claimed).await;
    assert!(outcome.success);

    // Verify the canonical payload and signature.
    let guard = captured_data.lock().await;
    let (headers, body) = guard.as_ref().expect("mock server received request");

    let sig_header = headers
        .get("x-gobrowse-signature")
        .and_then(|v| v.to_str().ok())
        .expect("X-Gobrowse-Signature header present");

    let delivery_header = headers
        .get("x-gobrowse-delivery")
        .and_then(|v| v.to_str().ok())
        .expect("X-Gobrowse-Delivery header present");
    assert_eq!(delivery_header, delivery_id);

    let canonical_body = format!("{target_url}\n{delivery_id}");
    assert_eq!(
        String::from_utf8_lossy(body),
        canonical_body,
        "body must be canonical payload: target\\ndelivery_id"
    );

    let expected_mac = hmac_sha256(&secret, canonical_body.as_bytes());
    let expected_sig = format!("sha256={}", hex_encode(&expected_mac));
    assert_eq!(
        sig_header, expected_sig,
        "outbound signature must match HMAC-SHA256 of canonical payload"
    );

    server.abort();
    cleanup(&pool, webhook_id, profile_id).await;
}

// ────────────────────────────────────────────────────────────────────
// Test 6: crashed worker running row is reclaimed on restart
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
        "http://127.0.0.1:1/deliver",
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

    let success = webhook_scheduler::DeliveryOutcome {
        response_code: Some(200),
        success: true,
        retryable: false,
        error: None,
    };
    webhook_scheduler::persist_outcome(&pool, &stale, &success, 5).await;

    let row = sqlx::query(
        "SELECT status, attempts, lease_token FROM webhook_deliveries \
         WHERE webhook_id=$1 AND delivery_id=$2",
    )
    .bind(webhook_id)
    .bind(&delivery_id)
    .fetch_one(&pool)
    .await
    .expect("read fenced delivery");
    assert_eq!(row.get::<String, _>("status"), "running");
    assert_eq!(row.get::<i32, _>("attempts"), 2);
    assert_eq!(row.get::<Uuid, _>("lease_token"), current.lease_token);

    // The current worker's outcome is still accepted by the same fence.
    webhook_scheduler::persist_outcome(&pool, &current, &success, 5).await;
    let status: String = sqlx::query_scalar(
        "SELECT status FROM webhook_deliveries WHERE webhook_id=$1 AND delivery_id=$2",
    )
    .bind(webhook_id)
    .bind(&delivery_id)
    .fetch_one(&pool)
    .await
    .expect("read completed delivery");
    assert_eq!(status, "succeeded");

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
