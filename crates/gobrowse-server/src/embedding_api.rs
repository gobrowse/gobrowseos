use axum::{
    Json,
    extract::{Path, State},
    http::{HeaderMap, StatusCode},
};
use serde::{Deserialize, Serialize};
use sqlx::Row;
use time::OffsetDateTime;
use url::Url;
use uuid::Uuid;

use crate::{
    AppState,
    auth::{AuthenticatedUser, audit, require_user},
    embedding,
    error::AppError,
};

#[derive(Deserialize)]
pub struct CreateConfigurationRequest {
    pub display_name: String,
    pub provider_type: String,
    pub base_url: Url,
    pub secret_reference: Option<String>,
    pub model_reference: String,
    pub dimensions: i32,
    #[serde(default)]
    pub activate: bool,
}

#[derive(Serialize)]
pub struct EmbeddingConfiguration {
    pub id: String,
    pub provider_id: String,
    pub display_name: String,
    pub provider_type: String,
    pub base_url: String,
    pub model_reference: String,
    pub dimensions: i32,
    pub authenticated: bool,
    pub active: bool,
}

#[derive(Serialize)]
pub struct EmbeddingJobResponse {
    pub id: Uuid,
    pub book_id: Uuid,
    pub embedding_model_id: String,
    pub target_revision: i64,
    pub status: String,
    pub attempts: i32,
    pub max_attempts: i32,
    pub last_error_code: Option<String>,
    pub created_at: OffsetDateTime,
    pub updated_at: OffsetDateTime,
}

pub async fn create_configuration(
    State(state): State<AppState>,
    headers: HeaderMap,
    Json(input): Json<CreateConfigurationRequest>,
) -> Result<(StatusCode, Json<EmbeddingConfiguration>), AppError> {
    let user = require_user(&state, &headers).await?;
    require_admin(&user)?;
    validate_configuration(&input)?;
    embedding::validate_provider_endpoint(
        &input.base_url,
        &input.provider_type,
        input.secret_reference.is_some(),
        state.settings.features.local_embeddings,
    )
    .await?;
    if let Some(secret_id) = &input.secret_reference {
        let host = input
            .base_url
            .host_str()
            .expect("validated provider URL has a host")
            .trim_end_matches('.')
            .to_ascii_lowercase();
        let exists: bool = sqlx::query_scalar(
            "SELECT EXISTS(SELECT 1 FROM secret_references WHERE id=$1 AND profile_id=$2 \
             AND purpose='provider_credential' AND backend='encrypted_database' AND $3=ANY(allowed_hosts))",
        ).bind(secret_id).bind(user.profile_id).bind(host).fetch_one(&state.pool).await?;
        if !exists {
            return Err(AppError::Validation(
                "provider credential does not exist in this profile".into(),
            ));
        }
    }
    let provider_id = format!("provider_{}", Uuid::now_v7());
    let model_id = format!("embedding_{}", Uuid::now_v7());
    let mut tx = state.pool.begin().await?;
    sqlx::query(
        "INSERT INTO providers (id, profile_id, provider_type, display_name, base_url, secret_reference) VALUES ($1,$2,$3,$4,$5,$6)",
    ).bind(&provider_id).bind(user.profile_id).bind(&input.provider_type).bind(input.display_name.trim())
    .bind(input.base_url.as_str()).bind(&input.secret_reference).execute(&mut *tx).await?;
    sqlx::query("INSERT INTO embedding_models (id, provider_id, model_reference, dimensions) VALUES ($1,$2,$3,$4)")
        .bind(&model_id).bind(&provider_id).bind(input.model_reference.trim()).bind(input.dimensions)
        .execute(&mut *tx).await?;
    if input.activate {
        activate(&mut tx, user.profile_id, &model_id).await?;
    }
    audit(
        &mut tx,
        Some(user.id),
        Some(user.profile_id),
        "embedding.configuration_created",
        "embedding_model",
        Some(model_id.clone()),
        "success",
    )
    .await?;
    tx.commit().await?;
    Ok((
        StatusCode::CREATED,
        Json(EmbeddingConfiguration {
            id: model_id,
            provider_id,
            display_name: input.display_name.trim().into(),
            provider_type: input.provider_type,
            base_url: input.base_url.to_string(),
            model_reference: input.model_reference.trim().into(),
            dimensions: input.dimensions,
            authenticated: input.secret_reference.is_some(),
            active: input.activate,
        }),
    ))
}

