use axum::{
    Json,
    extract::{Path, State},
    http::{HeaderMap, StatusCode},
};
use gobrowse_core::library::AUTOBIOGRAPHY_MAX_CHARS;
use serde::{Deserialize, Serialize};
use sqlx::Row;
use time::OffsetDateTime;
use uuid::Uuid;

use crate::{
    AppState,
    auth::{AuthenticatedUser, audit, require_user},
    error::AppError,
    library_api,
};

#[derive(Deserialize)]
pub struct UpdatePolicyRequest {
    pub policy: String,
}

#[derive(Deserialize)]
pub struct CreateProposalRequest {
    pub after_body: String,
    pub reason: String,
    #[serde(default)]
    pub source_book_ids: Vec<Uuid>,
    #[serde(default)]
    pub source_conversation_ids: Vec<Uuid>,
}

#[derive(Deserialize)]
pub struct ReviewProposalRequest {
    pub decision: String,
    pub reason: String,
}

#[derive(Deserialize)]
pub struct RollbackRequest {
    pub target_revision: i64,
    pub reason: String,
}

#[derive(Deserialize)]
pub struct ManualUpdateRequest {
    pub body: String,
    pub expected_revision: i64,
    pub reason: String,
}

#[derive(Serialize)]
pub struct AutobiographyResponse {
    pub id: Uuid,
    pub body: String,
    pub revision: i64,
    pub policy: String,
    pub updated_at: OffsetDateTime,
}

#[derive(Serialize)]
pub struct ProposalResponse {
    pub id: Uuid,
    pub book_id: Uuid,
    pub base_revision: i64,
    pub before_body: String,
    pub after_body: String,
    pub reason: String,
    pub source_book_ids: Vec<Uuid>,
    pub source_conversation_ids: Vec<Uuid>,
    pub status: String,
    pub review_reason: Option<String>,
    pub reviewed_by: Option<Uuid>,
    pub created_at: OffsetDateTime,
    pub reviewed_at: Option<OffsetDateTime>,
}

pub async fn get_autobiography(
    State(state): State<AppState>,
    headers: HeaderMap,
) -> Result<Json<AutobiographyResponse>, AppError> {
    let user = require_user(&state, &headers).await?;
    let row = sqlx::query(
        "SELECT b.id,b.body,b.revision,b.updated_at,p.autobiography_update_policy AS policy \
         FROM books b JOIN profiles p ON p.id=b.profile_id \
         WHERE b.profile_id=$1 AND b.book_type='AUTOBIOGRAPHY'",
    )
    .bind(user.profile_id)
    .fetch_optional(&state.pool)
    .await?
    .ok_or(AppError::NotFound)?;
    Ok(Json(AutobiographyResponse {
        id: row.get("id"),
        body: row.get("body"),
        revision: row.get("revision"),
        policy: row.get("policy"),
        updated_at: row.get("updated_at"),
    }))
}

pub async fn update_policy(
    State(state): State<AppState>,
    headers: HeaderMap,
    Json(input): Json<UpdatePolicyRequest>,
) -> Result<StatusCode, AppError> {
    let user = require_user(&state, &headers).await?;
    require_admin(&user)?;
    if !matches!(input.policy.as_str(), "manual" | "propose" | "automatic") {
        return Err(AppError::Validation(
            "policy must be manual, propose, or automatic".into(),
        ));
    }
    let mut tx = state.pool.begin().await?;
    sqlx::query("UPDATE profiles SET autobiography_update_policy=$1,updated_at=now() WHERE id=$2")
        .bind(&input.policy)
        .bind(user.profile_id)
        .execute(&mut *tx)
        .await?;
    audit(
        &mut tx,
        Some(user.id),
        Some(user.profile_id),
        "autobiography.policy_updated",
        "profile",
        Some(user.profile_id.to_string()),
        "success",
    )
    .await?;
    tx.commit().await?;
    Ok(StatusCode::NO_CONTENT)
}

