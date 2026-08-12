mod common;

use axum::{
    Router,
    body::Body,
    http::{Method, Request, StatusCode, header},
};
use gobrowse_server::{
    AppState,
    config::{
        AuthSettings, DatabaseSettings, FeatureSettings, HttpSettings, ObservabilitySettings,
        Settings, VaultSettings,
    },
    db, router, run_api,
};
use http_body_util::BodyExt;
use secrecy::SecretString;
use serde_json::Value;
use sha2::{Digest, Sha256};
use sqlx::{PgPool, Row};
use time::{Duration, OffsetDateTime};
use tower::ServiceExt;
use uuid::Uuid;

/// Insert a run directly via SQL with a controlled execution token and lease expiry.
///
/// `lease_offset` is a SQL expression appended to `clock_timestamp()`, e.g.
/// `+ make_interval(hours => 1)` for a future lease or
/// `- make_interval(mins => 30)` for an expired lease.
///
/// Returns the run_id, profile_id, and a cookie for the user that owns (requested_by) the run.
async fn insert_run_with_lease(
    pool: &PgPool,
    token: Uuid,
    lease_offset: &str,
    state: &str,
    execution_owner: &str,
) -> (Uuid, Uuid, String) {
    let profile_id = Uuid::now_v7();
    sqlx::query("INSERT INTO profiles (id,name) VALUES ($1,$2)")
        .bind(profile_id)
        .bind(format!("takeover test profile {profile_id}"))
        .execute(pool)
        .await
        .expect("create test profile");
    let user_id = Uuid::now_v7();
    let session_token = format!("test-session-{}", Uuid::now_v7());
    sqlx::query(
        "INSERT INTO users (id,email,display_name,password_hash,role,primary_profile_id) \
         VALUES ($1,$2,'Test User','unused','OWNER',$3)",
    )
    .bind(user_id)
    .bind(format!("{user_id}@example.test"))
    .bind(profile_id)
    .execute(pool)
    .await
    .expect("create test user");
    let now = OffsetDateTime::now_utc();
    sqlx::query(
        "INSERT INTO sessions (token_hash,user_id,auth_epoch,expires_at,absolute_expires_at) VALUES ($1,$2,1,$3,$4)",
    )
    .bind(Sha256::digest(session_token.as_bytes()).to_vec())
    .bind(user_id)
    .bind(now + Duration::hours(1))
    .bind(now + Duration::hours(2))
    .execute(pool)
    .await
    .expect("create test session");
    let cookie = format!("gobrowse_session={session_token}");

    let conversation_id = Uuid::now_v7();
    sqlx::query(
        "INSERT INTO conversations (id,profile_id,workspace_id,title,created_by_user_id) \
         VALUES ($1,$2,NULL,$3,$4)",
    )
    .bind(conversation_id)
    .bind(profile_id)
    .bind(format!("takeover conv {conversation_id}"))
    .bind(user_id)
    .execute(pool)
    .await
    .expect("create test conversation");

    let run_id = Uuid::now_v7();
    let now_ts = OffsetDateTime::now_utc();
    // lease_offset is a SQL expression, not a bound parameter; interpolate
    // safely into the query string (callers provide hardcoded constants).
    let insert_sql = format!(
        "INSERT INTO agent_runs (id,profile_id,conversation_id,state,step,run_kind,requested_by, \
         execution_owner,execution_token,lease_expires_at,execution_attempts,created_at,updated_at) \
         VALUES ($1,$2,$3,$4,0,'conversation_turn',$5,$6,$7,clock_timestamp() {lease_offset},0,$8,$8)"
    );
    sqlx::query(&insert_sql)
        .bind(run_id)
        .bind(profile_id)
        .bind(conversation_id)
        .bind(state)
        .bind(user_id)
        .bind(execution_owner)
        .bind(token)
        .bind(now_ts)
        .execute(pool)
        .await
        .expect("create test agent run");

    (run_id, profile_id, cookie)
}

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
            local_embeddings: true,
            local_models: true,
            ..FeatureSettings::default()
        },
        observability: ObservabilitySettings::default(),
    }
}

