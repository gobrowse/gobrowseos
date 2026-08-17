//! MCP server CRUD API.
//!
//! Provides profile-scoped list, create, update, and delete operations for
//! MCP servers.  No process spawning or MCP transport connection — metadata
//! only.

use axum::{
    Json,
    extract::{Path, State},
    http::{HeaderMap, StatusCode},
};
use serde::{Deserialize, Serialize};
use serde_json::Value;
use sqlx::Row;
use time::OffsetDateTime;
use uuid::Uuid;

use crate::{
    AppState,
    auth::{audit, require_user},
    error::AppError,
};

// ---------------------------------------------------------------------------
// Response types
// ---------------------------------------------------------------------------

#[derive(Debug, Serialize)]
pub struct McpServerResponse {
    pub id: Uuid,
    pub name: String,
    pub transport: String,
    pub configuration: Value,
    pub enabled: bool,
    pub auth_secret_reference: Option<String>,
    pub created_at: OffsetDateTime,
}

// ---------------------------------------------------------------------------
// Request types
// ---------------------------------------------------------------------------

#[derive(Debug, Deserialize)]
pub struct CreateMcpServerRequest {
    pub name: String,
    pub transport: String,
    pub configuration: Value,
    #[serde(default = "default_enabled")]
    pub enabled: bool,
}

#[derive(Debug, Deserialize)]
pub struct UpdateMcpServerRequest {
    pub name: Option<String>,
    pub configuration: Option<Value>,
    pub enabled: Option<bool>,
    pub auth_secret_reference: Option<Option<String>>,
}

const fn default_enabled() -> bool {
    true
}

const MAX_NAME_CHARS: usize = 200;

// ---------------------------------------------------------------------------
// Helpers
// ---------------------------------------------------------------------------

fn validate_create(input: &CreateMcpServerRequest) -> Result<(), AppError> {
    let name = input.name.trim();
    if name.is_empty() {
        return Err(AppError::Validation("name must not be empty".into()));
    }
    if name.len() > MAX_NAME_CHARS {
        return Err(AppError::Validation(format!(
            "name must not exceed {MAX_NAME_CHARS} characters"
        )));
    }
    match input.transport.as_str() {
        "stdio" | "streamable_http" => {}
        other => {
            return Err(AppError::Validation(format!(
                "invalid transport '{other}': must be 'stdio' or 'streamable_http'"
            )));
        }
    }
    if !input.configuration.is_object() {
        return Err(AppError::Validation(
            "configuration must be a JSON object".into(),
        ));
    }
    Ok(())
}

fn validate_update(input: &UpdateMcpServerRequest) -> Result<(), AppError> {
    if let Some(ref name) = input.name {
        let name = name.trim();
        if name.is_empty() {
            return Err(AppError::Validation("name must not be empty".into()));
        }
        if name.len() > MAX_NAME_CHARS {
            return Err(AppError::Validation(format!(
                "name must not exceed {MAX_NAME_CHARS} characters"
            )));
        }
    }
    Ok(())
}

fn row_to_response(row: &sqlx::postgres::PgRow) -> Result<McpServerResponse, AppError> {
    Ok(McpServerResponse {
        id: row.get("id"),
        name: row.get("name"),
        transport: row.get("transport"),
        configuration: row.get("configuration"),
        enabled: row.get("enabled"),
        auth_secret_reference: row.get("auth_secret_reference"),
        created_at: row.get("created_at"),
    })
}

// ---------------------------------------------------------------------------
// Handlers
// ---------------------------------------------------------------------------

/// `GET /api/v1/mcp/servers` — list user's MCP servers.
pub async fn list(
    State(state): State<AppState>,
    headers: HeaderMap,
) -> Result<Json<Vec<McpServerResponse>>, AppError> {
    let user = require_user(&state, &headers).await?;
    let rows = sqlx::query(
        "SELECT id, name, transport, configuration, enabled, \
                auth_secret_reference, created_at \
         FROM mcp_servers \
         WHERE profile_id = $1 \
         ORDER BY created_at DESC, id DESC \
         LIMIT 200",
    )
    .bind(user.profile_id)
    .fetch_all(&state.pool)
    .await?;
    rows.iter()
        .map(row_to_response)
        .collect::<Result<Vec<_>, _>>()
        .map(Json)
}

