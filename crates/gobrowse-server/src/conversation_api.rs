use axum::{
    Json,
    extract::{Path, Query, State},
    http::{HeaderMap, StatusCode},
};
use serde::{Deserialize, Serialize};
use sqlx::Row;
use time::OffsetDateTime;
use uuid::Uuid;

use crate::{
    AppState,
    auth::{AuthenticatedUser, audit, require_user},
    embedding,
    error::AppError,
    library_api,
};

#[derive(Deserialize)]
pub struct CreateConversationRequest {
    pub title: String,
    pub workspace_id: Option<Uuid>,
}

#[derive(Deserialize)]
pub struct AppendMessageRequest {
    pub role: String,
    pub text: String,
    pub provider: Option<String>,
    pub model: Option<String>,
    pub usage: Option<serde_json::Value>,
    pub tool_calls: Option<serde_json::Value>,
}

#[derive(Deserialize)]
pub struct ForkConversationRequest {
    pub title: Option<String>,
    pub at_message_id: Option<Uuid>,
}

#[derive(Deserialize)]
pub struct ConversationSearchQuery {
    pub q: String,
    pub workspace_id: Option<Uuid>,
    pub limit: Option<i64>,
}

#[derive(Serialize)]
pub struct ConversationResponse {
    pub id: Uuid,
    pub title: String,
    pub workspace_id: Option<Uuid>,
    pub status: String,
    pub forked_from_id: Option<Uuid>,
    pub forked_at_message_id: Option<Uuid>,
    pub created_at: OffsetDateTime,
    pub updated_at: OffsetDateTime,
}

#[derive(Serialize)]
pub struct MessageResponse {
    pub id: Uuid,
    pub ordinal: i64,
    pub role: String,
    pub text: String,
    pub provider: Option<String>,
    pub model: Option<String>,
    pub created_at: OffsetDateTime,
}

#[derive(Serialize)]
pub struct ConversationSearchResult {
    pub conversation_id: Uuid,
    pub message_id: Uuid,
    pub title: String,
    pub role: String,
    pub snippet: String,
    pub relevance: f32,
    pub created_at: OffsetDateTime,
}

pub async fn create_conversation(
    State(state): State<AppState>,
    headers: HeaderMap,
    Json(input): Json<CreateConversationRequest>,
) -> Result<(StatusCode, Json<ConversationResponse>), AppError> {
    let user = require_user(&state, &headers).await?;
    require_writer(&user)?;
    validate_title(&input.title)?;
    authorize_workspace(&state, user.profile_id, input.workspace_id).await?;
    let mut tx = state.pool.begin().await?;
    let conversation = insert_conversation(
        &mut tx,
        &user,
        input.title.trim(),
        input.workspace_id,
        None,
        None,
    )
    .await?;
    audit(
        &mut tx,
        Some(user.id),
        Some(user.profile_id),
        "conversation.created",
        "conversation",
        Some(conversation.id.to_string()),
        "success",
    )
    .await?;
    tx.commit().await?;
    Ok((StatusCode::CREATED, Json(conversation)))
}

pub async fn list_conversations(
    State(state): State<AppState>,
    headers: HeaderMap,
) -> Result<Json<Vec<ConversationResponse>>, AppError> {
    let user = require_user(&state, &headers).await?;
    let rows = sqlx::query(
        "SELECT id,title,workspace_id,status,forked_from_id,forked_at_message_id,created_at,updated_at \
         FROM conversations WHERE profile_id=$1 AND status <> 'deleted' ORDER BY updated_at DESC LIMIT 200",
    )
    .bind(user.profile_id)
    .fetch_all(&state.pool)
    .await?;
    Ok(Json(rows.iter().map(row_to_conversation).collect()))
}

pub async fn get_conversation(
    State(state): State<AppState>,
    headers: HeaderMap,
    Path(id): Path<Uuid>,
) -> Result<Json<ConversationResponse>, AppError> {
    let user = require_user(&state, &headers).await?;
    let row = conversation_row(&state, user.profile_id, id).await?;
    Ok(Json(row_to_conversation(&row)))
}

