use std::{collections::HashMap, future::Future, io::ErrorKind, sync::Arc, time::Duration};

use base64::{Engine, engine::general_purpose::STANDARD as BASE64};
use gobrowse_core::sandbox::{
    HARD_RESOURCE_LIMITS, MAX_FILE_PAYLOAD_BYTES, MAX_PROTOCOL_LINE_BYTES,
    MAX_TERMINAL_INPUT_BYTES, MAX_TERMINAL_OUTPUT_READ_BYTES, MAX_TERMINAL_OUTPUT_WAIT_MS,
    NetworkPolicy, RequestEnvelope, ResourceLimits, ResponseEnvelope, SANDBOX_PROTOCOL_VERSION,
    SandboxErrorCode, SandboxOperation, SandboxProtocolError, SandboxResult,
    valid_restricted_network_name,
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
    Filesystem, FilesystemError, JournalError, MutationDecision, RuntimeError, SandboxRuntime,
    SocketConfig, TerminalJournal, ValidatedStart, WorkspacePause,
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
    #[error("a previous operation may have taken effect")]
    OutcomeUnknown,
    #[error("terminal journal failed")]
    Journal,
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
    workspace_locks: Arc<Mutex<HashMap<Uuid, Arc<Mutex<()>>>>>,
    runtime: Arc<dyn SandboxRuntime>,
    journal: TerminalJournal,
    replay: Arc<ReplayCache>,
    connections: Arc<Semaphore>,
}

struct ReplayExecutionGuard {
    cache: Arc<ReplayCache>,
    request_id: Uuid,
    fingerprint: [u8; 32],
    armed: bool,
}

struct WorkspaceResumeGuard {
    runtime: Arc<dyn SandboxRuntime>,
    pause: Option<WorkspacePause>,
}

impl WorkspaceResumeGuard {
    async fn resume(&mut self) -> Result<(), RuntimeError> {
        let pause = self
            .pause
            .as_ref()
            .ok_or(RuntimeError::WorkspaceResumeFailed)?;
        self.runtime.resume_workspace(pause).await?;
        self.pause = None;
        Ok(())
    }
}

