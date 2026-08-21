//! Lane C plugin install flow integration tests.
//!
//! Gated on `GOBROWSE_TEST_DATABASE_URL` like the other DB integration tests.
//! A local axum mock server stands in for the GitHub REST + raw-content API
//! (base URLs injected through `FeatureSettings::github_api_base_url` /
//! `github_raw_base_url`), so the whole preview → install → upgrade → rollback
//! → uninstall pipeline runs against real HTTP + PostgreSQL without touching
//! the network or a sandbox daemon (the test manifests declare no executable
//! components or self_test).

mod common;

use std::{collections::HashMap, fmt::Write as _, io::Write, sync::Arc};

use axum::{
    Json, Router,
    body::Body,
    extract::{Path, Query, State},
    http::{Method, Request, StatusCode, header},
    routing::get,
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
use sqlx::{PgPool, Row};
use time::{Duration, OffsetDateTime};
use tokio::net::TcpListener;
use tower::ServiceExt;
use url::Url;
use uuid::Uuid;

const V1_TAG: &str = "v1.0.0";
const V2_TAG: &str = "v2.0.0";
const V3_TAG: &str = "v3.0.0";
const V1_SHA: &str = "1111111111111111111111111111111111111111";
const V2_SHA: &str = "2222222222222222222222222222222222222222";
const V3_SHA: &str = "3333333333333333333333333333333333333333";

// ---------------------------------------------------------------------------
// Mock GitHub
// ---------------------------------------------------------------------------

struct MockGitHub {
    base: Url,
    raw_base: Url,
    join: tokio::task::JoinHandle<()>,
}

impl Drop for MockGitHub {
    fn drop(&mut self) {
        self.join.abort();
    }
}

fn sha_for_tag(tag: &str) -> &'static str {
    match tag {
        V1_TAG => V1_SHA,
        V2_TAG => V2_SHA,
        V3_TAG => V3_SHA,
        _ => V1_SHA,
    }
}

fn manifest_for_sha(sha: &str) -> Value {
    match sha {
        V1_SHA => manifest_v1(),
        V2_SHA => manifest_v2(),
        V3_SHA => manifest_invalid(),
        _ => manifest_v1(),
    }
}

fn manifest_v1() -> Value {
    json!({
        "manifest_version": 1,
        "name": "demo-plugin",
        "version": "1.0.0",
        "description": "Demo plugin v1",
        "publisher": { "name": "demo-labs", "url": "https://example.com" },
        "permissions": {
            "filesystem_read": ["/workspace/**"],
            "secrets": ["DEMO_TOKEN"]
        },
        "components": [
            { "type": "skill", "name": "run-demo", "ref": "skills/run-demo.md" },
            { "type": "source_book", "name": "demo-runbook", "ref": "docs/runbook.md" }
        ]
    })
}

fn manifest_v2() -> Value {
    json!({
        "manifest_version": 1,
        "name": "demo-plugin",
        "version": "2.0.0",
        "description": "Demo plugin v2",
        "publisher": { "name": "demo-labs" },
        "permissions": {
            "filesystem_read": ["/workspace/**"],
            "filesystem_write": ["/workspace/out/**"],
            "network": ["api.example.com:443"]
        },
        "components": [
            { "type": "skill", "name": "run-demo-v2", "ref": "skills/run-demo-v2.md" },
            { "type": "schema", "name": "workflow-v2", "ref": "schemas/workflow.json" }
        ]
    })
}

fn manifest_invalid() -> Value {
    json!({
        "manifest_version": 1,
        "name": "demo-plugin",
        "version": "3.0.0",
        "description": "invalid",
        "publisher": { "name": "demo-labs" },
        "components": []
    })
}

fn artifact_zip(tag: &str) -> Vec<u8> {
    let cursor = std::io::Cursor::new(Vec::new());
    let mut writer = zip::ZipWriter::new(cursor);
    let options = zip::write::SimpleFileOptions::default();
    writer
        .start_file(format!("skills/run-{tag}.md"), options)
        .unwrap();
    writer.write_all(format!("# run {tag}").as_bytes()).unwrap();
    writer.finish().unwrap().into_inner()
}

fn sha256_hex(bytes: &[u8]) -> String {
    let digest = Sha256::digest(bytes);
    let mut out = String::with_capacity(64);
    for byte in digest {
        let _ = write!(out, "{byte:02x}");
    }
    out
}

