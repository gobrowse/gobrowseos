use std::collections::HashSet;

use axum::{
    Json,
    extract::{Path, Query, State},
    http::{HeaderMap, StatusCode},
};
use gobrowse_core::activity::TaskState;
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use sqlx::{Postgres, Row, Transaction};
use time::OffsetDateTime;
use uuid::Uuid;

use crate::{
    AppState,
    auth::{audit, require_user},
    error::AppError,
};

const MAX_ACTIVITY_PAYLOAD_CHARS: usize = 100_000;
const MAX_DEPENDENCIES: usize = 200;
const ACTIVITY_KINDS: &[&str] = &[
    "TASK_CREATED",
    "TASK_ASSIGNED",
    "AGENT_STARTED",
    "AGENT_STOPPED",
    "WORKTREE_CREATED",
    "FILES_CHANGED",
    "COMMIT_CREATED",
    "TEST_STARTED",
    "TEST_COMPLETED",
    "BLOCKED",
    "WAITING",
    "MERGE_REQUESTED",
    "MERGED",
    "SKILL_CREATED",
    "SKILL_UPDATED",
    "BOOK_CREATED",
    "BOOK_UPDATED",
    // This is the server's task projection event; the core enum contains
    // domain events emitted by agents, while task state is persisted here.
    "TASK_STATE_CHANGED",
    "TASK_UPDATED",
];

#[derive(Debug, Deserialize)]
pub struct CreateTaskRequest {
    pub title: String,
    #[serde(default)]
    pub description: String,
    pub parent_task_id: Option<Uuid>,
    pub assigned_agent_id: Option<Uuid>,
    #[serde(default)]
    pub dependencies: Vec<Uuid>,
    pub state: Option<TaskState>,
}

#[derive(Debug, Deserialize)]
pub struct UpdateTaskRequest {
    pub title: Option<String>,
    pub description: Option<String>,
    pub state: Option<TaskState>,
    // None means omitted; Some(None) explicitly unassigns the task.
    #[serde(default)]
    pub assigned_agent_id: Option<Option<Uuid>>,
}

#[derive(Debug, Deserialize)]
pub struct TaskQuery {
    pub state: Option<TaskState>,
    pub limit: Option<i64>,
}

#[derive(Debug, Serialize)]
pub struct TaskResponse {
    pub id: Uuid,
    pub workspace_id: Uuid,
    pub parent_task_id: Option<Uuid>,
    pub title: String,
    pub description: String,
    pub state: TaskState,
    pub assigned_agent_id: Option<Uuid>,
    pub dependencies: Vec<Uuid>,
    pub created_at: OffsetDateTime,
    pub updated_at: OffsetDateTime,
}

#[derive(Debug, Deserialize)]
pub struct CreateActivityRequest {
    pub kind: String,
    pub task_id: Option<Uuid>,
    pub agent_id: Option<Uuid>,
    #[serde(default)]
    pub payload: Value,
}

#[derive(Debug, Deserialize)]
pub struct ActivityQuery {
    pub after: Option<i64>,
    pub limit: Option<i64>,
}

#[derive(Debug, Serialize)]
pub struct ActivityResponse {
    pub id: i64,
    pub workspace_id: Uuid,
    pub task_id: Option<Uuid>,
    pub agent_id: Option<Uuid>,
    pub kind: String,
    pub payload: Value,
    pub created_at: OffsetDateTime,
}

pub async fn list_tasks(
    State(state): State<AppState>,
    headers: HeaderMap,
    Path(workspace_id): Path<Uuid>,
    Query(query): Query<TaskQuery>,
) -> Result<Json<Vec<TaskResponse>>, AppError> {
    let user = require_user(&state, &headers).await?;
    authorize_workspace(&state, &user, workspace_id, false).await?;
    let state_filter = query.state.map(task_state_name);
    let rows = sqlx::query(
        "SELECT id,workspace_id,parent_task_id,title,description,state,assigned_agent_id,dependencies,created_at,updated_at \
         FROM tasks WHERE workspace_id=$1 AND ($2::text IS NULL OR state=$2) \
         ORDER BY updated_at DESC,id DESC LIMIT $3",
    )
    .bind(workspace_id)
    .bind(state_filter)
    .bind(query.limit.unwrap_or(200).clamp(1, 500))
    .fetch_all(&state.pool)
    .await?;
    Ok(Json(rows.iter().map(row_to_task).collect()))
}

