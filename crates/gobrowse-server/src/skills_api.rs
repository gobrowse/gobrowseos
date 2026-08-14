//! Profile/workspace-scoped Skills and immutable revision lifecycle.
//!
//! A Skill row is only a name and description.  Procedure content lives in
//! revisions; changing content always inserts a new revision.  Promotion only
//! moves the active pointer and the single promoted marker, so it cannot
//! silently overwrite production content.

use axum::{
    Json,
    extract::{Path, Query, State},
    http::{HeaderMap, StatusCode},
};
use gobrowse_core::skills::{PromotionPolicy, SkillEvaluation};
use serde::{Deserialize, Serialize};
use serde_json::Value;
use sqlx::{Postgres, Row, Transaction};
use time::OffsetDateTime;
use uuid::Uuid;

use crate::{
    AppState,
    auth::{AuthenticatedUser, audit, require_user},
    error::AppError,
};

const MAX_NAME_CHARS: usize = 200;
const MAX_DESCRIPTION_CHARS: usize = 10_000;
const MAX_CONTENT_CHARS: usize = 100_000;
const MAX_REASON_CHARS: usize = 10_000;
const MAX_SOURCE_CONVERSATIONS: usize = 100;
const MAX_EVALUATION_ATTEMPTS: u32 = 1_000_000;
const MAX_EVALUATION_STEPS: u32 = 10_000_000;
const MAX_EVALUATION_DURATION_MS: u64 = 86_400_000;

#[derive(Debug, Deserialize)]
pub struct SkillListQuery {
    pub workspace_id: Option<Uuid>,
}

#[derive(Debug, Deserialize)]
pub struct CreateSkillRequest {
    pub name: String,
    #[serde(default)]
    pub description: String,
    pub content: String,
    pub reason: String,
    pub workspace_id: Option<Uuid>,
    #[serde(default)]
    pub source_conversation_ids: Vec<Uuid>,
    #[serde(default)]
    pub promotion_policy: Option<PromotionPolicy>,
}

#[derive(Debug, Deserialize)]
pub struct CreateRevisionRequest {
    pub content: String,
    pub reason: String,
    #[serde(default)]
    pub source_conversation_ids: Vec<Uuid>,
}

#[derive(Debug, Deserialize)]
pub struct EvaluateRequest {
    pub evaluation: SkillEvaluation,
}

#[derive(Debug, Deserialize)]
pub struct ReasonRequest {
    pub reason: String,
}

#[derive(Debug, Deserialize)]
pub struct PromoteRequest {
    pub revision: i64,
    pub reason: String,
}

#[derive(Debug, Deserialize)]
pub struct EvaluateSkillRequest {
    pub revision: i64,
    pub evaluation: SkillEvaluation,
}

#[derive(Debug, Deserialize)]
pub struct RollbackRequest {
    pub target_revision: i64,
    pub reason: String,
}

#[derive(Debug, Serialize)]
pub struct SkillResponse {
    pub id: Uuid,
    pub profile_id: Uuid,
    pub workspace_id: Option<Uuid>,
    pub name: String,
    pub description: String,
    pub active_revision: Option<i64>,
    pub promotion_policy: PromotionPolicy,
    pub created_at: OffsetDateTime,
    pub updated_at: OffsetDateTime,
}

#[derive(Debug, Serialize)]
pub struct SkillRevisionResponse {
    pub id: Uuid,
    pub skill_id: Uuid,
    pub revision: i64,
    pub content: String,
    pub author: String,
    pub reason: String,
    pub source_conversation_ids: Vec<Uuid>,
    pub created_at: OffsetDateTime,
    pub evaluation: Option<SkillEvaluation>,
    pub promoted: bool,
}

pub async fn list_skills(
    State(state): State<AppState>,
    headers: HeaderMap,
    Query(query): Query<SkillListQuery>,
) -> Result<Json<Vec<SkillResponse>>, AppError> {
    let user = require_user(&state, &headers).await?;
    let mut tx = state.pool.begin().await?;
    authorize_scope(&mut tx, &user, query.workspace_id, false, false).await?;
    let rows = sqlx::query(
        "SELECT id,profile_id,workspace_id,name,description,active_revision,promotion_policy,created_at,updated_at \
         FROM skills WHERE profile_id=$1 AND (($2::uuid IS NULL AND workspace_id IS NULL) OR workspace_id=$2) \
         ORDER BY updated_at DESC, id DESC LIMIT 200",
    )
    .bind(user.profile_id)
    .bind(query.workspace_id)
    .fetch_all(&mut *tx)
    .await?;
    tx.commit().await?;
    rows.into_iter()
        .map(|row| row_to_skill(&row))
        .collect::<Result<Vec<_>, _>>()
        .map(Json)
}

