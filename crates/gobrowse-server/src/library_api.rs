use axum::{
    Json,
    extract::{Path, Query, State},
    http::HeaderMap,
};
use gobrowse_core::library::{
    Book, BookScope, BookType, EmbeddingStatus, Provenance, SecurityClassification, TrustLevel,
    chunk_text,
};
use serde::{Deserialize, Serialize};
use sqlx::Row;
use time::OffsetDateTime;
use uuid::Uuid;

use crate::{
    AppState,
    auth::{audit, require_user},
    error::AppError,
};

#[derive(Debug, Deserialize)]
pub struct CreateBookRequest {
    pub title: String,
    pub body: String,
    pub book_type: BookType,
    pub scope: BookScope,
    #[serde(default)]
    pub tags: Vec<String>,
    pub provenance: Provenance,
    pub trust: TrustLevel,
    pub workspace_id: Option<Uuid>,
    pub conversation_id: Option<Uuid>,
    pub security_classification: SecurityClassification,
    #[serde(default = "empty_object")]
    pub metadata: serde_json::Value,
}

#[derive(Debug, Deserialize)]
pub struct UpdateBookRequest {
    pub title: String,
    pub body: String,
    #[serde(default)]
    pub tags: Vec<String>,
    #[serde(default = "empty_object")]
    pub metadata: serde_json::Value,
    pub expected_revision: i64,
    pub reason: String,
}

#[derive(Debug, Deserialize)]
pub struct SearchQuery {
    pub q: String,
    pub workspace_id: Option<Uuid>,
    pub limit: Option<i64>,
}

#[derive(Debug, Serialize)]
pub struct BookSummary {
    pub id: Uuid,
    pub title: String,
    pub snippet: String,
    pub book_type: String,
    pub scope: String,
    pub tags: Vec<String>,
    pub provenance: String,
    pub trust: String,
    pub revision: i64,
    pub relevance: f32,
    pub updated_at: OffsetDateTime,
}

#[derive(Debug, Serialize)]
pub struct BookRevisionResponse {
    pub revision: i64,
    pub title: String,
    pub body: String,
    pub tags: Vec<String>,
    pub change_reason: String,
    pub created_at: OffsetDateTime,
}

pub async fn create_book(
    State(state): State<AppState>,
    headers: HeaderMap,
    Json(input): Json<CreateBookRequest>,
) -> Result<Json<Book>, AppError> {
    let user = require_user(&state, &headers).await?;
    let now = OffsetDateTime::now_utc();
    let book = Book {
        id: Uuid::now_v7(),
        profile_id: user.profile_id,
        title: input.title.trim().to_owned(),
        body: input.body,
        book_type: input.book_type,
        scope: input.scope,
        tags: normalize_tags(input.tags)?,
        provenance: input.provenance,
        trust: input.trust,
        author: user.display_name.clone(),
        workspace_id: input.workspace_id,
        conversation_id: input.conversation_id,
        security_classification: input.security_classification,
        embedding_status: EmbeddingStatus::Pending,
        metadata: input.metadata,
        revision: 1,
        created_at: now,
        updated_at: now,
    };
    book.validate()
        .map_err(|error| AppError::Validation(error.to_string()))?;
    authorize_workspace(&state, user.profile_id, book.workspace_id).await?;

    let mut tx = state.pool.begin().await?;
    sqlx::query(
        "INSERT INTO books (id, profile_id, title, body, book_type, scope, tags, provenance, trust, author, \
         workspace_id, conversation_id, security_classification, metadata, created_at, updated_at) \
         VALUES ($1,$2,$3,$4,$5,$6,$7,$8,$9,$10,$11,$12,$13,$14,$15,$15)",
    )
    .bind(book.id)
    .bind(book.profile_id)
    .bind(&book.title)
    .bind(&book.body)
    .bind(enum_db(&book.book_type))
    .bind(enum_db(&book.scope))
    .bind(&book.tags)
    .bind(enum_db(&book.provenance))
    .bind(enum_db(&book.trust))
    .bind(&book.author)
    .bind(book.workspace_id)
    .bind(book.conversation_id)
    .bind(enum_db(&book.security_classification))
    .bind(&book.metadata)
    .bind(now)
    .execute(&mut *tx)
    .await?;
    insert_chunks(&mut tx, book.id, &book.body).await?;
    audit(
        &mut tx,
        Some(user.id),
        Some(user.profile_id),
        "book.created",
        "book",
        Some(book.id.to_string()),
        "success",
    )
    .await?;
    tx.commit().await?;
    Ok(Json(book))
}

