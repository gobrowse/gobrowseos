use std::sync::Arc;

use argon2::{
    Algorithm, Argon2, Params, PasswordHash, PasswordHasher, PasswordVerifier, Version,
    password_hash::{SaltString, rand_core::OsRng},
};
use axum::{
    Json,
    extract::State,
    http::{HeaderMap, HeaderValue, StatusCode, header},
    response::IntoResponse,
};
use base64::{Engine as _, engine::general_purpose::URL_SAFE_NO_PAD};
use rand::RngCore;
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use sqlx::{Postgres, Row, Transaction};
use time::{Duration, OffsetDateTime};
use tokio::sync::Semaphore;
use uuid::Uuid;
use zeroize::Zeroizing;

use crate::{AppState, config::AuthSettings, embedding, error::AppError};

const SESSION_COOKIE: &str = "__Host-gobrowse_session";
const DEV_SESSION_COOKIE: &str = "gobrowse_session";

#[derive(Clone)]
pub struct PasswordRuntime {
    semaphore: Arc<Semaphore>,
    settings: AuthSettings,
    dummy_hash: Arc<String>,
}

impl PasswordRuntime {
    pub async fn new(settings: AuthSettings) -> Result<Self, AppError> {
        let runtime = Self {
            semaphore: Arc::new(Semaphore::new(settings.max_parallel_hashes)),
            settings,
            dummy_hash: Arc::new(String::new()),
        };
        let dummy_hash = runtime.hash("gobrowse-dummy-password-never-valid").await?;
        Ok(Self {
            dummy_hash: Arc::new(dummy_hash),
            ..runtime
        })
    }

    pub async fn hash(&self, password: &str) -> Result<String, AppError> {
        let permit = self
            .semaphore
            .clone()
            .acquire_owned()
            .await
            .map_err(|error| anyhow::anyhow!(error))?;
        let settings = self.settings.clone();
        let password = Zeroizing::new(password.to_owned());
        tokio::task::spawn_blocking(move || {
            let _permit = permit;
            let params = Params::new(
                settings.argon2_memory_kib,
                settings.argon2_iterations,
                settings.argon2_parallelism,
                None,
            )
            .map_err(|error| anyhow::anyhow!(error.to_string()))?;
            let argon2 = Argon2::new(Algorithm::Argon2id, Version::V0x13, params);
            let salt = SaltString::generate(&mut OsRng);
            Ok::<_, anyhow::Error>(
                argon2
                    .hash_password(password.as_bytes(), &salt)
                    .map_err(|error| anyhow::anyhow!(error.to_string()))?
                    .to_string(),
            )
        })
        .await
        .map_err(|error| anyhow::anyhow!(error))?
        .map_err(AppError::Internal)
    }

    pub async fn verify(&self, password: &str, encoded: Option<&str>) -> Result<bool, AppError> {
        let permit = self
            .semaphore
            .clone()
            .acquire_owned()
            .await
            .map_err(|error| anyhow::anyhow!(error))?;
        let settings = self.settings.clone();
        let password = Zeroizing::new(password.to_owned());
        let encoded = encoded.unwrap_or(&self.dummy_hash).to_owned();
        tokio::task::spawn_blocking(move || {
            let _permit = permit;
            let params = Params::new(
                settings.argon2_memory_kib,
                settings.argon2_iterations,
                settings.argon2_parallelism,
                None,
            )
            .map_err(|error| anyhow::anyhow!(error.to_string()))?;
            let argon2 = Argon2::new(Algorithm::Argon2id, Version::V0x13, params);
            let parsed =
                PasswordHash::new(&encoded).map_err(|error| anyhow::anyhow!(error.to_string()))?;
            Ok::<_, anyhow::Error>(argon2.verify_password(password.as_bytes(), &parsed).is_ok())
        })
        .await
        .map_err(|error| anyhow::anyhow!(error))?
        .map_err(AppError::Internal)
    }
}

#[derive(Debug, Clone, Serialize)]
pub struct AuthenticatedUser {
    pub id: Uuid,
    pub profile_id: Uuid,
    pub email: String,
    pub display_name: String,
    pub role: String,
    #[serde(skip)]
    pub session_hash: Option<Vec<u8>>,
}

#[derive(Debug, Serialize)]
pub struct SetupStatus {
    pub owner_required: bool,
}

#[derive(Debug, Deserialize)]
pub struct OwnerSetupRequest {
    pub email: String,
    pub display_name: String,
    pub password: String,
}