pub async fn create_skill(
    State(state): State<AppState>,
    headers: HeaderMap,
    Json(input): Json<CreateSkillRequest>,
) -> Result<(StatusCode, Json<SkillResponse>), AppError> {
    let user = require_user(&state, &headers).await?;
    validate_skill_fields(
        &input.name,
        &input.description,
        &input.content,
        &input.reason,
    )?;
    validate_ids(&input.source_conversation_ids)?;
    let policy = input.promotion_policy.unwrap_or_default();

    let mut tx = state.pool.begin().await?;
    authorize_scope(&mut tx, &user, input.workspace_id, true, false).await?;
    validate_sources(
        &mut tx,
        &user,
        input.workspace_id,
        &input.source_conversation_ids,
    )
    .await?;
    let now = OffsetDateTime::now_utc();
    let skill_id = Uuid::now_v7();
    sqlx::query(
        "INSERT INTO skills (id,profile_id,workspace_id,name,description,active_revision,promotion_policy,created_at,updated_at) \
         VALUES ($1,$2,$3,$4,$5,NULL,$6,$7,$7)",
    )
    .bind(skill_id)
    .bind(user.profile_id)
    .bind(input.workspace_id)
    .bind(input.name.trim())
    .bind(input.description.trim())
    .bind(policy.as_str())
    .bind(now)
    .execute(&mut *tx)
    .await
    .map_err(map_skill_insert_error)?;
    let _revision = insert_revision(
        &mut tx,
        &user,
        skill_id,
        1,
        &input.content,
        &input.reason,
        &input.source_conversation_ids,
        now,
    )
    .await?;
    audit(
        &mut tx,
        Some(user.id),
        Some(user.profile_id),
        "skill.created",
        "skill",
        Some(skill_id.to_string()),
        "success",
    )
    .await?;
    tx.commit().await?;
    Ok((
        StatusCode::CREATED,
        Json(SkillResponse {
            id: skill_id,
            profile_id: user.profile_id,
            workspace_id: input.workspace_id,
            name: input.name.trim().into(),
            description: input.description.trim().into(),
            active_revision: None,
            promotion_policy: policy,
            created_at: now,
            updated_at: now,
        }),
    ))
}

pub async fn create_revision(
    State(state): State<AppState>,
    headers: HeaderMap,
    Path(skill_id): Path<Uuid>,
    Json(input): Json<CreateRevisionRequest>,
) -> Result<(StatusCode, Json<SkillRevisionResponse>), AppError> {
    create_revision_inner(state, headers, skill_id, input, "skill.revision_created").await
}

pub async fn propose_revision(
    State(state): State<AppState>,
    headers: HeaderMap,
    Path(skill_id): Path<Uuid>,
    Json(input): Json<CreateRevisionRequest>,
) -> Result<(StatusCode, Json<SkillRevisionResponse>), AppError> {
    create_revision_inner(state, headers, skill_id, input, "skill.proposed").await
}

async fn create_revision_inner(
    state: AppState,
    headers: HeaderMap,
    skill_id: Uuid,
    input: CreateRevisionRequest,
    action: &str,
) -> Result<(StatusCode, Json<SkillRevisionResponse>), AppError> {
    let user = require_user(&state, &headers).await?;
    validate_content_and_reason(&input.content, &input.reason)?;
    validate_ids(&input.source_conversation_ids)?;
    let mut tx = state.pool.begin().await?;
    let (workspace_id, revision_number) = lock_skill(&mut tx, &user, skill_id, true, false).await?;
    validate_sources(&mut tx, &user, workspace_id, &input.source_conversation_ids).await?;
    let now = OffsetDateTime::now_utc();
    let revision = insert_revision(
        &mut tx,
        &user,
        skill_id,
        revision_number,
        &input.content,
        &input.reason,
        &input.source_conversation_ids,
        now,
    )
    .await?;
    sqlx::query("UPDATE skills SET updated_at=$1 WHERE id=$2")
        .bind(now)
        .bind(skill_id)
        .execute(&mut *tx)
        .await?;
    audit(
        &mut tx,
        Some(user.id),
        Some(user.profile_id),
        action,
        "skill_revision",
        Some(revision.id.to_string()),
        "success",
    )
    .await?;
    tx.commit().await?;
    Ok((StatusCode::CREATED, Json(revision)))
}