pub async fn get_book(
    State(state): State<AppState>,
    headers: HeaderMap,
    Path(id): Path<Uuid>,
) -> Result<Json<Book>, AppError> {
    let user = require_user(&state, &headers).await?;
    let row = sqlx::query("SELECT * FROM books WHERE id = $1 AND profile_id = $2")
        .bind(id)
        .bind(user.profile_id)
        .fetch_optional(&state.pool)
        .await?
        .ok_or(AppError::NotFound)?;
    Ok(Json(row_to_book(&row)?))
}

pub async fn search_books(
    State(state): State<AppState>,
    headers: HeaderMap,
    Query(query): Query<SearchQuery>,
) -> Result<Json<Vec<BookSummary>>, AppError> {
    let user = require_user(&state, &headers).await?;
    let text = query.q.trim();
    if text.is_empty() || text.chars().count() > 1_000 {
        return Err(AppError::Validation(
            "search query must contain 1 to 1000 characters".into(),
        ));
    }
    authorize_workspace(&state, user.profile_id, query.workspace_id).await?;
    let limit = query.limit.unwrap_or(20).clamp(1, 100);
    let rows = sqlx::query(
        "SELECT id, title, ts_headline('english', body, websearch_to_tsquery('english', $1), \
             'MaxWords=32, MinWords=8, ShortWord=3') AS snippet, book_type, scope, tags, provenance, trust, revision, \
             ts_rank_cd(search_document, websearch_to_tsquery('english', $1)) AS relevance, updated_at \
         FROM books WHERE profile_id = $2 AND ($3::uuid IS NULL OR workspace_id = $3) \
           AND search_document @@ websearch_to_tsquery('english', $1) \
         ORDER BY relevance DESC, updated_at DESC LIMIT $4",
    )
    .bind(text)
    .bind(user.profile_id)
    .bind(query.workspace_id)
    .bind(limit)
    .fetch_all(&state.pool)
    .await?;
    Ok(Json(
        rows.into_iter()
            .map(|row| BookSummary {
                id: row.get("id"),
                title: row.get("title"),
                snippet: row.get("snippet"),
                book_type: row.get("book_type"),
                scope: row.get("scope"),
                tags: row.get("tags"),
                provenance: row.get("provenance"),
                trust: row.get("trust"),
                revision: row.get("revision"),
                relevance: row.get("relevance"),
                updated_at: row.get("updated_at"),
            })
            .collect(),
    ))
}

pub async fn list_books(
    State(state): State<AppState>,
    headers: HeaderMap,
) -> Result<Json<Vec<BookSummary>>, AppError> {
    let user = require_user(&state, &headers).await?;
    let rows = sqlx::query(
        "SELECT id, title, left(body, 240) AS snippet, book_type, scope, tags, provenance, trust, revision, \
         0.0::real AS relevance, updated_at FROM books WHERE profile_id = $1 ORDER BY updated_at DESC LIMIT 100",
    )
    .bind(user.profile_id)
    .fetch_all(&state.pool)
    .await?;
    Ok(Json(
        rows.into_iter()
            .map(|row| BookSummary {
                id: row.get("id"),
                title: row.get("title"),
                snippet: row.get("snippet"),
                book_type: row.get("book_type"),
                scope: row.get("scope"),
                tags: row.get("tags"),
                provenance: row.get("provenance"),
                trust: row.get("trust"),
                revision: row.get("revision"),
                relevance: row.get("relevance"),
                updated_at: row.get("updated_at"),
            })
            .collect(),
    ))
}

