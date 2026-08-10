use std::collections::{HashMap, HashSet};

use axum::{
    Json,
    extract::{Path, Query, State},
    http::HeaderMap,
};
use gobrowse_core::library::{
    Book, BookScope, BookType, EmbeddingStatus, Provenance, RankingWeights, SecurityClassification,
    TrustLevel, chunk_text, rank_fusion,
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
    pub retrieval_mode: String,
    pub lexical_score: Option<f32>,
    pub semantic_score: Option<f32>,
    pub updated_at: OffsetDateTime,
}

struct SearchCandidate {
    summary: BookSummary,
    workspace_id: Option<Uuid>,
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
    require_book_writer(&user)?;
    if matches!(
        input.book_type,
        BookType::Conversation | BookType::Autobiography
    ) {
        return Err(AppError::Validation(
            "conversation and Autobiography Books use dedicated workflows".into(),
        ));
    }
    if input.scope == BookScope::Agent {
        return Err(AppError::Validation(
            "agent-scoped Books require an agent workflow".into(),
        ));
    }
    if input.scope == BookScope::Global && !is_admin(&user) {
        return Err(AppError::Forbidden);
    }
    if input.security_classification == SecurityClassification::Restricted && !is_admin(&user) {
        return Err(AppError::Forbidden);
    }
    if !is_admin(&user)
        && (input.provenance != Provenance::User || input.trust != TrustLevel::UserProvided)
    {
        return Err(AppError::Forbidden);
    }
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
    authorize_conversation(&state, user.profile_id, book.conversation_id).await?;
    let owner_user_id =
        matches!(book.scope, BookScope::User | BookScope::Private).then_some(user.id);

