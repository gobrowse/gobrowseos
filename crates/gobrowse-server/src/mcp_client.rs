//! MCP client for the server: spawns stdio MCP servers, performs the protocol
//! handshake (modern `server/discover` or legacy `initialize`), and exposes
//! bounded `tools/list` + `tools/call` over [`McpStdioTransport`].
//!
//! Secrets: the `mcp_servers.auth_secret_reference` is resolved from the
//! credential vault and injected into the child's environment under the env
//! var named by `configuration.auth_env_var` (default `MCP_AUTH_TOKEN`).
//! OAuth-required servers (rows in `mcp_auth_states` or an explicit
//! `configuration.requires_oauth` flag) return the typed
//! [`McpClientError::AuthRequired`] instead of failing hard.
//!
//! Bounds: [`MAX_MCP_TOOLS`] tools and [`MAX_MCP_TOOLS_SCHEMA_BYTES`] total
//! schema bytes per `tools/list`; [`MAX_MCP_CALL_RESULT_BYTES`] per
//! `tools/call`; [`MCP_TIMEOUT`] on every handshake and request.
//!
//! Plugin-embedded MCP (stdio through a sandbox terminal session, journey H)
//! is deliberately deferred: this client implements the plain stdio path only.

use std::{collections::HashMap, sync::Arc, time::Duration};

use gobrowse_core::mcp::{
    McpProtocolEra,
    model::{DiscoverResult, InitializeResult, Paginated, Tool, ToolCallParams, ToolCallResult},
    select_protocol_version,
    stdio::{McpStdioError, McpStdioTransport},
    wire::{
        METHOD_DISCOVER, METHOD_INITIALIZE, METHOD_INITIALIZED, METHOD_TOOLS_CALL,
        METHOD_TOOLS_LIST, RequestId, ResponseBody, ValidatedMessage, ValidatedResponse, WireError,
        try_notification, try_request,
    },
};
use secrecy::ExposeSecret;
use serde::Deserialize;
use serde_json::{Value, json};
use sqlx::Row;
use std::process::Stdio;
use thiserror::Error;
use tokio::{
    io::{AsyncRead, AsyncWrite},
    process::{Child, ChildStdin, ChildStdout, Command},
    sync::Mutex,
};
use uuid::Uuid;

use crate::{AppState, error::AppError, vault::Vault};

/// Every handshake step and request is bounded by this timeout.
pub const MCP_TIMEOUT: Duration = Duration::from_secs(30);
/// Maximum tools returned by `tools/list` (Stage 3/4 progressive loading).
pub const MAX_MCP_TOOLS: usize = 16;
/// Maximum total serialized tool schema size from one `tools/list`.
pub const MAX_MCP_TOOLS_SCHEMA_BYTES: usize = 64 * 1024;
/// Maximum serialized `tools/call` result size.
pub const MAX_MCP_CALL_RESULT_BYTES: usize = 64 * 1024;
/// How many non-response frames (notifications/unsolicited requests) the
/// client tolerates before giving up on a response.
const MAX_INTERLEAVED_FRAMES: usize = 16;
const LEGACY_PROTOCOL_VERSION: &str = "2025-11-25";

/// Runtime configuration needed to spawn one stdio MCP server.
#[derive(Debug, Clone)]
pub struct McpServerConfig {
    pub command: String,
    pub args: Vec<String>,
    /// Environment injected into the child (vault secrets already resolved).
    pub env: HashMap<String, String>,
}

