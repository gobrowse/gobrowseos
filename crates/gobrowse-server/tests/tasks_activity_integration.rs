//! Focused PostgreSQL coverage for the durable task and activity endpoints.

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

#[tokio::test]
async fn tasks_and_activity_are_durable_and_workspace_scoped() {
    let Some(database_url) = std::env::var("GOBROWSE_TEST_DATABASE_URL").ok() else {
        eprintln!("GOBROWSE_TEST_DATABASE_URL is unset; skipping task integration test");
        return;
    };
    let _lock = common::acquire_test_lock(&database_url).await;
    let pool = PgPool::connect(&database_url)
        .await
        .expect("connect to test PostgreSQL");
    db::migrate(&pool).await.expect("apply test migrations");

    let profile_id = Uuid::now_v7();
    sqlx::query("INSERT INTO profiles (id,name) VALUES ($1,'task integration profile')")
        .bind(profile_id)
        .execute(&pool)
        .await
        .expect("create profile");
    let (owner_id, owner_cookie) = create_session(&pool, profile_id, "OWNER").await;
    let (_viewer_id, viewer_cookie) = create_session(&pool, profile_id, "VIEWER").await;
    let (foreign_profile, foreign_cookie) = create_foreign_session(&pool).await;
    let workspace_id = Uuid::now_v7();
    sqlx::query(
        "INSERT INTO workspaces (id,profile_id,title,created_by_user_id) VALUES ($1,$2,'Tasks',$3)",
    )
    .bind(workspace_id)
    .bind(profile_id)
    .bind(owner_id)
    .execute(&pool)
    .await
    .expect("create workspace");
    sqlx::query(
        "INSERT INTO workspace_memberships (workspace_id,user_id,access) VALUES ($1,$2,'OWNER'),($1,$3,'VIEWER')",
    )
    .bind(workspace_id)
    .bind(owner_id)
    .bind(_viewer_id)
    .execute(&pool)
    .await
    .expect("create workspace memberships");

    let state = AppState::new(pool.clone(), test_settings(&database_url))
        .await
        .expect("create app state");
    let app = router(state);
    let task_path = format!("/api/v1/workspaces/{workspace_id}/tasks");
    let (status, task) = request_json(
        &app,
        Method::POST,
        &task_path,
        &owner_cookie,
        Some(json!({"title":"First durable task","description":"Persist it","dependencies":[]})),
    )
    .await;
    assert_eq!(status, StatusCode::CREATED, "{task}");
    let task_id = uuid_field(&task, "id");
    assert_eq!(task["state"], "BACKLOG");

    let (status, viewer_tasks) =
        request_json(&app, Method::GET, &task_path, &viewer_cookie, None).await;
    assert_eq!(status, StatusCode::OK, "{viewer_tasks}");
    assert_eq!(viewer_tasks.as_array().expect("task list").len(), 1);
    let (status, forbidden_activity) = request_json(
        &app,
        Method::POST,
        &format!("/api/v1/workspaces/{workspace_id}/activity"),
        &viewer_cookie,
        Some(json!({"kind":"FILES_CHANGED","task_id":task_id,"payload":{}})),
    )
    .await;
    assert_eq!(status, StatusCode::FORBIDDEN, "{forbidden_activity}");

    let (status, activity) = request_json(
        &app,
        Method::POST,
        &format!("/api/v1/workspaces/{workspace_id}/activity"),
        &owner_cookie,
        Some(json!({"kind":"FILES_CHANGED","task_id":task_id,"payload":{"path":"src/lib.rs"}})),
    )
    .await;
    assert_eq!(status, StatusCode::CREATED, "{activity}");
    let activity_id = activity["id"].as_i64().expect("activity id");

    let (status, invalid_transition) = request_json(
        &app,
        Method::PATCH,
        &format!("/api/v1/tasks/{task_id}"),
        &owner_cookie,
        Some(json!({"state":"DONE"})),
    )
    .await;
    assert_eq!(status, StatusCode::CONFLICT, "{invalid_transition}");
    let (status, updated) = request_json(
        &app,
        Method::PATCH,
        &format!("/api/v1/tasks/{task_id}"),
        &owner_cookie,
        Some(json!({"state":"READY"})),
    )
    .await;
    assert_eq!(status, StatusCode::OK, "{updated}");
    assert_eq!(updated["state"], "READY");

    let (status, replay) = request_json(
        &app,
        Method::GET,
        &format!("/api/v1/workspaces/{workspace_id}/activity?after={activity_id}"),
        &owner_cookie,
        None,
    )
    .await;
    assert_eq!(status, StatusCode::OK, "{replay}");
    assert_eq!(
        replay.as_array().expect("activity list")[0]["kind"],
        "TASK_STATE_CHANGED"
    );

    let (status, foreign_get) = request_json(
        &app,
        Method::GET,
        &format!("/api/v1/tasks/{task_id}"),
        &foreign_cookie,
        None,
    )
    .await;
    assert_eq!(status, StatusCode::NOT_FOUND, "{foreign_get}");

    let event_count: i64 =
        sqlx::query_scalar("SELECT count(*) FROM activity_events WHERE workspace_id=$1")
            .bind(workspace_id)
            .fetch_one(&pool)
            .await
            .expect("count activity events");
    assert_eq!(
        event_count, 3,
        "create, explicit event, and state transition"
    );
    assert!(foreign_profile != profile_id);
}

async fn create_session(pool: &PgPool, profile_id: Uuid, role: &str) -> (Uuid, String) {
    let user_id = Uuid::now_v7();
    let token = format!("task-session-{user_id}");
    sqlx::query(
        "INSERT INTO users (id,email,display_name,password_hash,role,primary_profile_id) VALUES ($1,$2,'Task Test User','unused',$3,$4)",
    )
    .bind(user_id)
    .bind(format!("{user_id}@example.test"))
    .bind(role)
    .bind(profile_id)
    .execute(pool)
    .await
    .expect("create user");
    insert_session(pool, user_id, &token).await;
    (user_id, format!("gobrowse_session={token}"))
}

async fn create_foreign_session(pool: &PgPool) -> (Uuid, String) {
    let profile_id = Uuid::now_v7();
    sqlx::query("INSERT INTO profiles (id,name) VALUES ($1,'foreign task profile')")
        .bind(profile_id)
        .execute(pool)
        .await
        .expect("create foreign profile");
    let (_, cookie) = create_session(pool, profile_id, "OWNER").await;
    (profile_id, cookie)
}

async fn insert_session(pool: &PgPool, user_id: Uuid, token: &str) {
    let now = OffsetDateTime::now_utc();
    sqlx::query(
        "INSERT INTO sessions (token_hash,user_id,auth_epoch,expires_at,absolute_expires_at) VALUES ($1,$2,1,$3,$4)",
    )
    .bind(Sha256::digest(token.as_bytes()).to_vec())
    .bind(user_id)
    .bind(now + Duration::hours(1))
    .bind(now + Duration::hours(2))
    .execute(pool)
    .await
    .expect("create session");
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

fn uuid_field(value: &Value, field: &str) -> Uuid {
    Uuid::parse_str(value[field].as_str().expect("UUID string")).expect("valid UUID")
}
