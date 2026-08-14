mod common;

use axum::{
    Router,
    body::Body,
    http::{Method, Request, StatusCode, header},
};
use gobrowse_core::skills::SkillEvaluation;
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
use sqlx::{PgPool, Row};
use time::{Duration, OffsetDateTime};
use tower::ServiceExt;
use uuid::Uuid;

async fn test_pool() -> Option<PgPool> {
    let url = std::env::var("GOBROWSE_TEST_DATABASE_URL").ok()?;
    let pool = PgPool::connect(&url)
        .await
        .expect("connect to test PostgreSQL");
    gobrowse_server::db::migrate(&pool)
        .await
        .expect("apply test migrations");
    Some(pool)
}

async fn profile(pool: &PgPool, name: &str) -> Uuid {
    let id = Uuid::now_v7();
    sqlx::query("INSERT INTO profiles (id,name) VALUES ($1,$2)")
        .bind(id)
        .bind(name)
        .execute(pool)
        .await
        .expect("create test profile");
    id
}

#[tokio::test]
async fn skills_api_lifecycle_is_durable_metadata_only_and_audited() {
    let Some(url) = std::env::var("GOBROWSE_TEST_DATABASE_URL").ok() else {
        eprintln!("GOBROWSE_TEST_DATABASE_URL is unset; skipping Skills API integration test");
        return;
    };
    let _lock = common::acquire_test_lock(&url).await;
    let pool = PgPool::connect(&url)
        .await
        .expect("connect to test PostgreSQL");
    db::migrate(&pool).await.expect("apply test migrations");
    let profile_id = profile(&pool, "skills-api-lifecycle").await;
    let (owner_id, owner_cookie) = create_session(&pool, profile_id, "OWNER").await;
    let (_member_id, member_cookie) = create_session(&pool, profile_id, "MEMBER").await;
    let state = AppState::new(pool.clone(), test_settings(&url))
        .await
        .expect("create app state");
    let app = router(state);
    let (status, created) = request_json(&app, Method::POST, "/api/v1/skills", &owner_cookie, Some(json!({"name":"durable","description":"metadata","content":"procedure-v1","reason":"initial","promotion_policy":"manual","source_conversation_ids":[]}))).await;
    assert_eq!(status, StatusCode::CREATED, "{created}");
    let skill_id =
        Uuid::parse_str(created["id"].as_str().expect("skill id")).expect("valid skill id");
    assert_eq!(created["promotion_policy"], "manual");
    let (status, listed) =
        request_json(&app, Method::GET, "/api/v1/skills", &owner_cookie, None).await;
    assert_eq!(status, StatusCode::OK, "{listed}");
    assert!(
        listed[0].get("content").is_none(),
        "metadata list must omit procedure content"
    );
    let (status, history) = request_json(
        &app,
        Method::GET,
        &format!("/api/v1/skills/{skill_id}/history"),
        &owner_cookie,
        None,
    )
    .await;
    assert_eq!(status, StatusCode::OK, "{history}");
    assert_eq!(history[0]["content"], "procedure-v1");
    let (status, denied) = request_json(
        &app,
        Method::POST,
        "/api/v1/skills",
        &member_cookie,
        Some(
            json!({"name":"forbidden","content":"x","reason":"nope","source_conversation_ids":[]}),
        ),
    )
    .await;
    assert_eq!(status, StatusCode::FORBIDDEN, "{denied}");
    let audit_count: i64 = sqlx::query_scalar("SELECT count(*) FROM audit_events WHERE actor_user_id=$1 AND action='skill.created' AND resource_id=$2 AND outcome='success'").bind(owner_id).bind(skill_id.to_string()).fetch_one(&pool).await.expect("read skill audit");
    assert_eq!(audit_count, 1);
}

#[tokio::test]
async fn skills_reject_cross_profile_workspace_links_and_duplicate_globals() {
    let Some(url) = std::env::var("GOBROWSE_TEST_DATABASE_URL").ok() else {
        eprintln!("GOBROWSE_TEST_DATABASE_URL is unset; skipping Skills integration test");
        return;
    };
    let _lock = common::acquire_test_lock(&url).await;
    let pool = test_pool().await.expect("test pool");
    let first = profile(&pool, "skills-profile-a").await;
    let second = profile(&pool, "skills-profile-b").await;
    let workspace = Uuid::now_v7();
    sqlx::query("INSERT INTO workspaces (id,profile_id,title) VALUES ($1,$2,'other workspace')")
        .bind(workspace)
        .bind(second)
        .execute(&pool)
        .await
        .expect("create workspace");

    let cross_profile = sqlx::query(
        "INSERT INTO skills (id,profile_id,workspace_id,name,description) VALUES ($1,$2,$3,'cross','')",
    )
    .bind(Uuid::now_v7())
    .bind(first)
    .bind(workspace)
    .execute(&pool)
    .await;
    assert!(
        cross_profile.is_err(),
        "workspace must belong to skill profile"
    );

    let skill = Uuid::now_v7();
    sqlx::query("INSERT INTO skills (id,profile_id,name,description) VALUES ($1,$2,'global','')")
        .bind(skill)
        .bind(first)
        .execute(&pool)
        .await
        .expect("create global skill");
    let duplicate = sqlx::query(
        "INSERT INTO skills (id,profile_id,name,description) VALUES ($1,$2,'global','duplicate')",
    )
    .bind(Uuid::now_v7())
    .bind(first)
    .execute(&pool)
    .await;
    assert!(
        duplicate.is_err(),
        "global names must be unique with NULL workspace"
    );
}

