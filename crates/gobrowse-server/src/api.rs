use axum::{Json, extract::State, http::HeaderMap};
use gobrowse_core::VersionInfo;
use serde::{Deserialize, Serialize};
use sqlx::Row;
use time::OffsetDateTime;
use uuid::Uuid;

use crate::{
    AppState,
    auth::{audit, require_user},
    db,
    error::AppError,
};

#[derive(Debug, Serialize)]
pub struct HealthResponse {
    pub status: &'static str,
    pub version: &'static str,
}

pub async fn live() -> Json<HealthResponse> {
    Json(HealthResponse {
        status: "live",
        version: env!("CARGO_PKG_VERSION"),
    })
}

pub async fn ready(State(state): State<AppState>) -> Result<Json<HealthResponse>, AppError> {
    db::ready(&state.pool).await?;
    Ok(Json(HealthResponse {
        status: "ready",
        version: env!("CARGO_PKG_VERSION"),
    }))
}

pub async fn version(State(state): State<AppState>) -> Result<Json<VersionInfo>, AppError> {
    let schema_version: i64 =
        sqlx::query_scalar("SELECT schema_version FROM schema_metadata WHERE singleton")
            .fetch_one(&state.pool)
            .await?;
    Ok(Json(VersionInfo {
        version: env!("CARGO_PKG_VERSION").into(),
        api_version: "v1".into(),
        schema_version,
        build_commit: option_env!("GOBROWSE_BUILD_COMMIT").map(str::to_owned),
    }))
}

#[derive(Debug, Deserialize)]
pub struct CreateWorkspaceRequest {
    pub title: String,
    #[serde(default)]
    pub description: String,
}

#[derive(Debug, Serialize)]
pub struct WorkspaceResponse {
    pub id: Uuid,
    pub title: String,
    pub description: String,
    pub network_policy: String,
    pub created_at: OffsetDateTime,
    pub updated_at: OffsetDateTime,
}

pub async fn create_workspace(
    State(state): State<AppState>,
    headers: HeaderMap,
    Json(input): Json<CreateWorkspaceRequest>,
) -> Result<Json<WorkspaceResponse>, AppError> {
    let user = require_user(&state, &headers).await?;
    let title = input.title.trim();
    if title.is_empty() || title.chars().count() > 300 {
        return Err(AppError::Validation(
            "workspace title must contain 1 to 300 characters".into(),
        ));
    }
    if input.description.chars().count() > 20_000 {
        return Err(AppError::Validation(
            "workspace description exceeds 20000 characters".into(),
        ));
    }
    let id = Uuid::now_v7();
    let now = OffsetDateTime::now_utc();
    let mut tx = state.pool.begin().await?;
    sqlx::query(
        "INSERT INTO workspaces (id, profile_id, title, description, created_at, updated_at) \
         VALUES ($1,$2,$3,$4,$5,$5)",
    )
    .bind(id)
    .bind(user.profile_id)
    .bind(title)
    .bind(&input.description)
    .bind(now)
    .execute(&mut *tx)
    .await?;
    audit(
        &mut tx,
        Some(user.id),
        Some(user.profile_id),
        "workspace.created",
        "workspace",
        Some(id.to_string()),
        "success",
    )
    .await?;
    tx.commit().await?;
    Ok(Json(WorkspaceResponse {
        id,
        title: title.into(),
        description: input.description,
        network_policy: "RESTRICTED".into(),
        created_at: now,
        updated_at: now,
    }))
}

pub async fn list_workspaces(
    State(state): State<AppState>,
    headers: HeaderMap,
) -> Result<Json<Vec<WorkspaceResponse>>, AppError> {
    let user = require_user(&state, &headers).await?;
    let rows = sqlx::query(
        "SELECT id, title, description, network_policy, created_at, updated_at \
         FROM workspaces WHERE profile_id = $1 ORDER BY updated_at DESC LIMIT 200",
    )
    .bind(user.profile_id)
    .fetch_all(&state.pool)
    .await?;
    Ok(Json(
        rows.into_iter()
            .map(|row| WorkspaceResponse {
                id: row.get("id"),
                title: row.get("title"),
                description: row.get("description"),
                network_policy: row.get("network_policy"),
                created_at: row.get("created_at"),
                updated_at: row.get("updated_at"),
            })
            .collect(),
    ))
}
