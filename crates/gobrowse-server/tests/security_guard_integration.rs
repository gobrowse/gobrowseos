//! CSRF origin-guard and WebSocket origin-validation integration tests.
//!
//! HTTP tests use Router::oneshot against a minimal AppState; WS tests
//! require a real PostgreSQL database for session authentication.

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
    db,
};
use http_body_util::BodyExt;
use secrecy::SecretString;
use serde_json::Value;
use sha2::{Digest, Sha256};
use sqlx::PgPool;
use tokio::net::TcpListener;
use tokio_tungstenite::connect_async;
use tokio_tungstenite::tungstenite::client::IntoClientRequest;
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

fn settings_with_origin(public_origin: &str, database_url: &str) -> Settings {
    Settings {
        http: HttpSettings {
            public_origin: public_origin.parse().expect("valid origin URL"),
            ..HttpSettings::default()
        },
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

async fn create_authed_session(pool: &PgPool, role: &str) -> (Uuid, Uuid, String) {
    let profile_id = Uuid::now_v7();
    sqlx::query("INSERT INTO profiles (id, name) VALUES ($1, $2)")
        .bind(profile_id)
        .bind(format!("security-guard-test-{profile_id}"))
        .execute(pool)
        .await
        .expect("create test profile");

    let user_id = Uuid::now_v7();
    let token = format!("security-guard-session-{}", Uuid::now_v7());
    sqlx::query(
        "INSERT INTO users (id, email, display_name, password_hash, role, primary_profile_id) \
         VALUES ($1, $2, 'Security Guard Test', 'unused', $3, $4)",
    )
    .bind(user_id)
    .bind(format!("{user_id}@example.test"))
    .bind(role)
    .bind(profile_id)
    .execute(pool)
    .await
    .expect("create test user");

    let now = time::OffsetDateTime::now_utc();
    sqlx::query(
        "INSERT INTO sessions (token_hash, user_id, auth_epoch, expires_at, absolute_expires_at) \
         VALUES ($1, $2, 1, $3, $4)",
    )
    .bind(Sha256::digest(token.as_bytes()).to_vec())
    .bind(user_id)
    .bind(now + time::Duration::hours(1))
    .bind(now + time::Duration::hours(2))
    .execute(pool)
    .await
    .expect("create test session");

    (profile_id, user_id, format!("gobrowse_session={token}"))
}

async fn cleanup_session(pool: &PgPool, user_id: Uuid, profile_id: Uuid) {
    sqlx::query("DELETE FROM sessions WHERE user_id = $1")
        .bind(user_id)
        .execute(pool)
        .await
        .expect("cleanup sessions");
    sqlx::query("DELETE FROM users WHERE id = $1")
        .bind(user_id)
        .execute(pool)
        .await
        .expect("cleanup user");
    sqlx::query("DELETE FROM profiles WHERE id = $1")
        .bind(profile_id)
        .execute(pool)
        .await
        .expect("cleanup profile");
}

/// Send a request through the app Router and return status + JSON body.
async fn send(
    app: &Router,
    method: Method,
    uri: &str,
    cookie: Option<&str>,
    origin: Option<&str>,
    sec_fetch_site: Option<&str>,
    body: Option<&[u8]>,
) -> (StatusCode, Value) {
    let mut builder = Request::builder().method(method).uri(uri);
    if let Some(c) = cookie {
        builder = builder.header(header::COOKIE, c);
    }
    if let Some(o) = origin {
        builder = builder.header(header::ORIGIN, o);
    }
    if let Some(sfs) = sec_fetch_site {
        builder = builder.header("sec-fetch-site", sfs);
    }
    let req_body = if let Some(b) = body {
        builder = builder.header(header::CONTENT_TYPE, "application/json");
        Body::from(b.to_vec())
    } else {
        Body::empty()
    };
    let response = app
        .clone()
        .oneshot(builder.body(req_body).expect("build request"))
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
        serde_json::from_slice(&bytes).unwrap_or(Value::Null)
    };
    (status, value)
}

// ---------------------------------------------------------------------------
// HTTP Origin Guard Tests (no DB session needed, Router::oneshot)
// ---------------------------------------------------------------------------

#[tokio::test]
async fn cross_origin_post_is_forbidden() {
    let Some(pool) = test_pool().await else {
        eprintln!("GOBROWSE_TEST_DATABASE_URL is unset; skipping security guard test");
        return;
    };
    let database_url =
        std::env::var("GOBROWSE_TEST_DATABASE_URL").expect("already checked by test_pool");
    let state = AppState::new(
        pool.clone(),
        settings_with_origin("https://app.example.com", &database_url),
    )
    .await
    .expect("create app state");
    let app = gobrowse_server::router(state);

    let (status, _body) = send(
        &app,
        Method::POST,
        "/api/v1/auth/login",
        None,
        Some("https://evil.example.com"),
        None,
        Some(b"{\"email\":\"x@x\",\"password\":\"x\"}"),
    )
    .await;
    assert_eq!(
        status,
        StatusCode::FORBIDDEN,
        "cross-origin POST must be forbidden"
    );
}

#[tokio::test]
async fn cross_site_fetch_site_is_forbidden() {
    let Some(pool) = test_pool().await else {
        eprintln!("GOBROWSE_TEST_DATABASE_URL is unset; skipping security guard test");
        return;
    };
    let database_url =
        std::env::var("GOBROWSE_TEST_DATABASE_URL").expect("already checked by test_pool");
    let state = AppState::new(
        pool.clone(),
        settings_with_origin("https://app.example.com", &database_url),
    )
    .await
    .expect("create app state");
    let app = gobrowse_server::router(state);

    // Even with correct Origin, Sec-Fetch-Site: cross-site must be rejected.
    let (status, _body) = send(
        &app,
        Method::POST,
        "/api/v1/auth/login",
        None,
        Some("https://app.example.com"),
        Some("cross-site"),
        Some(b"{\"email\":\"x@x\",\"password\":\"x\"}"),
    )
    .await;
    assert_eq!(
        status,
        StatusCode::FORBIDDEN,
        "cross-site Sec-Fetch-Site on POST must be forbidden"
    );
}

#[tokio::test]
async fn same_site_post_is_allowed() {
    let Some(pool) = test_pool().await else {
        eprintln!("GOBROWSE_TEST_DATABASE_URL is unset; skipping security guard test");
        return;
    };
    let database_url =
        std::env::var("GOBROWSE_TEST_DATABASE_URL").expect("already checked by test_pool");
    let state = AppState::new(
        pool.clone(),
        settings_with_origin("http://localhost:8080", &database_url),
    )
    .await
    .expect("create app state");
    let app = gobrowse_server::router(state);

    // POST with correct Origin and Sec-Fetch-Site: same-site passes the
    // origin guard. The request may still return UNAUTHORIZED/BAD_REQUEST
    // depending on auth state, but must NOT be FORBIDDEN (403).
    let (status, _body) = send(
        &app,
        Method::POST,
        "/api/v1/auth/login",
        None,
        Some("http://localhost:8080"),
        Some("same-site"),
        Some(b"{\"email\":\"nobody@example.test\",\"password\":\"bad-password\"}"),
    )
    .await;
    assert!(
        status != StatusCode::FORBIDDEN,
        "same-site POST with correct Origin must not be forbidden, got {status}"
    );
}

#[tokio::test]
async fn missing_origin_on_state_change_is_forbidden() {
    let Some(pool) = test_pool().await else {
        eprintln!("GOBROWSE_TEST_DATABASE_URL is unset; skipping security guard test");
        return;
    };
    let database_url =
        std::env::var("GOBROWSE_TEST_DATABASE_URL").expect("already checked by test_pool");
    let state = AppState::new(
        pool.clone(),
        settings_with_origin("http://localhost:8080", &database_url),
    )
    .await
    .expect("create app state");
    let app = gobrowse_server::router(state);

    // POST without an Origin header must be rejected (latent CSRF).
    let (status, body) = send(
        &app,
        Method::POST,
        "/api/v1/auth/login",
        None,
        None, // no Origin
        None,
        Some(b"{\"email\":\"x@x\",\"password\":\"x\"}"),
    )
    .await;
    assert_eq!(
        status,
        StatusCode::FORBIDDEN,
        "POST without Origin must be forbidden, got {status}: {body}"
    );
}

// ---------------------------------------------------------------------------
// WebSocket Origin Validation Tests (DB-backed: require session)
// ---------------------------------------------------------------------------

#[tokio::test]
async fn websocket_upgrade_rejects_missing_origin() {
    let Some(pool) = test_pool().await else {
        eprintln!("GOBROWSE_TEST_DATABASE_URL is unset; skipping security guard test");
        return;
    };
    let database_url =
        std::env::var("GOBROWSE_TEST_DATABASE_URL").expect("already checked by test_pool");
    let (profile_id, user_id, _cookie) = create_authed_session(&pool, "OWNER").await;
    let state = AppState::new(
        pool.clone(),
        settings_with_origin("http://localhost:8080", &database_url),
    )
    .await
    .expect("create app state");
    let app = gobrowse_server::router(state.clone());

    let listener = TcpListener::bind("127.0.0.1:0")
        .await
        .expect("bind WS test server");
    let address = listener.local_addr().expect("WS test address");
    let server = tokio::spawn(async move {
        axum::serve(listener, app).await.expect("serve WS test app");
    });

    // Build WS request WITHOUT Origin header
    let fake_run_id = Uuid::now_v7();
    let request = format!("ws://{address}/api/v1/runs/{fake_run_id}/realtime?after=0")
        .into_client_request()
        .expect("WS request");
    // Deliberately omit the Origin header

    let result = connect_async(request).await;
    assert!(
        result.is_err(),
        "WS upgrade without Origin must be rejected"
    );

    server.abort();
    cleanup_session(&pool, user_id, profile_id).await;
}

#[tokio::test]
async fn websocket_upgrade_rejects_mismatched_origin() {
    let Some(pool) = test_pool().await else {
        eprintln!("GOBROWSE_TEST_DATABASE_URL is unset; skipping security guard test");
        return;
    };
    let database_url =
        std::env::var("GOBROWSE_TEST_DATABASE_URL").expect("already checked by test_pool");
    let (profile_id, user_id, cookie) = create_authed_session(&pool, "OWNER").await;
    let state = AppState::new(
        pool.clone(),
        settings_with_origin("https://app.example.com", &database_url),
    )
    .await
    .expect("create app state");
    let app = gobrowse_server::router(state.clone());

    let listener = TcpListener::bind("127.0.0.1:0")
        .await
        .expect("bind WS test server");
    let address = listener.local_addr().expect("WS test address");
    let server = tokio::spawn(async move {
        axum::serve(listener, app).await.expect("serve WS test app");
    });

    // Build WS request with a mismatched Origin
    let fake_run_id = Uuid::now_v7();
    let mut request = format!("ws://{address}/api/v1/runs/{fake_run_id}/realtime?after=0")
        .into_client_request()
        .expect("WS request");
    request
        .headers_mut()
        .insert(header::ORIGIN, "https://evil.example.com".parse().unwrap());
    request
        .headers_mut()
        .insert(header::COOKIE, cookie.parse().unwrap());

    let result = connect_async(request).await;
    assert!(
        result.is_err(),
        "WS upgrade with mismatched Origin must be rejected"
    );

    server.abort();
    cleanup_session(&pool, user_id, profile_id).await;
}

#[tokio::test]
async fn websocket_upgrade_rejects_http_scheme_against_https_public_origin() {
    let Some(pool) = test_pool().await else {
        eprintln!("GOBROWSE_TEST_DATABASE_URL is unset; skipping security guard test");
        return;
    };
    let database_url =
        std::env::var("GOBROWSE_TEST_DATABASE_URL").expect("already checked by test_pool");
    let (profile_id, user_id, cookie) = create_authed_session(&pool, "OWNER").await;
    // Public origin is https but the WS Origin header uses http.
    let state = AppState::new(
        pool.clone(),
        settings_with_origin("https://app.example.com", &database_url),
    )
    .await
    .expect("create app state");
    let app = gobrowse_server::router(state.clone());

    let listener = TcpListener::bind("127.0.0.1:0")
        .await
        .expect("bind WS test server");
    let address = listener.local_addr().expect("WS test address");
    let server = tokio::spawn(async move {
        axum::serve(listener, app).await.expect("serve WS test app");
    });

    // WS request with http://app.example.com Origin against https public_origin
    let fake_run_id = Uuid::now_v7();
    let mut request = format!("ws://{address}/api/v1/runs/{fake_run_id}/realtime?after=0")
        .into_client_request()
        .expect("WS request");
    request
        .headers_mut()
        .insert(header::ORIGIN, "http://app.example.com".parse().unwrap());
    request
        .headers_mut()
        .insert(header::COOKIE, cookie.parse().unwrap());

    let result = connect_async(request).await;
    assert!(
        result.is_err(),
        "WS upgrade with http Origin against https public_origin must be rejected"
    );

    server.abort();
    cleanup_session(&pool, user_id, profile_id).await;
}
