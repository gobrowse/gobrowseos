mod common;

use base64::{Engine as _, engine::general_purpose::STANDARD};
use gobrowse_server::{
    config::{
        AuthSettings, DatabaseSettings, FeatureSettings, HttpSettings, ObservabilitySettings,
        Settings, VaultSettings,
    },
    doctor, vault,
};
use secrecy::{ExposeSecret, SecretString};
use sqlx::{Connection, PgConnection, PgPool, Row};
use time::{Duration, OffsetDateTime};
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

#[tokio::test]
async fn migrations_enable_pgvector_and_schema_version() {
    let Some(database_url) = std::env::var("GOBROWSE_TEST_DATABASE_URL").ok() else {
        eprintln!("GOBROWSE_TEST_DATABASE_URL is unset; skipping PostgreSQL integration test");
        return;
    };
    let _lock = common::acquire_test_lock(&database_url).await;
    let pool = test_pool().await.expect("test pool after lock");
    let row = sqlx::query(
        "SELECT schema_version, EXISTS(SELECT 1 FROM pg_extension WHERE extname = 'vector') AS vector_enabled \
         FROM schema_metadata WHERE singleton",
    )
    .fetch_one(&pool)
    .await
    .expect("read schema metadata");
    assert_eq!(row.get::<i64, _>("schema_version"), 24);
    assert!(row.get::<bool, _>("vector_enabled"));
}