pub async fn create_task(
    State(state): State<AppState>,
    headers: HeaderMap,
    Path(workspace_id): Path<Uuid>,
    Json(input): Json<CreateTaskRequest>,
) -> Result<(StatusCode, Json<TaskResponse>), AppError> {
    let user = require_user(&state, &headers).await?;
    require_writer(&user)?;
    authorize_workspace(&state, &user, workspace_id, true).await?;
    validate_task_input(&input.title, &input.description, &input.dependencies)?;
    let task_state = input.state.unwrap_or(TaskState::Backlog);
    validate_task_references(
        &state.pool,
        workspace_id,
        input.parent_task_id,
        input.assigned_agent_id,
        &input.dependencies,
    )
    .await?;

    let id = Uuid::now_v7();
    let now = OffsetDateTime::now_utc();
    let mut tx = state.pool.begin().await?;
    sqlx::query(
        "INSERT INTO tasks (id,workspace_id,parent_task_id,title,description,state,assigned_agent_id,dependencies,created_at,updated_at) \
         VALUES ($1,$2,$3,$4,$5,$6,$7,$8,$9,$9)",
    )
    .bind(id)
    .bind(workspace_id)
    .bind(input.parent_task_id)
    .bind(input.title.trim())
    .bind(&input.description)
    .bind(task_state_name(task_state))
    .bind(input.assigned_agent_id)
    .bind(&input.dependencies)
    .bind(now)
    .execute(&mut *tx)
    .await?;
    append_activity(
        &mut tx,
        workspace_id,
        Some(id),
        input.assigned_agent_id,
        "TASK_CREATED",
        json!({"state": task_state_name(task_state), "title": input.title.trim()}),
    )
    .await?;
    audit(
        &mut tx,
        Some(user.id),
        Some(user.profile_id),
        "task.created",
        "task",
        Some(id.to_string()),
        "success",
    )
    .await?;
    tx.commit().await?;
    Ok((
        StatusCode::CREATED,
        Json(TaskResponse {
            id,
            workspace_id,
            parent_task_id: input.parent_task_id,
            title: input.title.trim().into(),
            description: input.description,
            state: task_state,
            assigned_agent_id: input.assigned_agent_id,
            dependencies: input.dependencies,
            created_at: now,
            updated_at: now,
        }),
    ))
}

pub async fn get_task(
    State(state): State<AppState>,
    headers: HeaderMap,
    Path(id): Path<Uuid>,
) -> Result<Json<TaskResponse>, AppError> {
    let user = require_user(&state, &headers).await?;
    let row = authorized_task(&state, &user, id, false).await?;
    Ok(Json(row_to_task(&row)))
}

