use axum::{
    Router,
    body::Body,
    http::{Method, Request, StatusCode},
};
use gobrowse_server::{
    AppState,
    config::{
        AuthSettings, DatabaseSettings, FeatureSettings, HttpSettings, ObservabilitySettings,
        Settings, VaultSettings,
    },
    db,
};
use http_body_util::BodyExt;
use secrecy::SecretString;
use serde_json::Value;
use sha2::Digest;
use sqlx::PgPool;
use tower::ServiceExt;
use uuid::Uuid;

// ---------------------------------------------------------------------------
// Helpers
// ---------------------------------------------------------------------------

async fn test_pool() -> Option<PgPool> {
    let url = std::env::var("GOBROWSE_TEST_DATABASE_URL").ok()?;
    let pool = PgPool::connect(&url)
        .await
        .expect("connect to test PostgreSQL");
    db::migrate(&pool).await.expect("apply test migrations");
    Some(pool)
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
        features: FeatureSettings::default(),
        observability: ObservabilitySettings::default(),
    }
}

fn hmac_sha256(key: &[u8], message: &[u8]) -> [u8; 32] {
    use sha2::Sha256;
    const BLOCK_SIZE: usize = 64;
    let mut key_block = [0u8; BLOCK_SIZE];

    if key.len() > BLOCK_SIZE {
        let mut h = Sha256::new();
        h.update(key);
        let hashed = h.finalize();
        key_block[..32].copy_from_slice(&hashed);
    } else {
        key_block[..key.len()].copy_from_slice(key);
    }

    let mut ipad_block = [0x36u8; BLOCK_SIZE];
    for i in 0..BLOCK_SIZE {
        ipad_block[i] ^= key_block[i];
    }
    let mut inner = Sha256::new();
    inner.update(ipad_block);
    inner.update(message);
    let inner_hash = inner.finalize();

    let mut opad_block = [0x5cu8; BLOCK_SIZE];
    for i in 0..BLOCK_SIZE {
        opad_block[i] ^= key_block[i];
    }
    let mut outer = Sha256::new();
    outer.update(opad_block);
    outer.update(inner_hash);
    outer.finalize().into()
}

fn hex_encode(bytes: &[u8]) -> String {
    bytes.iter().map(|b| format!("{b:02x}")).collect()
}

async fn insert_webhook(pool: &PgPool, profile_id: Uuid, secret_key: &[u8], enabled: bool) -> Uuid {
    let id = Uuid::now_v7();
    sqlx::query(
        "INSERT INTO webhooks (id, profile_id, name, secret_reference, target, event_filter, enabled, secret_key) \
         VALUES ($1, $2, $3, $4, $5, $6, $7, $8)",
    )
    .bind(id)
    .bind(profile_id)
    .bind(format!("test-webhook-{id}"))
    .bind::<Option<&str>>(None)
    .bind(serde_json::json!({"url":"https://example.test"}))
    .bind(serde_json::json!({"events":["*"]}))
    .bind(enabled)
    .bind(secret_key)
    .execute(pool)
    .await
    .expect("insert test webhook");
    id
}