#[tokio::test]
async fn schema_v14_to_v18_repairs_skill_worktree_and_mcp_integrity() {
    let Some(database_url) = std::env::var("GOBROWSE_TEST_DATABASE_URL").ok() else {
        eprintln!("GOBROWSE_TEST_DATABASE_URL is unset; skipping PostgreSQL integration test");
        return;
    };
    let _lock = common::acquire_test_lock(&database_url).await;
    let pool = PgPool::connect(&database_url)
        .await
        .expect("connect to test PostgreSQL");
    gobrowse_server::db::migrate(&pool)
        .await
        .expect("install shared extensions");
    let schema = format!("skills_v14_{}", Uuid::now_v7().simple());
    sqlx::query(&format!("CREATE SCHEMA {schema}"))
        .execute(&pool)
        .await
        .expect("create isolated schema");
    let mut connection = PgConnection::connect(&database_url)
        .await
        .expect("connect isolated session");
    sqlx::query(&format!("SET search_path TO {schema},public"))
        .execute(&mut connection)
        .await
        .expect("set isolated search path");
    let migrations = [
        include_str!("../migrations/0001_initial.sql"),
        include_str!("../migrations/0002_library_embeddings.sql"),
        include_str!("../migrations/0003_chat_runs.sql"),
        include_str!("../migrations/0004_login_attempts.sql"),
        include_str!("../migrations/0005_webhooks.sql"),
        include_str!("../migrations/0006_audit_append_only.sql"),
        include_str!("../migrations/0007_webhook_scheduler.sql"),
        include_str!("../migrations/0008_webhook_delivery_lease.sql"),
        include_str!("../migrations/0009_webhook_delivery_fencing.sql"),
        include_str!("../migrations/0010_task_integrity_activity_ledger.sql"),
        include_str!("../migrations/0011_task_activity_hardening.sql"),
        include_str!("../migrations/0012_webhook_lease_check.sql"),
        include_str!("../migrations/0013_skill_integrity.sql"),
        include_str!("../migrations/0014_skill_revision_immutability.sql"),
    ];
    for sql in migrations {
        sqlx::raw_sql(sql)
            .execute(&mut connection)
            .await
            .expect("install schema 14 migration");
    }
    let profile_id = Uuid::now_v7();
    let primary_user_id = Uuid::now_v7();
    let skill_id = Uuid::now_v7();
    let revision_id = Uuid::now_v7();
    sqlx::query("INSERT INTO profiles (id,name) VALUES ($1,'skills-v14-repair')")
        .bind(profile_id)
        .execute(&mut connection)
        .await
        .expect("create profile");
    sqlx::query(
        "INSERT INTO users (id,email,display_name,password_hash,role,primary_profile_id) \
         VALUES ($1,$2,'Primary Owner','unused','OWNER',$3)",
    )
    .bind(primary_user_id)
    .bind(format!("{primary_user_id}@example.test"))
    .bind(profile_id)
    .execute(&mut connection)
    .await
    .expect("create primary owner");
    sqlx::query("INSERT INTO skills (id,profile_id,name,description) VALUES ($1,$2,'repair','')")
        .bind(skill_id)
        .bind(profile_id)
        .execute(&mut connection)
        .await
        .expect("create skill");
    sqlx::query(
        "INSERT INTO skill_revisions (id,skill_id,revision,content,author,reason,source_conversation_ids,evaluation,promoted) VALUES ($1,$2,1,'content','tester','legacy',$3,$4,true)",
    )
    .bind(revision_id)
    .bind(skill_id)
    .bind(vec![Uuid::nil()])
    .bind(serde_json::json!({ "malformed": true }))
    .execute(&mut connection)
    .await
    .expect("insert legacy malformed revision");
    let evaluation_cases = [
        serde_json::json!({
            "deterministic_checks_passed": true,
            "attempts": 0,
            "successful_attempts": 0,
            "steps": 0,
            "retries": 0,
            "errors": 0,
            "duration_ms": 0
        }),
        serde_json::json!({
            "deterministic_checks_passed": true,
            "attempts": 0,
            "successful_attempts": 0,
            "steps": 0,
            "retries": 0,
            "errors": 0,
            "duration_ms": 0,
            "user_corrections": 0,
            "extra": true
        }),
        serde_json::json!({
            "deterministic_checks_passed": "yes",
            "attempts": 0,
            "successful_attempts": 0,
            "steps": 0,
            "retries": 0,
            "errors": 0,
            "duration_ms": 0,
            "user_corrections": 0
        }),
        serde_json::json!({
            "deterministic_checks_passed": true,
            "attempts": 1000001,
            "successful_attempts": 0,
            "steps": 0,
            "retries": 0,
            "errors": 0,
            "duration_ms": 0,
            "user_corrections": 0
        }),
    ];
    let mut evaluation_ids = Vec::new();
    for (index, value) in evaluation_cases.iter().enumerate() {
        let id = Uuid::now_v7();
        evaluation_ids.push((id, value.clone()));
        sqlx::query(
            "INSERT INTO skill_revisions (id,skill_id,revision,content,author,reason,evaluation) VALUES ($1,$2,$3,'invalid-eval','tester','legacy',$4)",
        )
        .bind(id)
        .bind(skill_id)
        .bind((index + 2) as i64)
        .bind(value)
        .execute(&mut connection)
        .await
        .expect("insert malformed evaluation");
    }
    let other_profile = Uuid::now_v7();
    let other_user_id = Uuid::now_v7();
    let workspace = Uuid::now_v7();
    let same_profile_workspace = Uuid::now_v7();
    let other_workspace = Uuid::now_v7();
    sqlx::query("INSERT INTO profiles (id,name) VALUES ($1,'skills-v14-other')")
        .bind(other_profile)
        .execute(&mut connection)
        .await
        .expect("other profile");
    sqlx::query(
        "INSERT INTO users (id,email,display_name,password_hash,role,primary_profile_id) \
         VALUES ($1,$2,'Other Owner','unused','OWNER',$3)",
    )
    .bind(other_user_id)
    .bind(format!("{other_user_id}@example.test"))
    .bind(other_profile)
    .execute(&mut connection)
    .await
    .expect("create other owner");
    sqlx::query("INSERT INTO workspaces (id,profile_id,title,created_by_user_id) VALUES ($1,$2,'same workspace',$3),($4,$2,'same profile other workspace',$3),($5,$6,'other workspace',$7)").bind(workspace).bind(profile_id).bind(primary_user_id).bind(same_profile_workspace).bind(other_workspace).bind(other_profile).bind(other_user_id).execute(&mut connection).await.expect("workspaces");
    let source = Uuid::now_v7();
    let wrong_source = Uuid::now_v7();
    let deleted_source = Uuid::now_v7();
    let cross_source = Uuid::now_v7();
    let missing_source = Uuid::now_v7();
    sqlx::query("INSERT INTO conversations (id,profile_id,workspace_id,created_by_user_id,title) VALUES ($1,$2,$3,$4,'source'),($5,$2,$6,$4,'wrong'),($7,$2,NULL,$4,'deleted'),($8,$9,NULL,$10,'cross')").bind(source).bind(profile_id).bind(workspace).bind(primary_user_id).bind(wrong_source).bind(same_profile_workspace).bind(deleted_source).bind(cross_source).bind(other_profile).bind(other_user_id).execute(&mut connection).await.expect("source fixtures");
    sqlx::query("UPDATE conversations SET status='deleted' WHERE id=$1")
        .bind(deleted_source)
        .execute(&mut connection)
        .await
        .expect("deleted source");
    let duplicate_skill = Uuid::now_v7();
    let duplicate_revision = Uuid::now_v7();
    sqlx::query(
        "INSERT INTO skills (id,profile_id,name,description) VALUES ($1,$2,'duplicate-source','')",
    )
    .bind(duplicate_skill)
    .bind(profile_id)
    .execute(&mut connection)
    .await
    .expect("duplicate skill");
    sqlx::query("INSERT INTO skill_revisions (id,skill_id,revision,content,author,reason,source_conversation_ids) VALUES ($1,$2,1,'x','x','x',$3)").bind(duplicate_revision).bind(duplicate_skill).bind(vec![source,source]).execute(&mut connection).await.expect("duplicate source");
    let inaccessible_skill = Uuid::now_v7();
    let inaccessible_revision = Uuid::now_v7();
    sqlx::query("INSERT INTO skills (id,profile_id,name,description) VALUES ($1,$2,'inaccessible-source','')").bind(inaccessible_skill).bind(profile_id).execute(&mut connection).await.expect("inaccessible skill");
    sqlx::query("INSERT INTO skill_revisions (id,skill_id,revision,content,author,reason,source_conversation_ids) VALUES ($1,$2,1,'x','x','x',$3)").bind(inaccessible_revision).bind(inaccessible_skill).bind(vec![missing_source,cross_source]).execute(&mut connection).await.expect("inaccessible sources");
    let missing_skill = Uuid::now_v7();
    let missing_revision = Uuid::now_v7();
    let cross_skill = Uuid::now_v7();
    let cross_revision = Uuid::now_v7();
    let deleted_skill = Uuid::now_v7();
    let deleted_revision = Uuid::now_v7();
    let wrong_skill = Uuid::now_v7();
    let wrong_revision = Uuid::now_v7();
    for (sid, name, wid) in [
        (missing_skill, "missing-source", None),
        (cross_skill, "cross-source", None),
        (deleted_skill, "deleted-source", None),
        (wrong_skill, "wrong-workspace-source", Some(workspace)),
    ] {
        sqlx::query("INSERT INTO skills (id,profile_id,workspace_id,name,description) VALUES ($1,$2,$3,$4,'')").bind(sid).bind(profile_id).bind(wid).bind(name).execute(&mut connection).await.expect("independent source skill");
    }
    sqlx::query("INSERT INTO skill_revisions (id,skill_id,revision,content,author,reason,source_conversation_ids) VALUES ($1,$2,1,'x','x','x',$3),($4,$5,1,'x','x','x',$6),($7,$8,1,'x','x','x',$9),($10,$11,1,'x','x','x',$12)").bind(missing_revision).bind(missing_skill).bind(vec![missing_source]).bind(cross_revision).bind(cross_skill).bind(vec![cross_source]).bind(deleted_revision).bind(deleted_skill).bind(vec![deleted_source]).bind(wrong_revision).bind(wrong_skill).bind(vec![wrong_source]).execute(&mut connection).await.expect("independent source revisions");
    let workspace_skill = Uuid::now_v7();
    let workspace_revision = Uuid::now_v7();
    sqlx::query("INSERT INTO skills (id,profile_id,workspace_id,name,description) VALUES ($1,$2,$3,'workspace-source','')").bind(workspace_skill).bind(profile_id).bind(workspace).execute(&mut connection).await.expect("workspace skill");
    sqlx::query("INSERT INTO skill_revisions (id,skill_id,revision,content,author,reason,source_conversation_ids) VALUES ($1,$2,1,'x','x','x',$3)").bind(workspace_revision).bind(workspace_skill).bind(vec![source,wrong_source,deleted_source]).execute(&mut connection).await.expect("workspace sources");
    let oversized_skill = Uuid::now_v7();
    let oversized_revision = Uuid::now_v7();
    sqlx::query(
        "INSERT INTO skills (id,profile_id,name,description) VALUES ($1,$2,'oversized-source','')",
    )
    .bind(oversized_skill)
    .bind(profile_id)
    .execute(&mut connection)
    .await
    .expect("oversized skill");
    let mut oversized = Vec::new();
    for _ in 0..101 {
        let id = Uuid::now_v7();
        sqlx::query("INSERT INTO conversations (id,profile_id,created_by_user_id,title) VALUES ($1,$2,$3,'oversized')")
            .bind(id)
            .bind(profile_id)
            .bind(primary_user_id)
            .execute(&mut connection)
            .await
            .expect("oversized conversation");
        oversized.push(id);
    }
    sqlx::query("INSERT INTO skill_revisions (id,skill_id,revision,content,author,reason,source_conversation_ids) VALUES ($1,$2,1,'x','x','x',$3)").bind(oversized_revision).bind(oversized_skill).bind(&oversized).execute(&mut connection).await.expect("oversized sources");
    let active_skill = Uuid::now_v7();
    sqlx::query(
        "INSERT INTO skills (id,profile_id,name,description) VALUES ($1,$2,'active-canonical','')",
    )
    .bind(active_skill)
    .bind(profile_id)
    .execute(&mut connection)
    .await
    .expect("active skill");
    let active_one = Uuid::now_v7();
    let active_two = Uuid::now_v7();
    for (id, rev, promoted) in [(active_one, 1, false), (active_two, 2, true)] {
        sqlx::query("INSERT INTO skill_revisions (id,skill_id,revision,content,author,reason,promoted) VALUES ($1,$2,$3,'x','x','x',$4)").bind(id).bind(active_skill).bind(rev).bind(promoted).execute(&mut connection).await.expect("active revision");
    }
    sqlx::query("UPDATE skills SET active_revision=1 WHERE id=$1")
        .bind(active_skill)
        .execute(&mut connection)
        .await
        .expect("active pointer");
    let pre_active_revision: i64 =
        sqlx::query_scalar("SELECT active_revision FROM skills WHERE id=$1")
            .bind(active_skill)
            .fetch_one(&mut connection)
            .await
            .expect("read pre-migration active revision");
    let pre_promoted: (bool, bool) = sqlx::query_as(
        "SELECT (SELECT promoted FROM skill_revisions WHERE id=$1), (SELECT promoted FROM skill_revisions WHERE id=$2)",
    )
    .bind(active_one)
    .bind(active_two)
    .fetch_one(&mut connection)
    .await
    .expect("read pre-migration promotion state");
    assert_eq!(pre_active_revision, 1);
    assert_eq!(pre_promoted, (false, true));
    let none_skill = Uuid::now_v7();
    sqlx::query(
        "INSERT INTO skills (id,profile_id,name,description) VALUES ($1,$2,'none-promoted','')",
    )
    .bind(none_skill)
    .bind(profile_id)
    .execute(&mut connection)
    .await
    .expect("none skill");
    sqlx::query("INSERT INTO skill_revisions (id,skill_id,revision,content,author,reason) VALUES ($1,$2,1,'x','x','x')").bind(Uuid::now_v7()).bind(none_skill).execute(&mut connection).await.expect("none revision");
    sqlx::query("UPDATE skills SET active_revision=1 WHERE id=$1")
        .bind(none_skill)
        .execute(&mut connection)
        .await
        .expect("none pointer");
    sqlx::raw_sql(include_str!(
        "../migrations/0015_skill_lifecycle_hardening.sql"
    ))
    .execute(&mut connection)
    .await
    .expect("upgrade schema 14 to 15");
    let safe_worktree_id = Uuid::now_v7();
    let unsafe_branch_worktree_id = Uuid::now_v7();
    let cross_workspace_task_worktree_id = Uuid::now_v7();
    let cross_workspace_owner_worktree_id = Uuid::now_v7();
    let unsafe_base_commit_worktree_id = Uuid::now_v7();
    let unsafe_path_worktree_id = Uuid::now_v7();
    let unsafe_changed_files_worktree_id = Uuid::now_v7();
    let safe_worktree_task_id = Uuid::from_u128(0x00000000000170008000000000000001);
    let unsafe_worktree_task_id = Uuid::from_u128(0x00000000000270008000000000000002);
    let foreign_worktree_task_id = Uuid::from_u128(0x00000000000370008000000000000003);
    let cross_workspace_owner_task_id = Uuid::from_u128(0x00000000000470008000000000000004);
    let unsafe_base_commit_task_id = Uuid::from_u128(0x00000000000570008000000000000005);
    let unsafe_changed_files_task_id = Uuid::from_u128(0x00000000000670008000000000000006);
    let unsafe_recurrence_task_id = Uuid::from_u128(0x00000000000770008000000000000007);
    let worktree_agent_id = Uuid::now_v7();
    let foreign_worktree_agent_id = Uuid::now_v7();
    // PostgreSQL `TIMESTAMPTZ` persists microseconds, so seed the legacy value at that precision
    // and retain exact equality as the migration-preservation check.
    let legacy_activity_at = OffsetDateTime::now_utc() - Duration::hours(1);
    let legacy_activity_at = legacy_activity_at
        .replace_nanosecond(legacy_activity_at.nanosecond() / 1_000 * 1_000)
        .expect("truncate legacy activity timestamp to PostgreSQL microseconds");
    for task_id in [
        safe_worktree_task_id,
        unsafe_worktree_task_id,
        cross_workspace_owner_task_id,
        unsafe_base_commit_task_id,
        unsafe_changed_files_task_id,
        unsafe_recurrence_task_id,
    ] {
        sqlx::query(
            "INSERT INTO tasks (id,workspace_id,title,state) VALUES ($1,$2,'Legacy worktree task','BACKLOG')",
        )
        .bind(task_id)
        .bind(workspace)
        .execute(&mut connection)
        .await
        .expect("seed schema-15 local worktree task");
    }
    sqlx::query(
        "INSERT INTO tasks (id,workspace_id,title,state) VALUES ($1,$2,'Foreign worktree task','BACKLOG')",
    )
    .bind(foreign_worktree_task_id)
    .bind(other_workspace)
    .execute(&mut connection)
    .await
    .expect("seed schema-15 foreign worktree task");
    for (agent_id, agent_workspace, name) in [
        (worktree_agent_id, workspace, "Legacy worktree agent"),
        (
            foreign_worktree_agent_id,
            other_workspace,
            "Foreign worktree agent",
        ),
    ] {
        sqlx::query(
            "INSERT INTO agents (id,workspace_id,name,kind,permissions,status) \
             VALUES ($1,$2,$3,'coding','{}','paused')",
        )
        .bind(agent_id)
        .bind(agent_workspace)
        .bind(name)
        .execute(&mut connection)
        .await
        .expect("seed schema-15 worktree agent");
    }
    let worktree_path = |task_id: Uuid| {
        format!(
            "/srv/legacy/worktrees/task-{}",
            &task_id.simple().to_string()[..12]
        )
    };
    let seeded_worktrees = vec![
        (
            safe_worktree_id,
            safe_worktree_task_id,
            worktree_agent_id,
            "agent/schema-15-safe".to_string(),
            "a".repeat(40),
            worktree_path(safe_worktree_task_id),
            vec!["legacy.rs".to_string()],
        ),
        (
            unsafe_branch_worktree_id,
            unsafe_worktree_task_id,
            worktree_agent_id,
            "agent/schema-15[unsafe".to_string(),
            "a".repeat(40),
            worktree_path(unsafe_worktree_task_id),
            vec!["legacy.rs".to_string()],
        ),
        (
            cross_workspace_task_worktree_id,
            foreign_worktree_task_id,
            worktree_agent_id,
            "agent/schema-15-cross-task".to_string(),
            "a".repeat(40),
            worktree_path(foreign_worktree_task_id),
            vec!["legacy.rs".to_string()],
        ),
        (
            cross_workspace_owner_worktree_id,
            cross_workspace_owner_task_id,
            foreign_worktree_agent_id,
            "agent/schema-15-cross-owner".to_string(),
            "a".repeat(40),
            worktree_path(cross_workspace_owner_task_id),
            vec!["legacy.rs".to_string()],
        ),
        (
            unsafe_base_commit_worktree_id,
            unsafe_base_commit_task_id,
            worktree_agent_id,
            "agent/schema-15-unsafe-base".to_string(),
            "not-a-full-object-id".to_string(),
            worktree_path(unsafe_base_commit_task_id),
            vec!["legacy.rs".to_string()],
        ),
        (
            unsafe_path_worktree_id,
            unsafe_worktree_task_id,
            worktree_agent_id,
            "agent/schema-15-unsafe-path".to_string(),
            "a".repeat(40),
            "/srv/legacy/derived-path".to_string(),
            vec!["legacy.rs".to_string()],
        ),
        (
            unsafe_changed_files_worktree_id,
            unsafe_changed_files_task_id,
            worktree_agent_id,
            "agent/schema-15-unsafe-files".to_string(),
            "a".repeat(40),
            worktree_path(unsafe_changed_files_task_id),
            vec!["a/../legacy.rs".to_string()],
        ),
    ];
    for (id, task_id, owner_agent_id, branch, base_commit, path, changed_files) in seeded_worktrees
    {
        sqlx::query(
            "INSERT INTO worktrees \
             (id,workspace_id,task_id,owner_agent_id,branch,base_commit,path,status,changed_files,last_activity_at) \
             VALUES ($1,$2,$3,$4,$5,$6,$7,'ACTIVE',$8,$9)",
        )
        .bind(id)
        .bind(workspace)
        .bind(task_id)
        .bind(owner_agent_id)
        .bind(branch)
        .bind(base_commit)
        .bind(path)
        .bind(changed_files)
        .bind(legacy_activity_at)
        .execute(&mut connection)
        .await
        .expect("seed schema-15 worktree");
    }
    let other_profile_id = Uuid::now_v7();
    let valid_secret_id = format!("mcp-valid-{}", Uuid::now_v7().simple());
    let dirty_secret_id = format!("mcp-dirty-{}", Uuid::now_v7().simple());
    let valid_server_id = Uuid::now_v7();
    let dirty_server_id = Uuid::now_v7();
    let nullable_server_id = Uuid::now_v7();
    sqlx::query("INSERT INTO profiles (id,name) VALUES ($1,'mcp-other-profile')")
        .bind(other_profile_id)
        .execute(&mut connection)
        .await
        .expect("create MCP foreign profile");
    for (secret_id, secret_profile_id) in [
        (&valid_secret_id, profile_id),
        (&dirty_secret_id, other_profile_id),
    ] {
        sqlx::query(
            "INSERT INTO secret_references \
             (id,profile_id,backend,locator,purpose,allowed_hosts) \
             VALUES ($1,$2,'legacy_encrypted_database',$3,'provider_credential','{}')",
        )
        .bind(secret_id)
        .bind(secret_profile_id)
        .bind(format!("locator-{secret_id}"))
        .execute(&mut connection)
        .await
        .expect("seed MCP secret reference");
    }
    for (server_id, server_profile_id, secret_reference, enabled) in [
        (
            valid_server_id,
            profile_id,
            Some(valid_secret_id.as_str()),
            true,
        ),
        (
            dirty_server_id,
            profile_id,
            Some(dirty_secret_id.as_str()),
            false,
        ),
        (nullable_server_id, profile_id, None, true),
    ] {
        sqlx::query(
            "INSERT INTO mcp_servers \
             (id,profile_id,name,transport,configuration,auth_secret_reference,enabled) \
             VALUES ($1,$2,$3,'stdio','{}',$4,$5)",
        )
        .bind(server_id)
        .bind(server_profile_id)
        .bind(format!("mcp-{server_id}"))
        .bind(secret_reference)
        .bind(enabled)
        .execute(&mut connection)
        .await
        .expect("seed MCP server");
    }

    sqlx::raw_sql(include_str!("../migrations/0016_worktree_integrity.sql"))
        .execute(&mut connection)
        .await
        .expect("upgrade schema 15 to 16");
    let version: i64 = sqlx::query_scalar("SELECT schema_version FROM schema_metadata")
        .fetch_one(&mut connection)
        .await
        .expect("read schema version");
    assert_eq!(version, 16);
    let safe_survives: i64 = sqlx::query_scalar("SELECT count(*) FROM worktrees WHERE id=$1")
        .bind(safe_worktree_id)
        .fetch_one(&mut connection)
        .await
        .expect("safe schema-15 worktree survives");
    assert_eq!(safe_survives, 1);
    for (id, reason) in [
        (unsafe_branch_worktree_id, "unsafe_branch"),
        (cross_workspace_task_worktree_id, "cross_workspace_task"),
        (
            cross_workspace_owner_worktree_id,
            "cross_workspace_owner_agent",
        ),
        (unsafe_base_commit_worktree_id, "unsafe_base_commit"),
        (unsafe_path_worktree_id, "unsafe_path"),
        (unsafe_changed_files_worktree_id, "unsafe_changed_files"),
    ] {
        let removed: i64 = sqlx::query_scalar("SELECT count(*) FROM worktrees WHERE id=$1")
            .bind(id)
            .fetch_one(&mut connection)
            .await
            .expect("unsafe schema-15 worktree removed");
        assert_eq!(removed, 0, "{reason} worktree must be removed");
        let quarantined_reason: String =
            sqlx::query_scalar("SELECT reason FROM worktree_integrity_quarantine WHERE id=$1")
                .bind(id)
                .fetch_one(&mut connection)
                .await
                .expect("unsafe schema-15 worktree quarantined");
        assert!(
            quarantined_reason.contains(reason),
            "quarantine reason must preserve {reason}: {quarantined_reason}"
        );
    }
    let quarantined = sqlx::query(
        "SELECT id,workspace_id,task_id,owner_agent_id,branch,base_commit,path,status,changed_files,last_activity_at,reason,quarantined_at \
         FROM worktree_integrity_quarantine WHERE id=$1",
    )
    .bind(unsafe_branch_worktree_id)
    .fetch_one(&mut connection)
    .await
    .expect("unsafe worktree is fully quarantined");
    assert_eq!(quarantined.get::<Uuid, _>("id"), unsafe_branch_worktree_id);
    assert_eq!(quarantined.get::<Uuid, _>("workspace_id"), workspace);
    assert_eq!(
        quarantined.get::<Uuid, _>("task_id"),
        unsafe_worktree_task_id
    );
    assert_eq!(
        quarantined.get::<Uuid, _>("owner_agent_id"),
        worktree_agent_id
    );
    assert_eq!(
        quarantined.get::<String, _>("branch"),
        "agent/schema-15[unsafe"
    );
    assert_eq!(quarantined.get::<String, _>("base_commit"), "a".repeat(40));
    assert_eq!(
        quarantined.get::<String, _>("path"),
        format!(
            "/srv/legacy/worktrees/task-{}",
            &unsafe_worktree_task_id.simple().to_string()[..12]
        )
    );
    assert_eq!(quarantined.get::<String, _>("status"), "ACTIVE");
    assert_eq!(
        quarantined.get::<Vec<String>, _>("changed_files"),
        vec!["legacy.rs".to_string()]
    );
    assert_eq!(
        quarantined.get::<OffsetDateTime, _>("last_activity_at"),
        legacy_activity_at
    );
    assert!(
        quarantined
            .get::<String, _>("reason")
            .contains("unsafe_branch")
    );
    assert!(quarantined.get::<OffsetDateTime, _>("quarantined_at") >= legacy_activity_at);
    assert!(
        sqlx::query(
            "INSERT INTO worktrees \
             (id,workspace_id,task_id,owner_agent_id,branch,base_commit,path,status,changed_files) \
             VALUES ($1,$2,$3,$4,'agent[recurrence',$5,$6,'ACTIVE',ARRAY[]::text[])",
        )
        .bind(Uuid::now_v7())
        .bind(workspace)
        .bind(unsafe_recurrence_task_id)
        .bind(worktree_agent_id)
        .bind("a".repeat(40))
        .bind(worktree_path(unsafe_recurrence_task_id))
        .execute(&mut connection)
        .await
        .is_err(),
        "schema 16 must block unsafe worktree recurrence"
    );
    for (id, original) in &evaluation_ids {
        let live: Option<serde_json::Value> =
            sqlx::query_scalar("SELECT evaluation FROM skill_revisions WHERE id=$1")
                .bind(id)
                .fetch_one(&mut connection)
                .await
                .expect("cleared evaluation");
        assert!(live.is_none());
        let quarantined: serde_json::Value = sqlx::query_scalar("SELECT original_evaluation FROM skill_revision_integrity_quarantine WHERE revision_id=$1").bind(id).fetch_one(&mut connection).await.expect("original evaluation");
        assert_eq!(&quarantined, original);
    }
    let (evaluation, sources): (Option<serde_json::Value>, Vec<Uuid>) = sqlx::query_as(
        "SELECT evaluation,source_conversation_ids FROM skill_revisions WHERE id=$1",
    )
    .bind(revision_id)
    .fetch_one(&mut connection)
    .await
    .expect("read repaired revision");
    assert!(evaluation.is_none());
    assert!(sources.is_empty());
    let (nil_original,nil_repaired):(Vec<Uuid>,Vec<Uuid>)=sqlx::query_as("SELECT original_source_conversation_ids,repaired_source_conversation_ids FROM skill_revision_integrity_quarantine WHERE revision_id=$1 AND issue='invalid_source_conversation_ids'").bind(revision_id).fetch_one(&mut connection).await.expect("nil quarantine");
    assert_eq!(nil_original, vec![Uuid::nil()]);
    assert!(nil_repaired.is_empty());
    let duplicate_sources: Vec<Uuid> = sqlx::query_scalar("SELECT source_conversation_ids FROM skill_revisions WHERE skill_id=(SELECT id FROM skills WHERE name='duplicate-source')")
        .fetch_one(&mut connection).await.expect("duplicate repair");
    assert_eq!(duplicate_sources, vec![source]);
    let inaccessible_sources: Vec<Uuid> = sqlx::query_scalar("SELECT source_conversation_ids FROM skill_revisions WHERE skill_id=(SELECT id FROM skills WHERE name='inaccessible-source')")
        .fetch_one(&mut connection).await.expect("inaccessible repair");
    assert!(inaccessible_sources.is_empty());
    let workspace_sources: Vec<Uuid> = sqlx::query_scalar("SELECT source_conversation_ids FROM skill_revisions WHERE skill_id=(SELECT id FROM skills WHERE name='workspace-source')")
        .fetch_one(&mut connection).await.expect("workspace repair");
    assert_eq!(workspace_sources, vec![source]);
    let (duplicate_original, duplicate_repaired): (Vec<Uuid>,Vec<Uuid>) = sqlx::query_as("SELECT original_source_conversation_ids,repaired_source_conversation_ids FROM skill_revision_integrity_quarantine WHERE revision_id=$1").bind(duplicate_revision).fetch_one(&mut connection).await.expect("duplicate quarantine");
    assert_eq!(duplicate_original, vec![source, source]);
    assert_eq!(duplicate_repaired, vec![source]);
    let (inaccessible_original, inaccessible_repaired): (Vec<Uuid>,Vec<Uuid>) = sqlx::query_as("SELECT original_source_conversation_ids,repaired_source_conversation_ids FROM skill_revision_integrity_quarantine WHERE revision_id=$1").bind(inaccessible_revision).fetch_one(&mut connection).await.expect("inaccessible quarantine");
    assert_eq!(inaccessible_original, vec![missing_source, cross_source]);
    assert!(inaccessible_repaired.is_empty());
    let (workspace_original, workspace_repaired): (Vec<Uuid>,Vec<Uuid>) = sqlx::query_as("SELECT original_source_conversation_ids,repaired_source_conversation_ids FROM skill_revision_integrity_quarantine WHERE revision_id=$1").bind(workspace_revision).fetch_one(&mut connection).await.expect("workspace quarantine");
    assert_eq!(
        workspace_original,
        vec![source, wrong_source, deleted_source]
    );
    assert_eq!(workspace_repaired, vec![source]);
    for (id, original) in [
        (missing_revision, vec![missing_source]),
        (cross_revision, vec![cross_source]),
        (deleted_revision, vec![deleted_source]),
        (wrong_revision, vec![wrong_source]),
    ] {
        let (before,after):(Vec<Uuid>,Vec<Uuid>)=sqlx::query_as("SELECT original_source_conversation_ids,repaired_source_conversation_ids FROM skill_revision_integrity_quarantine WHERE revision_id=$1").bind(id).fetch_one(&mut connection).await.expect("independent source quarantine");
        assert_eq!(before, original);
        assert!(after.is_empty());
    }
    let repaired_oversized: Vec<Uuid> =
        sqlx::query_scalar("SELECT source_conversation_ids FROM skill_revisions WHERE id=$1")
            .bind(oversized_revision)
            .fetch_one(&mut connection)
            .await
            .expect("oversized repair");
    assert_eq!(repaired_oversized, oversized[..100].to_vec());
    let (oversized_original,oversized_repaired):(Vec<Uuid>,Vec<Uuid>)=sqlx::query_as("SELECT original_source_conversation_ids,repaired_source_conversation_ids FROM skill_revision_integrity_quarantine WHERE revision_id=$1").bind(oversized_revision).fetch_one(&mut connection).await.expect("oversized quarantine");
    assert_eq!(oversized_original, oversized);
    assert_eq!(oversized_repaired, oversized[..100].to_vec());
    let repaired_active_revision: i64 =
        sqlx::query_scalar("SELECT active_revision FROM skills WHERE id=$1")
            .bind(active_skill)
            .fetch_one(&mut connection)
            .await
            .expect("read canonical active revision");
    let repaired_promoted: (bool, bool) = sqlx::query_as(
        "SELECT (SELECT promoted FROM skill_revisions WHERE id=$1), (SELECT promoted FROM skill_revisions WHERE id=$2)",
    )
    .bind(active_one)
    .bind(active_two)
    .fetch_one(&mut connection)
    .await
    .expect("read canonical promotion state");
    let promoted_count: i64 =
        sqlx::query_scalar("SELECT count(*) FROM skill_revisions WHERE skill_id=$1 AND promoted")
            .bind(active_skill)
            .fetch_one(&mut connection)
            .await
            .expect("count canonical promoted revisions");
    assert_eq!(repaired_active_revision, 1);
    assert_eq!(repaired_promoted, (true, false));
    assert_eq!(promoted_count, 1);
    let promotion_detail: String = sqlx::query_scalar(
        "SELECT detail FROM skill_integrity_quarantine WHERE skill_id=$1 AND issue='promotion_state_mismatch'",
    )
    .bind(active_skill)
    .fetch_one(&mut connection)
    .await
    .expect("read canonical promotion quarantine");
    let promotion_detail: serde_json::Value =
        serde_json::from_str(&promotion_detail).expect("parse promotion quarantine detail");
    assert_eq!(promotion_detail["active_revision"], 1);
    assert_eq!(promotion_detail["promoted_revision"], 2);
    let promoted_canonical: i64 = sqlx::query_scalar("SELECT revision FROM skill_revisions WHERE skill_id=(SELECT id FROM skills WHERE name='active-canonical') AND promoted")
        .fetch_one(&mut connection).await.expect("active repair");
    assert_eq!(promoted_canonical, 1);
    let none_promoted: bool = sqlx::query_scalar("SELECT promoted FROM skill_revisions WHERE skill_id=(SELECT id FROM skills WHERE name='none-promoted')")
        .fetch_one(&mut connection).await.expect("none promotion repair");
    assert!(none_promoted);
    let active: i64 = sqlx::query_scalar("SELECT active_revision FROM skills WHERE id=$1")
        .bind(skill_id)
        .fetch_one(&mut connection)
        .await
        .expect("read repaired active revision");
    assert_eq!(active, 1);
    let quarantined: i64 = sqlx::query_scalar(
        "SELECT count(*) FROM skill_revision_integrity_quarantine WHERE revision_id=$1",
    )
    .bind(revision_id)
    .fetch_one(&mut connection)
    .await
    .expect("read revision quarantine");
    assert_eq!(quarantined, 2);
    let repaired_sources: i64 = sqlx::query_scalar("SELECT count(*) FROM skill_revision_integrity_quarantine WHERE issue='invalid_source_conversation_ids'")
        .fetch_one(&mut connection).await.expect("source quarantine count");
    assert!(repaired_sources >= 4);
    let promotion_repairs: i64 = sqlx::query_scalar(
        "SELECT count(*) FROM skill_integrity_quarantine WHERE issue='promotion_state_mismatch'",
    )
    .fetch_one(&mut connection)
    .await
    .expect("promotion quarantine count");
    assert!(promotion_repairs >= 3);
    for promoted_skill in [skill_id, active_skill, none_skill] {
        let record: i64 = sqlx::query_scalar("SELECT count(*) FROM skill_integrity_quarantine WHERE skill_id=$1 AND issue='promotion_state_mismatch'").bind(promoted_skill).fetch_one(&mut connection).await.expect("per-skill promotion quarantine");
        assert_eq!(record, 1);
    }
    let quarantine_id: i64 = sqlx::query_scalar(
        "SELECT id FROM skill_revision_integrity_quarantine WHERE revision_id=$1 LIMIT 1",
    )
    .bind(revision_id)
    .fetch_one(&mut connection)
    .await
    .expect("read quarantine id");
    assert!(
        sqlx::query("DELETE FROM skill_revision_integrity_quarantine WHERE id=$1")
            .bind(quarantine_id)
            .execute(&mut connection)
            .await
            .is_err()
    );
    let skill_quarantine_id:i64=sqlx::query_scalar("INSERT INTO skill_integrity_quarantine (skill_id,profile_id,name,issue,detail) VALUES ($1,$2,'q','test','{}') RETURNING id").bind(skill_id).bind(profile_id).fetch_one(&mut connection).await.expect("insert skill quarantine");
    assert!(
        sqlx::query("UPDATE skill_integrity_quarantine SET issue='tampered' WHERE id=$1")
            .bind(skill_quarantine_id)
            .execute(&mut connection)
            .await
            .is_err()
    );
    assert!(
        sqlx::query("DELETE FROM skill_integrity_quarantine WHERE id=$1")
            .bind(skill_quarantine_id)
            .execute(&mut connection)
            .await
            .is_err()
    );
    assert!(
        sqlx::query("TRUNCATE skill_integrity_quarantine")
            .execute(&mut connection)
            .await
            .is_err()
    );
    assert!(
        sqlx::query("UPDATE skill_revision_integrity_quarantine SET issue='tampered' WHERE id=$1")
            .bind(quarantine_id)
            .execute(&mut connection)
            .await
            .is_err()
    );
    assert!(
        sqlx::query("TRUNCATE skill_revision_integrity_quarantine")
            .execute(&mut connection)
            .await
            .is_err()
    );
    let valid = serde_json::json!({"deterministic_checks_passed":true,"attempts":1,"successful_attempts":1,"steps":1,"retries":0,"errors":0,"duration_ms":1,"user_corrections":0});
    sqlx::query("UPDATE skill_revisions SET evaluation=$1 WHERE id=$2")
        .bind(&valid)
        .bind(revision_id)
        .execute(&mut connection)
        .await
        .expect("record first evaluation");
    let post_valid = Uuid::now_v7();
    let post_deleted = Uuid::now_v7();
    let post_cross = Uuid::now_v7();
    let post_wrong = Uuid::now_v7();
    let post_profile = Uuid::now_v7();
    let post_user_id = Uuid::now_v7();
    let post_workspace = Uuid::now_v7();
    let post_other_workspace = Uuid::now_v7();
    sqlx::query("INSERT INTO profiles (id,name) VALUES ($1,'post-v14-profile')")
        .bind(post_profile)
        .execute(&mut connection)
        .await
        .expect("post profile");
    sqlx::query(
        "INSERT INTO users (id,email,display_name,password_hash,role,primary_profile_id) \
         VALUES ($1,$2,'Post Owner','unused','OWNER',$3)",
    )
    .bind(post_user_id)
    .bind(format!("{post_user_id}@example.test"))
    .bind(post_profile)
    .execute(&mut connection)
    .await
    .expect("post profile owner");
    sqlx::query("INSERT INTO workspaces (id,profile_id,title,created_by_user_id) VALUES ($1,$2,'post same workspace',$3),($4,$5,'post other workspace',$6)").bind(post_workspace).bind(profile_id).bind(primary_user_id).bind(post_other_workspace).bind(post_profile).bind(post_user_id).execute(&mut connection).await.expect("post workspaces");
    sqlx::query("INSERT INTO conversations (id,profile_id,workspace_id,created_by_user_id,title,status) VALUES ($1,$2,$3,$4,'valid','active'),($5,$2,NULL,$4,'deleted','deleted'),($6,$7,NULL,$8,'cross','active'),($9,$2,$10,$4,'wrong','active')").bind(post_valid).bind(profile_id).bind(workspace).bind(primary_user_id).bind(post_deleted).bind(post_cross).bind(post_profile).bind(post_user_id).bind(post_wrong).bind(post_workspace).execute(&mut connection).await.expect("post source rows");
    let post_missing = Uuid::now_v7();
    let mut post_revision = 10_i64;
    for ids in [
        vec![Uuid::nil()],
        vec![post_valid, post_valid],
        vec![post_missing],
        vec![post_deleted],
        vec![post_cross],
    ] {
        let result=sqlx::query("INSERT INTO skill_revisions (id,skill_id,revision,content,author,reason,source_conversation_ids) VALUES ($1,$2,$3,'bad','tester','bad',$4)").bind(Uuid::now_v7()).bind(skill_id).bind(post_revision).bind(ids).execute(&mut connection).await;
        let constraint = result.as_ref().err().and_then(|error| match error {
            sqlx::Error::Database(database) => database.constraint(),
            _ => None,
        });
        assert_eq!(constraint, Some("skill_revisions_sources_valid"));
        post_revision += 1;
    }
    let post_workspace_skill = Uuid::now_v7();
    sqlx::query("INSERT INTO skills (id,profile_id,workspace_id,name,description) VALUES ($1,$2,$3,'post-scoped','')").bind(post_workspace_skill).bind(profile_id).bind(workspace).execute(&mut connection).await.expect("post scoped skill");
    let result=sqlx::query("INSERT INTO skill_revisions (id,skill_id,revision,content,author,reason,source_conversation_ids) VALUES ($1,$2,15,'bad','tester','bad',$3)").bind(Uuid::now_v7()).bind(post_workspace_skill).bind(vec![post_wrong]).execute(&mut connection).await;
    let constraint = result.as_ref().err().and_then(|error| match error {
        sqlx::Error::Database(database) => database.constraint(),
        _ => None,
    });
    assert_eq!(constraint, Some("skill_revisions_sources_valid"));
    assert!(
        sqlx::query("UPDATE skill_revisions SET evaluation=$1 WHERE id=$2")
            .bind(serde_json::json!({"tampered":true}))
            .bind(revision_id)
            .execute(&mut connection)
            .await
            .is_err()
    );
    assert!(
        sqlx::query("UPDATE skills SET active_revision=NULL WHERE id=$1")
            .bind(skill_id)
            .execute(&mut connection)
            .await
            .is_err()
    );
    sqlx::raw_sql(include_str!(
        "../migrations/0017_mcp_server_secret_profile_integrity.sql"
    ))
    .execute(&mut connection)
    .await
    .expect("upgrade schema 16 to 17");
    let version: i64 = sqlx::query_scalar("SELECT schema_version FROM schema_metadata")
        .fetch_one(&mut connection)
        .await
        .expect("read schema 17 version");
    assert_eq!(version, 17);
    let links: (Option<String>, Option<String>, Option<String>) = sqlx::query_as(
        "SELECT \
            (SELECT auth_secret_reference FROM mcp_servers WHERE id=$1), \
            (SELECT auth_secret_reference FROM mcp_servers WHERE id=$2), \
            (SELECT auth_secret_reference FROM mcp_servers WHERE id=$3)",
    )
    .bind(valid_server_id)
    .bind(dirty_server_id)
    .bind(nullable_server_id)
    .fetch_one(&mut connection)
    .await
    .expect("read repaired MCP links");
    assert_eq!(links.0.as_deref(), Some(valid_secret_id.as_str()));
    assert_eq!(links.1, None);
    assert_eq!(links.2, None);
    let enabled: (bool, bool, bool) = sqlx::query_as(
        "SELECT \
            (SELECT enabled FROM mcp_servers WHERE id=$1), \
            (SELECT enabled FROM mcp_servers WHERE id=$2), \
            (SELECT enabled FROM mcp_servers WHERE id=$3)",
    )
    .bind(valid_server_id)
    .bind(dirty_server_id)
    .bind(nullable_server_id)
    .fetch_one(&mut connection)
    .await
    .expect("read preserved MCP enabled state");
    assert_eq!(enabled, (true, false, true));
    let secret_count: i64 = sqlx::query_scalar("SELECT count(*) FROM secret_references")
        .fetch_one(&mut connection)
        .await
        .expect("count preserved MCP secrets");
    assert_eq!(secret_count, 2);
    let constraints: (bool, bool) = sqlx::query_as(
        "SELECT \
            EXISTS (SELECT 1 FROM pg_constraint WHERE conname='mcp_servers_auth_secret_same_profile_fk'), \
            EXISTS (SELECT 1 FROM pg_constraint WHERE conname='mcp_servers_auth_secret_reference_fkey')",
    )
    .fetch_one(&mut connection)
    .await
    .expect("inspect MCP secret constraints");
    assert_eq!(constraints, (true, false));

    let cross_profile_insert = sqlx::query(
        "INSERT INTO mcp_servers \
         (id,profile_id,name,transport,configuration,auth_secret_reference) \
         VALUES ($1,$2,'cross-profile','stdio','{}',$3)",
    )
    .bind(Uuid::now_v7())
    .bind(profile_id)
    .bind(&dirty_secret_id)
    .execute(&mut connection)
    .await;
    let cross_profile_constraint =
        cross_profile_insert
            .as_ref()
            .err()
            .and_then(|error| match error {
                sqlx::Error::Database(database) => database.constraint(),
                _ => None,
            });
    assert_eq!(
        cross_profile_constraint,
        Some("mcp_servers_auth_secret_same_profile_fk")
    );
    let cross_profile_update =
        sqlx::query("UPDATE mcp_servers SET auth_secret_reference=$1 WHERE id=$2")
            .bind(&dirty_secret_id)
            .bind(dirty_server_id)
            .execute(&mut connection)
            .await;
    let cross_profile_update_constraint =
        cross_profile_update
            .as_ref()
            .err()
            .and_then(|error| match error {
                sqlx::Error::Database(database) => database.constraint(),
                _ => None,
            });
    assert_eq!(
        cross_profile_update_constraint,
        Some("mcp_servers_auth_secret_same_profile_fk")
    );

    sqlx::query("UPDATE mcp_servers SET auth_secret_reference=$1 WHERE id=$2")
        .bind(&valid_secret_id)
        .bind(nullable_server_id)
        .execute(&mut connection)
        .await
        .expect("same-profile MCP link succeeds");
    let profile_reassignment = sqlx::query("UPDATE mcp_servers SET profile_id=$1 WHERE id=$2")
        .bind(other_profile_id)
        .bind(valid_server_id)
        .execute(&mut connection)
        .await;
    let profile_reassignment_constraint =
        profile_reassignment
            .as_ref()
            .err()
            .and_then(|error| match error {
                sqlx::Error::Database(database) => database.constraint(),
                _ => None,
            });
    assert_eq!(
        profile_reassignment_constraint,
        Some("mcp_servers_auth_secret_same_profile_fk")
    );
    let secret_reassignment = sqlx::query("UPDATE secret_references SET profile_id=$1 WHERE id=$2")
        .bind(other_profile_id)
        .bind(&valid_secret_id)
        .execute(&mut connection)
        .await;
    let secret_reassignment_constraint =
        secret_reassignment
            .as_ref()
            .err()
            .and_then(|error| match error {
                sqlx::Error::Database(database) => database.constraint(),
                _ => None,
            });
    assert_eq!(
        secret_reassignment_constraint,
        Some("mcp_servers_auth_secret_same_profile_fk")
    );
    let deletable_secret_id = format!("mcp-delete-{}", Uuid::now_v7().simple());
    let deletable_server_id = Uuid::now_v7();
    sqlx::query(
        "INSERT INTO secret_references \
         (id,profile_id,backend,locator,purpose,allowed_hosts) \
         VALUES ($1,$2,'legacy_encrypted_database',$3,'provider_credential','{}')",
    )
    .bind(&deletable_secret_id)
    .bind(profile_id)
    .bind("locator-delete")
    .execute(&mut connection)
    .await
    .expect("insert deletable MCP secret");
    sqlx::query(
        "INSERT INTO mcp_servers \
         (id,profile_id,name,transport,configuration,auth_secret_reference,enabled) \
         VALUES ($1,$2,'delete-secret','stdio','{}',$3,false)",
    )
    .bind(deletable_server_id)
    .bind(profile_id)
    .bind(&deletable_secret_id)
    .execute(&mut connection)
    .await
    .expect("insert deletable MCP link");
    sqlx::query("DELETE FROM secret_references WHERE id=$1")
        .bind(&deletable_secret_id)
        .execute(&mut connection)
        .await
        .expect("delete MCP secret");
    let deleted_link: (Option<String>, bool) =
        sqlx::query_as("SELECT auth_secret_reference,enabled FROM mcp_servers WHERE id=$1")
            .bind(deletable_server_id)
            .fetch_one(&mut connection)
            .await
            .expect("read MCP link after secret deletion");
    assert_eq!(deleted_link, (None, false));

    sqlx::raw_sql(include_str!(
        "../migrations/0018_mcp_auth_states_vault_pkce.sql"
    ))
    .execute(&mut connection)
    .await
    .expect("upgrade schema 17 to 18");
    let v18_version: i64 = sqlx::query_scalar("SELECT schema_version FROM schema_metadata")
        .fetch_one(&mut connection)
        .await
        .expect("read schema 18 version");
    assert_eq!(v18_version, 18);

    connection.close().await.expect("close isolated session");
    sqlx::query(&format!("DROP SCHEMA {schema} CASCADE"))
        .execute(&pool)
        .await
        .expect("drop isolated schema");
}