pub async fn update_task(
    State(state): State<AppState>,
    headers: HeaderMap,
    Path(id): Path<Uuid>,
    Json(input): Json<UpdateTaskRequest>,
) -> Result<Json<TaskResponse>, AppError> {
    let user = require_user(&state, &headers).await?;
    require_writer(&user)?;
    let mut tx = state.pool.begin().await?;
    let row = authorized_task_in_transaction(&mut tx, &user, id, true).await?;
    let workspace_id: Uuid = row.get("workspace_id");
    let old_state_name: String = row.get("state");
    let old_state = parse_task_state(&old_state_name)?;
    let next_state = input.state.unwrap_or(old_state);
    let next_agent = match input.assigned_agent_id {
        Some(agent) => agent,
        None => row.get("assigned_agent_id"),
    };
    let old_title: String = row.get("title");
    let old_description: String = row.get("description");
    let title = input.title.as_deref().unwrap_or(&old_title);
    let description = input.description.as_deref().unwrap_or(&old_description);
    if input.title.is_none()
        && input.description.is_none()
        && input.state.is_none()
        && input.assigned_agent_id.is_none()
    {
        return Err(AppError::Validation(
            "task update contains no changes".into(),
        ));
    }
    validate_task_text(title, description)?;
    if next_state != old_state && !old_state.can_transition_to(next_state) {
        return Err(AppError::Conflict("task state transition is not allowed"));
    }
    validate_task_references_in_transaction(&mut tx, workspace_id, None, next_agent, &[]).await?;

    let now = OffsetDateTime::now_utc();
    let updated = sqlx::query(
        "UPDATE tasks SET title=$2,description=$3,state=$4,assigned_agent_id=$5,updated_at=$6 WHERE id=$1 \
         RETURNING id,workspace_id,parent_task_id,title,description,state,assigned_agent_id,dependencies,created_at,updated_at",
    )
    .bind(id)
    .bind(title)
    .bind(description)
    .bind(task_state_name(next_state))
    .bind(next_agent)
    .bind(now)
    .fetch_one(&mut *tx)
    .await?;
    if next_state != old_state {
        append_activity(
            &mut tx,
            workspace_id,
            Some(id),
            next_agent,
            "TASK_STATE_CHANGED",
            json!({"from": task_state_name(old_state), "to": task_state_name(next_state)}),
        )
        .await?;
    }
    if input.assigned_agent_id.is_some() && next_agent != row.get("assigned_agent_id") {
        append_activity(
            &mut tx,
            workspace_id,
            Some(id),
            next_agent,
            "TASK_ASSIGNED",
            json!({"assigned_agent_id": next_agent}),
        )
        .await?;
    }
    if input.title.is_some() || input.description.is_some() {
        append_activity(
            &mut tx,
            workspace_id,
            Some(id),
            next_agent,
            "TASK_UPDATED",
            json!({"title_changed": input.title.is_some(), "description_changed": input.description.is_some()}),
        )
        .await?;
    }
    audit(
        &mut tx,
        Some(user.id),
        Some(user.profile_id),
        "task.updated",
        "task",
        Some(id.to_string()),
        "success",
    )
    .await?;
    tx.commit().await?;
    Ok(Json(row_to_task(&updated)))
}

pub async fn list_activity(
    State(state): State<AppState>,
    headers: HeaderMap,
    Path(workspace_id): Path<Uuid>,
    Query(query): Query<ActivityQuery>,
) -> Result<Json<Vec<ActivityResponse>>, AppError> {
    let user = require_user(&state, &headers).await?;
    authorize_workspace(&state, &user, workspace_id, false).await?;
    let after = query.after.unwrap_or(0);
    if after < 0 {
        return Err(AppError::Validation(
            "activity cursor must be non-negative".into(),
        ));
    }
    let rows = sqlx::query(
        "SELECT id,workspace_id,task_id,agent_id,kind,payload,created_at FROM activity_events \
         WHERE workspace_id=$1 AND id>$2 ORDER BY id LIMIT $3",
    )
    .bind(workspace_id)
    .bind(after)
    .bind(query.limit.unwrap_or(200).clamp(1, 500))
    .fetch_all(&state.pool)
    .await?;
    Ok(Json(rows.iter().map(row_to_activity).collect()))
}

