//! Router and PostgreSQL contracts for inert worktree metadata.

mod common;

use std::path::Path;

use axum::{
    Router,
    body::Body,
    http::{Method, Request, StatusCode, header},
};
use gobrowse_core::worktrees::{
    validate_base_commit, validate_branch, validate_changed_files,
    worktree_path as derive_worktree_path,
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
async fn worktree_database_constraints_match_pure_validation_and_quarantine_is_append_only() {
    let Some(database_url) = std::env::var("GOBROWSE_TEST_DATABASE_URL").ok() else {
        eprintln!("GOBROWSE_TEST_DATABASE_URL is unset; skipping PostgreSQL integration test");
        return;
    };
    let _lock = common::acquire_test_lock(&database_url).await;
    let pool = PgPool::connect(&database_url)
        .await
        .expect("connect to test PostgreSQL");
    db::migrate(&pool).await.expect("apply test migrations");

    let version: i64 =
        sqlx::query_scalar("SELECT schema_version FROM schema_metadata WHERE singleton")
            .fetch_one(&pool)
            .await
            .expect("read schema version");
    assert_eq!(version, 16);
    let branch_fixtures = [
        ("agent/task-safe", true),
        ("agent/closing]bracket", true),
        ("foo//bar", false),
        ("bad[branch", false),
        ("agent/@", false),
    ];
    for (branch, expected) in branch_fixtures {
        let rust_valid = validate_branch(branch).is_ok();
        let sql_valid: bool = sqlx::query_scalar("SELECT gobrowse_valid_worktree_branch($1)")
            .bind(branch)
            .fetch_one(&pool)
            .await
            .expect("evaluate SQL branch validator");
        assert_eq!(
            rust_valid, expected,
            "Rust branch fixture failed for {branch:?}"
        );
        assert_eq!(
            sql_valid, expected,
            "SQL branch fixture failed for {branch:?}"
        );
        assert_eq!(
            rust_valid, sql_valid,
            "Rust and SQL branch validators diverged for {branch:?}"
        );
    }

    let base_commit_fixtures = [
        ("a".repeat(40), true),
        ("B".repeat(64), true),
        ("short".to_owned(), false),
        ("g".repeat(40), false),
        ("a".repeat(39), false),
    ];
    for (base_commit, expected) in base_commit_fixtures {
        let rust_valid = validate_base_commit(&base_commit).is_ok();
        let sql_valid: bool = sqlx::query_scalar("SELECT gobrowse_valid_worktree_base_commit($1)")
            .bind(&base_commit)
            .fetch_one(&pool)
            .await
            .expect("evaluate SQL base-commit validator");
        assert_eq!(
            rust_valid, expected,
            "Rust base-commit fixture failed for {base_commit:?}"
        );
        assert_eq!(
            sql_valid, expected,
            "SQL base-commit fixture failed for {base_commit:?}"
        );
        assert_eq!(
            rust_valid, sql_valid,
            "Rust and SQL base-commit validators diverged for {base_commit:?}"
        );
    }

    let path_task_id = Uuid::now_v7();
    let path_task_simple = path_task_id.simple().to_string();
    let path_task_suffix = &path_task_simple[..12];
    let path_fixtures = [
        ("/srv/repos/gobrowse", true),
        ("relative/repository", false),
        ("/srv//repository", false),
        ("/srv/repos/../repository", false),
        ("/srv/repos/repository/", false),
    ];
    for (repository_root, expected) in path_fixtures {
        let path = format!("{repository_root}/worktrees/task-{path_task_suffix}");
        let rust_path = derive_worktree_path(Path::new(repository_root), path_task_id);
        let rust_valid = rust_path.is_ok();
        if expected {
            assert_eq!(
                rust_path.expect("derive valid worktree path"),
                Path::new(&path),
                "Rust derived path fixture diverged for {repository_root:?}"
            );
        }
        let sql_valid: bool = sqlx::query_scalar("SELECT gobrowse_valid_worktree_path($1, $2)")
            .bind(&path)
            .bind(path_task_id)
            .fetch_one(&pool)
            .await
            .expect("evaluate SQL derived-path validator");
        assert_eq!(
            rust_valid, expected,
            "Rust derived-path fixture failed for {repository_root:?}"
        );
        assert_eq!(
            sql_valid, expected,
            "SQL derived-path fixture failed for {repository_root:?}"
        );
        assert_eq!(
            rust_valid, sql_valid,
            "Rust and SQL derived-path validators diverged for {repository_root:?}"
        );
    }

    let changed_file_fixtures = [
        (Vec::<String>::new(), true),
        (vec!["a/b.rs".to_owned(), "z.rs".to_owned()], true),
        (vec!["a/../b.rs".to_owned()], false),
        (vec!["a//b.rs".to_owned()], false),
        (vec!["/absolute.rs".to_owned()], false),
        (vec!["back\\slash.rs".to_owned()], false),
    ];
    for (changed_files, expected) in changed_file_fixtures {
        let rust_valid = validate_changed_files(&changed_files).is_ok();
        let sql_valid: bool = sqlx::query_scalar("SELECT gobrowse_valid_changed_files($1)")
            .bind(&changed_files)
            .fetch_one(&pool)
            .await
            .expect("evaluate SQL changed-files validator");
        assert_eq!(
            rust_valid, expected,
            "Rust changed-files fixture failed for {changed_files:?}"
        );
        assert_eq!(
            sql_valid, expected,
            "SQL changed-files fixture failed for {changed_files:?}"
        );
        assert_eq!(
            rust_valid, sql_valid,
            "Rust and SQL changed-files validators diverged for {changed_files:?}"
        );
    }

    let fixture = integrity_fixture(&pool).await;
    let safe_id = Uuid::now_v7();
    insert_worktree(
        &pool,
        safe_id,
        fixture.workspace_id,
        fixture.task_id,
        fixture.agent_id,
        "agent/raw-safe",
    )
    .await
    .expect("raw SQL accepts safe worktree");
    assert!(
        sqlx::query("UPDATE worktrees SET branch='bad[branch' WHERE id=$1")
            .bind(safe_id)
            .execute(&pool)
            .await
            .is_err(),
        "raw SQL must not bypass branch validation"
    );
    for (branch, task_id, agent_id) in [
        (
            "agent/cross-task",
            fixture.foreign_task_id,
            fixture.agent_id,
        ),
        (
            "agent/cross-agent",
            fixture.task_id,
            fixture.foreign_agent_id,
        ),
    ] {
        assert!(
            insert_worktree(
                &pool,
                Uuid::now_v7(),
                fixture.workspace_id,
                task_id,
                agent_id,
                branch,
            )
            .await
            .is_err(),
            "raw SQL must reject {branch}"
        );
    }
    assert!(
        sqlx::query("UPDATE worktrees SET base_commit='not-a-full-object-id' WHERE id=$1",)
            .bind(safe_id)
            .execute(&pool)
            .await
            .is_err(),
        "raw SQL must not bypass the base-commit check"
    );
    assert!(
        sqlx::query("UPDATE worktrees SET path='/srv/unsafe/../path' WHERE id=$1")
            .bind(safe_id)
            .execute(&pool)
            .await
            .is_err(),
        "raw SQL must not bypass derived-path validation"
    );
    assert!(
        sqlx::query("UPDATE worktrees SET changed_files=ARRAY['a/../b'] WHERE id=$1",)
            .bind(safe_id)
            .execute(&pool)
            .await
            .is_err(),
        "raw SQL must not bypass changed-file validation"
    );
    assert!(
        sqlx::query("UPDATE worktrees SET status='STALE' WHERE id=$1")
            .bind(safe_id)
            .execute(&pool)
            .await
            .is_err(),
        "raw SQL must not bypass status validation"
    );

    let quarantine_id: i64 = sqlx::query_scalar(
        "INSERT INTO worktree_integrity_quarantine \
         (id,workspace_id,task_id,owner_agent_id,branch,base_commit,path,status,changed_files,last_activity_at,reason) \
         VALUES ($1,$2,$3,$4,'agent/quarantine',$5,$6,'ACTIVE',ARRAY[]::text[],now(),'test') \
         RETURNING quarantine_id",
    )
    .bind(Uuid::now_v7())
    .bind(fixture.workspace_id)
    .bind(fixture.task_id)
    .bind(fixture.agent_id)
    .bind("a".repeat(40))
    .bind(worktree_path(fixture.task_id))
    .fetch_one(&pool)
    .await
    .expect("insert quarantine evidence");
    for statement in [
        "UPDATE worktree_integrity_quarantine SET reason='tampered'",
        "DELETE FROM worktree_integrity_quarantine",
        "TRUNCATE worktree_integrity_quarantine",
    ] {
        assert!(
            sqlx::query(statement).execute(&pool).await.is_err(),
            "quarantine mutation must be rejected: {statement}"
        );
    }
    let (preserved, quarantined_at): (OffsetDateTime, OffsetDateTime) = sqlx::query_as(
        "SELECT last_activity_at,quarantined_at \
         FROM worktree_integrity_quarantine WHERE quarantine_id=$1",
    )
    .bind(quarantine_id)
    .fetch_one(&pool)
    .await
    .expect("quarantine preserves activity and records quarantine timestamps");
    let now = OffsetDateTime::now_utc();
    assert!(preserved <= now);
    assert!(quarantined_at <= now);
}

#[tokio::test]
async fn worktree_router_enforces_tenant_authorization_lifecycle_ledger_and_validation() {
    let Some(database_url) = std::env::var("GOBROWSE_TEST_DATABASE_URL").ok() else {
        eprintln!("GOBROWSE_TEST_DATABASE_URL is unset; skipping router integration test");
        return;
    };
    let _lock = common::acquire_test_lock(&database_url).await;
    let pool = PgPool::connect(&database_url)
        .await
        .expect("connect to test PostgreSQL");
    db::migrate(&pool).await.expect("apply test migrations");

    let fixture = router_fixture(&pool).await;
    let app = router(
        AppState::new(pool.clone(), test_settings(&database_url))
            .await
            .expect("create app state"),
    );
    let collection = format!("/api/v1/workspaces/{}/worktrees", fixture.workspace_id);
    let payload = json!({
        "task_id": fixture.task_id,
        "owner_agent_id": fixture.agent_id,
        "repository_root": "/srv/repositories/project",
        "base_commit": "aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa"
    });
    let (status, created) = request_json(
        &app,
        Method::POST,
        &collection,
        &fixture.owner_cookie,
        Some(payload.clone()),
    )
    .await;
    assert_eq!(status, StatusCode::CREATED, "{created}");
    let worktree_id = uuid_field(&created, "id");
    assert_eq!(
        created["branch"],
        format!(
            "agent/{}-task-title",
            &fixture.task_id.simple().to_string()[..12]
        )
    );
    assert_eq!(
        created["path"],
        format!(
            "/srv/repositories/project/worktrees/task-{}",
            &fixture.task_id.simple().to_string()[..12]
        )
    );

    let (status, list) =
        request_json(&app, Method::GET, &collection, &fixture.viewer_cookie, None).await;
    assert_eq!(status, StatusCode::OK, "{list}");
    assert_eq!(list.as_array().expect("worktree list").len(), 1);
    let (status, fetched) = request_json(
        &app,
        Method::GET,
        &format!("/api/v1/worktrees/{worktree_id}"),
        &fixture.owner_cookie,
        None,
    )
    .await;
    assert_eq!(status, StatusCode::OK, "{fetched}");
    assert_eq!(fetched["id"], created["id"]);
    let before_denied_counts: (i64, i64, i64) = sqlx::query_as(
        "SELECT \
         (SELECT count(*) FROM worktrees WHERE workspace_id=$1), \
         (SELECT count(*) FROM activity_events WHERE workspace_id=$1), \
         (SELECT count(*) FROM audit_events WHERE profile_id=(SELECT profile_id FROM workspaces WHERE id=$1))",
    )
    .bind(fixture.workspace_id)
    .fetch_one(&pool)
    .await
    .expect("count denied mutation side effects");
    let (status, viewer_write) = request_json(
        &app,
        Method::POST,
        &collection,
        &fixture.viewer_cookie,
        Some(payload.clone()),
    )
    .await;
    assert_eq!(status, StatusCode::FORBIDDEN, "{viewer_write}");
    let after_denied_counts: (i64, i64, i64) = sqlx::query_as(
        "SELECT \
         (SELECT count(*) FROM worktrees WHERE workspace_id=$1), \
         (SELECT count(*) FROM activity_events WHERE workspace_id=$1), \
         (SELECT count(*) FROM audit_events WHERE profile_id=(SELECT profile_id FROM workspaces WHERE id=$1))",
    )
    .bind(fixture.workspace_id)
    .fetch_one(&pool)
    .await
    .expect("count denied mutation side effects");
    assert_eq!(
        after_denied_counts, before_denied_counts,
        "viewer denial must not change worktree, ledger, or audit state"
    );
    let (status, foreign_list) = request_json(
        &app,
        Method::GET,
        &collection,
        &fixture.foreign_cookie,
        None,
    )
    .await;
    assert_eq!(status, StatusCode::NOT_FOUND, "{foreign_list}");
    let (status, foreign_get) = request_json(
        &app,
        Method::GET,
        &format!("/api/v1/worktrees/{worktree_id}"),
        &fixture.foreign_cookie,
        None,
    )
    .await;
    assert_eq!(status, StatusCode::NOT_FOUND, "{foreign_get}");
    let before_hidden_counts = after_denied_counts;
    let (status, foreign_write) = request_json(
        &app,
        Method::PATCH,
        &format!("/api/v1/worktrees/{worktree_id}"),
        &fixture.foreign_cookie,
        Some(json!({"changed_files":["must-not-commit.rs"]})),
    )
    .await;
    assert_eq!(status, StatusCode::NOT_FOUND, "{foreign_write}");
    let (status, absent_write) = request_json(
        &app,
        Method::POST,
        &collection,
        &fixture.absent_cookie,
        Some(payload.clone()),
    )
    .await;
    assert_eq!(status, StatusCode::NOT_FOUND, "{absent_write}");
    let nonexistent_id = Uuid::now_v7();
    let (status, nonexistent_write) = request_json(
        &app,
        Method::DELETE,
        &format!("/api/v1/worktrees/{nonexistent_id}"),
        &fixture.owner_cookie,
        None,
    )
    .await;
    assert_eq!(status, StatusCode::NOT_FOUND, "{nonexistent_write}");
    let after_hidden_counts: (i64, i64, i64) = sqlx::query_as(
        "SELECT \
         (SELECT count(*) FROM worktrees WHERE workspace_id=$1), \
         (SELECT count(*) FROM activity_events WHERE workspace_id=$1), \
         (SELECT count(*) FROM audit_events WHERE profile_id=(SELECT profile_id FROM workspaces WHERE id=$1))",
    )
    .bind(fixture.workspace_id)
    .fetch_one(&pool)
    .await
    .expect("count hidden mutation side effects");
    assert_eq!(
        after_hidden_counts, before_hidden_counts,
        "hidden or nonexistent writes must not change worktree, ledger, or audit state"
    );

    let (status, invalid) = request_json(
        &app,
        Method::POST,
        &collection,
        &fixture.owner_cookie,
        Some(json!({
            "task_id": fixture.task_id,
            "owner_agent_id": fixture.agent_id,
            "repository_root": "/srv/./repositories",
            "base_commit": "aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa"
        })),
    )
    .await;
    assert_eq!(status, StatusCode::UNPROCESSABLE_ENTITY, "{invalid}");
    let (status, immutable_input) = request_json(
        &app,
        Method::POST,
        &collection,
        &fixture.owner_cookie,
        Some(json!({
            "task_id": fixture.task_id,
            "owner_agent_id": fixture.agent_id,
            "repository_root": "/srv/repositories/project",
            "base_commit": "aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa",
            "path": "/client-controlled",
            "status": "DELETED"
        })),
    )
    .await;
    assert_eq!(
        status,
        StatusCode::UNPROCESSABLE_ENTITY,
        "{immutable_input}"
    );
    let (status, conflict) = request_json(
        &app,
        Method::POST,
        &collection,
        &fixture.owner_cookie,
        Some(payload),
    )
    .await;
    assert_eq!(status, StatusCode::CONFLICT, "{conflict}");

    let (status, updated) = request_json(
        &app,
        Method::PATCH,
        &format!("/api/v1/worktrees/{worktree_id}"),
        &fixture.owner_cookie,
        Some(json!({"changed_files":["z.rs","a.rs","z.rs"]})),
    )
    .await;
    assert_eq!(status, StatusCode::OK, "{updated}");
    assert_eq!(updated["changed_files"], json!(["a.rs", "z.rs"]));
    assert_eq!(updated["task_id"], created["task_id"]);
    assert_eq!(updated["owner_agent_id"], created["owner_agent_id"]);
    assert_eq!(updated["branch"], created["branch"]);
    assert_eq!(updated["base_commit"], created["base_commit"]);
    assert_eq!(updated["path"], created["path"]);
    let (status, same_files) = request_json(
        &app,
        Method::PATCH,
        &format!("/api/v1/worktrees/{worktree_id}"),
        &fixture.owner_cookie,
        Some(json!({"changed_files":["a.rs","z.rs"]})),
    )
    .await;
    assert_eq!(status, StatusCode::UNPROCESSABLE_ENTITY, "{same_files}");

    for (kind, action) in [
        ("WORKTREE_CREATED", "worktree.created"),
        ("FILES_CHANGED", "worktree.files_changed"),
    ] {
        let actors: (Option<Uuid>, Option<Uuid>) = sqlx::query_as(
            "SELECT actor_user_id,agent_id FROM activity_events \
             WHERE workspace_id=$1 AND kind=$2",
        )
        .bind(fixture.workspace_id)
        .bind(kind)
        .fetch_one(&pool)
        .await
        .expect("read lifecycle actor");
        assert_eq!(actors, (Some(fixture.owner_id), None), "{kind}");
        let audit_count: i64 = sqlx::query_scalar(
            "SELECT count(*) FROM audit_events WHERE actor_user_id=$1 AND action=$2 AND resource_id=$3",
        )
        .bind(fixture.owner_id)
        .bind(action)
        .bind(worktree_id.to_string())
        .fetch_one(&pool)
        .await
        .expect("read lifecycle audit");
        assert_eq!(audit_count, 1, "{action} must append exactly one audit");
    }

    let (status, deleted) = request_json(
        &app,
        Method::DELETE,
        &format!("/api/v1/worktrees/{worktree_id}"),
        &fixture.owner_cookie,
        None,
    )
    .await;
    assert_eq!(status, StatusCode::NO_CONTENT, "{deleted}");
    let row_count: i64 = sqlx::query_scalar("SELECT count(*) FROM worktrees WHERE id=$1")
        .bind(worktree_id)
        .fetch_one(&pool)
        .await
        .expect("worktree was deleted");
    assert_eq!(row_count, 0);
    let delete_events: i64 = sqlx::query_scalar(
        "SELECT count(*) FROM activity_events WHERE workspace_id=$1 AND kind='WORKTREE_DELETED' AND agent_id IS NULL AND actor_user_id=$2",
    )
    .bind(fixture.workspace_id)
    .bind(fixture.owner_id)
    .fetch_one(&pool)
    .await
    .expect("read deletion activity");
    assert_eq!(delete_events, 1);
    let delete_audits: i64 = sqlx::query_scalar(
        "SELECT count(*) FROM audit_events WHERE actor_user_id=$1 AND action='worktree.deleted' AND resource_id=$2",
    )
    .bind(fixture.owner_id)
    .bind(worktree_id.to_string())
    .fetch_one(&pool)
    .await
    .expect("read deletion audit");
    assert_eq!(delete_audits, 1);
}

#[tokio::test]
async fn workspace_editor_can_create_worktree_metadata() {
    let Some(database_url) = std::env::var("GOBROWSE_TEST_DATABASE_URL").ok() else {
        eprintln!("GOBROWSE_TEST_DATABASE_URL is unset; skipping editor router test");
        return;
    };
    let _lock = common::acquire_test_lock(&database_url).await;
    let pool = PgPool::connect(&database_url)
        .await
        .expect("connect to test PostgreSQL");
    db::migrate(&pool).await.expect("apply test migrations");
    let fixture = router_fixture(&pool).await;
    let app = router(
        AppState::new(pool.clone(), test_settings(&database_url))
            .await
            .expect("create app state"),
    );
    let (status, created) = request_json(
        &app,
        Method::POST,
        &format!("/api/v1/workspaces/{}/worktrees", fixture.workspace_id),
        &fixture.editor_cookie,
        Some(json!({
            "task_id": fixture.task_id,
            "owner_agent_id": fixture.agent_id,
            "repository_root": "/srv/editor",
            "branch": "agent/editor-create",
            "base_commit": "aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa"
        })),
    )
    .await;
    assert_eq!(status, StatusCode::CREATED, "{created}");
    let actor: Option<Uuid> = sqlx::query_scalar(
        "SELECT actor_user_id FROM activity_events WHERE workspace_id=$1 AND kind='WORKTREE_CREATED'",
    )
    .bind(fixture.workspace_id)
    .fetch_one(&pool)
    .await
    .expect("read editor activity actor");
    assert_eq!(actor, Some(fixture.editor_id));
}

#[tokio::test]
async fn concurrent_identical_worktree_creates_have_one_winner_and_one_ledger_entry() {
    let Some(database_url) = std::env::var("GOBROWSE_TEST_DATABASE_URL").ok() else {
        eprintln!("GOBROWSE_TEST_DATABASE_URL is unset; skipping race integration test");
        return;
    };
    let _lock = common::acquire_test_lock(&database_url).await;
    let pool = PgPool::connect(&database_url)
        .await
        .expect("connect to test PostgreSQL");
    db::migrate(&pool).await.expect("apply test migrations");
    let fixture = router_fixture(&pool).await;
    let app = router(
        AppState::new(pool.clone(), test_settings(&database_url))
            .await
            .expect("create app state"),
    );
    let uri = format!("/api/v1/workspaces/{}/worktrees", fixture.workspace_id);
    let payload = json!({
        "task_id": fixture.task_id,
        "owner_agent_id": fixture.agent_id,
        "repository_root": "/srv/race",
        "branch": "agent/race",
        "base_commit": "aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa"
    });
    let first = {
        let app = app.clone();
        let cookie = fixture.owner_cookie.clone();
        let uri = uri.clone();
        let payload = payload.clone();
        tokio::spawn(
            async move { request_json(&app, Method::POST, &uri, &cookie, Some(payload)).await },
        )
    };
    let second = {
        let app = app.clone();
        let cookie = fixture.owner_cookie.clone();
        tokio::spawn(
            async move { request_json(&app, Method::POST, &uri, &cookie, Some(payload)).await },
        )
    };
    let statuses = [
        first.await.expect("first create"),
        second.await.expect("second create"),
    ];
    assert_eq!(
        statuses
            .iter()
            .filter(|(status, _)| *status == StatusCode::CREATED)
            .count(),
        1,
        "{statuses:?}"
    );
    assert_eq!(
        statuses
            .iter()
            .filter(|(status, _)| *status == StatusCode::CONFLICT)
            .count(),
        1,
        "{statuses:?}"
    );
    let rows: i64 = sqlx::query_scalar(
        "SELECT count(*) FROM worktrees WHERE workspace_id=$1 AND branch='agent/race'",
    )
    .bind(fixture.workspace_id)
    .fetch_one(&pool)
    .await
    .expect("count race worktrees");
    assert_eq!(rows, 1);
    let events: i64 = sqlx::query_scalar(
        "SELECT count(*) FROM activity_events WHERE workspace_id=$1 AND kind='WORKTREE_CREATED'",
    )
    .bind(fixture.workspace_id)
    .fetch_one(&pool)
    .await
    .expect("count race activities");
    assert_eq!(events, 1);
    let audits: i64 = sqlx::query_scalar(
        "SELECT count(*) FROM audit_events WHERE actor_user_id=$1 AND action='worktree.created'",
    )
    .bind(fixture.owner_id)
    .fetch_one(&pool)
    .await
    .expect("count race audits");
    assert_eq!(audits, 1);
}

#[tokio::test]
async fn revoked_editor_cannot_commit_worktree_create_update_or_delete() {
    let Some(database_url) = std::env::var("GOBROWSE_TEST_DATABASE_URL").ok() else {
        eprintln!("GOBROWSE_TEST_DATABASE_URL is unset; skipping revocation race test");
        return;
    };
    let _lock = common::acquire_test_lock(&database_url).await;
    let pool = PgPool::connect(&database_url)
        .await
        .expect("connect to test PostgreSQL");
    db::migrate(&pool).await.expect("apply test migrations");
    let fixture = router_fixture(&pool).await;
    let existing_id = Uuid::now_v7();
    insert_worktree(
        &pool,
        existing_id,
        fixture.workspace_id,
        fixture.task_id,
        fixture.agent_id,
        "agent/revocation-existing",
    )
    .await
    .expect("seed mutable worktree");
    let baseline_audits: i64 =
        sqlx::query_scalar("SELECT count(*) FROM audit_events WHERE actor_user_id=$1")
            .bind(fixture.editor_id)
            .fetch_one(&pool)
            .await
            .expect("count revoked editor audit baseline");
    let app = router(
        AppState::new(pool.clone(), test_settings(&database_url))
            .await
            .expect("create app state"),
    );
    let collection = format!("/api/v1/workspaces/{}/worktrees", fixture.workspace_id);
    let (status, _) = revoke_editor_and_request(
        &pool,
        &app,
        &fixture,
        Method::POST,
        collection,
        Some(json!({
            "task_id": fixture.task_id,
            "owner_agent_id": fixture.agent_id,
            "repository_root": "/srv/revocation",
            "branch": "agent/revocation-create",
            "base_commit": "aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa"
        })),
    )
    .await;
    assert_eq!(
        status,
        StatusCode::NOT_FOUND,
        "revoked editor create must be hidden"
    );
    let created_rows: i64 = sqlx::query_scalar(
        "SELECT count(*) FROM worktrees WHERE workspace_id=$1 AND branch='agent/revocation-create'",
    )
    .bind(fixture.workspace_id)
    .fetch_one(&pool)
    .await
    .expect("check revoked create");
    assert_eq!(created_rows, 0);
    restore_editor(&pool, &fixture).await;

    let (status, _) = revoke_editor_and_request(
        &pool,
        &app,
        &fixture,
        Method::PATCH,
        format!("/api/v1/worktrees/{existing_id}"),
        Some(json!({"changed_files":["must-not-commit.rs"]})),
    )
    .await;
    assert_eq!(
        status,
        StatusCode::NOT_FOUND,
        "revoked editor update must be hidden"
    );
    let changed_files: Vec<String> =
        sqlx::query_scalar("SELECT changed_files FROM worktrees WHERE id=$1")
            .bind(existing_id)
            .fetch_one(&pool)
            .await
            .expect("read revoked update target");
    assert!(changed_files.is_empty());
    restore_editor(&pool, &fixture).await;

    let (status, _) = revoke_editor_and_request(
        &pool,
        &app,
        &fixture,
        Method::DELETE,
        format!("/api/v1/worktrees/{existing_id}"),
        None,
    )
    .await;
    assert_eq!(
        status,
        StatusCode::NOT_FOUND,
        "revoked editor delete must be hidden"
    );
    let remaining_rows: i64 = sqlx::query_scalar("SELECT count(*) FROM worktrees WHERE id=$1")
        .bind(existing_id)
        .fetch_one(&pool)
        .await
        .expect("check revoked delete");
    assert_eq!(remaining_rows, 1);
    let evidence: i64 = sqlx::query_scalar(
        "SELECT count(*) FROM activity_events WHERE workspace_id=$1 AND kind IN ('WORKTREE_CREATED','FILES_CHANGED','WORKTREE_DELETED')",
    )
    .bind(fixture.workspace_id)
    .fetch_one(&pool)
    .await
    .expect("check revoked mutation ledger");
    let final_audits: i64 =
        sqlx::query_scalar("SELECT count(*) FROM audit_events WHERE actor_user_id=$1")
            .bind(fixture.editor_id)
            .fetch_one(&pool)
            .await
            .expect("count revoked editor audit evidence");
    assert_eq!(
        final_audits, baseline_audits,
        "revoked writes must not append audit evidence"
    );
    assert_eq!(evidence, 0, "revoked writes must not append activity");
}

struct IntegrityFixture {
    workspace_id: Uuid,
    task_id: Uuid,
    agent_id: Uuid,
    foreign_task_id: Uuid,
    foreign_agent_id: Uuid,
}

async fn integrity_fixture(pool: &PgPool) -> IntegrityFixture {
    let profile_id = Uuid::now_v7();
    let owner_id = Uuid::now_v7();
    let workspace_id = Uuid::now_v7();
    let foreign_workspace_id = Uuid::now_v7();
    sqlx::query("INSERT INTO profiles (id,name) VALUES ($1,'worktree integrity profile')")
        .bind(profile_id)
        .execute(pool)
        .await
        .expect("create integrity profile");
    sqlx::query(
        "INSERT INTO users (id,email,display_name,password_hash,role,primary_profile_id) \
         VALUES ($1,$2,'Integrity Owner','unused','OWNER',$3)",
    )
    .bind(owner_id)
    .bind(format!("{owner_id}@example.test"))
    .bind(profile_id)
    .execute(pool)
    .await
    .expect("create integrity owner");
    sqlx::query(
        "INSERT INTO workspaces (id,profile_id,title,created_by_user_id) \
         VALUES ($1,$2,'Integrity',$3),($4,$2,'Foreign integrity',$3)",
    )
    .bind(workspace_id)
    .bind(profile_id)
    .bind(owner_id)
    .bind(foreign_workspace_id)
    .execute(pool)
    .await
    .expect("create integrity workspaces");
    let task_id = Uuid::now_v7();
    let foreign_task_id = Uuid::now_v7();
    let agent_id = Uuid::now_v7();
    let foreign_agent_id = Uuid::now_v7();
    for (task, workspace) in [
        (task_id, workspace_id),
        (foreign_task_id, foreign_workspace_id),
    ] {
        sqlx::query(
            "INSERT INTO tasks (id,workspace_id,title,state) VALUES ($1,$2,'Worktree task','BACKLOG')",
        )
        .bind(task)
        .bind(workspace)
        .execute(pool)
        .await
        .expect("create worktree task");
    }
    for (agent, workspace) in [
        (agent_id, workspace_id),
        (foreign_agent_id, foreign_workspace_id),
    ] {
        sqlx::query(
            "INSERT INTO agents (id,workspace_id,name,kind,permissions,status) \
             VALUES ($1,$2,'Worktree agent','coding','{}','paused')",
        )
        .bind(agent)
        .bind(workspace)
        .execute(pool)
        .await
        .expect("create worktree agent");
    }
    IntegrityFixture {
        workspace_id,
        task_id,
        agent_id,
        foreign_task_id,
        foreign_agent_id,
    }
}

async fn insert_worktree(
    pool: &PgPool,
    id: Uuid,
    workspace_id: Uuid,
    task_id: Uuid,
    agent_id: Uuid,
    branch: &str,
) -> Result<(), sqlx::Error> {
    sqlx::query(
        "INSERT INTO worktrees \
         (id,workspace_id,task_id,owner_agent_id,branch,base_commit,path,status,changed_files) \
         VALUES ($1,$2,$3,$4,$5,$6,$7,'ACTIVE',ARRAY[]::text[])",
    )
    .bind(id)
    .bind(workspace_id)
    .bind(task_id)
    .bind(agent_id)
    .bind(branch)
    .bind("a".repeat(40))
    .bind(worktree_path(task_id))
    .execute(pool)
    .await
    .map(|_| ())
}

struct RouterFixture {
    workspace_id: Uuid,
    task_id: Uuid,
    agent_id: Uuid,
    owner_id: Uuid,
    owner_cookie: String,
    editor_id: Uuid,
    editor_cookie: String,
    viewer_cookie: String,
    foreign_cookie: String,
    absent_cookie: String,
}

async fn router_fixture(pool: &PgPool) -> RouterFixture {
    let profile_id = Uuid::now_v7();
    sqlx::query("INSERT INTO profiles (id,name) VALUES ($1,'worktree router profile')")
        .bind(profile_id)
        .execute(pool)
        .await
        .expect("create router profile");
    let (owner_id, owner_cookie) = create_session(pool, profile_id, "OWNER").await;
    let (editor_id, editor_cookie) = create_session(pool, profile_id, "MEMBER").await;
    let (viewer_id, viewer_cookie) = create_session(pool, profile_id, "MEMBER").await;
    let (_absent_id, absent_cookie) = create_session(pool, profile_id, "MEMBER").await;
    let (_foreign_id, foreign_cookie) = create_foreign_session(pool).await;
    let workspace_id = Uuid::now_v7();
    sqlx::query(
        "INSERT INTO workspaces (id,profile_id,title,created_by_user_id) VALUES ($1,$2,'Worktrees',$3)",
    )
    .bind(workspace_id)
    .bind(profile_id)
    .bind(owner_id)
    .execute(pool)
    .await
    .expect("create router workspace");
    sqlx::query(
        "INSERT INTO workspace_memberships (workspace_id,user_id,access) \
         VALUES ($1,$2,'OWNER'),($1,$3,'EDITOR'),($1,$4,'VIEWER')",
    )
    .bind(workspace_id)
    .bind(owner_id)
    .bind(editor_id)
    .bind(viewer_id)
    .execute(pool)
    .await
    .expect("create router memberships");
    let task_id = Uuid::now_v7();
    let agent_id = Uuid::now_v7();
    sqlx::query(
        "INSERT INTO tasks (id,workspace_id,title,state) VALUES ($1,$2,'Task title','BACKLOG')",
    )
    .bind(task_id)
    .bind(workspace_id)
    .execute(pool)
    .await
    .expect("create router task");
    sqlx::query(
        "INSERT INTO agents (id,workspace_id,name,kind,permissions,status) \
         VALUES ($1,$2,'Worktree owner','coding','{}','paused')",
    )
    .bind(agent_id)
    .bind(workspace_id)
    .execute(pool)
    .await
    .expect("create router agent");
    RouterFixture {
        workspace_id,
        task_id,
        agent_id,
        owner_id,
        owner_cookie,
        editor_id,
        editor_cookie,
        viewer_cookie,
        foreign_cookie,
        absent_cookie,
    }
}

async fn revoke_editor_and_request(
    pool: &PgPool,
    app: &Router,
    fixture: &RouterFixture,
    method: Method,
    uri: String,
    body: Option<Value>,
) -> (StatusCode, Value) {
    let mut revoke = pool.begin().await.expect("begin membership revocation");
    sqlx::query(
        "SELECT access FROM workspace_memberships WHERE workspace_id=$1 AND user_id=$2 FOR UPDATE",
    )
    .bind(fixture.workspace_id)
    .bind(fixture.editor_id)
    .fetch_one(&mut *revoke)
    .await
    .expect("lock editor membership");
    let app = app.clone();
    let cookie = fixture.editor_cookie.clone();
    let request =
        tokio::spawn(async move { request_json(&app, method, &uri, &cookie, body).await });
    tokio::task::yield_now().await;
    sqlx::query("DELETE FROM workspace_memberships WHERE workspace_id=$1 AND user_id=$2")
        .bind(fixture.workspace_id)
        .bind(fixture.editor_id)
        .execute(&mut *revoke)
        .await
        .expect("revoke editor membership");
    revoke.commit().await.expect("commit membership revocation");
    request.await.expect("join revoked editor request")
}

async fn restore_editor(pool: &PgPool, fixture: &RouterFixture) {
    sqlx::query(
        "INSERT INTO workspace_memberships (workspace_id,user_id,access) VALUES ($1,$2,'EDITOR')",
    )
    .bind(fixture.workspace_id)
    .bind(fixture.editor_id)
    .execute(pool)
    .await
    .expect("restore editor membership");
}

async fn create_session(pool: &PgPool, profile_id: Uuid, role: &str) -> (Uuid, String) {
    let user_id = Uuid::now_v7();
    let token = format!("worktree-session-{user_id}");
    sqlx::query(
        "INSERT INTO users (id,email,display_name,password_hash,role,primary_profile_id) \
         VALUES ($1,$2,'Worktree User','unused',$3,$4)",
    )
    .bind(user_id)
    .bind(format!("{user_id}@example.test"))
    .bind(role)
    .bind(profile_id)
    .execute(pool)
    .await
    .expect("create worktree user");
    let now = OffsetDateTime::now_utc();
    sqlx::query(
        "INSERT INTO sessions (token_hash,user_id,auth_epoch,expires_at,absolute_expires_at) \
         VALUES ($1,$2,1,$3,$4)",
    )
    .bind(Sha256::digest(token.as_bytes()).to_vec())
    .bind(user_id)
    .bind(now + Duration::hours(1))
    .bind(now + Duration::hours(2))
    .execute(pool)
    .await
    .expect("create worktree session");
    (user_id, format!("gobrowse_session={token}"))
}

async fn create_foreign_session(pool: &PgPool) -> (Uuid, String) {
    let profile_id = Uuid::now_v7();
    sqlx::query("INSERT INTO profiles (id,name) VALUES ($1,'foreign worktree profile')")
        .bind(profile_id)
        .execute(pool)
        .await
        .expect("create foreign profile");
    create_session(pool, profile_id, "OWNER").await
}

fn worktree_path(task_id: Uuid) -> String {
    format!(
        "/srv/raw/worktrees/task-{}",
        &task_id.simple().to_string()[..12]
    )
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
    let body = if let Some(body) = body {
        builder = builder.header(header::CONTENT_TYPE, "application/json");
        Body::from(body.to_string())
    } else {
        Body::empty()
    };
    let response = app
        .clone()
        .oneshot(builder.body(body).expect("build request"))
        .await
        .expect("route request");
    let status = response.status();
    let body = response
        .into_body()
        .collect()
        .await
        .expect("collect response")
        .to_bytes();
    let value = if status.is_success() && !body.is_empty() {
        serde_json::from_slice(&body).expect("JSON success response")
    } else {
        Value::Null
    };
    (status, value)
}

fn uuid_field(value: &Value, field: &str) -> Uuid {
    Uuid::parse_str(value[field].as_str().expect("UUID string")).expect("valid UUID")
}