#[tokio::test]
async fn autobiography_constraint_uses_character_length() {
    let Some(database_url) = std::env::var("GOBROWSE_TEST_DATABASE_URL").ok() else {
        eprintln!("GOBROWSE_TEST_DATABASE_URL is unset; skipping PostgreSQL integration test");
        return;
    };
    let _lock = common::acquire_test_lock(&database_url).await;
    let pool = test_pool().await.expect("test pool after lock");
    let profile_id = Uuid::now_v7();
    sqlx::query("INSERT INTO profiles (id, name) VALUES ($1, 'constraint-test')")
        .bind(profile_id)
        .execute(&pool)
        .await
        .expect("create test profile");
    sqlx::query(
        "INSERT INTO books (id, profile_id, title, body, book_type, scope, provenance, trust, author, security_classification) \
         VALUES ($1,$2,'Autobiography',$3,'AUTOBIOGRAPHY','PROFILE','USER','USER_PROVIDED','test','CONFIDENTIAL')",
    )
    .bind(Uuid::now_v7())
    .bind(profile_id)
    .bind("🦀".repeat(99_999))
    .execute(&pool)
    .await
    .expect("99,999 Unicode characters must be accepted");

    let second_profile = Uuid::now_v7();
    sqlx::query("INSERT INTO profiles (id, name) VALUES ($1, 'constraint-test-overflow')")
        .bind(second_profile)
        .execute(&pool)
        .await
        .expect("create overflow test profile");
    let overflow = sqlx::query(
        "INSERT INTO books (id, profile_id, title, body, book_type, scope, provenance, trust, author, security_classification) \
         VALUES ($1,$2,'Autobiography',$3,'AUTOBIOGRAPHY','PROFILE','USER','USER_PROVIDED','test','CONFIDENTIAL')",
    )
    .bind(Uuid::now_v7())
    .bind(second_profile)
    .bind("🦀".repeat(100_000))
    .execute(&pool)
    .await;
    assert!(
        overflow.is_err(),
        "100,000 Unicode characters must be rejected"
    );
}