#[derive(Debug, Deserialize)]
pub struct LoginRequest {
    pub email: String,
    pub password: String,
}

#[derive(Debug, Deserialize)]
pub struct RotateSessionsRequest {
    pub user_id: Uuid,
}

async fn check_login_throttle(
    pool: &sqlx::PgPool,
    email: &str,
    settings: &crate::config::AuthSettings,
) -> Result<(), AppError> {
    let window = time::Duration::seconds(settings.login_throttle_window_secs);
    let max_attempts = settings.login_throttle_max_attempts as i64;
    let cutoff = time::OffsetDateTime::now_utc() - window;
    let count: i64 = sqlx::query_scalar(
        "SELECT COUNT(*) FROM login_attempts WHERE email = $1 AND occurred_at > $2",
    )
    .bind(email.to_lowercase())
    .bind(cutoff)
    .fetch_one(pool)
    .await?;
    if count >= max_attempts {
        return Err(AppError::Unauthorized);
    }
    Ok(())
}

async fn record_login_attempt(
    pool: &sqlx::PgPool,
    email: &str,
    ip_hash: Option<&[u8]>,
    outcome: &str,
) -> Result<(), AppError> {
    sqlx::query("INSERT INTO login_attempts (email, ip_hash, outcome) VALUES ($1, $2, $3)")
        .bind(email.to_lowercase())
        .bind(ip_hash)
        .bind(outcome)
        .execute(pool)
        .await?;
    Ok(())
}

pub async fn setup_status(State(state): State<AppState>) -> Result<Json<SetupStatus>, AppError> {
    let exists: bool =
        sqlx::query_scalar("SELECT EXISTS(SELECT 1 FROM users WHERE role = 'OWNER')")
            .fetch_one(&state.pool)
            .await?;
    Ok(Json(SetupStatus {
        owner_required: !exists,
    }))
}

pub async fn create_owner(
    State(state): State<AppState>,
    Json(input): Json<OwnerSetupRequest>,
) -> Result<impl IntoResponse, AppError> {
    check_login_throttle(&state.pool, input.email.trim(), &state.settings.auth).await?;
    validate_identity(&input.email, &input.display_name, &input.password)?;
    let password_hash = state.passwords.hash(&input.password).await?;
    let mut tx = state.pool.begin().await?;
    sqlx::query("SELECT pg_advisory_xact_lock(607_629_719)")
        .execute(&mut *tx)
        .await?;
    let exists: bool =
        sqlx::query_scalar("SELECT EXISTS(SELECT 1 FROM users WHERE role = 'OWNER')")
            .fetch_one(&mut *tx)
            .await?;
    if exists {
        return Err(AppError::Conflict("owner setup is already complete"));
    }

    let profile_id = Uuid::now_v7();
    let user_id = Uuid::now_v7();
    sqlx::query("INSERT INTO profiles (id, name) VALUES ($1, $2)")
        .bind(profile_id)
        .bind(format!("{}'s profile", input.display_name.trim()))
        .execute(&mut *tx)
        .await?;
    sqlx::query(
        "INSERT INTO users (id, email, display_name, password_hash, role, primary_profile_id) \
         VALUES ($1, lower($2), $3, $4, 'OWNER', $5)",
    )
    .bind(user_id)
    .bind(input.email.trim())
    .bind(input.display_name.trim())
    .bind(password_hash)
    .bind(profile_id)
    .execute(&mut *tx)
    .await?;
    let autobiography_id = Uuid::now_v7();
    sqlx::query(
        "INSERT INTO books (id, profile_id, title, body, book_type, scope, provenance, trust, author, security_classification, created_by_user_id) \
         VALUES ($1, $2, 'Autobiography', '', 'AUTOBIOGRAPHY', 'PROFILE', 'USER', 'USER_PROVIDED', $3, 'CONFIDENTIAL', $4)",
    )
    .bind(autobiography_id)
    .bind(profile_id)
    .bind(input.display_name.trim())
    .bind(user_id)
    .execute(&mut *tx)
    .await?;
    sqlx::query(
        "INSERT INTO book_revisions (id,book_id,revision,title,body,tags,metadata,changed_by,change_reason) \
         VALUES ($1,$2,1,'Autobiography','','{}','{}',$3,'Initial autobiography')",
    )
    .bind(Uuid::now_v7())
    .bind(autobiography_id)
    .bind(user_id)
    .execute(&mut *tx)
    .await?;
    embedding::enqueue_book(&mut tx, profile_id, autobiography_id, 1).await?;
    audit(
        &mut tx,
        Some(user_id),
        Some(profile_id),
        "owner.created",
        "user",
        Some(user_id.to_string()),
        "success",
    )
    .await?;
    let (cookie, user) = create_session(&state, &mut tx, user_id).await?;
    tx.commit().await?;
    record_login_attempt(&state.pool, input.email.trim(), None, "success").await?;
    Ok(([(header::SET_COOKIE, cookie)], Json(user)))
}