pub async fn history(
    State(state): State<AppState>,
    headers: HeaderMap,
    Path(skill_id): Path<Uuid>,
) -> Result<Json<Vec<SkillRevisionResponse>>, AppError> {
    let user = require_user(&state, &headers).await?;
    let mut tx = state.pool.begin().await?;
    let _ = lock_skill(&mut tx, &user, skill_id, false, false).await?;
    let rows = sqlx::query(
        "SELECT id,skill_id,revision,content,author,reason,source_conversation_ids,created_at,evaluation,promoted \
         FROM skill_revisions WHERE skill_id=$1 ORDER BY revision DESC LIMIT 200",
    )
    .bind(skill_id)
    .fetch_all(&mut *tx)
    .await?;
    tx.commit().await?;
    rows.into_iter()
        .map(row_to_revision)
        .collect::<Result<Vec<_>, _>>()
        .map(Json)
}

pub async fn evaluate(
    State(state): State<AppState>,
    headers: HeaderMap,
    Path((skill_id, revision)): Path<(Uuid, i64)>,
    Json(input): Json<EvaluateRequest>,
) -> Result<Json<SkillRevisionResponse>, AppError> {
    evaluate_revision(state, headers, skill_id, revision, input.evaluation).await
}

pub async fn evaluate_skill(
    State(state): State<AppState>,
    headers: HeaderMap,
    Path(skill_id): Path<Uuid>,
    Json(input): Json<EvaluateSkillRequest>,
) -> Result<Json<SkillRevisionResponse>, AppError> {
    evaluate_revision(state, headers, skill_id, input.revision, input.evaluation).await
}

async fn evaluate_revision(
    state: AppState,
    headers: HeaderMap,
    skill_id: Uuid,
    revision: i64,
    evaluation: SkillEvaluation,
) -> Result<Json<SkillRevisionResponse>, AppError> {
    let user = require_user(&state, &headers).await?;
    validate_evaluation(&evaluation)?;
    let mut tx = state.pool.begin().await?;
    let (_, _) = lock_skill(&mut tx, &user, skill_id, true, false).await?;
    let row = sqlx::query(
        "SELECT id,skill_id,revision,content,author,reason,source_conversation_ids,created_at,evaluation,promoted \
         FROM skill_revisions WHERE skill_id=$1 AND revision=$2 FOR UPDATE",
    )
    .bind(skill_id)
    .bind(revision)
    .fetch_optional(&mut *tx)
    .await?
    .ok_or(AppError::NotFound)?;
    if row.get::<Option<Value>, _>("evaluation").is_some() {
        return Err(AppError::Conflict(
            "revision already has evaluation evidence",
        ));
    }
    let evaluation_json = serde_json::to_value(&evaluation)
        .map_err(|error| AppError::Internal(anyhow::anyhow!(error)))?;
    sqlx::query("UPDATE skill_revisions SET evaluation=$1 WHERE id=$2")
        .bind(&evaluation_json)
        .bind(row.get::<Uuid, _>("id"))
        .execute(&mut *tx)
        .await?;

    let policy: PromotionPolicy =
        sqlx::query_scalar::<_, String>("SELECT promotion_policy FROM skills WHERE id=$1")
            .bind(skill_id)
            .fetch_one(&mut *tx)
            .await?
            .parse()
            .map_err(|_| AppError::Internal(anyhow::anyhow!("unknown promotion policy")))?;
    let previous_value: Option<Option<Value>> = sqlx::query_scalar::<_, Option<Value>>(
        "SELECT evaluation FROM skill_revisions WHERE skill_id=$1 AND promoted AND revision<>$2",
    )
    .bind(skill_id)
    .bind(revision)
    .fetch_optional(&mut *tx)
    .await?;
    let has_previous = previous_value.is_some();
    let previous = previous_value
        .flatten()
        .map(serde_json::from_value)
        .transpose()
        .map_err(|error| AppError::Internal(anyhow::anyhow!(error)))?;
    let automatic = policy == PromotionPolicy::Automatic
        && evaluation.deterministic_checks_passed
        && evaluation.attempts > 0
        && ((!has_previous && previous.is_none())
            || previous
                .as_ref()
                .is_some_and(|previous| evaluation.can_auto_promote_over(previous)));
    if automatic {
        promote_locked(&mut tx, skill_id, revision).await?;
    }
    audit(
        &mut tx,
        Some(user.id),
        Some(user.profile_id),
        "skill.evaluated",
        "skill_revision",
        Some(row.get::<Uuid, _>("id").to_string()),
        "success",
    )
    .await?;
    if automatic {
        audit(
            &mut tx,
            Some(user.id),
            Some(user.profile_id),
            "skill.automatically_promoted",
            "skill_revision",
            Some(row.get::<Uuid, _>("id").to_string()),
            "success",
        )
        .await?;
    }
    let response = revision_response_from_row(
        &row,
        Some(evaluation),
        row.get::<bool, _>("promoted") || automatic,
    )?;
    tx.commit().await?;
    Ok(Json(response))
}

