//! Typed client for the gobrowse-sandboxd Unix-socket protocol.
//!
//! The client speaks the exact `gobrowse_core::sandbox` protocol: one
//! [`RequestEnvelope`] serialized as a single JSON line (≤ 512 KiB), one
//! [`ResponseEnvelope`] JSON line back, with the daemon's `request_id` echo
//! verified. No translation layer exists between [`SandboxOperation`] and the
//! wire.
//!
//! Connection policy: sandboxd accepts exactly one client per connection, so
//! the client opens a **fresh connection per `send`** and closes it after the
//! response. There is deliberately no connection pool, buffering, or
//! reconnection layer yet; callers decide retry semantics (agent tools get one
//! attempt per sandbox call). Every socket read/write is bounded by the
//! configured [`SandboxConfig::timeout`] so a hung daemon never blocks a
//! request forever.

use std::{path::PathBuf, time::Duration};

use base64::{Engine, engine::general_purpose::STANDARD as BASE64};
use gobrowse_core::sandbox::{
    FilesystemEntry, MAX_FILE_PAYLOAD_BYTES, MAX_PROTOCOL_LINE_BYTES, MAX_TERMINAL_INPUT_BYTES,
    MAX_TERMINAL_OUTPUT_READ_BYTES, NetworkPolicy, RequestEnvelope, ResourceLimits,
    ResponseEnvelope, SANDBOX_PROTOCOL_VERSION, SandboxErrorCode, SandboxOperation, SandboxProcess,
    SandboxResult, TerminalStartRequest, TerminalState,
};
use secrecy::{ExposeSecret, SecretString};
use thiserror::Error;
use tokio::{
    io::{AsyncBufReadExt, AsyncReadExt, AsyncWriteExt, BufReader},
    net::UnixStream,
};
use uuid::Uuid;

/// Configuration required to talk to one sandboxd instance.
#[derive(Debug, Clone)]
pub struct SandboxConfig {
    pub socket_path: PathBuf,
    pub auth_token: SecretString,
    pub timeout: Duration,
}

/// Client-side error taxonomy for sandbox operations. Mirrors
/// [`SandboxErrorCode`] with protocol-shape failures collapsed into
/// [`SandboxClientError::ProtocolViolation`] and transport failures into
/// [`SandboxClientError::Disconnected`].
#[derive(Debug, Error)]
pub enum SandboxClientError {
    #[error("sandbox daemon is not reachable")]
    Disconnected,
    #[error("sandbox authentication failed")]
    Unauthorized,
    #[error("sandbox resource not found")]
    NotFound,
    #[error("sandbox operation denied by policy")]
    PolicyDenied,
    #[error("sandbox resource limit exceeded")]
    LimitExceeded,
    #[error("sandbox operation timed out")]
    Timeout,
    #[error("sandbox protocol violation: {0}")]
    ProtocolViolation(String),
    #[error("sandbox internal error: {0}")]
    Internal(String),
}

/// A thin, stateless wrapper around the sandboxd protocol.
#[derive(Debug, Clone)]
pub struct SandboxClient {
    socket_path: PathBuf,
    auth_token: SecretString,
    timeout: Duration,
}

impl SandboxClient {
    /// Validates and stores the connection configuration. No socket is opened
    /// eagerly: connections happen lazily per [`Self::send`], so a down daemon
    /// never fails server startup.
    pub async fn connect(config: SandboxConfig) -> Result<Self, SandboxClientError> {
        if config.socket_path.as_os_str().is_empty() {
            return Err(SandboxClientError::ProtocolViolation(
                "sandbox socket path must not be empty".into(),
            ));
        }
        if config.timeout.is_zero() {
            return Err(SandboxClientError::ProtocolViolation(
                "sandbox socket timeout must be positive".into(),
            ));
        }
        Ok(Self {
            socket_path: config.socket_path,
            auth_token: config.auth_token,
            timeout: config.timeout,
        })
    }