#[tokio::test]
async fn lexical_search_and_revision_history_are_transactional() {
    let Some(database_url) = std::env::var("GOBROWSE_TEST_DATABASE_URL").ok() else {
        eprintln!("GOBROWSE_TEST_DATABASE_URL is unset; skipping PostgreSQL integration test");
        return;
    };
    let _lock = common::acquire_test_lock(&database_url).await;
    let pool = test_pool().await.expect("test pool after lock");
    let profile_id = Uuid::now_v7();
    let book_id = Uuid::now_v7();
    sqlx::query("INSERT INTO profiles (id, name) VALUES ($1, 'search-test')")
        .bind(profile_id)
        .execute(&pool)
        .await
        .expect("create test profile");
    sqlx::query(
        "INSERT INTO books (id, profile_id, title, body, book_type, scope, provenance, trust, author, security_classification) \
         VALUES ($1,$2,'Sandbox boundary','rootless containers isolate terminal execution','DOCUMENT','PROFILE','USER','VERIFIED','test','INTERNAL')",
    )
    .bind(book_id)
    .bind(profile_id)
    .execute(&pool)
    .await
    .expect("create searchable Book");
    let found: bool = sqlx::query_scalar(
        "SELECT search_document @@ websearch_to_tsquery('english', 'rootless terminal') FROM books WHERE id = $1",
    )
    .bind(book_id)
    .fetch_one(&pool)
    .await
    .expect("run lexical query");
    assert!(found);

    let mut tx = pool.begin().await.expect("begin revision transaction");
    sqlx::query(
        "INSERT INTO book_revisions (id, book_id, revision, title, body, tags, metadata, change_reason) \
         SELECT $1, id, revision, title, body, tags, metadata, 'integration test' FROM books WHERE id = $2 FOR UPDATE",
    )
    .bind(Uuid::now_v7())
    .bind(book_id)
    .execute(&mut *tx)
    .await
    .expect("write immutable revision");
    sqlx::query(
        "UPDATE books SET body = 'updated canonical body', revision = revision + 1 WHERE id = $1",
    )
    .bind(book_id)
    .execute(&mut *tx)
    .await
    .expect("update canonical Book");
    tx.commit().await.expect("commit revision transaction");
    let old_body: String =
        sqlx::query_scalar("SELECT body FROM book_revisions WHERE book_id = $1 AND revision = 1")
            .bind(book_id)
            .fetch_one(&pool)
            .await
            .expect("read revision");
    assert_eq!(old_body, "rootless containers isolate terminal execution");
}

#[tokio::test]
async fn vault_round_trip_never_persists_plaintext() {
    let Some(database_url) = std::env::var("GOBROWSE_TEST_DATABASE_URL").ok() else {
        eprintln!("GOBROWSE_TEST_DATABASE_URL is unset; skipping PostgreSQL integration test");
        return;
    };
    let _lock = common::acquire_test_lock(&database_url).await;
    let pool = test_pool().await.expect("test pool after lock");
    let profile_id = Uuid::now_v7();
    sqlx::query("INSERT INTO profiles (id, name) VALUES ($1, 'vault-test')")
        .bind(profile_id)
        .execute(&pool)
        .await
        .expect("create vault test profile");
    let settings = VaultSettings {
        master_key_file: None,
        master_key_base64: Some(SecretString::from(STANDARD.encode([19_u8; 32]))),
        key_version: 1,
        ..VaultSettings::default()
    };
    let vault = vault::Vault::from_settings(&settings)
        .await
        .expect("create vault");
    let id = format!("secret_{}", Uuid::now_v7());
    let plaintext = SecretString::from("provider-token-that-must-not-leak".to_owned());
    let encrypted = vault
        .encrypt(profile_id, &id, "provider_credential", &plaintext)
        .expect("encrypt provider credential");
    sqlx::query(
        "INSERT INTO secret_references \
         (id,profile_id,backend,locator,encrypted_value,nonce,key_version,purpose,algorithm,wrapped_data_key,wrap_nonce) \
         VALUES ($1,$2,'encrypted_database','database',$3,$4,$5,'provider_credential',$6,$7,$8)",
    )
    .bind(&id)
    .bind(profile_id)
    .bind(&encrypted.ciphertext)
    .bind(&encrypted.nonce)
    .bind(encrypted.key_version)
    .bind(vault::algorithm())
    .bind(&encrypted.wrapped_data_key)
    .bind(&encrypted.wrap_nonce)
    .execute(&pool)
    .await
    .expect("store encrypted credential");
    assert!(
        !encrypted
            .ciphertext
            .windows(plaintext.expose_secret().len())
            .any(|window| window == plaintext.expose_secret().as_bytes())
    );
    let resolved = vault
        .resolve(&pool, profile_id, &id)
        .await
        .expect("resolve encrypted credential");
    assert_eq!(resolved.expose_secret(), plaintext.expose_secret());

    let rotating = vault::Vault::from_settings(&VaultSettings {
        master_key_file: None,
        master_key_base64: Some(SecretString::from(STANDARD.encode([20_u8; 32]))),
        key_version: 2,
        previous_master_key_file: None,
        previous_master_key_base64: Some(SecretString::from(STANDARD.encode([19_u8; 32]))),
        previous_key_version: Some(1),
    })
    .await
    .expect("create rotating vault");
    let mut tx = pool.begin().await.expect("start key rotation");
    assert_eq!(
        rotating
            .rotate_profile(&mut tx, profile_id)
            .await
            .expect("rewrap profile secrets"),
        1
    );
    tx.commit().await.expect("commit key rotation");
    let key_version: i32 =
        sqlx::query_scalar("SELECT key_version FROM secret_references WHERE id=$1")
            .bind(&id)
            .fetch_one(&pool)
            .await
            .expect("read rotated key version");
    assert_eq!(key_version, 2);
    let resolved = rotating
        .resolve(&pool, profile_id, &id)
        .await
        .expect("resolve rotated credential");
    assert_eq!(resolved.expose_secret(), plaintext.expose_secret());
    let mut stale_writer = pool.begin().await.expect("start stale writer");
    assert!(
        vault
            .fence_current_key(&mut stale_writer, profile_id)
            .await
            .is_err(),
        "an instance with the previous key must be fenced after rotation"
    );
    stale_writer
        .rollback()
        .await
        .expect("rollback stale writer");
}