#[derive(Debug, Error)]
pub enum McpClientError {
    #[error("MCP server is not configured for this profile")]
    NotFound,
    #[error("MCP server is disabled")]
    Disabled,
    #[error("MCP transport '{0}' is not supported yet (only stdio)")]
    UnsupportedTransport(String),
    #[error("MCP server requires OAuth authorization")]
    AuthRequired,
    #[error("MCP server authentication secret is missing or unreadable")]
    SecretUnavailable,
    #[error("failed to spawn MCP server process: {0}")]
    SpawnFailed(String),
    #[error("MCP transport error: {0}")]
    Transport(#[from] McpStdioError),
    #[error("MCP wire error: {0}")]
    Wire(#[from] WireError),
    #[error("MCP handshake failed: {0}")]
    Handshake(String),
    #[error("MCP request timed out")]
    Timeout,
    #[error(
        "MCP server exposed more than {MAX_MCP_TOOLS} tools ({count}); refusing to load them all"
    )]
    ToolsLimitExceeded { count: usize },
    #[error("MCP tool schema payload exceeds the 64 KiB bound")]
    SchemaTooLarge,
    #[error("MCP server returned an invalid response")]
    InvalidResponse,
    #[error("MCP protocol violation: {0}")]
    ProtocolViolation(String),
    #[error("MCP server error {code}: {message}")]
    Rpc { code: i64, message: String },
    #[error("database error while loading MCP server configuration")]
    Database(#[from] sqlx::Error),
}

/// A connection to one spawned stdio MCP server. Generic over the pipe types
/// so tests can drive an in-process dispatcher over `tokio::io::duplex`
/// pipes; production uses [`McpClient::spawn`] with a child process.
pub struct McpClient<R, W> {
    child: Option<Child>,
    transport: McpStdioTransport<R, W>,
    era: McpProtocolEra,
    next_id: i64,
}

/// The concrete client type the pool stores (child-process pipes).
pub type ProcessMcpClient = McpClient<ChildStdout, ChildStdin>;

impl McpClient<ChildStdout, ChildStdin> {
    /// Spawns the configured stdio MCP server process. The handshake is NOT
    /// performed here; call [`Self::initialize`] before issuing requests.
    pub fn spawn(config: &McpServerConfig) -> Result<Self, McpClientError> {
        let mut command = Command::new(&config.command);
        command
            .args(&config.args)
            .envs(&config.env)
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .kill_on_drop(true);
        let mut child = command
            .spawn()
            .map_err(|error| McpClientError::SpawnFailed(error.to_string()))?;
        let stdin = child
            .stdin
            .take()
            .ok_or_else(|| McpClientError::SpawnFailed("stdin was not piped".into()))?;
        let stdout = child
            .stdout
            .take()
            .ok_or_else(|| McpClientError::SpawnFailed("stdout was not piped".into()))?;
        let mut stderr = child
            .stderr
            .take()
            .ok_or_else(|| McpClientError::SpawnFailed("stderr was not piped".into()))?;
        // Drain stderr so a chatty server never blocks on a full pipe. The
        // tail is discarded; it is intentionally never surfaced to model
        // context or API responses.
        tokio::spawn(async move {
            use tokio::io::AsyncReadExt;
            let mut buffer = [0_u8; 1024];
            while stderr.read(&mut buffer).await.is_ok_and(|n| n > 0) {}
        });
        Ok(Self {
            child: Some(child),
            transport: McpStdioTransport::new(stdout, stdin),
            era: McpProtocolEra::Modern20260728,
            next_id: 0,
        })
    }
}

impl<R, W> McpClient<R, W>
where
    R: AsyncRead + Unpin,
    W: AsyncWrite + Unpin,
{
    /// Wraps caller-provided pipes (test helper).
    pub fn with_transport(transport: McpStdioTransport<R, W>) -> Self {
        Self {
            child: None,
            transport,
            era: McpProtocolEra::Modern20260728,
            next_id: 0,
        }
    }

    /// Performs the protocol handshake within [`MCP_TIMEOUT`].
    pub async fn initialize(&mut self) -> Result<(), McpClientError> {
        tokio::time::timeout(MCP_TIMEOUT, self.initialize_inner())
            .await
            .map_err(|_| McpClientError::Timeout)?
    }

    async fn initialize_inner(&mut self) -> Result<(), McpClientError> {
        // Modern path first: server/discover. Any error (a legacy-only server
        // replies method-not-found or a negotiation error) falls back to the
        // legacy initialize handshake.
        let id = self.next_request_id();
        self.transport
            .send_request(try_request(id.clone(), METHOD_DISCOVER, None)?)
            .await?;
        let response = self.await_response(&id).await?;
        let correlated = response
            .correlate(METHOD_DISCOVER)
            .map_err(|_| McpClientError::InvalidResponse)?;
        match correlated.body() {
            ResponseBody::Result { result } => {
                let discovered: DiscoverResult = serde_json::from_value(result.clone())
                    .map_err(|_| McpClientError::InvalidResponse)?;
                let era =
                    select_protocol_version(&discovered.supported_versions).map_err(|_| {
                        McpClientError::Handshake(
                            "no mutually supported MCP protocol version".into(),
                        )
                    })?;
                if era.is_modern() {
                    self.era = era;
                    return Ok(());
                }
                self.era = era;
                self.legacy_initialize().await
            }
            // Any discover error (method-not-found on legacy-only servers,
            // negotiation errors when no modern era is mutually supported)
            // falls back to the legacy initialize handshake.
            ResponseBody::Error { .. } => self.legacy_initialize().await,
        }
    }

    async fn legacy_initialize(&mut self) -> Result<(), McpClientError> {
        let id = self.next_request_id();
        let params = json!({
            "protocolVersion": LEGACY_PROTOCOL_VERSION,
            "capabilities": {},
            "clientInfo": {
                "name": "gobrowse",
                "version": env!("CARGO_PKG_VERSION"),
            },
        });
        self.transport
            .send_request(try_request(id.clone(), METHOD_INITIALIZE, Some(params))?)
            .await?;
        let response = self.await_response(&id).await?;
        let correlated = response
            .correlate(METHOD_INITIALIZE)
            .map_err(|_| McpClientError::InvalidResponse)?;
        match correlated.body() {
            ResponseBody::Result { result } => {
                let initialized: InitializeResult = serde_json::from_value(result.clone())
                    .map_err(|_| McpClientError::InvalidResponse)?;
                if initialized.protocol_version != LEGACY_PROTOCOL_VERSION
                    || McpProtocolEra::from_wire_version(&initialized.protocol_version)
                        != Some(McpProtocolEra::Legacy20251125)
                {
                    return Err(McpClientError::Handshake(
                        "server negotiated an unsupported protocol version".into(),
                    ));
                }
                let notification = try_notification(METHOD_INITIALIZED, None)?;
                self.transport.send_notification(notification).await?;
                self.era = McpProtocolEra::Legacy20251125;
                Ok(())
            }
            ResponseBody::Error { error } => Err(McpClientError::Rpc {
                code: error.code,
                message: error.message.clone(),
            }),
        }
    }

    /// `tools/list` bounded to [`MAX_MCP_TOOLS`] tools and
    /// [`MAX_MCP_TOOLS_SCHEMA_BYTES`] total serialized schema bytes.
    pub async fn tools_list(&mut self) -> Result<Vec<Tool>, McpClientError> {
        tokio::time::timeout(MCP_TIMEOUT, self.tools_list_inner())
            .await
            .map_err(|_| McpClientError::Timeout)?
    }

    async fn tools_list_inner(&mut self) -> Result<Vec<Tool>, McpClientError> {
        let id = self.next_request_id();
        self.transport
            .send_request(try_request(id.clone(), METHOD_TOOLS_LIST, None)?)
            .await?;
        let response = self.await_response(&id).await?;
        let correlated = response
            .correlate(METHOD_TOOLS_LIST)
            .map_err(|_| McpClientError::InvalidResponse)?;
        match correlated.body() {
            ResponseBody::Result { result } => {
                let page: Paginated<Tool> = serde_json::from_value(result.clone())
                    .map_err(|_| McpClientError::InvalidResponse)?;
                if page.next_cursor.is_some() || page.items.len() > MAX_MCP_TOOLS {
                    return Err(McpClientError::ToolsLimitExceeded {
                        count: page.items.len(),
                    });
                }
                let total_bytes: usize = page
                    .items
                    .iter()
                    .map(|tool| {
                        serde_json::to_string(tool)
                            .map(|serialized| serialized.len())
                            .unwrap_or(usize::MAX)
                    })
                    .sum();
                if total_bytes > MAX_MCP_TOOLS_SCHEMA_BYTES {
                    return Err(McpClientError::SchemaTooLarge);
                }
                Ok(page.items)
            }
            ResponseBody::Error { error } => Err(McpClientError::Rpc {
                code: error.code,
                message: error.message.clone(),
            }),
        }
    }

    /// `tools/call` with the response bounded to
    /// [`MAX_MCP_CALL_RESULT_BYTES`].
    pub async fn tools_call(
        &mut self,
        name: &str,
        arguments: Value,
    ) -> Result<ToolCallResult, McpClientError> {
        tokio::time::timeout(MCP_TIMEOUT, self.tools_call_inner(name, arguments))
            .await
            .map_err(|_| McpClientError::Timeout)?
    }

    async fn tools_call_inner(
        &mut self,
        name: &str,
        arguments: Value,
    ) -> Result<ToolCallResult, McpClientError> {
        let id = self.next_request_id();
        let params = ToolCallParams {
            name: name.to_owned(),
            arguments,
        };
        let params_value =
            serde_json::to_value(&params).map_err(|_| McpClientError::InvalidResponse)?;
        self.transport
            .send_request(try_request(
                id.clone(),
                METHOD_TOOLS_CALL,
                Some(params_value),
            )?)
            .await?;
        let response = self.await_response(&id).await?;
        let correlated = response
            .correlate(METHOD_TOOLS_CALL)
            .map_err(|_| McpClientError::InvalidResponse)?;
        match correlated.body() {
            ResponseBody::Result { result } => {
                let call: ToolCallResult = serde_json::from_value(result.clone())
                    .map_err(|_| McpClientError::InvalidResponse)?;
                let encoded =
                    serde_json::to_vec(&call).map_err(|_| McpClientError::InvalidResponse)?;
                if encoded.len() > MAX_MCP_CALL_RESULT_BYTES {
                    return Err(McpClientError::SchemaTooLarge);
                }
                Ok(call)
            }
            ResponseBody::Error { error } => Err(McpClientError::Rpc {
                code: error.code,
                message: error.message.clone(),
            }),
        }
    }

    /// Negotiated protocol era (diagnostics).
    pub fn era(&self) -> McpProtocolEra {
        self.era
    }

    /// The spawned child process, if any (dropped/killed with the client).
    pub fn child(&self) -> Option<&Child> {
        self.child.as_ref()
    }

    /// Terminates the child process (if spawned) and waits for it to exit.
    pub async fn shutdown(&mut self) {
        if let Some(child) = self.child.as_mut() {
            let _ = child.kill().await;
            let _ = child.wait().await;
        }
        self.child = None;
    }

    fn next_request_id(&mut self) -> RequestId {
        self.next_id += 1;
        RequestId::Number(self.next_id)
    }

    /// Receives frames until a response matching `expected` arrives.
    /// Server-initiated notifications and unsolicited requests are skipped
    /// (bounded by [`MAX_INTERLEAVED_FRAMES`]).
    async fn await_response(
        &mut self,
        expected: &RequestId,
    ) -> Result<ValidatedResponse, McpClientError> {
        for _ in 0..MAX_INTERLEAVED_FRAMES {
            match self.transport.recv().await? {
                ValidatedMessage::Response(response) if response.id() == expected => {
                    return Ok(response);
                }
                ValidatedMessage::Response(_) => {
                    return Err(McpClientError::ProtocolViolation(
                        "server replied with an unexpected request id".into(),
                    ));
                }
                ValidatedMessage::Notification(_) | ValidatedMessage::Request(_) => {
                    // Notifications are fine to skip; unsolicited requests are
                    // dropped (the wire layer forbids responding from here).
                    continue;
                }
            }
        }
        Err(McpClientError::ProtocolViolation(
            "too many interleaved frames before the response".into(),
        ))
    }
}

/// Parsed stdio configuration from `mcp_servers.configuration`.
#[derive(Debug, Clone, Deserialize)]
struct StdioConfiguration {
    command: String,
    #[serde(default)]
    args: Vec<String>,
    #[serde(default)]
    env: HashMap<String, String>,
    /// Environment variable that receives the vault secret when
    /// `auth_secret_reference` is set on the server row.
    #[serde(default = "default_auth_env_var")]
    auth_env_var: String,
    /// Explicit opt-in for OAuth-required servers without pending auth state.
    #[serde(default)]
    requires_oauth: bool,
}

fn default_auth_env_var() -> String {
    "MCP_AUTH_TOKEN".into()
}

/// Per-server connection pool. Concurrent `library_load` calls for the same
/// server share one [`ProcessMcpClient`]; connections are created lazily on
/// first use.
#[derive(Clone, Default)]
pub struct McpClientPool {
    clients: Arc<Mutex<HashMap<Uuid, Arc<Mutex<ProcessMcpClient>>>>>,
}

impl McpClientPool {
    pub fn new() -> Self {
        Self::default()
    }