pub async fn promote(
    State(state): State<AppState>,
    headers: HeaderMap,
    Path((skill_id, revision)): Path<(Uuid, i64)>,
    Json(input): Json<ReasonRequest>,
) -> Result<Json<SkillResponse>, AppError> {
    promote_revision(state, headers, skill_id, revision, input.reason).await
}

pub async fn promote_skill(
    State(state): State<AppState>,
    headers: HeaderMap,
    Path(skill_id): Path<Uuid>,
    Json(input): Json<PromoteRequest>,
) -> Result<Json<SkillResponse>, AppError> {
    promote_revision(state, headers, skill_id, input.revision, input.reason).await
}

async fn promote_revision(
    state: AppState,
    headers: HeaderMap,
    skill_id: Uuid,
    revision: i64,
    reason: String,
) -> Result<Json<SkillResponse>, AppError> {
    let user = require_user(&state, &headers).await?;
    validate_reason(&reason)?;
    let mut tx = state.pool.begin().await?;
    let (workspace_id, _) = lock_skill(&mut tx, &user, skill_id, true, true).await?;
    sqlx::query("SELECT id FROM skill_revisions WHERE skill_id=$1 AND revision=$2")
        .bind(skill_id)
        .bind(revision)
        .fetch_optional(&mut *tx)
        .await?
        .ok_or(AppError::NotFound)?;
    promote_locked(&mut tx, skill_id, revision).await?;
    let now = OffsetDateTime::now_utc();
    sqlx::query("UPDATE skills SET updated_at=$1 WHERE id=$2")
        .bind(now)
        .bind(skill_id)
        .execute(&mut *tx)
        .await?;
    audit(
        &mut tx,
        Some(user.id),
        Some(user.profile_id),
        "skill.promoted",
        "skill_revision",
        Some(format!("{skill_id}:{revision}")),
        "success",
    )
    .await?;
    let response = fetch_skill_in_tx(&mut tx, &user, skill_id, workspace_id).await?;
    tx.commit().await?;
    Ok(Json(response))
}

pub async fn rollback(
    State(state): State<AppState>,
    headers: HeaderMap,
    Path(skill_id): Path<Uuid>,
    Json(input): Json<RollbackRequest>,
) -> Result<Json<SkillResponse>, AppError> {
    let user = require_user(&state, &headers).await?;
    if input.target_revision <= 0 {
        return Err(AppError::Validation(
            "target revision must be positive".into(),
        ));
    }
    validate_reason(&input.reason)?;
    let mut tx = state.pool.begin().await?;
    let (workspace_id, _) = lock_skill(&mut tx, &user, skill_id, true, true).await?;
    sqlx::query("SELECT id FROM skill_revisions WHERE skill_id=$1 AND revision=$2")
        .bind(skill_id)
        .bind(input.target_revision)
        .fetch_optional(&mut *tx)
        .await?
        .ok_or(AppError::NotFound)?;
    promote_locked(&mut tx, skill_id, input.target_revision).await?;
    let now = OffsetDateTime::now_utc();
    sqlx::query("UPDATE skills SET updated_at=$1 WHERE id=$2")
        .bind(now)
        .bind(skill_id)
        .execute(&mut *tx)
        .await?;
    audit(
        &mut tx,
        Some(user.id),
        Some(user.profile_id),
        "skill.rolled_back",
        "skill",
        Some(skill_id.to_string()),
        "success",
    )
    .await?;
    let response = fetch_skill_in_tx(&mut tx, &user, skill_id, workspace_id).await?;
    tx.commit().await?;
    Ok(Json(response))
}