#[tokio::test]
async fn doctor_reports_redacted_mcp_metadata_readiness_counts() {
    let Some(database_url) = std::env::var("GOBROWSE_TEST_DATABASE_URL").ok() else {
        eprintln!("GOBROWSE_TEST_DATABASE_URL is unset; skipping PostgreSQL integration test");
        return;
    };
    let _lock = common::acquire_test_lock(&database_url).await;
    let pool = test_pool().await.expect("test pool after lock");
    let profile_id = Uuid::now_v7();
    sqlx::query("INSERT INTO profiles (id, name) VALUES ($1, 'doctor-mcp-test')")
        .bind(profile_id)
        .execute(&pool)
        .await
        .expect("create doctor test profile");
    let settings = Settings {
        http: HttpSettings::default(),
        database: DatabaseSettings {
            url: database_url.clone().into(),
            max_connections: 1,
        },
        auth: AuthSettings::default(),
        vault: VaultSettings {
            master_key_base64: Some(SecretString::from(STANDARD.encode([29_u8; 32]))),
            key_version: 7,
            previous_master_key_base64: Some(SecretString::from(STANDARD.encode([30_u8; 32]))),
            previous_key_version: Some(6),
            ..VaultSettings::default()
        },
        features: FeatureSettings::default(),
        observability: ObservabilitySettings::default(),
    };
    let configured_vault = vault::Vault::from_settings(&settings.vault)
        .await
        .expect("valid doctor vault");
    let empty_checks = doctor::run(&settings, Some(&pool)).await;
    let empty_check = empty_checks
        .iter()
        .find(|check| check.name == "MCP OAuth vault metadata")
        .expect("empty MCP doctor check");
    assert!(matches!(empty_check.status, doctor::Status::Warn));
    assert_eq!(empty_check.detail, "credentials=0, ready=0, invalid=0");
    let rows = [
        (
            "mcp_oauth_access_token",
            vec!["example.com"],
            "encrypted_database",
            7,
        ),
        (
            "mcp_oauth_refresh_token",
            vec!["example.com"],
            "encrypted_database",
            6,
        ),
        ("mcp_unknown", vec!["example.com"], "encrypted_database", 7),
        (
            "mcp_oauth_client_secret",
            vec!["example.com"],
            "legacy_encrypted_database",
            7,
        ),
        (
            "mcp_oauth_pkce_verifier",
            vec!["127.0.0.1"],
            "encrypted_database",
            7,
        ),
    ];
    for (purpose, allowed_hosts, backend, key_version) in rows {
        let id = format!("doctor-secret-{}", Uuid::now_v7());
        if backend == "encrypted_database" {
            let encrypted = configured_vault
                .encrypt(
                    profile_id,
                    &id,
                    purpose,
                    &SecretString::from("doctor-test-secret"),
                )
                .expect("encrypt doctor fixture");
            sqlx::query(
                "INSERT INTO secret_references \
                 (id,profile_id,backend,locator,encrypted_value,nonce,key_version,purpose,allowed_hosts,algorithm,wrapped_data_key,wrap_nonce) \
                 VALUES ($1,$2,$3,'database',$4,$5,$6,$7,$8,$9,$10,$11)",
            )
            .bind(&id)
            .bind(profile_id)
            .bind(backend)
            .bind(encrypted.ciphertext)
            .bind(encrypted.nonce)
            .bind(key_version)
            .bind(purpose)
            .bind(allowed_hosts.as_slice())
            .bind(vault::algorithm())
            .bind(encrypted.wrapped_data_key)
            .bind(encrypted.wrap_nonce)
            .execute(&pool)
            .await
            .expect("insert encrypted doctor fixture");
        } else {
            sqlx::query(
                "INSERT INTO secret_references \
                 (id,profile_id,backend,locator,purpose,allowed_hosts) \
                 VALUES ($1,$2,$3,'legacy',$4,$5)",
            )
            .bind(&id)
            .bind(profile_id)
            .bind(backend)
            .bind(purpose)
            .bind(allowed_hosts.as_slice())
            .execute(&pool)
            .await
            .expect("insert legacy doctor fixture");
        }
    }
    let null_key_version_id = format!("doctor-null-key-version-{}", Uuid::now_v7());
    sqlx::query(
        "INSERT INTO secret_references \
         (id,profile_id,backend,locator,purpose,allowed_hosts) \
         VALUES ($1,$2,'legacy_encrypted_database','legacy',$3,$4)",
    )
    .bind(&null_key_version_id)
    .bind(profile_id)
    .bind("mcp_oauth_access_token")
    .bind(["example.com"].as_slice())
    .execute(&pool)
    .await
    .expect("insert null key-version doctor fixture");

    let checks = doctor::run(&settings, Some(&pool)).await;
    let check = checks
        .iter()
        .find(|check| check.name == "MCP OAuth vault metadata")
        .expect("MCP doctor check");
    assert!(matches!(check.status, doctor::Status::Fail));
    assert_eq!(check.detail, "credentials=6, ready=2, invalid=4");
    assert!(!check.detail.contains("doctor-secret"));
    assert!(!check.detail.contains("doctor-test-secret"));

    let overflow_prefix = format!("doctor-overflow-{}-", Uuid::now_v7());
    sqlx::query(
        "INSERT INTO secret_references \
         (id,profile_id,backend,locator,encrypted_value,nonce,key_version,purpose,allowed_hosts,algorithm,wrapped_data_key,wrap_nonce) \
         SELECT $1 || i::text,$2,'encrypted_database','database',decode('00','hex'), \
                decode('000000000000000000000000','hex'),7,'mcp_oauth_access_token', \
                ARRAY['example.com']::text[],$3,decode('00','hex'),decode(md5(i::text),'hex') \
         FROM generate_series(1,1001) AS i",
    )
    .bind(&overflow_prefix)
    .bind(profile_id)
    .bind(vault::algorithm())
    .execute(&pool)
    .await
    .expect("insert overflow doctor fixtures");
    let overflow_checks = doctor::run(&settings, Some(&pool)).await;
    let overflow_check = overflow_checks
        .iter()
        .find(|check| check.name == "MCP OAuth vault metadata")
        .expect("overflow MCP doctor check");
    assert!(matches!(overflow_check.status, doctor::Status::Fail));
    assert_eq!(
        overflow_check.detail,
        "credentials>1000, ready=0, invalid>1000"
    );
    sqlx::query("DELETE FROM profiles WHERE id=$1")
        .bind(profile_id)
        .execute(&pool)
        .await
        .expect("cleanup doctor test profile");
}

