use axum::{
    Router,
    body::Body,
    http::{Method, Request, StatusCode},
};
use base64::{Engine as _, engine::general_purpose::URL_SAFE_NO_PAD};
use gobrowse_server::{
    AppState,
    config::{
        AuthSettings, DatabaseSettings, FeatureSettings, HttpSettings, ObservabilitySettings,
        Settings, VaultSettings,
    },
    db,
};
use http_body_util::BodyExt;
use rand::RngCore;
use secrecy::SecretString;
use sha2::{Digest, Sha256};
use sqlx::PgPool;
use time::OffsetDateTime;
use tower::ServiceExt;
use uuid::Uuid;

// ---------------------------------------------------------------------------
// Helpers (mirror session_rotation_integration helpers; kept self-contained)
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

fn token_hash(token: &[u8]) -> Vec<u8> {
    Sha256::digest(token).to_vec()
}

struct TestUser {
    user_id: Uuid,
    profile_id: Uuid,
    email: String,
    _password: String,
}

async fn create_user_with_password(
    pool: &PgPool,
    state: &AppState,
    email: &str,
    display_name: &str,
    password: &str,
    role: &str,
) -> TestUser {
    let profile_id = Uuid::now_v7();
    let user_id = Uuid::now_v7();
    let password_hash = state
        .passwords
        .hash(password)
        .await
        .expect("hash test password");

    sqlx::query("INSERT INTO profiles (id, name) VALUES ($1, $2)")
        .bind(profile_id)
        .bind(format!("{display_name}'s profile"))
        .execute(pool)
        .await
        .expect("create test profile");

    sqlx::query(
        "INSERT INTO users (id, email, display_name, password_hash, role, primary_profile_id) \
         VALUES ($1, lower($2), $3, $4, $5, $6)",
    )
    .bind(user_id)
    .bind(email)
    .bind(display_name)
    .bind(password_hash)
    .bind(role)
    .bind(profile_id)
    .execute(pool)
    .await
    .expect("create test user");

    TestUser {
        user_id,
        profile_id,
        email: email.to_string(),
        _password: password.to_string(),
    }
}

/// Insert a session row with caller-controlled `expires_at` and
/// `absolute_expires_at` columns. Returns a `Cookie` header value.
async fn create_session_with_expires(
    pool: &PgPool,
    user_id: Uuid,
    auth_epoch: i64,
    expires_at: OffsetDateTime,
    absolute_expires_at: OffsetDateTime,
) -> String {
    let mut raw = [0u8; 32];
    rand::rng().fill_bytes(&mut raw);
    let encoded = URL_SAFE_NO_PAD.encode(raw);
    let hash = token_hash(encoded.as_bytes());
    sqlx::query(
        "INSERT INTO sessions (token_hash, user_id, auth_epoch, expires_at, absolute_expires_at) \
         VALUES ($1, $2, $3, $4, $5)",
    )
    .bind(&hash)
    .bind(user_id)
    .bind(auth_epoch)
    .bind(expires_at)
    .bind(absolute_expires_at)
    .execute(pool)
    .await
    .expect("create test session");
    format!("gobrowse_session={encoded}")
}