    /// Sends one operation and returns the daemon's result, mapping
    /// [`SandboxErrorCode`] onto the client error taxonomy.
    pub async fn send(
        &self,
        operation: SandboxOperation,
    ) -> Result<SandboxResult, SandboxClientError> {
        let request_id = Uuid::now_v7();
        let envelope = RequestEnvelope {
            version: SANDBOX_PROTOCOL_VERSION,
            request_id,
            token: self.auth_token.expose_secret().to_owned(),
            operation,
        };
        let mut encoded = serde_json::to_vec(&envelope).map_err(|_| {
            SandboxClientError::ProtocolViolation("request serialization failed".into())
        })?;
        if encoded.len() >= MAX_PROTOCOL_LINE_BYTES {
            return Err(SandboxClientError::LimitExceeded);
        }
        encoded.push(b'\n');

        let stream = tokio::time::timeout(self.timeout, UnixStream::connect(&self.socket_path))
            .await
            .map_err(|_| SandboxClientError::Timeout)?
            .map_err(|_| SandboxClientError::Disconnected)?;
        let (read, mut write) = stream.into_split();
        tokio::time::timeout(self.timeout, write.write_all(&encoded))
            .await
            .map_err(|_| SandboxClientError::Timeout)?
            .map_err(|_| SandboxClientError::Disconnected)?;

        let mut reader = BufReader::new(read);
        let mut line = Vec::with_capacity(1024);
        let bytes = tokio::time::timeout(self.timeout, async {
            (&mut reader)
                .take(MAX_PROTOCOL_LINE_BYTES as u64 + 1)
                .read_until(b'\n', &mut line)
                .await
        })
        .await
        .map_err(|_| SandboxClientError::Timeout)?
        .map_err(|_| SandboxClientError::Disconnected)?;
        if bytes == 0 {
            return Err(SandboxClientError::Disconnected);
        }
        if line.len() > MAX_PROTOCOL_LINE_BYTES || !line.ends_with(b"\n") {
            return Err(SandboxClientError::ProtocolViolation(
                "response line exceeds the protocol bound or is unterminated".into(),
            ));
        }
        line.pop();
        let response: ResponseEnvelope = serde_json::from_slice(&line).map_err(|_| {
            SandboxClientError::ProtocolViolation("invalid response envelope".into())
        })?;
        if response.version != SANDBOX_PROTOCOL_VERSION {
            return Err(SandboxClientError::ProtocolViolation(format!(
                "daemon replied with unsupported protocol version {}",
                response.version
            )));
        }
        if response.request_id != request_id {
            return Err(SandboxClientError::ProtocolViolation(
                "daemon echoed a mismatched request_id".into(),
            ));
        }
        response.result.map_err(|error| match error.code {
            SandboxErrorCode::Unauthorized => SandboxClientError::Unauthorized,
            SandboxErrorCode::NotFound => SandboxClientError::NotFound,
            SandboxErrorCode::PolicyDenied => SandboxClientError::PolicyDenied,
            SandboxErrorCode::LimitExceeded => SandboxClientError::LimitExceeded,
            SandboxErrorCode::DeadlineExceeded => SandboxClientError::Timeout,
            SandboxErrorCode::Internal => SandboxClientError::Internal(error.message),
            SandboxErrorCode::InvalidRequest
            | SandboxErrorCode::UnsupportedVersion
            | SandboxErrorCode::Conflict
            | SandboxErrorCode::OutcomeUnknown => {
                SandboxClientError::ProtocolViolation(error.message)
            }
        })
    }

    /// `health()` returns the daemon's status string (normally `"ok"`).
    pub async fn health(&self) -> Result<String, SandboxClientError> {
        match self.send(SandboxOperation::Health).await? {
            SandboxResult::Health { status } => Ok(status),
            _ => Err(SandboxClientError::ProtocolViolation(
                "unexpected response to health".into(),
            )),
        }
    }

    /// Provisions workspace storage on the daemon. Idempotent: calling it again
    /// for the same workspace succeeds and returns the same workspace id.
    pub async fn provision_workspace(&self, workspace_id: Uuid) -> Result<(), SandboxClientError> {
        match self
            .send(SandboxOperation::ProvisionWorkspace { workspace_id })
            .await?
        {
            SandboxResult::Provisioned {
                workspace_id: echoed,
            } if echoed == workspace_id => Ok(()),
            SandboxResult::Provisioned {
                workspace_id: echoed,
            } => Err(SandboxClientError::ProtocolViolation(format!(
                "daemon provisioned a mismatched workspace id {echoed}"
            ))),
            _ => Err(SandboxClientError::ProtocolViolation(
                "unexpected response to provision_workspace".into(),
            )),
        }
    }