pub async fn login(
    State(state): State<AppState>,
    headers: HeaderMap,
    Json(input): Json<LoginRequest>,
) -> Result<impl IntoResponse, AppError> {
    check_login_throttle(&state.pool, input.email.trim(), &state.settings.auth).await?;
    // M25a: account lockout after consecutive failures.
    let locked: Option<time::OffsetDateTime> = sqlx::query_scalar(
        "SELECT locked_until FROM users WHERE lower(email) = lower($1) AND locked_until > now()",
    )
    .bind(input.email.trim())
    .fetch_optional(&state.pool)
    .await?;
    if locked.is_some() {
        return Err(AppError::Conflict("account is temporarily locked"));
    }
    // M25a: in-memory per-IP + global sliding-window limits on top of the
    // existing per-email DB throttle.
    let ip = headers
        .get("x-forwarded-for")
        .and_then(|v| v.to_str().ok())
        .map(str::trim)
        .filter(|s| !s.is_empty())
        .map(|s| s.split(',').next().unwrap_or(s).trim().to_string());
    let ip_key = ip.unwrap_or_else(|| "unknown".to_string());
    if state
        .rate_limiter
        .check_key(&format!("login:ip:{ip_key}"), 300, 20)
        .await
    {
        return Err(AppError::RateLimited);
    }
    if state
        .rate_limiter
        .check_global("login:global", 60, 100)
        .await
    {
        return Err(AppError::RateLimited);
    }
    let row = sqlx::query(
        "SELECT id, primary_profile_id, email, display_name, role, password_hash \
         FROM users WHERE lower(email) = lower($1) AND disabled_at IS NULL",
    )
    .bind(input.email.trim())
    .fetch_optional(&state.pool)
    .await?;
    let encoded = row
        .as_ref()
        .map(|row| row.get::<String, _>("password_hash"));
    if !state
        .passwords
        .verify(&input.password, encoded.as_deref())
        .await?
        || row.is_none()
    {
        record_login_attempt(&state.pool, input.email.trim(), None, "failure").await?;
        // M25a: increment consecutive-failure counter; lock after 10.
        sqlx::query(
            "UPDATE users SET consecutive_failures = consecutive_failures + 1, \
             locked_until = CASE WHEN consecutive_failures + 1 >= 10 \
               THEN now() + interval '30 minutes' ELSE locked_until END \
             WHERE lower(email) = lower($1)",
        )
        .bind(input.email.trim())
        .execute(&state.pool)
        .await?;
        return Err(AppError::Unauthorized);
    }
    let row = row.expect("checked above");
    let user_id: Uuid = row.get("id");
    let mut tx = state.pool.begin().await?;
    // M25a: reset failure counter + enforce concurrent-session limit.
    sqlx::query("UPDATE users SET consecutive_failures = 0, locked_until = NULL WHERE id = $1")
        .bind(user_id)
        .execute(&mut *tx)
        .await?;
    let role: String = row.get("role");
    crate::session_manager::enforce_concurrent_limit(&mut tx, user_id, &role).await?;
    let (cookie, user) = create_session(&state, &mut tx, user_id).await?;
    audit(
        &mut tx,
        Some(user_id),
        Some(user.profile_id),
        "auth.login",
        "session",
        None,
        "success",
    )
    .await?;
    tx.commit().await?;
    record_login_attempt(&state.pool, input.email.trim(), None, "success").await?;
    Ok(([(header::SET_COOKIE, cookie)], Json(user)))
}

pub async fn logout(
    State(state): State<AppState>,
    headers: HeaderMap,
) -> Result<impl IntoResponse, AppError> {
    if let Some(token) = session_token(&headers, state.settings.http.secure_cookies) {
        sqlx::query("DELETE FROM sessions WHERE token_hash = $1")
            .bind(token_hash(token.as_bytes()))
            .execute(&state.pool)
            .await?;
    }
    Ok((
        [(
            header::SET_COOKIE,
            expired_cookie(state.settings.http.secure_cookies)?,
        )],
        axum::http::StatusCode::NO_CONTENT,
    ))
}

