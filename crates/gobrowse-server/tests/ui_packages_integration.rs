//! M24b UI package install flow integration tests.
//! Mirrors plugin_integration style but uses local_package inline manifests.
//! Gated on GOBROWSE_TEST_DATABASE_URL, serializes via advisory lock.

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
    db,
};
use http_body_util::BodyExt;
use secrecy::SecretString;
use serde_json::{Value, json};
use sha2::{Digest, Sha256};
use sqlx::PgPool;
use time::{Duration, OffsetDateTime};
use tower::ServiceExt;
use uuid::Uuid;

fn theme_manifest(name: &str, version: &str) -> Value {
    json!({
        "kind": "gobrowse-ui",
        "manifest_version": 1,
        "name": name,
        "version": version,
        "ui_kind": "THEME",
        "description": "test theme for integration",
        "publisher": { "name": "Test", "url": "https://example.test" },
        "license": "MIT",
        "api_version": "v1",
        "min_api_version": "v1",
        "entry": null,
        "capabilities": ["conversations"],
        "theme": { "variables": { "--ink": "#111111", "--canvas": "#eeeeee" } }
    })
}

#[allow(dead_code)]
fn full_ui_manifest(name: &str, version: &str) -> Value {
    json!({
        "kind": "gobrowse-ui",
        "manifest_version": 1,
        "name": name,
        "version": version,
        "ui_kind": "FULL_UI",
        "description": "test full ui",
        "api_version": "v1",
        "entry": "index.html",
        "capabilities": ["conversations","library"],
        "theme": null
    })
}

fn invalid_manifest() -> Value {
    json!({ "kind": "gobrowse-ui", "manifest_version": 99, "name": "", "version": "1.0.0", "ui_kind": "UNKNOWN", "api_version": "v1" })
}

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
        http: HttpSettings {
            public_origin: "http://127.0.0.1:8080".parse().unwrap(),
            ..HttpSettings::default()
        },
        database: DatabaseSettings {
            url: SecretString::from(database_url.to_owned()),
            max_connections: 10,
        },
        auth: AuthSettings::default(),
        vault: VaultSettings::default(),
        features: FeatureSettings {
            ui_packages_dir: std::env::temp_dir()
                .join(format!("gobrowse-ui-test-{}", Uuid::new_v4())),
            ..FeatureSettings::default()
        },
        observability: ObservabilitySettings::default(),
    }
}

async fn create_session(pool: &PgPool, profile_id: Uuid, role: &str) -> (Uuid, String) {
    let user_id = Uuid::now_v7();
    let token = format!("ui-session-{user_id}");
    sqlx::query("INSERT INTO users (id,email,display_name,password_hash,role,primary_profile_id) VALUES ($1,$2,'UI Test User','unused',$3,$4)")
        .bind(user_id).bind(format!("{user_id}@example.test")).bind(role).bind(profile_id)
        .execute(pool).await.expect("create user");
    let now = OffsetDateTime::now_utc();
    sqlx::query("INSERT INTO sessions (token_hash,user_id,auth_epoch,expires_at,absolute_expires_at) VALUES ($1,$2,1,$3,$4)")
        .bind(Sha256::digest(token.as_bytes()).to_vec()).bind(user_id)
        .bind(now + Duration::hours(1)).bind(now + Duration::hours(2))
        .execute(pool).await.expect("create session");
    (user_id, format!("gobrowse_session={token}"))
}

async fn create_profile(pool: &PgPool) -> Uuid {
    let profile_id = Uuid::now_v7();
    sqlx::query("INSERT INTO profiles (id, name) VALUES ($1, 'ui-test-profile')")
        .bind(profile_id)
        .execute(pool)
        .await
        .expect("create profile");
    profile_id
}

async fn request_json(
    app: &Router,
    method: Method,
    uri: &str,
    cookie: &str,
    body: Option<Value>,
) -> (StatusCode, Value) {
    let state_change = matches!(
        method,
        Method::POST | Method::PUT | Method::PATCH | Method::DELETE
    );
    let mut builder = Request::builder()
        .method(method)
        .uri(uri)
        .header(header::COOKIE, cookie);
    if state_change {
        builder = builder.header(header::ORIGIN, "http://127.0.0.1:8080");
    }
    let body_bytes = if let Some(b) = body {
        builder = builder.header(header::CONTENT_TYPE, "application/json");
        Body::from(b.to_string())
    } else {
        Body::empty()
    };
    let resp = app
        .clone()
        .oneshot(builder.body(body_bytes).expect("build"))
        .await
        .expect("route");
    let status = resp.status();
    let bytes = resp
        .into_body()
        .collect()
        .await
        .expect("collect")
        .to_bytes();
    let val = if bytes.is_empty() {
        Value::Null
    } else {
        serde_json::from_slice(&bytes)
            .unwrap_or_else(|_| Value::String(String::from_utf8_lossy(&bytes).into_owned()))
    };
    (status, val)
}

