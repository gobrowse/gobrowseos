use axum::{
    Json,
    extract::{Path, State},
    http::{HeaderMap, StatusCode},
};
use serde::{Deserialize, Serialize};
use sqlx::Row;
use url::Url;
use uuid::Uuid;

use crate::{
    AppState,
    auth::{AuthenticatedUser, audit, require_user},
    embedding,
    error::AppError,
};

#[derive(Deserialize)]
pub struct CreateChatModelRequest {
    pub display_name: String,
    pub provider_type: String,
    pub base_url: Url,
    pub secret_reference: Option<String>,
    pub model_reference: String,
    pub context_window: i32,
    pub output_limit: i32,
    #[serde(default)]
    pub priority: i32,
    #[serde(default)]
    pub activate: bool,
    #[serde(default)]
    pub fallback_model_ids: Vec<String>,
}

#[derive(Serialize)]
pub struct ChatModelResponse {
    pub id: String,
    pub display_name: String,
    pub provider_type: String,
    pub model_reference: String,
    pub context_window: i32,
    pub output_limit: i32,
    pub priority: i32,
    pub authenticated: bool,
    pub active: bool,
    pub fallback_model_ids: Vec<String>,
}

pub async fn create_chat_model(
    State(state): State<AppState>,
    headers: HeaderMap,
    Json(input): Json<CreateChatModelRequest>,
) -> Result<(StatusCode, Json<ChatModelResponse>), AppError> {
    let user = require_user(&state, &headers).await?;
    require_admin(&user)?;
    validate(&input)?;
    embedding::validate_provider_endpoint(
        &input.base_url,
        &input.provider_type,
        input.secret_reference.is_some(),
        state.settings.features.local_models,
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
        )
        .bind(secret_id)
        .bind(user.profile_id)
        .bind(host)
        .fetch_one(&state.pool)
        .await?;
        if !exists {
            return Err(AppError::Validation(
                "provider credential is unavailable or not allowed for this host".into(),
            ));
        }
    }
    let provider_id = format!("provider_{}", Uuid::now_v7());
    let model_id = format!("model_{}", Uuid::now_v7());
    let mut tx = state.pool.begin().await?;
    if !input.fallback_model_ids.is_empty() {
        let owned: i64 = sqlx::query_scalar(
            "SELECT count(*) FROM models m JOIN providers p ON p.id=m.provider_id \
             WHERE p.profile_id=$1 AND m.id=ANY($2::text[]) AND p.enabled AND m.enabled",
        )
        .bind(user.profile_id)
        .bind(&input.fallback_model_ids)
        .fetch_one(&mut *tx)
        .await?;
        if usize::try_from(owned).ok() != Some(input.fallback_model_ids.len()) {
            return Err(AppError::Validation(
                "fallback models must be enabled models in this profile".into(),
            ));
        }
    }
    sqlx::query(
        "INSERT INTO providers (id,profile_id,provider_type,display_name,base_url,secret_reference) \
         VALUES ($1,$2,$3,$4,$5,$6)",
    )
    .bind(&provider_id)
    .bind(user.profile_id)
    .bind(&input.provider_type)
    .bind(input.display_name.trim())
    .bind(input.base_url.as_str())
    .bind(&input.secret_reference)
    .execute(&mut *tx)
    .await?;
    sqlx::query(
        "INSERT INTO models (id,provider_id,model_reference,display_name,context_window,output_limit,capabilities,priority) \
         VALUES ($1,$2,$3,$4,$5,$6,ARRAY['text'],$7)",
    )
    .bind(&model_id)
    .bind(&provider_id)
    .bind(input.model_reference.trim())
    .bind(input.display_name.trim())
    .bind(input.context_window)
    .bind(input.output_limit)
    .bind(input.priority)
    .execute(&mut *tx)
    .await?;
    for (position, fallback_id) in input.fallback_model_ids.iter().enumerate() {
        sqlx::query(
            "INSERT INTO model_fallback_routes (profile_id,primary_model_id,fallback_model_id,position) \
             VALUES ($1,$2,$3,$4)",
        )
        .bind(user.profile_id)
        .bind(&model_id)
        .bind(fallback_id)
        .bind(i32::try_from(position).map_err(|_| {
            AppError::Validation("too many fallback models".into())
        })?)
        .execute(&mut *tx)
        .await?;
    }
    if input.activate {
        sqlx::query("UPDATE profiles SET active_chat_model_id=$1,updated_at=now() WHERE id=$2")
            .bind(&model_id)
            .bind(user.profile_id)
            .execute(&mut *tx)
            .await?;
    }
    audit(
        &mut tx,
        Some(user.id),
        Some(user.profile_id),
        "chat_model.created",
        "model",
        Some(model_id.clone()),
        "success",
    )
    .await?;
    tx.commit().await?;
    Ok((
        StatusCode::CREATED,
        Json(ChatModelResponse {
            id: model_id,
            display_name: input.display_name.trim().into(),
            provider_type: input.provider_type,
            model_reference: input.model_reference.trim().into(),
            context_window: input.context_window,
            output_limit: input.output_limit,
            priority: input.priority,
            authenticated: input.secret_reference.is_some(),
            active: input.activate,
            fallback_model_ids: input.fallback_model_ids,
        }),
    ))
}