    // --- Filesystem helpers -------------------------------------------------

    pub async fn fs_list(
        &self,
        workspace_id: Uuid,
        path: &str,
    ) -> Result<Vec<FilesystemEntry>, SandboxClientError> {
        match self
            .send(SandboxOperation::FsList {
                workspace_id,
                path: path.into(),
            })
            .await?
        {
            SandboxResult::FsList { entries } => Ok(entries),
            _ => Err(SandboxClientError::ProtocolViolation(
                "unexpected response to fs_list".into(),
            )),
        }
    }

    /// Returns the decoded file bytes and their SHA-256 hex digest.
    pub async fn fs_read(
        &self,
        workspace_id: Uuid,
        path: &str,
    ) -> Result<(Vec<u8>, String), SandboxClientError> {
        match self
            .send(SandboxOperation::FsRead {
                workspace_id,
                path: path.into(),
            })
            .await?
        {
            SandboxResult::FsRead {
                data_base64,
                sha256,
            } => Ok((
                BASE64.decode(data_base64).map_err(|_| {
                    SandboxClientError::ProtocolViolation("invalid file payload".into())
                })?,
                sha256,
            )),
            _ => Err(SandboxClientError::ProtocolViolation(
                "unexpected response to fs_read".into(),
            )),
        }
    }

    /// Writes `data` to `path`; returns the byte count and SHA-256 hex digest.
    pub async fn fs_write(
        &self,
        workspace_id: Uuid,
        path: &str,
        data: &[u8],
    ) -> Result<(usize, String), SandboxClientError> {
        if data.len() > MAX_FILE_PAYLOAD_BYTES {
            return Err(SandboxClientError::LimitExceeded);
        }
        match self
            .send(SandboxOperation::FsWrite {
                workspace_id,
                path: path.into(),
                data_base64: BASE64.encode(data),
            })
            .await?
        {
            SandboxResult::FsWritten { bytes, sha256 } => Ok((bytes, sha256)),
            _ => Err(SandboxClientError::ProtocolViolation(
                "unexpected response to fs_write".into(),
            )),
        }
    }

    pub async fn fs_mkdir(&self, workspace_id: Uuid, path: &str) -> Result<(), SandboxClientError> {
        match self
            .send(SandboxOperation::FsMkdir {
                workspace_id,
                path: path.into(),
            })
            .await?
        {
            SandboxResult::FsCreated => Ok(()),
            _ => Err(SandboxClientError::ProtocolViolation(
                "unexpected response to fs_mkdir".into(),
            )),
        }
    }

    pub async fn fs_delete(
        &self,
        workspace_id: Uuid,
        path: &str,
    ) -> Result<(), SandboxClientError> {
        match self
            .send(SandboxOperation::FsDelete {
                workspace_id,
                path: path.into(),
            })
            .await?
        {
            SandboxResult::FsDeleted => Ok(()),
            _ => Err(SandboxClientError::ProtocolViolation(
                "unexpected response to fs_delete".into(),
            )),
        }
    }

    // --- Terminal helpers ---------------------------------------------------

    /// Starts a PTY session for `request` under the given `terminal_id`.
    pub async fn terminal_start(
        &self,
        terminal_id: Uuid,
        request: TerminalStartRequest,
    ) -> Result<Started, SandboxClientError> {
        match self
            .send(SandboxOperation::Start {
                terminal_id,
                request,
            })
            .await?
        {
            SandboxResult::Started {
                terminal_id: echoed,
                limits,
                network_policy,
            } if echoed == terminal_id => Ok(Started {
                terminal_id,
                limits,
                network_policy,
            }),
            _ => Err(SandboxClientError::ProtocolViolation(
                "unexpected response to terminal start".into(),
            )),
        }
    }

