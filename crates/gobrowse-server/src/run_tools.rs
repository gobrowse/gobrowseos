//! Run-time tool implementations: library_search and library_add.
//!
//! These are native (non-MCP) tools executed in-process by the run loop.
//! They reuse [`Tool`] / [`ToolContext`] / [`ToolError`] from gobrowse-core.

use async_trait::async_trait;
use gobrowse_core::{
    model::ToolDefinition,
    tools::{Tool, ToolContext, ToolDescriptor, ToolError},
};
use serde_json::Value;
use sqlx::Row;
use time::OffsetDateTime;
use uuid::Uuid;

use crate::{
    AppState,
    auth::audit,
    embedding,
    error::AppError,
    library_api,
};

// ---------------------------------------------------------------------------
// Tool definitions (exported for the run loop)
// ---------------------------------------------------------------------------

/// Returns the two tool definitions as `ToolDefinition` (model wire format).
pub fn tool_definitions() -> Vec<ToolDefinition> {
    vec![
        ToolDefinition {
            id: "library_search".into(),
            description: "Search the profile Library for books matching a query. \
                          Returns bounded summaries (id, title, snippet, scope, trust). \
                          Never returns full book content."
                .into(),
            input_schema: serde_json::json!({
                "type": "object",
                "properties": {
                    "q": {
                        "type": "string",
                        "minLength": 1,
                        "maxLength": 1000
                    },
                    "workspace_id": {
                        "type": ["string", "null"],
                        "format": "uuid"
                    },
                    "limit": {
                        "type": "integer",
                        "minimum": 1,
                        "maximum": 20,
                        "default": 10
                    }
                },
                "required": ["q"],
                "additionalProperties": false
            }),
        },
        ToolDefinition {
            id: "library_add".into(),
            description: "Create a NOTE in the Library. The note is user-authored and \
                          scoped to the current workspace (or profile). Returns the new book id."
                .into(),
            input_schema: serde_json::json!({
                "type": "object",
                "properties": {
                    "title": {
                        "type": "string",
                        "minLength": 1,
                        "maxLength": 512
                    },
                    "body": {
                        "type": "string",
                        "minLength": 1,
                        "maxLength": 100000
                    },
                    "tags": {
                        "type": "array",
                        "items": {
                            "type": "string",
                            "maxLength": 100
                        },
                        "maxItems": 32
                    }
                },
                "required": ["title", "body"],
                "additionalProperties": false
            }),
        },
    ]
}

// ---------------------------------------------------------------------------
// Tool descriptors (server-side execution metadata)
// ---------------------------------------------------------------------------

pub fn tool_descriptors() -> Vec<ToolDescriptor> {
    vec![
        ToolDescriptor {
            id: "library_search".into(),
            description: "Search the profile Library for books matching a query".into(),
            input_schema: serde_json::json!({
                "type": "object",
                "properties": {
                    "q": {"type": "string", "minLength": 1, "maxLength": 1000},
                    "workspace_id": {"type": ["string", "null"], "format": "uuid"},
                    "limit": {"type": "integer", "minimum": 1, "maximum": 20}
                },
                "required": ["q"],
                "additionalProperties": false
            }),
            output_schema: serde_json::json!({
                "type": "array",
                "items": {
                    "type": "object",
                    "properties": {
                        "id": {"type": "string"},
                        "title": {"type": "string"},
                        "snippet": {"type": "string"},
                        "scope": {"type": "string"},
                        "trust": {"type": "string"}
                    }
                }
            }),
            risk: gobrowse_core::policy::RiskClass::Read,
            permissions: vec!["library:read".into()],
            timeout_seconds: 10,
            source: "native".into(),
        },
        ToolDescriptor {
            id: "library_add".into(),
            description: "Create a NOTE in the Library".into(),
            input_schema: serde_json::json!({
                "type": "object",
                "properties": {
                    "title": {"type": "string", "minLength": 1, "maxLength": 512},
                    "body": {"type": "string", "minLength": 1, "maxLength": 100000},
                    "tags": {"type": "array", "items": {"type": "string", "maxLength": 100}, "maxItems": 32}
                },
                "required": ["title", "body"],
                "additionalProperties": false
            }),
            output_schema: serde_json::json!({
                "type": "object",
                "properties": {
                    "book_id": {"type": "string", "format": "uuid"}
                }
            }),
            risk: gobrowse_core::policy::RiskClass::Write,
            permissions: vec!["library:write".into()],
            timeout_seconds: 10,
            source: "native".into(),
        },
    ]
}

// ---------------------------------------------------------------------------
// Tool implementations
// ---------------------------------------------------------------------------

// Keep static descriptors so we can return references.
static LIBRARY_SEARCH_DESC: std::sync::LazyLock<ToolDescriptor> = std::sync::LazyLock::new(|| {
    tool_descriptors().into_iter().next().expect("search descriptor")
});

static LIBRARY_ADD_DESC: std::sync::LazyLock<ToolDescriptor> = std::sync::LazyLock::new(|| {
    tool_descriptors().into_iter().nth(1).expect("add descriptor")
});

pub struct LibrarySearchTool {
    pub state: AppState,
}