async fn request_json(
    app: &Router,
    method: Method,
    uri: &str,
    cookie: &str,
    body: Option<Value>,
) -> (StatusCode, Value) {
    let is_state_change = matches!(
        method,
        Method::POST | Method::PUT | Method::PATCH | Method::DELETE
    );
    let mut builder = Request::builder()
        .method(method)
        .uri(uri)
        .header(header::COOKIE, cookie);
    if is_state_change {
        builder = builder.header(header::ORIGIN, "http://localhost:8080");
    }
    let body = if let Some(body) = body {
        builder = builder.header(header::CONTENT_TYPE, "application/json");
        Body::from(body.to_string())
    } else {
        Body::empty()
    };
    let response = app
        .clone()
        .oneshot(builder.body(body).expect("build request"))
        .await
        .expect("route request");
    let status = response.status();
    let bytes = response
        .into_body()
        .collect()
        .await
        .expect("collect response")
        .to_bytes();
    let value = if bytes.is_empty() {
        Value::Null
    } else {
        serde_json::from_slice(&bytes).expect("JSON response")
    };
    (status, value)
}

/// Clean up test data in reverse-FK order to avoid constraint violations.
async fn cleanup(pool: &PgPool, run_id: Uuid, user_id: Uuid, profile_id: Uuid) {
    let conversation_id: Option<Uuid> =
        sqlx::query_scalar("SELECT conversation_id FROM agent_runs WHERE id=$1")
            .bind(run_id)
            .fetch_optional(pool)
            .await
            .ok()
            .flatten();
    sqlx::query("DELETE FROM run_events WHERE run_id=$1")
        .bind(run_id)
        .execute(pool)
        .await
        .ok();
    sqlx::query("DELETE FROM agent_runs WHERE id=$1")
        .bind(run_id)
        .execute(pool)
        .await
        .ok();
    if let Some(cid) = conversation_id {
        sqlx::query("DELETE FROM books WHERE conversation_id=$1")
            .bind(cid)
            .execute(pool)
            .await
            .ok();
        sqlx::query("DELETE FROM conversations WHERE id=$1")
            .bind(cid)
            .execute(pool)
            .await
            .ok();
    }
    sqlx::query("DELETE FROM sessions WHERE user_id=$1")
        .bind(user_id)
        .execute(pool)
        .await
        .ok();
    sqlx::query("DELETE FROM users WHERE id=$1")
        .bind(user_id)
        .execute(pool)
        .await
        .ok();
    sqlx::query("DELETE FROM profiles WHERE id=$1")
        .bind(profile_id)
        .execute(pool)
        .await
        .ok();
}

// ── Test 1: cancel_run finalizes immediately when lease is already expired ─────

