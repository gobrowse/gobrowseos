mod common;

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
    sqlx::query("INSERT INTO profiles (id, name) VALUES ($1, $2)")
        .bind(profile_id)
        .bind(format!("{display_name}'s profile"))
        .execute(pool)
        .await
        .expect("create test profile");
    create_user_in_profile_with_password(
        pool,
        state,
        profile_id,
        email,
        display_name,
        password,
        role,
    )
    .await
}

async fn create_user_in_profile_with_password(
    pool: &PgPool,
    state: &AppState,
    profile_id: Uuid,
    email: &str,
    display_name: &str,
    password: &str,
    role: &str,
) -> TestUser {
    let user_id = Uuid::now_v7();
    let password_hash = state
        .passwords
        .hash(password)
        .await
        .expect("hash test password");

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

async fn create_session(pool: &PgPool, user_id: Uuid, auth_epoch: i64) -> String {
    let mut raw = [0u8; 32];
    rand::rng().fill_bytes(&mut raw);
    let encoded = URL_SAFE_NO_PAD.encode(raw);
    let hash = token_hash(encoded.as_bytes());
    let now = OffsetDateTime::now_utc();
    sqlx::query(
        "INSERT INTO sessions (token_hash, user_id, auth_epoch, expires_at, absolute_expires_at) \
         VALUES ($1, $2, $3, $4, $5)",
    )
    .bind(&hash)
    .bind(user_id)
    .bind(auth_epoch)
    .bind(now + time::Duration::minutes(120))
    .bind(now + time::Duration::hours(336))
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
    // State-changing requests from a browser always include an Origin
    // header. The default public_origin is http://localhost:8080.
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

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[tokio::test]
async fn rotate_sessions_invalidates_existing_sessions() {
    let Some(database_url) = std::env::var("GOBROWSE_TEST_DATABASE_URL").ok() else {
        eprintln!(
            "GOBROWSE_TEST_DATABASE_URL is unset; skipping session rotation integration test"
        );
        return;
    };
    let _lock = common::acquire_test_lock(&database_url).await;
    let pool = test_pool().await.expect("test pool after lock");

    let state = AppState::new(pool.clone(), test_settings(&database_url))
        .await
        .expect("create app state");
    let app = gobrowse_server::router(state.clone());

    let user = create_user_with_password(
        &pool,
        &state,
        &format!("rot-test-{}@example.test", Uuid::now_v7().simple()),
        "Rotation Test",
        "long-enough-password",
        "OWNER",
    )
    .await;

    let cookie = create_session(&pool, user.user_id, 1).await;

    // Verify session works
    let (status, body) =
        request_json(&app, Method::GET, "/api/v1/auth/me", None, Some(&cookie)).await;
    assert_eq!(
        status,
        StatusCode::OK,
        "initial session should be valid: {body}"
    );

    // Rotate sessions
    let (status, _body) = request_json(
        &app,
        Method::POST,
        "/api/v1/auth/rotate",
        Some(serde_json::json!({"user_id": user.user_id})),
        Some(&cookie),
    )
    .await;
    assert_eq!(status, StatusCode::NO_CONTENT);

    // Old session should now be invalid
    let (status, _body) =
        request_json(&app, Method::GET, "/api/v1/auth/me", None, Some(&cookie)).await;
    assert_eq!(
        status,
        StatusCode::UNAUTHORIZED,
        "old session must be rejected after rotation"
    );

    // Clean up
    sqlx::query("DELETE FROM sessions WHERE user_id = $1")
        .bind(user.user_id)
        .execute(&pool)
        .await
        .expect("cleanup sessions");
    sqlx::query("DELETE FROM login_attempts WHERE email = $1")
        .bind(user.email.to_lowercase())
        .execute(&pool)
        .await
        .expect("cleanup login attempts");
    // audit_events is append-only; FK ON DELETE SET NULL handles cleanup.
    sqlx::query("DELETE FROM users WHERE id = $1")
        .bind(user.user_id)
        .execute(&pool)
        .await
        .expect("cleanup user");
    sqlx::query("DELETE FROM profiles WHERE id = $1")
        .bind(user.profile_id)
        .execute(&pool)
        .await
        .expect("cleanup profile");
}

#[tokio::test]
async fn fresh_session_survives_self_rotation() {
    let Some(database_url) = std::env::var("GOBROWSE_TEST_DATABASE_URL").ok() else {
        eprintln!(
            "GOBROWSE_TEST_DATABASE_URL is unset; skipping session rotation integration test"
        );
        return;
    };
    let _lock = common::acquire_test_lock(&database_url).await;
    let pool = test_pool().await.expect("test pool after lock");

    let state = AppState::new(pool.clone(), test_settings(&database_url))
        .await
        .expect("create app state");
    let app = gobrowse_server::router(state.clone());

    let email = format!("fresh-{}@example.test", Uuid::now_v7().simple());
    let password = "long-enough-password";
    let user =
        create_user_with_password(&pool, &state, &email, "Fresh Test", password, "OWNER").await;

    // Login to get a cookie
    let (status, login_body) = request_json(
        &app,
        Method::POST,
        "/api/v1/auth/login",
        Some(serde_json::json!({"email": email, "password": password})),
        None,
    )
    .await;
    assert_eq!(status, StatusCode::OK, "login should succeed: {login_body}");

    // Extract cookie from login response (since we can't access the Set-Cookie header easily with oneshot)
    // Instead, create a new session manually simulating a post-rotation login
    let new_cookie = create_session(&pool, user.user_id, 1).await;

    // Rotate with the new session
    let (status, _body) = request_json(
        &app,
        Method::POST,
        "/api/v1/auth/rotate",
        Some(serde_json::json!({"user_id": user.user_id})),
        Some(&new_cookie),
    )
    .await;
    assert_eq!(status, StatusCode::NO_CONTENT);

    // Now the auth_epoch is bumped to 2. Login again to get a session with epoch 2.
    let (status, _login_body) = request_json(
        &app,
        Method::POST,
        "/api/v1/auth/login",
        Some(serde_json::json!({"email": email, "password": password})),
        None,
    )
    .await;
    assert_eq!(
        status,
        StatusCode::OK,
        "login after rotation should succeed"
    );

    // Create a fresh session with the bumped epoch
    let fresh_cookie = create_session(&pool, user.user_id, 2).await;

    let (status, body) = request_json(
        &app,
        Method::GET,
        "/api/v1/auth/me",
        None,
        Some(&fresh_cookie),
    )
    .await;
    assert_eq!(
        status,
        StatusCode::OK,
        "fresh session after rotation should be valid: {body}"
    );
    assert_eq!(
        body.get("id")
            .and_then(|v| v.as_str())
            .map(|s| s.to_string()),
        Some(user.user_id.to_string()),
        "me response must return the correct user id"
    );

    // Clean up
    sqlx::query("DELETE FROM sessions WHERE user_id = $1")
        .bind(user.user_id)
        .execute(&pool)
        .await
        .expect("cleanup sessions");
    sqlx::query("DELETE FROM login_attempts WHERE email = $1")
        .bind(user.email.to_lowercase())
        .execute(&pool)
        .await
        .expect("cleanup login attempts");
    // audit_events is append-only; FK ON DELETE SET NULL handles cleanup.
    sqlx::query("DELETE FROM users WHERE id = $1")
        .bind(user.user_id)
        .execute(&pool)
        .await
        .expect("cleanup user");
    sqlx::query("DELETE FROM profiles WHERE id = $1")
        .bind(user.profile_id)
        .execute(&pool)
        .await
        .expect("cleanup profile");
}

#[tokio::test]
async fn disabled_user_cannot_login_after_disable() {
    let Some(database_url) = std::env::var("GOBROWSE_TEST_DATABASE_URL").ok() else {
        eprintln!(
            "GOBROWSE_TEST_DATABASE_URL is unset; skipping session rotation integration test"
        );
        return;
    };
    let _lock = common::acquire_test_lock(&database_url).await;
    let pool = test_pool().await.expect("test pool after lock");

    let state = AppState::new(pool.clone(), test_settings(&database_url))
        .await
        .expect("create app state");
    let app = gobrowse_server::router(state.clone());

    let email = format!("disabled-{}@example.test", Uuid::now_v7().simple());
    let password = "long-enough-password";
    let user =
        create_user_with_password(&pool, &state, &email, "Disabled Test", password, "OWNER").await;

    // Verify user can login before disable
    let (status, _body) = request_json(
        &app,
        Method::POST,
        "/api/v1/auth/login",
        Some(serde_json::json!({"email": email, "password": password})),
        None,
    )
    .await;
    assert_eq!(
        status,
        StatusCode::OK,
        "login before disable should succeed"
    );

    // Disable the user
    sqlx::query("UPDATE users SET disabled_at = now() WHERE id = $1")
        .bind(user.user_id)
        .execute(&pool)
        .await
        .expect("disable user");

    // Attempt login after disable
    let (status, body) = request_json(
        &app,
        Method::POST,
        "/api/v1/auth/login",
        Some(serde_json::json!({"email": email, "password": password})),
        None,
    )
    .await;
    assert_eq!(
        status,
        StatusCode::UNAUTHORIZED,
        "disabled user must not be able to login: {body}"
    );

    // Clean up
    sqlx::query("DELETE FROM sessions WHERE user_id = $1")
        .bind(user.user_id)
        .execute(&pool)
        .await
        .expect("cleanup sessions");
    sqlx::query("DELETE FROM login_attempts WHERE email = $1")
        .bind(user.email.to_lowercase())
        .execute(&pool)
        .await
        .expect("cleanup login attempts");
    // audit_events is append-only; FK ON DELETE SET NULL handles cleanup.
    sqlx::query("DELETE FROM users WHERE id = $1")
        .bind(user.user_id)
        .execute(&pool)
        .await
        .expect("cleanup user");
    sqlx::query("DELETE FROM profiles WHERE id = $1")
        .bind(user.profile_id)
        .execute(&pool)
        .await
        .expect("cleanup profile");
}

#[tokio::test]
async fn admin_and_owner_rotation_is_limited_to_their_profile() {
    let Some(database_url) = std::env::var("GOBROWSE_TEST_DATABASE_URL").ok() else {
        eprintln!(
            "GOBROWSE_TEST_DATABASE_URL is unset; skipping session rotation integration test"
        );
        return;
    };
    let _lock = common::acquire_test_lock(&database_url).await;
    let pool = test_pool().await.expect("test pool after lock");

    let state = AppState::new(pool.clone(), test_settings(&database_url))
        .await
        .expect("create app state");
    let app = gobrowse_server::router(state.clone());

    let admin = create_user_with_password(
        &pool,
        &state,
        &format!("admin-{}@example.test", Uuid::now_v7().simple()),
        "Profile Admin",
        "long-enough-password",
        "ADMIN",
    )
    .await;
    let owner = create_user_in_profile_with_password(
        &pool,
        &state,
        admin.profile_id,
        &format!("owner-{}@example.test", Uuid::now_v7().simple()),
        "Profile Owner",
        "long-enough-password",
        "OWNER",
    )
    .await;
    let same_profile_target = create_user_in_profile_with_password(
        &pool,
        &state,
        admin.profile_id,
        &format!("same-{}@example.test", Uuid::now_v7().simple()),
        "Same Profile Target",
        "long-enough-password",
        "MEMBER",
    )
    .await;
    let cross_profile_target = create_user_with_password(
        &pool,
        &state,
        &format!("cross-{}@example.test", Uuid::now_v7().simple()),
        "Cross Profile Target",
        "long-enough-password",
        "MEMBER",
    )
    .await;

    let admin_cookie = create_session(&pool, admin.user_id, 1).await;
    let owner_cookie = create_session(&pool, owner.user_id, 1).await;
    let same_profile_cookie = create_session(&pool, same_profile_target.user_id, 1).await;
    let cross_profile_cookie = create_session(&pool, cross_profile_target.user_id, 1).await;

    let (status, body) = request_json(
        &app,
        Method::POST,
        "/api/v1/auth/rotate",
        Some(serde_json::json!({"user_id": cross_profile_target.user_id})),
        Some(&admin_cookie),
    )
    .await;
    assert_eq!(
        status,
        StatusCode::FORBIDDEN,
        "admin must not rotate a cross-profile user: {body}"
    );

    let (status, body) = request_json(
        &app,
        Method::POST,
        "/api/v1/auth/rotate",
        Some(serde_json::json!({"user_id": cross_profile_target.user_id})),
        Some(&owner_cookie),
    )
    .await;
    assert_eq!(
        status,
        StatusCode::FORBIDDEN,
        "owner must not have an otherwise unsupported global rotation capability: {body}"
    );

    let (status, body) = request_json(
        &app,
        Method::GET,
        "/api/v1/auth/me",
        None,
        Some(&cross_profile_cookie),
    )
    .await;
    assert_eq!(
        status,
        StatusCode::OK,
        "cross-profile target session must remain valid: {body}"
    );

    let (status, body) = request_json(
        &app,
        Method::POST,
        "/api/v1/auth/rotate",
        Some(serde_json::json!({"user_id": same_profile_target.user_id})),
        Some(&admin_cookie),
    )
    .await;
    assert_eq!(
        status,
        StatusCode::NO_CONTENT,
        "admin should rotate a same-profile user: {body}"
    );

    let (status, body) = request_json(
        &app,
        Method::GET,
        "/api/v1/auth/me",
        None,
        Some(&same_profile_cookie),
    )
    .await;
    assert_eq!(
        status,
        StatusCode::UNAUTHORIZED,
        "same-profile target session must be invalidated: {body}"
    );

    sqlx::query("DELETE FROM sessions WHERE user_id = ANY($1)")
        .bind(vec![
            admin.user_id,
            owner.user_id,
            same_profile_target.user_id,
            cross_profile_target.user_id,
        ])
        .execute(&pool)
        .await
        .expect("cleanup sessions");
    sqlx::query("DELETE FROM users WHERE id = ANY($1)")
        .bind(vec![
            admin.user_id,
            owner.user_id,
            same_profile_target.user_id,
            cross_profile_target.user_id,
        ])
        .execute(&pool)
        .await
        .expect("cleanup users");
    sqlx::query("DELETE FROM profiles WHERE id = ANY($1)")
        .bind(vec![admin.profile_id, cross_profile_target.profile_id])
        .execute(&pool)
        .await
        .expect("cleanup profiles");
}

#[tokio::test]
async fn non_admin_cannot_rotate_other_user() {
    let Some(database_url) = std::env::var("GOBROWSE_TEST_DATABASE_URL").ok() else {
        eprintln!(
            "GOBROWSE_TEST_DATABASE_URL is unset; skipping session rotation integration test"
        );
        return;
    };
    let _lock = common::acquire_test_lock(&database_url).await;
    let pool = test_pool().await.expect("test pool after lock");

    let state = AppState::new(pool.clone(), test_settings(&database_url))
        .await
        .expect("create app state");
    let app = gobrowse_server::router(state.clone());

    let user_a = create_user_with_password(
        &pool,
        &state,
        &format!("a-{}@example.test", Uuid::now_v7().simple()),
        "User A",
        "long-enough-password",
        "MEMBER",
    )
    .await;

    let user_b = create_user_with_password(
        &pool,
        &state,
        &format!("b-{}@example.test", Uuid::now_v7().simple()),
        "User B",
        "long-enough-password",
        "MEMBER",
    )
    .await;

    let cookie_a = create_session(&pool, user_a.user_id, 1).await;

    // User A (MEMBER) tries to rotate User B (MEMBER) — must be forbidden
    let (status, body) = request_json(
        &app,
        Method::POST,
        "/api/v1/auth/rotate",
        Some(serde_json::json!({"user_id": user_b.user_id})),
        Some(&cookie_a),
    )
    .await;
    assert_eq!(
        status,
        StatusCode::FORBIDDEN,
        "non-admin must not be able to rotate another user: {body}"
    );

    // Verify User B's sessions are still valid (auth_epoch unchanged)
    let b_epoch: i64 = sqlx::query_scalar("SELECT auth_epoch FROM users WHERE id = $1")
        .bind(user_b.user_id)
        .fetch_one(&pool)
        .await
        .expect("read user b epoch");
    assert_eq!(b_epoch, 1, "user B auth_epoch must be unchanged");

    // Clean up
    for user in &[&user_a, &user_b] {
        sqlx::query("DELETE FROM sessions WHERE user_id = $1")
            .bind(user.user_id)
            .execute(&pool)
            .await
            .expect("cleanup sessions");
        sqlx::query("DELETE FROM login_attempts WHERE email = $1")
            .bind(user.email.to_lowercase())
            .execute(&pool)
            .await
            .expect("cleanup login attempts");
        // audit_events is append-only; FK ON DELETE SET NULL handles cleanup.
        sqlx::query("DELETE FROM users WHERE id = $1")
            .bind(user.user_id)
            .execute(&pool)
            .await
            .expect("cleanup user");
        sqlx::query("DELETE FROM profiles WHERE id = $1")
            .bind(user.profile_id)
            .execute(&pool)
            .await
            .expect("cleanup profile");
    }
}