fn release_json(tag: &str, base: &str) -> Value {
    let bytes = artifact_zip(tag);
    json!({
        "tag_name": tag,
        "target_commitish": sha_for_tag(tag),
        "published_at": "2026-01-01T00:00:00Z",
        "body": format!("release {tag}"),
        "assets": [
            {
                "name": "demo-plugin.zip",
                "browser_download_url": format!("{base}/assets/{tag}.zip"),
                "size": bytes.len(),
                "content_type": "application/zip"
            }
        ]
    })
}

#[derive(Clone)]
struct MockState {
    base: Arc<String>,
}

async fn releases_latest(State(state): State<MockState>) -> Json<Value> {
    Json(release_json(V1_TAG, &state.base))
}

async fn release_by_tag(
    State(state): State<MockState>,
    Path((_owner, _repo, tag)): Path<(String, String, String)>,
) -> Result<Json<Value>, StatusCode> {
    if matches!(tag.as_str(), V1_TAG | V2_TAG | V3_TAG) {
        Ok(Json(release_json(&tag, &state.base)))
    } else {
        Err(StatusCode::NOT_FOUND)
    }
}

async fn git_ref_tags(Path((_owner, _repo, tag)): Path<(String, String, String)>) -> Json<Value> {
    Json(json!({ "object": { "sha": sha_for_tag(&tag), "type": "commit" } }))
}

async fn tags_list() -> Json<Value> {
    Json(json!([{ "name": V1_TAG }]))
}

async fn raw_manifest(
    Path((_owner, _repo, sha, file)): Path<(String, String, String, String)>,
) -> Result<Json<Value>, StatusCode> {
    if file == "gobrowse-plugin.json" {
        Ok(Json(manifest_for_sha(&sha)))
    } else {
        Err(StatusCode::NOT_FOUND)
    }
}

async fn asset(Path(file): Path<String>) -> Result<Vec<u8>, StatusCode> {
    if let Some(tag) = file.strip_suffix(".sha256") {
        let tag = tag.strip_suffix(".zip").unwrap_or(tag);
        if matches!(tag, V1_TAG | V2_TAG | V3_TAG) {
            return Ok(sha256_hex(&artifact_zip(tag)).into_bytes());
        }
        return Err(StatusCode::NOT_FOUND);
    }
    if let Some(tag) = file.strip_suffix(".zip")
        && matches!(tag, V1_TAG | V2_TAG | V3_TAG)
    {
        return Ok(artifact_zip(tag));
    }
    Err(StatusCode::NOT_FOUND)
}

async fn search_repositories(Query(_params): Query<HashMap<String, String>>) -> Json<Value> {
    Json(json!({
        "items": [
            {
                "full_name": "demo/demo-plugin",
                "name": "demo-plugin",
                "description": "Demo plugin",
                "owner": { "login": "demo" },
                "stargazers_count": 42,
                "topics": ["gobrowse", "plugin"]
            }
        ]
    }))
}

async fn repo_inspect(Path((_owner, _repo)): Path<(String, String)>) -> Json<Value> {
    Json(json!({
        "full_name": "demo/demo-plugin",
        "name": "demo-plugin",
        "description": "Demo plugin",
        "owner": { "login": "demo" },
        "stargazers_count": 42,
        "topics": ["gobrowse", "plugin"]
    }))
}

async fn releases_list(Path((_owner, _repo)): Path<(String, String)>) -> Json<Value> {
    Json(json!([
        { "tag_name": V2_TAG, "published_at": "2026-02-01T00:00:00Z", "body": "v2" },
        { "tag_name": V1_TAG, "published_at": "2026-01-01T00:00:00Z", "body": "v1" }
    ]))
}

async fn spawn_mock_github() -> MockGitHub {
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    let base = format!("http://{addr}");
    let state = MockState {
        base: Arc::new(base.clone()),
    };
    let app = Router::new()
        .route(
            "/repos/{owner}/{repo}/releases/latest",
            get(releases_latest),
        )
        .route(
            "/repos/{owner}/{repo}/releases/tags/{tag}",
            get(release_by_tag),
        )
        .route("/repos/{owner}/{repo}/releases", get(releases_list))
        .route(
            "/repos/{owner}/{repo}/git/ref/tags/{tag}",
            get(git_ref_tags),
        )
        .route("/repos/{owner}/{repo}/tags", get(tags_list))
        .route("/repos/{owner}/{repo}", get(repo_inspect))
        .route("/search/repositories", get(search_repositories))
        .route("/raw/{owner}/{repo}/{sha}/{file}", get(raw_manifest))
        .route("/assets/{file}", get(asset))
        .with_state(state);
    let join = tokio::spawn(async move {
        if let Err(error) = axum::serve(listener, app).await {
            eprintln!("mock GitHub server error: {error}");
        }
    });
    MockGitHub {
        base: Url::parse(&base).unwrap(),
        raw_base: Url::parse(&format!("{base}/raw")).unwrap(),
        join,
    }
}