pub async fn update_book(
    State(state): State<AppState>,
    headers: HeaderMap,
    Path(id): Path<Uuid>,
    Json(input): Json<UpdateBookRequest>,
) -> Result<Json<Book>, AppError> {
    let user = require_user(&state, &headers).await?;
    if input.reason.trim().is_empty() {
        return Err(AppError::Validation("a revision reason is required".into()));
    }
    let tags = normalize_tags(input.tags)?;
    let mut tx = state.pool.begin().await?;
    let row = sqlx::query("SELECT * FROM books WHERE id = $1 AND profile_id = $2 FOR UPDATE")
        .bind(id)
        .bind(user.profile_id)
        .fetch_optional(&mut *tx)
        .await?
        .ok_or(AppError::NotFound)?;
    let mut book = row_to_book(&row)?;
    if book.revision != input.expected_revision {
        return Err(AppError::Conflict("book was changed by another writer"));
    }
    book.title = input.title.trim().to_owned();
    book.body = input.body;
    book.tags = tags;
    book.metadata = input.metadata;
    book.validate()
        .map_err(|error| AppError::Validation(error.to_string()))?;
    sqlx::query(
        "INSERT INTO book_revisions (id, book_id, revision, title, body, tags, metadata, changed_by, change_reason) \
         VALUES ($1,$2,$3,$4,$5,$6,$7,$8,$9)",
    )
    .bind(Uuid::now_v7()).bind(id).bind(book.revision).bind(row.get::<String, _>("title"))
    .bind(row.get::<String, _>("body")).bind(row.get::<Vec<String>, _>("tags"))
    .bind(row.get::<serde_json::Value, _>("metadata")).bind(user.id).bind(input.reason.trim())
    .execute(&mut *tx).await?;
    book.revision += 1;
    book.updated_at = OffsetDateTime::now_utc();
    sqlx::query(
        "UPDATE books SET title=$1, body=$2, tags=$3, metadata=$4, revision=$5, updated_at=$6, embedding_status='stale' \
         WHERE id=$7",
    )
    .bind(&book.title).bind(&book.body).bind(&book.tags).bind(&book.metadata)
    .bind(book.revision).bind(book.updated_at).bind(id).execute(&mut *tx).await?;
    sqlx::query("DELETE FROM book_chunks WHERE book_id = $1")
        .bind(id)
        .execute(&mut *tx)
        .await?;
    insert_chunks(&mut tx, id, &book.body).await?;
    audit(
        &mut tx,
        Some(user.id),
        Some(user.profile_id),
        "book.updated",
        "book",
        Some(id.to_string()),
        "success",
    )
    .await?;
    tx.commit().await?;
    book.embedding_status = EmbeddingStatus::Stale;
    Ok(Json(book))
}

pub async fn book_history(
    State(state): State<AppState>,
    headers: HeaderMap,
    Path(id): Path<Uuid>,
) -> Result<Json<Vec<BookRevisionResponse>>, AppError> {
    let user = require_user(&state, &headers).await?;
    let rows = sqlx::query(
        "SELECT r.revision, r.title, r.body, r.tags, r.change_reason, r.created_at \
         FROM book_revisions r JOIN books b ON b.id = r.book_id \
         WHERE r.book_id = $1 AND b.profile_id = $2 ORDER BY r.revision DESC",
    )
    .bind(id)
    .bind(user.profile_id)
    .fetch_all(&state.pool)
    .await?;
    Ok(Json(
        rows.into_iter()
            .map(|row| BookRevisionResponse {
                revision: row.get("revision"),
                title: row.get("title"),
                body: row.get("body"),
                tags: row.get("tags"),
                change_reason: row.get("change_reason"),
                created_at: row.get("created_at"),
            })
            .collect(),
    ))
}

async fn authorize_workspace(
    state: &AppState,
    profile_id: Uuid,
    workspace_id: Option<Uuid>,
) -> Result<(), AppError> {
    if let Some(workspace_id) = workspace_id {
        let allowed: bool = sqlx::query_scalar(
            "SELECT EXISTS(SELECT 1 FROM workspaces WHERE id = $1 AND profile_id = $2)",
        )
        .bind(workspace_id)
        .bind(profile_id)
        .fetch_one(&state.pool)
        .await?;
        if !allowed {
            return Err(AppError::Forbidden);
        }
    }
    Ok(())
}