pub async fn list_configurations(
    State(state): State<AppState>,
    headers: HeaderMap,
) -> Result<Json<Vec<EmbeddingConfiguration>>, AppError> {
    let user = require_user(&state, &headers).await?;
    require_admin(&user)?;
    let rows = sqlx::query(
        "SELECT em.id, p.id AS provider_id, p.display_name, p.provider_type, p.base_url, em.model_reference, em.dimensions, \
         p.secret_reference IS NOT NULL AS authenticated, profile.active_embedding_model_id=em.id AS active \
         FROM embedding_models em JOIN providers p ON p.id=em.provider_id JOIN profiles profile ON profile.id=p.profile_id \
         WHERE p.profile_id=$1 ORDER BY p.display_name, em.model_reference",
    ).bind(user.profile_id).fetch_all(&state.pool).await?;
    Ok(Json(
        rows.into_iter()
            .map(|row| EmbeddingConfiguration {
                id: row.get("id"),
                provider_id: row.get("provider_id"),
                display_name: row.get("display_name"),
                provider_type: row.get("provider_type"),
                base_url: row.get("base_url"),
                model_reference: row.get("model_reference"),
                dimensions: row.get("dimensions"),
                authenticated: row.get("authenticated"),
                active: row.get("active"),
            })
            .collect(),
    ))
}

pub async fn activate_configuration(
    State(state): State<AppState>,
    headers: HeaderMap,
    Path(id): Path<String>,
) -> Result<StatusCode, AppError> {
    let user = require_user(&state, &headers).await?;
    require_admin(&user)?;
    let mut tx = state.pool.begin().await?;
    let owned: bool = sqlx::query_scalar(
        "SELECT EXISTS(SELECT 1 FROM embedding_models em JOIN providers p ON p.id=em.provider_id WHERE em.id=$1 AND p.profile_id=$2 AND em.enabled AND p.enabled)",
    ).bind(&id).bind(user.profile_id).fetch_one(&mut *tx).await?;
    if !owned {
        return Err(AppError::NotFound);
    }
    activate(&mut tx, user.profile_id, &id).await?;
    audit(
        &mut tx,
        Some(user.id),
        Some(user.profile_id),
        "embedding.configuration_activated",
        "embedding_model",
        Some(id),
        "success",
    )
    .await?;
    tx.commit().await?;
    Ok(StatusCode::NO_CONTENT)
}

pub async fn list_jobs(
    State(state): State<AppState>,
    headers: HeaderMap,
) -> Result<Json<Vec<EmbeddingJobResponse>>, AppError> {
    let user = require_user(&state, &headers).await?;
    require_admin(&user)?;
    let rows = sqlx::query(
        "SELECT j.id,j.book_id,j.embedding_model_id,j.target_revision,j.status,j.attempts,j.max_attempts,j.last_error_code,j.created_at,j.updated_at \
         FROM embedding_jobs j JOIN books b ON b.id=j.book_id WHERE b.profile_id=$1 ORDER BY j.created_at DESC LIMIT 200",
    ).bind(user.profile_id).fetch_all(&state.pool).await?;
    Ok(Json(
        rows.into_iter()
            .map(|row| EmbeddingJobResponse {
                id: row.get("id"),
                book_id: row.get("book_id"),
                embedding_model_id: row.get("embedding_model_id"),
                target_revision: row.get("target_revision"),
                status: row.get("status"),
                attempts: row.get("attempts"),
                max_attempts: row.get("max_attempts"),
                last_error_code: row.get("last_error_code"),
                created_at: row.get("created_at"),
                updated_at: row.get("updated_at"),
            })
            .collect(),
    ))
}

pub async fn retry_job(
    State(state): State<AppState>,
    headers: HeaderMap,
    Path(id): Path<Uuid>,
) -> Result<StatusCode, AppError> {
    let user = require_user(&state, &headers).await?;
    require_admin(&user)?;
    let mut tx = state.pool.begin().await?;
    let row = sqlx::query(
        "SELECT j.book_id,j.embedding_model_id,j.target_revision FROM embedding_jobs j \
         JOIN books b ON b.id=j.book_id WHERE j.id=$1 AND b.profile_id=$2 \
         AND j.status IN ('failed','canceled') FOR UPDATE OF j",
    )
    .bind(id)
    .bind(user.profile_id)
    .fetch_optional(&mut *tx)
    .await?
    .ok_or(AppError::Conflict(
        "only failed or canceled embedding jobs can be retried",
    ))?;
    let book_id: Uuid = row.get("book_id");
    sqlx::query("SELECT pg_advisory_xact_lock(hashtextextended($1::text,0))")
        .bind(book_id)
        .execute(&mut *tx)
        .await?;
    let active: bool = sqlx::query_scalar(
        "SELECT EXISTS(SELECT 1 FROM embedding_jobs WHERE id<>$1 AND book_id=$2 \
         AND embedding_model_id=$3 AND target_revision=$4 AND status IN ('queued','retry','running'))",
    )
    .bind(id)
    .bind(book_id)
    .bind(row.get::<String, _>("embedding_model_id"))
    .bind(row.get::<i64, _>("target_revision"))
    .fetch_one(&mut *tx)
    .await?;
    if active {
        return Err(AppError::Conflict(
            "an active embedding job already covers this Book revision",
        ));
    }
    sqlx::query(
        "UPDATE embedding_jobs SET status='retry',attempts=0,available_at=now(), \
         last_error_code=NULL,last_error_detail=NULL,updated_at=now() WHERE id=$1",
    )
    .bind(id)
    .execute(&mut *tx)
    .await?;
    audit(
        &mut tx,
        Some(user.id),
        Some(user.profile_id),
        "embedding.job_retried",
        "embedding_job",
        Some(id.to_string()),
        "success",
    )
    .await?;
    tx.commit().await?;
    Ok(StatusCode::NO_CONTENT)
}