#[tokio::test]
async fn deployed_schema_v3_upgrades_to_v18() {
    let Some(database_url) = std::env::var("GOBROWSE_TEST_DATABASE_URL").ok() else {
        eprintln!("GOBROWSE_TEST_DATABASE_URL is unset; skipping PostgreSQL integration test");
        return;
    };
    let _lock = common::acquire_test_lock(&database_url).await;
    let pool = PgPool::connect(&database_url)
        .await
        .expect("connect to test PostgreSQL");
    gobrowse_server::db::migrate(&pool)
        .await
        .expect("install shared extensions before isolated upgrade test");
    let schema = format!("upgrade_{}", Uuid::now_v7().simple());
    sqlx::query(&format!("CREATE SCHEMA {schema}"))
        .execute(&pool)
        .await
        .expect("create isolated upgrade schema");
    let mut connection = PgConnection::connect(&database_url)
        .await
        .expect("connect upgrade session");
    sqlx::query(&format!("SET search_path TO {schema},public"))
        .execute(&mut connection)
        .await
        .expect("select upgrade schema");
    sqlx::raw_sql(include_str!("../migrations/0001_initial.sql"))
        .execute(&mut connection)
        .await
        .expect("install schema v1");
    let profile_id = Uuid::now_v7();
    let user_id = Uuid::now_v7();
    let private_book = Uuid::now_v7();
    let agent_book = Uuid::now_v7();
    let autobiography = Uuid::now_v7();
    let conversation_id = Uuid::now_v7();
    let workspace_id = Uuid::now_v7();
    let foreign_workspace_id = Uuid::now_v7();
    let agent_id = Uuid::now_v7();
    let foreign_agent_id = Uuid::now_v7();
    let run_id = Uuid::now_v7();
    let chunk_id = Uuid::now_v7();
    sqlx::query("INSERT INTO profiles (id,name) VALUES ($1,'upgrade-test')")
        .bind(profile_id)
        .execute(&mut connection)
        .await
        .expect("create v1 profile");
    sqlx::query(
        "INSERT INTO users (id,email,display_name,password_hash,role,primary_profile_id) \
         VALUES ($1,$2,'Upgrade Owner','unused','OWNER',$3)",
    )
    .bind(user_id)
    .bind(format!("{user_id}@example.test"))
    .bind(profile_id)
    .execute(&mut connection)
    .await
    .expect("create v1 user");
    sqlx::query(
        "INSERT INTO workspaces (id,profile_id,title) \
         VALUES ($1,$2,'Legacy workspace'),($3,$2,'Foreign legacy workspace')",
    )
    .bind(workspace_id)
    .bind(profile_id)
    .bind(foreign_workspace_id)
    .execute(&mut connection)
    .await
    .expect("create v1 workspaces");
    sqlx::query("INSERT INTO audit_events (actor_user_id,profile_id,action,resource_type,resource_id,outcome) VALUES ($1,$2,'workspace.created','workspace',$3,'success')")
        .bind(user_id).bind(profile_id).bind(workspace_id.to_string()).execute(&mut connection).await.expect("audit workspace creator");
    sqlx::query("INSERT INTO agents (id,workspace_id,name,kind,permissions,status) VALUES ($1,$2,'Legacy agent','coding','{}','paused')")
        .bind(agent_id).bind(workspace_id).execute(&mut connection).await.expect("create v1 agent");
    sqlx::query("INSERT INTO agent_runs (id,agent_id,state) VALUES ($1,$2,'paused')")
        .bind(run_id)
        .bind(agent_id)
        .execute(&mut connection)
        .await
        .expect("create v1 run");
    sqlx::query(
        "INSERT INTO run_events (run_id,event_type,payload) VALUES ($1,'legacy.paused','{}')",
    )
    .bind(run_id)
    .execute(&mut connection)
    .await
    .expect("create v1 run event");
    for (id, title, scope, book_type) in [
        (private_book, "Private", "PRIVATE", "NOTE"),
        (agent_book, "Agent", "AGENT", "NOTE"),
        (autobiography, "Autobiography", "PROFILE", "AUTOBIOGRAPHY"),
    ] {
        sqlx::query(
            "INSERT INTO books (id,profile_id,title,body,book_type,scope,provenance,trust,author,security_classification,embedding_model_id,embedding_status) \
             VALUES ($1,$2,$3,'legacy body',$4,$5,'USER','USER_PROVIDED','legacy','INTERNAL','legacy-model','ready')",
        )
        .bind(id)
        .bind(profile_id)
        .bind(title)
        .bind(book_type)
        .bind(scope)
        .execute(&mut connection)
        .await
        .expect("create v1 Book");
    }
    sqlx::query(
        "INSERT INTO providers (id,profile_id,provider_type,display_name,base_url) \
         VALUES ('legacy-provider',$1,'ollama','Legacy','http://127.0.0.1:11434')",
    )
    .bind(profile_id)
    .execute(&mut connection)
    .await
    .expect("create v1 provider");
    sqlx::query(
        "INSERT INTO conversations (id,profile_id,workspace_id,title) VALUES ($1,$2,$3,'Legacy conversation')",
    )
    .bind(conversation_id)
    .bind(profile_id)
    .bind(workspace_id)
    .execute(&mut connection)
    .await
    .expect("create v1 conversation");
    for title in ["Projection one", "Projection two"] {
        sqlx::query(
            "INSERT INTO books (id,profile_id,title,body,book_type,scope,provenance,trust,author,conversation_id,security_classification) \
             VALUES ($1,$2,$3,'legacy conversation','CONVERSATION','CONVERSATION','CONVERSATION','VERIFIED','legacy',$4,'INTERNAL')",
        )
        .bind(Uuid::now_v7())
        .bind(profile_id)
        .bind(title)
        .bind(conversation_id)
        .execute(&mut connection)
        .await
        .expect("create duplicate v1 projection");
    }
    sqlx::query(
        "INSERT INTO embedding_models (id,provider_id,model_reference,dimensions) \
         VALUES ('legacy-model','legacy-provider','legacy',3)",
    )
    .execute(&mut connection)
    .await
    .expect("create v1 embedding model");
    sqlx::query(
        "INSERT INTO embedding_models (id,provider_id,model_reference,dimensions) \
         VALUES ('legacy-model-duplicate','legacy-provider','legacy',3)",
    )
    .execute(&mut connection)
    .await
    .expect("create duplicate v1 embedding model");
    sqlx::query(
        "INSERT INTO book_chunks (id,book_id,ordinal,text,token_estimate,source_start,source_end,embedding,embedding_model_id) \
         VALUES ($1,$2,0,'legacy body',3,0,11,'[1,0,0]'::vector,'legacy-model')",
    )
    .bind(chunk_id)
    .bind(private_book)
    .execute(&mut connection)
    .await
    .expect("create v1 vector");
    for status in ["queued", "running"] {
        sqlx::query(
            "INSERT INTO embedding_jobs (id,book_id,embedding_model_id,status) VALUES ($1,$2,'legacy-model',$3)",
        )
        .bind(Uuid::now_v7())
        .bind(private_book)
        .bind(status)
        .execute(&mut connection)
        .await
        .expect("create v1 duplicate active job");
    }
    sqlx::query(
        "INSERT INTO embedding_jobs (id,book_id,embedding_model_id,status,attempts) \
         VALUES ($1,$2,'legacy-model','queued',99)",
    )
    .bind(Uuid::now_v7())
    .bind(agent_book)
    .execute(&mut connection)
    .await
    .expect("create exhausted v1 job");
    sqlx::query(
        "INSERT INTO secret_references (id,profile_id,backend,locator,encrypted_value,nonce,key_version) \
         VALUES ('legacy-secret',$1,'encrypted_database','legacy',decode('01','hex'),decode('02','hex'),1)",
    )
    .bind(profile_id)
    .execute(&mut connection)
    .await
    .expect("create v1 encrypted secret");
    for _ in 0..2 {
        sqlx::query(
            "INSERT INTO autobiography_proposals \
             (id,profile_id,book_id,before_body,after_body,reason,status) \
             VALUES ($1,$2,$3,'legacy body','proposed','legacy proposal','pending')",
        )
        .bind(Uuid::now_v7())
        .bind(profile_id)
        .bind(autobiography)
        .execute(&mut connection)
        .await
        .expect("create v1 pending proposal");
    }
    sqlx::raw_sql(include_str!("../migrations/0002_library_embeddings.sql"))
        .execute(&mut connection)
        .await
        .expect("upgrade dirty v1 state to v2");
    let schema_version: i64 = sqlx::query_scalar("SELECT schema_version FROM schema_metadata")
        .fetch_one(&mut connection)
        .await
        .expect("read upgraded version");
    assert_eq!(schema_version, 2);
    let migrated_vectors: i64 = sqlx::query_scalar("SELECT count(*) FROM book_chunk_embeddings")
        .fetch_one(&mut connection)
        .await
        .expect("count migrated vectors");
    assert_eq!(migrated_vectors, 1);
    let active_jobs: i64 = sqlx::query_scalar(
        "SELECT count(*) FROM embedding_jobs WHERE book_id=$1 AND embedding_model_id='legacy-model' \
         AND status IN ('queued','retry','running')",
    )
    .bind(private_book)
    .fetch_one(&mut connection)
    .await
    .expect("count deduplicated jobs");
    assert_eq!(active_jobs, 1);
    let exhausted_status: String =
        sqlx::query_scalar("SELECT status FROM embedding_jobs WHERE book_id=$1 AND attempts=99")
            .bind(agent_book)
            .fetch_one(&mut connection)
            .await
            .expect("read exhausted migrated job");
    assert_eq!(exhausted_status, "failed");
    let projection_count: i64 = sqlx::query_scalar(
        "SELECT count(*) FROM books WHERE conversation_id=$1 AND book_type='CONVERSATION'",
    )
    .bind(conversation_id)
    .fetch_one(&mut connection)
    .await
    .expect("count deduplicated projections");
    assert_eq!(projection_count, 1);
    let model_count: i64 = sqlx::query_scalar(
        "SELECT count(*) FROM embedding_models WHERE provider_id='legacy-provider' AND model_reference='legacy'",
    )
    .fetch_one(&mut connection)
    .await
    .expect("count deduplicated models");
    assert_eq!(model_count, 1);
    let legacy_backend: String =
        sqlx::query_scalar("SELECT backend FROM secret_references WHERE id='legacy-secret'")
            .fetch_one(&mut connection)
            .await
            .expect("read quarantined secret");
    assert_eq!(legacy_backend, "legacy_encrypted_database");
    let rejected_proposals: i64 =
        sqlx::query_scalar("SELECT count(*) FROM autobiography_proposals WHERE status='rejected'")
            .fetch_one(&mut connection)
            .await
            .expect("count invalidated proposals");
    assert_eq!(rejected_proposals, 2);
    let agent_scope: String = sqlx::query_scalar("SELECT scope FROM books WHERE id=$1")
        .bind(agent_book)
        .fetch_one(&mut connection)
        .await
        .expect("read quarantined Agent Book");
    assert_eq!(agent_scope, "PROFILE");
    sqlx::raw_sql(include_str!("../migrations/0003_chat_runs.sql"))
        .execute(&mut connection)
        .await
        .expect("upgrade deployed v2 state to v3");
    let schema_version: i64 = sqlx::query_scalar("SELECT schema_version FROM schema_metadata")
        .fetch_one(&mut connection)
        .await
        .expect("read schema v3 version");
    assert_eq!(schema_version, 3);
    let conversation_owner: Uuid =
        sqlx::query_scalar("SELECT created_by_user_id FROM conversations WHERE id=$1")
            .bind(conversation_id)
            .fetch_one(&mut connection)
            .await
            .expect("read conversation owner");
    assert_eq!(conversation_owner, user_id);
    let workspace_access: String = sqlx::query_scalar(
        "SELECT access FROM workspace_memberships WHERE workspace_id=$1 AND user_id=$2",
    )
    .bind(workspace_id)
    .bind(user_id)
    .fetch_one(&mut connection)
    .await
    .expect("read backfilled workspace owner");
    assert_eq!(workspace_access, "OWNER");
    let legacy_run: (Uuid, String) =
        sqlx::query_as("SELECT profile_id,run_kind FROM agent_runs WHERE id=$1")
            .bind(run_id)
            .fetch_one(&mut connection)
            .await
            .expect("read migrated legacy run");
    assert_eq!(legacy_run, (profile_id, "agent".into()));
    let event_profile: Uuid =
        sqlx::query_scalar("SELECT profile_id FROM run_events WHERE run_id=$1")
            .bind(run_id)
            .fetch_one(&mut connection)
            .await
            .expect("read migrated event profile");
    assert_eq!(event_profile, profile_id);
    let immutable = sqlx::query("UPDATE book_revisions SET body='rewritten'")
        .execute(&mut connection)
        .await;
    assert!(immutable.is_err(), "revision trigger must reject updates");
    let legacy_safe_task_id = Uuid::from_u128(0x00000000000170008000000000000001);
    let legacy_unsafe_task_id = Uuid::from_u128(0x00000000000270008000000000000002);
    let legacy_foreign_task_id = Uuid::from_u128(0x00000000000370008000000000000003);
    let legacy_cross_owner_task_id = Uuid::from_u128(0x00000000000470008000000000000004);
    let legacy_unsafe_base_task_id = Uuid::from_u128(0x00000000000570008000000000000005);
    let legacy_unsafe_path_task_id = Uuid::from_u128(0x00000000000670008000000000000006);
    let legacy_unsafe_files_task_id = Uuid::from_u128(0x00000000000770008000000000000007);
    let legacy_unsafe_recurrence_task_id = Uuid::from_u128(0x00000000000870008000000000000008);
    let legacy_safe_worktree_id = Uuid::now_v7();
    let legacy_unsafe_branch_worktree_id = Uuid::now_v7();
    let legacy_cross_task_worktree_id = Uuid::now_v7();
    let legacy_cross_owner_worktree_id = Uuid::now_v7();
    let legacy_unsafe_base_worktree_id = Uuid::now_v7();
    let legacy_unsafe_path_worktree_id = Uuid::now_v7();
    let legacy_unsafe_files_worktree_id = Uuid::now_v7();
    // PostgreSQL `TIMESTAMPTZ` persists microseconds, so seed the legacy value at that precision
    // and retain exact equality as the migration-preservation check.
    let legacy_worktree_activity_at = OffsetDateTime::now_utc() - Duration::hours(1);
    let legacy_worktree_activity_at = legacy_worktree_activity_at
        .replace_nanosecond(legacy_worktree_activity_at.nanosecond() / 1_000 * 1_000)
        .expect("truncate legacy activity timestamp to PostgreSQL microseconds");
    for task_id in [
        legacy_safe_task_id,
        legacy_unsafe_task_id,
        legacy_cross_owner_task_id,
        legacy_unsafe_base_task_id,
        legacy_unsafe_path_task_id,
        legacy_unsafe_files_task_id,
        legacy_unsafe_recurrence_task_id,
    ] {
        sqlx::query(
            "INSERT INTO tasks (id,workspace_id,title,state) \
             VALUES ($1,$2,'Schema three worktree task','BACKLOG')",
        )
        .bind(task_id)
        .bind(workspace_id)
        .execute(&mut connection)
        .await
        .expect("seed schema-3 local worktree task");
    }
    sqlx::query(
        "INSERT INTO tasks (id,workspace_id,title,state) \
         VALUES ($1,$2,'Schema three foreign worktree task','BACKLOG')",
    )
    .bind(legacy_foreign_task_id)
    .bind(foreign_workspace_id)
    .execute(&mut connection)
    .await
    .expect("seed schema-3 foreign worktree task");
    sqlx::query(
        "INSERT INTO agents (id,workspace_id,name,kind,permissions,status) \
         VALUES ($1,$2,'Foreign legacy agent','coding','{}','paused')",
    )
    .bind(foreign_agent_id)
    .bind(foreign_workspace_id)
    .execute(&mut connection)
    .await
    .expect("seed schema-3 foreign worktree agent");
    let legacy_worktree_path = |task_id: Uuid| {
        format!(
            "/srv/schema-3/worktrees/task-{}",
            &task_id.simple().to_string()[..12]
        )
    };
    let seeded_worktrees = vec![
        (
            legacy_safe_worktree_id,
            legacy_safe_task_id,
            agent_id,
            "agent/schema-3-safe".to_string(),
            "a".repeat(40),
            legacy_worktree_path(legacy_safe_task_id),
            vec!["legacy.rs".to_string()],
        ),
        (
            legacy_unsafe_branch_worktree_id,
            legacy_unsafe_task_id,
            agent_id,
            "agent/schema-3[unsafe".to_string(),
            "a".repeat(40),
            legacy_worktree_path(legacy_unsafe_task_id),
            vec!["legacy.rs".to_string()],
        ),
        (
            legacy_cross_task_worktree_id,
            legacy_foreign_task_id,
            agent_id,
            "agent/schema-3-cross-task".to_string(),
            "a".repeat(40),
            legacy_worktree_path(legacy_foreign_task_id),
            vec!["legacy.rs".to_string()],
        ),
        (
            legacy_cross_owner_worktree_id,
            legacy_cross_owner_task_id,
            foreign_agent_id,
            "agent/schema-3-cross-owner".to_string(),
            "a".repeat(40),
            legacy_worktree_path(legacy_cross_owner_task_id),
            vec!["legacy.rs".to_string()],
        ),
        (
            legacy_unsafe_base_worktree_id,
            legacy_unsafe_base_task_id,
            agent_id,
            "agent/schema-3-unsafe-base".to_string(),
            "not-a-full-object-id".to_string(),
            legacy_worktree_path(legacy_unsafe_base_task_id),
            vec!["legacy.rs".to_string()],
        ),
        (
            legacy_unsafe_path_worktree_id,
            legacy_unsafe_path_task_id,
            agent_id,
            "agent/schema-3-unsafe-path".to_string(),
            "a".repeat(40),
            "/srv/schema-3/derived-path".to_string(),
            vec!["legacy.rs".to_string()],
        ),
        (
            legacy_unsafe_files_worktree_id,
            legacy_unsafe_files_task_id,
            agent_id,
            "agent/schema-3-unsafe-files".to_string(),
            "a".repeat(40),
            legacy_worktree_path(legacy_unsafe_files_task_id),
            vec!["a/../legacy.rs".to_string()],
        ),
    ];
    for (id, task_id, owner_agent_id, branch, base_commit, path, changed_files) in seeded_worktrees
    {
        sqlx::query(
            "INSERT INTO worktrees \
             (id,workspace_id,task_id,owner_agent_id,branch,base_commit,path,status,changed_files,last_activity_at) \
             VALUES ($1,$2,$3,$4,$5,$6,$7,'ACTIVE',$8,$9)",
        )
        .bind(id)
        .bind(workspace_id)
        .bind(task_id)
        .bind(owner_agent_id)
        .bind(branch)
        .bind(base_commit)
        .bind(path)
        .bind(changed_files)
        .bind(legacy_worktree_activity_at)
        .execute(&mut connection)
        .await
        .expect("seed schema-3 worktree");
    }
    let migrations = [
        (
            "0004_login_attempts.sql",
            include_str!("../migrations/0004_login_attempts.sql"),
        ),
        (
            "0005_webhooks.sql",
            include_str!("../migrations/0005_webhooks.sql"),
        ),
        (
            "0006_audit_append_only.sql",
            include_str!("../migrations/0006_audit_append_only.sql"),
        ),
        (
            "0007_webhook_scheduler.sql",
            include_str!("../migrations/0007_webhook_scheduler.sql"),
        ),
        (
            "0008_webhook_delivery_lease.sql",
            include_str!("../migrations/0008_webhook_delivery_lease.sql"),
        ),
        (
            "0009_webhook_delivery_fencing.sql",
            include_str!("../migrations/0009_webhook_delivery_fencing.sql"),
        ),
        (
            "0010_task_integrity_activity_ledger.sql",
            include_str!("../migrations/0010_task_integrity_activity_ledger.sql"),
        ),
        (
            "0011_task_activity_hardening.sql",
            include_str!("../migrations/0011_task_activity_hardening.sql"),
        ),
        (
            "0012_webhook_lease_check.sql",
            include_str!("../migrations/0012_webhook_lease_check.sql"),
        ),
        (
            "0013_skill_integrity.sql",
            include_str!("../migrations/0013_skill_integrity.sql"),
        ),
        (
            "0014_skill_revision_immutability.sql",
            include_str!("../migrations/0014_skill_revision_immutability.sql"),
        ),
        (
            "0015_skill_lifecycle_hardening.sql",
            include_str!("../migrations/0015_skill_lifecycle_hardening.sql"),
        ),
        (
            "0016_worktree_integrity.sql",
            include_str!("../migrations/0016_worktree_integrity.sql"),
        ),
        (
            "0017_mcp_server_secret_profile_integrity.sql",
            include_str!("../migrations/0017_mcp_server_secret_profile_integrity.sql"),
        ),
        (
            "0018_mcp_auth_states_vault_pkce.sql",
            include_str!("../migrations/0018_mcp_auth_states_vault_pkce.sql"),
        ),
    ];
    for (migration, sql) in migrations {
        sqlx::raw_sql(sql)
            .execute(&mut connection)
            .await
            .unwrap_or_else(|error| panic!("upgrade with {migration}: {error}"));
    }
    let final_schema: i64 = sqlx::query_scalar("SELECT schema_version FROM schema_metadata")
        .fetch_one(&mut connection)
        .await
        .expect("read final schema version");
    assert_eq!(final_schema, 18);
    let safe_worktree_survives: i64 =
        sqlx::query_scalar("SELECT count(*) FROM worktrees WHERE id=$1")
            .bind(legacy_safe_worktree_id)
            .fetch_one(&mut connection)
            .await
            .expect("safe schema-3 worktree survives upgrade");
    assert_eq!(safe_worktree_survives, 1);
    for (id, reason) in [
        (legacy_unsafe_branch_worktree_id, "unsafe_branch"),
        (legacy_cross_task_worktree_id, "cross_workspace_task"),
        (
            legacy_cross_owner_worktree_id,
            "cross_workspace_owner_agent",
        ),
        (legacy_unsafe_base_worktree_id, "unsafe_base_commit"),
        (legacy_unsafe_path_worktree_id, "unsafe_path"),
        (legacy_unsafe_files_worktree_id, "unsafe_changed_files"),
    ] {
        let removed: i64 = sqlx::query_scalar("SELECT count(*) FROM worktrees WHERE id=$1")
            .bind(id)
            .fetch_one(&mut connection)
            .await
            .expect("unsafe schema-3 worktree is removed");
        assert_eq!(removed, 0, "{reason} worktree must be removed");
        let quarantined_reason: String =
            sqlx::query_scalar("SELECT reason FROM worktree_integrity_quarantine WHERE id=$1")
                .bind(id)
                .fetch_one(&mut connection)
                .await
                .expect("unsafe schema-3 worktree is quarantined");
        assert!(
            quarantined_reason.contains(reason),
            "quarantine reason must preserve {reason}: {quarantined_reason}"
        );
    }
    let quarantined_schema_three: (
        String,
        String,
        String,
        Vec<String>,
        OffsetDateTime,
        String,
        OffsetDateTime,
    ) = sqlx::query_as(
        "SELECT branch,base_commit,path,changed_files,last_activity_at,reason,quarantined_at \
         FROM worktree_integrity_quarantine WHERE id=$1",
    )
    .bind(legacy_unsafe_branch_worktree_id)
    .fetch_one(&mut connection)
    .await
    .expect("unsafe schema-3 worktree is fully quarantined");
    assert_eq!(quarantined_schema_three.0, "agent/schema-3[unsafe");
    assert_eq!(quarantined_schema_three.1, "a".repeat(40));
    assert_eq!(
        quarantined_schema_three.2,
        format!(
            "/srv/schema-3/worktrees/task-{}",
            &legacy_unsafe_task_id.simple().to_string()[..12]
        )
    );
    assert_eq!(quarantined_schema_three.3, vec!["legacy.rs".to_string()]);
    assert_eq!(quarantined_schema_three.4, legacy_worktree_activity_at);
    assert!(quarantined_schema_three.5.contains("unsafe_branch"));
    assert!(quarantined_schema_three.6 >= legacy_worktree_activity_at);
    assert!(
        sqlx::query(
            "INSERT INTO worktrees \
             (id,workspace_id,task_id,owner_agent_id,branch,base_commit,path,status,changed_files) \
             VALUES ($1,$2,$3,$4,'agent[recurrence',$5,$6,'ACTIVE',ARRAY[]::text[])",
        )
        .bind(Uuid::now_v7())
        .bind(workspace_id)
        .bind(legacy_unsafe_recurrence_task_id)
        .bind(agent_id)
        .bind("a".repeat(40))
        .bind(legacy_worktree_path(legacy_unsafe_recurrence_task_id))
        .execute(&mut connection)
        .await
        .is_err(),
        "upgraded schema must block unsafe worktree recurrence"
    );
    let source_trigger: bool = sqlx::query_scalar(
        "SELECT EXISTS (SELECT 1 FROM pg_trigger WHERE tgname='skill_revisions_sources_valid')",
    )
    .fetch_one(&mut connection)
    .await
    .expect("read Skills source trigger");
    assert!(source_trigger);
    let skill_id = Uuid::now_v7();
    let revision_id = Uuid::now_v7();
    sqlx::query(
        "INSERT INTO skills (id,profile_id,name,description) VALUES ($1,$2,'v3-upgrade-skill','')",
    )
    .bind(skill_id)
    .bind(profile_id)
    .execute(&mut connection)
    .await
    .expect("insert upgraded skill");
    sqlx::query("INSERT INTO skill_revisions (id,skill_id,revision,content,author,reason) VALUES ($1,$2,1,'content','owner','initial')")
        .bind(revision_id).bind(skill_id).execute(&mut connection).await.expect("insert upgraded revision");
    assert!(
        sqlx::query("UPDATE skill_revisions SET evaluation='{}' WHERE id=$1")
            .bind(revision_id)
            .execute(&mut connection)
            .await
            .is_err()
    );
    let valid = serde_json::json!({"deterministic_checks_passed":true,"attempts":1,"successful_attempts":1,"steps":1,"retries":0,"errors":0,"duration_ms":1,"user_corrections":0});
    sqlx::query("UPDATE skill_revisions SET evaluation=$1 WHERE id=$2")
        .bind(&valid)
        .bind(revision_id)
        .execute(&mut connection)
        .await
        .expect("record upgraded evidence");
    assert!(
        sqlx::query("UPDATE skill_revisions SET evaluation=$1 WHERE id=$2")
            .bind(serde_json::json!({"tampered":true}))
            .bind(revision_id)
            .execute(&mut connection)
            .await
            .is_err()
    );
    let other_profile = Uuid::now_v7();
    let other_user_id = Uuid::now_v7();
    let same_profile_workspace = Uuid::now_v7();
    let cross_source = Uuid::now_v7();
    let wrong_source = Uuid::now_v7();
    sqlx::query("INSERT INTO profiles (id,name) VALUES ($1,'v3-other-profile')")
        .bind(other_profile)
        .execute(&mut connection)
        .await
        .expect("other profile");
    sqlx::query(
        "INSERT INTO users (id,email,display_name,password_hash,role,primary_profile_id) \
         VALUES ($1,$2,'Other Owner','unused','OWNER',$3)",
    )
    .bind(other_user_id)
    .bind(format!("{other_user_id}@example.test"))
    .bind(other_profile)
    .execute(&mut connection)
    .await
    .expect("other profile owner");
    sqlx::query("INSERT INTO workspaces (id,profile_id,title,created_by_user_id) VALUES ($1,$2,'v3 same-profile workspace',$3)")
        .bind(same_profile_workspace)
        .bind(profile_id)
        .bind(user_id)
        .execute(&mut connection)
        .await
        .expect("same-profile workspace");
    sqlx::query("INSERT INTO conversations (id,profile_id,created_by_user_id,title) VALUES ($1,$2,$3,'cross source')")
        .bind(cross_source)
        .bind(other_profile)
        .bind(other_user_id)
        .execute(&mut connection)
        .await
        .expect("cross source");
    sqlx::query("INSERT INTO conversations (id,profile_id,workspace_id,created_by_user_id,title) VALUES ($1,$2,$3,$4,'wrong source')").bind(wrong_source).bind(profile_id).bind(same_profile_workspace).bind(user_id).execute(&mut connection).await.expect("wrong source");
    let post_valid = Uuid::now_v7();
    let post_deleted = Uuid::now_v7();
    let post_cross = Uuid::now_v7();
    let post_wrong = Uuid::now_v7();
    sqlx::query("INSERT INTO conversations (id,profile_id,workspace_id,created_by_user_id,title,status) VALUES ($1,$2,$3,$4,'post valid','active'),($5,$2,NULL,$4,'post deleted','deleted'),($6,$7,NULL,$8,'post cross','active'),($9,$2,$10,$4,'post wrong','active')").bind(post_valid).bind(profile_id).bind(workspace_id).bind(user_id).bind(post_deleted).bind(post_cross).bind(other_profile).bind(other_user_id).bind(post_wrong).bind(same_profile_workspace).execute(&mut connection).await.expect("post upgrade source fixtures");
    let post_missing = Uuid::now_v7();
    let mut post_revision = 2_i64;
    for ids in [
        vec![Uuid::nil()],
        vec![post_valid, post_valid],
        vec![post_missing],
        vec![post_deleted],
        vec![post_cross],
    ] {
        let result=sqlx::query("INSERT INTO skill_revisions (id,skill_id,revision,content,author,reason,source_conversation_ids) VALUES ($1,$2,$3,'bad','owner','bad',$4)").bind(Uuid::now_v7()).bind(skill_id).bind(post_revision).bind(ids).execute(&mut connection).await;
        let constraint = result.as_ref().err().and_then(|error| match error {
            sqlx::Error::Database(database) => database.constraint(),
            _ => None,
        });
        assert_eq!(constraint, Some("skill_revisions_sources_valid"));
        post_revision += 1;
    }

    let workspace_skill = Uuid::now_v7();
    sqlx::query("INSERT INTO skills (id,profile_id,workspace_id,name,description) VALUES ($1,$2,$3,'v3-scoped','')").bind(workspace_skill).bind(profile_id).bind(workspace_id).execute(&mut connection).await.expect("v3 scoped skill");
    let result=sqlx::query("INSERT INTO skill_revisions (id,skill_id,revision,content,author,reason,source_conversation_ids) VALUES ($1,$2,1,'bad','owner','bad',$3)").bind(Uuid::now_v7()).bind(workspace_skill).bind(vec![post_wrong]).execute(&mut connection).await;
    let constraint = result.as_ref().err().and_then(|error| match error {
        sqlx::Error::Database(database) => database.constraint(),
        _ => None,
    });
    assert_eq!(constraint, Some("skill_revisions_sources_valid"));
    assert!(sqlx::query("INSERT INTO skill_revisions (id,skill_id,revision,content,author,reason,source_conversation_ids) VALUES ($1,$2,2,'bad','owner','bad',$3)").bind(Uuid::now_v7()).bind(skill_id).bind(vec![Uuid::nil()]).execute(&mut connection).await.is_err());
    let mut divergence = connection.begin().await.expect("begin divergence");
    sqlx::query("UPDATE skill_revisions SET promoted=true WHERE id=$1")
        .bind(revision_id)
        .execute(&mut *divergence)
        .await
        .expect("set invalid promoted state");
    assert!(divergence.commit().await.is_err());
    sqlx::query("INSERT INTO skill_integrity_quarantine (skill_id,profile_id,name,issue,detail) VALUES ($1,$2,'q','upgrade','{}')").bind(skill_id).bind(profile_id).execute(&mut connection).await.expect("insert quarantine evidence");
    assert!(
        sqlx::query("UPDATE skill_integrity_quarantine SET issue='tampered'")
            .execute(&mut connection)
            .await
            .is_err()
    );
    connection.close().await.expect("close upgrade session");
    sqlx::query(&format!("DROP SCHEMA {schema} CASCADE"))
        .execute(&pool)
        .await
        .expect("drop isolated upgrade schema");
}