    /// Drops a pooled connection (used when a server is deleted or disabled).
    pub fn remove(&self, server_id: Uuid) {
        if let Ok(mut clients) = self.clients.try_lock() {
            clients.remove(&server_id);
        }
    }

    /// Returns the pooled client for `server_id`, connecting on first use.
    /// The caller must hold the returned mutex for the duration of each
    /// request (one in-flight request per server at a time).
    pub async fn get_or_connect(
        &self,
        state: &AppState,
        profile_id: Uuid,
        server_id: Uuid,
    ) -> Result<Arc<Mutex<ProcessMcpClient>>, McpClientError> {
        if let Some(client) = self.clients.lock().await.get(&server_id) {
            return Ok(client.clone());
        }
        let row = sqlx::query(
            "SELECT name, transport, configuration, enabled, auth_secret_reference \
             FROM mcp_servers WHERE id=$1 AND profile_id=$2",
        )
        .bind(server_id)
        .bind(profile_id)
        .fetch_optional(&state.pool)
        .await?
        .ok_or_else(|| {
            // The row may have been deleted while a connection was pooled:
            // drop the stale entry so it cannot shadow the NotFound.
            if let Ok(mut clients) = self.clients.try_lock() {
                clients.remove(&server_id);
            }
            McpClientError::NotFound
        })?;
        let enabled: bool = row.get("enabled");
        if !enabled {
            return Err(McpClientError::Disabled);
        }
        let transport: String = row.get("transport");
        if transport != "stdio" {
            return Err(McpClientError::UnsupportedTransport(transport));
        }
        let configuration: Value = row.get("configuration");
        let auth_secret_reference: Option<String> = row.get("auth_secret_reference");
        let parsed: StdioConfiguration = serde_json::from_value(configuration).map_err(|_| {
            McpClientError::Handshake(
                "mcp_servers.configuration is not a valid stdio configuration \
                 (requires a 'command' string)"
                    .into(),
            )
        })?;
        if parsed.requires_oauth {
            return Err(McpClientError::AuthRequired);
        }
        let has_pending_oauth: bool = sqlx::query_scalar(
            "SELECT EXISTS(SELECT 1 FROM mcp_auth_states WHERE mcp_server_id=$1)",
        )
        .bind(server_id)
        .fetch_one(&state.pool)
        .await?;
        if has_pending_oauth {
            return Err(McpClientError::AuthRequired);
        }
        let mut env = parsed.env;
        if let Some(secret_id) = auth_secret_reference {
            let secret =
                resolve_mcp_secret(&state.vault, &state.pool, profile_id, &secret_id).await?;
            env.insert(parsed.auth_env_var.clone(), secret);
        }
        let config = McpServerConfig {
            command: parsed.command,
            args: parsed.args,
            env,
        };
        let mut client = ProcessMcpClient::spawn(&config)?;
        client.initialize().await?;
        // Re-check under the lock in case a concurrent caller connected first.
        let mut clients = self.clients.lock().await;
        if let Some(existing) = clients.get(&server_id) {
            let _ = client.shutdown().await;
            return Ok(existing.clone());
        }
        let client = Arc::new(Mutex::new(client));
        clients.insert(server_id, client.clone());
        Ok(client)
    }
}

async fn resolve_mcp_secret(
    vault: &Vault,
    pool: &sqlx::PgPool,
    profile_id: Uuid,
    secret_id: &str,
) -> Result<String, McpClientError> {
    match vault.resolve(pool, profile_id, secret_id).await {
        Ok(secret) => Ok(secret.expose_secret().to_owned()),
        Err(AppError::NotFound) | Err(_) => Err(McpClientError::SecretUnavailable),
    }
}

#[cfg(test)]
mod tests {
    use async_trait::async_trait;
    use gobrowse_core::mcp::{
        capabilities::{ServerCapabilities, ToolsCapability},
        model::{
            ClientInfo, Content, InitializeParams, InitializeResult, ListParams, Paginated, Tool,
        },
        server::{McpServerDispatcher, McpServerHandler},
        wire::{self, RpcError, ValidatedMessage, encode},
    };
    use tokio::io::{AsyncReadExt, AsyncWriteExt, DuplexStream, duplex};