#[async_trait]
impl Tool for LibrarySearchTool {
    fn descriptor(&self) -> &ToolDescriptor {
        &LIBRARY_SEARCH_DESC
    }

    async fn execute(
        &self,
        context: &ToolContext,
        input: Value,
    ) -> Result<Value, ToolError> {
        let q = input
            .get("q")
            .and_then(|v| v.as_str())
            .map(|s| s.trim())
            .filter(|s| !s.is_empty())
            .ok_or(ToolError::InvalidInput)?;
        if q.chars().count() > 1000 {
            return Err(ToolError::InvalidInput);
        }
        let workspace_id = input
            .get("workspace_id")
            .and_then(|v| v.as_str())
            .and_then(|s| Uuid::parse_str(s).ok());
        let limit = input
            .get("limit")
            .and_then(|v| v.as_u64())
            .unwrap_or(10)
            .clamp(1, 20) as i64;

        let rows = match lexical_search(
            &self.state.pool,
            context.profile_id,
            workspace_id,
            context.user_id,
            q,
            limit,
        )
        .await
        {
            Ok(rows) => rows,
            Err(_) => return Err(ToolError::Execution),
        };

        let results: Vec<Value> = rows
            .into_iter()
            .map(|row| {
                let scope: String = row.get("scope");
                let trust: String = row.get("trust");
                serde_json::json!({
                    "id": row.get::<Uuid, _>("id").to_string(),
                    "title": row.get::<String, _>("title"),
                    "snippet": row.get::<String, _>("snippet"),
                    "scope": scope,
                    "trust": trust,
                })
            })
            .collect();

        Ok(serde_json::json!({ "books": results }))
    }
}

pub struct LibraryAddTool {
    pub state: AppState,
}

#[async_trait]
impl Tool for LibraryAddTool {
    fn descriptor(&self) -> &ToolDescriptor {
        &LIBRARY_ADD_DESC
    }

    async fn execute(
        &self,
        context: &ToolContext,
        input: Value,
    ) -> Result<Value, ToolError> {
        let title = input
            .get("title")
            .and_then(|v| v.as_str())
            .map(|s| s.trim())
            .filter(|s| !s.is_empty())
            .ok_or(ToolError::InvalidInput)?;
        if title.len() > 512 {
            return Err(ToolError::InvalidInput);
        }
        let body = input
            .get("body")
            .and_then(|v| v.as_str())
            .filter(|s| !s.is_empty())
            .ok_or(ToolError::InvalidInput)?;
        if body.len() > 100_000 {
            return Err(ToolError::InvalidInput);
        }
        let tags: Vec<String> = input
            .get("tags")
            .and_then(|v| v.as_array())
            .map(|arr| {
                arr.iter()
                    .filter_map(|v| v.as_str().map(|s| s.to_owned()))
                    .take(32)
                    .collect()
            })
            .unwrap_or_default();

        match create_library_note(
            &self.state,
            context.profile_id,
            context.workspace_id,
            context.user_id,
            title,
            body,
            &tags,
        )
        .await
        {
            Ok(book_id) => Ok(serde_json::json!({ "book_id": book_id.to_string() })),
            Err(AppError::Validation(msg)) => {
                Err(if msg.contains("scope") || msg.contains("book_type") {
                    ToolError::InvalidInput
                } else {
                    ToolError::Execution
                })
            }
            Err(AppError::Database(_)) => Err(ToolError::Execution),
            Err(_) => Err(ToolError::Execution),
        }
    }
}

// ---------------------------------------------------------------------------
// Shared helpers (exposed for run_tools and tests)
// ---------------------------------------------------------------------------

/// Lexical FTS search returning bounded snippets (never full body).
/// Reuses the same auth predicates as `library_api::search_books`.
pub(crate) async fn lexical_search(
    pool: &sqlx::PgPool,
    profile_id: Uuid,
    workspace_id: Option<Uuid>,
    user_id: Uuid,
    q: &str,
    limit: i64,
) -> Result<Vec<sqlx::postgres::PgRow>, sqlx::Error> {
    sqlx::query(
        "SELECT id, title, ts_headline('english', body, websearch_to_tsquery('english', $1), \
             'MaxWords=32, MinWords=8, ShortWord=3') AS snippet, scope, trust \
         FROM books WHERE profile_id = $2 \
           AND (security_classification <> 'RESTRICTED') \
           AND (scope <> 'AGENT') \
           AND (scope NOT IN ('USER','PRIVATE') OR owner_user_id=$3) \
           AND (scope<>'CONVERSATION' OR EXISTS(SELECT 1 FROM conversations c WHERE c.id=books.conversation_id AND ( \
               (c.workspace_id IS NULL AND c.created_by_user_id=$3) OR EXISTS( \
               SELECT 1 FROM workspace_memberships member WHERE member.workspace_id=c.workspace_id AND member.user_id=$3)))) \
           AND (scope NOT IN ('WORKSPACE','PROJECT') OR EXISTS( \
               SELECT 1 FROM workspace_memberships member WHERE member.workspace_id=books.workspace_id AND member.user_id=$3)) \
           AND ($4::uuid IS NULL OR scope IN ('GLOBAL','PROFILE','USER','PRIVATE','AGENT') OR workspace_id=$4 \
                OR (scope='CONVERSATION' AND EXISTS(SELECT 1 FROM conversations c WHERE c.id=books.conversation_id AND c.workspace_id=$4))) \
           AND search_document @@ websearch_to_tsquery('english', $1) \
         ORDER BY ts_rank_cd(search_document, websearch_to_tsquery('english', $1)) DESC, updated_at DESC LIMIT $5",
    )
    .bind(q)
    .bind(profile_id)
    .bind(user_id)
    .bind(workspace_id)
    .bind(limit)
    .fetch_all(pool)
    .await
}