pub async fn manual_update(
    State(state): State<AppState>,
    headers: HeaderMap,
    Json(input): Json<ManualUpdateRequest>,
) -> Result<Json<AutobiographyResponse>, AppError> {
    let user = require_user(&state, &headers).await?;
    require_admin(&user)?;
    validate_body_and_reason(&input.body, &input.reason)?;
    let mut tx = state.pool.begin().await?;
    let row = sqlx::query(
        "SELECT b.id,b.revision,p.autobiography_update_policy AS policy FROM books b \
         JOIN profiles p ON p.id=b.profile_id WHERE b.profile_id=$1 AND b.book_type='AUTOBIOGRAPHY' FOR UPDATE OF b,p",
    )
    .bind(user.profile_id)
    .fetch_optional(&mut *tx)
    .await?
    .ok_or(AppError::NotFound)?;
    if row.get::<String, _>("policy") != "manual" {
        return Err(AppError::Conflict(
            "direct updates require the manual Autobiography policy",
        ));
    }
    if row.get::<i64, _>("revision") != input.expected_revision {
        return Err(AppError::Conflict(
            "Autobiography was changed by another writer",
        ));
    }
    let book_id: Uuid = row.get("id");
    let revision = library_api::replace_book_body(
        &mut tx,
        book_id,
        &input.body,
        Some(user.id),
        input.reason.trim(),
    )
    .await?;
    audit(
        &mut tx,
        Some(user.id),
        Some(user.profile_id),
        "autobiography.manually_updated",
        "book",
        Some(book_id.to_string()),
        "success",
    )
    .await?;
    tx.commit().await?;
    Ok(Json(AutobiographyResponse {
        id: book_id,
        body: input.body,
        revision,
        policy: "manual".into(),
        updated_at: OffsetDateTime::now_utc(),
    }))
}

pub async fn create_proposal(
    State(state): State<AppState>,
    headers: HeaderMap,
    Json(mut input): Json<CreateProposalRequest>,
) -> Result<(StatusCode, Json<ProposalResponse>), AppError> {
    let user = require_user(&state, &headers).await?;
    require_admin(&user)?;
    validate_body_and_reason(&input.after_body, &input.reason)?;
    normalize_ids(&mut input.source_book_ids);
    normalize_ids(&mut input.source_conversation_ids);
    validate_sources(
        &state,
        user.profile_id,
        &input.source_book_ids,
        &input.source_conversation_ids,
    )
    .await?;
    let mut tx = state.pool.begin().await?;
    let row = sqlx::query(
        "SELECT b.id,b.body,b.revision,p.autobiography_update_policy AS policy \
         FROM books b JOIN profiles p ON p.id=b.profile_id \
         WHERE b.profile_id=$1 AND b.book_type='AUTOBIOGRAPHY' FOR UPDATE OF b,p",
    )
    .bind(user.profile_id)
    .fetch_optional(&mut *tx)
    .await?
    .ok_or(AppError::NotFound)?;
    let policy: String = row.get("policy");
    if policy == "manual" {
        return Err(AppError::Conflict(
            "Autobiography policy requires direct manual edits",
        ));
    }
    let id = Uuid::now_v7();
    let book_id: Uuid = row.get("id");
    let base_revision: i64 = row.get("revision");
    let before_body: String = row.get("body");
    let now = OffsetDateTime::now_utc();
    let automatic = policy == "automatic";
    sqlx::query(
        "INSERT INTO autobiography_proposals \
         (id,profile_id,book_id,before_body,after_body,reason,source_book_ids,source_conversation_ids,status,base_revision,reviewed_by,reviewed_at,review_reason,created_at) \
         VALUES ($1,$2,$3,$4,$5,$6,$7,$8,$9,$10,$11,$12,$13,$14)",
    )
    .bind(id)
    .bind(user.profile_id)
    .bind(book_id)
    .bind(&before_body)
    .bind(&input.after_body)
    .bind(input.reason.trim())
    .bind(&input.source_book_ids)
    .bind(&input.source_conversation_ids)
    .bind(if automatic { "accepted" } else { "pending" })
    .bind(base_revision)
    .bind(automatic.then_some(user.id))
    .bind(automatic.then_some(now))
    .bind(automatic.then_some("Applied by automatic policy"))
    .bind(now)
    .execute(&mut *tx)
    .await?;
    if automatic {
        library_api::replace_book_body(
            &mut tx,
            book_id,
            &input.after_body,
            Some(user.id),
            input.reason.trim(),
        )
        .await?;
    }
    audit(
        &mut tx,
        Some(user.id),
        Some(user.profile_id),
        if automatic {
            "autobiography.automatically_updated"
        } else {
            "autobiography.proposed"
        },
        "autobiography_proposal",
        Some(id.to_string()),
        "success",
    )
    .await?;
    tx.commit().await?;
    Ok((
        StatusCode::CREATED,
        Json(ProposalResponse {
            id,
            book_id,
            base_revision,
            before_body,
            after_body: input.after_body,
            reason: input.reason.trim().into(),
            source_book_ids: input.source_book_ids,
            source_conversation_ids: input.source_conversation_ids,
            status: if automatic { "accepted" } else { "pending" }.into(),
            review_reason: automatic.then_some("Applied by automatic policy".into()),
            reviewed_by: automatic.then_some(user.id),
            created_at: now,
            reviewed_at: automatic.then_some(now),
        }),
    ))
}