#[tokio::test]
async fn skill_revision_content_is_immutable() {
    let Some(url) = std::env::var("GOBROWSE_TEST_DATABASE_URL").ok() else {
        eprintln!("GOBROWSE_TEST_DATABASE_URL is unset; skipping Skills integration test");
        return;
    };
    let _lock = common::acquire_test_lock(&url).await;
    let pool = test_pool().await.expect("test pool");
    let profile_id = profile(&pool, "skills-immutable").await;
    let skill_id = Uuid::now_v7();
    let revision_id = Uuid::now_v7();
    sqlx::query(
        "INSERT INTO skills (id,profile_id,name,description) VALUES ($1,$2,'immutable','')",
    )
    .bind(skill_id)
    .bind(profile_id)
    .execute(&pool)
    .await
    .expect("create skill");
    sqlx::query(
        "INSERT INTO skill_revisions (id,skill_id,revision,content,author,reason) VALUES ($1,$2,1,'original','tester','initial')",
    )
    .bind(revision_id)
    .bind(skill_id)
    .execute(&pool)
    .await
    .expect("create revision");
    let mutation = sqlx::query("UPDATE skill_revisions SET content='tampered' WHERE id=$1")
        .bind(revision_id)
        .execute(&pool)
        .await;
    assert!(mutation.is_err(), "revision procedure must be immutable");
    let content: String = sqlx::query_scalar("SELECT content FROM skill_revisions WHERE id=$1")
        .bind(revision_id)
        .fetch_one(&pool)
        .await
        .expect("read revision");
    assert_eq!(content, "original");
}

#[tokio::test]
async fn promotion_and_rollback_keep_one_active_promoted_revision() {
    let Some(url) = std::env::var("GOBROWSE_TEST_DATABASE_URL").ok() else {
        eprintln!("GOBROWSE_TEST_DATABASE_URL is unset; skipping Skills integration test");
        return;
    };
    let _lock = common::acquire_test_lock(&url).await;
    let pool = test_pool().await.expect("test pool");
    let profile_id = profile(&pool, "skills-promotion").await;
    let skill_id = Uuid::now_v7();
    sqlx::query("INSERT INTO skills (id,profile_id,name,description) VALUES ($1,$2,'promote','')")
        .bind(skill_id)
        .bind(profile_id)
        .execute(&pool)
        .await
        .expect("create skill");
    for revision in 1..=2 {
        sqlx::query(
            "INSERT INTO skill_revisions (id,skill_id,revision,content,author,reason) VALUES ($1,$2,$3,$4,'tester','test')",
        )
        .bind(Uuid::now_v7())
        .bind(skill_id)
        .bind(revision)
        .bind(format!("revision {revision}"))
        .execute(&pool)
        .await
        .expect("create revision");
    }
    let mut tx = pool.begin().await.expect("begin promotion transaction");
    sqlx::query("UPDATE skill_revisions SET promoted=true WHERE skill_id=$1 AND revision=1")
        .bind(skill_id)
        .execute(&mut *tx)
        .await
        .expect("promote first revision");
    sqlx::query("UPDATE skill_revisions SET promoted=false WHERE skill_id=$1 AND promoted")
        .bind(skill_id)
        .execute(&mut *tx)
        .await
        .expect("demote old revision");
    sqlx::query("UPDATE skill_revisions SET promoted=true WHERE skill_id=$1 AND revision=2")
        .bind(skill_id)
        .execute(&mut *tx)
        .await
        .expect("promote second revision");
    sqlx::query("UPDATE skills SET active_revision=2 WHERE id=$1")
        .bind(skill_id)
        .execute(&mut *tx)
        .await
        .expect("activate second revision");
    sqlx::query("UPDATE skill_revisions SET promoted=false WHERE skill_id=$1 AND promoted")
        .bind(skill_id)
        .execute(&mut *tx)
        .await
        .expect("rollback demotion");
    sqlx::query("UPDATE skill_revisions SET promoted=true WHERE skill_id=$1 AND revision=1")
        .bind(skill_id)
        .execute(&mut *tx)
        .await
        .expect("rollback promotion");
    sqlx::query("UPDATE skills SET active_revision=1 WHERE id=$1")
        .bind(skill_id)
        .execute(&mut *tx)
        .await
        .expect("rollback active pointer");
    tx.commit().await.expect("commit promotion transaction");
    let row = sqlx::query(
        "SELECT active_revision,(SELECT count(*) FROM skill_revisions WHERE skill_id=$1 AND promoted) AS promoted_count FROM skills WHERE id=$1",
    )
    .bind(skill_id)
    .fetch_one(&pool)
    .await
    .expect("read promotion state");
    assert_eq!(row.get::<i64, _>("active_revision"), 1);
    assert_eq!(row.get::<i64, _>("promoted_count"), 1);
}

#[tokio::test]
async fn automatic_promotion_requires_deterministic_attempt_evidence() {
    let current = SkillEvaluation {
        deterministic_checks_passed: true,
        attempts: 0,
        successful_attempts: 0,
        steps: 0,
        retries: 0,
        errors: 0,
        duration_ms: 0,
        user_corrections: 0,
    };
    let previous = current.clone();
    assert!(!current.can_auto_promote_over(&previous));
}

async fn create_session(pool: &PgPool, profile_id: Uuid, role: &str) -> (Uuid, String) {
    let user_id = Uuid::now_v7();
    let token = format!("skills-session-{user_id}");
    sqlx::query("INSERT INTO users (id,email,display_name,password_hash,role,primary_profile_id) VALUES ($1,$2,'Skills Test User','unused',$3,$4)")
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
