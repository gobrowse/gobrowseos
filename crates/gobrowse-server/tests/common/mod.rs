//! Shared test helpers for DB integration tests.
//!
//! ## Test isolation
//!
//! Multiple integration test files share ONE PostgreSQL database and run as
//! parallel nextest processes.  Tests that spawn background workers with
//! GLOBAL-scan SQL (notably `claim_runs` and `claim_deliveries`, which scan
//! all `agent_runs` / `webhook_deliveries` rows with `FOR UPDATE SKIP LOCKED`)
//! race other tests' rows.
//!
//! `acquire_test_lock` serializes ALL DB integration tests across processes
//! via a single PostgreSQL advisory lock.  Every DB-touching test MUST acquire
//! this lock at the start of its body and hold the returned connection for
//! the test's full lifetime (the connection's Drop releases the session-level
//! lock automatically, even on panic).

use sqlx::{Connection, PgConnection};

/// Single shared advisory lock key — must be unique within the database
/// across all applications.  Stable, never changes.
pub const GOBROWSE_TEST_LOCK_KEY: i64 = 2_025_080_801;

/// Acquire a cross-process advisory lock with exponential-backoff retry.
///
/// Returns a dedicated `PgConnection` that holds a **session-level** lock
/// (via `pg_try_advisory_lock`).  When the connection is dropped (test exit
/// or panic unwind) PostgreSQL auto-releases the lock.
///
/// `database_url` must be the same URL used by the test's pool.
pub async fn acquire_test_lock(database_url: &str) -> PgConnection {
    let mut delay_ms = 10u64;
    loop {
        match sqlx::PgConnection::connect(database_url).await {
            Ok(mut conn) => {
                let got: bool = sqlx::query_scalar("SELECT pg_try_advisory_lock($1::bigint)")
                    .bind(GOBROWSE_TEST_LOCK_KEY)
                    .fetch_one(&mut conn)
                    .await
                    .expect("pg_try_advisory_lock query must succeed");
                if got {
                    return conn;
                }
                // Lock held by another process — drop conn, wait, retry.
                drop(conn);
            }
            Err(e) => {
                eprintln!(
                    "acquire_test_lock: connect error {e}, database_url={database_url}, retrying…"
                );
            }
        }
        tokio::time::sleep(std::time::Duration::from_millis(delay_ms)).await;
        delay_ms = (delay_ms * 2).min(2000);
    }
}