#[allow(clippy::too_many_arguments)]
async fn insert_revision(
    tx: &mut Transaction<'_, Postgres>,
    user: &AuthenticatedUser,
    skill_id: Uuid,
    revision: i64,
    content: &str,
    reason: &str,
    sources: &[Uuid],
    created_at: OffsetDateTime,
) -> Result<SkillRevisionResponse, AppError> {
    let id = Uuid::now_v7();
    sqlx::query(
        "INSERT INTO skill_revisions (id,skill_id,revision,content,author,reason,source_conversation_ids,created_at) \
         VALUES ($1,$2,$3,$4,$5,$6,$7,$8)",
    )
    .bind(id)
    .bind(skill_id)
    .bind(revision)
    .bind(content)
    .bind(&user.display_name)
    .bind(reason.trim())
    .bind(sources)
    .bind(created_at)
    .execute(&mut **tx)
    .await?;
    Ok(SkillRevisionResponse {
        id,
        skill_id,
        revision,
        content: content.into(),
        author: user.display_name.clone(),
        reason: reason.trim().into(),
        source_conversation_ids: sources.to_vec(),
        created_at,
        evaluation: None,
        promoted: false,
    })
}

async fn lock_skill(
    tx: &mut Transaction<'_, Postgres>,
    user: &AuthenticatedUser,
    skill_id: Uuid,
    write: bool,
    privileged: bool,
) -> Result<(Option<Uuid>, i64), AppError> {
    let workspace_id: Option<Uuid> =
        sqlx::query_scalar("SELECT workspace_id FROM skills WHERE id=$1 AND profile_id=$2")
            .bind(skill_id)
            .bind(user.profile_id)
            .fetch_optional(&mut **tx)
            .await?
            .ok_or(AppError::NotFound)?;
    authorize_scope(tx, user, workspace_id, write, privileged).await?;
    sqlx::query("SELECT id FROM skills WHERE id=$1 FOR UPDATE")
        .bind(skill_id)
        .fetch_one(&mut **tx)
        .await?;
    let revision: i64 = sqlx::query_scalar(
        "SELECT COALESCE(MAX(revision),0)+1 FROM skill_revisions WHERE skill_id=$1",
    )
    .bind(skill_id)
    .fetch_one(&mut **tx)
    .await?;
    Ok((workspace_id, revision))
}

async fn authorize_scope(
    tx: &mut Transaction<'_, Postgres>,
    user: &AuthenticatedUser,
    workspace_id: Option<Uuid>,
    write: bool,
    privileged: bool,
) -> Result<(), AppError> {
    if workspace_id.is_none() {
        if write {
            sqlx::query("SELECT id FROM profiles WHERE id=$1 FOR UPDATE")
                .bind(user.profile_id)
                .fetch_optional(&mut **tx)
                .await?
                .ok_or(AppError::NotFound)?;
            if !is_admin(user) {
                return Err(AppError::Forbidden);
            }
        }
        if privileged && !is_admin(user) {
            return Err(AppError::Forbidden);
        }
        return Ok(());
    }
    let workspace_id = workspace_id.expect("checked above");
    sqlx::query("SELECT id FROM workspaces WHERE id=$1 AND profile_id=$2 FOR UPDATE")
        .bind(workspace_id)
        .bind(user.profile_id)
        .fetch_optional(&mut **tx)
        .await?
        .ok_or(AppError::NotFound)?;
    if is_admin(user) {
        return Ok(());
    }
    let access: Option<String> = sqlx::query_scalar(
        "SELECT access FROM workspace_memberships WHERE workspace_id=$1 AND user_id=$2 FOR UPDATE",
    )
    .bind(workspace_id)
    .bind(user.id)
    .fetch_optional(&mut **tx)
    .await?;
    if access.is_none() {
        return Err(AppError::NotFound);
    }
    if write && user.role == "VIEWER" {
        return Err(AppError::Forbidden);
    }
    let allowed = if write {
        matches!(access.as_deref(), Some("OWNER" | "EDITOR"))
    } else {
        true
    };
    if allowed {
        Ok(())
    } else {
        Err(AppError::NotFound)
    }
}

