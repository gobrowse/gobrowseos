mod common;

use std::sync::Arc;

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
use tokio::sync::Barrier;
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
    let (status, revision) = request_json(
        &app,
        Method::POST,
        &format!("/api/v1/skills/{skill_id}/revisions"),
        &owner_cookie,
        Some(json!({"content":"procedure-v2","reason":"improve","source_conversation_ids":[]})),
    )
    .await;
    assert_eq!(status, StatusCode::CREATED, "{revision}");
    let (status, evaluated) = request_json(
        &app,
        Method::POST,
        &format!("/api/v1/skills/{skill_id}/revisions/2/evaluate"),
        &owner_cookie,
        Some(json!({"evaluation":valid_evaluation(1)})),
    )
    .await;
    assert_eq!(status, StatusCode::OK, "{evaluated}");
    let (status, promoted) = request_json(
        &app,
        Method::POST,
        &format!("/api/v1/skills/{skill_id}/revisions/2/promote"),
        &owner_cookie,
        Some(json!({"reason":"release"})),
    )
    .await;
    assert_eq!(status, StatusCode::OK, "{promoted}");
    let (status, rolled_back) = request_json(
        &app,
        Method::POST,
        &format!("/api/v1/skills/{skill_id}/rollback"),
        &owner_cookie,
        Some(json!({"target_revision":1,"reason":"rollback"})),
    )
    .await;
    assert_eq!(status, StatusCode::OK, "{rolled_back}");
    assert_eq!(rolled_back["active_revision"], 1);
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

fn valid_evaluation(attempts: u32) -> Value {
    json!({"deterministic_checks_passed":true,"attempts":attempts,"successful_attempts":attempts,"steps":1,"retries":0,"errors":0,"duration_ms":1,"user_corrections":0})
}

async fn insert_skill_fixture(
    pool: &PgPool,
    profile_id: Uuid,
    workspace_id: Option<Uuid>,
    name: &str,
    policy: &str,
) -> (Uuid, Uuid) {
    let skill_id = Uuid::now_v7();
    let revision_id = Uuid::now_v7();
    sqlx::query("INSERT INTO skills (id,profile_id,workspace_id,name,description,promotion_policy) VALUES ($1,$2,$3,$4,'fixture',$5)")
        .bind(skill_id).bind(profile_id).bind(workspace_id).bind(name).bind(policy).execute(pool).await.expect("insert skill fixture");
    sqlx::query("INSERT INTO skill_revisions (id,skill_id,revision,content,author,reason) VALUES ($1,$2,1,'fixture content','fixture','initial')")
        .bind(revision_id).bind(skill_id).execute(pool).await.expect("insert revision fixture");
    (skill_id, revision_id)
}

async fn insert_workspace(pool: &PgPool, profile_id: Uuid, owner_id: Uuid) -> Uuid {
    let workspace_id = Uuid::now_v7();
    sqlx::query("INSERT INTO workspaces (id,profile_id,title,created_by_user_id) VALUES ($1,$2,'Skills workspace',$3)")
        .bind(workspace_id).bind(profile_id).bind(owner_id).execute(pool).await.expect("insert workspace");
    sqlx::query(
        "INSERT INTO workspace_memberships (workspace_id,user_id,access) VALUES ($1,$2,'OWNER')",
    )
    .bind(workspace_id)
    .bind(owner_id)
    .execute(pool)
    .await
    .expect("insert owner membership");
    workspace_id
}

async fn wait_for_lock_waiters(pool: &PgPool, query_prefix: &str, minimum: i64) {
    let pattern = format!("{query_prefix}%");
    tokio::time::timeout(std::time::Duration::from_secs(5), async {
        loop {
            let count: i64 = sqlx::query_scalar("SELECT count(*) FROM pg_stat_activity WHERE wait_event_type='Lock' AND query LIKE $1 AND pid <> pg_backend_pid()")
                .bind(&pattern).fetch_one(pool).await.expect("lock waiter query");
            if count >= minimum { break; }
            tokio::time::sleep(std::time::Duration::from_millis(10)).await;
        }
    }).await.expect("lock waiter timeout");
}

async fn skill_side_effect_counts(
    pool: &PgPool,
    skill_id: Uuid,
) -> (Vec<Uuid>, Vec<Uuid>, Option<i64>, Vec<i64>) {
    let revisions: Vec<Uuid> =
        sqlx::query_scalar("SELECT id FROM skill_revisions WHERE skill_id=$1 ORDER BY revision")
            .bind(skill_id)
            .fetch_all(pool)
            .await
            .expect("revision IDs");
    let evaluated: Vec<Uuid> = sqlx::query_scalar("SELECT id FROM skill_revisions WHERE skill_id=$1 AND evaluation IS NOT NULL ORDER BY revision").bind(skill_id).fetch_all(pool).await.expect("evaluated IDs");
    let active: Option<i64> = sqlx::query_scalar("SELECT active_revision FROM skills WHERE id=$1")
        .bind(skill_id)
        .fetch_one(pool)
        .await
        .expect("active revision");
    let promoted: Vec<i64> = sqlx::query_scalar(
        "SELECT revision FROM skill_revisions WHERE skill_id=$1 AND promoted ORDER BY revision",
    )
    .bind(skill_id)
    .fetch_all(pool)
    .await
    .expect("promoted revisions");
    (revisions, evaluated, active, promoted)
}

async fn audit_count(
    pool: &PgPool,
    actor: Uuid,
    profile: Uuid,
    action: &str,
    resource_type: &str,
    resource_id: Option<&str>,
) -> i64 {
    sqlx::query_scalar("SELECT count(*) FROM audit_events WHERE actor_user_id=$1 AND profile_id=$2 AND action=$3 AND resource_type=$4 AND ($5::text IS NULL OR resource_id=$5) AND outcome='success'")
        .bind(actor).bind(profile).bind(action).bind(resource_type).bind(resource_id).fetch_one(pool).await.expect("audit count")
}

#[tokio::test]
async fn profile_global_skill_mutations_require_owner_or_admin() {
    let Some(url) = std::env::var("GOBROWSE_TEST_DATABASE_URL").ok() else {
        eprintln!("GOBROWSE_TEST_DATABASE_URL unset; skipping");
        return;
    };
    let _lock = common::acquire_test_lock(&url).await;
    let pool = test_pool().await.expect("pool");
    let profile_id = profile(&pool, "global-role-test").await;
    let (_owner, _owner_cookie) = create_session(&pool, profile_id, "OWNER").await;
    let (member, member_cookie) = create_session(&pool, profile_id, "MEMBER").await;
    let (viewer, viewer_cookie) = create_session(&pool, profile_id, "VIEWER").await;
    let (skill_id, revision_id) =
        insert_skill_fixture(&pool, profile_id, None, "global-role", "manual").await;
    let app = router(
        AppState::new(pool.clone(), test_settings(&url))
            .await
            .expect("state"),
    );
    let before = skill_side_effect_counts(&pool, skill_id).await;
    for (actor, cookie) in [(member, &member_cookie), (viewer, &viewer_cookie)] {
        let create_audit =
            audit_count(&pool, actor, profile_id, "skill.created", "skill", None).await;
        let revision_audit = audit_count(
            &pool,
            actor,
            profile_id,
            "skill.revision_created",
            "skill_revision",
            None,
        )
        .await;
        let revision_id_text = revision_id.to_string();
        let evaluate_audit = audit_count(
            &pool,
            actor,
            profile_id,
            "skill.evaluated",
            "skill_revision",
            Some(&revision_id_text),
        )
        .await;
        let promote_id = format!("{skill_id}:1");
        let promote_audit = audit_count(
            &pool,
            actor,
            profile_id,
            "skill.promoted",
            "skill_revision",
            Some(&promote_id),
        )
        .await;
        let skill_id_text = skill_id.to_string();
        let rollback_audit = audit_count(
            &pool,
            actor,
            profile_id,
            "skill.rolled_back",
            "skill",
            Some(&skill_id_text),
        )
        .await;
        for (method, path, body) in [
            (
                Method::POST,
                "/api/v1/skills",
                json!({"name":"blocked","content":"x","reason":"x","source_conversation_ids":[]}),
            ),
            (
                Method::POST,
                &format!("/api/v1/skills/{skill_id}/revisions"),
                json!({"content":"x","reason":"x","source_conversation_ids":[]}),
            ),
            (
                Method::POST,
                &format!("/api/v1/skills/{skill_id}/revisions/1/evaluate"),
                json!({"evaluation":valid_evaluation(1)}),
            ),
            (
                Method::POST,
                &format!("/api/v1/skills/{skill_id}/revisions/1/promote"),
                json!({"reason":"x"}),
            ),
            (
                Method::POST,
                &format!("/api/v1/skills/{skill_id}/rollback"),
                json!({"target_revision":1,"reason":"x"}),
            ),
        ] {
            let (status, _) = request_json(&app, method, path, cookie, Some(body)).await;
            assert_eq!(status, StatusCode::FORBIDDEN);
        }
        assert_eq!(skill_side_effect_counts(&pool, skill_id).await, before);
        assert_eq!(
            audit_count(&pool, actor, profile_id, "skill.created", "skill", None).await,
            create_audit
        );
        assert_eq!(
            audit_count(
                &pool,
                actor,
                profile_id,
                "skill.revision_created",
                "skill_revision",
                None
            )
            .await,
            revision_audit
        );
        assert_eq!(
            audit_count(
                &pool,
                actor,
                profile_id,
                "skill.evaluated",
                "skill_revision",
                Some(&revision_id_text)
            )
            .await,
            evaluate_audit
        );
        assert_eq!(
            audit_count(
                &pool,
                actor,
                profile_id,
                "skill.promoted",
                "skill_revision",
                Some(&promote_id)
            )
            .await,
            promote_audit
        );
        assert_eq!(
            audit_count(
                &pool,
                actor,
                profile_id,
                "skill.rolled_back",
                "skill",
                Some(&skill_id_text)
            )
            .await,
            rollback_audit
        );
    }
}