pub async fn append_message(
    State(state): State<AppState>,
    headers: HeaderMap,
    Path(id): Path<Uuid>,
    Json(input): Json<AppendMessageRequest>,
) -> Result<(StatusCode, Json<MessageResponse>), AppError> {
    let user = require_user(&state, &headers).await?;
    require_writer(&user)?;
    if input.role != "user"
        || input.provider.is_some()
        || input.model.is_some()
        || input.usage.is_some()
        || input.tool_calls.is_some()
    {
        return Err(AppError::Validation(
            "interactive submission accepts only unannotated user messages".into(),
        ));
    }
    if input.text.trim().is_empty() || input.text.chars().count() > 1_000_000 {
        return Err(AppError::Validation(
            "message text must contain 1 to 1000000 characters".into(),
        ));
    }
    let mut tx = state.pool.begin().await?;
    let conversation = sqlx::query(
        "SELECT id FROM conversations WHERE id=$1 AND profile_id=$2 AND status='active' FOR UPDATE",
    )
    .bind(id)
    .bind(user.profile_id)
    .fetch_optional(&mut *tx)
    .await?;
    if conversation.is_none() {
        return Err(AppError::NotFound);
    }
    let ordinal: i64 = sqlx::query_scalar(
        "SELECT coalesce(max(ordinal),0)+1 FROM messages WHERE conversation_id=$1",
    )
    .bind(id)
    .fetch_one(&mut *tx)
    .await?;
    let message_id = Uuid::now_v7();
    let now = OffsetDateTime::now_utc();
    sqlx::query(
        "INSERT INTO messages (id,conversation_id,ordinal,role,content,provider,model,usage,tool_calls,created_at) \
         VALUES ($1,$2,$3,$4,jsonb_build_object('text',$5::text),$6,$7,$8,$9,$10)",
    )
    .bind(message_id)
    .bind(id)
    .bind(ordinal)
    .bind(&input.role)
    .bind(&input.text)
    .bind(&input.provider)
    .bind(&input.model)
    .bind(&input.usage)
    .bind(&input.tool_calls)
    .bind(now)
    .execute(&mut *tx)
    .await?;
    sqlx::query("UPDATE conversations SET updated_at=$1 WHERE id=$2")
        .bind(now)
        .bind(id)
        .execute(&mut *tx)
        .await?;
    rebuild_projection(&mut tx, id, Some(user.id)).await?;
    audit(
        &mut tx,
        Some(user.id),
        Some(user.profile_id),
        "message.appended",
        "conversation",
        Some(id.to_string()),
        "success",
    )
    .await?;
    tx.commit().await?;
    Ok((
        StatusCode::CREATED,
        Json(MessageResponse {
            id: message_id,
            ordinal,
            role: input.role,
            text: input.text,
            provider: input.provider,
            model: input.model,
            created_at: now,
        }),
    ))
}

pub async fn list_messages(
    State(state): State<AppState>,
    headers: HeaderMap,
    Path(id): Path<Uuid>,
) -> Result<Json<Vec<MessageResponse>>, AppError> {
    let user = require_user(&state, &headers).await?;
    conversation_row(&state, user.profile_id, id).await?;
    let rows = sqlx::query(
        "SELECT id,ordinal,role,content->>'text' AS text,provider,model,created_at \
         FROM messages WHERE conversation_id=$1 ORDER BY ordinal LIMIT 10000",
    )
    .bind(id)
    .fetch_all(&state.pool)
    .await?;
    Ok(Json(rows.iter().map(row_to_message).collect()))
}

pub async fn fork_conversation(
    State(state): State<AppState>,
    headers: HeaderMap,
    Path(id): Path<Uuid>,
    Json(input): Json<ForkConversationRequest>,
) -> Result<(StatusCode, Json<ConversationResponse>), AppError> {
    let user = require_user(&state, &headers).await?;
    require_writer(&user)?;
    let mut tx = state.pool.begin().await?;
    let source = sqlx::query(
        "SELECT id,title,workspace_id FROM conversations \
         WHERE id=$1 AND profile_id=$2 AND status <> 'deleted' FOR UPDATE",
    )
    .bind(id)
    .bind(user.profile_id)
    .fetch_optional(&mut *tx)
    .await?
    .ok_or(AppError::NotFound)?;
    let source_title: String = source.get("title");
    let title = input
        .title
        .unwrap_or_else(|| format!("Fork of {source_title}"));
    validate_title(&title)?;
    let boundary = if let Some(message_id) = input.at_message_id {
        Some(
            sqlx::query("SELECT id,ordinal FROM messages WHERE id=$1 AND conversation_id=$2")
                .bind(message_id)
                .bind(id)
                .fetch_optional(&mut *tx)
                .await?
                .ok_or(AppError::NotFound)?,
        )
    } else {
        sqlx::query(
            "SELECT id,ordinal FROM messages WHERE conversation_id=$1 ORDER BY ordinal DESC LIMIT 1",
        )
        .bind(id)
        .fetch_optional(&mut *tx)
        .await?
    };
    let fork_message_id = boundary.as_ref().map(|row| row.get::<Uuid, _>("id"));
    let fork_ordinal = boundary.as_ref().map(|row| row.get::<i64, _>("ordinal"));
    let fork = insert_conversation(
        &mut tx,
        &user,
        title.trim(),
        source.get("workspace_id"),
        Some(id),
        fork_message_id,
    )
    .await?;
    sqlx::query(
        "INSERT INTO messages (id,conversation_id,ordinal,role,content,provider,model,usage,tool_calls,reasoning_metadata,created_at) \
         SELECT gen_random_uuid(),$1,ordinal,role,content,provider,model,usage,tool_calls,reasoning_metadata,created_at \
         FROM messages WHERE conversation_id=$2 AND ($3::bigint IS NULL OR ordinal <= $3) ORDER BY ordinal",
    )
    .bind(fork.id)
    .bind(id)
    .bind(fork_ordinal)
    .execute(&mut *tx)
    .await?;
    rebuild_projection(&mut tx, fork.id, Some(user.id)).await?;
    audit(
        &mut tx,
        Some(user.id),
        Some(user.profile_id),
        "conversation.forked",
        "conversation",
        Some(fork.id.to_string()),
        "success",
    )
    .await?;
    tx.commit().await?;
    Ok((StatusCode::CREATED, Json(fork)))
}