    pub async fn terminal_input(
        &self,
        terminal_id: Uuid,
        input_id: Uuid,
        data: &[u8],
    ) -> Result<InputAccepted, SandboxClientError> {
        if data.len() > MAX_TERMINAL_INPUT_BYTES {
            return Err(SandboxClientError::LimitExceeded);
        }
        match self
            .send(SandboxOperation::Input {
                terminal_id,
                input_id,
                data_base64: BASE64.encode(data),
            })
            .await?
        {
            SandboxResult::InputAccepted {
                input_id: echoed,
                bytes,
                replayed,
            } if echoed == input_id => Ok(InputAccepted {
                input_id,
                bytes,
                replayed,
            }),
            _ => Err(SandboxClientError::ProtocolViolation(
                "unexpected response to terminal input".into(),
            )),
        }
    }

    /// Reads up to `max_bytes` of terminal output; `data` is decoded from base64.
    pub async fn terminal_read_output(
        &self,
        terminal_id: Uuid,
        after_cursor: u64,
        max_bytes: u32,
        wait_ms: u32,
    ) -> Result<TerminalOutput, SandboxClientError> {
        if max_bytes > MAX_TERMINAL_OUTPUT_READ_BYTES as u32 {
            return Err(SandboxClientError::LimitExceeded);
        }
        match self
            .send(SandboxOperation::ReadOutput {
                terminal_id,
                after_cursor,
                max_bytes,
                wait_ms,
            })
            .await?
        {
            SandboxResult::Output {
                terminal_id: echoed,
                start_cursor,
                next_cursor,
                data_base64,
                state,
                output_complete,
            } if echoed == terminal_id => Ok(TerminalOutput {
                terminal_id,
                start_cursor,
                next_cursor,
                data: BASE64.decode(data_base64).map_err(|_| {
                    SandboxClientError::ProtocolViolation("invalid terminal output".into())
                })?,
                state,
                output_complete,
            }),
            _ => Err(SandboxClientError::ProtocolViolation(
                "unexpected response to terminal read_output".into(),
            )),
        }
    }

    pub async fn terminal_resize(
        &self,
        terminal_id: Uuid,
        cols: u16,
        rows: u16,
    ) -> Result<(), SandboxClientError> {
        match self
            .send(SandboxOperation::Resize {
                terminal_id,
                cols,
                rows,
            })
            .await?
        {
            SandboxResult::Resized => Ok(()),
            _ => Err(SandboxClientError::ProtocolViolation(
                "unexpected response to terminal resize".into(),
            )),
        }
    }

    pub async fn terminal_interrupt(&self, terminal_id: Uuid) -> Result<(), SandboxClientError> {
        match self
            .send(SandboxOperation::Interrupt { terminal_id })
            .await?
        {
            SandboxResult::Interrupted => Ok(()),
            _ => Err(SandboxClientError::ProtocolViolation(
                "unexpected response to terminal interrupt".into(),
            )),
        }
    }

    pub async fn terminal_terminate(&self, terminal_id: Uuid) -> Result<(), SandboxClientError> {
        match self
            .send(SandboxOperation::Terminate { terminal_id })
            .await?
        {
            SandboxResult::Terminated => Ok(()),
            _ => Err(SandboxClientError::ProtocolViolation(
                "unexpected response to terminal terminate".into(),
            )),
        }
    }

    pub async fn terminal_reconnect(
        &self,
        terminal_id: Uuid,
    ) -> Result<TerminalSessionInfo, SandboxClientError> {
        match self
            .send(SandboxOperation::Reconnect { terminal_id })
            .await?
        {
            SandboxResult::Reconnected {
                terminal_id: echoed,
                workspace_id,
                state,
                cols,
                rows,
                exit_code,
                reason,
                output_start_cursor,
                output_end_cursor,
                acked_cursor,
                output_complete,
            } if echoed == terminal_id => Ok(TerminalSessionInfo {
                terminal_id,
                workspace_id,
                state,
                cols,
                rows,
                exit_code,
                reason,
                output_start_cursor,
                output_end_cursor,
                acked_cursor,
                output_complete,
            }),
            _ => Err(SandboxClientError::ProtocolViolation(
                "unexpected response to terminal reconnect".into(),
            )),
        }
    }

    pub async fn terminal_processes(
        &self,
        terminal_id: Uuid,
    ) -> Result<Vec<SandboxProcess>, SandboxClientError> {
        match self
            .send(SandboxOperation::Processes { terminal_id })
            .await?
        {
            SandboxResult::Processes { processes } => Ok(processes),
            _ => Err(SandboxClientError::ProtocolViolation(
                "unexpected response to terminal processes".into(),
            )),
        }
    }

