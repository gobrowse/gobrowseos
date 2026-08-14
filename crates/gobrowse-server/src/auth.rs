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
    Json(input): Json<LoginRequest>,
) -> Result<impl IntoResponse, AppError> {
    check_login_throttle(&state.pool, input.email.trim(), &state.settings.auth).await?;
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
        return Err(AppError::Unauthorized);
    }
    let row = row.expect("checked above");
    let user_id: Uuid = row.get("id");
    let mut tx = state.pool.begin().await?;
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
           AND s.expires_at > $2 AND s.absolute_expires_at > $2",
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

async fn create_session(
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

#[cfg(test)]
mod tests {
    use super::*;

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