pub async fn list_proposals(
    State(state): State<AppState>,
    headers: HeaderMap,
) -> Result<Json<Vec<ProposalResponse>>, AppError> {
    let user = require_user(&state, &headers).await?;
    require_admin(&user)?;
    let rows = sqlx::query(
        "SELECT id,book_id,base_revision,before_body,after_body,reason,source_book_ids,source_conversation_ids, \
         status,review_reason,reviewed_by,created_at,reviewed_at FROM autobiography_proposals \
         WHERE profile_id=$1 ORDER BY created_at DESC LIMIT 200",
    )
    .bind(user.profile_id)
    .fetch_all(&state.pool)
    .await?;
    Ok(Json(rows.iter().map(row_to_proposal).collect()))
}

pub async fn review_proposal(
    State(state): State<AppState>,
    headers: HeaderMap,
    Path(id): Path<Uuid>,
    Json(input): Json<ReviewProposalRequest>,
) -> Result<Json<ProposalResponse>, AppError> {
    let user = require_user(&state, &headers).await?;
    require_admin(&user)?;
    if !matches!(input.decision.as_str(), "accept" | "reject") {
        return Err(AppError::Validation(
            "decision must be accept or reject".into(),
        ));
    }
    if input.reason.trim().is_empty() || input.reason.chars().count() > 2000 {
        return Err(AppError::Validation(
            "review reason must contain 1 to 2000 characters".into(),
        ));
    }
    let mut tx = state.pool.begin().await?;
    let row = sqlx::query(
        "SELECT id,book_id,base_revision,before_body,after_body,reason,source_book_ids,source_conversation_ids, \
         status,review_reason,reviewed_by,created_at,reviewed_at FROM autobiography_proposals \
         WHERE id=$1 AND profile_id=$2 FOR UPDATE",
    )
    .bind(id)
    .bind(user.profile_id)
    .fetch_optional(&mut *tx)
    .await?
    .ok_or(AppError::NotFound)?;
    if row.get::<String, _>("status") != "pending" {
        return Err(AppError::Conflict("proposal was already reviewed"));
    }
    let policy: String = sqlx::query_scalar(
        "SELECT autobiography_update_policy FROM profiles WHERE id=$1 FOR UPDATE",
    )
    .bind(user.profile_id)
    .fetch_one(&mut *tx)
    .await?;
    if input.decision == "accept" {
        if policy == "manual" {
            return Err(AppError::Conflict(
                "pending proposals cannot be accepted under the manual policy",
            ));
        }
        let current_revision: i64 = sqlx::query_scalar(
            "SELECT revision FROM books WHERE id=$1 AND profile_id=$2 FOR UPDATE",
        )
        .bind(row.get::<Uuid, _>("book_id"))
        .bind(user.profile_id)
        .fetch_one(&mut *tx)
        .await?;
        if current_revision != row.get::<i64, _>("base_revision") {
            return Err(AppError::Conflict(
                "proposal is stale because the Autobiography changed",
            ));
        }
        library_api::replace_book_body(
            &mut tx,
            row.get("book_id"),
            row.get("after_body"),
            Some(user.id),
            row.get("reason"),
        )
        .await?;
    }
    let now = OffsetDateTime::now_utc();
    let status = if input.decision == "accept" {
        "accepted"
    } else {
        "rejected"
    };
    sqlx::query(
        "UPDATE autobiography_proposals SET status=$1,reviewed_by=$2,reviewed_at=$3,review_reason=$4 WHERE id=$5",
    )
    .bind(status)
    .bind(user.id)
    .bind(now)
    .bind(input.reason.trim())
    .bind(id)
    .execute(&mut *tx)
    .await?;
    audit(
        &mut tx,
        Some(user.id),
        Some(user.profile_id),
        if status == "accepted" {
            "autobiography.proposal_accepted"
        } else {
            "autobiography.proposal_rejected"
        },
        "autobiography_proposal",
        Some(id.to_string()),
        "success",
    )
    .await?;
    tx.commit().await?;
    Ok(Json(ProposalResponse {
        id,
        book_id: row.get("book_id"),
        base_revision: row.get("base_revision"),
        before_body: row.get("before_body"),
        after_body: row.get("after_body"),
        reason: row.get("reason"),
        source_book_ids: row.get("source_book_ids"),
        source_conversation_ids: row.get("source_conversation_ids"),
        status: status.into(),
        review_reason: Some(input.reason.trim().into()),
        reviewed_by: Some(user.id),
        created_at: row.get("created_at"),
        reviewed_at: Some(now),
    }))
}