pub async fn me(
    State(state): State<AppState>,
    headers: HeaderMap,
) -> Result<Json<AuthenticatedUser>, AppError> {
    Ok(Json(require_user(&state, &headers).await?))
}

pub async fn rotate_sessions(
    State(state): State<AppState>,
    headers: HeaderMap,
    Json(input): Json<RotateSessionsRequest>,
) -> Result<StatusCode, AppError> {
    let user = require_user(&state, &headers).await?;
    if user.id != input.user_id && !matches!(user.role.as_str(), "OWNER" | "ADMIN") {
        return Err(AppError::Forbidden);
    }
    let mut tx = state.pool.begin().await?;
    let target_row = sqlx::query(
        "SELECT id, primary_profile_id FROM users WHERE id = $1 AND disabled_at IS NULL FOR UPDATE",
    )
    .bind(input.user_id)
    .fetch_optional(&mut *tx)
    .await?
    .ok_or(AppError::NotFound)?;
    let target_profile_id: Uuid = target_row.get("primary_profile_id");
    if user.id != input.user_id && user.profile_id != target_profile_id {
        return Err(AppError::Forbidden);
    }
    sqlx::query("UPDATE users SET auth_epoch = auth_epoch + 1 WHERE id = $1")
        .bind(input.user_id)
        .execute(&mut *tx)
        .await?;
    audit(
        &mut tx,
        Some(user.id),
        Some(target_profile_id),
        "auth.sessions_rotated",
        "user",
        Some(input.user_id.to_string()),
        "success",
    )
    .await?;
    tx.commit().await?;
    Ok(StatusCode::NO_CONTENT)
}

/// List the caller's linked auth methods.
pub async fn list_auth_methods(
    State(state): State<AppState>,
    headers: HeaderMap,
) -> Result<Json<serde_json::Value>, AppError> {
    let user = require_user(&state, &headers).await?;
    let rows = sqlx::query(
        "SELECT id, method_type, label, is_primary, last_used_at, created_at \
         FROM auth_methods WHERE user_id = $1 ORDER BY is_primary DESC, created_at ASC",
    )
    .bind(user.id)
    .fetch_all(&state.pool)
    .await?;
    let methods: Vec<serde_json::Value> = rows
        .iter()
        .map(|row| {
            serde_json::json!({
                "id": row.get::<Uuid, _>("id"),
                "method_type": row.get::<String, _>("method_type"),
                "label": row.get::<Option<String>, _>("label"),
                "is_primary": row.get::<bool, _>("is_primary"),
                "last_used_at": row.get::<Option<time::OffsetDateTime>, _>("last_used_at"),
                "created_at": row.get::<time::OffsetDateTime, _>("created_at"),
            })
        })
        .collect();
    Ok(Json(serde_json::json!({ "methods": methods })))
}

/// Delete an auth method; the last remaining method cannot be removed.
pub async fn delete_auth_method(
    State(state): State<AppState>,
    headers: HeaderMap,
    axum::extract::Path(id): axum::extract::Path<Uuid>,
) -> Result<StatusCode, AppError> {
    let user = require_user(&state, &headers).await?;
    let mut tx = state.pool.begin().await?;
    let method: Option<(Uuid, String)> = sqlx::query_as(
        "SELECT id, method_type FROM auth_methods WHERE id = $1 AND user_id = $2 FOR UPDATE",
    )
    .bind(id)
    .bind(user.id)
    .fetch_optional(&mut *tx)
    .await?;
    let Some((method_id, _)) = method else {
        return Err(AppError::NotFound);
    };
    let remaining: i64 =
        sqlx::query_scalar("SELECT count(*) FROM auth_methods WHERE user_id = $1 AND id <> $2")
            .bind(user.id)
            .bind(method_id)
            .fetch_one(&mut *tx)
            .await?;
    if remaining == 0 {
        return Err(AppError::Conflict(
            "cannot remove the last authentication method",
        ));
    }
    sqlx::query("DELETE FROM auth_methods WHERE id = $1")
        .bind(method_id)
        .execute(&mut *tx)
        .await?;
    audit(
        &mut tx,
        Some(user.id),
        Some(user.profile_id),
        "auth.method_deleted",
        "auth_method",
        Some(method_id.to_string()),
        "success",
    )
    .await?;
    tx.commit().await?;
    Ok(StatusCode::NO_CONTENT)
}