    let mut tx = state.pool.begin().await?;
    sqlx::query(
        "INSERT INTO books (id, profile_id, title, body, book_type, scope, tags, provenance, trust, author, \
         workspace_id, conversation_id, security_classification, metadata, owner_user_id, created_by_user_id, created_at, updated_at) \
         VALUES ($1,$2,$3,$4,$5,$6,$7,$8,$9,$10,$11,$12,$13,$14,$15,$16,$17,$17)",
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
    .bind(owner_user_id)
    .bind(user.id)
    .bind(now)
    .execute(&mut *tx)
    .await?;
    insert_chunks(&mut tx, book.id, &book.body).await?;
    insert_revision(&mut tx, &book, Some(user.id), "Initial Book revision").await?;
    embedding::enqueue_book(&mut tx, book.profile_id, book.id, book.revision).await?;
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
    let row = sqlx::query(
        "SELECT * FROM books WHERE id=$1 AND profile_id=$2 \
         AND ($3 IN ('OWNER','ADMIN') OR security_classification <> 'RESTRICTED') \
         AND ($3 IN ('OWNER','ADMIN') OR scope <> 'AGENT') \
         AND ($3 IN ('OWNER','ADMIN') OR scope NOT IN ('USER','PRIVATE') OR owner_user_id=$4)",
    )
    .bind(id)
    .bind(user.profile_id)
    .bind(&user.role)
    .bind(user.id)
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
    let candidate_limit = (limit * 3).min(300);
    let lexical_rows = sqlx::query(
        "SELECT id, title, ts_headline('english', body, websearch_to_tsquery('english', $1), \
             'MaxWords=32, MinWords=8, ShortWord=3') AS snippet, book_type, scope, tags, provenance, trust, revision, \
             ts_rank_cd(search_document, websearch_to_tsquery('english', $1)) AS relevance, workspace_id, updated_at \
         FROM books WHERE profile_id = $2 \
           AND ($5 IN ('OWNER','ADMIN') OR security_classification <> 'RESTRICTED') \
           AND ($5 IN ('OWNER','ADMIN') OR scope <> 'AGENT') \
           AND ($5 IN ('OWNER','ADMIN') OR scope NOT IN ('USER','PRIVATE') OR owner_user_id=$6) \
           AND ($3::uuid IS NULL OR scope IN ('GLOBAL','PROFILE','USER','PRIVATE','AGENT') OR workspace_id=$3 \
                OR (scope='CONVERSATION' AND EXISTS(SELECT 1 FROM conversations c WHERE c.id=books.conversation_id AND c.workspace_id=$3))) \
           AND search_document @@ websearch_to_tsquery('english', $1) \
         ORDER BY relevance DESC, updated_at DESC LIMIT $4",
    )
    .bind(text)
    .bind(user.profile_id)
    .bind(query.workspace_id)
    .bind(candidate_limit)
    .bind(&user.role)
    .bind(user.id)
    .fetch_all(&state.pool)
    .await?;
    let mut candidates = HashMap::new();
    let mut lexical_ids = Vec::with_capacity(lexical_rows.len());
    for row in lexical_rows {
        let candidate = search_candidate(&row, "lexical", Some(row.get("relevance")), None);
        lexical_ids.push(candidate.summary.id);
        candidates.insert(candidate.summary.id, candidate);
    }

    let mut semantic_ids = Vec::new();
    if let Some((model_id, vector)) = embedding::embed_query(&state, user.profile_id, text).await? {
        let vector = embedding::vector_literal(&vector);
        let semantic_rows = sqlx::query(
            "SELECT * FROM (SELECT DISTINCT ON (b.id) b.id,b.title,left(c.text,400) AS snippet,b.book_type,b.scope,b.tags, \
                 b.provenance,b.trust,b.revision,b.workspace_id,b.updated_at, \
                 (1-(e.embedding <=> ($1::text)::vector))::real AS relevance, \
                 e.embedding <=> ($1::text)::vector AS distance \
             FROM book_chunk_embeddings e JOIN book_chunks c ON c.id=e.chunk_id JOIN books b ON b.id=c.book_id \
             WHERE e.embedding_model_id=$2 AND e.book_revision=b.revision AND b.profile_id=$3 \
               AND ($5 IN ('OWNER','ADMIN') OR b.security_classification <> 'RESTRICTED') \
               AND ($5 IN ('OWNER','ADMIN') OR b.scope <> 'AGENT') \
               AND ($5 IN ('OWNER','ADMIN') OR b.scope NOT IN ('USER','PRIVATE') OR b.owner_user_id=$6) \
               AND ($4::uuid IS NULL OR b.scope IN ('GLOBAL','PROFILE','USER','PRIVATE','AGENT') OR b.workspace_id=$4 \
                    OR (b.scope='CONVERSATION' AND EXISTS(SELECT 1 FROM conversations conversation \
                        WHERE conversation.id=b.conversation_id AND conversation.workspace_id=$4))) \
             ORDER BY b.id, distance) ranked ORDER BY distance,id LIMIT $7",
        )
        .bind(vector)
        .bind(model_id)
        .bind(user.profile_id)
        .bind(query.workspace_id)
        .bind(&user.role)
        .bind(user.id)
        .bind(candidate_limit)
        .fetch_all(&state.pool)
        .await?;
        for row in semantic_rows {
            let id: Uuid = row.get("id");
            let score = row.get("relevance");
            semantic_ids.push(id);
            if let Some(candidate) = candidates.get_mut(&id) {
                candidate.summary.semantic_score = Some(score);
                candidate.summary.retrieval_mode = "hybrid".into();
            } else {
                candidates.insert(id, search_candidate(&row, "semantic", None, Some(score)));
            }
        }
    }
    let now = OffsetDateTime::now_utc();
    let boosts = candidates
        .iter()
        .map(|(id, candidate)| {
            let age_days = (now - candidate.summary.updated_at).whole_days().max(0) as f32;
            let recency = 1.0 / (1.0 + age_days / 30.0);
            let source = match candidate.summary.trust.as_str() {
                "VERIFIED" => 1.0,
                "USER_PROVIDED" => 0.8,
                "AGENT_INFERRED" => 0.5,
                "EXTERNAL" => 0.3,
                _ => 0.0,
            };
            let workspace =
                if query.workspace_id.is_some() && query.workspace_id == candidate.workspace_id {
                    1.0
                } else {
                    0.0
                };
            (*id, (recency, source, workspace))
        })
        .collect();
    let lexical_set: HashSet<_> = lexical_ids.iter().copied().collect();
    let semantic_set: HashSet<_> = semantic_ids.iter().copied().collect();
    let ranked = rank_fusion(
        &lexical_ids,
        &semantic_ids,
        &boosts,
        RankingWeights::default(),
    );
    let results = ranked
        .into_iter()
        .take(usize::try_from(limit).unwrap_or(100))
        .filter_map(|ranked| {
            candidates.remove(&ranked.id).map(|mut candidate| {
                candidate.summary.relevance = ranked.score;
                candidate.summary.retrieval_mode = match (
                    lexical_set.contains(&ranked.id),
                    semantic_set.contains(&ranked.id),
                ) {
                    (true, true) => "hybrid",
                    (true, false) => "lexical",
                    (false, true) => "semantic",
                    (false, false) => "metadata",
                }
                .into();
                candidate.summary
            })
        })
        .collect();
    Ok(Json(results))
}

pub async fn list_books(
    State(state): State<AppState>,
    headers: HeaderMap,
) -> Result<Json<Vec<BookSummary>>, AppError> {
    let user = require_user(&state, &headers).await?;
    let rows = sqlx::query(
        "SELECT id, title, left(body, 240) AS snippet, book_type, scope, tags, provenance, trust, revision, \
         0.0::real AS relevance, updated_at FROM books WHERE profile_id=$1 \
         AND ($2 IN ('OWNER','ADMIN') OR security_classification <> 'RESTRICTED') \
         AND ($2 IN ('OWNER','ADMIN') OR scope <> 'AGENT') \
         AND ($2 IN ('OWNER','ADMIN') OR scope NOT IN ('USER','PRIVATE') OR owner_user_id=$3) \
         ORDER BY updated_at DESC LIMIT 100",
    )
    .bind(user.profile_id)
    .bind(&user.role)
    .bind(user.id)
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
                retrieval_mode: "recent".into(),
                lexical_score: None,
                semantic_score: None,
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
    require_book_writer(&user)?;
    if input.reason.trim().is_empty() {
        return Err(AppError::Validation("a revision reason is required".into()));
    }
    let tags = normalize_tags(input.tags)?;
    let mut tx = state.pool.begin().await?;
    let row = sqlx::query(
        "SELECT * FROM books WHERE id=$1 AND profile_id=$2 \
         AND ($3 IN ('OWNER','ADMIN') OR security_classification <> 'RESTRICTED') \
         AND ($3 IN ('OWNER','ADMIN') OR scope <> 'AGENT') \
         AND ($3 IN ('OWNER','ADMIN') OR scope NOT IN ('USER','PRIVATE') OR owner_user_id=$4) \
         AND ($3 IN ('OWNER','ADMIN') OR (created_by_user_id=$4 AND provenance='USER' AND trust='USER_PROVIDED' AND scope <> 'GLOBAL')) \
         FOR UPDATE",
    )
        .bind(id)
        .bind(user.profile_id)
        .bind(&user.role)
        .bind(user.id)
        .fetch_optional(&mut *tx)
        .await?
        .ok_or(AppError::NotFound)?;
    let mut book = row_to_book(&row)?;
    if matches!(
        book.book_type,
        BookType::Conversation | BookType::Autobiography
    ) {
        return Err(AppError::Validation(
            "managed Books must be changed through their dedicated workflow".into(),
        ));
    }
    if book.revision != input.expected_revision {
        return Err(AppError::Conflict("book was changed by another writer"));
    }
    book.title = input.title.trim().to_owned();
    book.body = input.body;
    book.tags = tags;
    book.metadata = input.metadata;
    book.validate()
        .map_err(|error| AppError::Validation(error.to_string()))?;
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
    insert_revision(&mut tx, &book, Some(user.id), input.reason.trim()).await?;
    embedding::enqueue_book(&mut tx, book.profile_id, id, book.revision).await?;
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
         WHERE r.book_id=$1 AND b.profile_id=$2 \
           AND ($3 IN ('OWNER','ADMIN') OR b.security_classification <> 'RESTRICTED') \
           AND ($3 IN ('OWNER','ADMIN') OR b.scope <> 'AGENT') \
           AND ($3 IN ('OWNER','ADMIN') OR b.scope NOT IN ('USER','PRIVATE') OR b.owner_user_id=$4) \
         ORDER BY r.revision DESC",
    )
    .bind(id)
    .bind(user.profile_id)
    .bind(&user.role)
    .bind(user.id)
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

async fn authorize_conversation(
    state: &AppState,
    profile_id: Uuid,
    conversation_id: Option<Uuid>,
) -> Result<(), AppError> {
    if let Some(conversation_id) = conversation_id {
        let allowed: bool = sqlx::query_scalar(
            "SELECT EXISTS(SELECT 1 FROM conversations WHERE id=$1 AND profile_id=$2 AND status <> 'deleted')",
        )
        .bind(conversation_id)
        .bind(profile_id)
        .fetch_one(&state.pool)
        .await?;
        if !allowed {
            return Err(AppError::Forbidden);
        }
    }
    Ok(())
}

pub(crate) async fn insert_chunks(
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

pub(crate) async fn replace_book_body(
    tx: &mut sqlx::Transaction<'_, sqlx::Postgres>,
    book_id: Uuid,
    body: &str,
    changed_by: Option<Uuid>,
    reason: &str,
) -> Result<i64, AppError> {
    let row = sqlx::query(
        "SELECT profile_id,title,tags,metadata,revision FROM books WHERE id=$1 FOR UPDATE",
    )
    .bind(book_id)
    .fetch_optional(&mut **tx)
    .await?
    .ok_or(AppError::NotFound)?;
    let profile_id: Uuid = row.get("profile_id");
    let revision: i64 = row.get::<i64, _>("revision") + 1;
    sqlx::query(
        "UPDATE books SET body=$1,revision=$2,embedding_status='stale',updated_at=now() WHERE id=$3",
    )
    .bind(body)
    .bind(revision)
    .bind(book_id)
    .execute(&mut **tx)
    .await?;
    sqlx::query("DELETE FROM book_chunks WHERE book_id=$1")
        .bind(book_id)
        .execute(&mut **tx)
        .await?;
    insert_chunks(tx, book_id, body).await?;
    sqlx::query(
        "INSERT INTO book_revisions (id,book_id,revision,title,body,tags,metadata,changed_by,change_reason) \
         VALUES ($1,$2,$3,$4,$5,$6,$7,$8,$9)",
    )
    .bind(Uuid::now_v7())
    .bind(book_id)
    .bind(revision)
    .bind(row.get::<String, _>("title"))
    .bind(body)
    .bind(row.get::<Vec<String>, _>("tags"))
    .bind(row.get::<serde_json::Value, _>("metadata"))
    .bind(changed_by)
    .bind(reason)
    .execute(&mut **tx)
    .await?;
    embedding::enqueue_book(tx, profile_id, book_id, revision).await?;
    Ok(revision)
}

#[allow(clippy::too_many_arguments)]
pub(crate) async fn replace_book_snapshot(
    tx: &mut sqlx::Transaction<'_, sqlx::Postgres>,
    book_id: Uuid,
    title: &str,
    body: &str,
    tags: &[String],
    metadata: &serde_json::Value,
    changed_by: Option<Uuid>,
    reason: &str,
) -> Result<i64, AppError> {
    let row = sqlx::query("SELECT profile_id,revision FROM books WHERE id=$1 FOR UPDATE")
        .bind(book_id)
        .fetch_optional(&mut **tx)
        .await?
        .ok_or(AppError::NotFound)?;
    let profile_id: Uuid = row.get("profile_id");
    let revision: i64 = row.get::<i64, _>("revision") + 1;
    sqlx::query(
        "UPDATE books SET title=$1,body=$2,tags=$3,metadata=$4,revision=$5,embedding_status='stale',updated_at=now() WHERE id=$6",
    )
    .bind(title)
    .bind(body)
    .bind(tags)
    .bind(metadata)
    .bind(revision)
    .bind(book_id)
    .execute(&mut **tx)
    .await?;
    sqlx::query("DELETE FROM book_chunks WHERE book_id=$1")
        .bind(book_id)
        .execute(&mut **tx)
        .await?;
    insert_chunks(tx, book_id, body).await?;
    sqlx::query(
        "INSERT INTO book_revisions (id,book_id,revision,title,body,tags,metadata,changed_by,change_reason) \
         VALUES ($1,$2,$3,$4,$5,$6,$7,$8,$9)",
    )
    .bind(Uuid::now_v7())
    .bind(book_id)
    .bind(revision)
    .bind(title)
    .bind(body)
    .bind(tags)
    .bind(metadata)
    .bind(changed_by)
    .bind(reason)
    .execute(&mut **tx)
    .await?;
    embedding::enqueue_book(tx, profile_id, book_id, revision).await?;
    Ok(revision)
}

async fn insert_revision(
    tx: &mut sqlx::Transaction<'_, sqlx::Postgres>,
    book: &Book,
    changed_by: Option<Uuid>,
    reason: &str,
) -> Result<(), AppError> {
    sqlx::query(
        "INSERT INTO book_revisions (id, book_id, revision, title, body, tags, metadata, changed_by, change_reason) \
         VALUES ($1,$2,$3,$4,$5,$6,$7,$8,$9)",
    )
    .bind(Uuid::now_v7())
    .bind(book.id)
    .bind(book.revision)
    .bind(&book.title)
    .bind(&book.body)
    .bind(&book.tags)
    .bind(&book.metadata)
    .bind(changed_by)
    .bind(reason)
    .execute(&mut **tx)
    .await?;
    Ok(())
}

fn is_admin(user: &AuthenticatedUser) -> bool {
    matches!(user.role.as_str(), "OWNER" | "ADMIN")
}

fn require_book_writer(user: &AuthenticatedUser) -> Result<(), AppError> {
    if user.role == "VIEWER" {
        Err(AppError::Forbidden)
    } else {
        Ok(())
    }
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

fn search_candidate(
    row: &sqlx::postgres::PgRow,
    mode: &str,
    lexical_score: Option<f32>,
    semantic_score: Option<f32>,
) -> SearchCandidate {
    SearchCandidate {
        summary: BookSummary {
            id: row.get("id"),
            title: row.get("title"),
            snippet: row.get("snippet"),
            book_type: row.get("book_type"),
            scope: row.get("scope"),
            tags: row.get("tags"),
            provenance: row.get("provenance"),
            trust: row.get("trust"),
            revision: row.get("revision"),
            relevance: 0.0,
            retrieval_mode: mode.into(),
            lexical_score,
            semantic_score,
            updated_at: row.get("updated_at"),
        },
        workspace_id: row.get("workspace_id"),
    }
}