pub async fn delete_conversation(
    State(state): State<AppState>,
    headers: HeaderMap,
    Path(id): Path<Uuid>,
) -> Result<StatusCode, AppError> {
    let user = require_user(&state, &headers).await?;
    require_writer(&user)?;
    let mut tx = state.pool.begin().await?;
    let result = sqlx::query("DELETE FROM conversations WHERE id=$1 AND profile_id=$2")
        .bind(id)
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
        "conversation.deleted",
        "conversation",
        Some(id.to_string()),
        "success",
    )
    .await?;
    tx.commit().await?;
    Ok(StatusCode::NO_CONTENT)
}

pub async fn search_conversations(
    State(state): State<AppState>,
    headers: HeaderMap,
    Query(query): Query<ConversationSearchQuery>,
) -> Result<Json<Vec<ConversationSearchResult>>, AppError> {
    let user = require_user(&state, &headers).await?;
    let text = query.q.trim();
    if text.is_empty() || text.chars().count() > 1000 {
        return Err(AppError::Validation(
            "search query must contain 1 to 1000 characters".into(),
        ));
    }
    authorize_workspace(&state, user.profile_id, query.workspace_id).await?;
    let rows = sqlx::query(
        "SELECT c.id AS conversation_id,m.id AS message_id,c.title,m.role, \
         ts_headline('english',m.content->>'text',websearch_to_tsquery('english',$1),'MaxWords=32,MinWords=8') AS snippet, \
         ts_rank_cd(to_tsvector('english',coalesce(m.content->>'text','')),websearch_to_tsquery('english',$1)) AS relevance,m.created_at \
         FROM messages m JOIN conversations c ON c.id=m.conversation_id \
         WHERE c.profile_id=$2 AND c.status <> 'deleted' AND ($3::uuid IS NULL OR c.workspace_id=$3) \
           AND to_tsvector('english',coalesce(m.content->>'text','')) @@ websearch_to_tsquery('english',$1) \
         ORDER BY relevance DESC,m.created_at DESC LIMIT $4",
    )
    .bind(text)
    .bind(user.profile_id)
    .bind(query.workspace_id)
    .bind(query.limit.unwrap_or(50).clamp(1, 200))
    .fetch_all(&state.pool)
    .await?;
    Ok(Json(
        rows.into_iter()
            .map(|row| ConversationSearchResult {
                conversation_id: row.get("conversation_id"),
                message_id: row.get("message_id"),
                title: row.get("title"),
                role: row.get("role"),
                snippet: row.get("snippet"),
                relevance: row.get("relevance"),
                created_at: row.get("created_at"),
            })
            .collect(),
    ))
}