async fn send_webhook_delivery(
    app: &Router,
    webhook_id: Uuid,
    secret: &[u8],
    body: &[u8],
    timestamp_millis: i64,
    delivery_id: Uuid,
) -> (StatusCode, Value) {
    let payload = format!("{timestamp_millis}.{}", String::from_utf8_lossy(body));
    let mac = hmac_sha256(secret, payload.as_bytes());
    let sig = format!("sha256={}", hex_encode(&mac));

    let builder = Request::builder()
        .method(Method::POST)
        .uri(format!("/api/v1/webhooks/{webhook_id}/deliver"))
        .header("x-gobrowse-signature", sig)
        .header("x-gobrowse-delivery", delivery_id.to_string())
        .header("x-gobrowse-timestamp", timestamp_millis.to_string());

    // For webhook delivery, body is raw bytes (no application/json content-type needed)
    let response = app
        .clone()
        .oneshot(
            builder
                .body(Body::from(body.to_vec()))
                .expect("build request"),
        )
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
        serde_json::from_slice(&bytes)
            .unwrap_or(Value::String(String::from_utf8_lossy(&bytes).into_owned()))
    };
    (status, value)
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[tokio::test]
async fn valid_signature_is_accepted_and_dedups_delivery() {
    let Some(pool) = test_pool().await else {
        eprintln!("GOBROWSE_TEST_DATABASE_URL is unset; skipping webhook integration test");
        return;
    };
    let database_url =
        std::env::var("GOBROWSE_TEST_DATABASE_URL").expect("already checked by test_pool");

    let profile_id = Uuid::now_v7();
    sqlx::query("INSERT INTO profiles (id, name) VALUES ($1, 'webhook-test-profile')")
        .bind(profile_id)
        .execute(&pool)
        .await
        .expect("create test profile");

    let secret = b"integration-test-secret-key-32b";
    let webhook_id = insert_webhook(&pool, profile_id, secret, true).await;

    let state = AppState::new(pool.clone(), test_settings(&database_url))
        .await
        .expect("create app state");
    let app = gobrowse_server::router(state.clone());

    let now = time::OffsetDateTime::now_utc();
    let ts_millis = now.unix_timestamp() * 1000 + now.millisecond() as i64;
    let body = b"{\"event\":\"test.ping\"}";
    let delivery_id = Uuid::now_v7();

    // First delivery: should succeed with 200
    let (status, _response) =
        send_webhook_delivery(&app, webhook_id, secret, body, ts_millis, delivery_id).await;
    assert_eq!(
        status,
        StatusCode::OK,
        "first delivery must succeed, got {status}: {_response}"
    );

    // Verify delivery row exists
    let count: i64 = sqlx::query_scalar(
        "SELECT count(*) FROM webhook_deliveries WHERE webhook_id = $1 AND delivery_id = $2",
    )
    .bind(webhook_id)
    .bind(delivery_id.to_string())
    .fetch_one(&pool)
    .await
    .expect("count deliveries");
    assert_eq!(count, 1, "delivery row must be inserted");

    // Second delivery with same id: should return 409 Conflict
    let (status, _response) =
        send_webhook_delivery(&app, webhook_id, secret, body, ts_millis, delivery_id).await;
    assert_eq!(
        status,
        StatusCode::CONFLICT,
        "replayed delivery must return 409, got {status}: {_response}"
    );

    // Clean up
    sqlx::query("DELETE FROM webhook_deliveries WHERE webhook_id = $1")
        .bind(webhook_id)
        .execute(&pool)
        .await
        .expect("clean deliveries");
    sqlx::query("DELETE FROM webhooks WHERE id = $1")
        .bind(webhook_id)
        .execute(&pool)
        .await
        .expect("clean webhook");
    sqlx::query("DELETE FROM profiles WHERE id = $1")
        .bind(profile_id)
        .execute(&pool)
        .await
        .expect("clean profile");
}

#[tokio::test]
async fn replayed_delivery_id_returns_conflict() {
    let Some(pool) = test_pool().await else {
        eprintln!("GOBROWSE_TEST_DATABASE_URL is unset; skipping webhook integration test");
        return;
    };
    let database_url =
        std::env::var("GOBROWSE_TEST_DATABASE_URL").expect("already checked by test_pool");

    let profile_id = Uuid::now_v7();
    sqlx::query("INSERT INTO profiles (id, name) VALUES ($1, 'webhook-replay-profile')")
        .bind(profile_id)
        .execute(&pool)
        .await
        .expect("create test profile");

    let secret = b"replay-test-secret-key-32bytes";
    let webhook_id = insert_webhook(&pool, profile_id, secret, true).await;

    let state = AppState::new(pool.clone(), test_settings(&database_url))
        .await
        .expect("create app state");
    let app = gobrowse_server::router(state.clone());

    let now = time::OffsetDateTime::now_utc();
    let ts_millis = now.unix_timestamp() * 1000 + now.millisecond() as i64;
    let body = b"{\"event\":\"repeat.me\"}";
    let delivery_id = Uuid::now_v7();

    // First: OK
    let (status, _) =
        send_webhook_delivery(&app, webhook_id, secret, body, ts_millis, delivery_id).await;
    assert_eq!(status, StatusCode::OK);

    // Second with same delivery_id: Conflict
    let (status, response) =
        send_webhook_delivery(&app, webhook_id, secret, body, ts_millis, delivery_id).await;
    assert_eq!(
        status,
        StatusCode::CONFLICT,
        "replay must return 409, got {status}: {response}"
    );
    // Verify the error code in the JSON response
    if let Some(code) = response.get("code").and_then(|v| v.as_str()) {
        assert_eq!(code, "conflict");
    }

    // Clean up
    sqlx::query("DELETE FROM webhook_deliveries WHERE webhook_id = $1")
        .bind(webhook_id)
        .execute(&pool)
        .await
        .expect("clean deliveries");
    sqlx::query("DELETE FROM webhooks WHERE id = $1")
        .bind(webhook_id)
        .execute(&pool)
        .await
        .expect("clean webhook");
    sqlx::query("DELETE FROM profiles WHERE id = $1")
        .bind(profile_id)
        .execute(&pool)
        .await
        .expect("clean profile");
}