#[tokio::test]
async fn cancel_run_finalizes_when_lease_already_expired() {
    let Some(database_url) = std::env::var("GOBROWSE_TEST_DATABASE_URL").ok() else {
        eprintln!("GOBROWSE_TEST_DATABASE_URL is unset; skipping takeover fence test");
        return;
    };
    let _lock = common::acquire_test_lock(&database_url).await;
    let pool = PgPool::connect(&database_url)
        .await
        .expect("connect to test PostgreSQL");
    db::migrate(&pool).await.expect("apply migrations");

    let token = Uuid::now_v7();
    let (run_id, profile_id, cookie) = insert_run_with_lease(
        &pool,
        token,
        "- make_interval(mins => 30)",
        "awaiting_model",
        "test-worker",
    )
    .await;

    // Hold a FOR UPDATE lock on the run row to prevent parallel tests'
    // `claim_runs` calls (which use FOR UPDATE SKIP LOCKED) from claiming
    // this run.  The lock is released when the holding transaction is
    // committed, after the cancel request has reached its UPDATE statement.
    let mut lock_tx = pool.begin().await.expect("begin lock transaction");
    sqlx::query("SELECT id FROM agent_runs WHERE id=$1 FOR UPDATE")
        .bind(run_id)
        .fetch_one(&mut *lock_tx)
        .await
        .expect("acquire row lock");

    let state = AppState::new(pool.clone(), test_settings(&database_url))
        .await
        .expect("create app state");
    let app = router(state.clone());

    let app_clone = app.clone();
    let run_id_task = run_id;
    let cookie_task = cookie.clone();
    let cancel_task = tokio::spawn(async move {
        request_json(
            &app_clone,
            Method::POST,
            &format!("/api/v1/runs/{run_id_task}/cancel"),
            &cookie_task,
            None,
        )
        .await
    });

    // Give the cancel handler time to reach the UPDATE, which blocks
    // behind our FOR UPDATE row lock.
    tokio::time::sleep(std::time::Duration::from_millis(500)).await;

    // Release the lock — the cancel UPDATE proceeds, sees the expired
    // lease, and finalizes the run to 'canceled'.
    lock_tx.commit().await.expect("release row lock");

    let (status, _body) = cancel_task.await.expect("cancel task completion");
    assert_eq!(
        status,
        StatusCode::ACCEPTED,
        "cancel with expired lease accepted"
    );

    let row = sqlx::query(
        "SELECT state,execution_token,execution_owner,lease_expires_at,finished_at \
         FROM agent_runs WHERE id=$1",
    )
    .bind(run_id)
    .fetch_one(&pool)
    .await
    .expect("read run after cancel");
    assert_eq!(row.get::<String, _>("state"), "canceled");
    assert!(
        row.get::<Option<Uuid>, _>("execution_token").is_none(),
        "execution_token should be NULL after finalize"
    );
    assert!(row.get::<Option<String>, _>("execution_owner").is_none());
    assert!(
        row.get::<Option<OffsetDateTime>, _>("lease_expires_at")
            .is_none()
    );
    assert!(
        row.get::<Option<OffsetDateTime>, _>("finished_at")
            .is_some()
    );

    let user_id: Uuid = sqlx::query_scalar("SELECT requested_by FROM agent_runs WHERE id=$1")
        .bind(run_id)
        .fetch_one(&pool)
        .await
        .expect("read requested_by");
    cleanup(&pool, run_id, user_id, profile_id).await;
}

// ── Test 2: release_lease silently no-ops with a stale execution token ─────────

#[tokio::test]
async fn release_lease_rejects_stale_execution_token_after_takeover() {
    let Some(database_url) = std::env::var("GOBROWSE_TEST_DATABASE_URL").ok() else {
        eprintln!("GOBROWSE_TEST_DATABASE_URL is unsetting; skipping takeover fence test");
        return;
    };
    let _lock = common::acquire_test_lock(&database_url).await;
    let pool = PgPool::connect(&database_url)
        .await
        .expect("connect to test PostgreSQL");
    db::migrate(&pool).await.expect("apply migrations");

    let stale_token = Uuid::now_v7();
    let (run_id, profile_id, _cookie) = insert_run_with_lease(
        &pool,
        stale_token,
        "+ make_interval(hours => 1)",
        "running_tool",
        "original-worker",
    )
    .await;

    // Simulate takeover: another worker claimed the run with a new token.
    let takeover_token = Uuid::now_v7();
    sqlx::query(
        "UPDATE agent_runs SET execution_token=$1, execution_owner='takeover-worker', \
         lease_expires_at=clock_timestamp() + make_interval(hours => 2) WHERE id=$2",
    )
    .bind(takeover_token)
    .bind(run_id)
    .execute(&pool)
    .await
    .expect("simulate takeover");

    // release_lease with the stale token should be a silent no-op.
    run_api::test_release_lease(&pool, run_id, stale_token, profile_id)
        .await
        .expect("release_lease with stale token does not error");

    let row = sqlx::query(
        "SELECT execution_token,execution_owner,lease_expires_at \
         FROM agent_runs WHERE id=$1",
    )
    .bind(run_id)
    .fetch_one(&pool)
    .await
    .expect("read run after stale release_lease");
    assert_eq!(
        row.get::<Uuid, _>("execution_token"),
        takeover_token,
        "execution_token should still be the takeover token"
    );
    assert_eq!(
        row.get::<String, _>("execution_owner"),
        "takeover-worker",
        "execution_owner unchanged by stale release_lease"
    );
    assert!(
        row.get::<Option<OffsetDateTime>, _>("lease_expires_at")
            .is_some(),
        "lease_expires_at still set after stale release_lease"
    );

    let user_id: Uuid = sqlx::query_scalar("SELECT requested_by FROM agent_runs WHERE id=$1")
        .bind(run_id)
        .fetch_one(&pool)
        .await
        .expect("read requested_by");
    cleanup(&pool, run_id, user_id, profile_id).await;
}