    use super::*;

    struct FullHandler;
    #[async_trait]
    impl McpServerHandler for FullHandler {
        async fn discover(&self) -> Result<DiscoverResult, RpcError> {
            Ok(DiscoverResult {
                supported_versions: vec!["2026-07-28".into(), "2025-11-25".into()],
                capabilities: ServerCapabilities {
                    tools: Some(ToolsCapability::default()),
                    ..Default::default()
                },
                server_info: None,
            })
        }
        async fn initialize(&self, params: InitializeParams) -> Result<InitializeResult, RpcError> {
            Ok(InitializeResult {
                protocol_version: params.protocol_version,
                capabilities: ServerCapabilities {
                    tools: Some(ToolsCapability::default()),
                    ..Default::default()
                },
                server_info: ClientInfo {
                    name: "fixture".into(),
                    version: "1".into(),
                },
                instructions: None,
            })
        }
        async fn tools_list(&self, _: ListParams) -> Result<Paginated<Tool>, RpcError> {
            Ok(Paginated {
                items: vec![Tool {
                    name: "echo".into(),
                    title: None,
                    description: Some("Echo text back".into()),
                    input_schema: json!({"type": "object"}),
                    output_schema: None,
                }],
                next_cursor: None,
            })
        }
        async fn tools_call(&self, params: ToolCallParams) -> Result<ToolCallResult, RpcError> {
            Ok(ToolCallResult {
                content: vec![Content::Text {
                    text: format!("echo:{}", params.arguments["text"]),
                }],
                is_error: false,
                structured_content: None,
            })
        }
    }

