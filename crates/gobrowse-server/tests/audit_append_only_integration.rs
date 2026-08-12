mod common;

use sqlx::PgPool;
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

/// Insert an audit row (mirroring gobrowse_server::auth::audit columns)
/// and return the generated sequence value so tests can target it.
async fn seed_audit_row(pool: &PgPool) -> (Uuid, Uuid, i64) {
    let profile_id = Uuid::now_v7();
    let user_id = Uuid::now_v7();

    sqlx::query("INSERT INTO profiles (id, name) VALUES ($1, 'audit-test-profile')")
        .bind(profile_id)
        .execute(pool)
        .await
        .expect("create test profile");

    sqlx::query(
        "INSERT INTO users (id, email, display_name, password_hash, role, primary_profile_id) \
         VALUES ($1, $2, 'Audit Tester', 'unused', 'OWNER', $3)",
    )
    .bind(user_id)
    .bind(format!("{user_id}@audit.test"))
    .bind(profile_id)
    .execute(pool)
    .await
    .expect("create test user");

    sqlx::query(
        "INSERT INTO audit_events \
         (actor_user_id, profile_id, action, resource_type, resource_id, outcome) \
         VALUES ($1, $2, 'test.action', 'test_type', 'res-1', 'success')",
    )
    .bind(user_id)
    .bind(profile_id)
    .execute(pool)
    .await
    .expect("insert audit row");

    let seq: i64 = sqlx::query_scalar(
        "SELECT sequence FROM audit_events WHERE profile_id = $1 ORDER BY sequence DESC LIMIT 1",
    )
    .bind(profile_id)
    .fetch_one(pool)
    .await
    .expect("fetch audit sequence");

    (profile_id, user_id, seq)
}

#[tokio::test]
async fn audit_events_rejects_update() {
    let Some(database_url) = std::env::var("GOBROWSE_TEST_DATABASE_URL").ok() else {
        eprintln!("GOBROWSE_TEST_DATABASE_URL is unset; skipping test");
        return;
    };
    let _lock = common::acquire_test_lock(&database_url).await;
    let pool = test_pool().await.expect("test pool after lock");
    let (_profile_id, _user_id, seq) = seed_audit_row(&pool).await;

    // Run UPDATE inside a transaction we always roll back.
    let mut tx = pool.begin().await.expect("begin tx");
    let result = sqlx::query("UPDATE audit_events SET outcome = 'tampered' WHERE sequence = $1")
        .bind(seq)
        .execute(&mut *tx)
        .await;
    if let Err(e) = &result {
        let msg = e.to_string();
        assert!(
            msg.contains("audit_events is append-only"),
            "expected append-only error, got: {msg}"
        );
    } else {
        panic!("UPDATE should have been rejected by the append-only trigger");
    }
    let _ = tx.rollback().await;
}

#[tokio::test]
async fn audit_events_rejects_delete() {
    let Some(database_url) = std::env::var("GOBROWSE_TEST_DATABASE_URL").ok() else {
        eprintln!("GOBROWSE_TEST_DATABASE_URL is unset; skipping test");
        return;
    };
    let _lock = common::acquire_test_lock(&database_url).await;
    let pool = test_pool().await.expect("test pool after lock");
    let (_profile_id, _user_id, seq) = seed_audit_row(&pool).await;

    let mut tx = pool.begin().await.expect("begin tx");
    let result = sqlx::query("DELETE FROM audit_events WHERE sequence = $1")
        .bind(seq)
        .execute(&mut *tx)
        .await;
    if let Err(e) = &result {
        let msg = e.to_string();
        assert!(
            msg.contains("audit_events is append-only"),
            "expected append-only error, got: {msg}"
        );
    } else {
        panic!("DELETE should have been rejected by the append-only trigger");
    }
    let _ = tx.rollback().await;
}

#[tokio::test]
async fn audit_events_rejects_truncate() {
    let Some(database_url) = std::env::var("GOBROWSE_TEST_DATABASE_URL").ok() else {
        eprintln!("GOBROWSE_TEST_DATABASE_URL is unset; skipping test");
        return;
    };
    let _lock = common::acquire_test_lock(&database_url).await;
    let pool = test_pool().await.expect("test pool after lock");
    // Seed a row so we can verify it survives.
    let (_profile_id, _user_id, _seq) = seed_audit_row(&pool).await;

    let mut tx = pool.begin().await.expect("begin tx");
    let result = sqlx::query("TRUNCATE audit_events").execute(&mut *tx).await;
    if let Err(e) = &result {
        let msg = e.to_string();
        assert!(
            msg.contains("audit_events is append-only"),
            "expected append-only error, got: {msg}"
        );
    } else {
        panic!("TRUNCATE should have been rejected by the append-only trigger");
    }
    // Transaction is already aborted; explicit rollback is safe.
    let _ = tx.rollback().await;
}

#[tokio::test]
async fn audit_appends_succeed_after_mutation_rejection() {
    let Some(database_url) = std::env::var("GOBROWSE_TEST_DATABASE_URL").ok() else {
        eprintln!("GOBROWSE_TEST_DATABASE_URL is unset; skipping test");
        return;
    };
    let _lock = common::acquire_test_lock(&database_url).await;
    let pool = test_pool().await.expect("test pool after lock");
    let (_profile_id, _user_id, seq) = seed_audit_row(&pool).await;

    // Verify UPDATE is rejected (proves the trigger is active).
    let mut tx = pool.begin().await.expect("begin tx");
    let result = sqlx::query("UPDATE audit_events SET outcome = 'tampered' WHERE sequence = $1")
        .bind(seq)
        .execute(&mut *tx)
        .await;
    assert!(
        result.is_err(),
        "UPDATE should have been rejected before the append test"
    );
    let _ = tx.rollback().await;

    // A fresh INSERT must still succeed — the trigger only blocks mutations.
    let new_profile = Uuid::now_v7();
    sqlx::query("INSERT INTO profiles (id, name) VALUES ($1, 'append-proof-profile')")
        .bind(new_profile)
        .execute(&pool)
        .await
        .expect("create second profile");

    let new_user = Uuid::now_v7();
    sqlx::query(
        "INSERT INTO users (id, email, display_name, password_hash, role, primary_profile_id) \
         VALUES ($1, $2, 'Appender', 'unused', 'OWNER', $3)",
    )
    .bind(new_user)
    .bind(format!("{new_user}@append.test"))
    .bind(new_profile)
    .execute(&pool)
    .await
    .expect("create second user");

    sqlx::query(
        "INSERT INTO audit_events \
         (actor_user_id, profile_id, action, resource_type, resource_id, outcome) \
         VALUES ($1, $2, 'post.rejection.insert', 'test_type', 'res-2', 'success')",
    )
    .bind(new_user)
    .bind(new_profile)
    .execute(&pool)
    .await
    .expect("INSERT must succeed after rejection — trigger must not block appends");

    let count: i64 = sqlx::query_scalar("SELECT count(*) FROM audit_events WHERE profile_id = $1")
        .bind(new_profile)
        .fetch_one(&pool)
        .await
        .expect("count appended rows");
    assert_eq!(
        count, 1,
        "exactly one audit row should exist for the new profile"
    );
}
