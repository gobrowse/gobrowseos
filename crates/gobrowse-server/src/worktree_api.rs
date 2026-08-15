use axum::{
    Json,
    extract::{Path, Query, State},
    http::{HeaderMap, StatusCode},
};
use gobrowse_core::worktrees::{
    validate_base_commit, validate_branch, validate_changed_files, worktree_path,
};
use serde::{Deserialize, Serialize};
use serde_json::json;
use sqlx::{Postgres, Row, Transaction};
use time::OffsetDateTime;
use uuid::Uuid;

use crate::{
    AppState,
    auth::{audit, require_user},
    error::AppError,
    task_api::{
        append_activity, authorize_workspace, authorize_workspace_in_transaction, require_writer,
    },
};

const WORKTREE_CREATED: &str = "WORKTREE_CREATED";
const FILES_CHANGED: &str = "FILES_CHANGED";
const WORKTREE_DELETED: &str = "WORKTREE_DELETED";

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct WorktreeQuery {
    pub limit: Option<i64>,
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct CreateWorktreeRequest {
    pub task_id: Uuid,
    pub owner_agent_id: Uuid,
    pub repository_root: String,
    pub branch: Option<String>,
    pub base_commit: String,
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct UpdateWorktreeRequest {
    pub changed_files: Vec<String>,
}

#[derive(Debug, Serialize)]
pub struct WorktreeResponse {
    pub id: Uuid,
    pub workspace_id: Uuid,
    pub task_id: Uuid,
    pub owner_agent_id: Uuid,
    pub branch: String,
    pub base_commit: String,
    pub path: String,
    pub status: String,
    pub changed_files: Vec<String>,
    pub last_activity_at: OffsetDateTime,
}

pub async fn list_worktrees(
    State(state): State<AppState>,
    headers: HeaderMap,
    Path(workspace_id): Path<Uuid>,
    Query(query): Query<WorktreeQuery>,
) -> Result<Json<Vec<WorktreeResponse>>, AppError> {
    let user = require_user(&state, &headers).await?;
    authorize_workspace(&state, &user, workspace_id, false).await?;
    let limit = query.limit.unwrap_or(200).clamp(1, 500);
    let rows = sqlx::query(
        "SELECT id,workspace_id,task_id,owner_agent_id,branch,base_commit,path,status,changed_files,last_activity_at \
         FROM worktrees WHERE workspace_id=$1 ORDER BY last_activity_at DESC,id DESC LIMIT $2",
    )
    .bind(workspace_id)
    .bind(limit)
    .fetch_all(&state.pool)
    .await?;
    Ok(Json(rows.iter().map(row_to_worktree).collect()))
}

pub async fn create_worktree(
    State(state): State<AppState>,
    headers: HeaderMap,
    Path(workspace_id): Path<Uuid>,
    Json(input): Json<CreateWorktreeRequest>,
) -> Result<(StatusCode, Json<WorktreeResponse>), AppError> {
    let user = require_user(&state, &headers).await?;
    require_writer(&user)?;
    validate_base_commit(&input.base_commit).map_err(validation)?;
    let root = std::path::Path::new(&input.repository_root);
    let mut tx = state.pool.begin().await?;
    authorize_workspace_in_transaction(&mut tx, &user, workspace_id, true).await?;

    let task: Option<(String, Uuid)> =
        sqlx::query_as("SELECT title,id FROM tasks WHERE workspace_id=$1 AND id=$2")
            .bind(workspace_id)
            .bind(input.task_id)
            .fetch_optional(&mut *tx)
            .await?;
    let (title, task_id) = task.ok_or_else(|| validation("task must belong to this workspace"))?;
    let agent_exists: bool =
        sqlx::query_scalar("SELECT EXISTS(SELECT 1 FROM agents WHERE workspace_id=$1 AND id=$2)")
            .bind(workspace_id)
            .bind(input.owner_agent_id)
            .fetch_one(&mut *tx)
            .await?;
    if !agent_exists {
        return Err(validation("owner agent must belong to this workspace"));
    }

    let branch = input
        .branch
        .unwrap_or_else(|| gobrowse_core::worktrees::task_branch(task_id, &title));
    validate_branch(&branch).map_err(validation)?;
    let path = worktree_path(root, task_id).map_err(validation)?;
    let changed_files: Vec<String> = Vec::new();
    let id = Uuid::now_v7();
    let row = sqlx::query::<Postgres>(
        "INSERT INTO worktrees (id,workspace_id,task_id,owner_agent_id,branch,base_commit,path,status,changed_files) \
         VALUES ($1,$2,$3,$4,$5,$6,$7,'ACTIVE',$8) \
         RETURNING id,workspace_id,task_id,owner_agent_id,branch,base_commit,path,status,changed_files,last_activity_at",
    )
    .bind(id)
    .bind(workspace_id)
    .bind(task_id)
    .bind(input.owner_agent_id)
    .bind(&branch)
    .bind(&input.base_commit)
    .bind(path.to_string_lossy().to_string())
    .bind(&changed_files)
    .fetch_optional(&mut *tx)
    .await
    .map_err(conflict_from_database)?
    .ok_or(AppError::Conflict("worktree branch or path is already owned"))?;
    let response = row_to_worktree(&row);
    append_activity(
        &mut tx,
        workspace_id,
        Some(task_id),
        None,
        Some(user.id),
        WORKTREE_CREATED,
        json!({
            "worktree_id": id,
            "branch": branch,
            "base_commit": input.base_commit,
            "owner_agent_id": input.owner_agent_id
        }),
    )
    .await?;
    audit(
        &mut tx,
        Some(user.id),
        Some(user.profile_id),
        "worktree.created",
        "worktree",
        Some(id.to_string()),
        "success",
    )
    .await?;
    tx.commit().await?;
    Ok((StatusCode::CREATED, Json(response)))
}

pub async fn get_worktree(
    State(state): State<AppState>,
    headers: HeaderMap,
    Path(id): Path<Uuid>,
) -> Result<Json<WorktreeResponse>, AppError> {
    let user = require_user(&state, &headers).await?;
    let row = authorized_worktree(&state, &user, id, false).await?;
    Ok(Json(row_to_worktree(&row)))
}

pub async fn update_worktree(
    State(state): State<AppState>,
    headers: HeaderMap,
    Path(id): Path<Uuid>,
    Json(input): Json<UpdateWorktreeRequest>,
) -> Result<Json<WorktreeResponse>, AppError> {
    let user = require_user(&state, &headers).await?;
    require_writer(&user)?;
    if input.changed_files.is_empty() {
        return Err(validation("changed_files must not be empty"));
    }
    let changed_files = validate_changed_files(&input.changed_files).map_err(validation)?;
    let mut tx = state.pool.begin().await?;
    let row = authorized_worktree_in_transaction(&mut tx, &user, id, true).await?;
    let old_files: Vec<String> = row.get("changed_files");
    if old_files == changed_files {
        return Err(validation(
            "changed_files must differ from the current inventory",
        ));
    }
    let updated = sqlx::query::<Postgres>(
        "UPDATE worktrees SET changed_files=$1,last_activity_at=now() WHERE id=$2 \
         RETURNING id,workspace_id,task_id,owner_agent_id,branch,base_commit,path,status,changed_files,last_activity_at",
    )
    .bind(&changed_files)
    .bind(id)
    .fetch_one(&mut *tx)
    .await?;
    let workspace_id: Uuid = updated.get("workspace_id");
    let task_id: Uuid = updated.get("task_id");
    append_activity(
        &mut tx,
        workspace_id,
        Some(task_id),
        None,
        Some(user.id),
        FILES_CHANGED,
        json!({ "worktree_id": id, "changed_files": changed_files }),
    )
    .await?;
    audit(
        &mut tx,
        Some(user.id),
        Some(user.profile_id),
        "worktree.files_changed",
        "worktree",
        Some(id.to_string()),
        "success",
    )
    .await?;
    tx.commit().await?;
    Ok(Json(row_to_worktree(&updated)))
}

pub async fn delete_worktree(
    State(state): State<AppState>,
    headers: HeaderMap,
    Path(id): Path<Uuid>,
) -> Result<StatusCode, AppError> {
    let user = require_user(&state, &headers).await?;
    require_writer(&user)?;
    let mut tx = state.pool.begin().await?;
    let row = authorized_worktree_in_transaction(&mut tx, &user, id, true).await?;
    let workspace_id: Uuid = row.get("workspace_id");
    let task_id: Uuid = row.get("task_id");
    let branch: String = row.get("branch");
    let owner_agent_id: Uuid = row.get("owner_agent_id");
    sqlx::query("DELETE FROM worktrees WHERE id=$1")
        .bind(id)
        .execute(&mut *tx)
        .await?;
    append_activity(
        &mut tx,
        workspace_id,
        Some(task_id),
        None,
        Some(user.id),
        WORKTREE_DELETED,
        json!({ "worktree_id": id, "branch": branch, "owner_agent_id": owner_agent_id }),
    )
    .await?;
    audit(
        &mut tx,
        Some(user.id),
        Some(user.profile_id),
        "worktree.deleted",
        "worktree",
        Some(id.to_string()),
        "success",
    )
    .await?;
    tx.commit().await?;
    Ok(StatusCode::NO_CONTENT)
}

async fn authorized_worktree(
    state: &AppState,
    user: &crate::auth::AuthenticatedUser,
    id: Uuid,
    require_write: bool,
) -> Result<sqlx::postgres::PgRow, AppError> {
    let mut tx = state.pool.begin().await?;
    let row = authorized_worktree_in_transaction(&mut tx, user, id, require_write).await?;
    tx.commit().await?;
    Ok(row)
}

async fn authorized_worktree_in_transaction(
    tx: &mut Transaction<'_, Postgres>,
    user: &crate::auth::AuthenticatedUser,
    id: Uuid,
    require_write: bool,
) -> Result<sqlx::postgres::PgRow, AppError> {
    let workspace_id: Uuid = sqlx::query_scalar(
        "SELECT workspace_id FROM worktrees WHERE id=$1 AND EXISTS(SELECT 1 FROM workspaces WHERE id=worktrees.workspace_id AND profile_id=$2)",
    )
    .bind(id)
    .bind(user.profile_id)
    .fetch_optional(&mut **tx)
    .await?
    .ok_or(AppError::NotFound)?;
    if require_write {
        authorize_workspace_in_transaction(tx, user, workspace_id, true).await?;
    }
    let query = if require_write {
        "SELECT id,workspace_id,task_id,owner_agent_id,branch,base_commit,path,status,changed_files,last_activity_at FROM worktrees WHERE id=$1 AND EXISTS(SELECT 1 FROM workspaces WHERE id=worktrees.workspace_id AND profile_id=$2) AND ($4 IN ('OWNER','ADMIN') OR EXISTS(SELECT 1 FROM workspace_memberships WHERE workspace_id=worktrees.workspace_id AND user_id=$3 AND access IN ('OWNER','EDITOR'))) FOR UPDATE"
    } else {
        "SELECT id,workspace_id,task_id,owner_agent_id,branch,base_commit,path,status,changed_files,last_activity_at FROM worktrees WHERE id=$1 AND EXISTS(SELECT 1 FROM workspaces WHERE id=worktrees.workspace_id AND profile_id=$2) AND ($4 IN ('OWNER','ADMIN') OR EXISTS(SELECT 1 FROM workspace_memberships WHERE workspace_id=worktrees.workspace_id AND user_id=$3))"
    };
    sqlx::query(query)
        .bind(id)
        .bind(user.profile_id)
        .bind(user.id)
        .bind(&user.role)
        .fetch_optional(&mut **tx)
        .await?
        .ok_or(AppError::NotFound)
}

fn row_to_worktree(row: &sqlx::postgres::PgRow) -> WorktreeResponse {
    WorktreeResponse {
        id: row.get("id"),
        workspace_id: row.get("workspace_id"),
        task_id: row.get("task_id"),
        owner_agent_id: row.get("owner_agent_id"),
        branch: row.get("branch"),
        base_commit: row.get("base_commit"),
        path: row.get("path"),
        status: row.get("status"),
        changed_files: row.get("changed_files"),
        last_activity_at: row.get("last_activity_at"),
    }
}

fn validation(error: impl ToString) -> AppError {
    AppError::Validation(error.to_string())
}

fn conflict_from_database(error: sqlx::Error) -> AppError {
    if let sqlx::Error::Database(database) = &error
        && database.constraint().is_some_and(|constraint| {
            constraint == "worktrees_workspace_id_branch_key"
                || constraint == "worktrees_workspace_id_path_key"
        })
    {
        return AppError::Conflict("worktree branch or path is already owned");
    }
    AppError::Database(error)
}