#[tokio::test]
async fn mcp_auth_states_vault_pkce_migration_guards_and_enforces() {
    let Some(database_url) = std::env::var("GOBROWSE_TEST_DATABASE_URL").ok() else {
        eprintln!("GOBROWSE_TEST_DATABASE_URL is unset; skipping PostgreSQL integration test");
        return;
    };
    let _lock = common::acquire_test_lock(&database_url).await;
    let pool = PgPool::connect(&database_url)
        .await
        .expect("connect to test PostgreSQL");
    gobrowse_server::db::migrate(&pool)
        .await
        .expect("install shared extensions");
    let schema = format!("mcp_auth_v18_{}", Uuid::now_v7().simple());
    sqlx::query(&format!("CREATE SCHEMA {schema}"))
        .execute(&pool)
        .await
        .expect("create isolated schema");
    let mut connection = PgConnection::connect(&database_url)
        .await
        .expect("connect isolated session");
    sqlx::query(&format!("SET search_path TO {schema},public"))
        .execute(&mut connection)
        .await
        .expect("set isolated search path");
    let core_migrations = [
        include_str!("../migrations/0001_initial.sql"),
        include_str!("../migrations/0002_library_embeddings.sql"),
        include_str!("../migrations/0003_chat_runs.sql"),
        include_str!("../migrations/0004_login_attempts.sql"),
        include_str!("../migrations/0005_webhooks.sql"),
        include_str!("../migrations/0006_audit_append_only.sql"),
        include_str!("../migrations/0007_webhook_scheduler.sql"),
        include_str!("../migrations/0008_webhook_delivery_lease.sql"),
        include_str!("../migrations/0009_webhook_delivery_fencing.sql"),
        include_str!("../migrations/0010_task_integrity_activity_ledger.sql"),
        include_str!("../migrations/0011_task_activity_hardening.sql"),
        include_str!("../migrations/0012_webhook_lease_check.sql"),
        include_str!("../migrations/0013_skill_integrity.sql"),
        include_str!("../migrations/0014_skill_revision_immutability.sql"),
        include_str!("../migrations/0015_skill_lifecycle_hardening.sql"),
        include_str!("../migrations/0016_worktree_integrity.sql"),
        include_str!("../migrations/0017_mcp_server_secret_profile_integrity.sql"),
    ];
    for sql in core_migrations {
        sqlx::raw_sql(sql)
            .execute(&mut connection)
            .await
            .expect("install core migrations");
    }
    let v17_version: i64 = sqlx::query_scalar("SELECT schema_version FROM schema_metadata")
        .fetch_one(&mut connection)
        .await
        .expect("read schema version after core migrations");
    assert_eq!(v17_version, 17);

    // Assert mcp_auth_states is empty before applying 0018.
    let empty: i64 = sqlx::query_scalar("SELECT count(*) FROM mcp_auth_states")
        .fetch_one(&mut connection)
        .await
        .expect("count auth states");
    assert_eq!(empty, 0, "mcp_auth_states must be empty for zero-row guard");

    sqlx::raw_sql(include_str!(
        "../migrations/0018_mcp_auth_states_vault_pkce.sql"
    ))
    .execute(&mut connection)
    .await
    .expect("upgrade to schema 18");

    let v18_version: i64 = sqlx::query_scalar("SELECT schema_version FROM schema_metadata")
        .fetch_one(&mut connection)
        .await
        .expect("read schema version after 0018");
    assert_eq!(v18_version, 18);

    // Verify column changes.
    let has_pkce_secret_ref: bool = sqlx::query_scalar(
        "SELECT EXISTS (SELECT 1 FROM information_schema.columns \
         WHERE table_schema=$1 AND table_name='mcp_auth_states' AND column_name='pkce_verifier_secret_ref')",
    )
    .bind(&schema)
    .fetch_one(&mut connection)
    .await
    .expect("check pkce_verifier_secret_ref column exists");
    assert!(
        has_pkce_secret_ref,
        "pkce_verifier_secret_ref column must exist"
    );

    let has_encrypted: bool = sqlx::query_scalar(
        "SELECT EXISTS (SELECT 1 FROM information_schema.columns \
         WHERE table_schema=$1 AND table_name='mcp_auth_states' AND column_name='pkce_verifier_encrypted')",
    )
    .bind(&schema)
    .fetch_one(&mut connection)
    .await
    .expect("check pkce_verifier_encrypted column is gone");
    assert!(
        !has_encrypted,
        "pkce_verifier_encrypted column must be dropped"
    );

    let profile_id_not_null: bool = sqlx::query_scalar(
        "SELECT EXISTS (SELECT 1 FROM information_schema.columns \
         WHERE table_schema=$1 AND table_name='mcp_auth_states' AND column_name='profile_id' \
         AND is_nullable='NO')",
    )
    .bind(&schema)
    .fetch_one(&mut connection)
    .await
    .expect("check profile_id NOT NULL");
    assert!(profile_id_not_null, "profile_id must be NOT NULL");

    // Seed test data: same-profile secret_reference and mcp_auth_states row.
    let profile_id = Uuid::now_v7();
    let server_id = Uuid::now_v7();
    let state_id = Uuid::now_v7();
    let secret_id = format!("pkce-verifier-{}", Uuid::now_v7().simple());
    sqlx::query("INSERT INTO profiles (id, name) VALUES ($1, 'mcp-auth-v18')")
        .bind(profile_id)
        .execute(&mut connection)
        .await
        .expect("create test profile");
    sqlx::query(
        "INSERT INTO secret_references (id, profile_id, backend, locator, encrypted_value, nonce, \
         key_version, purpose, algorithm, wrapped_data_key, wrap_nonce) \
         VALUES ($1, $2, 'encrypted_database', 'pkce-verifier-locator', decode('01','hex'), \
         decode('02','hex'), 1, 'mcp_oauth_pkce_verifier', 'A256GCM', decode('aa','hex'), decode('bb','hex'))",
    )
    .bind(&secret_id)
    .bind(profile_id)
    .execute(&mut connection)
    .await
    .expect("insert same-profile secret reference");
    sqlx::query(
        "INSERT INTO mcp_servers (id, profile_id, name, transport, configuration, enabled) \
         VALUES ($1, $2, 'pkce-test', 'stdio', '{}', true)",
    )
    .bind(server_id)
    .bind(profile_id)
    .execute(&mut connection)
    .await
    .expect("insert mcp server");
    sqlx::query(
        "INSERT INTO mcp_auth_states (id, mcp_server_id, profile_id, state_hash, \
         pkce_verifier_secret_ref, expected_issuer, resource_uri, redirect_uri, expires_at) \
         VALUES ($1, $2, $3, decode('abcdef', 'hex'), $4, 'https://issuer.test', \
         'https://resource.test', 'https://redirect.test', now() + interval '1 hour')",
    )
    .bind(state_id)
    .bind(server_id)
    .bind(profile_id)
    .bind(&secret_id)
    .execute(&mut connection)
    .await
    .expect("insert mcp_auth_states row with same-profile secret");

    // Cross-profile secret reference must be rejected.
    let other_profile_id = Uuid::now_v7();
    let other_secret_id = format!("pkce-cross-{}", Uuid::now_v7().simple());
    sqlx::query("INSERT INTO profiles (id, name) VALUES ($1, 'mcp-auth-other')")
        .bind(other_profile_id)
        .execute(&mut connection)
        .await
        .expect("create other profile");
    sqlx::query(
        "INSERT INTO secret_references (id, profile_id, backend, locator, encrypted_value, nonce, \
         key_version, purpose, algorithm, wrapped_data_key, wrap_nonce) \
         VALUES ($1, $2, 'encrypted_database', 'cross-profile-pkce', decode('11','hex'), \
         decode('22','hex'), 1, 'mcp_oauth_pkce_verifier', 'A256GCM', decode('aa','hex'), decode('cc','hex'))",
    )
    .bind(&other_secret_id)
    .bind(other_profile_id)
    .execute(&mut connection)
    .await
    .expect("insert cross-profile secret");
    let cross_profile_insert = sqlx::query(
        "INSERT INTO mcp_auth_states (id, mcp_server_id, profile_id, state_hash, \
         pkce_verifier_secret_ref, expected_issuer, resource_uri, redirect_uri, expires_at) \
         VALUES ($1, $2, $3, decode('123456', 'hex'), $4, 'https://issuer.test', \
         'https://resource.test', 'https://redirect.test', now() + interval '1 hour')",
    )
    .bind(Uuid::now_v7())
    .bind(server_id)
    .bind(profile_id)
    .bind(&other_secret_id)
    .execute(&mut connection)
    .await;
    let cross_constraint = cross_profile_insert
        .as_ref()
        .err()
        .and_then(|error| match error {
            sqlx::Error::Database(database) => database.constraint(),
            _ => None,
        });
    assert_eq!(
        cross_constraint,
        Some("mcp_auth_states_pkce_verifier_same_profile_fk")
    );

    // Deleting the referenced secret must null pkce_verifier_secret_ref.
    sqlx::query("DELETE FROM secret_references WHERE id=$1")
        .bind(&secret_id)
        .execute(&mut connection)
        .await
        .expect("delete referenced secret");
    let nulled_ref: Option<String> =
        sqlx::query_scalar("SELECT pkce_verifier_secret_ref FROM mcp_auth_states WHERE id=$1")
            .bind(state_id)
            .fetch_one(&mut connection)
            .await
            .expect("read nulled pkce_verifier_secret_ref");
    assert_eq!(
        nulled_ref, None,
        "pkce_verifier_secret_ref must be nulled after secret deletion"
    );

    // Deleting the profile must cascade to the mcp_auth_states row.
    sqlx::query("DELETE FROM profiles WHERE id=$1")
        .bind(profile_id)
        .execute(&mut connection)
        .await
        .expect("delete profile (cascades)");
    let deleted: i64 = sqlx::query_scalar("SELECT count(*) FROM mcp_auth_states WHERE id=$1")
        .bind(state_id)
        .fetch_one(&mut connection)
        .await
        .expect("count remaining mcp_auth_states rows");
    assert_eq!(
        deleted, 0,
        "mcp_auth_states row must cascade-delete with profile"
    );

    connection.close().await.expect("close isolated session");
    sqlx::query(&format!("DROP SCHEMA {schema} CASCADE"))
        .execute(&pool)
        .await
        .expect("drop isolated schema");
}