pub async fn rollback(
    State(state): State<AppState>,
    headers: HeaderMap,
    Json(input): Json<RollbackRequest>,
) -> Result<Json<AutobiographyResponse>, AppError> {
    let user = require_user(&state, &headers).await?;
    require_admin(&user)?;
    if input.target_revision <= 0 || input.reason.trim().is_empty() {
        return Err(AppError::Validation(
            "target revision and rollback reason are required".into(),
        ));
    }
    let mut tx = state.pool.begin().await?;
    let row = sqlx::query(
        "SELECT b.id,b.revision,p.autobiography_update_policy AS policy,r.title,r.body,r.tags,r.metadata \
         FROM books b JOIN profiles p ON p.id=b.profile_id \
         JOIN book_revisions r ON r.book_id=b.id AND r.revision=$2 \
         WHERE b.profile_id=$1 AND b.book_type='AUTOBIOGRAPHY' FOR UPDATE OF b",
    )
    .bind(user.profile_id)
    .bind(input.target_revision)
    .fetch_optional(&mut *tx)
    .await?
    .ok_or(AppError::NotFound)?;
    let book_id: Uuid = row.get("id");
    let title: String = row.get("title");
    let body: String = row.get("body");
    let tags: Vec<String> = row.get("tags");
    let metadata: serde_json::Value = row.get("metadata");
    let revision = library_api::replace_book_snapshot(
        &mut tx,
        book_id,
        &title,
        &body,
        &tags,
        &metadata,
        Some(user.id),
        input.reason.trim(),
    )
    .await?;
    audit(
        &mut tx,
        Some(user.id),
        Some(user.profile_id),
        "autobiography.rolled_back",
        "book",
        Some(book_id.to_string()),
        "success",
    )
    .await?;
    tx.commit().await?;
    Ok(Json(AutobiographyResponse {
        id: book_id,
        body,
        revision,
        policy: row.get("policy"),
        updated_at: OffsetDateTime::now_utc(),
    }))
}

async fn validate_sources(
    state: &AppState,
    profile_id: Uuid,
    book_ids: &[Uuid],
    conversation_ids: &[Uuid],
) -> Result<(), AppError> {
    let book_count: i64 =
        sqlx::query_scalar("SELECT count(*) FROM books WHERE profile_id=$1 AND id=ANY($2::uuid[])")
            .bind(profile_id)
            .bind(book_ids)
            .fetch_one(&state.pool)
            .await?;
    let conversation_count: i64 = sqlx::query_scalar(
        "SELECT count(*) FROM conversations WHERE profile_id=$1 AND id=ANY($2::uuid[]) AND status <> 'deleted'",
    )
    .bind(profile_id)
    .bind(conversation_ids)
    .fetch_one(&state.pool)
    .await?;
    if usize::try_from(book_count).ok() != Some(book_ids.len())
        || usize::try_from(conversation_count).ok() != Some(conversation_ids.len())
    {
        return Err(AppError::Validation(
            "proposal sources must belong to the active profile".into(),
        ));
    }
    Ok(())
}

fn validate_body_and_reason(body: &str, reason: &str) -> Result<(), AppError> {
    if body.chars().count() > AUTOBIOGRAPHY_MAX_CHARS {
        return Err(AppError::Validation(format!(
            "Autobiography exceeds {AUTOBIOGRAPHY_MAX_CHARS} characters"
        )));
    }
    if reason.trim().is_empty() || reason.chars().count() > 10_000 {
        return Err(AppError::Validation(
            "proposal reason must contain 1 to 10000 characters".into(),
        ));
    }
    Ok(())
}

fn normalize_ids(ids: &mut Vec<Uuid>) {
    ids.sort_unstable();
    ids.dedup();
}

fn row_to_proposal(row: &sqlx::postgres::PgRow) -> ProposalResponse {
    ProposalResponse {
        id: row.get("id"),
        book_id: row.get("book_id"),
        base_revision: row.get("base_revision"),
        before_body: row.get("before_body"),
        after_body: row.get("after_body"),
        reason: row.get("reason"),
        source_book_ids: row.get("source_book_ids"),
        source_conversation_ids: row.get("source_conversation_ids"),
        status: row.get("status"),
        review_reason: row.get("review_reason"),
        reviewed_by: row.get("reviewed_by"),
        created_at: row.get("created_at"),
        reviewed_at: row.get("reviewed_at"),
    }
}

fn require_admin(user: &AuthenticatedUser) -> Result<(), AppError> {
    if matches!(user.role.as_str(), "OWNER" | "ADMIN") {
        Ok(())
    } else {
        Err(AppError::Forbidden)
    }
}
