use std::{future::Future, io::ErrorKind, sync::Arc, time::Duration};

use base64::{Engine, engine::general_purpose::STANDARD as BASE64};
use gobrowse_core::sandbox::{
    HARD_RESOURCE_LIMITS, MAX_FILE_PAYLOAD_BYTES, MAX_PROTOCOL_LINE_BYTES,
    MAX_TERMINAL_INPUT_BYTES, NetworkPolicy, RequestEnvelope, ResourceLimits, ResponseEnvelope,
    SANDBOX_PROTOCOL_VERSION, SandboxErrorCode, SandboxOperation, SandboxProtocolError,
    SandboxResult,
};
use sha2::{Digest, Sha256};
use subtle::ConstantTimeEq;
use thiserror::Error;
use tokio::{
    io::{AsyncBufReadExt, AsyncReadExt, AsyncWriteExt, BufReader},
    net::UnixStream,
    sync::{Mutex, Semaphore},
};
use uuid::Uuid;

use crate::{
    Filesystem, FilesystemError, RuntimeError, SandboxRuntime, SocketConfig, ValidatedStart,
    replay::{ReplayCache, ReplayDecision},
    socket::shutdown_signal,
};

const MAX_TOKEN_BYTES: usize = 4 * 1024;
const MAX_REPLAY_ENTRIES_HARD: usize = 16_384;
const MAX_CONNECTIONS_HARD: usize = 1_024;
const MAX_PRE_AUTH_TIMEOUT: Duration = Duration::from_secs(60);
const MAX_IDLE_TIMEOUT: Duration = Duration::from_secs(60 * 60);
const MAX_OPERATION_TIMEOUT: Duration = Duration::from_secs(5 * 60);
const ACCEPT_RETRY_DELAY: Duration = Duration::from_millis(50);

#[derive(Debug, Clone)]
pub struct DaemonConfig {
    pub socket: SocketConfig,
    pub resource_ceiling: ResourceLimits,
    pub network: NetworkPolicyConfig,
    pub replay_capacity: usize,
    pub connections: ConnectionConfig,
}

#[derive(Debug, Clone, Copy)]
pub struct ConnectionConfig {
    pub max_connections: usize,
    pub pre_auth_timeout: Duration,
    pub idle_timeout: Duration,
    pub operation_timeout: Duration,
}

impl ConnectionConfig {
    fn validate(self) -> Result<(), DaemonError> {
        if self.max_connections == 0
            || self.max_connections > MAX_CONNECTIONS_HARD
            || self.pre_auth_timeout.is_zero()
            || self.pre_auth_timeout > MAX_PRE_AUTH_TIMEOUT
            || self.idle_timeout.is_zero()
            || self.idle_timeout > MAX_IDLE_TIMEOUT
            || self.operation_timeout.is_zero()
            || self.operation_timeout > MAX_OPERATION_TIMEOUT
        {
            return Err(DaemonError::InvalidConfiguration);
        }
        Ok(())
    }
}

#[derive(Debug, Clone)]
pub struct NetworkPolicyConfig {
    pub restricted_network: Option<String>,
    pub allow_full: bool,
}

impl NetworkPolicyConfig {
    pub fn resolve(&self, requested: NetworkPolicy) -> Result<String, DaemonError> {
        match requested {
            NetworkPolicy::None => Ok("none".into()),
            NetworkPolicy::Restricted => self
                .restricted_network
                .as_ref()
                .filter(|name| valid_restricted_network_name(name))
                .cloned()
                .ok_or(DaemonError::PolicyDenied),
            NetworkPolicy::Full if self.allow_full => Ok("slirp4netns".into()),
            NetworkPolicy::Full => Err(DaemonError::PolicyDenied),
        }
    }
}

#[derive(Clone)]
pub struct Authenticator {
    expected_digest: [u8; 32],
}

impl Authenticator {
    pub fn new(token: &str) -> Result<Self, DaemonError> {
        if token.is_empty() || token.len() > MAX_TOKEN_BYTES || token.as_bytes().contains(&0) {
            return Err(DaemonError::InvalidConfiguration);
        }
        Ok(Self {
            expected_digest: Sha256::digest(token.as_bytes()).into(),
        })
    }

    #[must_use]
    pub fn verify(&self, candidate: &str) -> bool {
        if candidate.len() > MAX_TOKEN_BYTES {
            return false;
        }
        let candidate_digest: [u8; 32] = Sha256::digest(candidate.as_bytes()).into();
        bool::from(self.expected_digest.ct_eq(&candidate_digest))
    }
}