    /// Legacy-only server: `server/discover` advertises only the legacy
    /// version (dispatcher negotiation rejects it), forcing the client onto
    /// the initialize handshake.
    struct LegacyOnlyHandler;
    #[async_trait]
    impl McpServerHandler for LegacyOnlyHandler {
        async fn discover(&self) -> Result<DiscoverResult, RpcError> {
            Ok(DiscoverResult {
                supported_versions: vec!["2025-11-25".into()],
                capabilities: ServerCapabilities::default(),
                server_info: None,
            })
        }
        async fn initialize(&self, params: InitializeParams) -> Result<InitializeResult, RpcError> {
            Ok(InitializeResult {
                protocol_version: params.protocol_version,
                capabilities: ServerCapabilities {
                    tools: Some(ToolsCapability::default()),
                    ..Default::default()
                },
                server_info: ClientInfo {
                    name: "fixture".into(),
                    version: "1".into(),
                },
                instructions: None,
            })
        }
        async fn tools_list(&self, _: ListParams) -> Result<Paginated<Tool>, RpcError> {
            Ok(Paginated {
                items: vec![Tool {
                    name: "legacy-tool".into(),
                    title: None,
                    description: None,
                    input_schema: json!({"type": "object"}),
                    output_schema: None,
                }],
                next_cursor: None,
            })
        }
    }