async fn request_raw(
    app: &Router,
    method: Method,
    uri: &str,
    cookie: &str,
) -> (StatusCode, axum::http::HeaderMap, Vec<u8>) {
    let mut builder = Request::builder().method(method).uri(uri);
    if !cookie.is_empty() {
        builder = builder.header(header::COOKIE, cookie);
    }
    let resp = app
        .clone()
        .oneshot(builder.body(Body::empty()).expect("build"))
        .await
        .expect("route");
    let status = resp.status();
    let headers = resp.headers().clone();
    let bytes = resp
        .into_body()
        .collect()
        .await
        .expect("collect")
        .to_bytes()
        .to_vec();
    (status, headers, bytes)
}

fn preview_body(manifest: &Value) -> Value {
    json!({ "source_type": "local_package", "source_uri": format!("inline:{}", manifest), "manifest": manifest })
}
fn install_body(manifest: &Value, digest: &str, approve: bool) -> Value {
    json!({ "source_type": "local_package", "source_uri": format!("inline:{}", manifest), "expected_digest": digest, "approve": approve, "manifest": manifest })
}

#[tokio::test]
async fn schema_24_migration_applies() {
    let Some(pool) = test_pool().await else {
        return;
    };
    let url = std::env::var("GOBROWSE_TEST_DATABASE_URL").unwrap();
    let _lock = common::acquire_test_lock(&url).await;
    let version: i64 =
        sqlx::query_scalar("SELECT schema_version FROM schema_metadata WHERE singleton")
            .fetch_one(&pool)
            .await
            .expect("schema_version");
    assert_eq!(version, 26, "schema should be 26 after M25b");
}

#[tokio::test]
async fn preview_valid_ui_package() {
    let Some(pool) = test_pool().await else {
        return;
    };
    let url = std::env::var("GOBROWSE_TEST_DATABASE_URL").unwrap();
    let _lock = common::acquire_test_lock(&url).await;
    let profile_id = create_profile(&pool).await;
    let (_uid, cookie) = create_session(&pool, profile_id, "OWNER").await;
    let settings = test_settings(&url);
    let state = AppState::new(pool.clone(), settings)
        .await
        .expect("app state");
    let app = gobrowse_server::router(state);
    let manifest = theme_manifest("preview-theme", "1.0.0");
    let (status, body) = request_json(
        &app,
        Method::POST,
        "/api/v1/ui/preview",
        &cookie,
        Some(preview_body(&manifest)),
    )
    .await;
    assert_eq!(status, StatusCode::OK, "preview: {body}");
    assert_eq!(body["ui_kind"], "THEME");
    assert!(
        body["digest"]
            .as_str()
            .map(|s| s.len() == 64)
            .unwrap_or(false),
        "digest len 64"
    );
}

#[tokio::test]
async fn preview_invalid_manifest_rejects() {
    let Some(pool) = test_pool().await else {
        return;
    };
    let url = std::env::var("GOBROWSE_TEST_DATABASE_URL").unwrap();
    let _lock = common::acquire_test_lock(&url).await;
    let profile_id = create_profile(&pool).await;
    let (_uid, cookie) = create_session(&pool, profile_id, "OWNER").await;
    let settings = test_settings(&url);
    let state = AppState::new(pool.clone(), settings)
        .await
        .expect("app state");
    let app = gobrowse_server::router(state);
    let manifest = invalid_manifest();
    let (status, body) = request_json(
        &app,
        Method::POST,
        "/api/v1/ui/preview",
        &cookie,
        Some(preview_body(&manifest)),
    )
    .await;
    assert!(
        status == StatusCode::BAD_REQUEST || status == StatusCode::UNPROCESSABLE_ENTITY,
        "invalid manifest should be rejected, got {status} {body}"
    );
}