/// `POST /api/v1/mcp/servers` — create a new MCP server.
pub async fn create(
    State(state): State<AppState>,
    headers: HeaderMap,
    Json(input): Json<CreateMcpServerRequest>,
) -> Result<(StatusCode, Json<McpServerResponse>), AppError> {
    let user = require_user(&state, &headers).await?;
    validate_create(&input)?;

    let name = input.name.trim();

    let mut tx = state.pool.begin().await?;
    let id = Uuid::now_v7();
    let now = OffsetDateTime::now_utc();

    sqlx::query(
        "INSERT INTO mcp_servers \
         (id, profile_id, name, transport, configuration, enabled, created_at, updated_at) \
         VALUES ($1, $2, $3, $4, $5, $6, $7, $7)",
    )
    .bind(id)
    .bind(user.profile_id)
    .bind(name)
    .bind(&input.transport)
    .bind(&input.configuration)
    .bind(input.enabled)
    .bind(now)
    .execute(&mut *tx)
    .await?;

    audit(
        &mut tx,
        Some(user.id),
        Some(user.profile_id),
        "mcp_server.created",
        "mcp_server",
        Some(id.to_string()),
        "success",
    )
    .await?;

    tx.commit().await?;

    Ok((
        StatusCode::CREATED,
        Json(McpServerResponse {
            id,
            name: name.to_owned(),
            transport: input.transport,
            configuration: input.configuration,
            enabled: input.enabled,
            auth_secret_reference: None,
            created_at: now,
        }),
    ))
}

/// `PATCH /api/v1/mcp/servers/{id}` — update an existing MCP server.
pub async fn update(
    State(state): State<AppState>,
    headers: HeaderMap,
    Path(server_id): Path<Uuid>,
    Json(input): Json<UpdateMcpServerRequest>,
) -> Result<Json<McpServerResponse>, AppError> {
    let user = require_user(&state, &headers).await?;
    validate_update(&input)?;

    let mut tx = state.pool.begin().await?;

    // Fetch existing row within the same profile for ownership check.
    let row = sqlx::query(
        "SELECT id, profile_id, name, transport, configuration, enabled, \
                auth_secret_reference, created_at, updated_at \
         FROM mcp_servers \
         WHERE id = $1",
    )
    .bind(server_id)
    .fetch_optional(&mut *tx)
    .await?
    .ok_or(AppError::NotFound)?;

    if row.get::<Uuid, _>("profile_id") != user.profile_id {
        return Err(AppError::NotFound);
    }

    // Build the UPDATE SET clause dynamically — only set provided fields.
    let name = input
        .name
        .as_deref()
        .map(str::trim)
        .map(ToOwned::to_owned)
        .unwrap_or_else(|| row.get("name"));
    let configuration = input.configuration.unwrap_or_else(|| row.get("configuration"));
    let enabled = input.enabled.unwrap_or_else(|| row.get("enabled"));
    let auth_secret_reference: Option<String> = match input.auth_secret_reference {
        Some(val) => val,
        None => row.get("auth_secret_reference"),
    };
    let updated_at = OffsetDateTime::now_utc();

    sqlx::query(
        "UPDATE mcp_servers \
         SET name = $1, configuration = $2, enabled = $3, \
             auth_secret_reference = $4, updated_at = $5 \
         WHERE id = $6",
    )
    .bind(&name)
    .bind(&configuration)
    .bind(enabled)
    .bind(&auth_secret_reference)
    .bind(updated_at)
    .bind(server_id)
    .execute(&mut *tx)
    .await?;

    audit(
        &mut tx,
        Some(user.id),
        Some(user.profile_id),
        "mcp_server.updated",
        "mcp_server",
        Some(server_id.to_string()),
        "success",
    )
    .await?;

    tx.commit().await?;

    Ok(Json(McpServerResponse {
        id: server_id,
        name,
        transport: row.get("transport"),
        configuration,
        enabled,
        auth_secret_reference,
        created_at: row.get("created_at"),
    }))
}