pub async fn list_chat_models(
    State(state): State<AppState>,
    headers: HeaderMap,
) -> Result<Json<Vec<ChatModelResponse>>, AppError> {
    let user = require_user(&state, &headers).await?;
    let rows = sqlx::query(
        "SELECT m.id,m.display_name,m.model_reference,m.context_window,m.output_limit,m.priority, \
         ARRAY(SELECT route.fallback_model_id FROM model_fallback_routes route \
               WHERE route.profile_id=p.profile_id AND route.primary_model_id=m.id ORDER BY route.position) AS fallback_model_ids, \
         p.provider_type,p.secret_reference IS NOT NULL AS authenticated,profile.active_chat_model_id=m.id AS active \
         FROM models m JOIN providers p ON p.id=m.provider_id JOIN profiles profile ON profile.id=p.profile_id \
         WHERE p.profile_id=$1 AND p.enabled AND m.enabled AND 'text'=ANY(m.capabilities) \
         ORDER BY active DESC,m.priority DESC,m.display_name",
    )
    .bind(user.profile_id)
    .fetch_all(&state.pool)
    .await?;
    Ok(Json(
        rows.into_iter()
            .map(|row| ChatModelResponse {
                id: row.get("id"),
                display_name: row.get("display_name"),
                provider_type: row.get("provider_type"),
                model_reference: row.get("model_reference"),
                context_window: row.get("context_window"),
                output_limit: row.get("output_limit"),
                priority: row.get("priority"),
                authenticated: row.get("authenticated"),
                active: row.get("active"),
                fallback_model_ids: row.get("fallback_model_ids"),
            })
            .collect(),
    ))
}

pub async fn activate_chat_model(
    State(state): State<AppState>,
    headers: HeaderMap,
    Path(id): Path<String>,
) -> Result<StatusCode, AppError> {
    let user = require_user(&state, &headers).await?;
    require_admin(&user)?;
    let result = sqlx::query(
        "UPDATE profiles SET active_chat_model_id=$1,updated_at=now() WHERE id=$2 AND EXISTS( \
         SELECT 1 FROM models m JOIN providers p ON p.id=m.provider_id \
         WHERE m.id=$1 AND p.profile_id=$2 AND p.enabled AND m.enabled AND 'text'=ANY(m.capabilities))",
    )
    .bind(&id)
    .bind(user.profile_id)
    .execute(&state.pool)
    .await?;
    if result.rows_affected() != 1 {
        return Err(AppError::NotFound);
    }
    Ok(StatusCode::NO_CONTENT)
}

fn validate(input: &CreateChatModelRequest) -> Result<(), AppError> {
    if !matches!(input.provider_type.as_str(), "openai_compatible" | "ollama") {
        return Err(AppError::Validation(
            "provider_type must be openai_compatible or ollama".into(),
        ));
    }
    let unique: std::collections::BTreeSet<_> = input.fallback_model_ids.iter().collect();
    if input.fallback_model_ids.len() > 5 || unique.len() != input.fallback_model_ids.len() {
        return Err(AppError::Validation(
            "configure at most five distinct fallback models".into(),
        ));
    }
    if input.display_name.trim().is_empty()
        || input.display_name.chars().count() > 200
        || input.model_reference.trim().is_empty()
        || input.model_reference.chars().count() > 300
        || !(256..=10_000_000).contains(&input.context_window)
        || !(1..=1_000_000).contains(&input.output_limit)
        || input.output_limit >= input.context_window
    {
        return Err(AppError::Validation(
            "invalid chat model configuration".into(),
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
            "base_url must be an http(s) URL without credentials, query, or fragment".into(),
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