// ── Test 3: fail_run errors and does NOT transition state with stale token ────

#[tokio::test]
async fn fail_run_rejects_stale_token_when_lease_renewed() {
    let Some(database_url) = std::env::var("GOBROWSE_TEST_DATABASE_URL").ok() else {
        eprintln!("GOBROWSE_TEST_DATABASE_URL is unsetting; skipping takeover fence test");
        return;
    };
    let _lock = common::acquire_test_lock(&database_url).await;
    let pool = PgPool::connect(&database_url)
        .await
        .expect("connect to test PostgreSQL");
    db::migrate(&pool).await.expect("apply migrations");

    let stale_token = Uuid::now_v7();
    let (run_id, profile_id, _cookie) = insert_run_with_lease(
        &pool,
        stale_token,
        "+ make_interval(hours => 1)",
        "running_tool",
        "original-worker",
    )
    .await;

    // Takeover: rotate token and extend lease.
    let takeover_token = Uuid::now_v7();
    sqlx::query(
        "UPDATE agent_runs SET execution_token=$1, execution_owner='takeover-worker', \
         lease_expires_at=clock_timestamp() + make_interval(hours => 2) WHERE id=$2",
    )
    .bind(takeover_token)
    .bind(run_id)
    .execute(&pool)
    .await
    .expect("simulate takeover");

    let result = run_api::test_fail_run(
        &pool,
        run_id,
        stale_token,
        profile_id,
        "test_failure",
        "stale token should not transition",
    )
    .await;
    assert!(result.is_err(), "fail_run should error with stale token");

    let row = sqlx::query(
        "SELECT state,error_code,execution_token \
         FROM agent_runs WHERE id=$1",
    )
    .bind(run_id)
    .fetch_one(&pool)
    .await
    .expect("read run after stale fail_run");
    assert_eq!(
        row.get::<String, _>("state"),
        "running_tool",
        "state should be unchanged"
    );
    assert!(
        row.get::<Option<String>, _>("error_code").is_none(),
        "error_code should not be set"
    );
    assert_eq!(
        row.get::<Uuid, _>("execution_token"),
        takeover_token,
        "execution_token should still be takeover token"
    );

    let user_id: Uuid = sqlx::query_scalar("SELECT requested_by FROM agent_runs WHERE id=$1")
        .bind(run_id)
        .fetch_one(&pool)
        .await
        .expect("read requested_by");
    cleanup(&pool, run_id, user_id, profile_id).await;
}

// ── Test 4: cancel_run returns 404 for an already-canceled run ─────────────────
//
//   The handler first tries to set cancellation_requested_at on runs whose
//   state NOT IN ('completed','failed','canceled').  For an already-canceled
//   run, that UPDATE matches zero rows.  The fallback query checks whether
//   cancellation_requested_at IS NOT NULL; since our row has it NULL,
//   we land at line 467 → AppError::NotFound (404).