#[tokio::test]
async fn install_requires_matching_digest_and_approve() {
    let Some(pool) = test_pool().await else {
        return;
    };
    let url = std::env::var("GOBROWSE_TEST_DATABASE_URL").unwrap();
    let _lock = common::acquire_test_lock(&url).await;
    let profile_id = create_profile(&pool).await;
    let (_uid, cookie) = create_session(&pool, profile_id, "OWNER").await;
    let settings = test_settings(&url);
    let state = AppState::new(pool.clone(), settings)
        .await
        .expect("app state");
    let app = gobrowse_server::router(state);
    let manifest = theme_manifest("digest-theme", "1.0.0");
    let (status, preview) = request_json(
        &app,
        Method::POST,
        "/api/v1/ui/preview",
        &cookie,
        Some(preview_body(&manifest)),
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    let good_digest = preview["digest"].as_str().unwrap().to_owned();
    // wrong digest
    let bad = "a".repeat(64);
    let (status, body) = request_json(
        &app,
        Method::POST,
        "/api/v1/ui/install",
        &cookie,
        Some(install_body(&manifest, &bad, true)),
    )
    .await;
    assert_eq!(
        status,
        StatusCode::CONFLICT,
        "bad digest should fail: {body}"
    );
    // missing approve
    let (status, body) = request_json(
        &app,
        Method::POST,
        "/api/v1/ui/install",
        &cookie,
        Some(install_body(&manifest, &good_digest, false)),
    )
    .await;
    assert_eq!(
        status,
        StatusCode::UNPROCESSABLE_ENTITY,
        "approve false should fail: {body}"
    );
    // good
    let (status, body) = request_json(
        &app,
        Method::POST,
        "/api/v1/ui/install",
        &cookie,
        Some(install_body(&manifest, &good_digest, true)),
    )
    .await;
    assert_eq!(status, StatusCode::OK, "good install: {body}");
    assert_eq!(body["state"], "candidate");
}

#[tokio::test]
async fn full_lifecycle_list_get_activate_rollback_delete_and_csp() {
    let Some(pool) = test_pool().await else {
        return;
    };
    let url = std::env::var("GOBROWSE_TEST_DATABASE_URL").unwrap();
    let _lock = common::acquire_test_lock(&url).await;
    let profile_id = create_profile(&pool).await;
    let (_uid, cookie) = create_session(&pool, profile_id, "OWNER").await;
    let settings = test_settings(&url);
    let state = AppState::new(pool.clone(), settings)
        .await
        .expect("app state");
    let app = gobrowse_server::router(state);
    // install theme A
    let manifest_a = theme_manifest(&format!("lifecycle-a-{}", Uuid::now_v7()), "1.0.0");
    let (status, preview_a) = request_json(
        &app,
        Method::POST,
        "/api/v1/ui/preview",
        &cookie,
        Some(preview_body(&manifest_a)),
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    let digest_a = preview_a["digest"].as_str().unwrap().to_owned();
    let (status, installed_a) = request_json(
        &app,
        Method::POST,
        "/api/v1/ui/install",
        &cookie,
        Some(install_body(&manifest_a, &digest_a, true)),
    )
    .await;
    assert_eq!(status, StatusCode::OK, "install A: {installed_a}");
    let id_a = installed_a["id"].as_str().unwrap().to_owned();
    // companion book exists
    let book_count: i64 =
        sqlx::query_scalar("SELECT COUNT(*) FROM books WHERE kind='GOBROWSE_UI' AND profile_id=$1")
            .bind(profile_id)
            .fetch_one(&pool)
            .await
            .unwrap();
    assert!(book_count >= 1, "companion book should exist");
    // list
    let (status, list) =
        request_json(&app, Method::GET, "/api/v1/ui/packages", &cookie, None).await;
    assert_eq!(status, StatusCode::OK);
    assert!(
        list.as_array()
            .map(|a| a.iter().any(|v| v["id"] == id_a))
            .unwrap_or(false),
        "list should contain A"
    );
    // get
    let (status, detail) = request_json(
        &app,
        Method::GET,
        &format!("/api/v1/ui/packages/{id_a}"),
        &cookie,
        None,
    )
    .await;
    assert_eq!(status, StatusCode::OK, "get: {detail}");
    assert_eq!(detail["name"], manifest_a["name"]);
    assert_eq!(detail["state"], "candidate");
    // table state staged
    let state_db: String = sqlx::query_scalar("SELECT state FROM ui_packages WHERE id=$1")
        .bind(Uuid::parse_str(&id_a).unwrap())
        .fetch_one(&pool)
        .await
        .unwrap();
    assert_eq!(state_db, "candidate");
    // activate A -> active
    let (status, act) = request_json(
        &app,
        Method::POST,
        &format!("/api/v1/ui/packages/{id_a}/activate"),
        &cookie,
        None,
    )
    .await;
    assert_eq!(status, StatusCode::OK, "activate A: {act}");
    assert_eq!(act["state"], "active");
    let active_count: i64 = sqlx::query_scalar(
        "SELECT COUNT(*) FROM ui_packages WHERE profile_id=$1 AND state='active'",
    )
    .bind(profile_id)
    .fetch_one(&pool)
    .await
    .unwrap();
    assert_eq!(active_count, 1);
    // install theme B
    let manifest_b = theme_manifest(&format!("lifecycle-b-{}", Uuid::now_v7()), "1.0.0");
    let (status, preview_b) = request_json(
        &app,
        Method::POST,
        "/api/v1/ui/preview",
        &cookie,
        Some(preview_body(&manifest_b)),
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    let digest_b = preview_b["digest"].as_str().unwrap().to_owned();
    let (status, installed_b) = request_json(
        &app,
        Method::POST,
        "/api/v1/ui/install",
        &cookie,
        Some(install_body(&manifest_b, &digest_b, true)),
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    let id_b = installed_b["id"].as_str().unwrap().to_owned();
    // activate B -> A becomes previous
    let (status, act_b) = request_json(
        &app,
        Method::POST,
        &format!("/api/v1/ui/packages/{id_b}/activate"),
        &cookie,
        None,
    )
    .await;
    assert_eq!(status, StatusCode::OK, "activate B: {act_b}");
    let state_a: String = sqlx::query_scalar("SELECT state FROM ui_packages WHERE id=$1")
        .bind(Uuid::parse_str(&id_a).unwrap())
        .fetch_one(&pool)
        .await
        .unwrap();
    assert_eq!(state_a, "previous", "A should be previous after B active");
    let state_b: String = sqlx::query_scalar("SELECT state FROM ui_packages WHERE id=$1")
        .bind(Uuid::parse_str(&id_b).unwrap())
        .fetch_one(&pool)
        .await
        .unwrap();
    assert_eq!(state_b, "active");
    // CSP header on built-in (GET /) — should have CSP and no unsafe-inline
    let (status, headers, _body) = request_raw(&app, Method::GET, "/", &cookie).await;
    assert!(
        status == StatusCode::OK || status == StatusCode::NOT_FOUND,
        "GET / status {status}"
    );
    let csp = headers
        .get("content-security-policy")
        .and_then(|v| v.to_str().ok())
        .unwrap_or("")
        .to_owned();
    assert!(!csp.is_empty(), "CSP header should be present on /");
    // script-src must not allow inline scripts (style-src-attr is
    // intentionally relaxed to 'unsafe-inline' so UI style attributes work).
    let script_src = csp
        .split(';')
        .find_map(|d| d.trim().strip_prefix("script-src"))
        .unwrap_or("");
    assert!(
        !script_src.contains("unsafe-inline"),
        "CSP script-src must not contain unsafe-inline"
    );
    assert!(
        csp.contains("default-src"),
        "CSP should contain default-src"
    );
    // CSP on /recovery
    let (status, headers, _body) = request_raw(&app, Method::GET, "/recovery", "").await;
    assert_eq!(status, StatusCode::OK, "recovery should be accessible");
    let csp_rec = headers
        .get("content-security-policy")
        .and_then(|v| v.to_str().ok())
        .unwrap_or("")
        .to_owned();
    // recovery should also have CSP or at least not unsafe-inline if present
    if !csp_rec.is_empty() {
        let script_src_rec = csp_rec
            .split(';')
            .find_map(|d| d.trim().strip_prefix("script-src"))
            .unwrap_or("");
        assert!(
            !script_src_rec.contains("unsafe-inline"),
            "recovery CSP script-src no unsafe-inline"
        );
    }
    // rollback -> B becomes previous/rolled_back, A active again
    let (status, rb) = request_json(&app, Method::POST, "/api/v1/ui/rollback", &cookie, None).await;
    assert_eq!(status, StatusCode::OK, "rollback: {rb}");
    let state_a2: String = sqlx::query_scalar("SELECT state FROM ui_packages WHERE id=$1")
        .bind(Uuid::parse_str(&id_a).unwrap())
        .fetch_one(&pool)
        .await
        .unwrap();
    assert_eq!(state_a2, "active", "A should be active after rollback");
    // delete rejects active
    let (status, body) = request_json(
        &app,
        Method::DELETE,
        &format!("/api/v1/ui/packages/{id_a}"),
        &cookie,
        None,
    )
    .await;
    assert!(
        status == StatusCode::BAD_REQUEST
            || status == StatusCode::CONFLICT
            || status == StatusCode::FORBIDDEN
            || status == StatusCode::UNPROCESSABLE_ENTITY,
        "delete active should be rejected, got {status} {body}"
    );
    // delete rejects last-known-good when only one candidate remains? Create single-package scenario in new profile
    // Instead test deleting B (previous) should succeed after rollback
    let (status, body) = request_json(
        &app,
        Method::DELETE,
        &format!("/api/v1/ui/packages/{id_b}"),
        &cookie,
        None,
    )
    .await;
    // B is now rolled_back/previous, delete should succeed (not active)
    // Accept either OK or 204, but our API returns 204 on success (Value::Null)
    assert!(
        status == StatusCode::OK || status == StatusCode::NO_CONTENT,
        "delete B should succeed, got {status} {body}"
    );
    // verify B gone
    let cnt: i64 = sqlx::query_scalar("SELECT COUNT(*) FROM ui_packages WHERE id=$1")
        .bind(Uuid::parse_str(&id_b).unwrap())
        .fetch_one(&pool)
        .await
        .unwrap();
    assert_eq!(cnt, 0);
    // schema still 24
    let ver: i64 = sqlx::query_scalar("SELECT schema_version FROM schema_metadata WHERE singleton")
        .fetch_one(&pool)
        .await
        .unwrap();
    assert_eq!(ver, 26);
}

#[tokio::test]
async fn capabilities_and_recovery_accessible_without_auth() {
    let Some(pool) = test_pool().await else {
        return;
    };
    let url = std::env::var("GOBROWSE_TEST_DATABASE_URL").unwrap();
    let _lock = common::acquire_test_lock(&url).await;
    let settings = test_settings(&url);
    let state = AppState::new(pool.clone(), settings)
        .await
        .expect("app state");
    let app = gobrowse_server::router(state);
    // capabilities no auth
    let (status, body) = request_json(&app, Method::GET, "/api/v1/capabilities", "", None).await;
    assert_eq!(status, StatusCode::OK, "capabilities: {body}");
    assert_eq!(body["api_version"], "v1");
    assert_eq!(body["schema_version"], 26);
    assert!(
        body["tools"]
            .as_array()
            .map(|a| !a.is_empty())
            .unwrap_or(false),
        "tools should be non-empty"
    );
    assert_eq!(body["features"]["ui_packages"], true);
    // recovery no auth
    let (status, _headers, bytes) = request_raw(&app, Method::GET, "/recovery", "").await;
    assert_eq!(status, StatusCode::OK);
    let text = String::from_utf8_lossy(&bytes);
    assert!(
        text.contains("Recovery") || text.contains("recovery") || text.contains("Gobrowse"),
        "recovery body should contain Recovery"
    );
}

#[tokio::test]
async fn delete_rejects_last_known_good() {
    let Some(pool) = test_pool().await else {
        return;
    };
    let url = std::env::var("GOBROWSE_TEST_DATABASE_URL").unwrap();
    let _lock = common::acquire_test_lock(&url).await;
    let profile_id = create_profile(&pool).await;
    let (_uid, cookie) = create_session(&pool, profile_id, "OWNER").await;
    let settings = test_settings(&url);
    let state = AppState::new(pool.clone(), settings)
        .await
        .expect("app state");
    let app = gobrowse_server::router(state);
    let manifest = theme_manifest(&format!("single-{}", Uuid::now_v7()), "1.0.0");
    let (status, preview) = request_json(
        &app,
        Method::POST,
        "/api/v1/ui/preview",
        &cookie,
        Some(preview_body(&manifest)),
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    let digest = preview["digest"].as_str().unwrap().to_owned();
    let (status, installed) = request_json(
        &app,
        Method::POST,
        "/api/v1/ui/install",
        &cookie,
        Some(install_body(&manifest, &digest, true)),
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    let id = installed["id"].as_str().unwrap().to_owned();
    // activate it
    let (status, _) = request_json(
        &app,
        Method::POST,
        &format!("/api/v1/ui/packages/{id}/activate"),
        &cookie,
        None,
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    // now try delete active — should be rejected (last-known-good protection)
    let (status, body) = request_json(
        &app,
        Method::DELETE,
        &format!("/api/v1/ui/packages/{id}"),
        &cookie,
        None,
    )
    .await;
    assert!(
        status == StatusCode::BAD_REQUEST
            || status == StatusCode::CONFLICT
            || status == StatusCode::UNPROCESSABLE_ENTITY,
        "delete last active should be rejected, got {status} {body}"
    );
}