/// `DELETE /api/v1/mcp/servers/{id}` — delete an MCP server.
pub async fn delete(
    State(state): State<AppState>,
    headers: HeaderMap,
    Path(server_id): Path<Uuid>,
) -> Result<StatusCode, AppError> {
    let user = require_user(&state, &headers).await?;

    let mut tx = state.pool.begin().await?;

    let result = sqlx::query(
        "DELETE FROM mcp_servers \
         WHERE id = $1 AND profile_id = $2",
    )
    .bind(server_id)
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
        "mcp_server.deleted",
        "mcp_server",
        Some(server_id.to_string()),
        "success",
    )
    .await?;

    tx.commit().await?;

    Ok(StatusCode::NO_CONTENT)
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn create_validation_rejects_empty_name() {
        let input = CreateMcpServerRequest {
            name: "   ".into(),
            transport: "stdio".into(),
            configuration: json!({"command": "echo"}),
            enabled: true,
        };
        let err = validate_create(&input).unwrap_err();
        assert!(
            err.to_string().contains("name must not be empty"),
            "got: {err}"
        );
    }

    #[test]
    fn create_validation_rejects_long_name() {
        let input = CreateMcpServerRequest {
            name: "a".repeat(201),
            transport: "stdio".into(),
            configuration: json!({}),
            enabled: true,
        };
        let err = validate_create(&input).unwrap_err();
        assert!(
            err.to_string().contains("200"),
            "got: {err}"
        );
    }

    #[test]
    fn create_validation_rejects_bad_transport() {
        let input = CreateMcpServerRequest {
            name: "valid".into(),
            transport: "tcp".into(),
            configuration: json!({}),
            enabled: true,
        };
        let err = validate_create(&input).unwrap_err();
        assert!(
            err.to_string().contains("invalid transport"),
            "got: {err}"
        );
    }

    #[test]
    fn create_validation_rejects_non_object_config() {
        let input = CreateMcpServerRequest {
            name: "valid".into(),
            transport: "streamable_http".into(),
            configuration: json!("not-an-object"),
            enabled: true,
        };
        let err = validate_create(&input).unwrap_err();
        assert!(
            err.to_string().contains("configuration must be a JSON object"),
            "got: {err}"
        );
    }

    #[test]
    fn create_validation_accepts_valid_input() {
        let input = CreateMcpServerRequest {
            name: "My Server".into(),
            transport: "stdio".into(),
            configuration: json!({"command": "npx", "args": ["-y", "@modelcontextprotocol/server-filesystem", "/tmp"]}),
            enabled: true,
        };
        validate_create(&input).unwrap();
    }

    #[test]
    fn create_validation_accepts_streamable_http() {
        let input = CreateMcpServerRequest {
            name: "Remote".into(),
            transport: "streamable_http".into(),
            configuration: json!({"url": "https://mcp.example.com"}),
            enabled: false,
        };
        validate_create(&input).unwrap();
    }

    #[test]
    fn update_validation_rejects_empty_name() {
        let input = UpdateMcpServerRequest {
            name: Some("   ".into()),
            configuration: None,
            enabled: None,
            auth_secret_reference: None,
        };
        let err = validate_update(&input).unwrap_err();
        assert!(
            err.to_string().contains("name must not be empty"),
            "got: {err}"
        );
    }

    #[test]
    fn update_validation_rejects_long_name() {
        let input = UpdateMcpServerRequest {
            name: Some("a".repeat(201)),
            configuration: None,
            enabled: None,
            auth_secret_reference: None,
        };
        let err = validate_update(&input).unwrap_err();
        assert!(
            err.to_string().contains("200"),
            "got: {err}"
        );
    }

    #[test]
    fn update_validation_accepts_no_name_change() {
        let input = UpdateMcpServerRequest {
            name: None,
            configuration: Some(json!({"url": "https://new.example.com"})),
            enabled: Some(false),
            auth_secret_reference: None,
        };
        validate_update(&input).unwrap();
    }

    #[test]
    fn update_validation_accepts_valid_name() {
        let input = UpdateMcpServerRequest {
            name: Some("Updated Name".into()),
            configuration: None,
            enabled: None,
            auth_secret_reference: None,
        };
        validate_update(&input).unwrap();
    }
}