#[tokio::test]
async fn workspace_skill_roles_and_tenants_are_enforced() {
    let Some(url) = std::env::var("GOBROWSE_TEST_DATABASE_URL").ok() else {
        eprintln!("GOBROWSE_TEST_DATABASE_URL unset; skipping");
        return;
    };
    let _lock = common::acquire_test_lock(&url).await;
    let pool = test_pool().await.expect("pool");
    let profile_id = profile(&pool, "workspace-role-test").await;
    let (owner, owner_cookie) = create_session(&pool, profile_id, "OWNER").await;
    let (editor, editor_cookie) = create_session(&pool, profile_id, "MEMBER").await;
    let (viewer, viewer_cookie) = create_session(&pool, profile_id, "VIEWER").await;
    let workspace = insert_workspace(&pool, profile_id, owner).await;
    sqlx::query("INSERT INTO workspace_memberships (workspace_id,user_id,access) VALUES ($1,$2,'EDITOR'),($1,$3,'VIEWER')").bind(workspace).bind(editor).bind(viewer).execute(&pool).await.expect("memberships");
    let (skill_id, _) = insert_skill_fixture(
        &pool,
        profile_id,
        Some(workspace),
        "workspace-role",
        "manual",
    )
    .await;
    let app = router(
        AppState::new(pool.clone(), test_settings(&url))
            .await
            .expect("state"),
    );
    let (status, _) = request_json(
        &app,
        Method::POST,
        &format!("/api/v1/skills/{skill_id}/revisions"),
        &editor_cookie,
        Some(json!({"content":"editor","reason":"edit","source_conversation_ids":[]})),
    )
    .await;
    assert_eq!(status, StatusCode::CREATED);
    let (status, _) = request_json(
        &app,
        Method::POST,
        &format!("/api/v1/skills/{skill_id}/revisions"),
        &viewer_cookie,
        Some(json!({"content":"viewer","reason":"edit","source_conversation_ids":[]})),
    )
    .await;
    assert_eq!(status, StatusCode::FORBIDDEN);
    let (status, _) = request_json(
        &app,
        Method::POST,
        &format!("/api/v1/skills/{skill_id}/revisions/1/promote"),
        &editor_cookie,
        Some(json!({"reason":"no"})),
    )
    .await;
    assert_eq!(status, StatusCode::FORBIDDEN);
    let foreign_profile = profile(&pool, "foreign-workspace-profile").await;
    let (_, foreign_cookie) = create_session(&pool, foreign_profile, "OWNER").await;
    let (status, _) = request_json(
        &app,
        Method::GET,
        &format!("/api/v1/skills/{skill_id}/history"),
        &foreign_cookie,
        None,
    )
    .await;
    assert_eq!(status, StatusCode::NOT_FOUND);
    let _ = owner_cookie;
}