async fn fetch_skill_in_tx(
    tx: &mut Transaction<'_, Postgres>,
    user: &AuthenticatedUser,
    skill_id: Uuid,
    workspace_id: Option<Uuid>,
) -> Result<SkillResponse, AppError> {
    let row = sqlx::query(
        "SELECT id,profile_id,workspace_id,name,description,active_revision,promotion_policy,created_at,updated_at \
         FROM skills WHERE id=$1 AND profile_id=$2 AND workspace_id IS NOT DISTINCT FROM $3",
    )
    .bind(skill_id)
    .bind(user.profile_id)
    .bind(workspace_id)
    .fetch_optional(&mut **tx)
    .await?
    .ok_or(AppError::NotFound)?;
    row_to_skill(&row)
}

async fn promote_locked(
    tx: &mut Transaction<'_, Postgres>,
    skill_id: Uuid,
    revision: i64,
) -> Result<(), AppError> {
    sqlx::query("UPDATE skill_revisions SET promoted=false WHERE skill_id=$1 AND promoted")
        .bind(skill_id)
        .execute(&mut **tx)
        .await?;
    sqlx::query("UPDATE skill_revisions SET promoted=true WHERE skill_id=$1 AND revision=$2")
        .bind(skill_id)
        .bind(revision)
        .execute(&mut **tx)
        .await?;
    sqlx::query("UPDATE skills SET active_revision=$1 WHERE id=$2")
        .bind(revision)
        .bind(skill_id)
        .execute(&mut **tx)
        .await?;
    Ok(())
}

async fn validate_sources(
    tx: &mut Transaction<'_, Postgres>,
    user: &AuthenticatedUser,
    workspace_id: Option<Uuid>,
    ids: &[Uuid],
) -> Result<(), AppError> {
    let mut lock_ids = ids.to_vec();
    lock_ids.sort_unstable();
    let rows = sqlx::query(
        "SELECT c.id FROM conversations c WHERE c.profile_id=$1 AND c.id=ANY($2::uuid[]) \
         AND c.status <> 'deleted' AND ($3::uuid IS NULL OR c.workspace_id=$3) \
         AND ($4 IN ('OWNER','ADMIN') OR (c.workspace_id IS NULL AND c.created_by_user_id=$5) OR EXISTS( \
             SELECT 1 FROM workspace_memberships m WHERE m.workspace_id=c.workspace_id AND m.user_id=$5)) \
         ORDER BY c.id FOR KEY SHARE OF c",
    )
    .bind(user.profile_id)
    .bind(&lock_ids)
    .bind(workspace_id)
    .bind(&user.role)
    .bind(user.id)
    .fetch_all(&mut **tx)
    .await?;
    let found: Vec<Uuid> = rows.into_iter().map(|row| row.get("id")).collect();
    if found != lock_ids {
        return Err(AppError::Validation(
            "source conversations must be accessible in the skill scope".into(),
        ));
    }
    Ok(())
}

fn row_to_skill(row: &sqlx::postgres::PgRow) -> Result<SkillResponse, AppError> {
    Ok(SkillResponse {
        id: row.get("id"),
        profile_id: row.get("profile_id"),
        workspace_id: row.get("workspace_id"),
        name: row.get("name"),
        description: row.get("description"),
        active_revision: row.get("active_revision"),
        promotion_policy: row
            .get::<String, _>("promotion_policy")
            .parse()
            .map_err(|_| AppError::Internal(anyhow::anyhow!("unknown promotion policy")))?,
        created_at: row.get("created_at"),
        updated_at: row.get("updated_at"),
    })
}

fn row_to_revision(row: sqlx::postgres::PgRow) -> Result<SkillRevisionResponse, AppError> {
    revision_response_from_row(&row, None, row.get("promoted"))
}