async fn insert_conversation(
    tx: &mut sqlx::Transaction<'_, sqlx::Postgres>,
    user: &AuthenticatedUser,
    title: &str,
    workspace_id: Option<Uuid>,
    forked_from_id: Option<Uuid>,
    forked_at_message_id: Option<Uuid>,
) -> Result<ConversationResponse, AppError> {
    let id = Uuid::now_v7();
    let book_id = Uuid::now_v7();
    let now = OffsetDateTime::now_utc();
    sqlx::query(
        "INSERT INTO conversations (id,profile_id,workspace_id,title,forked_from_id,forked_at_message_id,created_at,updated_at) \
         VALUES ($1,$2,$3,$4,$5,$6,$7,$7)",
    )
    .bind(id)
    .bind(user.profile_id)
    .bind(workspace_id)
    .bind(title)
    .bind(forked_from_id)
    .bind(forked_at_message_id)
    .bind(now)
    .execute(&mut **tx)
    .await?;
    sqlx::query(
        "INSERT INTO books (id,profile_id,title,body,book_type,scope,provenance,trust,source,author,workspace_id,conversation_id,security_classification,created_by_user_id,created_at,updated_at) \
         VALUES ($1,$2,$3,'','CONVERSATION','CONVERSATION','CONVERSATION','VERIFIED',jsonb_build_object('conversation_id',$4::text),$5,$6,$4,'INTERNAL',$7,$8,$8)",
    )
    .bind(book_id)
    .bind(user.profile_id)
    .bind(title)
    .bind(id)
    .bind(&user.display_name)
    .bind(workspace_id)
    .bind(user.id)
    .bind(now)
    .execute(&mut **tx)
    .await?;
    sqlx::query(
        "INSERT INTO book_revisions (id,book_id,revision,title,body,tags,metadata,changed_by,change_reason,created_at) \
         VALUES ($1,$2,1,$3,'','{}','{}',$4,'Initial conversation projection',$5)",
    )
    .bind(Uuid::now_v7())
    .bind(book_id)
    .bind(title)
    .bind(user.id)
    .bind(now)
    .execute(&mut **tx)
    .await?;
    embedding::enqueue_book(tx, user.profile_id, book_id, 1).await?;
    Ok(ConversationResponse {
        id,
        title: title.into(),
        workspace_id,
        status: "active".into(),
        forked_from_id,
        forked_at_message_id,
        created_at: now,
        updated_at: now,
    })
}

async fn rebuild_projection(
    tx: &mut sqlx::Transaction<'_, sqlx::Postgres>,
    conversation_id: Uuid,
    changed_by: Option<Uuid>,
) -> Result<(), AppError> {
    let body: String = sqlx::query_scalar(
        "SELECT coalesce(string_agg(upper(role) || ': ' || coalesce(content->>'text',''), E'\\n\\n' ORDER BY ordinal),'') \
         FROM messages WHERE conversation_id=$1",
    )
    .bind(conversation_id)
    .fetch_one(&mut **tx)
    .await?;
    let book_id: Uuid = sqlx::query_scalar(
        "SELECT id FROM books WHERE conversation_id=$1 AND book_type='CONVERSATION'",
    )
    .bind(conversation_id)
    .fetch_one(&mut **tx)
    .await?;
    library_api::replace_book_body(
        tx,
        book_id,
        &body,
        changed_by,
        "Conversation projection updated",
    )
    .await?;
    Ok(())
}

async fn conversation_row(
    state: &AppState,
    profile_id: Uuid,
    id: Uuid,
) -> Result<sqlx::postgres::PgRow, AppError> {
    sqlx::query(
        "SELECT id,title,workspace_id,status,forked_from_id,forked_at_message_id,created_at,updated_at \
         FROM conversations WHERE id=$1 AND profile_id=$2 AND status <> 'deleted'",
    )
    .bind(id)
    .bind(profile_id)
    .fetch_optional(&state.pool)
    .await?
    .ok_or(AppError::NotFound)
}

async fn authorize_workspace(
    state: &AppState,
    profile_id: Uuid,
    workspace_id: Option<Uuid>,
) -> Result<(), AppError> {
    if let Some(workspace_id) = workspace_id {
        let exists: bool = sqlx::query_scalar(
            "SELECT EXISTS(SELECT 1 FROM workspaces WHERE id=$1 AND profile_id=$2)",
        )
        .bind(workspace_id)
        .bind(profile_id)
        .fetch_one(&state.pool)
        .await?;
        if !exists {
            return Err(AppError::Forbidden);
        }
    }
    Ok(())
}

fn row_to_conversation(row: &sqlx::postgres::PgRow) -> ConversationResponse {
    ConversationResponse {
        id: row.get("id"),
        title: row.get("title"),
        workspace_id: row.get("workspace_id"),
        status: row.get("status"),
        forked_from_id: row.get("forked_from_id"),
        forked_at_message_id: row.get("forked_at_message_id"),
        created_at: row.get("created_at"),
        updated_at: row.get("updated_at"),
    }
}

fn row_to_message(row: &sqlx::postgres::PgRow) -> MessageResponse {
    MessageResponse {
        id: row.get("id"),
        ordinal: row.get("ordinal"),
        role: row.get("role"),
        text: row.get("text"),
        provider: row.get("provider"),
        model: row.get("model"),
        created_at: row.get("created_at"),
    }
}

fn validate_title(title: &str) -> Result<(), AppError> {
    if title.trim().is_empty() || title.chars().count() > 500 {
        return Err(AppError::Validation(
            "conversation title must contain 1 to 500 characters".into(),
        ));
    }
    Ok(())
}

fn require_writer(user: &AuthenticatedUser) -> Result<(), AppError> {
    if user.role == "VIEWER" {
        Err(AppError::Forbidden)
    } else {
        Ok(())
    }
}