#[tokio::test]
async fn cancel_run_is_idempotent_after_already_cancelled() {
    let Some(database_url) = std::env::var("GOBROWSE_TEST_DATABASE_URL").ok() else {
        eprintln!("GOBROWSE_TEST_DATABASE_URL is unsetting; skipping takeover fence test");
        return;
    };
    let _lock = common::acquire_test_lock(&database_url).await;
    let pool = PgPool::connect(&database_url)
        .await
        .expect("connect to test PostgreSQL");
    db::migrate(&pool).await.expect("apply migrations");

    // Build a fully finalized canceled row.
    let profile_id = Uuid::now_v7();
    sqlx::query("INSERT INTO profiles (id,name) VALUES ($1,$2)")
        .bind(profile_id)
        .bind(format!("canceled-idem profile {profile_id}"))
        .execute(&pool)
        .await
        .expect("create test profile");
    let user_id = Uuid::now_v7();
    let session_token = format!("test-session-{}", Uuid::now_v7());
    sqlx::query(
        "INSERT INTO users (id,email,display_name,password_hash,role,primary_profile_id) \
         VALUES ($1,$2,'Test User','unused','OWNER',$3)",
    )
    .bind(user_id)
    .bind(format!("{user_id}@example.test"))
    .bind(profile_id)
    .execute(&pool)
    .await
    .expect("create test user");
    let now = OffsetDateTime::now_utc();
    sqlx::query(
        "INSERT INTO sessions (token_hash,user_id,auth_epoch,expires_at,absolute_expires_at) \
         VALUES ($1,$2,1,$3,$4)",
    )
    .bind(Sha256::digest(session_token.as_bytes()).to_vec())
    .bind(user_id)
    .bind(now + Duration::hours(1))
    .bind(now + Duration::hours(2))
    .execute(&pool)
    .await
    .expect("create test session");
    let cookie = format!("gobrowse_session={session_token}");

    let conversation_id = Uuid::now_v7();
    sqlx::query(
        "INSERT INTO conversations (id,profile_id,workspace_id,title,created_by_user_id) \
         VALUES ($1,$2,NULL,$3,$4)",
    )
    .bind(conversation_id)
    .bind(profile_id)
    .bind(format!("canceled-idem conv {conversation_id}"))
    .bind(user_id)
    .execute(&pool)
    .await
    .expect("create test conversation");

    let run_id = Uuid::now_v7();
    let now_ts = OffsetDateTime::now_utc();
    sqlx::query(
        "INSERT INTO agent_runs (id,profile_id,conversation_id,state,step,run_kind,requested_by, \
         execution_owner,execution_token,lease_expires_at,execution_attempts,finished_at,created_at,updated_at) \
         VALUES ($1,$2,$3,'canceled',0,'conversation_turn',$4,NULL,NULL,NULL,0,$5,$6,$6)",
    )
    .bind(run_id)
    .bind(profile_id)
    .bind(conversation_id)
    .bind(user_id)
    .bind(now_ts)
    .bind(now_ts)
    .execute(&pool)
    .await
    .expect("create canceled run");

    // cancellation_requested_at is NULL; the handler will see NotFound.
    let state = AppState::new(pool.clone(), test_settings(&database_url))
        .await
        .expect("create app state");
    let app = router(state);

    let (status, _body) = request_json(
        &app,
        Method::POST,
        &format!("/api/v1/runs/{run_id}/cancel"),
        &cookie,
        None,
    )
    .await;
    assert_eq!(
        status,
        StatusCode::NOT_FOUND,
        "cancel on already-canceled run (NULL cancellation_requested_at) returns 404"
    );

    // Row unchanged.
    let row_state: String = sqlx::query_scalar("SELECT state FROM agent_runs WHERE id=$1")
        .bind(run_id)
        .fetch_one(&pool)
        .await
        .expect("read run state");
    assert_eq!(row_state, "canceled");

    cleanup(&pool, run_id, user_id, profile_id).await;
}

// ── Test 5: claim_runs does not re-claim a run held by a live lease ───────────
//
// NOTE: This test is intentionally omitted from the integration suite.
//
// `claim_runs` scans ALL `agent_runs` rows globally (no profile/shard
// filter) with `FOR UPDATE SKIP LOCKED`, so it cannot be tested in
// parallel with other integration suites that spawn `run_worker` tasks
// (e.g. `milestone3_integration`).  Those suites' workers call
// `claim_runs` internally, and a concurrent `test_claim_runs` call
// would race them — claiming rows that the other suite's worker was
// assigned to process, renewing the lease and rotating the token, which
// causes the original worker's subsequent fence checks to fail and the
// run to stall indefinitely.
//
// The post-test DELETE cleanup only removes rows after the damage is
// done (the row was already claimed away mid-flight from the other
// suite's worker), so per-test teardown cannot prevent this.
//
// Isolating `claim_runs` across suites would require a serializing lock
// touching ALL suites that create claimable `conversation_turn` runs,
// which is brittle, intrusive, and violates the instruction to avoid
// editing milestone3's test.
//
// The SKIP-LOCKED / stale-fence behaviour tested by this test is
// already covered:
//   • end-to-end by the milestone3 worker-driven happy path,
//   • by `one_scanner_wins_and_stale_fence_cannot_write` in `run_api.rs`.
//
// Tests 1–4 remain: they exercise `release_lease`, `fail_run`, and the
// cancel HTTP route — none of which scan rows globally.