async fn insert_chunks(
    tx: &mut sqlx::Transaction<'_, sqlx::Postgres>,
    book_id: Uuid,
    body: &str,
) -> Result<(), AppError> {
    for chunk in chunk_text(body, 4_000, 400) {
        sqlx::query(
            "INSERT INTO book_chunks (id, book_id, ordinal, text, token_estimate, source_start, source_end) \
             VALUES ($1,$2,$3,$4,$5,$6,$7)",
        )
        .bind(Uuid::now_v7()).bind(book_id).bind(i32::try_from(chunk.ordinal).map_err(|_| AppError::Validation("book has too many chunks".into()))?)
        .bind(chunk.text).bind(i32::try_from(chunk.token_estimate).unwrap_or(i32::MAX))
        .bind(i32::try_from(chunk.source_start).map_err(|_| AppError::Validation("book source offset is too large".into()))?)
        .bind(i32::try_from(chunk.source_end).map_err(|_| AppError::Validation("book source offset is too large".into()))?)
        .execute(&mut **tx).await?;
    }
    Ok(())
}

fn row_to_book(row: &sqlx::postgres::PgRow) -> Result<Book, AppError> {
    Ok(Book {
        id: row.get("id"),
        profile_id: row.get("profile_id"),
        title: row.get("title"),
        body: row.get("body"),
        book_type: parse_book_type(row.get::<String, _>("book_type").as_str())?,
        scope: parse_scope(row.get::<String, _>("scope").as_str())?,
        tags: row.get("tags"),
        provenance: parse_provenance(row.get::<String, _>("provenance").as_str())?,
        trust: parse_trust(row.get::<String, _>("trust").as_str())?,
        author: row.get("author"),
        workspace_id: row.get("workspace_id"),
        conversation_id: row.get("conversation_id"),
        security_classification: parse_security(
            row.get::<String, _>("security_classification").as_str(),
        )?,
        embedding_status: parse_embedding(row.get::<String, _>("embedding_status").as_str())?,
        metadata: row.get("metadata"),
        revision: row.get("revision"),
        created_at: row.get("created_at"),
        updated_at: row.get("updated_at"),
    })
}

fn enum_db<T: Serialize>(value: &T) -> String {
    serde_json::to_value(value)
        .expect("enum serialization cannot fail")
        .as_str()
        .expect("enum serializes to string")
        .to_owned()
}

fn parse_book_type(value: &str) -> Result<BookType, AppError> {
    parse_enum(value)
}
fn parse_scope(value: &str) -> Result<BookScope, AppError> {
    parse_enum(value)
}
fn parse_provenance(value: &str) -> Result<Provenance, AppError> {
    parse_enum(value)
}
fn parse_trust(value: &str) -> Result<TrustLevel, AppError> {
    parse_enum(value)
}
fn parse_security(value: &str) -> Result<SecurityClassification, AppError> {
    parse_enum(value)
}
fn parse_embedding(value: &str) -> Result<EmbeddingStatus, AppError> {
    parse_enum(value)
}

fn parse_enum<T: serde::de::DeserializeOwned>(value: &str) -> Result<T, AppError> {
    serde_json::from_value(serde_json::Value::String(value.to_owned()))
        .map_err(|error| AppError::Internal(anyhow::anyhow!(error)))
}

fn normalize_tags(tags: Vec<String>) -> Result<Vec<String>, AppError> {
    let mut tags: Vec<_> = tags
        .into_iter()
        .map(|tag| tag.trim().to_lowercase())
        .filter(|tag| !tag.is_empty())
        .collect();
    tags.sort();
    tags.dedup();
    if tags.len() > 64 || tags.iter().any(|tag| tag.chars().count() > 80) {
        return Err(AppError::Validation(
            "use at most 64 tags of at most 80 characters".into(),
        ));
    }
    Ok(tags)
}

fn empty_object() -> serde_json::Value {
    serde_json::json!({})
}