pub async fn create_activity(
    State(state): State<AppState>,
    headers: HeaderMap,
    Path(workspace_id): Path<Uuid>,
    Json(input): Json<CreateActivityRequest>,
) -> Result<(StatusCode, Json<ActivityResponse>), AppError> {
    let user = require_user(&state, &headers).await?;
    require_writer(&user)?;
    authorize_workspace(&state, &user, workspace_id, true).await?;
    validate_activity(&input.kind, &input.payload)?;
    validate_task_references(
        &state.pool,
        workspace_id,
        input.task_id,
        input.agent_id,
        &[],
    )
    .await?;
    let mut tx = state.pool.begin().await?;
    let event = append_activity(
        &mut tx,
        workspace_id,
        input.task_id,
        input.agent_id,
        &input.kind,
        input.payload,
    )
    .await?;
    audit(
        &mut tx,
        Some(user.id),
        Some(user.profile_id),
        "activity.created",
        "activity_event",
        Some(event.id.to_string()),
        "success",
    )
    .await?;
    tx.commit().await?;
    Ok((StatusCode::CREATED, Json(event)))
}

async fn append_activity(
    tx: &mut Transaction<'_, Postgres>,
    workspace_id: Uuid,
    task_id: Option<Uuid>,
    agent_id: Option<Uuid>,
    kind: &str,
    payload: Value,
) -> Result<ActivityResponse, AppError> {
    let row = sqlx::query(
        "INSERT INTO activity_events (workspace_id,task_id,agent_id,kind,payload) \
         VALUES ($1,$2,$3,$4,$5) RETURNING id,workspace_id,task_id,agent_id,kind,payload,created_at",
    )
    .bind(workspace_id)
    .bind(task_id)
    .bind(agent_id)
    .bind(kind)
    .bind(payload)
    .fetch_one(&mut **tx)
    .await?;
    Ok(row_to_activity(&row))
}

async fn validate_task_references(
    pool: &sqlx::PgPool,
    workspace_id: Uuid,
    parent_task_id: Option<Uuid>,
    assigned_agent_id: Option<Uuid>,
    dependencies: &[Uuid],
) -> Result<(), AppError> {
    if parent_task_id == Some(Uuid::nil()) || dependencies.contains(&Uuid::nil()) {
        return Err(AppError::Validation(
            "task references must be valid UUIDs".into(),
        ));
    }
    if parent_task_id.is_some_and(|parent| dependencies.contains(&parent)) {
        return Err(AppError::Validation(
            "a task parent cannot also be a dependency".into(),
        ));
    }
    let mut tx = pool.begin().await?;
    validate_task_references_in_transaction(
        &mut tx,
        workspace_id,
        parent_task_id,
        assigned_agent_id,
        dependencies,
    )
    .await?;
    tx.commit().await?;
    Ok(())
}

async fn validate_task_references_in_transaction(
    tx: &mut Transaction<'_, Postgres>,
    workspace_id: Uuid,
    parent_task_id: Option<Uuid>,
    assigned_agent_id: Option<Uuid>,
    dependencies: &[Uuid],
) -> Result<(), AppError> {
    if dependencies.len() > MAX_DEPENDENCIES {
        return Err(AppError::Validation("too many task dependencies".into()));
    }
    if let Some(parent) = parent_task_id {
        let exists: bool = sqlx::query_scalar(
            "SELECT EXISTS(SELECT 1 FROM tasks WHERE id=$1 AND workspace_id=$2)",
        )
        .bind(parent)
        .bind(workspace_id)
        .fetch_one(&mut **tx)
        .await?;
        if !exists {
            return Err(AppError::Validation(
                "parent task must belong to this workspace".into(),
            ));
        }
    }
    if !dependencies.is_empty() {
        let count: i64 = sqlx::query_scalar(
            "SELECT count(*) FROM tasks WHERE workspace_id=$1 AND id=ANY($2::uuid[])",
        )
        .bind(workspace_id)
        .bind(dependencies)
        .fetch_one(&mut **tx)
        .await?;
        if usize::try_from(count).ok() != Some(dependencies.len()) {
            return Err(AppError::Validation(
                "all task dependencies must belong to this workspace".into(),
            ));
        }
    }
    if let Some(agent) = assigned_agent_id {
        let exists: bool = sqlx::query_scalar(
            "SELECT EXISTS(SELECT 1 FROM agents WHERE id=$1 AND workspace_id=$2)",
        )
        .bind(agent)
        .bind(workspace_id)
        .fetch_one(&mut **tx)
        .await?;
        if !exists {
            return Err(AppError::Validation(
                "assigned agent must belong to this workspace".into(),
            ));
        }
    }
    Ok(())
}