/// List the caller's active sessions (device info from user-agent hash).
pub async fn list_sessions(
    State(state): State<AppState>,
    headers: HeaderMap,
) -> Result<Json<serde_json::Value>, AppError> {
    let user = require_user(&state, &headers).await?;
    let session_hash = user.session_hash.unwrap_or_default();
    let rows = sqlx::query(
        "SELECT token_hash, created_at, last_seen_at, expires_at, last_step_up_at \
         FROM sessions WHERE user_id = $1 AND expires_at > now() \
         AND NOT EXISTS (SELECT 1 FROM session_revocations r WHERE r.token_hash = sessions.token_hash) \
         ORDER BY created_at DESC",
    )
    .bind(user.id)
    .fetch_all(&state.pool)
    .await?;
    let sessions: Vec<serde_json::Value> = rows
        .iter()
        .map(|row| {
            let hash: Vec<u8> = row.get("token_hash");
            serde_json::json!({
                "hash": base64::Engine::encode(&base64::engine::general_purpose::URL_SAFE_NO_PAD, &hash),
                "current": hash == session_hash,
                "created_at": row.get::<time::OffsetDateTime, _>("created_at"),
                "last_seen_at": row.get::<time::OffsetDateTime, _>("last_seen_at"),
                "expires_at": row.get::<time::OffsetDateTime, _>("expires_at"),
                "last_step_up_at": row.get::<Option<time::OffsetDateTime>, _>("last_step_up_at"),
            })
        })
        .collect();
    Ok(Json(serde_json::json!({ "sessions": sessions })))
}

/// Revoke all other sessions (keep the current one).
pub async fn revoke_other_sessions(
    State(state): State<AppState>,
    headers: HeaderMap,
) -> Result<StatusCode, AppError> {
    let user = require_user(&state, &headers).await?;
    let Some(hash) = user.session_hash else {
        return Err(AppError::Unauthorized);
    };
    let mut tx = state.pool.begin().await?;
    let revoked = crate::session_manager::revoke_all_other_sessions(
        &mut tx,
        user.id,
        &hash,
        "user_revoked_others",
    )
    .await?;
    audit(
        &mut tx,
        Some(user.id),
        Some(user.profile_id),
        "auth.sessions_revoked_others",
        "session",
        None,
        "success",
    )
    .await?;
    tx.commit().await?;
    Ok(if revoked == 0 {
        StatusCode::NO_CONTENT
    } else {
        StatusCode::OK
    })
}

/// Revoke a specific session by its base64url hash.
pub async fn revoke_one_session(
    State(state): State<AppState>,
    headers: HeaderMap,
    axum::extract::Path(hash): axum::extract::Path<String>,
) -> Result<StatusCode, AppError> {
    let user = require_user(&state, &headers).await?;
    let decoded = base64::Engine::decode(
        &base64::engine::general_purpose::URL_SAFE_NO_PAD,
        hash.as_bytes(),
    )
    .map_err(|_| AppError::NotFound)?;
    let mut tx = state.pool.begin().await?;
    let owned: Option<Uuid> =
        sqlx::query_scalar("SELECT user_id FROM sessions WHERE token_hash = $1")
            .bind(&decoded)
            .fetch_optional(&mut *tx)
            .await?;
    let Some(owner) = owned else {
        return Err(AppError::NotFound);
    };
    if owner != user.id {
        return Err(AppError::NotFound);
    }
    crate::session_manager::revoke_session(
        &mut tx,
        user.id,
        &decoded,
        "user_revoked",
        Some(user.id),
    )
    .await?;
    audit(
        &mut tx,
        Some(user.id),
        Some(user.profile_id),
        "auth.session_revoked",
        "session",
        None,
        "success",
    )
    .await?;
    tx.commit().await?;
    Ok(StatusCode::NO_CONTENT)
}