    pub async fn terminal_kill(
        &self,
        terminal_id: Uuid,
        pid: u32,
    ) -> Result<(), SandboxClientError> {
        match self
            .send(SandboxOperation::Kill { terminal_id, pid })
            .await?
        {
            SandboxResult::Killed { pid: echoed } if echoed == pid => Ok(()),
            _ => Err(SandboxClientError::ProtocolViolation(
                "unexpected response to terminal kill".into(),
            )),
        }
    }
}

/// Result of a successful [`SandboxOperation::Start`].
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Started {
    pub terminal_id: Uuid,
    pub limits: ResourceLimits,
    pub network_policy: NetworkPolicy,
}

/// Result of a successful [`SandboxOperation::Input`].
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct InputAccepted {
    pub input_id: Uuid,
    pub bytes: usize,
    pub replayed: bool,
}

/// Result of a successful [`SandboxOperation::ReadOutput`].
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TerminalOutput {
    pub terminal_id: Uuid,
    pub start_cursor: u64,
    pub next_cursor: u64,
    pub data: Vec<u8>,
    pub state: TerminalState,
    pub output_complete: bool,
}

/// Snapshot of a session from [`SandboxOperation::Reconnect`].
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TerminalSessionInfo {
    pub terminal_id: Uuid,
    pub workspace_id: Uuid,
    pub state: TerminalState,
    pub cols: u16,
    pub rows: u16,
    pub exit_code: Option<i32>,
    pub reason: Option<String>,
    pub output_start_cursor: u64,
    pub output_end_cursor: u64,
    pub acked_cursor: u64,
    pub output_complete: bool,
}

#[cfg(test)]
mod tests {
    use std::{path::PathBuf, sync::Arc};

    use gobrowse_core::sandbox::{FilesystemEntryKind, HARD_RESOURCE_LIMITS, SandboxProtocolError};
    use parking_lot::Mutex;
    use tokio::{
        io::{AsyncBufReadExt, AsyncWriteExt},
        net::UnixListener,
    };

    use super::*;

    type Handler = Box<dyn Fn(RequestEnvelope) -> ResponseEnvelope + Send>;

    struct FakeDaemon {
        dir: PathBuf,
        join: tokio::task::JoinHandle<()>,
    }

    impl FakeDaemon {
        /// Accepts one connection per handler, reads the request line, records
        /// it in `captured`, and replies with the handler's envelope.
        fn responding(handlers: Vec<Handler>, captured: Arc<Mutex<Vec<Vec<u8>>>>) -> Self {
            let dir =
                std::env::temp_dir().join(format!("gobrowse-sandbox-client-{}", Uuid::new_v4()));
            std::fs::create_dir(&dir).unwrap();
            let path = dir.join("fake.sock");
            let listener = UnixListener::bind(&path).unwrap();
            let join = tokio::spawn(async move {
                for handler in handlers {
                    let (stream, _) = listener.accept().await.unwrap();
                    let (read, mut write) = stream.into_split();
                    let mut reader = BufReader::new(read);
                    let mut line = Vec::new();
                    let _ = reader.read_until(b'\n', &mut line).await;
                    captured.lock().push(line.clone());
                    let request: RequestEnvelope = serde_json::from_slice(&line).unwrap();
                    let response = handler(request);
                    let mut encoded = serde_json::to_vec(&response).unwrap();
                    encoded.push(b'\n');
                    let _ = write.write_all(&encoded).await;
                }
            });
            Self { dir, join }
        }

        /// Accepts connections and never replies; used for timeout tests.
        fn silent() -> Self {
            let dir =
                std::env::temp_dir().join(format!("gobrowse-sandbox-client-{}", Uuid::new_v4()));
            std::fs::create_dir(&dir).unwrap();
            let path = dir.join("fake.sock");
            let listener = UnixListener::bind(&path).unwrap();
            let join = tokio::spawn(async move {
                let (stream, _) = listener.accept().await.unwrap();
                let (read, _write) = stream.into_split();
                let mut reader = BufReader::new(read);
                let mut line = Vec::new();
                let _ = reader.read_until(b'\n', &mut line).await;
                // Hold the connection open without responding.
                tokio::time::sleep(Duration::from_secs(60)).await;
            });
            Self { dir, join }
        }