async fn authorize_workspace(
    state: &AppState,
    user: &crate::auth::AuthenticatedUser,
    workspace_id: Uuid,
    require_write: bool,
) -> Result<(), AppError> {
    let authorized: bool = sqlx::query_scalar(
        "SELECT EXISTS(SELECT 1 FROM workspaces workspace WHERE workspace.id=$1 AND workspace.profile_id=$2 AND \
         ($4 IN ('OWNER','ADMIN') OR EXISTS(SELECT 1 FROM workspace_memberships member WHERE member.workspace_id=$1 AND member.user_id=$3 \
         AND (NOT $5::boolean OR member.access IN ('OWNER','EDITOR')))))",
    )
    .bind(workspace_id)
    .bind(user.profile_id)
    .bind(user.id)
    .bind(&user.role)
    .bind(require_write)
    .fetch_one(&state.pool)
    .await?;
    if !authorized {
        return Err(AppError::NotFound);
    }
    Ok(())
}

async fn authorized_task(
    state: &AppState,
    user: &crate::auth::AuthenticatedUser,
    id: Uuid,
    require_write: bool,
) -> Result<sqlx::postgres::PgRow, AppError> {
    let mut tx = state.pool.begin().await?;
    let row = authorized_task_in_transaction(&mut tx, user, id, require_write).await?;
    tx.commit().await?;
    Ok(row)
}