#[tokio::test]
async fn workspace_skill_promotion_and_rollback_require_profile_admin() {
    let Some(url) = std::env::var("GOBROWSE_TEST_DATABASE_URL").ok() else {
        eprintln!("GOBROWSE_TEST_DATABASE_URL unset; skipping");
        return;
    };
    let _lock = common::acquire_test_lock(&url).await;
    let pool = test_pool().await.expect("pool");
    let profile_id = profile(&pool, "privileged-workspace").await;
    let (owner, owner_cookie) = create_session(&pool, profile_id, "OWNER").await;
    let (_admin, admin_cookie) = create_session(&pool, profile_id, "ADMIN").await;
    let (workspace_owner, workspace_owner_cookie) =
        create_session(&pool, profile_id, "MEMBER").await;
    let (editor, editor_cookie) = create_session(&pool, profile_id, "MEMBER").await;
    let workspace = insert_workspace(&pool, profile_id, owner).await;
    sqlx::query("INSERT INTO workspace_memberships (workspace_id,user_id,access) VALUES ($1,$2,'OWNER'),($1,$3,'EDITOR')").bind(workspace).bind(workspace_owner).bind(editor).execute(&pool).await.expect("membership");
    let (skill_id, _) =
        insert_skill_fixture(&pool, profile_id, Some(workspace), "privileged", "manual").await;
    sqlx::query("INSERT INTO skill_revisions (id,skill_id,revision,content,author,reason) VALUES ($1,$2,2,'v2','fixture','v2')").bind(Uuid::now_v7()).bind(skill_id).execute(&pool).await.expect("revision 2");
    let mut tx = pool.begin().await.expect("promotion fixture");
    sqlx::query("UPDATE skill_revisions SET promoted=true WHERE skill_id=$1 AND revision=1")
        .bind(skill_id)
        .execute(&mut *tx)
        .await
        .expect("promote 1");
    sqlx::query("UPDATE skills SET active_revision=1 WHERE id=$1")
        .bind(skill_id)
        .execute(&mut *tx)
        .await
        .expect("active 1");
    tx.commit().await.expect("promotion fixture commit");
    let app = router(
        AppState::new(pool.clone(), test_settings(&url))
            .await
            .expect("state"),
    );
    let before = skill_side_effect_counts(&pool, skill_id).await;
    for (actor, cookie) in [
        (workspace_owner, &workspace_owner_cookie),
        (editor, &editor_cookie),
    ] {
        for (path, body) in [
            (
                format!("/api/v1/skills/{skill_id}/revisions/2/promote"),
                json!({"reason":"x"}),
            ),
            (
                format!("/api/v1/skills/{skill_id}/rollback"),
                json!({"target_revision":1,"reason":"x"}),
            ),
        ] {
            let (status, _) = request_json(&app, Method::POST, &path, cookie, Some(body)).await;
            assert_eq!(status, StatusCode::FORBIDDEN);
        }
        assert_eq!(skill_side_effect_counts(&pool, skill_id).await, before);
        assert_eq!(
            audit_count(
                &pool,
                actor,
                profile_id,
                "skill.promoted",
                "skill_revision",
                Some(&format!("{skill_id}:2"))
            )
            .await,
            0
        );
        assert_eq!(
            audit_count(
                &pool,
                actor,
                profile_id,
                "skill.rolled_back",
                "skill",
                Some(&skill_id.to_string())
            )
            .await,
            0
        );
    }
    for (cookie, content, revision) in [
        (&workspace_owner_cookie, "owner-member", 3_i64),
        (&editor_cookie, "editor-member", 4_i64),
    ] {
        let (status, created) = request_json(
            &app,
            Method::POST,
            &format!("/api/v1/skills/{skill_id}/revisions"),
            cookie,
            Some(json!({"content":content,"reason":"evaluate","source_conversation_ids":[]})),
        )
        .await;
        assert_eq!(status, StatusCode::CREATED);
        assert_eq!(created["revision"], revision);
        let submitted_evaluation = valid_evaluation(1);
        let (status, evaluated) = request_json(
            &app,
            Method::POST,
            &format!("/api/v1/skills/{skill_id}/revisions/{revision}/evaluate"),
            cookie,
            Some(json!({"evaluation":submitted_evaluation})),
        )
        .await;
        assert_eq!(status, StatusCode::OK, "{evaluated}");
        let evaluated_id: Uuid = evaluated["id"]
            .as_str()
            .expect("evaluated ID")
            .parse()
            .expect("evaluation UUID");
        let persisted_evaluation: serde_json::Value =
            sqlx::query_scalar("SELECT evaluation FROM skill_revisions WHERE id=$1")
                .bind(evaluated_id)
                .fetch_one(&pool)
                .await
                .expect("persisted evaluation");
        assert_eq!(persisted_evaluation, submitted_evaluation);
        let actor = if revision == 3 {
            workspace_owner
        } else {
            editor
        };
        assert_eq!(
            audit_count(
                &pool,
                actor,
                profile_id,
                "skill.evaluated",
                "skill_revision",
                Some(&evaluated_id.to_string())
            )
            .await,
            1
        );
    }
    let (status, _) = request_json(
        &app,
        Method::POST,
        &format!("/api/v1/skills/{skill_id}/revisions/2/promote"),
        &owner_cookie,
        Some(json!({"reason":"owner"})),
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(
        audit_count(
            &pool,
            owner,
            profile_id,
            "skill.promoted",
            "skill_revision",
            Some(&format!("{skill_id}:2")),
        )
        .await,
        1
    );
    let (status, _) = request_json(
        &app,
        Method::POST,
        &format!("/api/v1/skills/{skill_id}/rollback"),
        &admin_cookie,
        Some(json!({"target_revision":1,"reason":"admin"})),
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(
        audit_count(
            &pool,
            _admin,
            profile_id,
            "skill.rolled_back",
            "skill",
            Some(&skill_id.to_string()),
        )
        .await,
        1
    );
    let state: (i64, i64) = sqlx::query_as(
        "SELECT s.active_revision, (SELECT r.revision FROM skill_revisions r WHERE r.skill_id=s.id AND r.promoted) FROM skills s WHERE s.id=$1",
    )
    .bind(skill_id)
    .fetch_one(&pool)
    .await
    .expect("rollback state");
    assert_eq!(state, (1, 1));
}

#[tokio::test]
async fn workspace_member_with_viewer_access_receives_forbidden_on_mutation() {
    let Some(url) = std::env::var("GOBROWSE_TEST_DATABASE_URL").ok() else {
        eprintln!("GOBROWSE_TEST_DATABASE_URL unset; skipping");
        return;
    };
    let _lock = common::acquire_test_lock(&url).await;
    let pool = test_pool().await.expect("pool");
    let profile_id = profile(&pool, "visible-viewer-member").await;
    let (owner, _owner_cookie) = create_session(&pool, profile_id, "OWNER").await;
    let (member, member_cookie) = create_session(&pool, profile_id, "MEMBER").await;
    let workspace = insert_workspace(&pool, profile_id, owner).await;
    sqlx::query(
        "INSERT INTO workspace_memberships (workspace_id,user_id,access) VALUES ($1,$2,'VIEWER')",
    )
    .bind(workspace)
    .bind(member)
    .execute(&pool)
    .await
    .expect("viewer membership");
    let (skill_id, _) = insert_skill_fixture(
        &pool,
        profile_id,
        Some(workspace),
        "visible-viewer",
        "manual",
    )
    .await;
    let app = router(
        AppState::new(pool.clone(), test_settings(&url))
            .await
            .expect("state"),
    );
    let (status, _) = request_json(
        &app,
        Method::GET,
        &format!("/api/v1/skills/{skill_id}/history"),
        &member_cookie,
        None,
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    let before = skill_side_effect_counts(&pool, skill_id).await;
    let revision_audit = audit_count(
        &pool,
        member,
        profile_id,
        "skill.revision_created",
        "skill_revision",
        None,
    )
    .await;
    let revision_id_text = sqlx::query_scalar::<_, Uuid>(
        "SELECT id FROM skill_revisions WHERE skill_id=$1 AND revision=1",
    )
    .bind(skill_id)
    .fetch_one(&pool)
    .await
    .expect("revision")
    .to_string();
    let evaluate_audit = audit_count(
        &pool,
        member,
        profile_id,
        "skill.evaluated",
        "skill_revision",
        Some(&revision_id_text),
    )
    .await;
    let promote_id = format!("{skill_id}:1");
    let promote_audit = audit_count(
        &pool,
        member,
        profile_id,
        "skill.promoted",
        "skill_revision",
        Some(&promote_id),
    )
    .await;
    let skill_id_text = skill_id.to_string();
    let rollback_audit = audit_count(
        &pool,
        member,
        profile_id,
        "skill.rolled_back",
        "skill",
        Some(&skill_id_text),
    )
    .await;
    for (path, body) in [
        (
            format!("/api/v1/skills/{skill_id}/revisions"),
            json!({"content":"x","reason":"x","source_conversation_ids":[]}),
        ),
        (
            format!("/api/v1/skills/{skill_id}/revisions/1/evaluate"),
            json!({"evaluation":valid_evaluation(1)}),
        ),
        (
            format!("/api/v1/skills/{skill_id}/revisions/1/promote"),
            json!({"reason":"x"}),
        ),
        (
            format!("/api/v1/skills/{skill_id}/rollback"),
            json!({"target_revision":1,"reason":"x"}),
        ),
    ] {
        let (status, _) = request_json(&app, Method::POST, &path, &member_cookie, Some(body)).await;
        assert_eq!(status, StatusCode::FORBIDDEN);
    }
    assert_eq!(skill_side_effect_counts(&pool, skill_id).await, before);
    assert_eq!(
        audit_count(
            &pool,
            member,
            profile_id,
            "skill.revision_created",
            "skill_revision",
            None
        )
        .await,
        revision_audit
    );
    assert_eq!(
        audit_count(
            &pool,
            member,
            profile_id,
            "skill.evaluated",
            "skill_revision",
            Some(&revision_id_text)
        )
        .await,
        evaluate_audit
    );
    assert_eq!(
        audit_count(
            &pool,
            member,
            profile_id,
            "skill.promoted",
            "skill_revision",
            Some(&promote_id)
        )
        .await,
        promote_audit
    );
    assert_eq!(
        audit_count(
            &pool,
            member,
            profile_id,
            "skill.rolled_back",
            "skill",
            Some(&skill_id_text)
        )
        .await,
        rollback_audit
    );
}

#[tokio::test]
async fn workspace_skill_nonmembers_receive_not_found_without_side_effects() {
    let Some(url) = std::env::var("GOBROWSE_TEST_DATABASE_URL").ok() else {
        eprintln!("GOBROWSE_TEST_DATABASE_URL unset; skipping");
        return;
    };
    let _lock = common::acquire_test_lock(&url).await;
    let pool = test_pool().await.expect("pool");
    let profile_id = profile(&pool, "workspace-nonmember-test").await;
    let (owner, _owner_cookie) = create_session(&pool, profile_id, "OWNER").await;
    let (member, member_cookie) = create_session(&pool, profile_id, "MEMBER").await;
    let (viewer, viewer_cookie) = create_session(&pool, profile_id, "VIEWER").await;
    let workspace = insert_workspace(&pool, profile_id, owner).await;
    let (skill_id, _) = insert_skill_fixture(
        &pool,
        profile_id,
        Some(workspace),
        "private-workspace",
        "manual",
    )
    .await;
    let foreign_profile = profile(&pool, "nonmember-foreign").await;
    let (_foreign_owner, foreign_cookie) = create_session(&pool, foreign_profile, "OWNER").await;
    let (foreign_skill, _) =
        insert_skill_fixture(&pool, foreign_profile, None, "foreign-skill", "manual").await;
    let missing_skill = Uuid::now_v7();
    let app = router(
        AppState::new(pool.clone(), test_settings(&url))
            .await
            .expect("state"),
    );
    let before = skill_side_effect_counts(&pool, skill_id).await;
    let before_member_audits = audit_count(
        &pool,
        member,
        profile_id,
        "skill.revision_created",
        "skill_revision",
        None,
    )
    .await;
    let before_viewer_audits = audit_count(
        &pool,
        viewer,
        profile_id,
        "skill.revision_created",
        "skill_revision",
        None,
    )
    .await;
    let revision_id_text = sqlx::query_scalar::<_, Uuid>(
        "SELECT id FROM skill_revisions WHERE skill_id=$1 AND revision=1",
    )
    .bind(skill_id)
    .fetch_one(&pool)
    .await
    .expect("revision")
    .to_string();
    let promote_id = format!("{skill_id}:1");
    let skill_id_text = skill_id.to_string();
    let before_member_evaluate = audit_count(
        &pool,
        member,
        profile_id,
        "skill.evaluated",
        "skill_revision",
        Some(&revision_id_text),
    )
    .await;
    let before_member_promote = audit_count(
        &pool,
        member,
        profile_id,
        "skill.promoted",
        "skill_revision",
        Some(&promote_id),
    )
    .await;
    let before_member_rollback = audit_count(
        &pool,
        member,
        profile_id,
        "skill.rolled_back",
        "skill",
        Some(&skill_id_text),
    )
    .await;
    let before_viewer_evaluate = audit_count(
        &pool,
        viewer,
        profile_id,
        "skill.evaluated",
        "skill_revision",
        Some(&revision_id_text),
    )
    .await;
    let before_viewer_promote = audit_count(
        &pool,
        viewer,
        profile_id,
        "skill.promoted",
        "skill_revision",
        Some(&promote_id),
    )
    .await;
    let before_viewer_rollback = audit_count(
        &pool,
        viewer,
        profile_id,
        "skill.rolled_back",
        "skill",
        Some(&skill_id_text),
    )
    .await;
    for cookie in [&member_cookie, &viewer_cookie] {
        for target in [skill_id, missing_skill, foreign_skill] {
            for (method, path, body) in [
                (
                    Method::GET,
                    format!("/api/v1/skills/{target}/history"),
                    None,
                ),
                (
                    Method::POST,
                    format!("/api/v1/skills/{target}/revisions"),
                    Some(json!({"content":"x","reason":"x","source_conversation_ids":[]})),
                ),
                (
                    Method::POST,
                    format!("/api/v1/skills/{target}/revisions/1/evaluate"),
                    Some(json!({"evaluation":valid_evaluation(1)})),
                ),
                (
                    Method::POST,
                    format!("/api/v1/skills/{target}/revisions/1/promote"),
                    Some(json!({"reason":"x"})),
                ),
                (
                    Method::POST,
                    format!("/api/v1/skills/{target}/rollback"),
                    Some(json!({"target_revision":1,"reason":"x"})),
                ),
            ] {
                let (status, response) = request_json(&app, method, &path, cookie, body).await;
                assert_eq!(status, StatusCode::NOT_FOUND);
                assert_eq!(response["code"], "not_found");
                assert_eq!(response["message"], "resource not found");
            }
        }
    }
    let (foreign_status, foreign_response) = request_json(
        &app,
        Method::GET,
        &format!("/api/v1/skills/{foreign_skill}/history"),
        &foreign_cookie,
        None,
    )
    .await;
    assert_eq!(foreign_status, StatusCode::OK);
    assert!(foreign_response.is_array());
    assert_eq!(skill_side_effect_counts(&pool, skill_id).await, before);
    assert_eq!(
        audit_count(
            &pool,
            member,
            profile_id,
            "skill.revision_created",
            "skill_revision",
            None
        )
        .await,
        before_member_audits
    );
    assert_eq!(
        audit_count(
            &pool,
            member,
            profile_id,
            "skill.evaluated",
            "skill_revision",
            Some(&revision_id_text)
        )
        .await,
        before_member_evaluate
    );
    assert_eq!(
        audit_count(
            &pool,
            member,
            profile_id,
            "skill.promoted",
            "skill_revision",
            Some(&promote_id)
        )
        .await,
        before_member_promote
    );
    assert_eq!(
        audit_count(
            &pool,
            member,
            profile_id,
            "skill.rolled_back",
            "skill",
            Some(&skill_id_text)
        )
        .await,
        before_member_rollback
    );
    assert_eq!(
        audit_count(
            &pool,
            viewer,
            profile_id,
            "skill.evaluated",
            "skill_revision",
            Some(&revision_id_text)
        )
        .await,
        before_viewer_evaluate
    );
    assert_eq!(
        audit_count(
            &pool,
            viewer,
            profile_id,
            "skill.promoted",
            "skill_revision",
            Some(&promote_id)
        )
        .await,
        before_viewer_promote
    );
    assert_eq!(
        audit_count(
            &pool,
            viewer,
            profile_id,
            "skill.rolled_back",
            "skill",
            Some(&skill_id_text)
        )
        .await,
        before_viewer_rollback
    );
    assert_eq!(
        audit_count(
            &pool,
            viewer,
            profile_id,
            "skill.revision_created",
            "skill_revision",
            None
        )
        .await,
        before_viewer_audits
    );
}

#[tokio::test]
async fn evaluating_manually_promoted_revision_returns_promoted_true() {
    let Some(url) = std::env::var("GOBROWSE_TEST_DATABASE_URL").ok() else {
        eprintln!("GOBROWSE_TEST_DATABASE_URL unset; skipping");
        return;
    };
    let _lock = common::acquire_test_lock(&url).await;
    let pool = test_pool().await.expect("pool");
    let profile_id = profile(&pool, "promoted-response-test").await;
    let (_owner, cookie) = create_session(&pool, profile_id, "OWNER").await;
    let (skill_id, _) =
        insert_skill_fixture(&pool, profile_id, None, "promoted-response", "manual").await;
    let mut promotion_tx = pool.begin().await.expect("promotion transaction");
    sqlx::query("UPDATE skill_revisions SET promoted=true WHERE skill_id=$1 AND revision=1")
        .bind(skill_id)
        .execute(&mut *promotion_tx)
        .await
        .expect("promote fixture");
    sqlx::query("UPDATE skills SET active_revision=1 WHERE id=$1")
        .bind(skill_id)
        .execute(&mut *promotion_tx)
        .await
        .expect("activate fixture");
    promotion_tx
        .commit()
        .await
        .expect("commit fixture promotion");
    let app = router(
        AppState::new(pool.clone(), test_settings(&url))
            .await
            .expect("state"),
    );
    let (status, response) = request_json(
        &app,
        Method::POST,
        &format!("/api/v1/skills/{skill_id}/revisions/1/evaluate"),
        &cookie,
        Some(json!({"evaluation":valid_evaluation(1)})),
    )
    .await;
    assert_eq!(status, StatusCode::OK, "{response}");
    assert_eq!(response["promoted"], true);
    let persisted: (bool, Option<i64>) = sqlx::query_as("SELECT r.promoted,s.active_revision FROM skill_revisions r JOIN skills s ON s.id=r.skill_id WHERE r.id=(SELECT id FROM skill_revisions WHERE skill_id=$1 AND revision=1)").bind(skill_id).fetch_one(&pool).await.expect("persisted promotion");
    assert_eq!(persisted, (true, Some(1)));
    let evaluated_audits: i64 = sqlx::query_scalar("SELECT count(*) FROM audit_events WHERE action='skill.evaluated' AND resource_id=(SELECT id::text FROM skill_revisions WHERE skill_id=$1 AND revision=1) AND outcome='success'").bind(skill_id).fetch_one(&pool).await.expect("evaluation audit");
    assert_eq!(evaluated_audits, 1);
    let automatic_audits: i64 = sqlx::query_scalar("SELECT count(*) FROM audit_events WHERE action='skill.automatically_promoted' AND resource_id=(SELECT id::text FROM skill_revisions WHERE skill_id=$1 AND revision=1)").bind(skill_id).fetch_one(&pool).await.expect("automatic audit");
    assert_eq!(automatic_audits, 0);
}

#[tokio::test]
async fn duplicate_skill_names_return_conflict_without_sql_details() {
    let Some(url) = std::env::var("GOBROWSE_TEST_DATABASE_URL").ok() else {
        eprintln!("GOBROWSE_TEST_DATABASE_URL unset; skipping");
        return;
    };
    let _lock = common::acquire_test_lock(&url).await;
    let pool = test_pool().await.expect("pool");
    let profile_id = profile(&pool, "duplicate-name-test").await;
    let (_owner, cookie) = create_session(&pool, profile_id, "OWNER").await;
    let app = router(
        AppState::new(pool.clone(), test_settings(&url))
            .await
            .expect("state"),
    );
    let body = json!({"name":"duplicate","content":"x","reason":"x","source_conversation_ids":[]});
    let (status, _) = request_json(
        &app,
        Method::POST,
        "/api/v1/skills",
        &cookie,
        Some(body.clone()),
    )
    .await;
    assert_eq!(status, StatusCode::CREATED);
    let (status, error) =
        request_json(&app, Method::POST, "/api/v1/skills", &cookie, Some(body)).await;
    assert_eq!(status, StatusCode::CONFLICT);
    assert_eq!(error["code"], "conflict");
    assert!(
        !error
            .to_string()
            .contains("skills_profile_global_name_unique")
    );
    let workspace = insert_workspace(
        &pool,
        profile_id,
        sqlx::query_scalar("SELECT id FROM users WHERE primary_profile_id=$1 LIMIT 1")
            .bind(profile_id)
            .fetch_one(&pool)
            .await
            .expect("owner id"),
    )
    .await;
    let workspace_body = json!({"name":"workspace-duplicate","content":"x","reason":"x","workspace_id":workspace,"source_conversation_ids":[]});
    let (status, _) = request_json(
        &app,
        Method::POST,
        "/api/v1/skills",
        &cookie,
        Some(workspace_body.clone()),
    )
    .await;
    assert_eq!(status, StatusCode::CREATED);
    let (status, error) = request_json(
        &app,
        Method::POST,
        "/api/v1/skills",
        &cookie,
        Some(workspace_body),
    )
    .await;
    assert_eq!(status, StatusCode::CONFLICT);
    assert_eq!(error["code"], "conflict");
}

#[tokio::test]
async fn duplicate_and_inaccessible_skill_sources_return_validation() {
    let Some(url) = std::env::var("GOBROWSE_TEST_DATABASE_URL").ok() else {
        eprintln!("GOBROWSE_TEST_DATABASE_URL unset; skipping");
        return;
    };
    let _lock = common::acquire_test_lock(&url).await;
    let pool = test_pool().await.expect("pool");
    let profile_id = profile(&pool, "source-validation-test").await;
    let (owner, cookie) = create_session(&pool, profile_id, "OWNER").await;
    let workspace = insert_workspace(&pool, profile_id, owner).await;
    let (skill_id, _) =
        insert_skill_fixture(&pool, profile_id, Some(workspace), "source-skill", "manual").await;
    let valid_source = Uuid::now_v7();
    let deleted_source = Uuid::now_v7();
    let wrong_workspace = Uuid::now_v7();
    let cross_profile = Uuid::now_v7();
    let other_profile = profile(&pool, "source-foreign-profile").await;
    let other_workspace = insert_workspace(
        &pool,
        other_profile,
        create_session(&pool, other_profile, "OWNER").await.0,
    )
    .await;
    sqlx::query("INSERT INTO conversations (id,profile_id,workspace_id,title) VALUES ($1,$2,$3,'valid'),($4,$2,$3,'deleted'),($5,$2,$6,'wrong'),($7,$8,NULL,'foreign')").bind(valid_source).bind(profile_id).bind(workspace).bind(deleted_source).bind(wrong_workspace).bind(other_workspace).bind(cross_profile).bind(other_profile).execute(&pool).await.expect("source cases");
    sqlx::query("UPDATE conversations SET status='deleted' WHERE id=$1")
        .bind(deleted_source)
        .execute(&pool)
        .await
        .expect("delete source");
    let app = router(
        AppState::new(pool.clone(), test_settings(&url))
            .await
            .expect("state"),
    );
    let before_audits: i64 = audit_count(
        &pool,
        owner,
        profile_id,
        "skill.revision_created",
        "skill_revision",
        None,
    )
    .await;
    for ids in [
        vec![valid_source, valid_source],
        vec![Uuid::nil()],
        vec![Uuid::now_v7()],
        vec![deleted_source],
        vec![cross_profile],
        vec![wrong_workspace],
    ] {
        let (status, _) = request_json(
            &app,
            Method::POST,
            &format!("/api/v1/skills/{skill_id}/revisions"),
            &cookie,
            Some(json!({"content":"x","reason":"x","source_conversation_ids":ids})),
        )
        .await;
        assert_eq!(status, StatusCode::UNPROCESSABLE_ENTITY);
    }
    assert_eq!(skill_side_effect_counts(&pool, skill_id).await.0.len(), 1);
    let after_audits: i64 = audit_count(
        &pool,
        owner,
        profile_id,
        "skill.revision_created",
        "skill_revision",
        None,
    )
    .await;
    assert_eq!(after_audits, before_audits);
}

#[tokio::test]
async fn automatic_promotion_requires_recorded_non_regression() {
    let Some(url) = std::env::var("GOBROWSE_TEST_DATABASE_URL").ok() else {
        eprintln!("GOBROWSE_TEST_DATABASE_URL unset; skipping");
        return;
    };
    let _lock = common::acquire_test_lock(&url).await;
    let pool = test_pool().await.expect("pool");
    let profile_id = profile(&pool, "automatic-api").await;
    let (owner, cookie) = create_session(&pool, profile_id, "OWNER").await;
    let app = router(
        AppState::new(pool.clone(), test_settings(&url))
            .await
            .expect("state"),
    );
    let (status,created)=request_json(&app,Method::POST,"/api/v1/skills",&cookie,Some(json!({"name":"automatic","content":"v1","reason":"initial","promotion_policy":"automatic","source_conversation_ids":[]}))).await;
    assert_eq!(status, StatusCode::CREATED);
    let skill_id = Uuid::parse_str(created["id"].as_str().expect("skill")).expect("uuid");
    let baseline = json!({"deterministic_checks_passed":true,"attempts":10,"successful_attempts":9,"steps":1,"retries":0,"errors":1,"duration_ms":1,"user_corrections":1});
    let (status, response) = request_json(
        &app,
        Method::POST,
        &format!("/api/v1/skills/{skill_id}/revisions/1/evaluate"),
        &cookie,
        Some(json!({"evaluation":baseline})),
    )
    .await;
    assert_eq!(status, StatusCode::OK, "{response}");
    assert_eq!(response["promoted"], true);
    let baseline_id = response["id"]
        .as_str()
        .expect("baseline revision ID")
        .to_owned();
    let cases = [
        json!({"deterministic_checks_passed":true,"attempts":0,"successful_attempts":0,"steps":1,"retries":0,"errors":1,"duration_ms":1,"user_corrections":0}),
        json!({"deterministic_checks_passed":false,"attempts":10,"successful_attempts":10,"steps":1,"retries":0,"errors":0,"duration_ms":1,"user_corrections":0}),
        json!({"deterministic_checks_passed":true,"attempts":10,"successful_attempts":8,"steps":1,"retries":0,"errors":0,"duration_ms":1,"user_corrections":0}),
        json!({"deterministic_checks_passed":true,"attempts":10,"successful_attempts":9,"steps":1,"retries":0,"errors":2,"duration_ms":1,"user_corrections":0}),
        json!({"deterministic_checks_passed":true,"attempts":10,"successful_attempts":9,"steps":1,"retries":0,"errors":0,"duration_ms":1,"user_corrections":2}),
        json!({"deterministic_checks_passed":true,"attempts":10,"successful_attempts":10,"steps":1,"retries":0,"errors":0,"duration_ms":1,"user_corrections":0}),
    ];
    for (offset, evidence) in cases.into_iter().enumerate() {
        let (status,revision)=request_json(&app,Method::POST,&format!("/api/v1/skills/{skill_id}/revisions"),&cookie,Some(json!({"content":format!("v{}",offset+2),"reason":"case","source_conversation_ids":[]}))).await;
        assert_eq!(status, StatusCode::CREATED);
        let number = revision["revision"].as_i64().expect("revision");
        let (status, response) = request_json(
            &app,
            Method::POST,
            &format!("/api/v1/skills/{skill_id}/revisions/{number}/evaluate"),
            &cookie,
            Some(json!({"evaluation":evidence})),
        )
        .await;
        assert_eq!(status, StatusCode::OK, "{response}");
        assert_eq!(response["promoted"], offset == 5);
        let revision_id = response["id"].as_str().expect("evaluation revision ID");
        assert_eq!(
            audit_count(
                &pool,
                owner,
                profile_id,
                "skill.evaluated",
                "skill_revision",
                Some(revision_id)
            )
            .await,
            1
        );
        let expected = if offset == 5 { 7 } else { 1 };
        let persisted: i64 = sqlx::query_scalar("SELECT active_revision FROM skills WHERE id=$1")
            .bind(skill_id)
            .fetch_one(&pool)
            .await
            .expect("case active");
        let persisted_promoted: i64 = sqlx::query_scalar(
            "SELECT revision FROM skill_revisions WHERE skill_id=$1 AND promoted",
        )
        .bind(skill_id)
        .fetch_one(&pool)
        .await
        .expect("case promoted");
        assert_eq!((persisted, persisted_promoted), (expected, expected));
    }
    let state:(i64,i64)=sqlx::query_as("SELECT s.active_revision, (SELECT r.revision FROM skill_revisions r WHERE r.skill_id=s.id AND r.promoted) FROM skills s WHERE s.id=$1").bind(skill_id).fetch_one(&pool).await.expect("promotion state");
    assert_eq!(state, (7, 7));
    let eval_audits:i64=sqlx::query_scalar("SELECT count(*) FROM audit_events WHERE actor_user_id=$1 AND action='skill.evaluated' AND outcome='success'").bind(owner).fetch_one(&pool).await.expect("evaluation audits");
    assert_eq!(eval_audits, 7);
    let auto_resources:Vec<String>=sqlx::query_scalar("SELECT resource_id FROM audit_events WHERE actor_user_id=$1 AND profile_id=$2 AND action='skill.automatically_promoted' AND resource_type='skill_revision' AND outcome='success' ORDER BY id").bind(owner).bind(profile_id).fetch_all(&pool).await.expect("automatic resources");
    assert_eq!(auto_resources.len(), 2);
    assert!(auto_resources.contains(&baseline_id));
    let qualifying_id: String =
        sqlx::query_scalar("SELECT id::text FROM skill_revisions WHERE skill_id=$1 AND revision=7")
            .bind(skill_id)
            .fetch_one(&pool)
            .await
            .expect("qualifying ID");
    assert!(auto_resources.contains(&qualifying_id));
}

#[tokio::test]
async fn skill_database_enforces_evaluation_source_and_promotion_invariants() {
    let Some(url) = std::env::var("GOBROWSE_TEST_DATABASE_URL").ok() else {
        eprintln!("GOBROWSE_TEST_DATABASE_URL unset; skipping");
        return;
    };
    let _lock = common::acquire_test_lock(&url).await;
    let pool = test_pool().await.expect("pool");
    let profile_id = profile(&pool, "raw-sql-invariants").await;
    let (skill_id, revision_id) =
        insert_skill_fixture(&pool, profile_id, None, "raw-invariants", "manual").await;
    assert!(
        sqlx::query("UPDATE skill_revisions SET evaluation='{}' WHERE id=$1")
            .bind(revision_id)
            .execute(&pool)
            .await
            .is_err()
    );
    sqlx::query("UPDATE skill_revisions SET evaluation=$1 WHERE id=$2")
        .bind(valid_evaluation(1))
        .bind(revision_id)
        .execute(&pool)
        .await
        .expect("valid evidence");
    assert!(
        sqlx::query("UPDATE skill_revisions SET evaluation=$1 WHERE id=$2")
            .bind(valid_evaluation(2))
            .bind(revision_id)
            .execute(&pool)
            .await
            .is_err()
    );
    assert!(
        sqlx::query("UPDATE skill_revisions SET evaluation=NULL WHERE id=$1")
            .bind(revision_id)
            .execute(&pool)
            .await
            .is_err()
    );
    let valid = Uuid::now_v7();
    let deleted = Uuid::now_v7();
    let cross = Uuid::now_v7();
    let source_profile = profile(&pool, "raw-source-profile").await;
    let workspace = insert_workspace(
        &pool,
        profile_id,
        sqlx::query_scalar("SELECT id FROM users WHERE primary_profile_id=$1 LIMIT 1")
            .bind(profile_id)
            .fetch_one(&pool)
            .await
            .expect("owner"),
    )
    .await;
    let wrong_workspace = insert_workspace(
        &pool,
        profile_id,
        sqlx::query_scalar("SELECT id FROM users WHERE primary_profile_id=$1 LIMIT 1")
            .bind(profile_id)
            .fetch_one(&pool)
            .await
            .expect("owner"),
    )
    .await;
    let wrong_conversation = Uuid::now_v7();
    let scoped_skill =
        insert_skill_fixture(&pool, profile_id, Some(workspace), "raw-scoped", "manual")
            .await
            .0;
    sqlx::query("INSERT INTO conversations (id,profile_id,workspace_id,title,status) VALUES ($1,$2,$3,'valid','active'),($4,$2,NULL,'deleted','deleted'),($5,$6,NULL,'cross','active'),($7,$2,$8,'wrong workspace','active')").bind(valid).bind(profile_id).bind(workspace).bind(deleted).bind(cross).bind(source_profile).bind(wrong_conversation).bind(wrong_workspace).execute(&pool).await.expect("raw source rows");
    let missing = Uuid::now_v7();
    for (revision, ids) in [
        (100, vec![Uuid::nil()]),
        (101, vec![valid, valid]),
        (102, vec![missing]),
        (103, vec![deleted]),
        (104, vec![cross]),
    ] {
        let result=sqlx::query("INSERT INTO skill_revisions (id,skill_id,revision,content,author,reason,source_conversation_ids) VALUES ($1,$2,$3,'bad','x','x',$4)").bind(Uuid::now_v7()).bind(skill_id).bind(revision).bind(ids).execute(&pool).await;
        let constraint = result.as_ref().err().and_then(|error| match error {
            sqlx::Error::Database(database) => database.constraint(),
            _ => None,
        });
        assert_eq!(constraint, Some("skill_revisions_sources_valid"));
    }
    let result=sqlx::query("INSERT INTO skill_revisions (id,skill_id,revision,content,author,reason,source_conversation_ids) VALUES ($1,$2,105,'bad','x','x',$3)").bind(Uuid::now_v7()).bind(scoped_skill).bind(vec![wrong_conversation]).execute(&pool).await;
    let constraint = result.as_ref().err().and_then(|error| match error {
        sqlx::Error::Database(database) => database.constraint(),
        _ => None,
    });
    assert_eq!(constraint, Some("skill_revisions_sources_valid"));
    assert!(
        sqlx::query("UPDATE skill_revisions SET source_conversation_ids=$1 WHERE id=$2")
            .bind(vec![Uuid::nil()])
            .bind(revision_id)
            .execute(&pool)
            .await
            .is_err()
    );
    let mut tx = pool.begin().await.expect("transaction");
    sqlx::query("UPDATE skill_revisions SET promoted=true WHERE id=$1")
        .bind(revision_id)
        .execute(&mut *tx)
        .await
        .expect("promote in transaction");
    assert!(tx.commit().await.is_err());
    let _ = skill_id;
}

#[tokio::test]
async fn skill_integrity_quarantines_reject_update_delete_and_truncate() {
    let Some(url) = std::env::var("GOBROWSE_TEST_DATABASE_URL").ok() else {
        eprintln!("GOBROWSE_TEST_DATABASE_URL unset; skipping");
        return;
    };
    let _lock = common::acquire_test_lock(&url).await;
    let pool = test_pool().await.expect("pool");
    let profile_id = profile(&pool, "quarantine-test").await;
    let (skill_id, revision_id) =
        insert_skill_fixture(&pool, profile_id, None, "quarantine", "manual").await;
    sqlx::query("INSERT INTO skill_integrity_quarantine (skill_id,profile_id,name,issue,detail) VALUES ($1,$2,'q','test','{}')").bind(skill_id).bind(profile_id).execute(&pool).await.expect("insert skill quarantine");
    sqlx::query("INSERT INTO skill_revision_integrity_quarantine (revision_id,skill_id,issue) VALUES ($1,$2,'test')").bind(revision_id).bind(skill_id).execute(&pool).await.expect("insert revision quarantine");
    assert!(
        sqlx::query("UPDATE skill_integrity_quarantine SET issue='x'")
            .execute(&pool)
            .await
            .is_err()
    );
    assert!(
        sqlx::query("DELETE FROM skill_integrity_quarantine")
            .execute(&pool)
            .await
            .is_err()
    );
    assert!(
        sqlx::query("TRUNCATE skill_integrity_quarantine")
            .execute(&pool)
            .await
            .is_err()
    );
    assert!(
        sqlx::query("UPDATE skill_revision_integrity_quarantine SET issue='x'")
            .execute(&pool)
            .await
            .is_err()
    );
    assert!(
        sqlx::query("DELETE FROM skill_revision_integrity_quarantine")
            .execute(&pool)
            .await
            .is_err()
    );
    assert!(
        sqlx::query("TRUNCATE skill_revision_integrity_quarantine")
            .execute(&pool)
            .await
            .is_err()
    );
}

#[tokio::test]
async fn concurrent_skill_revisions_receive_consecutive_numbers() {
    concurrent_revision_fixture().await;
}
#[tokio::test]
async fn concurrent_skill_evaluations_commit_once_and_conflict_once() {
    concurrent_evaluation_fixture().await;
}
#[tokio::test]
async fn concurrent_skill_promotions_leave_one_matching_active_revision() {
    concurrent_promotion_fixture().await;
}
#[tokio::test]
async fn source_deletion_wins_before_revision_commit_is_rejected() {
    concurrent_source_delete_fixture().await;
}
#[tokio::test]
async fn revoked_editor_cannot_commit_skill_revision() {
    concurrent_revocation_fixture().await;
}

async fn concurrent_revision_fixture() {
    let Some(url) = std::env::var("GOBROWSE_TEST_DATABASE_URL").ok() else {
        return;
    };
    let _lock = common::acquire_test_lock(&url).await;
    let pool = test_pool().await.expect("pool");
    let profile_id = profile(&pool, "revision-race").await;
    let (owner, cookie) = create_session(&pool, profile_id, "OWNER").await;
    let (skill_id, _) =
        insert_skill_fixture(&pool, profile_id, None, "revision-race", "manual").await;
    let app = router(
        AppState::new(pool.clone(), test_settings(&url))
            .await
            .expect("state"),
    );
    let request = move |app: Router, cookie: String| async move {
        request_json(
            &app,
            Method::POST,
            &format!("/api/v1/skills/{skill_id}/revisions"),
            &cookie,
            Some(json!({"content":"race","reason":"race","source_conversation_ids":[]})),
        )
        .await
    };
    let mut control = pool.begin().await.expect("profile control");
    sqlx::query("SELECT id FROM profiles WHERE id=$1 FOR UPDATE")
        .bind(profile_id)
        .fetch_one(&mut *control)
        .await
        .expect("lock profile");
    let barrier = Arc::new(Barrier::new(3));
    let first_barrier = barrier.clone();
    let second_barrier = barrier.clone();
    let first = tokio::spawn({
        let app = app.clone();
        let cookie = cookie.clone();
        async move {
            first_barrier.wait().await;
            request(app, cookie).await
        }
    });
    let second = tokio::spawn({
        let app = app.clone();
        async move {
            second_barrier.wait().await;
            request(app, cookie).await
        }
    });
    barrier.wait().await;
    wait_for_lock_waiters(&pool, "SELECT id FROM profiles WHERE id=", 2).await;
    control.commit().await.expect("release profile");
    let (one, two) = tokio::time::timeout(std::time::Duration::from_secs(5), async {
        (first.await.expect("first"), second.await.expect("second"))
    })
    .await
    .expect("race timeout");
    assert_eq!(one.0, StatusCode::CREATED);
    assert_eq!(two.0, StatusCode::CREATED);
    let one_revision = one.1["id"].as_str().expect("first revision ID");
    let two_revision = two.1["id"].as_str().expect("second revision ID");
    assert_eq!(
        audit_count(
            &pool,
            owner,
            profile_id,
            "skill.revision_created",
            "skill_revision",
            Some(one_revision)
        )
        .await,
        1
    );
    assert_eq!(
        audit_count(
            &pool,
            owner,
            profile_id,
            "skill.revision_created",
            "skill_revision",
            Some(two_revision)
        )
        .await,
        1
    );
    let revisions: Vec<i64> = sqlx::query_scalar(
        "SELECT revision FROM skill_revisions WHERE skill_id=$1 ORDER BY revision",
    )
    .bind(skill_id)
    .fetch_all(&pool)
    .await
    .expect("revisions");
    assert_eq!(revisions, vec![1, 2, 3]);
    let audits:i64=sqlx::query_scalar("SELECT count(*) FROM audit_events WHERE actor_user_id=$1 AND profile_id=$2 AND action='skill.revision_created' AND outcome='success'").bind(owner).bind(profile_id).fetch_one(&pool).await.expect("revision audits");
    assert_eq!(audits, 2);
}
async fn concurrent_evaluation_fixture() {
    let Some(url) = std::env::var("GOBROWSE_TEST_DATABASE_URL").ok() else {
        return;
    };
    let _lock = common::acquire_test_lock(&url).await;
    let pool = test_pool().await.expect("pool");
    let profile_id = profile(&pool, "evaluation-race").await;
    let (owner, cookie) = create_session(&pool, profile_id, "OWNER").await;
    let (skill_id, revision_id) =
        insert_skill_fixture(&pool, profile_id, None, "evaluation-race", "manual").await;
    let app = router(
        AppState::new(pool.clone(), test_settings(&url))
            .await
            .expect("state"),
    );
    let request = move |app: Router, cookie: String| async move {
        request_json(
            &app,
            Method::POST,
            &format!("/api/v1/skills/{skill_id}/revisions/1/evaluate"),
            &cookie,
            Some(json!({"evaluation":valid_evaluation(1)})),
        )
        .await
    };
    let mut control = pool.begin().await.expect("profile control");
    sqlx::query("SELECT id FROM profiles WHERE id=$1 FOR UPDATE")
        .bind(profile_id)
        .fetch_one(&mut *control)
        .await
        .expect("lock profile");
    let barrier = Arc::new(Barrier::new(3));
    let first_barrier = barrier.clone();
    let second_barrier = barrier.clone();
    let first = tokio::spawn({
        let app = app.clone();
        let cookie = cookie.clone();
        async move {
            first_barrier.wait().await;
            request(app, cookie).await
        }
    });
    let second = tokio::spawn({
        let app = app.clone();
        async move {
            second_barrier.wait().await;
            request(app, cookie).await
        }
    });
    barrier.wait().await;
    wait_for_lock_waiters(&pool, "SELECT id FROM profiles WHERE id=", 2).await;
    control.commit().await.expect("release profile");
    let (one, two) = tokio::time::timeout(std::time::Duration::from_secs(5), async {
        (first.await.expect("first"), second.await.expect("second"))
    })
    .await
    .expect("race timeout");
    let statuses = [one.0, two.0];
    assert!(statuses.contains(&StatusCode::OK));
    assert!(statuses.contains(&StatusCode::CONFLICT));
    let revision_id_text = revision_id.to_string();
    assert_eq!(
        audit_count(
            &pool,
            owner,
            profile_id,
            "skill.evaluated",
            "skill_revision",
            Some(&revision_id_text)
        )
        .await,
        1
    );
    let count: i64 = sqlx::query_scalar(
        "SELECT count(*) FROM skill_revisions WHERE skill_id=$1 AND evaluation IS NOT NULL",
    )
    .bind(skill_id)
    .fetch_one(&pool)
    .await
    .expect("count");
    assert_eq!(count, 1);
    let audits:i64=sqlx::query_scalar("SELECT count(*) FROM audit_events WHERE actor_user_id=$1 AND profile_id=$2 AND action='skill.evaluated' AND outcome='success'").bind(owner).bind(profile_id).fetch_one(&pool).await.expect("evaluation audits");
    assert_eq!(audits, 1);
}
async fn concurrent_promotion_fixture() {
    let Some(url) = std::env::var("GOBROWSE_TEST_DATABASE_URL").ok() else {
        return;
    };
    let _lock = common::acquire_test_lock(&url).await;
    let pool = test_pool().await.expect("pool");
    let profile_id = profile(&pool, "promotion-race").await;
    let (owner, cookie) = create_session(&pool, profile_id, "OWNER").await;
    let (skill_id, _) =
        insert_skill_fixture(&pool, profile_id, None, "promotion-race", "manual").await;
    sqlx::query("INSERT INTO skill_revisions (id,skill_id,revision,content,author,reason) VALUES ($1,$2,2,'x','x','x')").bind(Uuid::now_v7()).bind(skill_id).execute(&pool).await.expect("revision");
    let app = router(
        AppState::new(pool.clone(), test_settings(&url))
            .await
            .expect("state"),
    );
    let request = move |app: Router, cookie: String, revision: i64| async move {
        request_json(
            &app,
            Method::POST,
            &format!("/api/v1/skills/{skill_id}/revisions/{revision}/promote"),
            &cookie,
            Some(json!({"reason":"race"})),
        )
        .await
    };
    let mut control = pool.begin().await.expect("profile control");
    sqlx::query("SELECT id FROM profiles WHERE id=$1 FOR UPDATE")
        .bind(profile_id)
        .fetch_one(&mut *control)
        .await
        .expect("lock profile");
    let barrier = Arc::new(Barrier::new(3));
    let first_barrier = barrier.clone();
    let second_barrier = barrier.clone();
    let first = tokio::spawn({
        let app = app.clone();
        let cookie = cookie.clone();
        async move {
            first_barrier.wait().await;
            request(app, cookie, 1).await
        }
    });
    let second = tokio::spawn({
        let app = app.clone();
        async move {
            second_barrier.wait().await;
            request(app, cookie, 2).await
        }
    });
    barrier.wait().await;
    wait_for_lock_waiters(&pool, "SELECT id FROM profiles WHERE id=", 2).await;
    control.commit().await.expect("release profile");
    let (one, two) = tokio::time::timeout(std::time::Duration::from_secs(5), async {
        (first.await.expect("first"), second.await.expect("second"))
    })
    .await
    .expect("race timeout");
    assert_eq!(one.0, StatusCode::OK);
    assert_eq!(two.0, StatusCode::OK);
    let row=sqlx::query("SELECT active_revision,(SELECT count(*) FROM skill_revisions WHERE skill_id=$1 AND promoted) AS promoted_count,(SELECT revision FROM skill_revisions WHERE skill_id=$1 AND promoted) AS promoted_revision FROM skills WHERE id=$1").bind(skill_id).fetch_one(&pool).await.expect("state");
    assert_eq!(row.get::<i64, _>("promoted_count"), 1);
    assert_eq!(
        row.get::<i64, _>("active_revision"),
        row.get::<i64, _>("promoted_revision")
    );
    assert_eq!(
        audit_count(
            &pool,
            owner,
            profile_id,
            "skill.promoted",
            "skill_revision",
            Some(&format!("{skill_id}:1"))
        )
        .await,
        1
    );
    assert_eq!(
        audit_count(
            &pool,
            owner,
            profile_id,
            "skill.promoted",
            "skill_revision",
            Some(&format!("{skill_id}:2"))
        )
        .await,
        1
    );
}
async fn concurrent_source_delete_fixture() {
    let Some(url) = std::env::var("GOBROWSE_TEST_DATABASE_URL").ok() else {
        return;
    };
    let _lock = common::acquire_test_lock(&url).await;
    let pool = test_pool().await.expect("pool");
    let profile_id = profile(&pool, "source-race").await;
    let (owner, cookie) = create_session(&pool, profile_id, "OWNER").await;
    let conversation = Uuid::now_v7();
    sqlx::query("INSERT INTO conversations (id,profile_id,title,created_by_user_id) VALUES ($1,$2,'race',$3)").bind(conversation).bind(profile_id).bind(owner).execute(&pool).await.expect("conversation");
    let (skill_id, _) =
        insert_skill_fixture(&pool, profile_id, None, "source-race", "manual").await;
    let app = router(
        AppState::new(pool.clone(), test_settings(&url))
            .await
            .expect("state"),
    );
    let mut deletion = pool.begin().await.expect("deletion tx");
    sqlx::query("SELECT id FROM conversations WHERE id=$1 FOR UPDATE")
        .bind(conversation)
        .fetch_one(&mut *deletion)
        .await
        .expect("lock conversation");
    let audit_before = audit_count(
        &pool,
        owner,
        profile_id,
        "skill.revision_created",
        "skill_revision",
        None,
    )
    .await;
    let request_app = app.clone();
    let request_cookie = cookie.clone();
    let request_path = format!("/api/v1/skills/{skill_id}/revisions");
    let request = tokio::spawn(async move {
        request_json(
            &request_app,
            Method::POST,
            &request_path,
            &request_cookie,
            Some(json!({"content":"x","reason":"source","source_conversation_ids":[conversation]})),
        )
        .await
    });
    wait_for_lock_waiters(&pool, "SELECT c.id FROM conversations c", 1).await;
    sqlx::query("DELETE FROM conversations WHERE id=$1")
        .bind(conversation)
        .execute(&mut *deletion)
        .await
        .expect("delete conversation");
    deletion.commit().await.expect("commit deletion");
    let (status, _) = tokio::time::timeout(std::time::Duration::from_secs(5), request)
        .await
        .expect("race timeout")
        .expect("request");
    assert_eq!(status, StatusCode::UNPROCESSABLE_ENTITY);
    assert_eq!(
        sqlx::query_scalar::<_, i64>("SELECT count(*) FROM skill_revisions WHERE skill_id=$1")
            .bind(skill_id)
            .fetch_one(&pool)
            .await
            .expect("revisions"),
        1
    );
    assert_eq!(
        audit_count(
            &pool,
            owner,
            profile_id,
            "skill.revision_created",
            "skill_revision",
            None
        )
        .await,
        audit_before
    );
}
async fn concurrent_revocation_fixture() {
    let Some(url) = std::env::var("GOBROWSE_TEST_DATABASE_URL").ok() else {
        return;
    };
    let _lock = common::acquire_test_lock(&url).await;
    let pool = test_pool().await.expect("pool");
    let profile_id = profile(&pool, "revocation-race").await;
    let (owner, _owner_cookie) = create_session(&pool, profile_id, "OWNER").await;
    let (editor, editor_cookie) = create_session(&pool, profile_id, "MEMBER").await;
    let workspace = insert_workspace(&pool, profile_id, owner).await;
    sqlx::query(
        "INSERT INTO workspace_memberships (workspace_id,user_id,access) VALUES ($1,$2,'EDITOR')",
    )
    .bind(workspace)
    .bind(editor)
    .execute(&pool)
    .await
    .expect("editor membership");
    let (skill_id, _) = insert_skill_fixture(
        &pool,
        profile_id,
        Some(workspace),
        "revocation-race",
        "manual",
    )
    .await;
    let app = router(
        AppState::new(pool.clone(), test_settings(&url))
            .await
            .expect("state"),
    );
    let mut revocation = pool.begin().await.expect("revocation tx");
    sqlx::query("SELECT workspace_id,user_id FROM workspace_memberships WHERE workspace_id=$1 AND user_id=$2 FOR UPDATE").bind(workspace).bind(editor).fetch_one(&mut *revocation).await.expect("lock membership");
    let audit_before = audit_count(
        &pool,
        editor,
        profile_id,
        "skill.revision_created",
        "skill_revision",
        None,
    )
    .await;
    let request_app = app.clone();
    let request_cookie = editor_cookie.clone();
    let request_path = format!("/api/v1/skills/{skill_id}/revisions");
    let request = tokio::spawn(async move {
        request_json(
            &request_app,
            Method::POST,
            &request_path,
            &request_cookie,
            Some(json!({"content":"x","reason":"revocation","source_conversation_ids":[]})),
        )
        .await
    });
    wait_for_lock_waiters(&pool, "SELECT access FROM workspace_memberships", 1).await;
    sqlx::query("DELETE FROM workspace_memberships WHERE workspace_id=$1 AND user_id=$2")
        .bind(workspace)
        .bind(editor)
        .execute(&mut *revocation)
        .await
        .expect("revoke");
    revocation.commit().await.expect("commit revocation");
    let (status, _) = tokio::time::timeout(std::time::Duration::from_secs(5), request)
        .await
        .expect("race timeout")
        .expect("request");
    assert_eq!(status, StatusCode::NOT_FOUND);
    assert_eq!(
        sqlx::query_scalar::<_, i64>("SELECT count(*) FROM skill_revisions WHERE skill_id=$1")
            .bind(skill_id)
            .fetch_one(&pool)
            .await
            .expect("revisions"),
        1
    );
    assert_eq!(
        audit_count(
            &pool,
            editor,
            profile_id,
            "skill.revision_created",
            "skill_revision",
            None
        )
        .await,
        audit_before
    );
}