#[tokio::test]
async fn tampered_body_rejected_constant_time() {
    let Some(pool) = test_pool().await else {
        eprintln!("GOBROWSE_TEST_DATABASE_URL is unset; skipping webhook integration test");
        return;
    };
    let database_url =
        std::env::var("GOBROWSE_TEST_DATABASE_URL").expect("already checked by test_pool");

    let profile_id = Uuid::now_v7();
    sqlx::query("INSERT INTO profiles (id, name) VALUES ($1, 'webhook-tamper-profile')")
        .bind(profile_id)
        .execute(&pool)
        .await
        .expect("create test profile");

    let secret = b"tamper-test-secret-32-byte-key!";
    let webhook_id = insert_webhook(&pool, profile_id, secret, true).await;

    let state = AppState::new(pool.clone(), test_settings(&database_url))
        .await
        .expect("create app state");
    let app = gobrowse_server::router(state.clone());

    let now = time::OffsetDateTime::now_utc();
    let ts_millis = now.unix_timestamp() * 1000 + now.millisecond() as i64;
    let body = b"{\"event\":\"original\"}";
    let delivery_id = Uuid::now_v7();

    // Send with tampered body — compute signature over tampered body, but actually
    // a different-signature attack: use signature computed for "original" body
    // but send "hacked" body. Both bodies go through so the signature won't match.
    // The test is: flip a body byte and verify it's rejected.
    //
    // Strategy: compute signature for "original", send with "hacked" body
    let original_payload = format!("{ts_millis}.{}", String::from_utf8_lossy(body));
    let original_mac = hmac_sha256(secret, original_payload.as_bytes());
    let original_sig = format!("sha256={}", hex_encode(&original_mac));

    let tampered_body = b"{\"event\":\"hacked\"}";

    let builder = Request::builder()
        .method(Method::POST)
        .uri(format!("/api/v1/webhooks/{webhook_id}/deliver"))
        .header("x-gobrowse-signature", original_sig.as_str())
        .header("x-gobrowse-delivery", delivery_id.to_string())
        .header("x-gobrowse-timestamp", ts_millis.to_string());

    let response = app
        .clone()
        .oneshot(
            builder
                .body(Body::from(tampered_body.to_vec()))
                .expect("build tampered request"),
        )
        .await
        .expect("route tampered request");

    let status = response.status();
    assert_eq!(
        status,
        StatusCode::UNAUTHORIZED,
        "tampered body must be rejected with 401"
    );

    // Assert no delivery row was inserted
    let count: i64 = sqlx::query_scalar(
        "SELECT count(*) FROM webhook_deliveries WHERE webhook_id = $1 AND delivery_id = $2",
    )
    .bind(webhook_id)
    .bind(delivery_id.to_string())
    .fetch_one(&pool)
    .await
    .expect("count deliveries");
    assert_eq!(count, 0, "no delivery row must exist for rejected request");

    // Clean up
    sqlx::query("DELETE FROM webhook_deliveries WHERE webhook_id = $1")
        .bind(webhook_id)
        .execute(&pool)
        .await
        .expect("clean deliveries");
    sqlx::query("DELETE FROM webhooks WHERE id = $1")
        .bind(webhook_id)
        .execute(&pool)
        .await
        .expect("clean webhook");
    sqlx::query("DELETE FROM profiles WHERE id = $1")
        .bind(profile_id)
        .execute(&pool)
        .await
        .expect("clean profile");
}