        fn socket_path(&self) -> PathBuf {
            self.dir.join("fake.sock")
        }
    }

    impl Drop for FakeDaemon {
        fn drop(&mut self) {
            self.join.abort();
            let _ = std::fs::remove_dir_all(&self.dir);
        }
    }

    fn client(path: PathBuf) -> SandboxClient {
        SandboxClient {
            socket_path: path,
            auth_token: SecretString::from("test token"),
            timeout: Duration::from_millis(2_000),
        }
    }

    fn fast_client(path: PathBuf) -> SandboxClient {
        SandboxClient {
            socket_path: path,
            auth_token: SecretString::from("test token"),
            timeout: Duration::from_millis(150),
        }
    }

    fn ok_response(request: &RequestEnvelope, result: SandboxResult) -> ResponseEnvelope {
        ResponseEnvelope {
            version: SANDBOX_PROTOCOL_VERSION,
            request_id: request.request_id,
            result: Ok(result),
        }
    }

    fn error_response(
        request: &RequestEnvelope,
        code: SandboxErrorCode,
        message: &str,
    ) -> ResponseEnvelope {
        ResponseEnvelope {
            version: SANDBOX_PROTOCOL_VERSION,
            request_id: request.request_id,
            result: Err(SandboxProtocolError {
                code,
                message: message.into(),
            }),
        }
    }

    fn handler(f: impl Fn(RequestEnvelope) -> ResponseEnvelope + Send + 'static) -> Handler {
        Box::new(f)
    }

    fn code_message(code: SandboxErrorCode) -> String {
        match code {
            SandboxErrorCode::Unauthorized => "authentication failed".into(),
            SandboxErrorCode::NotFound => "not found".into(),
            SandboxErrorCode::PolicyDenied => "policy denied".into(),
            SandboxErrorCode::LimitExceeded => "limit exceeded".into(),
            SandboxErrorCode::DeadlineExceeded => "deadline".into(),
            SandboxErrorCode::Internal => "boom".into(),
            SandboxErrorCode::InvalidRequest => "invalid request".into(),
            SandboxErrorCode::UnsupportedVersion => "unsupported version".into(),
            SandboxErrorCode::Conflict => "conflict".into(),
            SandboxErrorCode::OutcomeUnknown => "outcome unknown".into(),
        }
    }

    #[tokio::test]
    async fn send_maps_every_sandbox_error_code_to_the_client_taxonomy() {
        let cases: Vec<(SandboxErrorCode, SandboxClientError)> = vec![
            (
                SandboxErrorCode::Unauthorized,
                SandboxClientError::Unauthorized,
            ),
            (SandboxErrorCode::NotFound, SandboxClientError::NotFound),
            (
                SandboxErrorCode::PolicyDenied,
                SandboxClientError::PolicyDenied,
            ),
            (
                SandboxErrorCode::LimitExceeded,
                SandboxClientError::LimitExceeded,
            ),
            (
                SandboxErrorCode::DeadlineExceeded,
                SandboxClientError::Timeout,
            ),
            (
                SandboxErrorCode::Internal,
                SandboxClientError::Internal("boom".into()),
            ),
            (
                SandboxErrorCode::InvalidRequest,
                SandboxClientError::ProtocolViolation("invalid request".into()),
            ),
            (
                SandboxErrorCode::UnsupportedVersion,
                SandboxClientError::ProtocolViolation("unsupported version".into()),
            ),
            (
                SandboxErrorCode::Conflict,
                SandboxClientError::ProtocolViolation("conflict".into()),
            ),
            (
                SandboxErrorCode::OutcomeUnknown,
                SandboxClientError::ProtocolViolation("outcome unknown".into()),
            ),
        ];
        for (code, expected) in cases {
            let message = code_message(code);
            let daemon = FakeDaemon::responding(
                vec![handler(move |request| {
                    error_response(&request, code, &message)
                })],
                Arc::new(Mutex::new(Vec::new())),
            );
            let result = client(daemon.socket_path())
                .send(SandboxOperation::Health)
                .await
                .unwrap_err();
            assert_eq!(
                std::mem::discriminant(&result),
                std::mem::discriminant(&expected),
                "code {code:?}"
            );
        }
    }