    /// Runs the dispatcher loop on the server end of a duplex pair. The
    /// stdio transport only exposes request/notification send, so responses
    /// are encoded directly and written to the raw writer stream.
    async fn run_dispatcher<H: McpServerHandler + 'static>(
        mut read: DuplexStream,
        mut write: DuplexStream,
        handler: H,
    ) {
        let mut dispatcher = McpServerDispatcher::new(handler);
        let mut frame = Vec::new();
        let mut byte = [0_u8; 1];
        loop {
            frame.clear();
            loop {
                if read.read(&mut byte).await.ok() != Some(1) {
                    return;
                }
                if byte[0] == b'\n' {
                    break;
                }
                if frame.len() < 1024 * 1024 {
                    frame.push(byte[0]);
                }
            }
            match wire::decode(&frame) {
                Ok(ValidatedMessage::Request(request)) => {
                    let response = dispatcher.dispatch(request).await;
                    if let Ok(payload) = encode(&ValidatedMessage::Response(response))
                        && (write.write_all(&payload).await.is_err()
                            || write.write_all(b"\n").await.is_err()
                            || write.flush().await.is_err())
                    {
                        return;
                    }
                }
                Ok(ValidatedMessage::Notification(notification)) => {
                    if dispatcher.notify(notification).await.is_err() {
                        return;
                    }
                }
                _ => return,
            }
        }
    }