#[tokio::test]
async fn disabled_webhook_returns_not_found() {
    let Some(pool) = test_pool().await else {
        eprintln!("GOBROWSE_TEST_DATABASE_URL is unset; skipping webhook integration test");
        return;
    };
    let database_url =
        std::env::var("GOBROWSE_TEST_DATABASE_URL").expect("already checked by test_pool");

    let profile_id = Uuid::now_v7();
    sqlx::query("INSERT INTO profiles (id, name) VALUES ($1, 'webhook-disabled-profile')")
        .bind(profile_id)
        .execute(&pool)
        .await
        .expect("create test profile");

    let secret = b"disabled-test-secret-key32bytes";
    let webhook_id = insert_webhook(&pool, profile_id, secret, false).await;

    let state = AppState::new(pool.clone(), test_settings(&database_url))
        .await
        .expect("create app state");
    let app = gobrowse_server::router(state.clone());

    let now = time::OffsetDateTime::now_utc();
    let ts_millis = now.unix_timestamp() * 1000 + now.millisecond() as i64;
    let body = b"{\"event\":\"ping\"}";
    let delivery_id = Uuid::now_v7();

    let (status, response) =
        send_webhook_delivery(&app, webhook_id, secret, body, ts_millis, delivery_id).await;
    assert_eq!(
        status,
        StatusCode::NOT_FOUND,
        "disabled webhook must return 404, got {status}: {response}"
    );

    // Clean up
    sqlx::query("DELETE FROM webhooks WHERE id = $1")
        .bind(webhook_id)
        .execute(&pool)
        .await
        .expect("clean webhook");
    sqlx::query("DELETE FROM profiles WHERE id = $1")
        .bind(profile_id)
        .execute(&pool)
        .await
        .expect("clean profile");
}

#[tokio::test]
async fn nonexistent_webhook_returns_not_found() {
    let Some(pool) = test_pool().await else {
        eprintln!("GOBROWSE_TEST_DATABASE_URL is unset; skipping webhook integration test");
        return;
    };
    let database_url =
        std::env::var("GOBROWSE_TEST_DATABASE_URL").expect("already checked by test_pool");

    let state = AppState::new(pool.clone(), test_settings(&database_url))
        .await
        .expect("create app state");
    let app = gobrowse_server::router(state.clone());

    let fake_id = Uuid::now_v7();
    let now = time::OffsetDateTime::now_utc();
    let ts_millis = now.unix_timestamp() * 1000 + now.millisecond() as i64;

    let builder = Request::builder()
        .method(Method::POST)
        .uri(format!("/api/v1/webhooks/{fake_id}/deliver"))
        .header(
            "x-gobrowse-signature",
            "sha256=0000000000000000000000000000000000000000000000000000000000000000",
        )
        .header("x-gobrowse-delivery", Uuid::now_v7().to_string())
        .header("x-gobrowse-timestamp", ts_millis.to_string());

    let response = app
        .clone()
        .oneshot(builder.body(Body::empty()).expect("build fake request"))
        .await
        .expect("route fake request");

    assert_eq!(response.status(), StatusCode::NOT_FOUND);
}

#[tokio::test]
async fn stale_timestamp_rejected() {
    let Some(pool) = test_pool().await else {
        eprintln!("GOBROWSE_TEST_DATABASE_URL is unset; skipping webhook integration test");
        return;
    };
    let database_url =
        std::env::var("GOBROWSE_TEST_DATABASE_URL").expect("already checked by test_pool");

    let profile_id = Uuid::now_v7();
    sqlx::query("INSERT INTO profiles (id, name) VALUES ($1, 'webhook-stale-profile')")
        .bind(profile_id)
        .execute(&pool)
        .await
        .expect("create test profile");

    let secret = b"stale-test-secret-key32bytes!!";
    let webhook_id = insert_webhook(&pool, profile_id, secret, true).await;

    let state = AppState::new(pool.clone(), test_settings(&database_url))
        .await
        .expect("create app state");
    let app = gobrowse_server::router(state.clone());

    let now = time::OffsetDateTime::now_utc();
    // Use a timestamp 10 minutes in the past (well beyond 5-min skew)
    let stale_millis = (now.unix_timestamp() - 600) * 1000 + now.millisecond() as i64;
    let body = b"{\"event\":\"stale\"}";
    let delivery_id = Uuid::now_v7();

    let (status, response) =
        send_webhook_delivery(&app, webhook_id, secret, body, stale_millis, delivery_id).await;
    assert_eq!(
        status,
        StatusCode::UNAUTHORIZED,
        "stale timestamp must be rejected with 401, got {status}: {response}"
    );

    // Verify no delivery row inserted
    let count: i64 = sqlx::query_scalar(
        "SELECT count(*) FROM webhook_deliveries WHERE webhook_id = $1 AND delivery_id = $2",
    )
    .bind(webhook_id)
    .bind(delivery_id.to_string())
    .fetch_one(&pool)
    .await
    .expect("count deliveries");
    assert_eq!(
        count, 0,
        "no delivery row must be inserted for stale request"
    );

    // Clean up
    sqlx::query("DELETE FROM webhooks WHERE id = $1")
        .bind(webhook_id)
        .execute(&pool)
        .await
        .expect("clean webhook");
    sqlx::query("DELETE FROM profiles WHERE id = $1")
        .bind(profile_id)
        .execute(&pool)
        .await
        .expect("clean profile");
}