fn revision_response_from_row(
    row: &sqlx::postgres::PgRow,
    evaluation_override: Option<SkillEvaluation>,
    promoted: bool,
) -> Result<SkillRevisionResponse, AppError> {
    let evaluation = match evaluation_override {
        Some(evaluation) => Some(evaluation),
        None => row
            .get::<Option<Value>, _>("evaluation")
            .map(serde_json::from_value)
            .transpose()
            .map_err(|error| AppError::Internal(anyhow::anyhow!(error)))?,
    };
    Ok(SkillRevisionResponse {
        id: row.get("id"),
        skill_id: row.get("skill_id"),
        revision: row.get("revision"),
        content: row.get("content"),
        author: row.get("author"),
        reason: row.get("reason"),
        source_conversation_ids: row.get("source_conversation_ids"),
        created_at: row.get("created_at"),
        evaluation,
        promoted,
    })
}

fn validate_skill_fields(
    name: &str,
    description: &str,
    content: &str,
    reason: &str,
) -> Result<(), AppError> {
    if name.trim().is_empty() || name.chars().count() > MAX_NAME_CHARS {
        return Err(AppError::Validation(
            "skill name must contain 1 to 200 characters".into(),
        ));
    }
    if description.chars().count() > MAX_DESCRIPTION_CHARS {
        return Err(AppError::Validation("skill description is too long".into()));
    }
    validate_content_and_reason(content, reason)
}

fn validate_content_and_reason(content: &str, reason: &str) -> Result<(), AppError> {
    if content.trim().is_empty() || content.chars().count() > MAX_CONTENT_CHARS {
        return Err(AppError::Validation(
            "skill content must contain 1 to 100000 characters".into(),
        ));
    }
    validate_reason(reason)
}

fn validate_reason(reason: &str) -> Result<(), AppError> {
    if reason.trim().is_empty() || reason.chars().count() > MAX_REASON_CHARS {
        return Err(AppError::Validation(
            "skill reason must contain 1 to 10000 characters".into(),
        ));
    }
    Ok(())
}

fn validate_evaluation(evaluation: &SkillEvaluation) -> Result<(), AppError> {
    if evaluation.attempts > MAX_EVALUATION_ATTEMPTS
        || evaluation.successful_attempts > evaluation.attempts
        || evaluation.steps > MAX_EVALUATION_STEPS
        || evaluation.retries > MAX_EVALUATION_STEPS
        || evaluation.errors > MAX_EVALUATION_ATTEMPTS
        || evaluation.user_corrections > evaluation.attempts
        || evaluation.duration_ms > MAX_EVALUATION_DURATION_MS
    {
        return Err(AppError::Validation(
            "evaluation evidence is out of bounds".into(),
        ));
    }
    Ok(())
}

fn validate_ids(ids: &[Uuid]) -> Result<(), AppError> {
    if ids.len() > MAX_SOURCE_CONVERSATIONS || ids.iter().any(Uuid::is_nil) {
        return Err(AppError::Validation(
            "source conversations must contain at most 100 unique non-nil IDs".into(),
        ));
    }
    let mut sorted = ids.to_vec();
    sorted.sort_unstable();
    if sorted.windows(2).any(|window| window[0] == window[1]) {
        return Err(AppError::Validation(
            "source conversations must contain at most 100 unique non-nil IDs".into(),
        ));
    }
    Ok(())
}

fn map_skill_insert_error(error: sqlx::Error) -> AppError {
    if let sqlx::Error::Database(database) = &error
        && matches!(
            database.constraint(),
            Some("skills_profile_global_name_unique" | "skills_profile_id_workspace_id_name_key")
        )
    {
        return AppError::Conflict("skill name already exists in this scope");
    }
    AppError::Database(error)
}

fn is_admin(user: &AuthenticatedUser) -> bool {
    matches!(user.role.as_str(), "OWNER" | "ADMIN")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn evaluation_bounds_reject_impossible_evidence() {
        let evaluation = SkillEvaluation {
            deterministic_checks_passed: true,
            attempts: 1,
            successful_attempts: 2,
            steps: 0,
            retries: 0,
            errors: 0,
            duration_ms: 0,
            user_corrections: 0,
        };
        assert!(validate_evaluation(&evaluation).is_err());
    }

    #[test]
    fn ids_are_bounded_and_deduplicated() {
        let id = Uuid::now_v7();
        let ids = vec![id, id];
        assert!(validate_ids(&ids).is_err());
    }
}