async fn test_pool() -> Option<PgPool> {
    let url = std::env::var("GOBROWSE_TEST_DATABASE_URL").ok()?;
    let pool = PgPool::connect(&url)
        .await
        .expect("connect to test PostgreSQL");
    db::migrate(&pool).await.expect("apply test migrations");
    Some(pool)
}

fn test_settings(database_url: &str, mock: &MockGitHub) -> Settings {
    Settings {
        http: HttpSettings::default(),
        database: DatabaseSettings {
            url: SecretString::from(database_url.to_owned()),
            max_connections: 10,
        },
        auth: AuthSettings::default(),
        vault: VaultSettings::default(),
        features: FeatureSettings {
            plugins_dir: std::env::temp_dir()
                .join(format!("gobrowse-plugin-test-{}", Uuid::new_v4())),
            github_api_base_url: Some(mock.base.clone()),
            github_raw_base_url: Some(mock.raw_base.clone()),
            ..FeatureSettings::default()
        },
        observability: ObservabilitySettings::default(),
    }
}

async fn create_session(pool: &PgPool, profile_id: Uuid, role: &str) -> (Uuid, String) {
    let user_id = Uuid::now_v7();
    let token = format!("plugin-session-{user_id}");
    sqlx::query("INSERT INTO users (id,email,display_name,password_hash,role,primary_profile_id) VALUES ($1,$2,'Plugin Test User','unused',$3,$4)")
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
    sqlx::query("INSERT INTO profiles (id, name) VALUES ($1, 'plugin-test-profile')")
        .bind(profile_id)
        .execute(pool)
        .await
        .expect("create test profile");
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
        builder = builder.header(header::ORIGIN, "http://localhost:8080");
    }
    let request_body = if let Some(body) = body {
        builder = builder.header(header::CONTENT_TYPE, "application/json");
        Body::from(body.to_string())
    } else {
        Body::empty()
    };
    let response = app
        .clone()
        .oneshot(builder.body(request_body).expect("build request"))
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
            .unwrap_or_else(|_| Value::String(String::from_utf8_lossy(&bytes).into_owned()))
    };
    (status, value)
}

fn preview_body(source_uri: &str, version: Option<&str>) -> Value {
    json!({
        "source_type": "github_release",
        "source_uri": source_uri,
        "version": version,
    })
}

fn install_body(source_uri: &str, version: Option<&str>, digest: &str, approve: bool) -> Value {
    json!({
        "source_type": "github_release",
        "source_uri": source_uri,
        "version": version,
        "expected_digest": digest,
        "approve": approve,
    })
}

