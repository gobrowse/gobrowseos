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

#[tokio::test]
async fn migrations_enable_pgvector_and_schema_version() {
    let Some(pool) = test_pool().await else {
        eprintln!("GOBROWSE_TEST_DATABASE_URL is unset; skipping PostgreSQL integration test");
        return;
    };
    let row = sqlx::query(
        "SELECT schema_version, EXISTS(SELECT 1 FROM pg_extension WHERE extname = 'vector') AS vector_enabled \
         FROM schema_metadata WHERE singleton",
    )
    .fetch_one(&pool)
    .await
    .expect("read schema metadata");
    assert_eq!(row.get::<i64, _>("schema_version"), 1);
    assert!(row.get::<bool, _>("vector_enabled"));
}

#[tokio::test]
async fn autobiography_constraint_uses_character_length() {
    let Some(pool) = test_pool().await else {
        eprintln!("GOBROWSE_TEST_DATABASE_URL is unset; skipping PostgreSQL integration test");
        return;
    };
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
    let Some(pool) = test_pool().await else {
        eprintln!("GOBROWSE_TEST_DATABASE_URL is unset; skipping PostgreSQL integration test");
        return;
    };
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
