//! Session lifecycle management (M25a).
//!
//! Centralizes the M25a session hardening rules: concurrent-session limits,
//! append-only session event tracking, forced revocation, idle expiry, and
//! step-up auth timestamps. All writes go through the caller's transaction
//! so session creation stays atomic with login.

use sqlx::{Postgres, Row, Transaction};
use uuid::Uuid;

use crate::error::AppError;

/// Concurrency limit by role. Single-user OWNER gets the highest ceiling.
pub fn concurrent_limit_for_role(role: &str) -> u32 {
    match role {
        "OWNER" => 10,
        "ADMIN" => 5,
        "MEMBER" => 3,
        "VIEWER" => 1,
        _ => 1,
    }
}

/// Enforce the concurrent-session limit: count the user's active sessions;
/// if at or above the limit, revoke the oldest (by `created_at`) so the new
/// session can proceed. Returns the number of revoked sessions.
pub async fn enforce_concurrent_limit(
    tx: &mut Transaction<'_, Postgres>,
    user_id: Uuid,
    role: &str,
) -> Result<u32, AppError> {
    let limit = concurrent_limit_for_role(role);
    let active: i64 = sqlx::query_scalar(
        "SELECT count(*) FROM sessions WHERE user_id = $1 AND expires_at > now() \
         AND NOT EXISTS (SELECT 1 FROM session_revocations r WHERE r.token_hash = sessions.token_hash)",
    )
    .bind(user_id)
    .fetch_one(&mut **tx)
    .await?;
    let over = active.saturating_sub(i64::from(limit));
    if over > 0 {
        let stale: Vec<Vec<u8>> = sqlx::query(
            "SELECT token_hash FROM sessions WHERE user_id = $1 AND expires_at > now() \
             AND NOT EXISTS (SELECT 1 FROM session_revocations r WHERE r.token_hash = sessions.token_hash) \
             ORDER BY created_at ASC LIMIT $2",
        )
        .bind(user_id)
        .bind(over)
        .fetch_all(&mut **tx)
        .await?
        .into_iter()
        .map(|row| row.get("token_hash"))
        .collect();
        for hash in stale {
            revoke_session(tx, user_id, &hash, "concurrent_limit", None).await?;
        }
        Ok(over as u32)
    } else {
        Ok(0)
    }
}

/// Record a session lifecycle event (append-only).
pub async fn record_session_event(
    tx: &mut Transaction<'_, Postgres>,
    session_hash: &[u8],
    user_id: Uuid,
    event_type: &str,
    ip_hash: Option<&[u8]>,
    user_agent_hash: Option<&[u8]>,
    metadata: serde_json::Value,
) -> Result<(), AppError> {
    sqlx::query(
        "INSERT INTO session_events (session_hash, user_id, event_type, ip_hash, user_agent_hash, metadata) \
         VALUES ($1, $2, $3, $4, $5, $6)",
    )
    .bind(session_hash)
    .bind(user_id)
    .bind(event_type)
    .bind(ip_hash)
    .bind(user_agent_hash)
    .bind(metadata)
    .execute(&mut **tx)
    .await?;
    Ok(())
}

/// Revoke one session by token hash. Records a `revoked` event and inserts
/// into `session_revocations` (idempotent via PK conflict skip).
pub async fn revoke_session(
    tx: &mut Transaction<'_, Postgres>,
    user_id: Uuid,
    session_hash: &[u8],
    reason: &str,
    revoked_by: Option<Uuid>,
) -> Result<(), AppError> {
    sqlx::query(
        "INSERT INTO session_revocations (token_hash, user_id, revoked_by, reason) \
         VALUES ($1, $2, $3, $4) ON CONFLICT (token_hash) DO NOTHING",
    )
    .bind(session_hash)
    .bind(user_id)
    .bind(revoked_by)
    .bind(reason)
    .execute(&mut **tx)
    .await?;
    record_session_event(
        tx,
        session_hash,
        user_id,
        "revoked",
        None,
        None,
        serde_json::json!({ "reason": reason }),
    )
    .await?;
    Ok(())
}

/// Revoke every session except `keep_hash`. Returns the count revoked.
pub async fn revoke_all_other_sessions(
    tx: &mut Transaction<'_, Postgres>,
    user_id: Uuid,
    keep_hash: &[u8],
    reason: &str,
) -> Result<u32, AppError> {
    let hashes: Vec<Vec<u8>> =
        sqlx::query("SELECT token_hash FROM sessions WHERE user_id = $1 AND token_hash <> $2")
            .bind(user_id)
            .bind(keep_hash)
            .fetch_all(&mut **tx)
            .await?
            .into_iter()
            .map(|row| row.get("token_hash"))
            .collect();
    for hash in &hashes {
        revoke_session(tx, user_id, hash, reason, Some(user_id)).await?;
    }
    Ok(hashes.len() as u32)
}

/// Touch `last_seen_at` on an authenticated request (idle timeout reset).
pub async fn touch_session(pool: &sqlx::PgPool, session_hash: &[u8]) -> Result<(), AppError> {
    sqlx::query("UPDATE sessions SET last_seen_at = now() WHERE token_hash = $1")
        .bind(session_hash)
        .execute(pool)
        .await?;
    Ok(())
}

/// Whether this session has performed step-up auth recently enough for
/// sensitive operations (`step_up_max_age_secs`).
pub async fn step_up_fresh(
    pool: &sqlx::PgPool,
    session_hash: &[u8],
    step_up_max_age_secs: i64,
) -> Result<bool, AppError> {
    let fresh: Option<bool> = sqlx::query_scalar(
        "SELECT last_step_up_at IS NOT NULL AND last_step_up_at > now() - make_interval(secs => $2) \
         FROM sessions WHERE token_hash = $1",
    )
    .bind(session_hash)
    .bind(step_up_max_age_secs)
    .fetch_optional(pool)
    .await?;
    Ok(fresh.unwrap_or(false))
}

/// Mark the session as having completed step-up auth now.
pub async fn mark_step_up(
    tx: &mut Transaction<'_, Postgres>,
    session_hash: &[u8],
    user_id: Uuid,
) -> Result<(), AppError> {
    sqlx::query("UPDATE sessions SET last_step_up_at = now() WHERE token_hash = $1")
        .bind(session_hash)
        .execute(&mut **tx)
        .await?;
    record_session_event(
        tx,
        session_hash,
        user_id,
        "step_up",
        None,
        None,
        serde_json::json!({}),
    )
    .await?;
    Ok(())
}

/// Increment the user's auth epoch, invalidating ALL existing sessions
/// (password change, forced logout, role change). Writes a `revoked` event
/// for each live session first.
pub async fn invalidate_all_sessions(
    tx: &mut Transaction<'_, Postgres>,
    user_id: Uuid,
    reason: &str,
) -> Result<(), AppError> {
    let hashes: Vec<Vec<u8>> =
        sqlx::query("SELECT token_hash FROM sessions WHERE user_id = $1 AND expires_at > now()")
            .bind(user_id)
            .fetch_all(&mut **tx)
            .await?
            .into_iter()
            .map(|row| row.get("token_hash"))
            .collect();
    for hash in &hashes {
        revoke_session(tx, user_id, hash, reason, None).await?;
    }
    sqlx::query("UPDATE users SET auth_epoch = auth_epoch + 1 WHERE id = $1")
        .bind(user_id)
        .execute(&mut **tx)
        .await?;
    Ok(())
}