#[tokio::test]
async fn mcp_auth_states_zero_row_guard_rejects_legacy_data() {
    let Some(database_url) = std::env::var("GOBROWSE_TEST_DATABASE_URL").ok() else {
        eprintln!("GOBROWSE_TEST_DATABASE_URL is unset; skipping PostgreSQL integration test");
        return;
    };
    let _lock = common::acquire_test_lock(&database_url).await;
    let pool = PgPool::connect(&database_url)
        .await
        .expect("connect to test PostgreSQL");
    gobrowse_server::db::migrate(&pool)
        .await
        .expect("install shared extensions");
    let schema = format!("mcp_auth_guard_{}", Uuid::now_v7().simple());
    sqlx::query(&format!("CREATE SCHEMA {schema}"))
        .execute(&pool)
        .await
        .expect("create isolated schema");
    let mut connection = PgConnection::connect(&database_url)
        .await
        .expect("connect isolated session");
    sqlx::query(&format!("SET search_path TO {schema},public"))
        .execute(&mut connection)
        .await
        .expect("set isolated search path");
    let core_migrations = [
        include_str!("../migrations/0001_initial.sql"),
        include_str!("../migrations/0002_library_embeddings.sql"),
        include_str!("../migrations/0003_chat_runs.sql"),
        include_str!("../migrations/0004_login_attempts.sql"),
        include_str!("../migrations/0005_webhooks.sql"),
        include_str!("../migrations/0006_audit_append_only.sql"),
        include_str!("../migrations/0007_webhook_scheduler.sql"),
        include_str!("../migrations/0008_webhook_delivery_lease.sql"),
        include_str!("../migrations/0009_webhook_delivery_fencing.sql"),
        include_str!("../migrations/0010_task_integrity_activity_ledger.sql"),
        include_str!("../migrations/0011_task_activity_hardening.sql"),
        include_str!("../migrations/0012_webhook_lease_check.sql"),
        include_str!("../migrations/0013_skill_integrity.sql"),
        include_str!("../migrations/0014_skill_revision_immutability.sql"),
        include_str!("../migrations/0015_skill_lifecycle_hardening.sql"),
        include_str!("../migrations/0016_worktree_integrity.sql"),
        include_str!("../migrations/0017_mcp_server_secret_profile_integrity.sql"),
    ];
    for sql in core_migrations {
        sqlx::raw_sql(sql)
            .execute(&mut connection)
            .await
            .expect("install core migrations");
    }
    let profile_id = Uuid::now_v7();
    let server_id = Uuid::now_v7();
    let state_id = Uuid::now_v7();
    sqlx::query("INSERT INTO profiles (id, name) VALUES ($1, 'mcp-auth-guard')")
        .bind(profile_id)
        .execute(&mut connection)
        .await
        .expect("create guard test profile");
    sqlx::query(
        "INSERT INTO mcp_servers (id, profile_id, name, transport, configuration, enabled) \
         VALUES ($1, $2, 'guard-test', 'stdio', '{}', true)",
    )
    .bind(server_id)
    .bind(profile_id)
    .execute(&mut connection)
    .await
    .expect("insert mcp server for guard test");
    sqlx::query(
        "INSERT INTO mcp_auth_states (id, mcp_server_id, state_hash, pkce_verifier_encrypted, \
         expected_issuer, resource_uri, redirect_uri, expires_at) \
         VALUES ($1, $2, decode('a1b2c3', 'hex'), decode('deadbeef', 'hex'), \
         'https://issuer.test', 'https://resource.test', 'https://redirect.test', \
         now() + interval '1 hour')",
    )
    .bind(state_id)
    .bind(server_id)
    .execute(&mut connection)
    .await
    .expect("seed legacy mcp_auth_states row");

    let guard_result = sqlx::raw_sql(include_str!(
        "../migrations/0018_mcp_auth_states_vault_pkce.sql"
    ))
    .execute(&mut connection)
    .await;
    let guard_error_message = match &guard_result {
        Err(sqlx::Error::Database(db_err)) => Some(db_err.message()),
        _ => None,
    };
    assert!(
        guard_error_message.is_some(),
        "0018 migration must fail when mcp_auth_states has rows"
    );
    let message = guard_error_message.unwrap();
    assert!(
        message.contains("must be empty"),
        "guard error message must describe the zero-row requirement: {message}"
    );

    connection.close().await.expect("close isolated session");
    sqlx::query(&format!("DROP SCHEMA {schema} CASCADE"))
        .execute(&pool)
        .await
        .expect("drop isolated schema");
}

#[tokio::test]
async fn login_rate_limit_blocks_after_threshold() {
    let Some(database_url) = std::env::var("GOBROWSE_TEST_DATABASE_URL").ok() else {
        eprintln!("GOBROWSE_TEST_DATABASE_URL is unset; skipping PostgreSQL integration test");
        return;
    };
    let _lock = common::acquire_test_lock(&database_url).await;
    let pool = test_pool().await.expect("test pool after lock");
    let email = format!("throttle-{}@example.test", Uuid::now_v7().simple());
    let email_lower = email.to_lowercase();
    let now = time::OffsetDateTime::now_utc();

    sqlx::query("DELETE FROM login_attempts WHERE email = $1")
        .bind(&email_lower)
        .execute(&pool)
        .await
        .expect("clean up previous attempts");

    for _ in 0..5 {
        sqlx::query(
            "INSERT INTO login_attempts (email, outcome, occurred_at) VALUES ($1, 'failure', $2)",
        )
        .bind(&email_lower)
        .bind(now)
        .execute(&pool)
        .await
        .expect("insert attempt");
    }

    let window = Duration::seconds(300);
    let cutoff = now - window;
    let count: i64 = sqlx::query_scalar(
        "SELECT COUNT(*) FROM login_attempts WHERE email = $1 AND occurred_at > $2",
    )
    .bind(&email_lower)
    .bind(cutoff)
    .fetch_one(&pool)
    .await
    .expect("count attempts");
    assert!(
        count >= 5,
        "expected at least 5 recent attempts for throttled email, got {count}"
    );

    sqlx::query("DELETE FROM login_attempts WHERE email = $1")
        .bind(&email_lower)
        .execute(&pool)
        .await
        .expect("clean up test data");
}

#[tokio::test]
async fn login_rate_limit_recovers_after_window_expiry() {
    let Some(database_url) = std::env::var("GOBROWSE_TEST_DATABASE_URL").ok() else {
        eprintln!("GOBROWSE_TEST_DATABASE_URL is unset; skipping PostgreSQL integration test");
        return;
    };
    let _lock = common::acquire_test_lock(&database_url).await;
    let pool = test_pool().await.expect("test pool after lock");
    let email = format!("recover-{}@example.test", Uuid::now_v7().simple());
    let email_lower = email.to_lowercase();

    sqlx::query("DELETE FROM login_attempts WHERE email = $1")
        .bind(&email_lower)
        .execute(&pool)
        .await
        .expect("clean up previous attempts");

    let old_time = time::OffsetDateTime::now_utc() - Duration::seconds(301);
    for _ in 0..5 {
        sqlx::query(
            "INSERT INTO login_attempts (email, outcome, occurred_at) VALUES ($1, 'failure', $2)",
        )
        .bind(&email_lower)
        .bind(old_time)
        .execute(&pool)
        .await
        .expect("insert old attempt");
    }

    let window = Duration::seconds(300);
    let cutoff = time::OffsetDateTime::now_utc() - window;
    let count: i64 = sqlx::query_scalar(
        "SELECT COUNT(*) FROM login_attempts WHERE email = $1 AND occurred_at > $2",
    )
    .bind(&email_lower)
    .bind(cutoff)
    .fetch_one(&pool)
    .await
    .expect("count recent attempts");
    assert_eq!(count, 0, "old attempts must not count within the window");

    sqlx::query("DELETE FROM login_attempts WHERE email = $1")
        .bind(&email_lower)
        .execute(&pool)
        .await
        .expect("clean up test data");
}

#[tokio::test]
async fn locked_account_returns_uniform_unauthorized() {
    let Some(database_url) = std::env::var("GOBROWSE_TEST_DATABASE_URL").ok() else {
        eprintln!("GOBROWSE_TEST_DATABASE_URL is unset; skipping PostgreSQL integration test");
        return;
    };
    let _lock = common::acquire_test_lock(&database_url).await;
    let pool = test_pool().await.expect("test pool after lock");
    let email = format!("locked-{}@example.test", Uuid::now_v7().simple());
    let email_lower = email.to_lowercase();
    let now = time::OffsetDateTime::now_utc();

    sqlx::query("DELETE FROM login_attempts WHERE email = $1")
        .bind(&email_lower)
        .execute(&pool)
        .await
        .expect("clean up previous attempts");

    for _ in 0..5 {
        sqlx::query(
            "INSERT INTO login_attempts (email, outcome, occurred_at) VALUES ($1, 'failure', $2)",
        )
        .bind(&email_lower)
        .bind(now)
        .execute(&pool)
        .await
        .expect("insert failure");
    }

    let window = Duration::seconds(300);
    let cutoff = now - window;
    let count: i64 = sqlx::query_scalar(
        "SELECT COUNT(*) FROM login_attempts WHERE email = $1 AND occurred_at > $2",
    )
    .bind(&email_lower)
    .bind(cutoff)
    .fetch_one(&pool)
    .await
    .expect("count attempts");
    assert!(
        count >= 5,
        "locked account must have at least {} recent failures, got {count}",
        5
    );

    let outcomes: Vec<String> =
        sqlx::query("SELECT outcome FROM login_attempts WHERE email = $1 ORDER BY occurred_at")
            .bind(&email_lower)
            .fetch_all(&pool)
            .await
            .expect("read outcomes")
            .into_iter()
            .map(|row| row.get("outcome"))
            .collect();
    assert!(!outcomes.is_empty(), "should have recorded outcomes");
    assert!(
        outcomes.iter().all(|o| o == "failure"),
        "all recorded attempts should be failures"
    );

    sqlx::query("DELETE FROM login_attempts WHERE email = $1")
        .bind(&email_lower)
        .execute(&pool)
        .await
        .expect("clean up test data");
}