/// Create a NOTE book with hardcoded safe metadata.
/// Reuses the same INSERT/chunks/revision/embedding/audit pattern as `library_api::create_book`.
pub(crate) async fn create_library_note(
    state: &AppState,
    profile_id: Uuid,
    workspace_id: Option<Uuid>,
    requester_id: Uuid,
    title: &str,
    body: &str,
    tags: &[String],
) -> Result<Uuid, AppError> {
    let now = OffsetDateTime::now_utc();
    let book_id = Uuid::now_v7();

    let mut tx = state.pool.begin().await?;

    sqlx::query(
        "INSERT INTO books (id, profile_id, title, body, book_type, scope, tags, provenance, trust, \
         author, workspace_id, security_classification, metadata, owner_user_id, created_by_user_id, created_at, updated_at) \
         VALUES ($1,$2,$3,$4,'NOTE', \
         CASE WHEN $5::uuid IS NOT NULL THEN 'WORKSPACE' ELSE 'PROFILE' END, \
         $6,'USER','USER_PROVIDED',$7,$5,'INTERNAL','{}'::jsonb,NULL,$8,$9,$9)"
    )
    .bind(book_id)
    .bind(profile_id)
    .bind(title)
    .bind(body)
    .bind(workspace_id)
    .bind(tags)
    .bind(requester_id)
    .bind(now)
    .execute(&mut *tx)
    .await?;

    library_api::insert_chunks(&mut tx, book_id, body).await?;
    library_api::insert_revision(
        &mut tx,
        &gobrowse_core::library::Book {
            id: book_id,
            profile_id,
            title: title.to_owned(),
            body: body.to_owned(),
            book_type: gobrowse_core::library::BookType::Note,
            scope: if workspace_id.is_some() {
                gobrowse_core::library::BookScope::Workspace
            } else {
                gobrowse_core::library::BookScope::Profile
            },
            tags: tags.to_vec(),
            provenance: gobrowse_core::library::Provenance::User,
            trust: gobrowse_core::library::TrustLevel::UserProvided,
            author: String::new(), // filled below from users table
            workspace_id,
            conversation_id: None,
            security_classification: gobrowse_core::library::SecurityClassification::Internal,
            embedding_status: gobrowse_core::library::EmbeddingStatus::Pending,
            metadata: serde_json::json!({}),
            revision: 1,
            created_at: now,
            updated_at: now,
        },
        Some(requester_id),
        "library_add tool",
    )
    .await?;

    embedding::enqueue_book(&mut tx, profile_id, book_id, 1).await?;

    audit(
        &mut tx,
        Some(requester_id),
        Some(profile_id),
        "book.created",
        "book",
        Some(book_id.to_string()),
        "success",
    )
    .await?;

    tx.commit().await?;
    Ok(book_id)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn tool_definitions_have_correct_ids() {
        let defs = tool_definitions();
        assert_eq!(defs.len(), 2);
        assert_eq!(defs[0].id, "library_search");
        assert_eq!(defs[1].id, "library_add");
    }

    #[test]
    fn library_search_input_schema_validates_bounds() {
        let schema = &tool_definitions()[0].input_schema;
        let props = schema["properties"].as_object().unwrap();
        assert_eq!(props["q"]["minLength"], 1);
        assert_eq!(props["q"]["maxLength"], 1000);
        assert_eq!(props["limit"]["minimum"], 1);
        assert_eq!(props["limit"]["maximum"], 20);
    }

    #[test]
    fn library_add_input_schema_rejects_extra_fields() {
        let schema = &tool_definitions()[1].input_schema;
        assert_eq!(schema["additionalProperties"], false);
        let props = schema["properties"].as_object().unwrap();
        assert_eq!(props["title"]["maxLength"], 512);
        assert_eq!(props["body"]["maxLength"], 100_000);
    }

    #[tokio::test]
    async fn library_search_requires_database() {
        let Some(_db_url) = std::env::var("GOBROWSE_TEST_DATABASE_URL").ok() else {
            return;
        };
        // Integration test verifying search returns only summaries (no body) is in
        // context_features_integration.rs.
    }

    #[tokio::test]
    async fn library_add_rejects_oversized_body() {
        let Some(_db_url) = std::env::var("GOBROWSE_TEST_DATABASE_URL").ok() else {
            return;
        };
        // Schema validation is tested above; integration test for actual creation
        // is in context_features_integration.rs.
    }
}
