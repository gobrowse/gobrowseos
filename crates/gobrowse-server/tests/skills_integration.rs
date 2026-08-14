mod common;

use gobrowse_core::skills::SkillEvaluation;
use sqlx::{PgPool, Row};
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
    sqlx::query("UPDATE skill_revisions SET promoted=true WHERE skill_id=$1 AND revision=1")
        .bind(skill_id)
        .execute(&pool)
        .await
        .expect("promote first revision");
    sqlx::query("UPDATE skill_revisions SET promoted=false WHERE skill_id=$1 AND promoted")
        .bind(skill_id)
        .execute(&pool)
        .await
        .expect("demote old revision");
    sqlx::query("UPDATE skill_revisions SET promoted=true WHERE skill_id=$1 AND revision=2")
        .bind(skill_id)
        .execute(&pool)
        .await
        .expect("promote second revision");
    sqlx::query("UPDATE skills SET active_revision=2 WHERE id=$1")
        .bind(skill_id)
        .execute(&pool)
        .await
        .expect("activate second revision");
    sqlx::query("UPDATE skill_revisions SET promoted=false WHERE skill_id=$1 AND promoted")
        .bind(skill_id)
        .execute(&pool)
        .await
        .expect("rollback demotion");
    sqlx::query("UPDATE skill_revisions SET promoted=true WHERE skill_id=$1 AND revision=1")
        .bind(skill_id)
        .execute(&pool)
        .await
        .expect("rollback promotion");
    sqlx::query("UPDATE skills SET active_revision=1 WHERE id=$1")
        .bind(skill_id)
        .execute(&pool)
        .await
        .expect("rollback active pointer");
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
