//! Lane E integration tests: unified-library search kind filter + snippet-only
//! results, and per-kind progressive loading via `POST /library/books/{id}/load`.
//!
//! Silently skipped unless `GOBROWSE_TEST_DATABASE_URL` is set (same gate as
//! `postgres_integration.rs`). All tests acquire the shared advisory lock.

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
    db, router,
};
use http_body_util::BodyExt;
use secrecy::SecretString;
use serde_json::{Value, json};
use sha2::{Digest, Sha256};
use sqlx::PgPool;
use time::{Duration, OffsetDateTime};
use tower::ServiceExt;
use uuid::Uuid;

async fn test_pool(url: &str) -> PgPool {
    let pool = PgPool::connect(url)
        .await
        .expect("connect to test PostgreSQL");
    db::migrate(&pool).await.expect("apply test migrations");
    pool
}

async fn create_session(pool: &PgPool, profile_id: Uuid, role: &str) -> (Uuid, String) {
    let user_id = Uuid::now_v7();
    let token = format!("library-load-session-{user_id}");
    sqlx::query("INSERT INTO users (id,email,display_name,password_hash,role,primary_profile_id) VALUES ($1,$2,'Library Load Test User','unused',$3,$4)")
        .bind(user_id).bind(format!("{user_id}@example.test")).bind(role).bind(profile_id)
        .execute(pool).await.expect("create user");
    let now = OffsetDateTime::now_utc();
    sqlx::query("INSERT INTO sessions (token_hash,user_id,auth_epoch,expires_at,absolute_expires_at) VALUES ($1,$2,1,$3,$4)")
        .bind(Sha256::digest(token.as_bytes()).to_vec()).bind(user_id)
        .bind(now + Duration::hours(1)).bind(now + Duration::hours(2))
        .execute(pool).await.expect("create session");
    (user_id, format!("gobrowse_session={token}"))
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
        serde_json::from_slice(&bytes).expect("JSON response")
    };
    (status, value)
}

/// Inserts a SOURCE book (legacy NULL kind) with a distinctive body.
async fn insert_source_book(pool: &PgPool, profile_id: Uuid, title: &str, body: &str) -> Uuid {
    let id = Uuid::now_v7();
    sqlx::query(
        "INSERT INTO books (id,profile_id,title,body,book_type,scope,provenance,trust,author,security_classification,kind,metadata) \
         VALUES ($1,$2,$3,$4,'DOCUMENT','PROFILE','USER','USER_PROVIDED','system','INTERNAL',NULL,'{}'::jsonb)",
    )
    .bind(id).bind(profile_id).bind(title).bind(body)
    .execute(pool).await.expect("insert source book");
    id
}

/// Inserts a skill + its SKILL companion book (mirrors migration 0020).
async fn insert_skill_with_book(
    pool: &PgPool,
    profile_id: Uuid,
    name: &str,
    content: &str,
) -> (Uuid, Uuid) {
    let skill_id = Uuid::now_v7();
    let book_id = Uuid::now_v7();
    sqlx::query("INSERT INTO skills (id,profile_id,name,description,active_revision,promotion_policy) VALUES ($1,$2,$3,'fixture skill',1,'manual')")
        .bind(skill_id).bind(profile_id).bind(name)
        .execute(pool).await.expect("insert skill");
    sqlx::query("INSERT INTO skill_revisions (id,skill_id,revision,content,author,reason,promoted) VALUES ($1,$2,1,$3,'fixture','initial',true)")
        .bind(Uuid::now_v7()).bind(skill_id).bind(content)
        .execute(pool).await.expect("insert skill revision");
    sqlx::query(
        "INSERT INTO books (id,profile_id,title,body,book_type,scope,tags,provenance,trust,source,author,security_classification,kind,metadata) \
         VALUES ($1,$2,$3,$4,'INSTRUCTION','PROFILE','{}','SKILL','USER_PROVIDED','{}','system','INTERNAL','SKILL',jsonb_build_object('skill_id',$5::uuid))",
    )
    .bind(book_id).bind(profile_id).bind(name).bind("fixture skill description").bind(skill_id)
    .execute(pool).await.expect("insert skill companion book");
    (skill_id, book_id)
}

