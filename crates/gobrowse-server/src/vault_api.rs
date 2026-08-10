use axum::{
    Json,
    extract::{Path, State},
    http::{HeaderMap, StatusCode},
};
use secrecy::SecretString;
use serde::{Deserialize, Serialize};
use sqlx::Row;
use time::OffsetDateTime;
use uuid::Uuid;

use crate::{
    AppState,
    auth::{AuthenticatedUser, audit, require_user},
    error::AppError,
    vault,
};

#[derive(Deserialize)]
pub struct StoreSecretRequest {
    pub purpose: String,
    #[serde(default)]
    pub allowed_hosts: Vec<String>,
    pub value: SecretString,
}

#[derive(Deserialize)]
pub struct ReplaceSecretRequest {
    pub value: SecretString,
    pub allowed_hosts: Option<Vec<String>>,
}

#[derive(Serialize)]
pub struct SecretMetadata {
    pub id: String,
    pub purpose: String,
    pub allowed_hosts: Vec<String>,
    pub backend: String,
    pub key_version: i32,
    pub created_at: OffsetDateTime,
    pub updated_at: OffsetDateTime,
}

#[derive(Serialize)]
pub struct RotationResponse {
    pub rotated: u64,
}

pub async fn create_secret(
    State(state): State<AppState>,
    headers: HeaderMap,
    Json(input): Json<StoreSecretRequest>,
) -> Result<(StatusCode, Json<SecretMetadata>), AppError> {
    let user = require_user(&state, &headers).await?;
    require_vault_admin(&user)?;
    validate_purpose(&input.purpose)?;
    let allowed_hosts = normalize_hosts(input.allowed_hosts)?;
    let id = format!("secret_{}", Uuid::now_v7());
    let mut tx = state.pool.begin().await?;
    state
        .vault
        .fence_current_key(&mut tx, user.profile_id)
        .await?;
    let encrypted = state
        .vault
        .encrypt(user.profile_id, &id, &input.purpose, &input.value)?;
    let now = OffsetDateTime::now_utc();
    sqlx::query(
        "INSERT INTO secret_references \
         (id, profile_id, backend, locator, encrypted_value, nonce, key_version, purpose, allowed_hosts, algorithm, wrapped_data_key, wrap_nonce, created_at, updated_at) \
         VALUES ($1,$2,'encrypted_database','database',$3,$4,$5,$6,$7,$8,$9,$10,$11,$11)",
    )
    .bind(&id)
    .bind(user.profile_id)
    .bind(encrypted.ciphertext)
    .bind(encrypted.nonce)
    .bind(encrypted.key_version)
    .bind(&input.purpose)
    .bind(&allowed_hosts)
    .bind(vault::algorithm())
    .bind(encrypted.wrapped_data_key)
    .bind(encrypted.wrap_nonce)
    .bind(now)
    .execute(&mut *tx)
    .await?;
    audit(
        &mut tx,
        Some(user.id),
        Some(user.profile_id),
        "secret.created",
        "secret",
        Some(id.clone()),
        "success",
    )
    .await?;
    tx.commit().await?;
    Ok((
        StatusCode::CREATED,
        Json(SecretMetadata {
            id,
            purpose: input.purpose,
            allowed_hosts,
            backend: "encrypted_database".into(),
            key_version: encrypted.key_version,
            created_at: now,
            updated_at: now,
        }),
    ))
}

pub async fn replace_secret(
    State(state): State<AppState>,
    headers: HeaderMap,
    Path(id): Path<String>,
    Json(input): Json<ReplaceSecretRequest>,
) -> Result<Json<SecretMetadata>, AppError> {
    let user = require_user(&state, &headers).await?;
    require_vault_admin(&user)?;
    let mut tx = state.pool.begin().await?;
    state
        .vault
        .fence_current_key(&mut tx, user.profile_id)
        .await?;
    let row = sqlx::query("SELECT purpose,allowed_hosts,created_at FROM secret_references WHERE id=$1 AND profile_id=$2 FOR UPDATE")
        .bind(&id).bind(user.profile_id).fetch_optional(&mut *tx).await?.ok_or(AppError::NotFound)?;
    let purpose: String = row.get("purpose");
    let allowed_hosts = input
        .allowed_hosts
        .map(normalize_hosts)
        .transpose()?
        .unwrap_or_else(|| row.get("allowed_hosts"));
    let encrypted = state
        .vault
        .encrypt(user.profile_id, &id, &purpose, &input.value)?;
    let now = OffsetDateTime::now_utc();
    sqlx::query(
        "UPDATE secret_references SET backend='encrypted_database',locator='database',encrypted_value=$1, nonce=$2, \
         key_version=$3, algorithm=$4,wrapped_data_key=$5,wrap_nonce=$6,allowed_hosts=$7,updated_at=$8 \
         WHERE id=$9 AND profile_id=$10",
    )
    .bind(encrypted.ciphertext)
    .bind(encrypted.nonce)
    .bind(encrypted.key_version)
    .bind(vault::algorithm())
    .bind(encrypted.wrapped_data_key)
    .bind(encrypted.wrap_nonce)
    .bind(&allowed_hosts)
    .bind(now)
    .bind(&id)
    .bind(user.profile_id)
    .execute(&mut *tx)
    .await?;
    audit(
        &mut tx,
        Some(user.id),
        Some(user.profile_id),
        "secret.replaced",
        "secret",
        Some(id.clone()),
        "success",
    )
    .await?;
    tx.commit().await?;
    Ok(Json(SecretMetadata {
        id,
        purpose,
        allowed_hosts,
        backend: "encrypted_database".into(),
        key_version: encrypted.key_version,
        created_at: row.get("created_at"),
        updated_at: now,
    }))
}