    #[tokio::test]
    async fn send_rejects_mismatched_request_id_and_wrong_version() {
        let captured = Arc::new(Mutex::new(Vec::new()));
        let daemon = FakeDaemon::responding(
            vec![handler(|_request| ResponseEnvelope {
                version: SANDBOX_PROTOCOL_VERSION,
                request_id: Uuid::new_v4(), // mismatched echo
                result: Ok(SandboxResult::Health {
                    status: "ok".into(),
                }),
            })],
            Arc::clone(&captured),
        );
        assert!(matches!(
            client(daemon.socket_path())
                .send(SandboxOperation::Health)
                .await,
            Err(SandboxClientError::ProtocolViolation(_))
        ));

        let captured = Arc::new(Mutex::new(Vec::new()));
        let daemon = FakeDaemon::responding(
            vec![handler(|request| ResponseEnvelope {
                version: 1, // wrong version
                request_id: request.request_id,
                result: Ok(SandboxResult::Health {
                    status: "ok".into(),
                }),
            })],
            Arc::clone(&captured),
        );
        assert!(matches!(
            client(daemon.socket_path())
                .send(SandboxOperation::Health)
                .await,
            Err(SandboxClientError::ProtocolViolation(_))
        ));
    }

    #[tokio::test]
    async fn send_serializes_a_version_two_envelope_with_the_configured_token() {
        let captured = Arc::new(Mutex::new(Vec::new()));
        let daemon = FakeDaemon::responding(
            vec![handler(|request| {
                ok_response(
                    &request,
                    SandboxResult::Health {
                        status: "ok".into(),
                    },
                )
            })],
            Arc::clone(&captured),
        );
        let client = client(daemon.socket_path());
        assert_eq!(client.health().await.unwrap(), "ok");
        let captured = captured.lock();
        assert_eq!(captured.len(), 1);
        let envelope: RequestEnvelope = serde_json::from_slice(&captured[0]).unwrap();
        assert_eq!(envelope.version, SANDBOX_PROTOCOL_VERSION);
        assert_eq!(envelope.version, 2);
        assert!(!envelope.request_id.is_nil());
        assert_eq!(envelope.token, "test token");
        assert!(matches!(envelope.operation, SandboxOperation::Health));
    }

    #[tokio::test]
    async fn provision_workspace_round_trips_and_detects_mismatched_echo() {
        let workspace_id = Uuid::new_v4();
        let daemon = FakeDaemon::responding(
            vec![
                handler(move |request| {
                    ok_response(&request, SandboxResult::Provisioned { workspace_id })
                }),
                handler(move |request| {
                    ok_response(
                        &request,
                        SandboxResult::Provisioned {
                            workspace_id: Uuid::new_v4(),
                        },
                    )
                }),
            ],
            Arc::new(Mutex::new(Vec::new())),
        );
        let client = client(daemon.socket_path());
        assert!(client.provision_workspace(workspace_id).await.is_ok());
        assert!(matches!(
            client.provision_workspace(workspace_id).await,
            Err(SandboxClientError::ProtocolViolation(_))
        ));
    }

    #[tokio::test]
    async fn fs_helpers_decode_and_verify_their_variants() {
        let workspace_id = Uuid::new_v4();
        let daemon = FakeDaemon::responding(
            vec![
                handler(|request| {
                    ok_response(
                        &request,
                        SandboxResult::FsList {
                            entries: vec![FilesystemEntry {
                                name: "hello.txt".into(),
                                kind: FilesystemEntryKind::File,
                                size: 5,
                            }],
                        },
                    )
                }),
                handler(|request| {
                    ok_response(
                        &request,
                        SandboxResult::FsRead {
                            data_base64: BASE64.encode(b"hello"),
                            sha256: "deadbeef".into(),
                        },
                    )
                }),
                handler(|request| {
                    ok_response(
                        &request,
                        SandboxResult::FsWritten {
                            bytes: 5,
                            sha256: "deadbeef".into(),
                        },
                    )
                }),
                handler(|request| ok_response(&request, SandboxResult::FsCreated)),
                handler(|request| ok_response(&request, SandboxResult::FsDeleted)),
            ],
            Arc::new(Mutex::new(Vec::new())),
        );
        let client = client(daemon.socket_path());
        let entries = client.fs_list(workspace_id, ".").await.unwrap();
        assert_eq!(entries[0].name, "hello.txt");
        assert_eq!(entries[0].size, 5);
        let (bytes, sha256) = client.fs_read(workspace_id, "hello.txt").await.unwrap();
        assert_eq!(bytes, b"hello");
        assert_eq!(sha256, "deadbeef");
        let (written, digest) = client
            .fs_write(workspace_id, "hello.txt", b"hello")
            .await
            .unwrap();
        assert_eq!(written, 5);
        assert_eq!(digest, "deadbeef");
        assert!(client.fs_mkdir(workspace_id, "sub").await.is_ok());
        assert!(client.fs_delete(workspace_id, "sub").await.is_ok());
    }