#[derive(Debug, Error)]
pub enum DaemonError {
    #[error("invalid daemon configuration")]
    InvalidConfiguration,
    #[error("request was not authenticated")]
    Unauthorized,
    #[error("request uses an unsupported protocol version")]
    UnsupportedVersion,
    #[error("request is invalid")]
    InvalidRequest,
    #[error("requested policy is not permitted")]
    PolicyDenied,
    #[error("requested operation exceeds a bound")]
    LimitExceeded,
    #[error("requested object does not exist")]
    NotFound,
    #[error("requested object conflicts with existing state")]
    Conflict,
    #[error("connection deadline exceeded")]
    ConnectionTimeout,
    #[error("operation deadline exceeded")]
    OperationTimeout,
    #[error("filesystem operation failed")]
    Filesystem(#[from] FilesystemError),
    #[error("runtime operation failed")]
    Runtime(#[from] RuntimeError),
    #[error("daemon I/O failed")]
    Io(#[from] std::io::Error),
}

#[derive(Clone)]
pub struct Daemon {
    config: Arc<DaemonConfig>,
    auth: Authenticator,
    filesystem: Filesystem,
    filesystem_lock: Arc<Mutex<()>>,
    runtime: Arc<dyn SandboxRuntime>,
    replay: Arc<ReplayCache>,
    connections: Arc<Semaphore>,
}

struct ReplayExecutionGuard {
    cache: Arc<ReplayCache>,
    request_id: Uuid,
    fingerprint: [u8; 32],
    armed: bool,
}

impl ReplayExecutionGuard {
    fn disarm(&mut self) {
        self.armed = false;
    }
}

impl Drop for ReplayExecutionGuard {
    fn drop(&mut self) {
        if self.armed {
            self.cache.complete(
                self.request_id,
                self.fingerprint,
                error_response(self.request_id, DaemonError::OperationTimeout),
            );
        }
    }
}

impl Daemon {
    pub fn new(
        config: DaemonConfig,
        auth: Authenticator,
        filesystem: Filesystem,
        runtime: Arc<dyn SandboxRuntime>,
    ) -> Result<Self, DaemonError> {
        config
            .resource_ceiling
            .validate_hard_ceiling()
            .map_err(|_| DaemonError::InvalidConfiguration)?;
        config.socket.validate()?;
        config.connections.validate()?;
        if config.replay_capacity == 0
            || config.replay_capacity > MAX_REPLAY_ENTRIES_HARD
            || config
                .network
                .restricted_network
                .as_ref()
                .is_some_and(|name| !valid_restricted_network_name(name))
        {
            return Err(DaemonError::InvalidConfiguration);
        }
        let replay =
            ReplayCache::new(config.replay_capacity).ok_or(DaemonError::InvalidConfiguration)?;
        let connections = Arc::new(Semaphore::new(config.connections.max_connections));
        Ok(Self {
            config: Arc::new(config),
            auth,
            filesystem,
            filesystem_lock: Arc::new(Mutex::new(())),
            runtime,
            replay: Arc::new(replay),
            connections,
        })
    }

    pub async fn serve(self) -> Result<(), DaemonError> {
        self.serve_until(async { shutdown_signal().await.map_err(DaemonError::Io) })
            .await
    }

    pub async fn serve_until<F>(self, shutdown: F) -> Result<(), DaemonError>
    where
        F: Future<Output = Result<(), DaemonError>>,
    {
        let (listener, _socket_guard) = self.config.socket.bind().await?;
        tokio::pin!(shutdown);
        loop {
            let permit = tokio::select! {
                result = &mut shutdown => return result,
                permit = Arc::clone(&self.connections).acquire_owned() => {
                    permit.map_err(|_| DaemonError::InvalidConfiguration)?
                }
            };
            let accepted = tokio::select! {
                result = &mut shutdown => return result,
                accepted = listener.accept() => accepted,
            };
            match accepted {
                Ok((stream, _)) => {
                    let daemon = self.clone();
                    tokio::spawn(async move {
                        let _permit = permit;
                        let _ = daemon.handle_connection(stream).await;
                    });
                }
                Err(error) if retryable_accept_error(&error) => {
                    drop(permit);
                    tokio::select! {
                        result = &mut shutdown => return result,
                        () = tokio::time::sleep(ACCEPT_RETRY_DELAY) => {}
                    }
                }
                Err(error) => return Err(DaemonError::Io(error)),
            }
        }
    }

    pub async fn handle_line(&self, line: &[u8]) -> ResponseEnvelope {
        let value = match serde_json::from_slice::<serde_json::Value>(line) {
            Ok(value) if valid_wire_shape(&value) => value,
            Err(_) => {
                return error_response(Uuid::nil(), DaemonError::InvalidRequest);
            }
            Ok(_) => return error_response(Uuid::nil(), DaemonError::InvalidRequest),
        };
        let request = match serde_json::from_value::<RequestEnvelope>(value) {
            Ok(request) => request,
            Err(_) => return error_response(Uuid::nil(), DaemonError::InvalidRequest),
        };
        let request_id = request.request_id;
        if request_id.is_nil() {
            return error_response(request_id, DaemonError::InvalidRequest);
        }
        if !self.auth.verify(&request.token) {
            return error_response(request_id, DaemonError::Unauthorized);
        }
        if request.version != SANDBOX_PROTOCOL_VERSION {
            return error_response(request_id, DaemonError::UnsupportedVersion);
        }
        if !is_non_idempotent(&request.operation) {
            return self.response_for(request_id, request.operation).await;
        }
        let fingerprint: [u8; 32] = match serde_json::to_vec(&request.operation) {
            Ok(encoded) => Sha256::digest(encoded).into(),
            Err(_) => return error_response(request_id, DaemonError::InvalidRequest),
        };
        loop {
            match self.replay.begin(request_id, fingerprint) {
                ReplayDecision::Execute => {
                    let mut completion_guard = ReplayExecutionGuard {
                        cache: Arc::clone(&self.replay),
                        request_id,
                        fingerprint,
                        armed: true,
                    };
                    let response = self.response_for(request_id, request.operation).await;
                    self.replay
                        .complete(request_id, fingerprint, response.clone());
                    completion_guard.disarm();
                    return response;
                }
                ReplayDecision::Cached(response) => return response,
                ReplayDecision::Conflict => {
                    return error_response(request_id, DaemonError::Conflict);
                }
                ReplayDecision::Full => {
                    return error_response(request_id, DaemonError::LimitExceeded);
                }
                ReplayDecision::Wait(notify) => {
                    let notified = notify.notified();
                    tokio::pin!(notified);
                    notified.as_mut().enable();
                    if matches!(
                        self.replay.begin(request_id, fingerprint),
                        ReplayDecision::Wait(_)
                    ) {
                        notified.await;
                    }
                }
            }
        }
    }

    async fn response_for(
        &self,
        request_id: Uuid,
        operation: SandboxOperation,
    ) -> ResponseEnvelope {
        match tokio::time::timeout(
            self.config.connections.operation_timeout,
            self.dispatch(operation),
        )
        .await
        {
            Ok(Ok(result)) => ResponseEnvelope {
                version: SANDBOX_PROTOCOL_VERSION,
                request_id,
                result: Ok(result),
            },
            Ok(Err(error)) => error_response(request_id, error),
            Err(_) => error_response(request_id, DaemonError::OperationTimeout),
        }
    }

    async fn handle_connection(&self, stream: UnixStream) -> Result<(), DaemonError> {
        self.config.socket.validate_peer(&stream)?;
        let (read, mut write) = stream.into_split();
        let mut reader = BufReader::new(read);
        let mut authenticated = false;
        loop {
            let deadline = if authenticated {
                self.config.connections.idle_timeout
            } else {
                self.config.connections.pre_auth_timeout
            };
            let mut line = Vec::with_capacity(1024);
            let bytes = tokio::time::timeout(deadline, async {
                (&mut reader)
                    .take(MAX_PROTOCOL_LINE_BYTES as u64 + 1)
                    .read_until(b'\n', &mut line)
                    .await
            })
            .await
            .map_err(|_| DaemonError::ConnectionTimeout)??;
            if bytes == 0 {
                return Ok(());
            }
            if line.len() > MAX_PROTOCOL_LINE_BYTES || !line.ends_with(b"\n") {
                let response = error_response(Uuid::nil(), DaemonError::LimitExceeded);
                tokio::time::timeout(deadline, write_response(&mut write, &response))
                    .await
                    .map_err(|_| DaemonError::ConnectionTimeout)??;
                return Ok(());
            }
            line.pop();
            if line.ends_with(b"\r") {
                line.pop();
            }
            let response = self.handle_line(&line).await;
            let authorized = !response.request_id.is_nil()
                && !matches!(
                    response.result,
                    Err(ref error) if error.code == SandboxErrorCode::Unauthorized
                );
            let operation_timed_out = matches!(
                response.result,
                Err(ref error) if error.code == SandboxErrorCode::DeadlineExceeded
            );
            tokio::time::timeout(deadline, write_response(&mut write, &response))
                .await
                .map_err(|_| DaemonError::ConnectionTimeout)??;
            if !authorized || operation_timed_out {
                return Ok(());
            }
            authenticated = true;
        }
    }

    async fn dispatch(&self, operation: SandboxOperation) -> Result<SandboxResult, DaemonError> {
        match operation {
            SandboxOperation::Health => Ok(SandboxResult::Health {
                status: "ok".into(),
            }),
            SandboxOperation::Start { mut request } => {
                request
                    .validate()
                    .map_err(|_| DaemonError::InvalidRequest)?;
                request.limits = request.limits.narrow_to(self.config.resource_ceiling);
                request.limits = request.limits.narrow_to(HARD_RESOURCE_LIMITS);
                let network_name = self.config.network.resolve(request.network_policy)?;
                let workspace_path = {
                    let _guard = self.filesystem_lock.lock().await;
                    // Start never creates workspace storage: quota-managed deployment
                    // provisioning must create the workspace before it can be mounted.
                    self.filesystem
                        .require_directory(request.workspace_id, &request.working_directory)?;
                    self.filesystem.workspace_path(request.workspace_id)
                };
                let terminal_id = Uuid::new_v4();
                let limits = request.limits;
                let network_policy = request.network_policy;
                self.runtime
                    .start(ValidatedStart {
                        terminal_id,
                        request,
                        workspace_path,
                        network_name,
                    })
                    .await?;
                Ok(SandboxResult::Started {
                    terminal_id,
                    limits,
                    network_policy,
                })
            }
            SandboxOperation::Input {
                terminal_id,
                data_base64,
            } => {
                let bytes = decode_bounded(&data_base64, MAX_TERMINAL_INPUT_BYTES)?;
                self.runtime.input(terminal_id, &bytes).await?;
                Ok(SandboxResult::InputAccepted { bytes: bytes.len() })
            }
            SandboxOperation::Resize {
                terminal_id,
                cols,
                rows,
            } => {
                if !(20..=1_000).contains(&cols) || !(5..=500).contains(&rows) {
                    return Err(DaemonError::InvalidRequest);
                }
                self.runtime.resize(terminal_id, cols, rows).await?;
                Ok(SandboxResult::Resized)
            }
            SandboxOperation::Terminate { terminal_id } => {
                self.runtime.terminate(terminal_id).await?;
                Ok(SandboxResult::Terminated)
            }
            SandboxOperation::Inspect { terminal_id } => {
                let inspected = self.runtime.inspect(terminal_id).await?;
                Ok(SandboxResult::Inspected {
                    terminal_id: inspected.terminal_id,
                    workspace_id: inspected.workspace_id,
                    state: inspected.state,
                })
            }
            SandboxOperation::FsList { workspace_id, path } => {
                let _guard = self.filesystem_lock.lock().await;
                Ok(SandboxResult::FsList {
                    entries: self.filesystem.list(workspace_id, &path)?,
                })
            }
            SandboxOperation::FsRead { workspace_id, path } => {
                let _guard = self.filesystem_lock.lock().await;
                let bytes = self.filesystem.read(workspace_id, &path)?;
                Ok(SandboxResult::FsRead {
                    data_base64: BASE64.encode(bytes),
                })
            }
            SandboxOperation::FsWrite {
                workspace_id,
                path,
                data_base64,
            } => {
                let bytes = decode_bounded(&data_base64, MAX_FILE_PAYLOAD_BYTES)?;
                let _guard = self.filesystem_lock.lock().await;
                self.filesystem.write(workspace_id, &path, &bytes)?;
                Ok(SandboxResult::FsWritten { bytes: bytes.len() })
            }
            SandboxOperation::FsMkdir { workspace_id, path } => {
                let _guard = self.filesystem_lock.lock().await;
                self.filesystem.mkdir(workspace_id, &path)?;
                Ok(SandboxResult::FsCreated)
            }
            SandboxOperation::FsRename {
                workspace_id,
                from,
                to,
            } => {
                let _guard = self.filesystem_lock.lock().await;
                self.filesystem.rename(workspace_id, &from, &to)?;
                Ok(SandboxResult::FsRenamed)
            }
            SandboxOperation::FsDelete { workspace_id, path } => {
                let _guard = self.filesystem_lock.lock().await;
                self.filesystem.delete(workspace_id, &path)?;
                Ok(SandboxResult::FsDeleted)
            }
        }
    }
}

async fn write_response(
    write: &mut tokio::net::unix::OwnedWriteHalf,
    response: &ResponseEnvelope,
) -> Result<(), DaemonError> {
    let mut encoded = serde_json::to_vec(response).map_err(|_| DaemonError::InvalidRequest)?;
    if encoded.len() + 1 > MAX_PROTOCOL_LINE_BYTES {
        encoded = serde_json::to_vec(&error_response(
            response.request_id,
            DaemonError::LimitExceeded,
        ))
        .map_err(|_| DaemonError::InvalidRequest)?;
    }
    encoded.push(b'\n');
    write.write_all(&encoded).await?;
    Ok(())
}

fn decode_bounded(value: &str, maximum: usize) -> Result<Vec<u8>, DaemonError> {
    let maximum_encoded = maximum.div_ceil(3) * 4;
    if value.len() > maximum_encoded {
        return Err(DaemonError::LimitExceeded);
    }
    let bytes = BASE64
        .decode(value)
        .map_err(|_| DaemonError::InvalidRequest)?;
    if bytes.len() > maximum {
        return Err(DaemonError::LimitExceeded);
    }
    Ok(bytes)
}

fn retryable_accept_error(error: &std::io::Error) -> bool {
    matches!(
        error.kind(),
        ErrorKind::Interrupted
            | ErrorKind::WouldBlock
            | ErrorKind::ConnectionAborted
            | ErrorKind::OutOfMemory
    ) || matches!(error.raw_os_error(), Some(23 | 24 | 105))
    // Linux ENFILE, EMFILE, and ENOBUFS. The daemon is Linux/Podman-specific.
}

fn valid_restricted_network_name(name: &str) -> bool {
    const RESERVED: [&str; 10] = [
        "host",
        "slirp4netns",
        "pasta",
        "container",
        "ns",
        "private",
        "bridge",
        "default",
        "none",
        "podman",
    ];
    let lower = name.to_ascii_lowercase();
    // This proves only that the mode cannot select a Podman special namespace and that the name
    // is deployment-owned. Provisioning and auditing the network's egress enforcement remains
    // release-gated; sandboxd never creates a permissive fallback network.
    name.starts_with("gobrowse-restricted-")
        && name.len() > "gobrowse-restricted-".len()
        && name.len() <= 128
        && !RESERVED.contains(&lower.as_str())
        && name
            .chars()
            .all(|character| character.is_ascii_alphanumeric() || matches!(character, '-' | '_'))
}

fn is_non_idempotent(operation: &SandboxOperation) -> bool {
    matches!(
        operation,
        SandboxOperation::Start { .. }
            | SandboxOperation::Input { .. }
            | SandboxOperation::Resize { .. }
            | SandboxOperation::Terminate { .. }
            | SandboxOperation::FsWrite { .. }
            | SandboxOperation::FsMkdir { .. }
            | SandboxOperation::FsRename { .. }
            | SandboxOperation::FsDelete { .. }
    )
}

fn valid_wire_shape(value: &serde_json::Value) -> bool {
    let Some(envelope) = value.as_object() else {
        return false;
    };
    if !envelope.keys().all(|key| {
        matches!(
            key.as_str(),
            "version" | "request_id" | "token" | "operation"
        )
    }) {
        return false;
    }
    let Some(operation) = envelope
        .get("operation")
        .and_then(serde_json::Value::as_object)
    else {
        return false;
    };
    let Some(name) = operation.get("op").and_then(serde_json::Value::as_str) else {
        return false;
    };
    let allowed: &[&str] = match name {
        "health" => &["op"],
        "start" => &["op", "request"],
        "input" => &["op", "terminal_id", "data_base64"],
        "resize" => &["op", "terminal_id", "cols", "rows"],
        "terminate" | "inspect" => &["op", "terminal_id"],
        "fs_list" | "fs_read" | "fs_mkdir" | "fs_delete" => &["op", "workspace_id", "path"],
        "fs_write" => &["op", "workspace_id", "path", "data_base64"],
        "fs_rename" => &["op", "workspace_id", "from", "to"],
        _ => return false,
    };
    operation.keys().all(|key| allowed.contains(&key.as_str()))
}

fn error_response(request_id: Uuid, error: DaemonError) -> ResponseEnvelope {
    let (code, message) = match error {
        DaemonError::Unauthorized => (SandboxErrorCode::Unauthorized, "authentication failed"),
        DaemonError::UnsupportedVersion => (
            SandboxErrorCode::UnsupportedVersion,
            "unsupported protocol version",
        ),
        DaemonError::PolicyDenied => (SandboxErrorCode::PolicyDenied, "policy denied"),
        DaemonError::LimitExceeded
        | DaemonError::Filesystem(FilesystemError::LimitExceeded)
        | DaemonError::Runtime(RuntimeError::SessionLimit) => {
            (SandboxErrorCode::LimitExceeded, "operation exceeds a bound")
        }
        DaemonError::Runtime(RuntimeError::WorkspaceQuotaUnavailable) => (
            SandboxErrorCode::PolicyDenied,
            "workspace quota unavailable",
        ),
        DaemonError::OperationTimeout | DaemonError::Runtime(RuntimeError::InputTimeout) => (
            SandboxErrorCode::DeadlineExceeded,
            "operation deadline exceeded",
        ),
        DaemonError::NotFound
        | DaemonError::Filesystem(FilesystemError::NotFound | FilesystemError::WorkspaceNotFound)
        | DaemonError::Runtime(RuntimeError::NotFound) => {
            (SandboxErrorCode::NotFound, "object not found")
        }
        DaemonError::Conflict | DaemonError::Filesystem(FilesystemError::Conflict) => {
            (SandboxErrorCode::Conflict, "object conflict")
        }
        DaemonError::InvalidRequest | DaemonError::Filesystem(FilesystemError::InvalidPath) => {
            (SandboxErrorCode::InvalidRequest, "invalid request")
        }
        DaemonError::InvalidConfiguration
        | DaemonError::ConnectionTimeout
        | DaemonError::Io(_)
        | DaemonError::Filesystem(_)
        | DaemonError::Runtime(_) => (SandboxErrorCode::Internal, "operation failed"),
    };
    ResponseEnvelope {
        version: SANDBOX_PROTOCOL_VERSION,
        request_id,
        result: Err(SandboxProtocolError {
            code,
            message: message.into(),
        }),
    }
}

#[cfg(test)]
mod tests {
    use std::{
        fs,
        os::unix::fs::PermissionsExt,
        path::{Path, PathBuf},
    };

    use async_trait::async_trait;
    use gobrowse_core::sandbox::{TerminalStartRequest, TerminalState};
    use tokio::sync::Mutex as TokioMutex;

    use super::*;
    use crate::{effective_uid_from_proc_status, runtime::RuntimeInspect};

    #[derive(Default)]
    struct MockRuntime {
        starts: TokioMutex<Vec<ValidatedStart>>,
        start_delay: std::time::Duration,
    }

    #[async_trait]
    impl SandboxRuntime for MockRuntime {
        async fn start(&self, start: ValidatedStart) -> Result<(), RuntimeError> {
            self.starts.lock().await.push(start);
            tokio::time::sleep(self.start_delay).await;
            Ok(())
        }

        async fn input(&self, _terminal_id: Uuid, _bytes: &[u8]) -> Result<(), RuntimeError> {
            Ok(())
        }

        async fn resize(
            &self,
            _terminal_id: Uuid,
            _cols: u16,
            _rows: u16,
        ) -> Result<(), RuntimeError> {
            Ok(())
        }

        async fn terminate(&self, _terminal_id: Uuid) -> Result<(), RuntimeError> {
            Ok(())
        }

        async fn inspect(&self, terminal_id: Uuid) -> Result<RuntimeInspect, RuntimeError> {
            Ok(RuntimeInspect {
                terminal_id,
                workspace_id: Uuid::nil(),
                state: TerminalState::Running,
            })
        }
    }

    struct TestDirectory(PathBuf);

    impl TestDirectory {
        fn new() -> Self {
            let path = std::env::temp_dir().join(format!("gobrowse-daemon-{}", Uuid::new_v4()));
            fs::create_dir(&path).unwrap();
            fs::set_permissions(&path, fs::Permissions::from_mode(0o700)).unwrap();
            Self(path)
        }
    }

    impl Drop for TestDirectory {
        fn drop(&mut self) {
            let _ = fs::remove_dir_all(&self.0);
        }
    }

    fn daemon(
        root: &Path,
        network: NetworkPolicyConfig,
        ceiling: ResourceLimits,
        runtime: Arc<MockRuntime>,
    ) -> Daemon {
        let uid = effective_uid_from_proc_status(&fs::read_to_string("/proc/self/status").unwrap())
            .unwrap();
        Daemon::new(
            DaemonConfig {
                socket: SocketConfig {
                    path: root.join("sandboxd.sock"),
                    mode: 0o600,
                    owner_uid: uid,
                    allowed_peer_uid: uid,
                },
                resource_ceiling: ceiling,
                network,
                replay_capacity: 32,
                connections: ConnectionConfig {
                    max_connections: 4,
                    pre_auth_timeout: Duration::from_secs(1),
                    idle_timeout: Duration::from_secs(1),
                    operation_timeout: Duration::from_secs(1),
                },
            },
            Authenticator::new("correct opaque daemon token").unwrap(),
            Filesystem::new(root.join("workspaces")).unwrap(),
            runtime,
        )
        .unwrap()
    }

    fn envelope(token: &str, operation: SandboxOperation) -> Vec<u8> {
        envelope_with_id(Uuid::new_v4(), token, operation)
    }

    fn envelope_with_id(request_id: Uuid, token: &str, operation: SandboxOperation) -> Vec<u8> {
        serde_json::to_vec(&RequestEnvelope {
            version: SANDBOX_PROTOCOL_VERSION,
            request_id,
            token: token.into(),
            operation,
        })
        .unwrap()
    }

    async fn request_socket(socket: &Path, request: &[u8]) -> ResponseEnvelope {
        let mut stream = UnixStream::connect(socket).await.unwrap();
        stream.write_all(request).await.unwrap();
        stream.write_all(b"\n").await.unwrap();
        let mut reader = BufReader::new(stream);
        let mut encoded = String::new();
        reader.read_line(&mut encoded).await.unwrap();
        serde_json::from_str(&encoded).unwrap()
    }

    fn start_request(policy: NetworkPolicy, limits: ResourceLimits) -> SandboxOperation {
        SandboxOperation::Start {
            request: TerminalStartRequest {
                workspace_id: Uuid::new_v4(),
                command: vec!["/usr/bin/env".into()],
                working_directory: ".".into(),
                cols: 80,
                rows: 24,
                network_policy: policy,
                limits,
            },
        }
    }

    fn provision_start_workspace(daemon: &Daemon, operation: &SandboxOperation) {
        let SandboxOperation::Start { request } = operation else {
            panic!("expected start operation");
        };
        daemon
            .filesystem
            .ensure_workspace(request.workspace_id)
            .unwrap();
    }

    fn with_connections(mut daemon: Daemon, connections: ConnectionConfig) -> Daemon {
        Arc::get_mut(&mut daemon.config).unwrap().connections = connections;
        daemon.connections = Arc::new(Semaphore::new(connections.max_connections));
        daemon
    }

    #[tokio::test]
    async fn protocol_authenticates_before_health_dispatch_and_fails_closed() {
        let root = TestDirectory::new();
        let daemon = daemon(
            &root.0,
            NetworkPolicyConfig {
                restricted_network: Some("gobrowse-restricted-test".into()),
                allow_full: false,
            },
            HARD_RESOURCE_LIMITS,
            Arc::new(MockRuntime::default()),
        );
        let denied = daemon
            .handle_line(&envelope("wrong", SandboxOperation::Health))
            .await;
        assert_eq!(
            denied.result.unwrap_err().code,
            SandboxErrorCode::Unauthorized
        );
        let health = daemon
            .handle_line(&envelope(
                "correct opaque daemon token",
                SandboxOperation::Health,
            ))
            .await;
        assert!(matches!(health.result, Ok(SandboxResult::Health { .. })));
        let malformed = daemon.handle_line(b"{not-json}").await;
        assert_eq!(
            malformed.result.unwrap_err().code,
            SandboxErrorCode::InvalidRequest
        );
        let unknown_operation_field = format!(
            r#"{{"version":1,"request_id":"{}","token":"correct opaque daemon token","operation":{{"op":"health","extra":true}}}}"#,
            Uuid::new_v4()
        );
        let rejected = daemon.handle_line(unknown_operation_field.as_bytes()).await;
        assert_eq!(
            rejected.result.unwrap_err().code,
            SandboxErrorCode::InvalidRequest
        );
        let mut unsupported: serde_json::Value = serde_json::from_slice(&envelope(
            "correct opaque daemon token",
            SandboxOperation::Health,
        ))
        .unwrap();
        unsupported["version"] = (SANDBOX_PROTOCOL_VERSION + 1).into();
        let rejected = daemon
            .handle_line(&serde_json::to_vec(&unsupported).unwrap())
            .await;
        assert_eq!(
            rejected.result.unwrap_err().code,
            SandboxErrorCode::UnsupportedVersion
        );
    }

    #[tokio::test]
    async fn unix_connection_exchanges_newline_delimited_responses() {
        let root = TestDirectory::new();
        let daemon = daemon(
            &root.0,
            NetworkPolicyConfig {
                restricted_network: Some("gobrowse-restricted-test".into()),
                allow_full: false,
            },
            HARD_RESOURCE_LIMITS,
            Arc::new(MockRuntime::default()),
        );
        let (mut client, server) = UnixStream::pair().unwrap();
        let task = tokio::spawn(async move { daemon.handle_connection(server).await });
        let first = envelope("correct opaque daemon token", SandboxOperation::Health);
        let second = envelope("correct opaque daemon token", SandboxOperation::Health);
        client.write_all(&first).await.unwrap();
        client.write_all(b"\n").await.unwrap();
        client.write_all(&second).await.unwrap();
        client.write_all(b"\n").await.unwrap();

        let mut reader = BufReader::new(client);
        for _ in 0..2 {
            let mut response = String::new();
            reader.read_line(&mut response).await.unwrap();
            assert!(response.ends_with('\n'));
            let response: ResponseEnvelope = serde_json::from_str(&response).unwrap();
            assert!(matches!(response.result, Ok(SandboxResult::Health { .. })));
        }
        drop(reader);
        task.await.unwrap().unwrap();
    }

    #[tokio::test]
    async fn pre_auth_and_authenticated_idle_deadlines_close_connections() {
        let root = TestDirectory::new();
        let daemon = with_connections(
            daemon(
                &root.0,
                NetworkPolicyConfig {
                    restricted_network: Some("gobrowse-restricted-test".into()),
                    allow_full: false,
                },
                HARD_RESOURCE_LIMITS,
                Arc::new(MockRuntime::default()),
            ),
            ConnectionConfig {
                max_connections: 2,
                pre_auth_timeout: Duration::from_millis(25),
                idle_timeout: Duration::from_millis(25),
                operation_timeout: Duration::from_secs(1),
            },
        );

        let (_client, server) = UnixStream::pair().unwrap();
        let pre_auth = {
            let daemon = daemon.clone();
            tokio::spawn(async move { daemon.handle_connection(server).await })
        };
        assert!(matches!(
            pre_auth.await.unwrap(),
            Err(DaemonError::ConnectionTimeout)
        ));

        let (mut client, server) = UnixStream::pair().unwrap();
        let idle = {
            let daemon = daemon.clone();
            tokio::spawn(async move { daemon.handle_connection(server).await })
        };
        client
            .write_all(&envelope(
                "correct opaque daemon token",
                SandboxOperation::Health,
            ))
            .await
            .unwrap();
        client.write_all(b"\n").await.unwrap();
        let mut reader = BufReader::new(client);
        let mut response = String::new();
        reader.read_line(&mut response).await.unwrap();
        assert!(matches!(
            idle.await.unwrap(),
            Err(DaemonError::ConnectionTimeout)
        ));
    }

    #[tokio::test]
    async fn connection_semaphore_defers_second_peer_until_capacity_releases() {
        let root = TestDirectory::new();
        let daemon = with_connections(
            daemon(
                &root.0,
                NetworkPolicyConfig {
                    restricted_network: Some("gobrowse-restricted-test".into()),
                    allow_full: false,
                },
                HARD_RESOURCE_LIMITS,
                Arc::new(MockRuntime::default()),
            ),
            ConnectionConfig {
                max_connections: 1,
                pre_auth_timeout: Duration::from_millis(150),
                idle_timeout: Duration::from_secs(1),
                operation_timeout: Duration::from_secs(1),
            },
        );
        let socket = daemon.config.socket.path.clone();
        let (shutdown_send, shutdown_receive) = tokio::sync::oneshot::channel();
        let server = {
            let daemon = daemon.clone();
            tokio::spawn(async move {
                daemon
                    .serve_until(async {
                        let _ = shutdown_receive.await;
                        Ok(())
                    })
                    .await
            })
        };
        while !socket.exists() {
            tokio::task::yield_now().await;
        }
        let _first = UnixStream::connect(&socket).await.unwrap();
        tokio::time::timeout(Duration::from_secs(1), async {
            while daemon.connections.available_permits() != 0 {
                tokio::task::yield_now().await;
            }
        })
        .await
        .unwrap();

        let mut second = UnixStream::connect(&socket).await.unwrap();
        second
            .write_all(&envelope(
                "correct opaque daemon token",
                SandboxOperation::Health,
            ))
            .await
            .unwrap();
        second.write_all(b"\n").await.unwrap();
        let mut reader = BufReader::new(second);
        let mut response = String::new();
        assert!(
            tokio::time::timeout(Duration::from_millis(30), reader.read_line(&mut response))
                .await
                .is_err()
        );
        tokio::time::timeout(Duration::from_secs(1), reader.read_line(&mut response))
            .await
            .unwrap()
            .unwrap();
        assert!(!response.is_empty());
        let _ = shutdown_send.send(());
        server.await.unwrap().unwrap();
    }

    #[tokio::test]
    async fn operation_timeout_is_replayed_stably_and_releases_connection_permit() {
        let root = TestDirectory::new();
        let runtime = Arc::new(MockRuntime {
            starts: TokioMutex::new(Vec::new()),
            start_delay: Duration::from_millis(100),
        });
        let daemon = with_connections(
            daemon(
                &root.0,
                NetworkPolicyConfig {
                    restricted_network: Some("gobrowse-restricted-test".into()),
                    allow_full: false,
                },
                HARD_RESOURCE_LIMITS,
                Arc::clone(&runtime),
            ),
            ConnectionConfig {
                max_connections: 1,
                pre_auth_timeout: Duration::from_secs(1),
                idle_timeout: Duration::from_secs(1),
                operation_timeout: Duration::from_millis(25),
            },
        );
        let operation = start_request(NetworkPolicy::None, HARD_RESOURCE_LIMITS);
        provision_start_workspace(&daemon, &operation);
        let request = envelope_with_id(Uuid::new_v4(), "correct opaque daemon token", operation);
        let socket = daemon.config.socket.path.clone();
        let (shutdown_send, shutdown_receive) = tokio::sync::oneshot::channel();
        let server = {
            let daemon = daemon.clone();
            tokio::spawn(async move {
                daemon
                    .serve_until(async {
                        let _ = shutdown_receive.await;
                        Ok(())
                    })
                    .await
            })
        };
        while !socket.exists() {
            tokio::task::yield_now().await;
        }

        let mut first_stream = UnixStream::connect(&socket).await.unwrap();
        first_stream.write_all(&request).await.unwrap();
        first_stream.write_all(b"\n").await.unwrap();
        let mut first_reader = BufReader::new(first_stream);
        let mut first_encoded = String::new();
        first_reader.read_line(&mut first_encoded).await.unwrap();
        let first: ResponseEnvelope = serde_json::from_str(&first_encoded).unwrap();
        assert_eq!(
            first.result.as_ref().unwrap_err().code,
            SandboxErrorCode::DeadlineExceeded
        );
        let retry = tokio::time::timeout(Duration::from_secs(1), request_socket(&socket, &request))
            .await
            .unwrap();
        assert_eq!(first, retry);
        assert_eq!(runtime.starts.lock().await.len(), 1);

        let _ = shutdown_send.send(());
        server.await.unwrap().unwrap();
    }

    #[tokio::test]
    async fn canceled_request_execution_completes_replay_with_stable_timeout() {
        let root = TestDirectory::new();
        let runtime = Arc::new(MockRuntime {
            starts: TokioMutex::new(Vec::new()),
            start_delay: Duration::from_secs(1),
        });
        let daemon = daemon(
            &root.0,
            NetworkPolicyConfig {
                restricted_network: Some("gobrowse-restricted-test".into()),
                allow_full: false,
            },
            HARD_RESOURCE_LIMITS,
            Arc::clone(&runtime),
        );
        let operation = start_request(NetworkPolicy::None, HARD_RESOURCE_LIMITS);
        provision_start_workspace(&daemon, &operation);
        let request = envelope_with_id(Uuid::new_v4(), "correct opaque daemon token", operation);
        let executing = {
            let daemon = daemon.clone();
            let request = request.clone();
            tokio::spawn(async move { daemon.handle_line(&request).await })
        };
        tokio::time::timeout(Duration::from_secs(1), async {
            while runtime.starts.lock().await.is_empty() {
                tokio::task::yield_now().await;
            }
        })
        .await
        .unwrap();
        executing.abort();
        assert!(executing.await.unwrap_err().is_cancelled());

        let retry = daemon.handle_line(&request).await;
        assert_eq!(
            retry.result.unwrap_err().code,
            SandboxErrorCode::DeadlineExceeded
        );
        assert_eq!(runtime.starts.lock().await.len(), 1);
    }

    #[test]
    fn retryable_accept_errors_are_classified_without_masking_fatal_errors() {
        assert!(retryable_accept_error(&std::io::Error::from_raw_os_error(
            24
        )));
        assert!(retryable_accept_error(&std::io::Error::new(
            ErrorKind::Interrupted,
            "interrupted"
        )));
        assert!(!retryable_accept_error(&std::io::Error::new(
            ErrorKind::PermissionDenied,
            "denied"
        )));
        assert!(
            ConnectionConfig {
                max_connections: MAX_CONNECTIONS_HARD + 1,
                pre_auth_timeout: Duration::from_secs(1),
                idle_timeout: Duration::from_secs(1),
                operation_timeout: Duration::from_secs(1),
            }
            .validate()
            .is_err()
        );
    }

    #[tokio::test]
    async fn graceful_shutdown_removes_the_owned_socket() {
        let root = TestDirectory::new();
        let daemon = daemon(
            &root.0,
            NetworkPolicyConfig {
                restricted_network: Some("gobrowse-restricted-test".into()),
                allow_full: false,
            },
            HARD_RESOURCE_LIMITS,
            Arc::new(MockRuntime::default()),
        );
        let socket_path = daemon.config.socket.path.clone();
        daemon.serve_until(async { Ok(()) }).await.unwrap();
        assert!(!socket_path.exists());
    }

    #[tokio::test]
    async fn daemon_narrows_resource_limits_to_configured_policy() {
        let root = TestDirectory::new();
        let runtime = Arc::new(MockRuntime::default());
        let ceiling = ResourceLimits {
            cpu_millis: 500,
            memory_bytes: 128 * 1024 * 1024,
            writable_storage_bytes: 256 * 1024 * 1024,
            pids: 32,
            execution_seconds: 30,
        };
        let daemon = daemon(
            &root.0,
            NetworkPolicyConfig {
                restricted_network: Some("gobrowse-restricted-egress_proxy".into()),
                allow_full: false,
            },
            ceiling,
            Arc::clone(&runtime),
        );
        let operation = start_request(NetworkPolicy::Restricted, HARD_RESOURCE_LIMITS);
        provision_start_workspace(&daemon, &operation);
        let response = daemon
            .handle_line(&envelope("correct opaque daemon token", operation))
            .await;
        assert!(matches!(
            response.result,
            Ok(SandboxResult::Started { limits, .. }) if limits == ceiling
        ));
        let starts = runtime.starts.lock().await;
        assert_eq!(starts[0].request.limits, ceiling);
        assert_eq!(starts[0].network_name, "gobrowse-restricted-egress_proxy");
    }

    #[tokio::test]
    async fn start_requires_preprovisioned_workspace_storage() {
        let root = TestDirectory::new();
        let runtime = Arc::new(MockRuntime::default());
        let daemon = daemon(
            &root.0,
            NetworkPolicyConfig {
                restricted_network: Some("gobrowse-restricted-test".into()),
                allow_full: false,
            },
            HARD_RESOURCE_LIMITS,
            Arc::clone(&runtime),
        );
        let response = daemon
            .handle_line(&envelope(
                "correct opaque daemon token",
                start_request(NetworkPolicy::None, HARD_RESOURCE_LIMITS),
            ))
            .await;
        assert_eq!(
            response.result.unwrap_err().code,
            SandboxErrorCode::NotFound
        );
        assert!(runtime.starts.lock().await.is_empty());
    }

    #[tokio::test]
    async fn network_policy_requires_explicit_restricted_and_full_configuration() {
        let root = TestDirectory::new();
        let daemon = daemon(
            &root.0,
            NetworkPolicyConfig {
                restricted_network: None,
                allow_full: false,
            },
            HARD_RESOURCE_LIMITS,
            Arc::new(MockRuntime::default()),
        );
        for policy in [NetworkPolicy::Restricted, NetworkPolicy::Full] {
            let response = daemon
                .handle_line(&envelope(
                    "correct opaque daemon token",
                    start_request(policy, HARD_RESOURCE_LIMITS),
                ))
                .await;
            assert_eq!(
                response.result.unwrap_err().code,
                SandboxErrorCode::PolicyDenied
            );
        }
        assert_eq!(
            daemon.config.network.resolve(NetworkPolicy::None).unwrap(),
            "none"
        );
        assert_eq!(
            NetworkPolicyConfig {
                restricted_network: None,
                allow_full: true,
            }
            .resolve(NetworkPolicy::Full)
            .unwrap(),
            "slirp4netns"
        );
    }

    #[test]
    fn restricted_network_rejects_reserved_modes_and_requires_owned_prefix() {
        for name in [
            "host",
            "slirp4netns",
            "pasta",
            "container",
            "ns",
            "private",
            "bridge",
            "default",
            "none",
            "restricted-egress",
        ] {
            assert!(!valid_restricted_network_name(name), "{name}");
        }
        assert!(valid_restricted_network_name(
            "gobrowse-restricted-production"
        ));
    }

    #[tokio::test]
    async fn non_idempotent_retries_return_exact_response_and_conflicts_are_denied() {
        let root = TestDirectory::new();
        let runtime = Arc::new(MockRuntime::default());
        let daemon = daemon(
            &root.0,
            NetworkPolicyConfig {
                restricted_network: Some("gobrowse-restricted-test".into()),
                allow_full: false,
            },
            HARD_RESOURCE_LIMITS,
            Arc::clone(&runtime),
        );
        let request_id = Uuid::new_v4();
        let operation = start_request(NetworkPolicy::None, HARD_RESOURCE_LIMITS);
        provision_start_workspace(&daemon, &operation);
        let encoded =
            envelope_with_id(request_id, "correct opaque daemon token", operation.clone());
        let first = daemon.handle_line(&encoded).await;
        let retried = daemon.handle_line(&encoded).await;
        assert_eq!(first, retried);
        assert_eq!(runtime.starts.lock().await.len(), 1);

        let conflicting = daemon
            .handle_line(&envelope_with_id(
                request_id,
                "correct opaque daemon token",
                SandboxOperation::FsDelete {
                    workspace_id: Uuid::new_v4(),
                    path: "file".into(),
                },
            ))
            .await;
        assert_eq!(
            conflicting.result.unwrap_err().code,
            SandboxErrorCode::Conflict
        );
    }

    #[tokio::test]
    async fn concurrent_non_idempotent_retries_execute_once() {
        let root = TestDirectory::new();
        let runtime = Arc::new(MockRuntime {
            starts: TokioMutex::new(Vec::new()),
            start_delay: std::time::Duration::from_millis(25),
        });
        let daemon = daemon(
            &root.0,
            NetworkPolicyConfig {
                restricted_network: Some("gobrowse-restricted-test".into()),
                allow_full: false,
            },
            HARD_RESOURCE_LIMITS,
            Arc::clone(&runtime),
        );
        let operation = start_request(NetworkPolicy::None, HARD_RESOURCE_LIMITS);
        provision_start_workspace(&daemon, &operation);
        let encoded = envelope_with_id(Uuid::new_v4(), "correct opaque daemon token", operation);
        let (first, second) =
            tokio::join!(daemon.handle_line(&encoded), daemon.handle_line(&encoded));
        assert_eq!(first, second);
        assert_eq!(runtime.starts.lock().await.len(), 1);
    }

    #[test]
    fn authenticator_checks_digest_and_rejects_unbounded_tokens() {
        let auth = Authenticator::new("correct opaque daemon token").unwrap();
        assert!(auth.verify("correct opaque daemon token"));
        assert!(!auth.verify("correct opaque daemon tokeN"));
        assert!(!auth.verify(&"x".repeat(MAX_TOKEN_BYTES + 1)));
    }
}