impl Drop for WorkspaceResumeGuard {
    fn drop(&mut self) {
        if let Some(pause) = self.pause.take() {
            let runtime = Arc::clone(&self.runtime);
            tokio::spawn(async move {
                let _ = runtime.resume_workspace(&pause).await;
            });
        }
    }
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
        journal: TerminalJournal,
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
            workspace_locks: Arc::new(Mutex::new(HashMap::new())),
            runtime,
            journal,
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
        self.runtime.reconcile_recoveries().await?;
        let (listener, _socket_guard) = self.config.socket.bind().await?;
        tokio::pin!(shutdown);
        let result = loop {
            let permit = tokio::select! {
                result = &mut shutdown => break result,
                permit = Arc::clone(&self.connections).acquire_owned() => {
                    permit.map_err(|_| DaemonError::InvalidConfiguration)?
                }
            };
            let accepted = tokio::select! {
                result = &mut shutdown => break result,
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
                        result = &mut shutdown => break result,
                        () = tokio::time::sleep(ACCEPT_RETRY_DELAY) => {}
                    }
                }
                Err(error) => break Err(DaemonError::Io(error)),
            }
        };
        drop(listener);
        let _connections = Arc::clone(&self.connections)
            .acquire_many_owned(self.config.connections.max_connections as u32)
            .await
            .map_err(|_| DaemonError::InvalidConfiguration)?;
        self.runtime.shutdown().await?;
        result
    }

    pub async fn handle_line(&self, line: &[u8]) -> ResponseEnvelope {
        if line.is_empty()
            || line.len() >= MAX_PROTOCOL_LINE_BYTES
            || line.iter().any(|byte| matches!(byte, b'\r' | b'\n'))
        {
            return error_response(Uuid::nil(), DaemonError::InvalidRequest);
        }
        // Deserialize directly into the deny_unknown_fields protocol types. Going through Value
        // would silently collapse duplicate object keys before the strict structs can reject them.
        let request = match serde_json::from_slice::<RequestEnvelope>(line) {
            Ok(request) => request,
            Err(_) => return error_response(Uuid::nil(), DaemonError::InvalidRequest),
        };
        if !matches!(
            serde_json::from_slice::<serde_json::Value>(line),
            Ok(value) if valid_wire_shape(&value)
        ) {
            return error_response(Uuid::nil(), DaemonError::InvalidRequest);
        }
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
        if let Some((terminal_id, kind)) = terminal_mutation(&request.operation) {
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
                        let response = match self.journal.begin_mutation(
                            request_id,
                            terminal_id,
                            kind,
                            fingerprint,
                        ) {
                            Ok(MutationDecision::Cached(response)) => response,
                            Ok(MutationDecision::Execute) => {
                                let response =
                                    self.response_for(request_id, request.operation).await;
                                if self
                                    .journal
                                    .complete_mutation(request_id, &response)
                                    .is_err()
                                {
                                    error_response(request_id, DaemonError::OutcomeUnknown)
                                } else {
                                    response
                                }
                            }
                            Err(JournalError::Conflict) => {
                                error_response(request_id, DaemonError::Conflict)
                            }
                            Err(JournalError::OutcomeUnknown) => {
                                error_response(request_id, DaemonError::OutcomeUnknown)
                            }
                            Err(_) => error_response(request_id, DaemonError::Journal),
                        };
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
        if terminal_id(&operation).is_some_and(|terminal_id| terminal_id.is_nil()) {
            return Err(DaemonError::InvalidRequest);
        }
        match operation {
            SandboxOperation::Health => Ok(SandboxResult::Health {
                status: "ok".into(),
            }),
            SandboxOperation::ProvisionWorkspace { workspace_id } => {
                if workspace_id.is_nil() {
                    return Err(DaemonError::InvalidRequest);
                }
                self.filesystem.ensure_workspace(workspace_id)?;
                self.runtime.provision_workspace(workspace_id).await?;
                Ok(SandboxResult::Provisioned { workspace_id })
            }
            SandboxOperation::Start {
                terminal_id,
                mut request,
            } => {
                request
                    .validate()
                    .map_err(|_| DaemonError::InvalidRequest)?;
                request.limits = request.limits.narrow_to(self.config.resource_ceiling);
                request.limits = request.limits.narrow_to(HARD_RESOURCE_LIMITS);
                let network_name = self.config.network.resolve(request.network_policy)?;
                // Storage must already exist under the trusted root before it can acquire a
                // lifecycle lock; this keeps the lock map bounded by provisioned workspaces.
                self.filesystem.workspace_storage(request.workspace_id)?;
                let workspace_lock = self.workspace_lock(request.workspace_id).await;
                let _workspace_guard = workspace_lock.lock().await;
                self.filesystem
                    .require_directory(request.workspace_id, &request.working_directory)?;
                let workspace_storage = self.filesystem.workspace_storage(request.workspace_id)?;
                let limits = request.limits;
                let network_policy = request.network_policy;
                self.runtime
                    .start(ValidatedStart {
                        terminal_id,
                        request,
                        workspace_storage,
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
                input_id,
                data_base64,
            } => {
                if input_id.is_nil() {
                    return Err(DaemonError::InvalidRequest);
                }
                let bytes = decode_bounded(&data_base64, MAX_TERMINAL_INPUT_BYTES)?;
                let outcome = self.runtime.input(terminal_id, input_id, &bytes).await?;
                Ok(SandboxResult::InputAccepted {
                    input_id,
                    bytes: outcome.bytes,
                    replayed: outcome.replayed,
                })
            }
            SandboxOperation::ReadOutput {
                terminal_id,
                after_cursor,
                max_bytes,
                wait_ms,
            } => {
                let max_bytes =
                    usize::try_from(max_bytes).map_err(|_| DaemonError::LimitExceeded)?;
                if max_bytes == 0
                    || max_bytes > MAX_TERMINAL_OUTPUT_READ_BYTES
                    || wait_ms > MAX_TERMINAL_OUTPUT_WAIT_MS
                {
                    return Err(DaemonError::LimitExceeded);
                }
                let output = self
                    .runtime
                    .read_output(
                        terminal_id,
                        after_cursor,
                        max_bytes,
                        Duration::from_millis(u64::from(wait_ms)),
                    )
                    .await?;
                Ok(SandboxResult::Output {
                    terminal_id,
                    start_cursor: output.start_cursor,
                    next_cursor: output.next_cursor,
                    data_base64: BASE64.encode(output.bytes),
                    state: output.record.state,
                    output_complete: output.record.output_complete,
                })
            }
            SandboxOperation::AckOutput {
                terminal_id,
                cursor,
            } => {
                let cursor = self.runtime.ack_output(terminal_id, cursor).await?;
                Ok(SandboxResult::OutputAcked { cursor })
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
            SandboxOperation::Interrupt { terminal_id } => {
                self.runtime.interrupt(terminal_id).await?;
                Ok(SandboxResult::Interrupted)
            }
            SandboxOperation::Processes { terminal_id } => Ok(SandboxResult::Processes {
                processes: self.runtime.processes(terminal_id).await?,
            }),
            SandboxOperation::Kill { terminal_id, pid } => {
                if pid == 0 {
                    return Err(DaemonError::InvalidRequest);
                }
                self.runtime.kill(terminal_id, pid).await?;
                Ok(SandboxResult::Killed { pid })
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
                    cols: inspected.cols,
                    rows: inspected.rows,
                    exit_code: inspected.exit_code,
                    reason: inspected.reason,
                    output_start_cursor: inspected.output_start_cursor,
                    output_end_cursor: inspected.output_end_cursor,
                    acked_cursor: inspected.acked_cursor,
                    output_complete: inspected.output_complete,
                })
            }
            SandboxOperation::Reconnect { terminal_id } => {
                let inspected = self.runtime.inspect(terminal_id).await?;
                Ok(SandboxResult::Reconnected {
                    terminal_id: inspected.terminal_id,
                    workspace_id: inspected.workspace_id,
                    state: inspected.state,
                    cols: inspected.cols,
                    rows: inspected.rows,
                    exit_code: inspected.exit_code,
                    reason: inspected.reason,
                    output_start_cursor: inspected.output_start_cursor,
                    output_end_cursor: inspected.output_end_cursor,
                    acked_cursor: inspected.acked_cursor,
                    output_complete: inspected.output_complete,
                })
            }
            SandboxOperation::FsList { workspace_id, path } => Ok(SandboxResult::FsList {
                entries: self.filesystem.list(workspace_id, &path)?,
            }),
            SandboxOperation::FsRead { workspace_id, path } => {
                let (bytes, sha256) = self.filesystem.read(workspace_id, &path)?;
                Ok(SandboxResult::FsRead {
                    data_base64: BASE64.encode(bytes),
                    sha256,
                })
            }
            SandboxOperation::FsMetadata { workspace_id, path } => Ok(SandboxResult::FsMetadata {
                metadata: self.filesystem.metadata(workspace_id, &path)?,
            }),
            SandboxOperation::FsSearch {
                workspace_id,
                path,
                query,
            } => Ok(SandboxResult::FsSearch {
                matches: self.filesystem.search(workspace_id, &path, &query)?,
            }),
            SandboxOperation::FsWrite {
                workspace_id,
                path,
                data_base64,
            } => {
                let bytes = decode_bounded(&data_base64, MAX_FILE_PAYLOAD_BYTES)?;
                let parent = Self::parent_of(&path).to_owned();
                let sha256 = self
                    .mutate_workspace(workspace_id, Some((parent, false)), None, || {
                        self.filesystem.write(workspace_id, &path, &bytes)
                    })
                    .await?;
                Ok(SandboxResult::FsWritten {
                    bytes: bytes.len(),
                    sha256,
                })
            }
            SandboxOperation::FsPatch {
                workspace_id,
                path,
                expected_sha256,
                data_base64,
            } => {
                let bytes = decode_bounded(&data_base64, MAX_FILE_PAYLOAD_BYTES)?;
                let parent = Self::parent_of(&path).to_owned();
                let sha256 = self
                    .mutate_workspace(workspace_id, Some((parent, false)), None, || {
                        self.filesystem
                            .patch(workspace_id, &path, &expected_sha256, &bytes)
                    })
                    .await?;
                Ok(SandboxResult::FsPatched {
                    bytes: bytes.len(),
                    sha256,
                })
            }
            SandboxOperation::FsMkdir { workspace_id, path } => {
                let parent = Self::parent_of(&path).to_owned();
                self.mutate_workspace(
                    workspace_id,
                    Some((parent, false)),
                    Some((path.clone(), false)),
                    || self.filesystem.mkdir(workspace_id, &path),
                )
                .await?;
                Ok(SandboxResult::FsCreated)
            }
            SandboxOperation::FsMove {
                workspace_id,
                from,
                to,
            } => {
                let parent = Self::parent_of(&to).to_owned();
                self.mutate_workspace(
                    workspace_id,
                    Some((parent, false)),
                    Some((to.clone(), false)),
                    || self.filesystem.move_entry(workspace_id, &from, &to),
                )
                .await?;
                Ok(SandboxResult::FsMoved)
            }
            SandboxOperation::FsCopy {
                workspace_id,
                from,
                to,
            } => {
                let parent = Self::parent_of(&to).to_owned();
                self.mutate_workspace(
                    workspace_id,
                    Some((parent, false)),
                    Some((to.clone(), true)),
                    || self.filesystem.copy(workspace_id, &from, &to),
                )
                .await?;
                Ok(SandboxResult::FsCopied)
            }
            SandboxOperation::FsDelete { workspace_id, path } => {
                let parent = Self::parent_of(&path).to_owned();
                self.mutate_workspace(workspace_id, Some((parent, false)), None, || {
                    self.filesystem.delete(workspace_id, &path)
                })
                .await?;
                Ok(SandboxResult::FsDeleted)
            }
        }
    }

    async fn workspace_lock(&self, workspace_id: Uuid) -> Arc<Mutex<()>> {
        Arc::clone(
            self.workspace_locks
                .lock()
                .await
                .entry(workspace_id)
                .or_insert_with(|| Arc::new(Mutex::new(()))),
        )
    }

    /// Workspace-relative parent of a path ("." when the path has no parent).
    fn parent_of(path: &str) -> &str {
        match path.rsplit_once('/') {
            Some((parent, _)) if !parent.is_empty() => parent,
            _ => ".",
        }
    }

    async fn mutate_workspace<T>(
        &self,
        workspace_id: Uuid,
        pre_chown: Option<(String, bool)>,
        post_chown: Option<(String, bool)>,
        operation: impl FnOnce() -> Result<T, FilesystemError>,
    ) -> Result<T, DaemonError> {
        self.filesystem.require_directory(workspace_id, ".")?;
        let workspace_lock = self.workspace_lock(workspace_id).await;
        let _workspace_guard = workspace_lock.lock().await;
        let pause = self.runtime.pause_workspace(workspace_id).await?;
        // Pre-op ownership fix (e.g. writing into a container-created dir).
        // Best-effort: the filesystem op is authoritative; a volume-less
        // workspace still operates.
        if let Some((path, recursive)) = &pre_chown {
            let _ = self
                .runtime
                .chown_workspace_path(workspace_id, path, *recursive)
                .await;
        }
        let mut resume = WorkspaceResumeGuard {
            runtime: Arc::clone(&self.runtime),
            pause: Some(pause),
        };
        let result = operation().map_err(DaemonError::Filesystem);
        if matches!(
            &result,
            Err(DaemonError::Filesystem(FilesystemError::ContainmentFailure))
        ) {
            let pause = resume
                .pause
                .take()
                .ok_or(RuntimeError::WorkspaceResumeFailed)?;
            for terminal_id in pause.terminal_ids {
                let _ = self.runtime.terminate(terminal_id).await;
            }
            return result;
        }
        // Post-op ownership fix (e.g. a new directory both sides must use) while
        // the workspace is still paused so no container write races it.
        // Best-effort, mirrors the pre-op fix.
        if let Some((path, recursive)) = &post_chown {
            let _ = self
                .runtime
                .chown_workspace_path(workspace_id, path, *recursive)
                .await;
        }
        let resumed = resume.resume().await;
        match (result, resumed) {
            (_, Err(error)) => Err(DaemonError::Runtime(error)),
            (result, Ok(())) => result,
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

fn is_non_idempotent(operation: &SandboxOperation) -> bool {
    matches!(
        operation,
        SandboxOperation::Start { .. }
            | SandboxOperation::Input { .. }
            | SandboxOperation::Resize { .. }
            | SandboxOperation::Terminate { .. }
            | SandboxOperation::FsWrite { .. }
            | SandboxOperation::FsPatch { .. }
            | SandboxOperation::FsMkdir { .. }
            | SandboxOperation::FsMove { .. }
            | SandboxOperation::FsCopy { .. }
            | SandboxOperation::FsDelete { .. }
    )
}

fn terminal_mutation(operation: &SandboxOperation) -> Option<(Uuid, &'static str)> {
    match operation {
        SandboxOperation::Start { terminal_id, .. } => Some((*terminal_id, "start")),
        SandboxOperation::Input { terminal_id, .. } => Some((*terminal_id, "input")),
        SandboxOperation::AckOutput { terminal_id, .. } => Some((*terminal_id, "ack_output")),
        SandboxOperation::Resize { terminal_id, .. } => Some((*terminal_id, "resize")),
        SandboxOperation::Interrupt { terminal_id } => Some((*terminal_id, "interrupt")),
        SandboxOperation::Kill { terminal_id, .. } => Some((*terminal_id, "kill")),
        SandboxOperation::Terminate { terminal_id } => Some((*terminal_id, "terminate")),
        _ => None,
    }
}

fn terminal_id(operation: &SandboxOperation) -> Option<Uuid> {
    match operation {
        SandboxOperation::Start { terminal_id, .. }
        | SandboxOperation::Input { terminal_id, .. }
        | SandboxOperation::ReadOutput { terminal_id, .. }
        | SandboxOperation::AckOutput { terminal_id, .. }
        | SandboxOperation::Resize { terminal_id, .. }
        | SandboxOperation::Interrupt { terminal_id }
        | SandboxOperation::Processes { terminal_id }
        | SandboxOperation::Kill { terminal_id, .. }
        | SandboxOperation::Terminate { terminal_id }
        | SandboxOperation::Inspect { terminal_id }
        | SandboxOperation::Reconnect { terminal_id } => Some(*terminal_id),
        SandboxOperation::Health
        | SandboxOperation::ProvisionWorkspace { .. }
        | SandboxOperation::FsList { .. }
        | SandboxOperation::FsRead { .. }
        | SandboxOperation::FsMetadata { .. }
        | SandboxOperation::FsSearch { .. }
        | SandboxOperation::FsWrite { .. }
        | SandboxOperation::FsPatch { .. }
        | SandboxOperation::FsMkdir { .. }
        | SandboxOperation::FsMove { .. }
        | SandboxOperation::FsCopy { .. }
        | SandboxOperation::FsDelete { .. } => None,
    }
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
        "provision_workspace" => &["op", "workspace_id"],
        "start" => &["op", "terminal_id", "request"],
        "input" => &["op", "terminal_id", "input_id", "data_base64"],
        "read_output" => &["op", "terminal_id", "after_cursor", "max_bytes", "wait_ms"],
        "ack_output" => &["op", "terminal_id", "cursor"],
        "resize" => &["op", "terminal_id", "cols", "rows"],
        "interrupt" | "processes" | "terminate" | "inspect" | "reconnect" => &["op", "terminal_id"],
        "kill" => &["op", "terminal_id", "pid"],
        "fs_list" | "fs_read" | "fs_metadata" | "fs_mkdir" | "fs_delete" => {
            &["op", "workspace_id", "path"]
        }
        "fs_search" => &["op", "workspace_id", "path", "query"],
        "fs_write" => &["op", "workspace_id", "path", "data_base64"],
        "fs_patch" => &[
            "op",
            "workspace_id",
            "path",
            "expected_sha256",
            "data_base64",
        ],
        "fs_move" | "fs_copy" => &["op", "workspace_id", "from", "to"],
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
        DaemonError::OutcomeUnknown | DaemonError::Runtime(RuntimeError::OutcomeUnknown) => (
            SandboxErrorCode::OutcomeUnknown,
            "operation outcome is unknown",
        ),
        DaemonError::Runtime(RuntimeError::Conflict) => {
            (SandboxErrorCode::Conflict, "object conflict")
        }
        DaemonError::InvalidRequest | DaemonError::Filesystem(FilesystemError::InvalidPath) => {
            (SandboxErrorCode::InvalidRequest, "invalid request")
        }
        DaemonError::InvalidConfiguration
        | DaemonError::ConnectionTimeout
        | DaemonError::Journal
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
        chowns: TokioMutex<Vec<(Uuid, String, bool)>>,
        start_delay: std::time::Duration,
    }

    #[async_trait]
    impl SandboxRuntime for MockRuntime {
        async fn start(&self, start: ValidatedStart) -> Result<(), RuntimeError> {
            self.starts.lock().await.push(start);
            tokio::time::sleep(self.start_delay).await;
            Ok(())
        }

        async fn provision_workspace(&self, _workspace_id: Uuid) -> Result<(), RuntimeError> {
            Ok(())
        }

        async fn input(
            &self,
            _terminal_id: Uuid,
            _input_id: Uuid,
            bytes: &[u8],
        ) -> Result<crate::InputOutcome, RuntimeError> {
            Ok(crate::InputOutcome {
                bytes: bytes.len(),
                replayed: false,
            })
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
                cols: 80,
                rows: 24,
                exit_code: None,
                reason: None,
                output_start_cursor: 0,
                output_end_cursor: 0,
                acked_cursor: 0,
                output_complete: true,
            })
        }

        async fn pause_workspace(
            &self,
            workspace_id: Uuid,
        ) -> Result<WorkspacePause, RuntimeError> {
            Ok(WorkspacePause {
                workspace_id,
                terminal_ids: Vec::new(),
                recovery_id: None,
            })
        }

        async fn resume_workspace(&self, _pause: &WorkspacePause) -> Result<(), RuntimeError> {
            Ok(())
        }

        async fn reconcile_recoveries(&self) -> Result<(), RuntimeError> {
            Ok(())
        }

        async fn ensure_workspace_recovered(
            &self,
            _workspace_id: Uuid,
        ) -> Result<(), RuntimeError> {
            Ok(())
        }

        async fn chown_workspace_path(
            &self,
            workspace_id: Uuid,
            relative: &str,
            recursive: bool,
        ) -> Result<(), RuntimeError> {
            self.chowns
                .lock()
                .await
                .push((workspace_id, relative.to_owned(), recursive));
            Ok(())
        }
    }

    struct LifecycleMockRuntime {
        events: TokioMutex<Vec<&'static str>>,
        active: TokioMutex<bool>,
        fail_pause: bool,
        start_delay: Duration,
    }

    impl LifecycleMockRuntime {
        fn new(active: bool, fail_pause: bool, start_delay: Duration) -> Self {
            Self {
                events: TokioMutex::new(Vec::new()),
                active: TokioMutex::new(active),
                fail_pause,
                start_delay,
            }
        }
    }

    #[async_trait]
    impl SandboxRuntime for LifecycleMockRuntime {
        async fn start(&self, _start: ValidatedStart) -> Result<(), RuntimeError> {
            self.events.lock().await.push("start-begin");
            tokio::time::sleep(self.start_delay).await;
            *self.active.lock().await = true;
            self.events.lock().await.push("start-end");
            Ok(())
        }

        async fn input(
            &self,
            _terminal_id: Uuid,
            _input_id: Uuid,
            bytes: &[u8],
        ) -> Result<crate::InputOutcome, RuntimeError> {
            Ok(crate::InputOutcome {
                bytes: bytes.len(),
                replayed: false,
            })
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
                cols: 80,
                rows: 24,
                exit_code: None,
                reason: None,
                output_start_cursor: 0,
                output_end_cursor: 0,
                acked_cursor: 0,
                output_complete: true,
            })
        }

        async fn pause_workspace(
            &self,
            workspace_id: Uuid,
        ) -> Result<WorkspacePause, RuntimeError> {
            self.events.lock().await.push("pause");
            if self.fail_pause && *self.active.lock().await {
                return Err(RuntimeError::WorkspacePauseFailed);
            }
            Ok(WorkspacePause {
                workspace_id,
                terminal_ids: Vec::new(),
                recovery_id: None,
            })
        }

        async fn resume_workspace(&self, _pause: &WorkspacePause) -> Result<(), RuntimeError> {
            self.events.lock().await.push("resume");
            Ok(())
        }

        async fn reconcile_recoveries(&self) -> Result<(), RuntimeError> {
            Ok(())
        }

        async fn ensure_workspace_recovered(
            &self,
            _workspace_id: Uuid,
        ) -> Result<(), RuntimeError> {
            Ok(())
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

    fn daemon<R: SandboxRuntime + 'static>(
        root: &Path,
        network: NetworkPolicyConfig,
        ceiling: ResourceLimits,
        runtime: Arc<R>,
    ) -> Daemon {
        let uid = effective_uid_from_proc_status(&fs::read_to_string("/proc/self/status").unwrap())
            .unwrap();
        fs::create_dir(root.join("workspaces")).unwrap();
        // Filesystem::new requires a private workspace root; pin the mode so the
        // tests are immune to the ambient umask.
        fs::set_permissions(root.join("workspaces"), fs::Permissions::from_mode(0o700)).unwrap();
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
            TerminalJournal::open(root.join("terminals.sqlite3")).unwrap(),
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
            terminal_id: Uuid::new_v4(),
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
        let SandboxOperation::Start { request, .. } = operation else {
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
        let duplicate_token = format!(
            r#"{{"version":{},"request_id":"{}","token":"correct opaque daemon token","token":"correct opaque daemon token","operation":{{"op":"health"}}}}"#,
            SANDBOX_PROTOCOL_VERSION,
            Uuid::new_v4()
        );
        let rejected = daemon.handle_line(duplicate_token.as_bytes()).await;
        assert_eq!(rejected.request_id, Uuid::nil());
        assert_eq!(
            rejected.result.unwrap_err().code,
            SandboxErrorCode::InvalidRequest
        );
        let mut embedded_newline =
            envelope("correct opaque daemon token", SandboxOperation::Health);
        embedded_newline.push(b'\n');
        assert_eq!(
            daemon
                .handle_line(&embedded_newline)
                .await
                .result
                .unwrap_err()
                .code,
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
    async fn terminal_protocol_rejects_nil_ids_and_operation_bounds_before_runtime_use() {
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
        for operation in [
            SandboxOperation::ReadOutput {
                terminal_id: Uuid::nil(),
                after_cursor: 0,
                max_bytes: 1,
                wait_ms: 0,
            },
            SandboxOperation::AckOutput {
                terminal_id: Uuid::nil(),
                cursor: 0,
            },
            SandboxOperation::Interrupt {
                terminal_id: Uuid::nil(),
            },
            SandboxOperation::Processes {
                terminal_id: Uuid::nil(),
            },
            SandboxOperation::Inspect {
                terminal_id: Uuid::nil(),
            },
            SandboxOperation::Reconnect {
                terminal_id: Uuid::nil(),
            },
        ] {
            let response = daemon
                .handle_line(&envelope("correct opaque daemon token", operation))
                .await;
            assert_eq!(
                response.result.unwrap_err().code,
                SandboxErrorCode::InvalidRequest
            );
        }

        let terminal_id = Uuid::new_v4();
        for operation in [
            SandboxOperation::ReadOutput {
                terminal_id,
                after_cursor: 0,
                max_bytes: MAX_TERMINAL_OUTPUT_READ_BYTES as u32 + 1,
                wait_ms: 0,
            },
            SandboxOperation::ReadOutput {
                terminal_id,
                after_cursor: 0,
                max_bytes: 1,
                wait_ms: MAX_TERMINAL_OUTPUT_WAIT_MS + 1,
            },
        ] {
            let response = daemon
                .handle_line(&envelope("correct opaque daemon token", operation))
                .await;
            assert_eq!(
                response.result.unwrap_err().code,
                SandboxErrorCode::LimitExceeded
            );
        }
        let invalid_pid = daemon
            .handle_line(&envelope(
                "correct opaque daemon token",
                SandboxOperation::Kill {
                    terminal_id,
                    pid: 0,
                },
            ))
            .await;
        assert_eq!(
            invalid_pid.result.unwrap_err().code,
            SandboxErrorCode::InvalidRequest
        );
        let invalid_input_id = daemon
            .handle_line(&envelope(
                "correct opaque daemon token",
                SandboxOperation::Input {
                    terminal_id,
                    input_id: Uuid::nil(),
                    data_base64: String::new(),
                },
            ))
            .await;
        assert_eq!(
            invalid_input_id.result.unwrap_err().code,
            SandboxErrorCode::InvalidRequest
        );
        let oversized_input = daemon
            .handle_line(&envelope(
                "correct opaque daemon token",
                SandboxOperation::Input {
                    terminal_id,
                    input_id: Uuid::new_v4(),
                    data_base64: "A".repeat(MAX_TERMINAL_INPUT_BYTES.div_ceil(3) * 4 + 1),
                },
            ))
            .await;
        assert_eq!(
            oversized_input.result.unwrap_err().code,
            SandboxErrorCode::LimitExceeded
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
    async fn socket_framing_rejects_crlf_and_oversized_frames_then_closes() {
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
        let task = {
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
        client.write_all(b"\r\n").await.unwrap();
        let mut reader = BufReader::new(client);
        let mut encoded = String::new();
        reader.read_line(&mut encoded).await.unwrap();
        let response: ResponseEnvelope = serde_json::from_str(&encoded).unwrap();
        assert_eq!(
            response.result.unwrap_err().code,
            SandboxErrorCode::InvalidRequest
        );
        assert_eq!(reader.read_line(&mut encoded).await.unwrap(), 0);
        task.await.unwrap().unwrap();

        let (mut client, server) = UnixStream::pair().unwrap();
        let task = tokio::spawn(async move { daemon.handle_connection(server).await });
        client
            .write_all(&vec![b' '; MAX_PROTOCOL_LINE_BYTES])
            .await
            .unwrap();
        client.write_all(b"\n").await.unwrap();
        let mut reader = BufReader::new(client);
        let mut encoded = String::new();
        reader.read_line(&mut encoded).await.unwrap();
        let response: ResponseEnvelope = serde_json::from_str(&encoded).unwrap();
        assert_eq!(
            response.result.unwrap_err().code,
            SandboxErrorCode::LimitExceeded
        );
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
        let first = UnixStream::connect(&socket).await.unwrap();
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
        drop(first);
        let _ = shutdown_send.send(());
        server.await.unwrap().unwrap();
    }

    #[tokio::test]
    async fn operation_timeout_is_replayed_stably_and_releases_connection_permit() {
        let root = TestDirectory::new();
        let runtime = Arc::new(MockRuntime {
            starts: TokioMutex::new(Vec::new()),
            chowns: TokioMutex::new(Vec::new()),
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
            chowns: TokioMutex::new(Vec::new()),
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
    async fn provision_workspace_provisions_storage_and_is_idempotent() {
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
        let workspace_id = Uuid::new_v4();
        for _ in 0..2 {
            let response = daemon
                .handle_line(&envelope(
                    "correct opaque daemon token",
                    SandboxOperation::ProvisionWorkspace { workspace_id },
                ))
                .await;
            assert!(matches!(
                response.result,
                Ok(SandboxResult::Provisioned { workspace_id: echoed })
                    if echoed == workspace_id
            ));
        }
        // Provisioning made the workspace volume attestable, which is what
        // `Start` requires before it will acquire a lifecycle lock.
        assert_eq!(
            daemon
                .filesystem
                .workspace_storage(workspace_id)
                .unwrap()
                .volume_name,
            gobrowse_core::sandbox::workspace_volume_name(workspace_id)
        );
    }

    #[tokio::test]
    async fn provision_workspace_rejects_nil_workspace() {
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
        let response = daemon
            .handle_line(&envelope(
                "correct opaque daemon token",
                SandboxOperation::ProvisionWorkspace {
                    workspace_id: Uuid::nil(),
                },
            ))
            .await;
        assert_eq!(
            response.result.unwrap_err().code,
            SandboxErrorCode::InvalidRequest
        );
    }

    #[tokio::test]
    async fn provision_workspace_rejects_unknown_wire_fields() {
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
        let request = format!(
            r#"{{"version":2,"request_id":"{}","token":"correct opaque daemon token","operation":{{"op":"provision_workspace","workspace_id":"{}","extra":true}}}}"#,
            Uuid::new_v4(),
            Uuid::new_v4()
        );
        let response = daemon.handle_line(request.as_bytes()).await;
        assert_eq!(
            response.result.unwrap_err().code,
            SandboxErrorCode::InvalidRequest
        );
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
    async fn conditional_patch_replay_returns_the_original_success_without_reexecution() {
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
        let workspace_id = Uuid::new_v4();
        daemon.filesystem.ensure_workspace(workspace_id).unwrap();
        let expected = daemon
            .filesystem
            .write(workspace_id, "file", b"old")
            .unwrap();
        let request_id = Uuid::new_v4();
        let encoded = envelope_with_id(
            request_id,
            "correct opaque daemon token",
            SandboxOperation::FsPatch {
                workspace_id,
                path: "file".into(),
                expected_sha256: expected,
                data_base64: BASE64.encode(b"new"),
            },
        );

        let first = daemon.handle_line(&encoded).await;
        let replayed = daemon.handle_line(&encoded).await;
        assert_eq!(first, replayed);
        assert!(matches!(first.result, Ok(SandboxResult::FsPatched { .. })));
        assert_eq!(
            daemon.filesystem.read(workspace_id, "file").unwrap().0,
            b"new"
        );

        let conflicting = daemon
            .handle_line(&envelope_with_id(
                request_id,
                "correct opaque daemon token",
                SandboxOperation::FsCopy {
                    workspace_id,
                    from: "file".into(),
                    to: "copy".into(),
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
            chowns: TokioMutex::new(Vec::new()),
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

    #[tokio::test]
    async fn workspace_start_and_mutation_are_lifecycle_ordered() {
        let root = TestDirectory::new();
        let runtime = Arc::new(LifecycleMockRuntime::new(
            false,
            false,
            Duration::from_millis(50),
        ));
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
        let SandboxOperation::Start { request, .. } = &operation else {
            unreachable!();
        };
        let workspace_id = request.workspace_id;
        provision_start_workspace(&daemon, &operation);
        let starting = {
            let daemon = daemon.clone();
            let encoded = envelope("correct opaque daemon token", operation);
            tokio::spawn(async move { daemon.handle_line(&encoded).await })
        };
        tokio::time::timeout(Duration::from_secs(1), async {
            while runtime.events.lock().await.as_slice() != ["start-begin"] {
                tokio::task::yield_now().await;
            }
        })
        .await
        .unwrap();
        let mutating = {
            let daemon = daemon.clone();
            tokio::spawn(async move {
                daemon
                    .handle_line(&envelope(
                        "correct opaque daemon token",
                        SandboxOperation::FsWrite {
                            workspace_id,
                            path: "ordered".into(),
                            data_base64: BASE64.encode(b"data"),
                        },
                    ))
                    .await
            })
        };
        tokio::time::sleep(Duration::from_millis(10)).await;
        assert_eq!(runtime.events.lock().await.as_slice(), ["start-begin"]);
        assert!(starting.await.unwrap().result.is_ok());
        assert!(mutating.await.unwrap().result.is_ok());
        assert_eq!(
            runtime.events.lock().await.as_slice(),
            ["start-begin", "start-end", "pause", "resume"]
        );
    }

    #[tokio::test]
    async fn workspace_mutations_pause_and_pause_failure_prevents_changes() {
        let root = TestDirectory::new();
        let runtime = Arc::new(LifecycleMockRuntime::new(true, false, Duration::ZERO));
        let active_daemon = daemon(
            &root.0,
            NetworkPolicyConfig {
                restricted_network: Some("gobrowse-restricted-test".into()),
                allow_full: false,
            },
            HARD_RESOURCE_LIMITS,
            Arc::clone(&runtime),
        );
        let workspace_id = Uuid::new_v4();
        active_daemon
            .filesystem
            .ensure_workspace(workspace_id)
            .unwrap();
        let expected = active_daemon
            .filesystem
            .write(workspace_id, "file", b"old")
            .unwrap();
        for operation in [
            SandboxOperation::FsPatch {
                workspace_id,
                path: "file".into(),
                expected_sha256: expected,
                data_base64: BASE64.encode(b"new"),
            },
            SandboxOperation::FsMove {
                workspace_id,
                from: "file".into(),
                to: "moved".into(),
            },
            SandboxOperation::FsDelete {
                workspace_id,
                path: "moved".into(),
            },
        ] {
            assert!(
                active_daemon
                    .handle_line(&envelope("correct opaque daemon token", operation))
                    .await
                    .result
                    .is_ok()
            );
        }
        assert_eq!(
            runtime.events.lock().await.as_slice(),
            ["pause", "resume", "pause", "resume", "pause", "resume"]
        );

        let failed_root = TestDirectory::new();
        let failed_runtime = Arc::new(LifecycleMockRuntime::new(true, true, Duration::ZERO));
        let failed_daemon = daemon(
            &failed_root.0,
            NetworkPolicyConfig {
                restricted_network: Some("gobrowse-restricted-test".into()),
                allow_full: false,
            },
            HARD_RESOURCE_LIMITS,
            failed_runtime,
        );
        failed_daemon
            .filesystem
            .ensure_workspace(workspace_id)
            .unwrap();
        let response = failed_daemon
            .handle_line(&envelope(
                "correct opaque daemon token",
                SandboxOperation::FsWrite {
                    workspace_id,
                    path: "blocked".into(),
                    data_base64: BASE64.encode(b"data"),
                },
            ))
            .await;
        assert_eq!(
            response.result.unwrap_err().code,
            SandboxErrorCode::Internal
        );
        assert!(matches!(
            failed_daemon.filesystem.read(workspace_id, "blocked"),
            Err(FilesystemError::NotFound)
        ));
    }

    #[test]
    fn authenticator_checks_digest_and_rejects_unbounded_tokens() {
        let auth = Authenticator::new("correct opaque daemon token").unwrap();
        assert!(auth.verify("correct opaque daemon token"));
        assert!(!auth.verify("correct opaque daemon tokeN"));
        assert!(!auth.verify(&"x".repeat(MAX_TOKEN_BYTES + 1)));
    }

    #[tokio::test]
    async fn fs_mkdir_invokes_pre_and_post_chown_hooks() {
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
        let workspace_id = Uuid::new_v4();
        daemon.filesystem.ensure_workspace(workspace_id).unwrap();
        for path in ["out", "out/sub"] {
            let response = daemon
                .handle_line(&envelope(
                    "correct opaque daemon token",
                    SandboxOperation::FsMkdir {
                        workspace_id,
                        path: path.into(),
                    },
                ))
                .await;
            assert!(response.result.is_ok(), "{:?}", response.result);
        }
        let chowns = runtime.chowns.lock().await;
        // Each mkdir runs a pre hook on its parent (fixing a container-created
        // dir) and a post hook on the freshly created dir so both sides can
        // write into it.
        assert_eq!(
            chowns.as_slice(),
            &[
                (workspace_id, ".".to_owned(), false),
                (workspace_id, "out".to_owned(), false),
                (workspace_id, "out".to_owned(), false),
                (workspace_id, "out/sub".to_owned(), false),
            ]
        );
    }
}
