use axum::{
    Json,
    extract::State,
    http::{HeaderMap, StatusCode},
};
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

#[derive(Deserialize)]
pub struct UpdateWorkspaceRequest {
    #[serde(default)]
    pub title: Option<String>,
    #[serde(default)]
    pub description: Option<String>,
    #[serde(default)]
    pub network_policy: Option<String>,
    #[serde(default)]
    pub model_preference: Option<String>,
}

#[derive(Debug, Serialize)]
pub struct WorkspaceResponse {
    pub id: Uuid,
    pub title: String,
    pub description: String,
    pub network_policy: String,
    pub model_preference: Option<String>,
    pub created_at: OffsetDateTime,
    pub updated_at: OffsetDateTime,
}

pub async fn create_workspace(
    State(state): State<AppState>,
    headers: HeaderMap,
    Json(input): Json<CreateWorkspaceRequest>,
) -> Result<Json<WorkspaceResponse>, AppError> {
    let user = require_user(&state, &headers).await?;
    if user.role == "VIEWER" {
        return Err(AppError::Forbidden);
    }
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
        "INSERT INTO workspaces (id, profile_id, title, description, created_by_user_id, created_at, updated_at) \
         VALUES ($1,$2,$3,$4,$5,$6,$6)",
    )
    .bind(id)
    .bind(user.profile_id)
    .bind(title)
    .bind(&input.description)
    .bind(user.id)
    .bind(now)
    .execute(&mut *tx)
    .await?;
    sqlx::query(
        "INSERT INTO workspace_memberships (workspace_id,user_id,access) VALUES ($1,$2,'OWNER')",
    )
    .bind(id)
    .bind(user.id)
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
        model_preference: None,
        created_at: now,
        updated_at: now,
    }))
}

/// Delete a workspace. Refuses while worktrees or workspace-scoped
/// conversations exist (409 with a hint). OWNER/ADMIN only.
/// Update workspace fields (title/description/network_policy/model_preference).
/// OWNER/ADMIN only. Network policy validated against the schema CHECK.
pub async fn update_workspace(
    State(state): State<AppState>,
    headers: HeaderMap,
    axum::extract::Path(id): axum::extract::Path<Uuid>,
    Json(input): Json<UpdateWorkspaceRequest>,
) -> Result<Json<WorkspaceResponse>, AppError> {
    let user = require_user(&state, &headers).await?;
    if !matches!(user.role.as_str(), "OWNER" | "ADMIN") {
        return Err(AppError::Forbidden);
    }
    let mut tx = state.pool.begin().await?;
    let owned: Option<bool> =
        sqlx::query_scalar("SELECT EXISTS(SELECT 1 FROM workspaces WHERE id=$1 AND profile_id=$2)")
            .bind(id)
            .bind(user.profile_id)
            .fetch_optional(&mut *tx)
            .await?;
    if owned != Some(true) {
        return Err(AppError::NotFound);
    }
    let mut title = input.title;
    let mut description = input.description;
    let mut model_preference = input.model_preference;
    let network_policy = input.network_policy;
    if title.as_deref().is_some_and(|t| t.trim().is_empty()) {
        return Err(AppError::Validation("title cannot be empty".into()));
    }
    if let Some(policy) = &network_policy
        && !matches!(policy.as_str(), "NONE" | "RESTRICTED" | "FULL")
    {
        return Err(AppError::Validation(
            "network_policy must be NONE, RESTRICTED, or FULL".into(),
        ));
    }
    // Coalesce: only update provided fields.
    title = title
        .map(|t| t.trim().to_string())
        .filter(|t| !t.is_empty());
    description = description
        .map(|d| d.trim().to_string())
        .filter(|d| !d.is_empty());
    model_preference = model_preference
        .map(|m| m.trim().to_string())
        .filter(|m| !m.is_empty());
    sqlx::query(
        "UPDATE workspaces SET          title = COALESCE($2, title),          description = COALESCE($3, description),          network_policy = COALESCE($4, network_policy),          model_preference = COALESCE($5, model_preference),          updated_at = now()          WHERE id = $1",
    )
    .bind(id)
    .bind(&title)
    .bind(&description)
    .bind(&network_policy)
    .bind(&model_preference)
    .execute(&mut *tx)
    .await?;
    let row = sqlx::query(
        "SELECT id, title, description, network_policy, model_preference,          created_at, updated_at FROM workspaces WHERE id=$1",
    )
    .bind(id)
    .fetch_one(&mut *tx)
    .await?;
    audit(
        &mut tx,
        Some(user.id),
        Some(user.profile_id),
        "workspace.updated",
        "workspace",
        Some(id.to_string()),
        "success",
    )
    .await?;
    tx.commit().await?;
    Ok(Json(WorkspaceResponse {
        id: row.get("id"),
        title: row.get("title"),
        description: row.get("description"),
        network_policy: row.get("network_policy"),
        model_preference: row.get("model_preference"),
        created_at: row.get("created_at"),
        updated_at: row.get("updated_at"),
    }))
}