    /// Builds (client_transport, server_read, server_write) over two duplex
    /// pairs so the client and the in-process dispatcher never share a handle.
    fn test_pair() -> (
        McpStdioTransport<DuplexStream, DuplexStream>,
        DuplexStream,
        DuplexStream,
    ) {
        let (client_read, server_write) = duplex(16 * 1024);
        let (server_read, client_write) = duplex(16 * 1024);
        (
            McpStdioTransport::new(client_read, client_write),
            server_read,
            server_write,
        )
    }

    #[tokio::test]
    async fn modern_handshake_lists_tools_and_calls() {
        let (client_side, server_read, server_write) = test_pair();
        let server = tokio::spawn(run_dispatcher(server_read, server_write, FullHandler));
        let mut client = McpClient::with_transport(client_side);
        client.initialize().await.unwrap();
        assert_eq!(client.era(), McpProtocolEra::Modern20260728);
        let tools = client.tools_list().await.unwrap();
        assert_eq!(tools.len(), 1);
        assert_eq!(tools[0].name, "echo");
        let result = client
            .tools_call("echo", json!({"text": "hello"}))
            .await
            .unwrap();
        match &result.content[0] {
            Content::Text { text } => assert_eq!(text, r#"echo:"hello""#),
            _ => panic!("expected text content"),
        }
        server.abort();
    }

    #[tokio::test]
    async fn legacy_initialize_fallback_works() {
        let (client_side, server_read, server_write) = test_pair();
        let server = tokio::spawn(run_dispatcher(server_read, server_write, LegacyOnlyHandler));
        let mut client = McpClient::with_transport(client_side);
        client.initialize().await.unwrap();
        assert_eq!(client.era(), McpProtocolEra::Legacy20251125);
        let tools = client.tools_list().await.unwrap();
        assert_eq!(tools[0].name, "legacy-tool");
        server.abort();
    }

    #[tokio::test]
    async fn oversized_tool_list_is_rejected() {
        struct ManyTools;
        #[async_trait]
        impl McpServerHandler for ManyTools {
            async fn discover(&self) -> Result<DiscoverResult, RpcError> {
                Ok(DiscoverResult {
                    supported_versions: vec!["2026-07-28".into()],
                    capabilities: ServerCapabilities {
                        tools: Some(ToolsCapability::default()),
                        ..Default::default()
                    },
                    server_info: None,
                })
            }
            async fn tools_list(&self, _: ListParams) -> Result<Paginated<Tool>, RpcError> {
                Ok(Paginated {
                    items: (0..17)
                        .map(|i| Tool {
                            name: format!("tool-{i}"),
                            title: None,
                            description: None,
                            input_schema: json!({"type": "object"}),
                            output_schema: None,
                        })
                        .collect(),
                    next_cursor: None,
                })
            }
        }
        let (client_side, server_read, server_write) = test_pair();
        let server = tokio::spawn(run_dispatcher(server_read, server_write, ManyTools));
        let mut client = McpClient::with_transport(client_side);
        client.initialize().await.unwrap();
        assert!(matches!(
            client.tools_list().await,
            Err(McpClientError::ToolsLimitExceeded { count: 17 })
        ));
        server.abort();
    }

    #[test]
    fn stdio_configuration_defaults() {
        let config: StdioConfiguration =
            serde_json::from_value(json!({"command": "/bin/true"})).unwrap();
        assert_eq!(config.command, "/bin/true");
        assert!(config.args.is_empty());
        assert_eq!(config.auth_env_var, "MCP_AUTH_TOKEN");
        assert!(!config.requires_oauth);
    }
}