pub async fn list_secrets(
    State(state): State<AppState>,
    headers: HeaderMap,
) -> Result<Json<Vec<SecretMetadata>>, AppError> {
    let user = require_user(&state, &headers).await?;
    require_vault_admin(&user)?;
    let rows = sqlx::query(
        "SELECT id,purpose,allowed_hosts,backend,key_version,created_at,updated_at FROM secret_references \
         WHERE profile_id=$1 AND backend='encrypted_database' ORDER BY created_at DESC",
    )
    .bind(user.profile_id)
    .fetch_all(&state.pool)
    .await?;
    Ok(Json(
        rows.into_iter()
            .map(|row| SecretMetadata {
                id: row.get("id"),
                purpose: row.get("purpose"),
                allowed_hosts: row.get("allowed_hosts"),
                backend: row.get("backend"),
                key_version: row.get("key_version"),
                created_at: row.get("created_at"),
                updated_at: row.get("updated_at"),
            })
            .collect(),
    ))
}

pub async fn delete_secret(
    State(state): State<AppState>,
    headers: HeaderMap,
    Path(id): Path<String>,
) -> Result<StatusCode, AppError> {
    let user = require_user(&state, &headers).await?;
    require_vault_admin(&user)?;
    let mut tx = state.pool.begin().await?;
    let result = sqlx::query("DELETE FROM secret_references WHERE id=$1 AND profile_id=$2")
        .bind(&id)
        .bind(user.profile_id)
        .execute(&mut *tx)
        .await?;
    if result.rows_affected() == 0 {
        return Err(AppError::NotFound);
    }
    audit(
        &mut tx,
        Some(user.id),
        Some(user.profile_id),
        "secret.deleted",
        "secret",
        Some(id),
        "success",
    )
    .await?;
    tx.commit().await?;
    Ok(StatusCode::NO_CONTENT)
}

pub async fn rotate_secrets(
    State(state): State<AppState>,
    headers: HeaderMap,
) -> Result<Json<RotationResponse>, AppError> {
    let user = require_user(&state, &headers).await?;
    require_vault_admin(&user)?;
    let mut tx = state.pool.begin().await?;
    let rotated = state.vault.rotate_profile(&mut tx, user.profile_id).await?;
    audit(
        &mut tx,
        Some(user.id),
        Some(user.profile_id),
        "secret.keys_rotated",
        "profile",
        Some(user.profile_id.to_string()),
        "success",
    )
    .await?;
    tx.commit().await?;
    Ok(Json(RotationResponse { rotated }))
}

fn require_vault_admin(user: &AuthenticatedUser) -> Result<(), AppError> {
    if matches!(user.role.as_str(), "OWNER" | "ADMIN") {
        Ok(())
    } else {
        Err(AppError::Forbidden)
    }
}

fn validate_purpose(purpose: &str) -> Result<(), AppError> {
    if purpose.is_empty() || purpose.len() > 100 {
        return Err(AppError::Validation(
            "secret purpose must contain 1 to 100 characters".into(),
        ));
    }
    Ok(())
}

fn normalize_hosts(hosts: Vec<String>) -> Result<Vec<String>, AppError> {
    let mut hosts: Vec<_> = hosts
        .into_iter()
        .map(|host| host.trim().trim_end_matches('.').to_ascii_lowercase())
        .filter(|host| !host.is_empty())
        .collect();
    hosts.sort();
    hosts.dedup();
    if hosts.len() > 20
        || hosts.iter().any(|host| {
            host.len() > 253
                || !host.chars().all(|character| {
                    character.is_ascii_alphanumeric() || matches!(character, '.' | '-')
                })
        })
    {
        return Err(AppError::Validation(
            "allowed_hosts must contain at most 20 valid DNS names or IP addresses".into(),
        ));
    }
    Ok(hosts)
}