/// Inserts a plugin + its PLUGIN companion book + one component.
async fn insert_plugin_with_book(pool: &PgPool, profile_id: Uuid, name: &str) -> (Uuid, Uuid) {
    let plugin_id = Uuid::now_v7();
    let book_id = Uuid::now_v7();
    sqlx::query(
        "INSERT INTO plugins (id,profile_id,name,description,version,source_type,source_uri,trust,state) \
         VALUES ($1,$2,$3,'fixture plugin','1.0.0','github_release','https://example.test/repo','UNTRUSTED','dormant')",
    )
    .bind(plugin_id).bind(profile_id).bind(name)
    .execute(pool).await.expect("insert plugin");
    sqlx::query(
        "INSERT INTO plugin_components (id,plugin_id,component_type,name,manifest_ref,metadata) \
         VALUES ($1,$2,'skill','fixture-skill','components[0]',jsonb_build_object('description','Runs the fixture'))",
    )
    .bind(Uuid::now_v7()).bind(plugin_id)
    .execute(pool).await.expect("insert plugin component");
    sqlx::query(
        "INSERT INTO books (id,profile_id,title,body,book_type,scope,tags,provenance,trust,source,author,security_classification,kind,metadata) \
         VALUES ($1,$2,$3,$4,'INSTRUCTION','PROFILE','{}','SYSTEM','USER_PROVIDED','{}','system','INTERNAL','PLUGIN',jsonb_build_object('plugin_id',$5::uuid,'capabilities',jsonb_build_array('fixture-skill')))",
    )
    .bind(book_id).bind(profile_id).bind(name).bind("fixture plugin description").bind(plugin_id)
    .execute(pool).await.expect("insert plugin companion book");
    (plugin_id, book_id)
}

#[tokio::test]
async fn search_supports_kind_filter_and_never_returns_bodies() {
    let Some(url) = std::env::var("GOBROWSE_TEST_DATABASE_URL").ok() else {
        eprintln!("GOBROWSE_TEST_DATABASE_URL is unset; skipping library search test");
        return;
    };
    let _lock = common::acquire_test_lock(&url).await;
    let pool = test_pool(&url).await;
    let profile_id = Uuid::now_v7();
    sqlx::query("INSERT INTO profiles (id,name) VALUES ($1,'library-search-kind')")
        .bind(profile_id)
        .execute(&pool)
        .await
        .expect("create profile");
    let (_user_id, cookie) = create_session(&pool, profile_id, "OWNER").await;
    let state = AppState::new(pool.clone(), test_settings(&url))
        .await
        .expect("create app state");
    let app = router(state);

    let source_id = insert_source_book(
        &pool,
        profile_id,
        "Deploy Runbook",
        "STEP_BY_STEP deploy the fixture to production",
    )
    .await;
    let (_skill_id, skill_book_id) =
        insert_skill_with_book(&pool, profile_id, "deploy-skill", "SKILL_PROCEDURE content").await;
    let (_plugin_id, plugin_book_id) =
        insert_plugin_with_book(&pool, profile_id, "deploy-plugin").await;

    // ALL search returns all three kinds and never the full bodies.
    let (status, results) = request_json(
        &app,
        Method::GET,
        "/api/v1/library/search?q=deploy",
        &cookie,
        None,
    )
    .await;
    assert_eq!(status, StatusCode::OK, "{results}");
    let ids: Vec<String> = results
        .as_array()
        .expect("search results array")
        .iter()
        .map(|item| item["id"].as_str().unwrap_or_default().to_owned())
        .collect();
    assert!(ids.contains(&source_id.to_string()), "{results}");
    assert!(ids.contains(&skill_book_id.to_string()), "{results}");
    assert!(ids.contains(&plugin_book_id.to_string()), "{results}");
    for item in results.as_array().expect("array") {
        let rendered = item.to_string();
        assert!(
            !rendered.contains("STEP_BY_STEP") && !rendered.contains("SKILL_PROCEDURE"),
            "search results must be snippets only, got {rendered}"
        );
        assert!(
            item["kind"].is_string() || item["kind"].is_null(),
            "kind must be present: {rendered}"
        );
    }

    // SOURCE filter includes legacy NULL-kind rows.
    let (status, results) = request_json(
        &app,
        Method::GET,
        "/api/v1/library/search?q=deploy&kind=SOURCE",
        &cookie,
        None,
    )
    .await;
    assert_eq!(status, StatusCode::OK, "{results}");
    let items = results.as_array().expect("array");
    assert!(
        items
            .iter()
            .any(|item| item["id"] == json!(source_id.to_string())),
        "{results}"
    );
    assert!(
        items
            .iter()
            .all(|item| item["kind"].is_null() || item["kind"] == "SOURCE"),
        "SOURCE filter must only return SOURCE/NULL-kind books: {results}"
    );

    // SKILL filter returns only the SKILL companion book.
    let (status, results) = request_json(
        &app,
        Method::GET,
        "/api/v1/library/search?q=deploy&kind=SKILL",
        &cookie,
        None,
    )
    .await;
    assert_eq!(status, StatusCode::OK, "{results}");
    let items = results.as_array().expect("array");
    assert!(
        items
            .iter()
            .any(|item| item["id"] == json!(skill_book_id.to_string())),
        "{results}"
    );
    assert!(
        items.iter().all(|item| item["kind"] == "SKILL"),
        "SKILL filter must only return SKILL books: {results}"
    );

    // PLUGIN filter returns the plugin companion book with capabilities.
    let (status, results) = request_json(
        &app,
        Method::GET,
        "/api/v1/library/search?q=deploy&kind=PLUGIN",
        &cookie,
        None,
    )
    .await;
    assert_eq!(status, StatusCode::OK, "{results}");
    let items = results.as_array().expect("array");
    let plugin = items
        .iter()
        .find(|item| item["id"] == json!(plugin_book_id.to_string()))
        .expect("plugin book in PLUGIN results");
    assert_eq!(plugin["capabilities"], json!(["fixture-skill"]));

    // Invalid kind filter is rejected.
    let (status, _) = request_json(
        &app,
        Method::GET,
        "/api/v1/library/search?q=deploy&kind=NOT_A_KIND",
        &cookie,
        None,
    )
    .await;
    assert_eq!(status, StatusCode::UNPROCESSABLE_ENTITY);
}