pub async fn delete_workspace(
    State(state): State<AppState>,
    headers: HeaderMap,
    axum::extract::Path(id): axum::extract::Path<Uuid>,
) -> Result<StatusCode, AppError> {
    let user = require_user(&state, &headers).await?;
    if !matches!(user.role.as_str(), "OWNER" | "ADMIN") {
        return Err(AppError::Forbidden);
    }
    let mut tx = state.pool.begin().await?;
    let owned: Option<bool> =
        sqlx::query_scalar("SELECT EXISTS(SELECT 1 FROM workspaces WHERE id=$1 AND profile_id=$2)")
            .bind(id)
            .bind(user.profile_id)
            .fetch_optional(&mut *tx)
            .await?;
    if owned != Some(true) {
        return Err(AppError::NotFound);
    }
    let worktrees: i64 = sqlx::query_scalar(
        "SELECT count(*) FROM worktrees WHERE workspace_id=$1 AND status <> 'deleted'",
    )
    .bind(id)
    .fetch_one(&mut *tx)
    .await?;
    let conversations: i64 =
        sqlx::query_scalar("SELECT count(*) FROM conversations WHERE workspace_id=$1")
            .bind(id)
            .fetch_one(&mut *tx)
            .await?;
    if worktrees > 0 || conversations > 0 {
        return Err(AppError::Conflict(
            "delete the workspace's worktrees and conversations first",
        ));
    }
    sqlx::query("DELETE FROM workspaces WHERE id=$1")
        .bind(id)
        .execute(&mut *tx)
        .await?;
    audit(
        &mut tx,
        Some(user.id),
        Some(user.profile_id),
        "workspace.deleted",
        "workspace",
        Some(id.to_string()),
        "success",
    )
    .await?;
    tx.commit().await?;
    Ok(StatusCode::NO_CONTENT)
}

pub async fn list_workspaces(
    State(state): State<AppState>,
    headers: HeaderMap,
) -> Result<Json<Vec<WorkspaceResponse>>, AppError> {
    let user = require_user(&state, &headers).await?;
    let rows = sqlx::query(
        "SELECT id, title, description, network_policy, model_preference, created_at, updated_at \
         FROM workspaces workspace WHERE profile_id=$1 AND ($3 IN ('OWNER','ADMIN') OR EXISTS( \
         SELECT 1 FROM workspace_memberships member WHERE member.workspace_id=workspace.id AND member.user_id=$2)) \
         ORDER BY updated_at DESC LIMIT 200",
    )
    .bind(user.profile_id)
    .bind(user.id)
    .bind(&user.role)
    .fetch_all(&state.pool)
    .await?;
    Ok(Json(
        rows.into_iter()
            .map(|row| WorkspaceResponse {
                id: row.get("id"),
                title: row.get("title"),
                description: row.get("description"),
                network_policy: row.get("network_policy"),
                model_preference: row.get("model_preference"),
                created_at: row.get("created_at"),
                updated_at: row.get("updated_at"),
            })
            .collect(),
    ))
}