async fn preview_and_install_v1(app: &Router, cookie: &str) -> (Uuid, Uuid) {
    let (status, preview) = request_json(
        app,
        Method::POST,
        "/api/v1/plugins/preview",
        cookie,
        Some(preview_body("demo/demo-plugin", None)),
    )
    .await;
    assert_eq!(status, StatusCode::OK, "preview: {preview}");
    let digest = preview["source"]["digest"]
        .as_str()
        .expect("digest")
        .to_owned();
    assert_eq!(digest.len(), 64);
    let (status, installed) = request_json(
        app,
        Method::POST,
        "/api/v1/plugins/install",
        cookie,
        Some(install_body("demo/demo-plugin", None, &digest, true)),
    )
    .await;
    assert_eq!(status, StatusCode::OK, "install: {installed}");
    (
        Uuid::parse_str(installed["plugin_id"].as_str().unwrap()).unwrap(),
        Uuid::parse_str(installed["book_id"].as_str().unwrap()).unwrap(),
    )
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[tokio::test]
async fn full_install_pipeline_creates_dormant_plugin_and_book() {
    let Some(database_url) = std::env::var("GOBROWSE_TEST_DATABASE_URL").ok() else {
        eprintln!("GOBROWSE_TEST_DATABASE_URL is unset; skipping plugin integration test");
        return;
    };
    let _lock = common::acquire_test_lock(&database_url).await;
    let pool = test_pool().await.expect("test pool after lock");
    let mock = spawn_mock_github().await;
    let profile_id = create_profile(&pool).await;
    let (_owner_id, cookie) = create_session(&pool, profile_id, "OWNER").await;
    let app = gobrowse_server::router(
        AppState::new(pool.clone(), test_settings(&database_url, &mock))
            .await
            .expect("create app state"),
    );

    // Preview: metadata + digest, no install.
    let (status, preview) = request_json(
        &app,
        Method::POST,
        "/api/v1/plugins/preview",
        &cookie,
        Some(preview_body("demo/demo-plugin", None)),
    )
    .await;
    assert_eq!(status, StatusCode::OK, "preview: {preview}");
    assert_eq!(preview["name"], "demo-plugin");
    assert_eq!(preview["version"], "1.0.0");
    assert_eq!(preview["publisher"], "demo-labs");
    assert_eq!(preview["trust"], "UNTRUSTED");
    assert_eq!(preview["source"]["commit_sha"].as_str().unwrap(), V1_SHA);
    assert_eq!(preview["components"].as_array().unwrap().len(), 2);
    assert_eq!(preview["permissions"].as_array().unwrap().len(), 2);
    let permissions: Vec<&str> = preview["permissions"]
        .as_array()
        .unwrap()
        .iter()
        .map(|p| p["domain"].as_str().unwrap())
        .collect();
    assert!(permissions.contains(&"filesystem_read"));
    assert!(permissions.contains(&"secrets"));

    let digest = preview["source"]["digest"].as_str().unwrap().to_owned();
    let (status, installed) = request_json(
        &app,
        Method::POST,
        "/api/v1/plugins/install",
        &cookie,
        Some(install_body("demo/demo-plugin", None, &digest, true)),
    )
    .await;
    assert_eq!(status, StatusCode::OK, "install: {installed}");
    let plugin_id = Uuid::parse_str(installed["plugin_id"].as_str().unwrap()).unwrap();
    let book_id = Uuid::parse_str(installed["book_id"].as_str().unwrap()).unwrap();
    assert_eq!(installed["state"], "dormant");
    assert_eq!(installed["trust"], "USER_PROVIDED");

    // Detail reflects components + permissions + installation.
    let (status, detail) = request_json(
        &app,
        Method::GET,
        &format!("/api/v1/plugins/{plugin_id}"),
        &cookie,
        None,
    )
    .await;
    assert_eq!(status, StatusCode::OK, "detail: {detail}");
    assert_eq!(detail["state"], "dormant");
    assert_eq!(detail["version"], "1.0.0");
    assert_eq!(detail["components"].as_array().unwrap().len(), 2);
    assert_eq!(detail["permissions"].as_array().unwrap().len(), 2);
    assert_eq!(detail["installations"].as_array().unwrap().len(), 1);
    assert_eq!(detail["installations"][0]["status"], "active");

    // PLUGIN companion Book (amendment A1).
    let row = sqlx::query(
        "SELECT kind, book_type, provenance, trust, scope, security_classification, author, metadata \
         FROM books WHERE id = $1",
    )
    .bind(book_id)
    .fetch_one(&pool)
    .await
    .expect("plugin book exists");
    assert_eq!(row.get::<String, _>("kind"), "PLUGIN");
    assert_eq!(row.get::<String, _>("book_type"), "INSTRUCTION");
    assert_eq!(row.get::<String, _>("provenance"), "SYSTEM");
    assert_eq!(row.get::<String, _>("trust"), "USER_PROVIDED");
    assert_eq!(row.get::<String, _>("scope"), "PROFILE");
    assert_eq!(row.get::<String, _>("security_classification"), "INTERNAL");
    assert_eq!(row.get::<String, _>("author"), "system");
    let metadata: Value = row.get("metadata");
    assert_eq!(
        metadata["plugin_id"].as_str().unwrap(),
        plugin_id.to_string()
    );
    let capabilities: Vec<String> = metadata["capabilities"]
        .as_array()
        .unwrap()
        .iter()
        .map(|value| value.as_str().unwrap().to_owned())
        .collect();
    assert_eq!(
        capabilities,
        vec!["run-demo".to_owned(), "demo-runbook".to_owned()]
    );

    // Plugin row.
    let plugin = sqlx::query("SELECT trust, state, verified, version FROM plugins WHERE id = $1")
        .bind(plugin_id)
        .fetch_one(&pool)
        .await
        .expect("plugin row exists");
    assert_eq!(plugin.get::<String, _>("trust"), "USER_PROVIDED");
    assert_eq!(plugin.get::<String, _>("state"), "dormant");
    assert!(!plugin.get::<bool, _>("verified"));

    // Audit trail.
    let audits: i64 = sqlx::query_scalar(
        "SELECT COUNT(*) FROM audit_events WHERE profile_id = $1 AND action = 'plugin.installed'",
    )
    .bind(profile_id)
    .fetch_one(&pool)
    .await
    .expect("audit count");
    assert_eq!(audits, 1);
}

#[tokio::test]
async fn digest_mismatch_and_missing_approval_are_rejected() {
    let Some(database_url) = std::env::var("GOBROWSE_TEST_DATABASE_URL").ok() else {
        eprintln!("GOBROWSE_TEST_DATABASE_URL is unset; skipping plugin integration test");
        return;
    };
    let _lock = common::acquire_test_lock(&database_url).await;
    let pool = test_pool().await.expect("test pool after lock");
    let mock = spawn_mock_github().await;
    let profile_id = create_profile(&pool).await;
    let (_owner_id, cookie) = create_session(&pool, profile_id, "OWNER").await;
    let app = gobrowse_server::router(
        AppState::new(pool.clone(), test_settings(&database_url, &mock))
            .await
            .expect("create app state"),
    );

    let (status, preview) = request_json(
        &app,
        Method::POST,
        "/api/v1/plugins/preview",
        &cookie,
        Some(preview_body("demo/demo-plugin", None)),
    )
    .await;
    assert_eq!(status, StatusCode::OK, "preview: {preview}");
    let digest = preview["source"]["digest"].as_str().unwrap().to_owned();

    // Wrong digest (TOCTOU protection).
    let wrong_digest = "0".repeat(64);
    let (status, body) = request_json(
        &app,
        Method::POST,
        "/api/v1/plugins/install",
        &cookie,
        Some(install_body("demo/demo-plugin", None, &wrong_digest, true)),
    )
    .await;
    assert_eq!(status, StatusCode::CONFLICT, "body: {body}");

    // Malformed digest.
    let (status, _) = request_json(
        &app,
        Method::POST,
        "/api/v1/plugins/install",
        &cookie,
        Some(install_body("demo/demo-plugin", None, "not-a-digest", true)),
    )
    .await;
    assert_eq!(status, StatusCode::UNPROCESSABLE_ENTITY);

    // Missing approval.
    let (status, body) = request_json(
        &app,
        Method::POST,
        "/api/v1/plugins/install",
        &cookie,
        Some(install_body("demo/demo-plugin", None, &digest, false)),
    )
    .await;
    assert_eq!(status, StatusCode::UNPROCESSABLE_ENTITY, "body: {body}");
    assert!(body["message"].as_str().unwrap().contains("approve"));

    // Nothing was installed.
    let count: i64 = sqlx::query_scalar("SELECT COUNT(*) FROM plugins WHERE profile_id = $1")
        .bind(profile_id)
        .fetch_one(&pool)
        .await
        .expect("count plugins");
    assert_eq!(count, 0);
}

#[tokio::test]
async fn invalid_manifest_is_rejected_at_preview() {
    let Some(database_url) = std::env::var("GOBROWSE_TEST_DATABASE_URL").ok() else {
        eprintln!("GOBROWSE_TEST_DATABASE_URL is unset; skipping plugin integration test");
        return;
    };
    let _lock = common::acquire_test_lock(&database_url).await;
    let pool = test_pool().await.expect("test pool after lock");
    let mock = spawn_mock_github().await;
    let profile_id = create_profile(&pool).await;
    let (_owner_id, cookie) = create_session(&pool, profile_id, "OWNER").await;
    let app = gobrowse_server::router(
        AppState::new(pool.clone(), test_settings(&database_url, &mock))
            .await
            .expect("create app state"),
    );

    // v3.0.0's manifest has an empty components array -> validation failure.
    let (status, body) = request_json(
        &app,
        Method::POST,
        "/api/v1/plugins/preview",
        &cookie,
        Some(preview_body("demo/demo-plugin", Some("v3.0.0"))),
    )
    .await;
    assert_eq!(status, StatusCode::UNPROCESSABLE_ENTITY, "body: {body}");
    assert!(
        body["message"]
            .as_str()
            .unwrap()
            .contains("invalid plugin manifest"),
        "message: {body}"
    );
    let count: i64 = sqlx::query_scalar("SELECT COUNT(*) FROM plugins WHERE profile_id = $1")
        .bind(profile_id)
        .fetch_one(&pool)
        .await
        .expect("count plugins");
    assert_eq!(count, 0);
}

#[tokio::test]
async fn upgrade_stage_diff_activate_and_rollback_round_trip() {
    let Some(database_url) = std::env::var("GOBROWSE_TEST_DATABASE_URL").ok() else {
        eprintln!("GOBROWSE_TEST_DATABASE_URL is unset; skipping plugin integration test");
        return;
    };
    let _lock = common::acquire_test_lock(&database_url).await;
    let pool = test_pool().await.expect("test pool after lock");
    let mock = spawn_mock_github().await;
    let profile_id = create_profile(&pool).await;
    let (_owner_id, cookie) = create_session(&pool, profile_id, "OWNER").await;
    let app = gobrowse_server::router(
        AppState::new(pool.clone(), test_settings(&database_url, &mock))
            .await
            .expect("create app state"),
    );

    let (plugin_id, _book_id) = preview_and_install_v1(&app, &cookie).await;

    // Stage v2: permission + capability diff.
    let (status, diff) = request_json(
        &app,
        Method::POST,
        &format!("/api/v1/plugins/{plugin_id}/upgrade"),
        &cookie,
        Some(json!({ "version": "2.0.0" })),
    )
    .await;
    assert_eq!(status, StatusCode::OK, "diff: {diff}");
    assert_eq!(diff["current_version"], "1.0.0");
    assert_eq!(diff["new_version"], "2.0.0");
    let added_perms: Vec<String> = diff["permissions"]["added"]
        .as_array()
        .unwrap()
        .iter()
        .map(|p| {
            format!(
                "{}:{}",
                p["domain"].as_str().unwrap(),
                p["scope_value"].as_str().unwrap()
            )
        })
        .collect();
    assert!(added_perms.contains(&"filesystem_write:/workspace/out/**".to_owned()));
    assert!(added_perms.contains(&"network:api.example.com:443".to_owned()));
    let removed_perms: Vec<String> = diff["permissions"]["removed"]
        .as_array()
        .unwrap()
        .iter()
        .map(|p| {
            format!(
                "{}:{}",
                p["domain"].as_str().unwrap(),
                p["scope_value"].as_str().unwrap()
            )
        })
        .collect();
    assert!(removed_perms.contains(&"secrets:DEMO_TOKEN".to_owned()));
    let added_components: Vec<String> = diff["components"]["added"]
        .as_array()
        .unwrap()
        .iter()
        .map(|name| name.as_str().unwrap().to_owned())
        .collect();
    assert!(added_components.contains(&"run-demo-v2".to_owned()));
    assert!(added_components.contains(&"workflow-v2".to_owned()));
    let removed_components: Vec<String> = diff["components"]["removed"]
        .as_array()
        .unwrap()
        .iter()
        .map(|name| name.as_str().unwrap().to_owned())
        .collect();
    assert!(removed_components.contains(&"run-demo".to_owned()));
    assert!(removed_components.contains(&"demo-runbook".to_owned()));

    // Activate v2.
    let (status, activated) = request_json(
        &app,
        Method::POST,
        &format!("/api/v1/plugins/{plugin_id}/upgrade/2.0.0/activate"),
        &cookie,
        None,
    )
    .await;
    assert_eq!(status, StatusCode::OK, "activate: {activated}");
    assert_eq!(activated["version"], "2.0.0");

    let (status, detail) = request_json(
        &app,
        Method::GET,
        &format!("/api/v1/plugins/{plugin_id}"),
        &cookie,
        None,
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(detail["version"], "2.0.0");
    let component_names: Vec<String> = detail["components"]
        .as_array()
        .unwrap()
        .iter()
        .map(|c| c["name"].as_str().unwrap().to_owned())
        .collect();
    assert_eq!(
        component_names,
        vec!["run-demo-v2".to_owned(), "workflow-v2".to_owned()]
    );
    let permission_domains: Vec<String> = detail["permissions"]
        .as_array()
        .unwrap()
        .iter()
        .map(|p| p["domain"].as_str().unwrap().to_owned())
        .collect();
    assert!(permission_domains.contains(&"filesystem_write".to_owned()));
    assert!(permission_domains.contains(&"network".to_owned()));
    assert!(!permission_domains.contains(&"secrets".to_owned()));
    // v1 installation kept for rollback.
    let installation_statuses: Vec<String> = detail["installations"]
        .as_array()
        .unwrap()
        .iter()
        .map(|i| i["status"].as_str().unwrap().to_owned())
        .collect();
    assert!(installation_statuses.contains(&"active".to_owned()));
    assert_eq!(installation_statuses.len(), 2);

    // Rollback to v1.
    let (status, rolled_back) = request_json(
        &app,
        Method::POST,
        &format!("/api/v1/plugins/{plugin_id}/rollback"),
        &cookie,
        None,
    )
    .await;
    assert_eq!(status, StatusCode::OK, "rollback: {rolled_back}");
    assert_eq!(rolled_back["version"], "1.0.0");

    let (status, detail) = request_json(
        &app,
        Method::GET,
        &format!("/api/v1/plugins/{plugin_id}"),
        &cookie,
        None,
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(detail["version"], "1.0.0");
    let component_names: Vec<String> = detail["components"]
        .as_array()
        .unwrap()
        .iter()
        .map(|c| c["name"].as_str().unwrap().to_owned())
        .collect();
    assert_eq!(
        component_names,
        vec!["demo-runbook".to_owned(), "run-demo".to_owned()],
        "components are ORDER BY name (server contract)"
    );
    let rollback_statuses: Vec<String> = detail["installations"]
        .as_array()
        .unwrap()
        .iter()
        .map(|i| {
            format!(
                "{}:{}",
                i["version"].as_str().unwrap(),
                i["status"].as_str().unwrap()
            )
        })
        .collect();
    assert!(rollback_statuses.contains(&"2.0.0:rolled_back".to_owned()));
    assert!(rollback_statuses.contains(&"1.0.0:active".to_owned()));

    // Book body/metadata follow the active version.
    let book: Value = sqlx::query_scalar(
        "SELECT metadata FROM books WHERE kind = 'PLUGIN' AND metadata->>'plugin_id' = $1::text",
    )
    .bind(plugin_id)
    .fetch_one(&pool)
    .await
    .expect("plugin book");
    assert_eq!(book["version"], "1.0.0");
}

#[tokio::test]
async fn uninstall_deletes_plugin_and_book() {
    let Some(database_url) = std::env::var("GOBROWSE_TEST_DATABASE_URL").ok() else {
        eprintln!("GOBROWSE_TEST_DATABASE_URL is unset; skipping plugin integration test");
        return;
    };
    let _lock = common::acquire_test_lock(&database_url).await;
    let pool = test_pool().await.expect("test pool after lock");
    let mock = spawn_mock_github().await;
    let profile_id = create_profile(&pool).await;
    let (_owner_id, cookie) = create_session(&pool, profile_id, "OWNER").await;
    let app = gobrowse_server::router(
        AppState::new(pool.clone(), test_settings(&database_url, &mock))
            .await
            .expect("create app state"),
    );

    let (plugin_id, book_id) = preview_and_install_v1(&app, &cookie).await;

    let (status, _) = request_json(
        &app,
        Method::DELETE,
        &format!("/api/v1/plugins/{plugin_id}"),
        &cookie,
        None,
    )
    .await;
    assert_eq!(status, StatusCode::NO_CONTENT);

    let plugin_count: i64 = sqlx::query_scalar("SELECT COUNT(*) FROM plugins WHERE id = $1")
        .bind(plugin_id)
        .fetch_one(&pool)
        .await
        .expect("count plugins");
    assert_eq!(plugin_count, 0);
    let book_count: i64 = sqlx::query_scalar("SELECT COUNT(*) FROM books WHERE id = $1")
        .bind(book_id)
        .fetch_one(&pool)
        .await
        .expect("count books");
    assert_eq!(book_count, 0);
    let component_count: i64 =
        sqlx::query_scalar("SELECT COUNT(*) FROM plugin_components WHERE plugin_id = $1")
            .bind(plugin_id)
            .fetch_one(&pool)
            .await
            .expect("count components");
    assert_eq!(component_count, 0);
    let installation_count: i64 =
        sqlx::query_scalar("SELECT COUNT(*) FROM plugin_installations WHERE plugin_id = $1")
            .bind(plugin_id)
            .fetch_one(&pool)
            .await
            .expect("count installations");
    assert_eq!(installation_count, 0);
}

#[tokio::test]
async fn workspace_install_requires_workspace_owner_and_scopes_book() {
    let Some(database_url) = std::env::var("GOBROWSE_TEST_DATABASE_URL").ok() else {
        eprintln!("GOBROWSE_TEST_DATABASE_URL is unset; skipping plugin integration test");
        return;
    };
    let _lock = common::acquire_test_lock(&database_url).await;
    let pool = test_pool().await.expect("test pool after lock");
    let mock = spawn_mock_github().await;
    let profile_id = create_profile(&pool).await;
    let (owner_id, owner_cookie) = create_session(&pool, profile_id, "OWNER").await;
    let (member_id, member_cookie) = create_session(&pool, profile_id, "MEMBER").await;
    let workspace_id = Uuid::now_v7();
    sqlx::query(
        "INSERT INTO workspaces (id, profile_id, title, created_by_user_id) VALUES ($1, $2, 'plugin-ws', $3)",
    )
    .bind(workspace_id)
    .bind(profile_id)
    .bind(owner_id)
    .execute(&pool)
    .await
    .expect("create workspace");
    sqlx::query(
        "INSERT INTO workspace_memberships (workspace_id, user_id, access) VALUES ($1, $2, 'EDITOR')",
    )
    .bind(workspace_id)
    .bind(member_id)
    .execute(&pool)
    .await
    .expect("member membership");
    let app = gobrowse_server::router(
        AppState::new(pool.clone(), test_settings(&database_url, &mock))
            .await
            .expect("create app state"),
    );

    // A non-owner member cannot install into the workspace.
    let (status, preview) = request_json(
        &app,
        Method::POST,
        "/api/v1/plugins/preview",
        &member_cookie,
        Some(preview_body("demo/demo-plugin", None)),
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    let digest = preview["source"]["digest"].as_str().unwrap().to_owned();
    let mut body = install_body("demo/demo-plugin", None, &digest, true);
    body["workspace_id"] = json!(workspace_id.to_string());
    let (status, denied) = request_json(
        &app,
        Method::POST,
        "/api/v1/plugins/install",
        &member_cookie,
        Some(body.clone()),
    )
    .await;
    assert_eq!(status, StatusCode::FORBIDDEN, "denied: {denied}");

    // The workspace owner succeeds and the Book is WORKSPACE-scoped.
    let (status, installed) = request_json(
        &app,
        Method::POST,
        "/api/v1/plugins/install",
        &owner_cookie,
        Some(body),
    )
    .await;
    assert_eq!(status, StatusCode::OK, "installed: {installed}");
    let plugin_id = Uuid::parse_str(installed["plugin_id"].as_str().unwrap()).unwrap();
    let book_id = Uuid::parse_str(installed["book_id"].as_str().unwrap()).unwrap();
    let scope: String = sqlx::query_scalar("SELECT scope FROM books WHERE id = $1")
        .bind(book_id)
        .fetch_one(&pool)
        .await
        .expect("book scope");
    assert_eq!(scope, "WORKSPACE");
    let plugin_workspace: Uuid =
        sqlx::query_scalar("SELECT workspace_id FROM plugins WHERE id = $1")
            .bind(plugin_id)
            .fetch_one(&pool)
            .await
            .expect("plugin workspace");
    assert_eq!(plugin_workspace, workspace_id);

    // The member can read the plugin detail (workspace membership) but not
    // delete it.
    let (status, detail) = request_json(
        &app,
        Method::GET,
        &format!("/api/v1/plugins/{plugin_id}"),
        &member_cookie,
        None,
    )
    .await;
    assert_eq!(status, StatusCode::OK, "detail: {detail}");
    let (status, _) = request_json(
        &app,
        Method::DELETE,
        &format!("/api/v1/plugins/{plugin_id}"),
        &member_cookie,
        None,
    )
    .await;
    assert_eq!(
        status,
        StatusCode::NOT_FOUND,
        "delete by non-owner member should hide existence (404): {status}"
    );
}

#[tokio::test]
async fn marketplace_search_returns_plugin_candidates_without_installing() {
    let Some(database_url) = std::env::var("GOBROWSE_TEST_DATABASE_URL").ok() else {
        eprintln!("GOBROWSE_TEST_DATABASE_URL is unset; skipping plugin integration test");
        return;
    };
    let _lock = common::acquire_test_lock(&database_url).await;
    let pool = test_pool().await.expect("test pool after lock");
    let mock = spawn_mock_github().await;
    let profile_id = create_profile(&pool).await;
    let (_owner_id, cookie) = create_session(&pool, profile_id, "OWNER").await;
    let app = gobrowse_server::router(
        AppState::new(pool.clone(), test_settings(&database_url, &mock))
            .await
            .expect("create app state"),
    );

    let (status, results) = request_json(
        &app,
        Method::POST,
        "/api/v1/plugins/search",
        &cookie,
        Some(json!({ "query": "gobrowse plugin" })),
    )
    .await;
    assert_eq!(status, StatusCode::OK, "search: {results}");
    let results = results.as_array().expect("results array");
    assert!(!results.is_empty());
    assert_eq!(results[0]["name"], "demo-plugin");
    assert_eq!(results[0]["publisher"], "demo");
    assert_eq!(results[0]["popularity"], 42);
    assert_eq!(results[0]["source"], "github_release:demo/demo-plugin");

    // Search never installs anything.
    let count: i64 = sqlx::query_scalar("SELECT COUNT(*) FROM plugins WHERE profile_id = $1")
        .bind(profile_id)
        .fetch_one(&pool)
        .await
        .expect("count plugins");
    assert_eq!(count, 0);
}