    #[tokio::test]
    async fn terminal_helpers_decode_output_and_verify_unit_variants() {
        let terminal_id = Uuid::new_v4();
        let workspace_id = Uuid::new_v4();
        let daemon = FakeDaemon::responding(
            vec![
                handler(move |request| {
                    ok_response(
                        &request,
                        SandboxResult::Started {
                            terminal_id,
                            limits: HARD_RESOURCE_LIMITS,
                            network_policy: NetworkPolicy::None,
                        },
                    )
                }),
                handler(move |request| {
                    ok_response(
                        &request,
                        SandboxResult::Output {
                            terminal_id,
                            start_cursor: 0,
                            next_cursor: 6,
                            data_base64: BASE64.encode(b"hello\n"),
                            state: TerminalState::Running,
                            output_complete: true,
                        },
                    )
                }),
                handler(|request| ok_response(&request, SandboxResult::Resized)),
                handler(|request| ok_response(&request, SandboxResult::Interrupted)),
            ],
            Arc::new(Mutex::new(Vec::new())),
        );
        let client = client(daemon.socket_path());
        let started = client
            .terminal_start(
                terminal_id,
                TerminalStartRequest {
                    workspace_id,
                    command: vec!["/bin/echo".into()],
                    working_directory: ".".into(),
                    cols: 80,
                    rows: 24,
                    network_policy: NetworkPolicy::None,
                    limits: HARD_RESOURCE_LIMITS,
                },
            )
            .await
            .unwrap();
        assert_eq!(started.terminal_id, terminal_id);
        let output = client
            .terminal_read_output(terminal_id, 0, 4096, 0)
            .await
            .unwrap();
        assert_eq!(output.data, b"hello\n");
        assert!(output.output_complete);
        assert!(client.terminal_resize(terminal_id, 100, 40).await.is_ok());
        assert!(client.terminal_interrupt(terminal_id).await.is_ok());
    }

    #[tokio::test]
    async fn connect_to_missing_socket_returns_disconnected() {
        let missing =
            std::env::temp_dir().join(format!("gobrowse-sandbox-missing-{}.sock", Uuid::new_v4()));
        let client = client(missing);
        assert!(matches!(
            client.send(SandboxOperation::Health).await,
            Err(SandboxClientError::Disconnected)
        ));
    }

    #[tokio::test]
    async fn unresponsive_daemon_yields_timeout() {
        let daemon = FakeDaemon::silent();
        let client = fast_client(daemon.socket_path());
        assert!(matches!(
            client.send(SandboxOperation::Health).await,
            Err(SandboxClientError::Timeout)
        ));
    }

    #[tokio::test]
    async fn connect_rejects_empty_socket_path_and_zero_timeout() {
        assert!(matches!(
            SandboxClient::connect(SandboxConfig {
                socket_path: PathBuf::new(),
                auth_token: SecretString::from("t"),
                timeout: Duration::from_secs(1),
            })
            .await,
            Err(SandboxClientError::ProtocolViolation(_))
        ));
        assert!(matches!(
            SandboxClient::connect(SandboxConfig {
                socket_path: PathBuf::from("/tmp/x.sock"),
                auth_token: SecretString::from("t"),
                timeout: Duration::ZERO,
            })
            .await,
            Err(SandboxClientError::ProtocolViolation(_))
        ));
    }
}