/// Step-up re-authentication: verify the password again and mark the current
/// session as step-up fresh for sensitive operations.
pub async fn step_up(
    State(state): State<AppState>,
    headers: HeaderMap,
    Json(input): Json<StepUpRequest>,
) -> Result<StatusCode, AppError> {
    let user = require_user(&state, &headers).await?;
    let Some(hash) = user.session_hash else {
        return Err(AppError::Unauthorized);
    };
    let row = sqlx::query("SELECT password_hash FROM users WHERE id = $1")
        .bind(user.id)
        .fetch_optional(&state.pool)
        .await?;
    let encoded = row.as_ref().map(|r| r.get::<String, _>("password_hash"));
    if !state
        .passwords
        .verify(&input.password, encoded.as_deref())
        .await?
    {
        return Err(AppError::Unauthorized);
    }
    let mut tx = state.pool.begin().await?;
    crate::session_manager::mark_step_up(&mut tx, &hash, user.id).await?;
    audit(
        &mut tx,
        Some(user.id),
        Some(user.profile_id),
        "auth.step_up",
        "session",
        None,
        "success",
    )
    .await?;
    tx.commit().await?;
    Ok(StatusCode::NO_CONTENT)
}

#[derive(Deserialize)]
pub struct StepUpRequest {
    pub password: String,
}

pub async fn require_user(
    state: &AppState,
    headers: &HeaderMap,
) -> Result<AuthenticatedUser, AppError> {
    let token =
        session_token(headers, state.settings.http.secure_cookies).ok_or(AppError::Unauthorized)?;
    let session_hash = token_hash(token.as_bytes());
    let now = OffsetDateTime::now_utc();
    let row = sqlx::query(
        "SELECT u.id, u.primary_profile_id, u.email, u.display_name, u.role \
         FROM sessions s JOIN users u ON u.id = s.user_id \
         WHERE s.token_hash = $1 AND s.auth_epoch = u.auth_epoch AND u.disabled_at IS NULL \
           AND s.expires_at > $2 AND s.absolute_expires_at > $2 \
           AND NOT EXISTS (SELECT 1 FROM session_revocations r WHERE r.token_hash = s.token_hash)",
    )
    .bind(&session_hash)
    .bind(now)
    .fetch_optional(&state.pool)
    .await?
    .ok_or(AppError::Unauthorized)?;
    Ok(AuthenticatedUser {
        id: row.get("id"),
        profile_id: row.get("primary_profile_id"),
        email: row.get("email"),
        display_name: row.get("display_name"),
        role: row.get("role"),
        session_hash: Some(session_hash),
    })
}

pub(crate) async fn create_session(
    state: &AppState,
    tx: &mut Transaction<'_, Postgres>,
    user_id: Uuid,
) -> Result<(HeaderValue, AuthenticatedUser), AppError> {
    let row = sqlx::query(
        "SELECT id, primary_profile_id, email, display_name, role, auth_epoch FROM users WHERE id = $1",
    )
    .bind(user_id)
    .fetch_one(&mut **tx)
    .await?;
    let mut bytes = Zeroizing::new([0_u8; 32]);
    rand::rng().fill_bytes(bytes.as_mut());
    let token = Zeroizing::new(URL_SAFE_NO_PAD.encode(*bytes));
    let now = OffsetDateTime::now_utc();
    let session_hash = token_hash(token.as_bytes());
    sqlx::query(
        "INSERT INTO sessions (token_hash, user_id, auth_epoch, expires_at, absolute_expires_at) \
         VALUES ($1, $2, $3, $4, $5)",
    )
    .bind(&session_hash)
    .bind(user_id)
    .bind(row.get::<i64, _>("auth_epoch"))
    .bind(now + Duration::minutes(state.settings.auth.session_idle_minutes))
    .bind(now + Duration::hours(state.settings.auth.session_absolute_hours))
    .execute(&mut **tx)
    .await?;
    // M25a: append-only session lifecycle event.
    crate::session_manager::record_session_event(
        tx,
        &session_hash,
        user_id,
        "created",
        None,
        None,
        serde_json::json!({}),
    )
    .await?;
    let cookie = session_cookie(&token, state.settings.http.secure_cookies)?;
    Ok((
        cookie,
        AuthenticatedUser {
            id: row.get("id"),
            profile_id: row.get("primary_profile_id"),
            email: row.get("email"),
            display_name: row.get("display_name"),
            role: row.get("role"),
            session_hash: Some(session_hash),
        },
    ))
}