async fn authorized_task_in_transaction(
    tx: &mut Transaction<'_, Postgres>,
    user: &crate::auth::AuthenticatedUser,
    id: Uuid,
    require_write: bool,
) -> Result<sqlx::postgres::PgRow, AppError> {
    let query = if require_write {
        "SELECT task.id,task.workspace_id,task.parent_task_id,task.title,task.description,task.state,task.assigned_agent_id,task.dependencies,task.created_at,task.updated_at \
         FROM tasks task JOIN workspaces workspace ON workspace.id=task.workspace_id WHERE task.id=$1 AND workspace.profile_id=$2 \
         AND ($4 IN ('OWNER','ADMIN') OR EXISTS(SELECT 1 FROM workspace_memberships member WHERE member.workspace_id=task.workspace_id AND member.user_id=$3 AND member.access IN ('OWNER','EDITOR'))) FOR UPDATE"
    } else {
        "SELECT task.id,task.workspace_id,task.parent_task_id,task.title,task.description,task.state,task.assigned_agent_id,task.dependencies,task.created_at,task.updated_at \
         FROM tasks task JOIN workspaces workspace ON workspace.id=task.workspace_id WHERE task.id=$1 AND workspace.profile_id=$2 \
         AND ($4 IN ('OWNER','ADMIN') OR EXISTS(SELECT 1 FROM workspace_memberships member WHERE member.workspace_id=task.workspace_id AND member.user_id=$3))"
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

fn row_to_task(row: &sqlx::postgres::PgRow) -> TaskResponse {
    TaskResponse {
        id: row.get("id"),
        workspace_id: row.get("workspace_id"),
        parent_task_id: row.get("parent_task_id"),
        title: row.get("title"),
        description: row.get("description"),
        state: parse_task_state(&row.get::<String, _>("state")).unwrap_or(TaskState::Backlog),
        assigned_agent_id: row.get("assigned_agent_id"),
        dependencies: row.get("dependencies"),
        created_at: row.get("created_at"),
        updated_at: row.get("updated_at"),
    }
}

fn row_to_activity(row: &sqlx::postgres::PgRow) -> ActivityResponse {
    ActivityResponse {
        id: row.get("id"),
        workspace_id: row.get("workspace_id"),
        task_id: row.get("task_id"),
        agent_id: row.get("agent_id"),
        kind: row.get("kind"),
        payload: row.get("payload"),
        created_at: row.get("created_at"),
    }
}

fn validate_task_input(
    title: &str,
    description: &str,
    dependencies: &[Uuid],
) -> Result<(), AppError> {
    if dependencies.len() > MAX_DEPENDENCIES {
        return Err(AppError::Validation("too many task dependencies".into()));
    }
    if dependencies
        .iter()
        .any(|dependency| *dependency == Uuid::nil())
    {
        return Err(AppError::Validation(
            "task references must be valid UUIDs".into(),
        ));
    }
    let unique_dependencies: HashSet<_> = dependencies.iter().collect();
    if unique_dependencies.len() != dependencies.len() {
        return Err(AppError::Validation(
            "task dependencies must be unique".into(),
        ));
    }
    validate_task_text(title, description)
}

fn validate_task_text(title: &str, description: &str) -> Result<(), AppError> {
    if title.trim().is_empty() || title.chars().count() > 500 {
        return Err(AppError::Validation(
            "task title must contain 1 to 500 characters".into(),
        ));
    }
    if description.chars().count() > 20_000 {
        return Err(AppError::Validation(
            "task description exceeds 20000 characters".into(),
        ));
    }
    Ok(())
}

fn validate_activity(kind: &str, payload: &Value) -> Result<(), AppError> {
    if !ACTIVITY_KINDS.contains(&kind) {
        return Err(AppError::Validation("unknown activity kind".into()));
    }
    if payload.to_string().chars().count() > MAX_ACTIVITY_PAYLOAD_CHARS {
        return Err(AppError::Validation("activity payload is too large".into()));
    }
    Ok(())
}

fn parse_task_state(value: &str) -> Result<TaskState, AppError> {
    match value {
        "BACKLOG" => Ok(TaskState::Backlog),
        "READY" => Ok(TaskState::Ready),
        "RUNNING" => Ok(TaskState::Running),
        "BLOCKED" => Ok(TaskState::Blocked),
        "REVIEW" => Ok(TaskState::Review),
        "DONE" => Ok(TaskState::Done),
        "FAILED" => Ok(TaskState::Failed),
        "CANCELED" => Ok(TaskState::Canceled),
        _ => Err(AppError::Internal(anyhow::anyhow!(
            "invalid persisted task state"
        ))),
    }
}

fn task_state_name(state: TaskState) -> &'static str {
    match state {
        TaskState::Backlog => "BACKLOG",
        TaskState::Ready => "READY",
        TaskState::Running => "RUNNING",
        TaskState::Blocked => "BLOCKED",
        TaskState::Review => "REVIEW",
        TaskState::Done => "DONE",
        TaskState::Failed => "FAILED",
        TaskState::Canceled => "CANCELED",
    }
}

fn require_writer(user: &crate::auth::AuthenticatedUser) -> Result<(), AppError> {
    if user.role == "VIEWER" {
        Err(AppError::Forbidden)
    } else {
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn task_state_round_trips_database_names() {
        for state in [
            TaskState::Backlog,
            TaskState::Ready,
            TaskState::Running,
            TaskState::Blocked,
            TaskState::Review,
            TaskState::Done,
            TaskState::Failed,
            TaskState::Canceled,
        ] {
            assert_eq!(parse_task_state(task_state_name(state)).unwrap(), state);
        }
    }

    #[test]
    fn activity_validation_rejects_unknown_kinds() {
        assert!(validate_activity("NOT_A_KIND", &json!({})).is_err());
        assert!(validate_activity("TASK_CREATED", &json!({})).is_ok());
    }

    #[test]
    fn task_input_rejects_duplicate_dependencies() {
        let dependency = Uuid::now_v7();
        assert!(validate_task_input("task", "", &[dependency, dependency]).is_err());
    }
}