async fn activate(
    tx: &mut sqlx::Transaction<'_, sqlx::Postgres>,
    profile_id: Uuid,
    model_id: &str,
) -> Result<(), AppError> {
    sqlx::query("UPDATE profiles SET active_embedding_model_id=$1, updated_at=now() WHERE id=$2")
        .bind(model_id)
        .bind(profile_id)
        .execute(&mut **tx)
        .await?;
    sqlx::query("UPDATE books SET embedding_status='stale' WHERE profile_id=$1 AND embedding_model_id IS DISTINCT FROM $2")
        .bind(profile_id).bind(model_id).execute(&mut **tx).await?;
    sqlx::query(
        "INSERT INTO embedding_jobs (id, book_id, embedding_model_id, target_revision) \
         SELECT gen_random_uuid(), id, $1, revision FROM books WHERE profile_id=$2 ON CONFLICT DO NOTHING",
    ).bind(model_id).bind(profile_id).execute(&mut **tx).await?;
    Ok(())
}

fn validate_configuration(input: &CreateConfigurationRequest) -> Result<(), AppError> {
    if !matches!(input.provider_type.as_str(), "openai_compatible" | "ollama") {
        return Err(AppError::Validation(
            "provider_type must be openai_compatible or ollama".into(),
        ));
    }
    if input.display_name.trim().is_empty()
        || input.display_name.chars().count() > 200
        || input.model_reference.trim().is_empty()
        || input.model_reference.chars().count() > 300
        || !(1..=16_000).contains(&input.dimensions)
    {
        return Err(AppError::Validation(
            "invalid embedding provider name, model reference, or dimensions".into(),
        ));
    }
    if !matches!(input.base_url.scheme(), "http" | "https")
        || input.base_url.host_str().is_none()
        || !input.base_url.username().is_empty()
        || input.base_url.password().is_some()
        || input.base_url.query().is_some()
        || input.base_url.fragment().is_some()
    {
        return Err(AppError::Validation(
            "base_url must be an http(s) origin or path without credentials, query, or fragment"
                .into(),
        ));
    }
    Ok(())
}

fn require_admin(user: &AuthenticatedUser) -> Result<(), AppError> {
    if matches!(user.role.as_str(), "OWNER" | "ADMIN") {
        Ok(())
    } else {
        Err(AppError::Forbidden)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn base_config() -> CreateConfigurationRequest {
        CreateConfigurationRequest {
            display_name: "Test Embedding".into(),
            provider_type: "openai_compatible".into(),
            base_url: Url::parse("https://api.example.com/v1").expect("valid URL"),
            secret_reference: None,
            model_reference: "text-embedding-test".into(),
            dimensions: 1536,
            activate: false,
        }
    }

    #[test]
    fn validate_configuration_rejects_dimensions_out_of_range_and_bad_provider_type() {
        // dimensions = 0 (below 1..=16000 range)
        let mut config = base_config();
        config.dimensions = 0;
        assert!(
            validate_configuration(&config).is_err(),
            "dimensions=0 should be rejected"
        );

        // dimensions = 16_001 (above 1..=16000 range)
        let mut config = base_config();
        config.dimensions = 16_001;
        assert!(
            validate_configuration(&config).is_err(),
            "dimensions=16001 should be rejected"
        );

        // dimensions = 16_000 (valid boundary)
        let mut config = base_config();
        config.dimensions = 16_000;
        assert!(
            validate_configuration(&config).is_ok(),
            "dimensions=16000 should be accepted"
        );

        // dimensions = 1 (valid boundary)
        let mut config = base_config();
        config.dimensions = 1;
        assert!(
            validate_configuration(&config).is_ok(),
            "dimensions=1 should be accepted"
        );

        // bad provider_type
        let mut config = base_config();
        config.provider_type = "anthropic".into();
        assert!(
            validate_configuration(&config).is_err(),
            "non-ollama/openai provider_type should be rejected"
        );
    }
}