fn validate_identity(email: &str, display_name: &str, password: &str) -> Result<(), AppError> {
    let email = email.trim();
    if email.len() > 320 || !email.contains('@') || email.chars().any(char::is_whitespace) {
        return Err(AppError::Validation("enter a valid email address".into()));
    }
    if display_name.trim().is_empty() || display_name.chars().count() > 200 {
        return Err(AppError::Validation(
            "display name must contain 1 to 200 characters".into(),
        ));
    }
    if password.chars().count() < 12 || password.len() > 1_024 {
        return Err(AppError::Validation(
            "password must contain at least 12 characters".into(),
        ));
    }
    Ok(())
}

fn token_hash(token: &[u8]) -> Vec<u8> {
    Sha256::digest(token).to_vec()
}

fn session_token(headers: &HeaderMap, secure: bool) -> Option<&str> {
    let name = if secure {
        SESSION_COOKIE
    } else {
        DEV_SESSION_COOKIE
    };
    headers
        .get(header::COOKIE)?
        .to_str()
        .ok()?
        .split(';')
        .filter_map(|part| part.trim().split_once('='))
        .find_map(|(key, value)| (key == name).then_some(value))
}

fn session_cookie(token: &str, secure: bool) -> Result<HeaderValue, AppError> {
    let name = if secure {
        SESSION_COOKIE
    } else {
        DEV_SESSION_COOKIE
    };
    let secure_attribute = if secure { "; Secure" } else { "" };
    HeaderValue::from_str(&format!(
        "{name}={token}; Path=/; HttpOnly; SameSite=Lax{secure_attribute}"
    ))
    .map_err(|error| AppError::Internal(anyhow::anyhow!(error)))
}

fn expired_cookie(secure: bool) -> Result<HeaderValue, AppError> {
    let name = if secure {
        SESSION_COOKIE
    } else {
        DEV_SESSION_COOKIE
    };
    let secure_attribute = if secure { "; Secure" } else { "" };
    HeaderValue::from_str(&format!(
        "{name}=; Path=/; HttpOnly; SameSite=Lax; Max-Age=0{secure_attribute}"
    ))
    .map_err(|error| AppError::Internal(anyhow::anyhow!(error)))
}

pub async fn audit(
    tx: &mut Transaction<'_, Postgres>,
    actor: Option<Uuid>,
    profile: Option<Uuid>,
    action: &str,
    resource_type: &str,
    resource_id: Option<String>,
    outcome: &str,
) -> Result<(), AppError> {
    sqlx::query(
        "INSERT INTO audit_events (actor_user_id, profile_id, action, resource_type, resource_id, outcome) \
         VALUES ($1, $2, $3, $4, $5, $6)",
    )
    .bind(actor)
    .bind(profile)
    .bind(action)
    .bind(resource_type)
    .bind(resource_id)
    .bind(outcome)
    .execute(&mut **tx)
    .await?;
    Ok(())
}

pub async fn require_user_or_run(
    state: &AppState,
    headers: &HeaderMap,
) -> Result<AuthenticatedUser, AppError> {
    // M24b: UI endpoints accept both human (session cookie) and agent (run token) auth.
    // Agent path reuses session auth for now; run-scoped tokens will be added when the
    // run worker exposes them. Until then, fall back to session auth.
    require_user(state, headers).await
}

pub async fn require_step_up(
    state: &AppState,
    user: &AuthenticatedUser,
    step_up_max_age_secs: i64,
) -> Result<(), AppError> {
    // Sensitive operations require recent step-up re-authentication. The
    // frontend prompts for the password (POST /auth/step-up) when this fails.
    let Some(hash) = user.session_hash.as_deref() else {
        return Err(AppError::PreconditionRequired);
    };
    let fresh =
        crate::session_manager::step_up_fresh(&state.pool, hash, step_up_max_age_secs).await?;
    if !fresh {
        return Err(AppError::PreconditionRequired);
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use crate::auth::{session_token, validate_identity};
    use axum::http::{HeaderMap, HeaderValue, header};

    #[test]
    fn identity_validation_rejects_weak_inputs() {
        assert!(validate_identity("not-email", "Owner", "long-enough-password").is_err());
        assert!(validate_identity("owner@example.com", "", "long-enough-password").is_err());
        assert!(validate_identity("owner@example.com", "Owner", "short").is_err());
    }

    #[test]
    fn cookie_parser_requires_exact_name() {
        let mut headers = HeaderMap::new();
        headers.insert(
            header::COOKIE,
            HeaderValue::from_static("other=1; gobrowse_session=abc"),
        );
        assert_eq!(session_token(&headers, false), Some("abc"));
        assert_eq!(session_token(&headers, true), None);
    }
}