#[tokio::test]
async fn load_book_resolves_each_kind_and_denies_foreign_profiles() {
    let Some(url) = std::env::var("GOBROWSE_TEST_DATABASE_URL").ok() else {
        eprintln!("GOBROWSE_TEST_DATABASE_URL is unset; skipping library load test");
        return;
    };
    let _lock = common::acquire_test_lock(&url).await;
    let pool = test_pool(&url).await;
    let profile_id = Uuid::now_v7();
    sqlx::query("INSERT INTO profiles (id,name) VALUES ($1,'library-load-kinds')")
        .bind(profile_id)
        .execute(&pool)
        .await
        .expect("create profile");
    let (_user_id, cookie) = create_session(&pool, profile_id, "OWNER").await;
    let state = AppState::new(pool.clone(), test_settings(&url))
        .await
        .expect("create app state");
    let app = router(state);

    let source_id = insert_source_book(
        &pool,
        profile_id,
        "Runbook",
        "full body of the runbook with PROCEDURE_STEPS",
    )
    .await;
    let (skill_id, skill_book_id) = insert_skill_with_book(
        &pool,
        profile_id,
        "runbook-skill",
        "the active skill procedure body",
    )
    .await;
    let (plugin_id, plugin_book_id) =
        insert_plugin_with_book(&pool, profile_id, "runbook-plugin").await;

    // SOURCE: full body with book metadata.
    let (status, loaded) = request_json(
        &app,
        Method::POST,
        &format!("/api/v1/library/books/{source_id}/load"),
        &cookie,
        None,
    )
    .await;
    assert_eq!(status, StatusCode::OK, "{loaded}");
    assert_eq!(loaded["kind"], Value::Null);
    assert_eq!(loaded["book_type"], "DOCUMENT");
    assert!(
        loaded["body"]
            .as_str()
            .expect("source body")
            .contains("PROCEDURE_STEPS")
    );

    // SKILL: active revision content, not the book description.
    let (status, loaded) = request_json(
        &app,
        Method::POST,
        &format!("/api/v1/library/books/{skill_book_id}/load"),
        &cookie,
        None,
    )
    .await;
    assert_eq!(status, StatusCode::OK, "{loaded}");
    assert_eq!(loaded["kind"], "SKILL");
    assert_eq!(loaded["skill_id"], json!(skill_id.to_string()));
    assert_eq!(loaded["content"], "the active skill procedure body");
    assert_eq!(loaded["revision"], 1);

    // PLUGIN: component names + descriptions (never secret values).
    let (status, loaded) = request_json(
        &app,
        Method::POST,
        &format!("/api/v1/library/books/{plugin_book_id}/load"),
        &cookie,
        None,
    )
    .await;
    assert_eq!(status, StatusCode::OK, "{loaded}");
    assert_eq!(loaded["kind"], "PLUGIN");
    assert_eq!(loaded["plugin_id"], json!(plugin_id.to_string()));
    let components = loaded["components"].as_array().expect("components");
    assert_eq!(components.len(), 1);
    assert_eq!(components[0]["name"], "fixture-skill");
    assert_eq!(components[0]["type"], "skill");
    assert_eq!(components[0]["description"], "Runs the fixture");

    // Stage-4: component load resolves just that component.
    let (status, loaded) = request_json(
        &app,
        Method::POST,
        &format!("/api/v1/library/books/{plugin_book_id}/load?component=fixture-skill"),
        &cookie,
        None,
    )
    .await;
    assert_eq!(status, StatusCode::OK, "{loaded}");
    assert_eq!(loaded["component"]["name"], "fixture-skill");
    assert!(loaded.get("components").is_none());

    // Cross-profile access is denied (404 for a book the user cannot see).
    let other_profile = Uuid::now_v7();
    sqlx::query("INSERT INTO profiles (id,name) VALUES ($1,'other-profile')")
        .bind(other_profile)
        .execute(&pool)
        .await
        .expect("create other profile");
    let (_other_user, other_cookie) = create_session(&pool, other_profile, "OWNER").await;
    let (status, _) = request_json(
        &app,
        Method::POST,
        &format!("/api/v1/library/books/{source_id}/load"),
        &other_cookie,
        None,
    )
    .await;
    assert_eq!(status, StatusCode::NOT_FOUND);
}