async fn request_json(
    app: &Router,
    method: Method,
    uri: &str,
    body: Option<serde_json::Value>,
    cookie: Option<&str>,
) -> (StatusCode, serde_json::Value) {
    let mut builder = Request::builder().method(method.clone()).uri(uri);
    if let Some(c) = cookie {
        builder = builder.header("Cookie", c);
    }
    if matches!(
        method,
        Method::POST | Method::PUT | Method::PATCH | Method::DELETE
    ) {
        builder = builder.header("Origin", "http://localhost:8080");
    }
    if let Some(b) = body {
        builder = builder.header("Content-Type", "application/json");
        let response = app
            .clone()
            .oneshot(
                builder
                    .body(Body::from(serde_json::to_vec(&b).expect("serialize body")))
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
        let value: serde_json::Value =
            serde_json::from_slice(&bytes).unwrap_or(serde_json::Value::Null);
        (status, value)
    } else {
        let response = app
            .clone()
            .oneshot(builder.body(Body::empty()).expect("build request"))
            .await
            .expect("route request");
        let status = response.status();
        let bytes = response
            .into_body()
            .collect()
            .await
            .expect("collect response")
            .to_bytes();
        let value: serde_json::Value =
            serde_json::from_slice(&bytes).unwrap_or(serde_json::Value::Null);
        (status, value)
    }
}

/// Delete test data for a user, respecting audit_events being append-only.
async fn cleanup_user(pool: &PgPool, user: &TestUser) {
    sqlx::query("DELETE FROM sessions WHERE user_id = $1")
        .bind(user.user_id)
        .execute(pool)
        .await
        .expect("cleanup sessions");
    sqlx::query("DELETE FROM login_attempts WHERE email = $1")
        .bind(user.email.to_lowercase())
        .execute(pool)
        .await
        .expect("cleanup login attempts");
    // audit_events is append-only; FK ON DELETE SET NULL handles cleanup.
    sqlx::query("DELETE FROM users WHERE id = $1")
        .bind(user.user_id)
        .execute(pool)
        .await
        .expect("cleanup user");
    sqlx::query("DELETE FROM profiles WHERE id = $1")
        .bind(user.profile_id)
        .execute(pool)
        .await
        .expect("cleanup profile");
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[tokio::test]
async fn expired_absolute_timeout_rejects_even_when_idle_remaining() {
    let Some(pool) = test_pool().await else {
        eprintln!(
            "GOBROWSE_TEST_DATABASE_URL is unset; skipping absolute-timeout integration test"
        );
        return;
    };
    let database_url =
        std::env::var("GOBROWSE_TEST_DATABASE_URL").expect("already checked by test_pool");

    let state = AppState::new(pool.clone(), test_settings(&database_url))
        .await
        .expect("create app state");
    let app = gobrowse_server::router(state.clone());

    let user = create_user_with_password(
        &pool,
        &state,
        &format!("abs-exp-{}@example.test", Uuid::now_v7().simple()),
        "Absolute Expired Test",
        "long-enough-password",
        "OWNER",
    )
    .await;

    let now = OffsetDateTime::now_utc();
    // idle timeout still in the future, but absolute timeout already passed
    let cookie = create_session_with_expires(
        &pool,
        user.user_id,
        1,
        now + time::Duration::hours(1), // expires_at: idle still valid
        now - time::Duration::minutes(1), // absolute_expires_at: already expired
    )
    .await;

    let (status, body) =
        request_json(&app, Method::GET, "/api/v1/auth/me", None, Some(&cookie)).await;
    assert_eq!(
        status,
        StatusCode::UNAUTHORIZED,
        "session with expired absolute_expires_at must be rejected even when idle remains: {body}"
    );

    cleanup_user(&pool, &user).await;
}

#[tokio::test]
async fn idle_refresh_within_absolute_window_still_works() {
    let Some(pool) = test_pool().await else {
        eprintln!(
            "GOBROWSE_TEST_DATABASE_URL is unset; skipping absolute-timeout integration test"
        );
        return;
    };
    let database_url =
        std::env::var("GOBROWSE_TEST_DATABASE_URL").expect("already checked by test_pool");

    let state = AppState::new(pool.clone(), test_settings(&database_url))
        .await
        .expect("create app state");
    let app = gobrowse_server::router(state.clone());

    let user = create_user_with_password(
        &pool,
        &state,
        &format!("abs-valid-{}@example.test", Uuid::now_v7().simple()),
        "Absolute Valid Test",
        "long-enough-password",
        "OWNER",
    )
    .await;

    let now = OffsetDateTime::now_utc();
    // both expires_at and absolute_expires_at are in the future
    let cookie = create_session_with_expires(
        &pool,
        user.user_id,
        1,
        now + time::Duration::hours(1), // expires_at: idle valid
        now + time::Duration::hours(2), // absolute_expires_at: still in future
    )
    .await;

    let (status, body) =
        request_json(&app, Method::GET, "/api/v1/auth/me", None, Some(&cookie)).await;
    assert_eq!(
        status,
        StatusCode::OK,
        "session with both timeouts in the future must be accepted: {body}"
    );
    assert_eq!(
        body.get("id")
            .and_then(|v| v.as_str())
            .map(|s| s.to_string()),
        Some(user.user_id.to_string()),
        "me response must return the correct user id"
    );

    cleanup_user(&pool, &user).await;
}
