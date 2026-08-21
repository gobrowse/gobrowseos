//! Run-time tool implementations: library_search, library_add, library_load,
//! and the sandbox/terminal tools.
//!
//! These are native (non-MCP) tools executed in-process by the run loop.
//! They reuse [`Tool`] / [`ToolContext`] / [`ToolError`] from gobrowse-core.

use std::{
    sync::{Arc, Mutex},
    time::Duration,
};

use async_trait::async_trait;
use base64::{Engine, engine::general_purpose::STANDARD as BASE64};
use gobrowse_core::{
    model::ToolDefinition,
    sandbox::{
        HARD_RESOURCE_LIMITS, NetworkPolicy, TerminalStartRequest, TerminalState, validate_command,
        validate_workspace_path,
    },
    tools::{Tool, ToolContext, ToolDescriptor, ToolError},
};
use serde_json::Value;
use sqlx::Row;
use time::OffsetDateTime;
use uuid::Uuid;

use crate::{
    AppState, auth::audit, embedding, error::AppError, library_api, run_api::RunTokenMetrics,
    sandbox_client::SandboxClient,
};

// ---------------------------------------------------------------------------
// Tool definitions (exported for the run loop)
// ---------------------------------------------------------------------------

/// Returns the tool definitions as `ToolDefinition` (model wire format).
/// Sandbox/terminal tools are offered only when a sandbox client is
/// configured (`sandbox_enabled`).
pub fn tool_definitions(sandbox_enabled: bool) -> Vec<ToolDefinition> {
    let mut definitions = vec![
        ToolDefinition {
            id: "library_search".into(),
            description: "Search library books".into(),
            input_schema: serde_json::json!({
                "type": "object",
                "properties": {
                    "q": {
                        "type": "string"
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
                "required": ["q"]
            }),
        },
        ToolDefinition {
            id: "library_add".into(),
            description: "Add a NOTE book".into(),
            input_schema: serde_json::json!({
                "type": "object",
                "properties": {
                    "title": {
                        "type": "string"
                    },
                    "body": {
                        "type": "string"
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
                "required": ["title", "body"]
            }),
        },
        ToolDefinition {
            id: "library_load".into(),
            description: "Load book content".into(),
            input_schema: serde_json::json!({
                "type": "object",
                "properties": {
                    "book_id": {
                        "type": "string",
                        "format": "uuid"
                    },
                    "component": {
                        "type": ["string", "null"]
                    }
                },
                "required": ["book_id"]
            }),
        },
    ];
    if sandbox_enabled {
        definitions.extend(sandbox_tool_definitions());
    }
    definitions
}

/// Sandbox/terminal tool definitions, offered only when `features.sandbox`
/// is enabled AND a sandbox client is configured.
fn sandbox_tool_definitions() -> Vec<ToolDefinition> {
    let path = |description: &str| {
        serde_json::json!({
            "type": "object",
            "properties": {
                "path": {
                    "type": "string",
                    "description": description,
                    "maxLength": 4096
                }
            },
            "required": ["path"]
        })
    };
    vec![
        ToolDefinition {
            id: "sandbox_exec".into(),
            description: "Run a command".into(),
            input_schema: serde_json::json!({
                "type": "object",
                "properties": {
                    "command": {
                        "type": "array",
                        "items": {"type": "string", "maxLength": 4096},
                        "minItems": 1,
                        "maxItems": 128
                    },
                    "working_directory": {
                        "type": "string",
                        "description": "working directory",
                        "maxLength": 4096
                    },
                    "timeout_seconds": {
                        "type": "integer",
                        "minimum": 1,
                        "maximum": 10,
                        "default": 10
                    }
                },
                "required": ["command"]
            }),
        },
        ToolDefinition {
            id: "sandbox_read_file".into(),
            description: "Read a file".into(),
            input_schema: path("file path"),
        },
        ToolDefinition {
            id: "sandbox_write_file".into(),
            description: "Write a file".into(),
            input_schema: serde_json::json!({
                "type": "object",
                "properties": {
                    "path": {"type": "string", "maxLength": 4096},
                    "data_base64": {"type": "string", "maxLength": 349_528}
                },
                "required": ["path", "data_base64"]
            }),
        },
        ToolDefinition {
            id: "sandbox_list_files".into(),
            description: "List directory".into(),
            input_schema: path("directory path"),
        },
        ToolDefinition {
            id: "sandbox_stat".into(),
            description: "Stat a path".into(),
            input_schema: path("file or directory"),
        },
        ToolDefinition {
            id: "sandbox_mkdir".into(),
            description: "Make directory".into(),
            input_schema: path("directory path"),
        },
        ToolDefinition {
            id: "sandbox_remove".into(),
            description: "Remove a path".into(),
            input_schema: path("file or directory"),
        },
        ToolDefinition {
            id: "terminal_start".into(),
            description: "Start a PTY".into(),
            input_schema: serde_json::json!({
                "type": "object",
                "properties": {
                    "command": {
                        "type": "array",
                        "items": {"type": "string", "maxLength": 4096},
                        "minItems": 1,
                        "maxItems": 128
                    },
                    "working_directory": {"type": "string", "maxLength": 4096},
                    "cols": {"type": "integer", "minimum": 20, "maximum": 1000, "default": 80},
                    "rows": {"type": "integer", "minimum": 5, "maximum": 500, "default": 24}
                },
                "required": ["command"]
            }),
        },
        ToolDefinition {
            id: "terminal_input".into(),
            description: "Write to terminal".into(),
            input_schema: serde_json::json!({
                "type": "object",
                "properties": {
                    "terminal_id": {"type": "string", "format": "uuid"},
                    "data_base64": {"type": "string", "maxLength": 87_381}
                },
                "required": ["terminal_id", "data_base64"]
            }),
        },
        ToolDefinition {
            id: "terminal_read_output".into(),
            description: "Read terminal output".into(),
            input_schema: serde_json::json!({
                "type": "object",
                "properties": {
                    "terminal_id": {"type": "string", "format": "uuid"},
                    "after_cursor": {"type": "integer", "minimum": 0, "default": 0},
                    "max_bytes": {"type": "integer", "minimum": 1, "maximum": 262144, "default": 32768},
                    "wait_ms": {"type": "integer", "minimum": 0, "maximum": 30000, "default": 500}
                },
                "required": ["terminal_id"]
            }),
        },
        ToolDefinition {
            id: "terminal_resize".into(),
            description: "Resize terminal".into(),
            input_schema: serde_json::json!({
                "type": "object",
                "properties": {
                    "terminal_id": {"type": "string", "format": "uuid"},
                    "cols": {"type": "integer", "minimum": 20, "maximum": 1000},
                    "rows": {"type": "integer", "minimum": 5, "maximum": 500}
                },
                "required": ["terminal_id", "cols", "rows"]
            }),
        },
        ToolDefinition {
            id: "terminal_interrupt".into(),
            description: "Interrupt terminal".into(),
            input_schema: terminal_id_schema(),
        },
        ToolDefinition {
            id: "terminal_close".into(),
            description: "Close terminal".into(),
            input_schema: terminal_id_schema(),
        },
        ToolDefinition {
            id: "process_list".into(),
            description: "List processes".into(),
            input_schema: terminal_id_schema(),
        },
        ToolDefinition {
            id: "process_kill".into(),
            description: "Kill a process".into(),
            input_schema: serde_json::json!({
                "type": "object",
                "properties": {
                    "terminal_id": {"type": "string", "format": "uuid"},
                    "pid": {"type": "integer", "minimum": 1}
                },
                "required": ["terminal_id", "pid"]
            }),
        },
    ]
}

fn terminal_id_schema() -> serde_json::Value {
    serde_json::json!({
        "type": "object",
        "properties": {
            "terminal_id": {"type": "string", "format": "uuid"}
        },
        "required": ["terminal_id"],
        "additionalProperties": false
    })
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
    tool_descriptors()
        .into_iter()
        .next()
        .expect("search descriptor")
});

static LIBRARY_ADD_DESC: std::sync::LazyLock<ToolDescriptor> = std::sync::LazyLock::new(|| {
    tool_descriptors()
        .into_iter()
        .nth(1)
        .expect("add descriptor")
});

static LIBRARY_LOAD_DESC: std::sync::LazyLock<ToolDescriptor> =
    std::sync::LazyLock::new(|| ToolDescriptor {
        id: "library_load".into(),
        description:
            "Load a Library book's full content on demand (1 book per call, max 5 loads per run)"
                .into(),
        input_schema: serde_json::json!({
            "type": "object",
            "properties": {
                "book_id": {"type": "string", "format": "uuid"},
                "component": {"type": ["string", "null"], "maxLength": 200}
            },
            "required": ["book_id"],
            "additionalProperties": false
        }),
        output_schema: serde_json::json!({
            "type": "object",
            "properties": {
                "kind": {"type": "string"},
                "body": {"type": "string"},
                "tools": {"type": "array"},
                "components": {"type": "array"}
            }
        }),
        risk: gobrowse_core::policy::RiskClass::Read,
        permissions: vec!["library:read".into()],
        timeout_seconds: 10,
        source: "native".into(),
    });

/// Descriptor list for the sandbox/terminal tools, indexed by
/// [`SandboxToolKind`].
static SANDBOX_DESCRIPTORS: std::sync::LazyLock<Vec<ToolDescriptor>> =
    std::sync::LazyLock::new(|| {
        let defs = sandbox_tool_definitions();
        let high = gobrowse_core::policy::RiskClass::Execute;
        let terminal_permissions = vec!["sandbox:terminal".into()];
        let fs_permissions = vec!["sandbox:files".into()];
        let mut descriptors = Vec::with_capacity(defs.len());
        for definition in defs {
            let (risk, permissions, output_schema) = match definition.id.as_str() {
                "sandbox_exec" => (
                    high,
                    vec!["sandbox:exec".into()],
                    serde_json::json!({
                        "type": "object",
                        "properties": {
                            "terminal_id": {"type": "string"},
                            "exit_code": {"type": "integer"},
                            "state": {"type": "string"},
                            "output": {"type": "string"}
                        }
                    }),
                ),
                "sandbox_read_file" => (
                    gobrowse_core::policy::RiskClass::Read,
                    fs_permissions.clone(),
                    serde_json::json!({
                        "type": "object",
                        "properties": {
                            "data_base64": {"type": "string"},
                            "sha256": {"type": "string"}
                        }
                    }),
                ),
                "sandbox_write_file" | "sandbox_mkdir" | "sandbox_remove" => (
                    high,
                    fs_permissions.clone(),
                    serde_json::json!({
                        "type": "object",
                        "properties": {
                            "bytes": {"type": "integer"},
                            "sha256": {"type": "string"}
                        }
                    }),
                ),
                "sandbox_list_files" => (
                    gobrowse_core::policy::RiskClass::Read,
                    fs_permissions.clone(),
                    serde_json::json!({
                        "type": "object",
                        "properties": {
                            "entries": {"type": "array"}
                        }
                    }),
                ),
                "sandbox_stat" => (
                    gobrowse_core::policy::RiskClass::Read,
                    fs_permissions.clone(),
                    serde_json::json!({
                        "type": "object",
                        "properties": {
                            "kind": {"type": "string"},
                            "size": {"type": "integer"},
                            "mode": {"type": "integer"},
                            "modified_unix_seconds": {"type": "integer"}
                        }
                    }),
                ),
                "terminal_start" | "terminal_input" | "terminal_resize" | "terminal_interrupt"
                | "terminal_close" | "process_kill" => (
                    high,
                    terminal_permissions.clone(),
                    serde_json::json!({
                        "type": "object",
                        "properties": {
                            "terminal_id": {"type": "string"},
                            "ok": {"type": "boolean"}
                        }
                    }),
                ),
                "terminal_read_output" => (
                    gobrowse_core::policy::RiskClass::Read,
                    terminal_permissions.clone(),
                    serde_json::json!({
                        "type": "object",
                        "properties": {
                            "text": {"type": "string"},
                            "next_cursor": {"type": "integer"},
                            "state": {"type": "string"},
                            "output_complete": {"type": "boolean"}
                        }
                    }),
                ),
                "process_list" => (
                    gobrowse_core::policy::RiskClass::Read,
                    terminal_permissions.clone(),
                    serde_json::json!({
                        "type": "object",
                        "properties": {
                            "processes": {"type": "array"}
                        }
                    }),
                ),
                _ => (
                    high,
                    vec!["sandbox:exec".into()],
                    serde_json::json!({"type": "object"}),
                ),
            };
            descriptors.push(ToolDescriptor {
                id: definition.id,
                description: definition.description,
                input_schema: definition.input_schema,
                output_schema,
                risk,
                permissions,
                timeout_seconds: 10,
                source: "sandbox".into(),
            });
        }
        descriptors
    });

/// Maps a model-requested tool name to its [`SandboxToolKind`], or `None`
/// when the name is not a sandbox/terminal tool.
pub fn sandbox_tool_kind(name: &str) -> Option<SandboxToolKind> {
    match name {
        "sandbox_exec" => Some(SandboxToolKind::Exec),
        "sandbox_read_file" => Some(SandboxToolKind::ReadFile),
        "sandbox_write_file" => Some(SandboxToolKind::WriteFile),
        "sandbox_list_files" => Some(SandboxToolKind::ListFiles),
        "sandbox_stat" => Some(SandboxToolKind::Stat),
        "sandbox_mkdir" => Some(SandboxToolKind::Mkdir),
        "sandbox_remove" => Some(SandboxToolKind::Remove),
        "terminal_start" => Some(SandboxToolKind::TerminalStart),
        "terminal_input" => Some(SandboxToolKind::TerminalInput),
        "terminal_read_output" => Some(SandboxToolKind::TerminalReadOutput),
        "terminal_resize" => Some(SandboxToolKind::TerminalResize),
        "terminal_interrupt" => Some(SandboxToolKind::TerminalInterrupt),
        "terminal_close" => Some(SandboxToolKind::TerminalClose),
        "process_list" => Some(SandboxToolKind::ProcessList),
        "process_kill" => Some(SandboxToolKind::ProcessKill),
        _ => None,
    }
}

/// The sandbox/terminal operations, one per [`SandboxToolKind`] entry.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[repr(usize)]
pub enum SandboxToolKind {
    Exec,
    ReadFile,
    WriteFile,
    ListFiles,
    Stat,
    Mkdir,
    Remove,
    TerminalStart,
    TerminalInput,
    TerminalReadOutput,
    TerminalResize,
    TerminalInterrupt,
    TerminalClose,
    ProcessList,
    ProcessKill,
}

impl SandboxToolKind {
    /// Inverse of the descriptor-index mapping (test helper).
    pub fn from_index(index: usize) -> Self {
        match index {
            0 => Self::Exec,
            1 => Self::ReadFile,
            2 => Self::WriteFile,
            3 => Self::ListFiles,
            4 => Self::Stat,
            5 => Self::Mkdir,
            6 => Self::Remove,
            7 => Self::TerminalStart,
            8 => Self::TerminalInput,
            9 => Self::TerminalReadOutput,
            10 => Self::TerminalResize,
            11 => Self::TerminalInterrupt,
            12 => Self::TerminalClose,
            13 => Self::ProcessList,
            _ => Self::ProcessKill,
        }
    }
}

pub struct LibrarySearchTool {
    pub state: AppState,
}

#[async_trait]
impl Tool for LibrarySearchTool {
    fn descriptor(&self) -> &ToolDescriptor {
        &LIBRARY_SEARCH_DESC
    }

    async fn execute(&self, context: &ToolContext, input: Value) -> Result<Value, ToolError> {
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

    async fn execute(&self, context: &ToolContext, input: Value) -> Result<Value, ToolError> {
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

pub struct LibraryLoadTool {
    pub state: AppState,
    pub metrics: Arc<Mutex<RunTokenMetrics>>,
}

#[async_trait]
impl Tool for LibraryLoadTool {
    fn descriptor(&self) -> &ToolDescriptor {
        &LIBRARY_LOAD_DESC
    }

    async fn execute(&self, context: &ToolContext, input: Value) -> Result<Value, ToolError> {
        let book_id = input
            .get("book_id")
            .and_then(|value| value.as_str())
            .and_then(|value| Uuid::parse_str(value).ok())
            .ok_or(ToolError::InvalidInput)?;
        let component = input
            .get("component")
            .and_then(|value| value.as_str())
            .filter(|value| !value.is_empty());
        if component.is_some_and(|value| value.len() > 200) {
            return Err(ToolError::InvalidInput);
        }
        // Bounded: max 5 loads per run.
        {
            let metrics = self.metrics.lock().map_err(|_| ToolError::Execution)?;
            if metrics.books_loaded >= 5 {
                return Err(ToolError::Execution);
            }
        }
        let loaded = library_api::load_book_content(
            &self.state,
            context.profile_id,
            context.user_id,
            &context.role,
            book_id,
            component,
        )
        .await
        .map_err(|_| ToolError::Execution)?;
        let mut metrics = self.metrics.lock().map_err(|_| ToolError::Execution)?;
        metrics.books_loaded = metrics.books_loaded.saturating_add(1);
        metrics.book_tokens_loaded = metrics
            .book_tokens_loaded
            .saturating_add(u64::try_from(loaded.loaded_chars).unwrap_or(u64::MAX));
        let discovered = u32::try_from(loaded.tools_discovered).unwrap_or(u32::MAX);
        let loaded_schemas = u32::try_from(loaded.tools_loaded).unwrap_or(u32::MAX);
        match loaded.kind {
            Some(gobrowse_core::library::BookKind::Skill) => {
                metrics.skill_book_loads = metrics.skill_book_loads.saturating_add(1);
            }
            Some(gobrowse_core::library::BookKind::Plugin) => {
                metrics.plugin_book_loads = metrics.plugin_book_loads.saturating_add(1);
                metrics.plugin_tools_discovered =
                    metrics.plugin_tools_discovered.saturating_add(discovered);
                metrics.plugin_tools_loaded =
                    metrics.plugin_tools_loaded.saturating_add(loaded_schemas);
            }
            Some(gobrowse_core::library::BookKind::Mcp) => {
                metrics.mcp_book_loads = metrics.mcp_book_loads.saturating_add(1);
                metrics.mcp_tools_discovered =
                    metrics.mcp_tools_discovered.saturating_add(discovered);
                metrics.mcp_tools_loaded = metrics.mcp_tools_loaded.saturating_add(loaded_schemas);
            }
            Some(gobrowse_core::library::BookKind::Autobiography)
            | None
            | Some(gobrowse_core::library::BookKind::Source)
            | Some(gobrowse_core::library::BookKind::GobrowseUi) => {
                metrics.source_book_loads = metrics.source_book_loads.saturating_add(1);
            }
        }
        drop(metrics);
        Ok(loaded.payload)
    }
}

/// One sandbox/terminal tool; the operation is selected by [`SandboxToolKind`].
pub struct SandboxTool {
    pub state: AppState,
    pub kind: SandboxToolKind,
}

#[async_trait]
impl Tool for SandboxTool {
    fn descriptor(&self) -> &ToolDescriptor {
        &SANDBOX_DESCRIPTORS[self.kind as usize]
    }

    async fn execute(&self, context: &ToolContext, input: Value) -> Result<Value, ToolError> {
        let handle = self.state.sandbox.as_ref().ok_or(ToolError::Execution)?;
        let client = handle.get_or_connect().map_err(|_| ToolError::Execution)?;
        let workspace_id = context.workspace_id.ok_or(ToolError::Execution)?;
        execute_sandbox_op(
            &client,
            workspace_id,
            self.kind,
            &input,
            context.network_policy,
        )
        .await
    }
}

/// Shared execution for every sandbox/terminal tool. Every operation is
/// bounded to 10 s by the run loop's per-tool timeout.
async fn execute_sandbox_op(
    client: &SandboxClient,
    workspace_id: Uuid,
    kind: SandboxToolKind,
    input: &Value,
    network_policy: NetworkPolicy,
) -> Result<Value, ToolError> {
    // Provision once per workspace on first use (idempotent).
    client
        .provision_workspace(workspace_id)
        .await
        .map_err(|_| ToolError::Execution)?;
    match kind {
        SandboxToolKind::Exec => {
            let command = input
                .get("command")
                .and_then(|value| value.as_array())
                .map(|items| {
                    items
                        .iter()
                        .filter_map(|value| value.as_str().map(str::to_owned))
                        .collect::<Vec<_>>()
                })
                .ok_or(ToolError::InvalidInput)?;
            let working_directory = input
                .get("working_directory")
                .and_then(|value| value.as_str())
                .unwrap_or(".");
            let timeout_seconds = input
                .get("timeout_seconds")
                .and_then(|value| value.as_u64())
                .unwrap_or(10)
                .clamp(1, 10);
            sandbox_exec(
                client,
                workspace_id,
                command,
                working_directory,
                timeout_seconds,
                network_policy,
            )
            .await
        }
        SandboxToolKind::ReadFile => {
            let path = required_path(input)?;
            let (data, sha256) = client
                .fs_read(workspace_id, &path)
                .await
                .map_err(sandbox_error)?;
            // The run loop drops tool results over 64 KiB serialized; cap the
            // base64 payload (48 KiB raw) and flag truncation instead.
            const READ_CAP_BYTES: usize = 48_000;
            let truncated = data.len() > READ_CAP_BYTES;
            let bounded = &data[..data.len().min(READ_CAP_BYTES)];
            Ok(serde_json::json!({
                "data_base64": BASE64.encode(bounded),
                "sha256": sha256,
                "truncated": truncated,
                "total_bytes": data.len(),
            }))
        }
        SandboxToolKind::WriteFile => {
            let path = required_path(input)?;
            let data = input
                .get("data_base64")
                .and_then(|value| value.as_str())
                .ok_or(ToolError::InvalidInput)?;
            let bytes = BASE64.decode(data).map_err(|_| ToolError::InvalidInput)?;
            let (written, sha256) = client
                .fs_write(workspace_id, &path, &bytes)
                .await
                .map_err(sandbox_error)?;
            Ok(serde_json::json!({ "bytes": written, "sha256": sha256 }))
        }
        SandboxToolKind::ListFiles => {
            let path = required_path(input)?;
            let entries = client
                .fs_list(workspace_id, &path)
                .await
                .map_err(sandbox_error)?;
            Ok(serde_json::json!({
                "entries": entries.iter().map(|entry| serde_json::json!({
                    "name": entry.name,
                    "kind": format!("{:?}", entry.kind),
                    "size": entry.size,
                })).collect::<Vec<_>>()
            }))
        }
        SandboxToolKind::Stat => {
            let path = required_path(input)?;
            let metadata = client
                .fs_stat(workspace_id, &path)
                .await
                .map_err(sandbox_error)?;
            Ok(serde_json::json!({
                "kind": format!("{:?}", metadata.kind),
                "size": metadata.size,
                "mode": metadata.mode,
                "modified_unix_seconds": metadata.modified_unix_seconds,
            }))
        }
        SandboxToolKind::Mkdir => {
            let path = required_path(input)?;
            client
                .fs_mkdir(workspace_id, &path)
                .await
                .map_err(sandbox_error)?;
            Ok(serde_json::json!({ "ok": true }))
        }
        SandboxToolKind::Remove => {
            let path = required_path(input)?;
            client
                .fs_delete(workspace_id, &path)
                .await
                .map_err(sandbox_error)?;
            Ok(serde_json::json!({ "ok": true }))
        }
        SandboxToolKind::TerminalStart => {
            let command = input
                .get("command")
                .and_then(|value| value.as_array())
                .map(|items| {
                    items
                        .iter()
                        .filter_map(|value| value.as_str().map(str::to_owned))
                        .collect::<Vec<_>>()
                })
                .ok_or(ToolError::InvalidInput)?;
            let working_directory = input
                .get("working_directory")
                .and_then(|value| value.as_str())
                .unwrap_or(".");
            let cols = input
                .get("cols")
                .and_then(|value| value.as_u64())
                .unwrap_or(80);
            let rows = input
                .get("rows")
                .and_then(|value| value.as_u64())
                .unwrap_or(24);
            terminal_start(
                client,
                workspace_id,
                command,
                working_directory,
                cols,
                rows,
                network_policy,
            )
            .await
        }
        SandboxToolKind::TerminalInput => {
            let terminal_id = required_terminal(input)?;
            let data = input
                .get("data_base64")
                .and_then(|value| value.as_str())
                .ok_or(ToolError::InvalidInput)?;
            let bytes = BASE64.decode(data).map_err(|_| ToolError::InvalidInput)?;
            let accepted = client
                .terminal_input(terminal_id, Uuid::now_v7(), &bytes)
                .await
                .map_err(sandbox_error)?;
            Ok(serde_json::json!({ "bytes": accepted.bytes }))
        }
        SandboxToolKind::TerminalReadOutput => {
            let terminal_id = required_terminal(input)?;
            let after_cursor = input
                .get("after_cursor")
                .and_then(|value| value.as_u64())
                .unwrap_or(0);
            let max_bytes = input
                .get("max_bytes")
                .and_then(|value| value.as_u64())
                .unwrap_or(32 * 1024)
                .clamp(1, 256 * 1024) as u32;
            let wait_ms = input
                .get("wait_ms")
                .and_then(|value| value.as_u64())
                .unwrap_or(500)
                .clamp(0, 30_000) as u32;
            let output = client
                .terminal_read_output(terminal_id, after_cursor, max_bytes, wait_ms)
                .await
                .map_err(sandbox_error)?;
            let text = String::from_utf8_lossy(&output.data).into_owned();
            Ok(serde_json::json!({
                "text": text,
                "next_cursor": output.next_cursor,
                "state": format!("{:?}", output.state),
                "output_complete": output.output_complete,
            }))
        }
        SandboxToolKind::TerminalResize => {
            let terminal_id = required_terminal(input)?;
            let cols = input
                .get("cols")
                .and_then(|value| value.as_u64())
                .ok_or(ToolError::InvalidInput)?;
            let rows = input
                .get("rows")
                .and_then(|value| value.as_u64())
                .ok_or(ToolError::InvalidInput)?;
            client
                .terminal_resize(terminal_id, cols as u16, rows as u16)
                .await
                .map_err(sandbox_error)?;
            Ok(serde_json::json!({ "ok": true }))
        }
        SandboxToolKind::TerminalInterrupt => {
            let terminal_id = required_terminal(input)?;
            client
                .terminal_interrupt(terminal_id)
                .await
                .map_err(sandbox_error)?;
            Ok(serde_json::json!({ "ok": true }))
        }
        SandboxToolKind::TerminalClose => {
            let terminal_id = required_terminal(input)?;
            client
                .terminal_terminate(terminal_id)
                .await
                .map_err(sandbox_error)?;
            Ok(serde_json::json!({ "ok": true }))
        }
        SandboxToolKind::ProcessList => {
            let terminal_id = required_terminal(input)?;
            let processes = client
                .terminal_processes(terminal_id)
                .await
                .map_err(sandbox_error)?;
            Ok(serde_json::json!({
                "processes": processes.iter().map(|process| serde_json::json!({
                    "pid": process.pid,
                    "command": process.command,
                })).collect::<Vec<_>>()
            }))
        }
        SandboxToolKind::ProcessKill => {
            let terminal_id = required_terminal(input)?;
            let pid = input
                .get("pid")
                .and_then(|value| value.as_u64())
                .ok_or(ToolError::InvalidInput)?;
            client
                .terminal_kill(terminal_id, pid as u32)
                .await
                .map_err(sandbox_error)?;
            Ok(serde_json::json!({ "ok": true }))
        }
    }
}

fn required_path(input: &Value) -> Result<String, ToolError> {
    let path = input
        .get("path")
        .and_then(|value| value.as_str())
        .ok_or(ToolError::InvalidInput)?;
    validate_workspace_path(path).map_err(|_| ToolError::InvalidInput)?;
    Ok(path.to_owned())
}

fn required_terminal(input: &Value) -> Result<Uuid, ToolError> {
    input
        .get("terminal_id")
        .and_then(|value| value.as_str())
        .and_then(|value| Uuid::parse_str(value).ok())
        .ok_or(ToolError::InvalidInput)
}

async fn sandbox_exec(
    client: &SandboxClient,
    workspace_id: Uuid,
    command: Vec<String>,
    working_directory: &str,
    timeout_seconds: u64,
    network_policy: NetworkPolicy,
) -> Result<Value, ToolError> {
    validate_command(&command).map_err(|_| ToolError::InvalidInput)?;
    validate_workspace_path(working_directory).map_err(|_| ToolError::InvalidInput)?;
    let terminal_id = Uuid::now_v7();
    let request = TerminalStartRequest {
        workspace_id,
        command,
        working_directory: working_directory.to_owned(),
        cols: 80,
        rows: 24,
        network_policy,
        limits: HARD_RESOURCE_LIMITS,
    };
    let _started = client
        .terminal_start(terminal_id, request)
        .await
        .map_err(sandbox_error)?;
    // Drain output until the session completes or the deadline expires.
    let deadline = tokio::time::Instant::now() + Duration::from_secs(timeout_seconds);
    let mut output: Vec<u8> = Vec::new();
    let mut cursor = 0_u64;
    let mut state = TerminalState::Running;
    let mut output_complete = false;
    while tokio::time::Instant::now() < deadline && output.len() < 60 * 1024 {
        let remaining = deadline.saturating_duration_since(tokio::time::Instant::now());
        let wait_ms = u32::try_from(remaining.as_millis())
            .unwrap_or(u32::MAX)
            .min(500);
        let read = tokio::time::timeout(
            Duration::from_secs(10),
            client.terminal_read_output(terminal_id, cursor, 32 * 1024, wait_ms.max(1)),
        )
        .await
        .map_err(|_| ToolError::Timeout)?
        .map_err(sandbox_error)?;
        cursor = read.next_cursor;
        output.extend_from_slice(&read.data);
        state = read.state;
        output_complete = read.output_complete;
        if output_complete
            || matches!(
                state,
                TerminalState::Exited | TerminalState::Terminated | TerminalState::Unrecoverable
            )
        {
            break;
        }
    }
    let inspect = client.terminal_inspect(terminal_id).await.ok();
    let exit_code = inspect.as_ref().and_then(|info| info.exit_code);
    let state = inspect.as_ref().map(|info| info.state).unwrap_or(state);
    let _ = client.terminal_terminate(terminal_id).await;
    let text = String::from_utf8_lossy(&output[..output.len().min(60 * 1024)]).into_owned();
    Ok(serde_json::json!({
        "terminal_id": terminal_id,
        "exit_code": exit_code,
        "state": format!("{state:?}"),
        "output_complete": output_complete,
        "output": text,
    }))
}

async fn terminal_start(
    client: &SandboxClient,
    workspace_id: Uuid,
    command: Vec<String>,
    working_directory: &str,
    cols: u64,
    rows: u64,
    network_policy: NetworkPolicy,
) -> Result<Value, ToolError> {
    validate_command(&command).map_err(|_| ToolError::InvalidInput)?;
    validate_workspace_path(working_directory).map_err(|_| ToolError::InvalidInput)?;
    if !(20..=1_000).contains(&cols) || !(5..=500).contains(&rows) {
        return Err(ToolError::InvalidInput);
    }
    let terminal_id = Uuid::now_v7();
    let request = TerminalStartRequest {
        workspace_id,
        command,
        working_directory: working_directory.to_owned(),
        cols: cols as u16,
        rows: rows as u16,
        network_policy,
        limits: HARD_RESOURCE_LIMITS,
    };
    let started = client
        .terminal_start(terminal_id, request)
        .await
        .map_err(sandbox_error)?;
    Ok(serde_json::json!({
        "terminal_id": started.terminal_id,
        "network_policy": format!("{:?}", started.network_policy),
    }))
}

fn sandbox_error(error: crate::sandbox_client::SandboxClientError) -> ToolError {
    let _ = error;
    ToolError::Execution
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
           AND (scope <> 'CONVERSATION') \
           AND (scope NOT IN ('USER','PRIVATE') OR owner_user_id=$3) \
           AND (scope NOT IN ('WORKSPACE','PROJECT') OR EXISTS( \
               SELECT 1 FROM workspace_memberships member WHERE member.workspace_id=books.workspace_id AND member.user_id=$3)) \
           AND ($4::uuid IS NULL OR scope IN ('GLOBAL','PROFILE','USER','PRIVATE','AGENT') OR workspace_id=$4) \
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
            kind: None, // library_add notes are SOURCE (SQL NULL)
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
        let defs = tool_definitions(false);
        assert_eq!(defs.len(), 3);
        assert_eq!(defs[0].id, "library_search");
        assert_eq!(defs[1].id, "library_add");
        assert_eq!(defs[2].id, "library_load");
    }

    #[test]
    fn sandbox_tools_are_conditional_and_high_risk() {
        assert_eq!(tool_definitions(false).len(), 3);
        let defs = tool_definitions(true);
        assert!(defs.len() > 3);
        assert!(
            defs.iter()
                .any(|definition| definition.id == "sandbox_exec")
        );
        assert!(
            defs.iter()
                .any(|definition| definition.id == "terminal_start")
        );
        // Sandbox tools are only offered when enabled.
        assert!(
            !tool_definitions(false)
                .iter()
                .any(|definition| definition.id == "sandbox_exec")
        );
        for descriptor in SANDBOX_DESCRIPTORS.iter() {
            assert_eq!(descriptor.source, "sandbox");
            assert_eq!(descriptor.timeout_seconds, 10);
        }
        // Every sandbox/terminal tool name maps to a kind (and vice versa).
        for (index, descriptor) in SANDBOX_DESCRIPTORS.iter().enumerate() {
            assert_eq!(
                sandbox_tool_kind(&descriptor.id),
                Some(SandboxToolKind::from_index(index))
            );
        }
    }

    #[test]
    fn library_search_schema_drops_redundant_bounds() {
        let schema = &tool_definitions(false)[0].input_schema;
        let props = schema["properties"].as_object().unwrap();
        // Redundant length bounds are now enforced server-side; the schema no
        // longer carries them.
        assert!(props["q"].get("minLength").is_none());
        assert!(props["q"].get("maxLength").is_none());
        // Numeric bounds are out of scope for the reduction and are retained.
        assert_eq!(props["limit"]["minimum"], 1);
        assert_eq!(props["limit"]["maximum"], 20);
    }

    #[test]
    fn library_add_schema_drops_redundant_bounds() {
        let schema = &tool_definitions(false)[1].input_schema;
        // additionalProperties is no longer emitted and length bounds are
        // enforced server-side.
        assert!(schema.get("additionalProperties").is_none());
        let props = schema["properties"].as_object().unwrap();
        assert!(props["title"].get("maxLength").is_none());
        assert!(props["body"].get("maxLength").is_none());
    }

    #[test]
    fn library_load_schema_requires_book_id() {
        let schema = &tool_definitions(false)[2].input_schema;
        assert!(schema.get("additionalProperties").is_none());
        let props = schema["properties"].as_object().unwrap();
        assert_eq!(props["book_id"]["format"], "uuid");
        // component length bound is now enforced server-side.
        assert!(props["component"].get("maxLength").is_none());
        assert!(
            schema["required"]
                .as_array()
                .unwrap()
                .contains(&"book_id".into())
        );
    }

    #[test]
    fn sandbox_path_tools_validate_relative_paths() {
        assert!(validate_workspace_path("src/main.rs").is_ok());
        assert!(validate_workspace_path("../escape").is_err());
        assert!(validate_workspace_path("/abs").is_err());
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
