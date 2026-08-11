use std::{
    collections::{HashMap, HashSet, VecDeque},
    net::IpAddr,
    os::unix::process::ExitStatusExt,
    path::PathBuf,
    process::Stdio,
    sync::{
        Arc,
        atomic::{AtomicBool, Ordering},
    },
    time::{Duration, Instant},
};

use async_trait::async_trait;
use gobrowse_core::sandbox::{
    MAX_TERMINAL_OUTPUT_CHUNK_BYTES, MAX_TERMINAL_PROCESS_BYTES,
    MAX_TERMINAL_PROCESS_COMMAND_BYTES, MAX_TERMINAL_PROCESSES, NetworkPolicy,
    RestrictedNetworkAttestation, SandboxProcess, TerminalStartRequest, TerminalState,
    WorkspaceStorageIdentity, workspace_volume_name,
};
use pty_process::{OwnedWritePty, Size};
use sha2::{Digest, Sha256};
use thiserror::Error;
use tokio::{
    io::{AsyncReadExt, AsyncWriteExt},
    process::Command,
    sync::{Mutex, Notify, watch},
};
use uuid::Uuid;

use crate::{
    InputDecision, JournalError, OutputRead, RecoveryRecord, RecoveryStore, StartDecision,
    TerminalJournal, TerminalRecord, WorkspaceResolver,
};

const MINIMAL_PATH: &str = "/usr/bin:/bin";
const CONTAINER_HOME: &str = "/tmp";
const MAX_ACTIVE_SESSIONS_HARD: usize = 1_024;
const MAX_RETAINED_RECORDS_HARD: usize = 16_384;
const CLEANUP_RETRY_INTERVAL: Duration = Duration::from_millis(50);
const MAX_READINESS_TIMEOUT: Duration = Duration::from_secs(60);
const MAX_CONTROL_TIMEOUT: Duration = Duration::from_secs(60);
const MAX_TERMINATION_TIMEOUT: Duration = Duration::from_secs(5 * 60);
const MAX_INPUT_WRITE_TIMEOUT: Duration = Duration::from_secs(10);

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct SessionLimits {
    pub max_active: usize,
    pub max_active_per_workspace: usize,
    pub max_retained_records: usize,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum WorkspaceProvisioning {
    Unverified,
    /// The provisioner creates an allowlisted named volume and attests enforced quota identity
    /// with the labels verified by sandboxd before every start.
    NamedVolume {
        maximum_bytes: u64,
    },
}

#[derive(Debug, Clone)]
pub struct PodmanConfig {
    pub executable: PathBuf,
    pub image: String,
    pub readiness_timeout: Duration,
    pub control_timeout: Duration,
    pub termination_timeout: Duration,
    pub input_write_timeout: Duration,
    pub session_limits: SessionLimits,
    pub workspace_provisioning: WorkspaceProvisioning,
    pub deployment_id: Uuid,
    pub workspace_resolver: WorkspaceResolver,
    pub recovery_store: RecoveryStore,
    pub terminal_journal: TerminalJournal,
    pub restricted_network: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ProcessSpec {
    pub program: PathBuf,
    pub args: Vec<String>,
}

impl ProcessSpec {
    fn command(&self) -> Command {
        let mut command = Command::new(&self.program);
        command
            .args(&self.args)
            .env_clear()
            .env("PATH", MINIMAL_PATH)
            .env("HOME", CONTAINER_HOME)
            .kill_on_drop(true);
        command
    }

    fn pty_command(&self) -> pty_process::Command {
        pty_process::Command::new(&self.program)
            .args(&self.args)
            .env_clear()
            .env("PATH", MINIMAL_PATH)
            .env("HOME", CONTAINER_HOME)
            .kill_on_drop(false)
    }
}

#[derive(Debug, Clone)]
pub struct ValidatedStart {
    pub terminal_id: Uuid,
    pub request: TerminalStartRequest,
    pub workspace_storage: WorkspaceStorageIdentity,
    pub network_name: String,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct WorkspacePause {
    pub(crate) workspace_id: Uuid,
    pub(crate) terminal_ids: Vec<Uuid>,
    pub(crate) recovery_id: Option<Uuid>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RuntimeInspect {
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

impl From<TerminalRecord> for RuntimeInspect {
    fn from(record: TerminalRecord) -> Self {
        Self {
            terminal_id: record.terminal_id,
            workspace_id: record.workspace_id,
            state: record.state,
            cols: record.cols,
            rows: record.rows,
            exit_code: record.exit_code,
            reason: record.reason,
            output_start_cursor: record.output_start_cursor,
            output_end_cursor: record.output_end_cursor,
            acked_cursor: record.acked_cursor,
            output_complete: record.output_complete,
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct InputOutcome {
    pub bytes: usize,
    pub replayed: bool,
}

#[derive(Debug, Error)]
pub enum RuntimeError {
    #[error("sandboxd cannot run as root")]
    RunningAsRoot,
    #[error("Podman configuration is unsafe")]
    InvalidConfiguration,
    #[error("quota-capable workspace provisioning is required")]
    WorkspaceQuotaUnavailable,
    #[error("active sandbox session limit reached")]
    SessionLimit,
    #[error("sandbox terminal does not exist")]
    NotFound,
    #[error("sandbox terminal is not running")]
    NotRunning,
    #[error("Podman did not report the container ready")]
    ReadinessFailed,
    #[error("Podman control operation timed out")]
    ControlTimeout,
    #[error("terminal input write timed out")]
    InputTimeout,
    #[error("Podman operation failed")]
    PodmanFailed,
    #[error("workspace volume identity or quota verification failed")]
    WorkspaceVolumeInvalid,
    #[error("workspace processes could not be positively paused")]
    WorkspacePauseFailed,
    #[error("workspace processes could not be positively resumed")]
    WorkspaceResumeFailed,
    #[error("workspace recovery could not be reconciled")]
    WorkspaceRecoveryFailed,
    #[error("terminal operation outcome is unknown")]
    OutcomeUnknown,
    #[error("terminal identifier conflicts with durable state")]
    Conflict,
    #[error("terminal journal failed")]
    Journal(#[source] JournalError),
    #[error("PTY operation failed")]
    Pty,
    #[error("runtime I/O failed")]
    Io(#[source] std::io::Error),
}

#[async_trait]
pub trait SandboxRuntime: Send + Sync {
    async fn start(&self, start: ValidatedStart) -> Result<(), RuntimeError>;
    async fn input(
        &self,
        terminal_id: Uuid,
        input_id: Uuid,
        bytes: &[u8],
    ) -> Result<InputOutcome, RuntimeError>;
    async fn read_output(
        &self,
        _terminal_id: Uuid,
        _after_cursor: u64,
        _max_bytes: usize,
        _wait: Duration,
    ) -> Result<OutputRead, RuntimeError> {
        Err(RuntimeError::InvalidConfiguration)
    }
    async fn ack_output(&self, _terminal_id: Uuid, _cursor: u64) -> Result<u64, RuntimeError> {
        Err(RuntimeError::InvalidConfiguration)
    }
    async fn resize(&self, terminal_id: Uuid, cols: u16, rows: u16) -> Result<(), RuntimeError>;
    async fn interrupt(&self, _terminal_id: Uuid) -> Result<(), RuntimeError> {
        Err(RuntimeError::InvalidConfiguration)
    }
    async fn processes(&self, _terminal_id: Uuid) -> Result<Vec<SandboxProcess>, RuntimeError> {
        Err(RuntimeError::InvalidConfiguration)
    }
    async fn kill(&self, _terminal_id: Uuid, _pid: u32) -> Result<(), RuntimeError> {
        Err(RuntimeError::InvalidConfiguration)
    }
    async fn terminate(&self, terminal_id: Uuid) -> Result<(), RuntimeError>;
    async fn inspect(&self, terminal_id: Uuid) -> Result<RuntimeInspect, RuntimeError>;
    async fn pause_workspace(&self, workspace_id: Uuid) -> Result<WorkspacePause, RuntimeError>;
    async fn resume_workspace(&self, pause: &WorkspacePause) -> Result<(), RuntimeError>;
    async fn reconcile_recoveries(&self) -> Result<(), RuntimeError>;
    async fn ensure_workspace_recovered(&self, workspace_id: Uuid) -> Result<(), RuntimeError>;
    async fn shutdown(&self) -> Result<(), RuntimeError> {
        Ok(())
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum LifecycleState {
    Starting,
    Running,
    Terminating,
    Unrecoverable,
    Exited,
    Terminated,
}

impl LifecycleState {
    fn terminal_state(self) -> TerminalState {
        match self {
            Self::Starting | Self::Running => TerminalState::Running,
            Self::Terminating => TerminalState::Terminating,
            Self::Unrecoverable => TerminalState::Unrecoverable,
            Self::Exited => TerminalState::Exited,
            Self::Terminated => TerminalState::Terminated,
        }
    }

    fn is_terminal(self) -> bool {
        matches!(self, Self::Exited | Self::Terminated)
    }
}

struct Session {
    terminal_id: Uuid,
    workspace_id: Uuid,
    state: watch::Sender<LifecycleState>,
    pty: Mutex<Option<OwnedWritePty>>,
    output_notify: Notify,
    termination_requested: AtomicBool,
    termination_owner: AtomicBool,
    background_cleanup_started: AtomicBool,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum OutputDrain {
    Complete,
    Failed(&'static str),
}

impl Session {
    fn new(terminal_id: Uuid, workspace_id: Uuid) -> Self {
        let (state, _) = watch::channel(LifecycleState::Starting);
        Self {
            terminal_id,
            workspace_id,
            state,
            pty: Mutex::new(None),
            output_notify: Notify::new(),
            termination_requested: AtomicBool::new(false),
            termination_owner: AtomicBool::new(false),
            background_cleanup_started: AtomicBool::new(false),
        }
    }

    fn lifecycle(&self) -> LifecycleState {
        *self.state.borrow()
    }

    fn transition(&self, next: LifecycleState) -> bool {
        self.state.send_if_modified(|current| {
            if current.is_terminal() {
                return false;
            }
            let allowed = match next {
                LifecycleState::Starting => false,
                LifecycleState::Running => *current == LifecycleState::Starting,
                LifecycleState::Terminating => matches!(
                    *current,
                    LifecycleState::Starting
                        | LifecycleState::Running
                        | LifecycleState::Unrecoverable
                ),
                LifecycleState::Unrecoverable => matches!(
                    *current,
                    LifecycleState::Starting
                        | LifecycleState::Running
                        | LifecycleState::Terminating
                ),
                LifecycleState::Exited | LifecycleState::Terminated => true,
            };
            if allowed && *current != next {
                *current = next;
                true
            } else {
                false
            }
        })
    }

    fn begin_termination(&self) -> bool {
        let state = self.lifecycle();
        if state.is_terminal() {
            return false;
        }
        self.termination_requested.store(true, Ordering::Release);
        if matches!(
            state,
            LifecycleState::Starting | LifecycleState::Running | LifecycleState::Unrecoverable
        ) {
            self.transition(LifecycleState::Terminating);
        }
        true
    }

    fn claim_termination(&self) -> Option<TerminationClaim<'_>> {
        self.termination_owner
            .compare_exchange(false, true, Ordering::AcqRel, Ordering::Acquire)
            .is_ok()
            .then_some(TerminationClaim {
                owner: &self.termination_owner,
            })
    }
}

struct TerminationClaim<'a> {
    owner: &'a AtomicBool,
}

struct BackgroundCleanupClaim(Arc<Session>);

impl Drop for BackgroundCleanupClaim {
    fn drop(&mut self) {
        self.0
            .background_cleanup_started
            .store(false, Ordering::Release);
    }
}

struct CleanupOnDrop {
    config: PodmanConfig,
    session: Arc<Session>,
    armed: bool,
}

impl CleanupOnDrop {
    fn disarm(&mut self) {
        self.armed = false;
    }
}

impl Drop for CleanupOnDrop {
    fn drop(&mut self) {
        if self.armed && !self.session.lifecycle().is_terminal() {
            self.session.begin_termination();
            spawn_persistent_cleanup(self.config.clone(), Arc::clone(&self.session));
        }
    }
}

impl Drop for TerminationClaim<'_> {
    fn drop(&mut self) {
        self.owner.store(false, Ordering::Release);
    }
}

struct Registry {
    sessions: HashMap<Uuid, Arc<Session>>,
    active: usize,
    active_by_workspace: HashMap<Uuid, usize>,
    completed: VecDeque<Uuid>,
}

impl Registry {
    fn new() -> Self {
        Self {
            sessions: HashMap::new(),
            active: 0,
            active_by_workspace: HashMap::new(),
            completed: VecDeque::new(),
        }
    }

    fn reserve(
        &mut self,
        session: Arc<Session>,
        limits: SessionLimits,
    ) -> Result<(), RuntimeError> {
        let workspace_active = self
            .active_by_workspace
            .get(&session.workspace_id)
            .copied()
            .unwrap_or_default();
        if self.active >= limits.max_active || workspace_active >= limits.max_active_per_workspace {
            return Err(RuntimeError::SessionLimit);
        }
        self.active += 1;
        *self
            .active_by_workspace
            .entry(session.workspace_id)
            .or_default() += 1;
        self.sessions.insert(session.terminal_id, session);
        Ok(())
    }

    fn complete(&mut self, terminal_id: Uuid, limits: SessionLimits) {
        let Some(session) = self.sessions.get(&terminal_id) else {
            return;
        };
        self.active = self.active.saturating_sub(1);
        if let Some(active) = self.active_by_workspace.get_mut(&session.workspace_id) {
            *active = active.saturating_sub(1);
            if *active == 0 {
                self.active_by_workspace.remove(&session.workspace_id);
            }
        }
        self.completed.push_back(terminal_id);
        while self.completed.len() > limits.max_retained_records {
            if let Some(expired) = self.completed.pop_front() {
                self.sessions.remove(&expired);
            }
        }
    }

    fn remove_completed(&mut self, terminal_id: Uuid) {
        self.sessions.remove(&terminal_id);
        self.completed.retain(|candidate| *candidate != terminal_id);
    }

    fn cancel_reservation(&mut self, terminal_id: Uuid) {
        let Some(session) = self.sessions.remove(&terminal_id) else {
            return;
        };
        self.active = self.active.saturating_sub(1);
        if let Some(active) = self.active_by_workspace.get_mut(&session.workspace_id) {
            *active = active.saturating_sub(1);
            if *active == 0 {
                self.active_by_workspace.remove(&session.workspace_id);
            }
        }
    }
}

#[derive(Clone)]
pub struct PodmanRuntime {
    config: PodmanConfig,
    registry: Arc<Mutex<Registry>>,
    restricted_network_attestation: Arc<std::sync::Mutex<Option<RestrictedNetworkAttestation>>>,
}

impl PodmanRuntime {
    pub fn new(config: PodmanConfig) -> Result<Self, RuntimeError> {
        let status = std::fs::read_to_string("/proc/self/status").map_err(RuntimeError::Io)?;
        if effective_uid_from_proc_status(&status)? == 0 {
            return Err(RuntimeError::RunningAsRoot);
        }
        Self::from_validated_config(config)
    }

    fn from_validated_config(config: PodmanConfig) -> Result<Self, RuntimeError> {
        if !config.executable.is_absolute()
            || config.executable.file_name().and_then(|name| name.to_str()) != Some("podman")
            || !valid_image_reference(&config.image)
            || config.readiness_timeout.is_zero()
            || config.readiness_timeout > MAX_READINESS_TIMEOUT
            || config.control_timeout.is_zero()
            || config.control_timeout > MAX_CONTROL_TIMEOUT
            || config.termination_timeout.is_zero()
            || config.termination_timeout > MAX_TERMINATION_TIMEOUT
            || config.input_write_timeout.is_zero()
            || config.input_write_timeout > MAX_INPUT_WRITE_TIMEOUT
            || config.session_limits.max_active == 0
            || config.session_limits.max_active > MAX_ACTIVE_SESSIONS_HARD
            || config.session_limits.max_active_per_workspace == 0
            || config.session_limits.max_active_per_workspace > config.session_limits.max_active
            || config.session_limits.max_retained_records == 0
            || config.session_limits.max_retained_records > MAX_RETAINED_RECORDS_HARD
            || config.deployment_id.is_nil()
        {
            return Err(RuntimeError::InvalidConfiguration);
        }
        if let WorkspaceProvisioning::NamedVolume { maximum_bytes } = config.workspace_provisioning
            && maximum_bytes == 0
        {
            return Err(RuntimeError::InvalidConfiguration);
        }
        if let Some(ref name) = config.restricted_network
            && !gobrowse_core::sandbox::valid_restricted_network_name(name)
        {
            return Err(RuntimeError::InvalidConfiguration);
        }
        Ok(Self {
            config,
            registry: Arc::new(Mutex::new(Registry::new())),
            restricted_network_attestation: Arc::new(std::sync::Mutex::new(None)),
        })
    }

    pub fn start_spec(&self, start: &ValidatedStart) -> Result<ProcessSpec, RuntimeError> {
        let WorkspaceProvisioning::NamedVolume { maximum_bytes } =
            self.config.workspace_provisioning
        else {
            return Err(RuntimeError::WorkspaceQuotaUnavailable);
        };
        if start.request.limits.writable_storage_bytes > maximum_bytes {
            return Err(RuntimeError::WorkspaceQuotaUnavailable);
        }
        let workspace = workspace_volume_name(start.request.workspace_id);
        if start.request.workspace_id.is_nil()
            || !valid_workspace_volume_name(&workspace)
            || start.workspace_storage.volume_name != workspace
            || start.workspace_storage.device == 0
            || start.workspace_storage.inode == 0
        {
            return Err(RuntimeError::InvalidConfiguration);
        }
        if !valid_runtime_network(start.request.network_policy, &start.network_name) {
            return Err(RuntimeError::InvalidConfiguration);
        }
        if start.request.network_policy == NetworkPolicy::Restricted
            && self.config.restricted_network.is_some()
            && self
                .restricted_network_attestation
                .lock()
                .unwrap()
                .is_none()
        {
            return Err(RuntimeError::InvalidConfiguration);
        }

        let limits = start.request.limits;
        let mut args = vec![
            "run".into(),
            "--interactive".into(),
            "--tty".into(),
            "--sig-proxy=false".into(),
            "--detach-keys=".into(),
            "--init".into(),
            "--pull=never".into(),
            "--log-driver=none".into(),
            "--image-volume=ignore".into(),
            "--http-proxy=false".into(),
            "--name".into(),
            container_name(start.terminal_id),
            "--label".into(),
            "io.gobrowse.managed=true".into(),
            "--label".into(),
            format!("io.gobrowse.deployment-id={}", self.config.deployment_id),
            "--label".into(),
            format!("io.gobrowse.workspace-id={}", start.request.workspace_id),
            "--label".into(),
            format!("io.gobrowse.terminal-id={}", start.terminal_id),
            "--cap-drop=ALL".into(),
            "--security-opt=no-new-privileges".into(),
            "--read-only".into(),
            "--userns=keep-id".into(),
            "--pid=private".into(),
            "--ipc=private".into(),
            "--uts=private".into(),
            "--cgroupns=private".into(),
            "--pids-limit".into(),
            limits.pids.to_string(),
            "--memory".into(),
            limits.memory_bytes.to_string(),
            "--cpus".into(),
            cpu_limit(limits.cpu_millis),
            "--network".into(),
            start.network_name.clone(),
            "--volume".into(),
            format!("{workspace}:/workspace:rw,nodev,nosuid"),
            "--tmpfs".into(),
            "/tmp:rw,noexec,nosuid,nodev,size=67108864".into(),
            "--workdir".into(),
            format!("/workspace/{}", start.request.working_directory),
            self.config.image.clone(),
        ];
        args.extend(start.request.command.iter().cloned());
        Ok(ProcessSpec {
            program: self.config.executable.clone(),
            args,
        })
    }

    pub fn readiness_spec(&self, terminal_id: Uuid) -> ProcessSpec {
        ProcessSpec {
            program: self.config.executable.clone(),
            args: vec![
                "inspect".into(),
                "--format".into(),
                "{{.State.Running}}".into(),
                container_name(terminal_id),
            ],
        }
    }

    pub fn resize_spec(&self, terminal_id: Uuid, cols: u16, rows: u16) -> ProcessSpec {
        ProcessSpec {
            program: self.config.executable.clone(),
            args: vec![
                "container".into(),
                "resize".into(),
                "--width".into(),
                cols.to_string(),
                "--height".into(),
                rows.to_string(),
                container_name(terminal_id),
            ],
        }
    }

    pub fn terminate_spec(&self, terminal_id: Uuid) -> ProcessSpec {
        ProcessSpec {
            program: self.config.executable.clone(),
            args: vec![
                "stop".into(),
                "--ignore".into(),
                "--time".into(),
                "1".into(),
                container_name(terminal_id),
            ],
        }
    }

    pub fn processes_spec(&self, terminal_id: Uuid) -> ProcessSpec {
        ProcessSpec {
            program: self.config.executable.clone(),
            args: vec![
                "top".into(),
                container_name(terminal_id),
                "pid".into(),
                "comm".into(),
                "args".into(),
            ],
        }
    }

    pub fn kill_spec(&self, terminal_id: Uuid, pid: u32) -> ProcessSpec {
        ProcessSpec {
            program: self.config.executable.clone(),
            args: vec![
                "exec".into(),
                container_name(terminal_id),
                "/bin/kill".into(),
                "-TERM".into(),
                "--".into(),
                pid.to_string(),
            ],
        }
    }

    pub fn volume_inspect_spec(&self, workspace_id: Uuid) -> ProcessSpec {
        ProcessSpec {
            program: self.config.executable.clone(),
            args: vec![
                "volume".into(),
                "inspect".into(),
                "--format".into(),
                "{{.Name}}|{{.Driver}}|{{json .Options}}|{{.Mountpoint}}".into(),
                workspace_volume_name(workspace_id),
            ],
        }
    }

    pub fn network_inspect_spec(&self, name: &str) -> ProcessSpec {
        ProcessSpec {
            program: self.config.executable.clone(),
            args: vec![
                "network".into(),
                "inspect".into(),
                "--format".into(),
                "{{.Name}}|{{.Subnet}}|{{.Gateway}}|{{json .DNS}}".into(),
                name.into(),
            ],
        }
    }

    pub fn pause_spec(&self, terminal_id: Uuid) -> ProcessSpec {
        ProcessSpec {
            program: self.config.executable.clone(),
            args: vec!["pause".into(), container_name(terminal_id)],
        }
    }

    pub fn unpause_spec(&self, terminal_id: Uuid) -> ProcessSpec {
        ProcessSpec {
            program: self.config.executable.clone(),
            args: vec!["unpause".into(), container_name(terminal_id)],
        }
    }

    pub fn paused_inspect_spec(&self, terminal_id: Uuid) -> ProcessSpec {
        ProcessSpec {
            program: self.config.executable.clone(),
            args: vec![
                "inspect".into(),
                "--format".into(),
                "{{.State.Paused}}".into(),
                container_name(terminal_id),
            ],
        }
    }

    pub fn managed_containers_spec(&self, workspace_id: Option<Uuid>) -> ProcessSpec {
        let mut args = vec![
            "ps".into(),
            "--all".into(),
            "--filter".into(),
            "label=io.gobrowse.managed=true".into(),
            "--filter".into(),
            format!(
                "label=io.gobrowse.deployment-id={}",
                self.config.deployment_id
            ),
        ];
        if let Some(workspace_id) = workspace_id {
            args.extend([
                "--filter".into(),
                format!("label=io.gobrowse.workspace-id={workspace_id}"),
            ]);
        }
        args.extend([
            "--format".into(),
            "{{.Names}}|{{.Label \"io.gobrowse.terminal-id\"}}|{{.Label \"io.gobrowse.workspace-id\"}}".into(),
        ]);
        ProcessSpec {
            program: self.config.executable.clone(),
            args,
        }
    }

    async fn session(&self, terminal_id: Uuid) -> Result<Arc<Session>, RuntimeError> {
        self.registry
            .lock()
            .await
            .sessions
            .get(&terminal_id)
            .cloned()
            .ok_or(RuntimeError::NotFound)
    }

    async fn run_checked(&self, spec: ProcessSpec) -> Result<(), RuntimeError> {
        let status = tokio::time::timeout(
            self.config.control_timeout,
            spec.command()
                .stdin(Stdio::null())
                .stdout(Stdio::null())
                .stderr(Stdio::null())
                .status(),
        )
        .await
        .map_err(|_| RuntimeError::ControlTimeout)?
        .map_err(RuntimeError::Io)?;
        if status.success() {
            Ok(())
        } else {
            Err(RuntimeError::PodmanFailed)
        }
    }

    async fn run_output_bounded(
        &self,
        spec: ProcessSpec,
        maximum: usize,
    ) -> Result<(std::process::ExitStatus, Vec<u8>), RuntimeError> {
        let mut child = spec
            .command()
            .stdin(Stdio::null())
            .stdout(Stdio::piped())
            .stderr(Stdio::null())
            .spawn()
            .map_err(RuntimeError::Io)?;
        let stdout = child.stdout.take().ok_or(RuntimeError::PodmanFailed)?;
        let result = tokio::time::timeout(self.config.control_timeout, async {
            let mut output = Vec::with_capacity(maximum.min(16 * 1024));
            stdout
                .take(maximum as u64 + 1)
                .read_to_end(&mut output)
                .await
                .map_err(RuntimeError::Io)?;
            let status = child.wait().await.map_err(RuntimeError::Io)?;
            Ok::<_, RuntimeError>((status, output))
        })
        .await
        .map_err(|_| RuntimeError::ControlTimeout)??;
        if result.1.len() > maximum {
            Err(RuntimeError::PodmanFailed)
        } else {
            Ok(result)
        }
    }

    async fn inspect_ready(&self, terminal_id: Uuid) -> Result<bool, RuntimeError> {
        let (status, output) = self
            .run_output_bounded(self.readiness_spec(terminal_id), 16)
            .await?;
        Ok(status.success() && output.as_slice() == b"true\n")
    }

    async fn verify_workspace_volume(&self, start: &ValidatedStart) -> Result<(), RuntimeError> {
        let WorkspaceProvisioning::NamedVolume { maximum_bytes } =
            self.config.workspace_provisioning
        else {
            return Err(RuntimeError::WorkspaceQuotaUnavailable);
        };
        let (status, output) = self
            .run_output_bounded(
                self.volume_inspect_spec(start.request.workspace_id),
                16 * 1024,
            )
            .await?;
        if !status.success() {
            Err(RuntimeError::WorkspaceVolumeInvalid)
        } else {
            let output =
                String::from_utf8(output).map_err(|_| RuntimeError::WorkspaceVolumeInvalid)?;
            let output = output
                .strip_suffix('\n')
                .ok_or(RuntimeError::WorkspaceVolumeInvalid)?;
            let mut fields = output.splitn(4, '|');
            let name = fields.next().ok_or(RuntimeError::WorkspaceVolumeInvalid)?;
            let driver = fields.next().ok_or(RuntimeError::WorkspaceVolumeInvalid)?;
            let options = fields.next().ok_or(RuntimeError::WorkspaceVolumeInvalid)?;
            let mountpoint = fields.next().ok_or(RuntimeError::WorkspaceVolumeInvalid)?;
            if name != start.workspace_storage.volume_name
                || start.request.limits.writable_storage_bytes > maximum_bytes
            {
                return Err(RuntimeError::WorkspaceVolumeInvalid);
            }
            self.config
                .workspace_resolver
                .verify_volume(
                    start.request.workspace_id,
                    driver,
                    options,
                    mountpoint,
                    &start.workspace_storage,
                )
                .map_err(|_| RuntimeError::WorkspaceVolumeInvalid)
        }
    }

    async fn attest_restricted_network(
        &self,
        name: &str,
    ) -> Result<RestrictedNetworkAttestation, RuntimeError> {
        let (status, output) = self
            .run_output_bounded(self.network_inspect_spec(name), 4 * 1024)
            .await?;
        if !status.success() {
            return Err(RuntimeError::InvalidConfiguration);
        }
        let output = String::from_utf8(output).map_err(|_| RuntimeError::InvalidConfiguration)?;
        let output = output
            .strip_suffix('\n')
            .ok_or(RuntimeError::InvalidConfiguration)?;
        let mut fields = output.splitn(4, '|');
        let inspected_name = fields.next().ok_or(RuntimeError::InvalidConfiguration)?;
        let subnet: IpAddr = fields
            .next()
            .and_then(|value| value.split('/').next())
            .ok_or(RuntimeError::InvalidConfiguration)?
            .parse()
            .map_err(|_| RuntimeError::InvalidConfiguration)?;
        let gateway: IpAddr = fields
            .next()
            .ok_or(RuntimeError::InvalidConfiguration)?
            .parse()
            .map_err(|_| RuntimeError::InvalidConfiguration)?;
        let dns_json = fields.next().ok_or(RuntimeError::InvalidConfiguration)?;
        let dns: Vec<IpAddr> =
            serde_json::from_str(dns_json).map_err(|_| RuntimeError::InvalidConfiguration)?;
        if inspected_name != name {
            return Err(RuntimeError::InvalidConfiguration);
        }
        let attestation = RestrictedNetworkAttestation {
            network_name: name.into(),
            subnet,
            gateway,
            dns,
        };
        attestation
            .validate()
            .map_err(|_| RuntimeError::InvalidConfiguration)?;
        Ok(attestation)
    }

    async fn inspect_paused(&self, terminal_id: Uuid) -> Result<bool, RuntimeError> {
        let (status, output) = self
            .run_output_bounded(self.paused_inspect_spec(terminal_id), 16)
            .await?;
        if !status.success() {
            return Err(RuntimeError::PodmanFailed);
        }
        match output.as_slice() {
            b"true\n" => Ok(true),
            b"false\n" => Ok(false),
            _ => Err(RuntimeError::PodmanFailed),
        }
    }

    async fn managed_containers(
        &self,
        workspace_id: Option<Uuid>,
    ) -> Result<Vec<(Uuid, Uuid)>, RuntimeError> {
        let (status, output) = self
            .run_output_bounded(self.managed_containers_spec(workspace_id), 1024 * 1024)
            .await?;
        if !status.success() {
            return Err(RuntimeError::WorkspaceRecoveryFailed);
        }
        let output =
            String::from_utf8(output).map_err(|_| RuntimeError::WorkspaceRecoveryFailed)?;
        let mut containers = Vec::new();
        for line in output.lines() {
            let mut fields = line.split('|');
            let name = fields.next().ok_or(RuntimeError::WorkspaceRecoveryFailed)?;
            let terminal_id = fields
                .next()
                .and_then(|value| Uuid::parse_str(value).ok())
                .ok_or(RuntimeError::WorkspaceRecoveryFailed)?;
            let listed_workspace = fields
                .next()
                .and_then(|value| Uuid::parse_str(value).ok())
                .ok_or(RuntimeError::WorkspaceRecoveryFailed)?;
            if fields.next().is_some()
                || name != container_name(terminal_id)
                || workspace_id.is_some_and(|expected| expected != listed_workspace)
            {
                return Err(RuntimeError::WorkspaceRecoveryFailed);
            }
            containers.push((terminal_id, listed_workspace));
        }
        Ok(containers)
    }

    async fn terminate_managed(&self, terminal_id: Uuid) -> Result<(), RuntimeError> {
        self.run_checked(force_removal_spec(&self.config, terminal_id))
            .await
    }

    async fn remove_workspace_orphans(&self, workspace_id: Uuid) -> Result<(), RuntimeError> {
        let registered = self
            .registry
            .lock()
            .await
            .sessions
            .values()
            .filter(|session| {
                session.workspace_id == workspace_id && !session.lifecycle().is_terminal()
            })
            .map(|session| session.terminal_id)
            .collect::<HashSet<_>>();
        let listed = self.managed_containers(Some(workspace_id)).await?;
        for (terminal_id, _) in listed {
            if !registered.contains(&terminal_id) {
                self.terminate_managed(terminal_id).await?;
            }
        }
        if self
            .managed_containers(Some(workspace_id))
            .await?
            .iter()
            .any(|(terminal_id, _)| !registered.contains(terminal_id))
        {
            return Err(RuntimeError::WorkspaceRecoveryFailed);
        }
        Ok(())
    }

    async fn await_readiness(&self, session: &Session) -> Result<(), RuntimeError> {
        tokio::time::timeout(self.config.readiness_timeout, async {
            loop {
                if session.lifecycle().is_terminal() {
                    return Err(RuntimeError::ReadinessFailed);
                }
                match self.inspect_ready(session.terminal_id).await {
                    Ok(true) => {
                        if session.lifecycle() == LifecycleState::Starting {
                            return Ok(());
                        }
                        return Err(RuntimeError::ReadinessFailed);
                    }
                    Ok(false) | Err(RuntimeError::PodmanFailed) => {}
                    Err(error) => return Err(error),
                }
                tokio::time::sleep(Duration::from_millis(50)).await;
            }
        })
        .await
        .map_err(|_| RuntimeError::ReadinessFailed)?
    }

    fn spawn_timeout(&self, session: Arc<Session>, timeout: Duration) {
        let config = self.config.clone();
        tokio::spawn(async move {
            tokio::time::sleep(timeout).await;
            let deadline = Instant::now() + config.termination_timeout;
            if !cleanup_until(&config, &session, deadline).await {
                spawn_persistent_cleanup(config, session);
            }
        });
    }
}

async fn run_checked_with(config: &PodmanConfig, spec: ProcessSpec) -> Result<(), RuntimeError> {
    let status = tokio::time::timeout(
        config.control_timeout,
        spec.command()
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .status(),
    )
    .await
    .map_err(|_| RuntimeError::ControlTimeout)?
    .map_err(RuntimeError::Io)?;
    if status.success() {
        Ok(())
    } else {
        Err(RuntimeError::PodmanFailed)
    }
}

fn termination_spec(config: &PodmanConfig, terminal_id: Uuid) -> ProcessSpec {
    ProcessSpec {
        program: config.executable.clone(),
        args: vec![
            "stop".into(),
            "--ignore".into(),
            "--time".into(),
            "1".into(),
            container_name(terminal_id),
        ],
    }
}

fn force_removal_spec(config: &PodmanConfig, terminal_id: Uuid) -> ProcessSpec {
    ProcessSpec {
        program: config.executable.clone(),
        args: vec![
            "rm".into(),
            "--force".into(),
            "--ignore".into(),
            container_name(terminal_id),
        ],
    }
}

async fn cleanup_attempt(config: &PodmanConfig, session: &Session) {
    if session.lifecycle().is_terminal() {
        return;
    }
    let Some(_claim) = session.claim_termination() else {
        return;
    };
    if session.lifecycle().is_terminal() {
        return;
    }
    if run_checked_with(config, termination_spec(config, session.terminal_id))
        .await
        .is_err()
    {
        let _ = run_checked_with(config, force_removal_spec(config, session.terminal_id)).await;
    }
}

async fn cleanup_until(config: &PodmanConfig, session: &Session, deadline: Instant) -> bool {
    if !session.begin_termination() {
        return true;
    }
    let _ = config.terminal_journal.set_state(
        session.terminal_id,
        TerminalState::Terminating,
        None,
        Some("termination_requested"),
        None,
    );
    if let Some(pty) = session.pty.lock().await.as_mut() {
        let _ = tokio::time::timeout(config.input_write_timeout, async {
            pty.write_all(&[3]).await?;
            pty.flush().await
        })
        .await;
    }
    let mut state = session.state.subscribe();
    loop {
        if state.borrow_and_update().is_terminal() {
            return true;
        }
        if Instant::now() >= deadline {
            session.transition(LifecycleState::Unrecoverable);
            let _ = config.terminal_journal.set_state(
                session.terminal_id,
                TerminalState::Unrecoverable,
                None,
                Some("force_cleanup_deadline_exceeded"),
                None,
            );
            session.output_notify.notify_waiters();
            return false;
        }
        let remaining = deadline.saturating_duration_since(Instant::now());
        let _ = tokio::time::timeout(remaining, cleanup_attempt(config, session)).await;
        let remaining = deadline.saturating_duration_since(Instant::now());
        let delay = CLEANUP_RETRY_INTERVAL.min(remaining);
        tokio::select! {
            changed = state.changed() => {
                if changed.is_err() {
                    return false;
                }
            }
            () = tokio::time::sleep(delay) => {}
        }
    }
}

fn spawn_persistent_cleanup(config: PodmanConfig, session: Arc<Session>) {
    if session
        .background_cleanup_started
        .swap(true, Ordering::AcqRel)
    {
        return;
    }
    tokio::spawn(async move {
        let claim = BackgroundCleanupClaim(session);
        let session = Arc::clone(&claim.0);
        while !session.lifecycle().is_terminal() {
            cleanup_attempt(&config, &session).await;
            tokio::time::sleep(CLEANUP_RETRY_INTERVAL).await;
        }
    });
}

impl PodmanRuntime {
    async fn pause_workspace_owned(
        &self,
        workspace_id: Uuid,
    ) -> Result<WorkspacePause, RuntimeError> {
        self.ensure_workspace_recovered(workspace_id).await?;
        let terminal_ids = self
            .managed_containers(Some(workspace_id))
            .await?
            .into_iter()
            .map(|(terminal_id, _)| terminal_id)
            .collect::<Vec<_>>();
        if terminal_ids.is_empty() {
            return Ok(WorkspacePause {
                workspace_id,
                terminal_ids,
                recovery_id: None,
            });
        }
        let recovery_id = Uuid::new_v4();
        self.config
            .recovery_store
            .persist(&RecoveryRecord {
                workspace_id,
                operation_id: recovery_id,
                containers: terminal_ids.iter().copied().map(container_name).collect(),
            })
            .map_err(|_| RuntimeError::WorkspaceRecoveryFailed)?;

        let mut paused = Vec::with_capacity(terminal_ids.len());
        for terminal_id in terminal_ids {
            let paused_ok = self.run_checked(self.pause_spec(terminal_id)).await.is_ok()
                && matches!(self.inspect_paused(terminal_id).await, Ok(true));
            if !paused_ok {
                let pause = WorkspacePause {
                    workspace_id,
                    terminal_ids: paused,
                    recovery_id: Some(recovery_id),
                };
                return if self.resume_workspace_owned(&pause).await.is_ok() {
                    Err(RuntimeError::WorkspacePauseFailed)
                } else {
                    Err(RuntimeError::WorkspaceResumeFailed)
                };
            }
            paused.push(terminal_id);
        }
        Ok(WorkspacePause {
            workspace_id,
            terminal_ids: paused,
            recovery_id: Some(recovery_id),
        })
    }

    async fn resume_workspace_owned(&self, pause: &WorkspacePause) -> Result<(), RuntimeError> {
        if pause.terminal_ids.is_empty() {
            return Ok(());
        }
        let recovery_id = pause
            .recovery_id
            .ok_or(RuntimeError::WorkspaceRecoveryFailed)?;
        for attempt in 0..3 {
            let listed = self.managed_containers(Some(pause.workspace_id)).await?;
            let listed = listed
                .into_iter()
                .map(|(terminal_id, _)| terminal_id)
                .collect::<HashSet<_>>();
            let mut unresolved = false;
            for terminal_id in &pause.terminal_ids {
                if !listed.contains(terminal_id) {
                    continue;
                }
                match self.inspect_paused(*terminal_id).await {
                    Ok(false) => {}
                    Ok(true) => {
                        if self
                            .run_checked(self.unpause_spec(*terminal_id))
                            .await
                            .is_err()
                            || !matches!(self.inspect_paused(*terminal_id).await, Ok(false))
                        {
                            unresolved = true;
                        }
                    }
                    Err(_) => unresolved = true,
                }
            }
            if !unresolved {
                self.config
                    .recovery_store
                    .clear(pause.workspace_id, recovery_id)
                    .map_err(|_| RuntimeError::WorkspaceRecoveryFailed)?;
                return Ok(());
            }
            if attempt < 2 {
                tokio::time::sleep(CLEANUP_RETRY_INTERVAL).await;
            }
        }
        Err(RuntimeError::WorkspaceResumeFailed)
    }
}

#[async_trait]
impl SandboxRuntime for PodmanRuntime {
    async fn start(&self, start: ValidatedStart) -> Result<(), RuntimeError> {
        self.ensure_workspace_recovered(start.request.workspace_id)
            .await?;
        let spec = self.start_spec(&start)?;
        self.verify_workspace_volume(&start).await?;
        let fingerprint = start_fingerprint(&start)?;
        match self
            .config
            .terminal_journal
            .begin_start(
                start.terminal_id,
                start.request.workspace_id,
                fingerprint,
                start.request.cols,
                start.request.rows,
            )
            .map_err(map_journal_error)?
        {
            StartDecision::Existing => {
                if self
                    .registry
                    .lock()
                    .await
                    .sessions
                    .get(&start.terminal_id)
                    .is_some_and(|session| {
                        session.workspace_id == start.request.workspace_id
                            && session.lifecycle() == LifecycleState::Running
                    })
                {
                    return Ok(());
                }
                return Err(RuntimeError::Conflict);
            }
            StartDecision::Execute => {}
        }
        let session = Arc::new(Session::new(start.terminal_id, start.request.workspace_id));
        if let Err(error) = self
            .registry
            .lock()
            .await
            .reserve(Arc::clone(&session), self.config.session_limits)
        {
            let _ = self.config.terminal_journal.set_state(
                session.terminal_id,
                TerminalState::Terminated,
                None,
                Some("session_reservation_failed"),
                Some(false),
            );
            return Err(error);
        }

        let (pty, pts) = match pty_process::open() {
            Ok(pair) => pair,
            Err(_) => {
                self.registry
                    .lock()
                    .await
                    .cancel_reservation(session.terminal_id);
                let _ = self.config.terminal_journal.set_state(
                    session.terminal_id,
                    TerminalState::Terminated,
                    None,
                    Some("pty_open_failed"),
                    Some(false),
                );
                return Err(RuntimeError::Pty);
            }
        };
        if configure_raw_pty(&pty).is_err()
            || pty
                .resize(Size::new(start.request.rows, start.request.cols))
                .is_err()
        {
            self.registry
                .lock()
                .await
                .cancel_reservation(session.terminal_id);
            let _ = self.config.terminal_journal.set_state(
                session.terminal_id,
                TerminalState::Terminated,
                None,
                Some("pty_initialization_failed"),
                Some(false),
            );
            return Err(RuntimeError::Pty);
        }
        let spawned = spec.pty_command().spawn(pts);
        let mut child = match spawned {
            Ok(child) => child,
            Err(_) => {
                self.registry
                    .lock()
                    .await
                    .cancel_reservation(session.terminal_id);
                let _ = self.config.terminal_journal.set_state(
                    session.terminal_id,
                    TerminalState::Terminated,
                    None,
                    Some("spawn_failed"),
                    Some(false),
                );
                return Err(RuntimeError::Pty);
            }
        };
        let (mut output, input) = pty.into_split();
        session
            .pty
            .try_lock()
            .expect("new session PTY lock is uncontended")
            .replace(input);

        let output_session = Arc::clone(&session);
        let output_config = self.config.clone();
        let output_task = tokio::spawn(async move {
            let result = drain_pty_output(
                &mut output,
                &output_config.terminal_journal,
                &output_session,
            )
            .await;
            if let OutputDrain::Failed(reason) = result {
                output_session.begin_termination();
                let output_journal = output_config.terminal_journal.clone();
                spawn_persistent_cleanup(output_config, Arc::clone(&output_session));
                record_output_failure(&output_journal, &output_session, reason);
            }
            result
        });

        let monitor_session = Arc::clone(&session);
        let registry = Arc::clone(&self.registry);
        let limits = self.config.session_limits;
        let monitor_config = self.config.clone();
        tokio::spawn(async move {
            let status = child.wait().await.ok();
            monitor_session.pty.lock().await.take();
            let output_result = match output_task.await {
                Ok(result) => result,
                Err(_) => {
                    let _ = monitor_config.terminal_journal.set_state(
                        monitor_session.terminal_id,
                        monitor_session.lifecycle().terminal_state(),
                        None,
                        Some("pty_output_task_failed"),
                        Some(false),
                    );
                    OutputDrain::Failed("pty_output_task_failed")
                }
            };
            let _ = run_checked_with(
                &monitor_config,
                force_removal_spec(&monitor_config, monitor_session.terminal_id),
            )
            .await;
            let final_state = if monitor_session
                .termination_requested
                .load(Ordering::Acquire)
            {
                LifecycleState::Terminated
            } else {
                LifecycleState::Exited
            };
            let (exit_code, exit_reason) = exit_description(status.as_ref(), final_state);
            let reason = match output_result {
                OutputDrain::Complete => exit_reason,
                OutputDrain::Failed(reason) => reason.to_owned(),
            };
            let _ = monitor_config.terminal_journal.set_state(
                monitor_session.terminal_id,
                final_state.terminal_state(),
                exit_code,
                Some(&reason),
                matches!(output_result, OutputDrain::Failed(_)).then_some(false),
            );
            registry
                .lock()
                .await
                .complete(monitor_session.terminal_id, limits);
            monitor_session.transition(final_state);
            monitor_session.output_notify.notify_waiters();
        });
        let mut cancellation_guard = CleanupOnDrop {
            config: self.config.clone(),
            session: Arc::clone(&session),
            armed: true,
        };

        let initialized = match self.await_readiness(&session).await {
            Ok(()) => self
                .run_checked(self.resize_spec(
                    session.terminal_id,
                    start.request.cols,
                    start.request.rows,
                ))
                .await
                .and_then(|()| {
                    session
                        .transition(LifecycleState::Running)
                        .then_some(())
                        .ok_or(RuntimeError::ReadinessFailed)
                }),
            Err(error) => Err(error),
        };
        if let Err(error) = initialized {
            let deadline = Instant::now() + self.config.termination_timeout;
            let cleaned = cleanup_until(&self.config, &session, deadline).await;
            if !cleaned {
                spawn_persistent_cleanup(self.config.clone(), Arc::clone(&session));
            }
            if cleaned {
                self.registry
                    .lock()
                    .await
                    .remove_completed(session.terminal_id);
            }
            cancellation_guard.disarm();
            let existing = self
                .config
                .terminal_journal
                .inspect(session.terminal_id)
                .ok();
            let reason = existing
                .as_ref()
                .filter(|record| !record.output_complete)
                .and_then(|record| record.reason.as_deref())
                .unwrap_or("start_readiness_failed");
            let _ = self.config.terminal_journal.set_state(
                session.terminal_id,
                TerminalState::Terminated,
                None,
                Some(reason),
                Some(false),
            );
            return Err(error);
        }
        self.config
            .terminal_journal
            .set_running(session.terminal_id)
            .map_err(map_journal_error)?;
        cancellation_guard.disarm();
        self.spawn_timeout(
            Arc::clone(&session),
            Duration::from_secs(start.request.limits.execution_seconds),
        );
        Ok(())
    }

    async fn input(
        &self,
        terminal_id: Uuid,
        input_id: Uuid,
        bytes: &[u8],
    ) -> Result<InputOutcome, RuntimeError> {
        let session = self.session(terminal_id).await?;
        if session.lifecycle() != LifecycleState::Running {
            return Err(RuntimeError::NotRunning);
        }
        let fingerprint: [u8; 32] = Sha256::digest(bytes).into();
        match self
            .config
            .terminal_journal
            .begin_input(terminal_id, input_id, fingerprint)
            .map_err(map_journal_error)?
        {
            InputDecision::Applied(bytes) => {
                return Ok(InputOutcome {
                    bytes,
                    replayed: true,
                });
            }
            InputDecision::Execute => {}
        }
        let mut pty = session.pty.lock().await;
        if session.lifecycle() != LifecycleState::Running {
            let _ = self
                .config
                .terminal_journal
                .mark_input_unknown(terminal_id, input_id);
            return Err(RuntimeError::NotRunning);
        }
        let write_result = tokio::time::timeout(self.config.input_write_timeout, async {
            let pty = pty.as_mut().ok_or(RuntimeError::NotRunning)?;
            pty.write_all(bytes).await.map_err(RuntimeError::Io)?;
            pty.flush().await.map_err(RuntimeError::Io)
        })
        .await;
        match write_result {
            Ok(Ok(())) => {
                if self
                    .config
                    .terminal_journal
                    .complete_input(terminal_id, input_id, bytes.len())
                    .is_err()
                {
                    let _ = self
                        .config
                        .terminal_journal
                        .mark_input_unknown(terminal_id, input_id);
                    return Err(RuntimeError::OutcomeUnknown);
                }
                Ok(InputOutcome {
                    bytes: bytes.len(),
                    replayed: false,
                })
            }
            Ok(Err(_)) | Err(_) => {
                let _ = self
                    .config
                    .terminal_journal
                    .mark_input_unknown(terminal_id, input_id);
                Err(RuntimeError::OutcomeUnknown)
            }
        }
    }

    async fn read_output(
        &self,
        terminal_id: Uuid,
        after_cursor: u64,
        max_bytes: usize,
        wait: Duration,
    ) -> Result<OutputRead, RuntimeError> {
        let first = self
            .config
            .terminal_journal
            .read_output(terminal_id, after_cursor, max_bytes)
            .map_err(map_journal_error)?;
        if !first.bytes.is_empty() || first.record.state != TerminalState::Running || wait.is_zero()
        {
            return Ok(first);
        }
        let Ok(session) = self.session(terminal_id).await else {
            return Ok(first);
        };
        let notified = session.output_notify.notified();
        tokio::pin!(notified);
        notified.as_mut().enable();
        let second = self
            .config
            .terminal_journal
            .read_output(terminal_id, after_cursor, max_bytes)
            .map_err(map_journal_error)?;
        if !second.bytes.is_empty() || second.record.state != TerminalState::Running {
            return Ok(second);
        }
        let _ = tokio::time::timeout(wait, notified).await;
        self.config
            .terminal_journal
            .read_output(terminal_id, after_cursor, max_bytes)
            .map_err(map_journal_error)
    }

    async fn ack_output(&self, terminal_id: Uuid, cursor: u64) -> Result<u64, RuntimeError> {
        self.config
            .terminal_journal
            .ack_output(terminal_id, cursor)
            .map_err(map_journal_error)
    }

    async fn resize(&self, terminal_id: Uuid, cols: u16, rows: u16) -> Result<(), RuntimeError> {
        let session = self.session(terminal_id).await?;
        if session.lifecycle() != LifecycleState::Running {
            return Err(RuntimeError::NotRunning);
        }
        session
            .pty
            .lock()
            .await
            .as_ref()
            .ok_or(RuntimeError::NotRunning)?
            .resize(Size::new(rows, cols))
            .map_err(|_| RuntimeError::Pty)?;
        self.run_checked(self.resize_spec(terminal_id, cols, rows))
            .await?;
        self.config
            .terminal_journal
            .set_size(terminal_id, cols, rows)
            .map_err(map_journal_error)
    }

    async fn interrupt(&self, terminal_id: Uuid) -> Result<(), RuntimeError> {
        let session = self.session(terminal_id).await?;
        if session.lifecycle() != LifecycleState::Running {
            return Err(RuntimeError::NotRunning);
        }
        let mut pty = session.pty.lock().await;
        let pty = pty.as_mut().ok_or(RuntimeError::NotRunning)?;
        tokio::time::timeout(self.config.input_write_timeout, async {
            pty.write_all(&[3]).await?;
            pty.flush().await
        })
        .await
        .map_err(|_| RuntimeError::OutcomeUnknown)?
        .map_err(RuntimeError::Io)
    }

    async fn processes(&self, terminal_id: Uuid) -> Result<Vec<SandboxProcess>, RuntimeError> {
        self.config
            .terminal_journal
            .inspect(terminal_id)
            .map_err(map_journal_error)?;
        let (status, output) = self
            .run_output_bounded(self.processes_spec(terminal_id), MAX_TERMINAL_PROCESS_BYTES)
            .await?;
        if !status.success() {
            return Err(RuntimeError::PodmanFailed);
        }
        parse_processes(&output)
    }

    async fn kill(&self, terminal_id: Uuid, pid: u32) -> Result<(), RuntimeError> {
        if pid == 0 {
            return Err(RuntimeError::InvalidConfiguration);
        }
        self.config
            .terminal_journal
            .inspect(terminal_id)
            .map_err(map_journal_error)?;
        self.run_checked(self.kill_spec(terminal_id, pid)).await
    }

    async fn terminate(&self, terminal_id: Uuid) -> Result<(), RuntimeError> {
        let session = self.session(terminal_id).await?;
        if session.lifecycle().is_terminal() {
            return Ok(());
        }
        session.begin_termination();
        let _ = self.config.terminal_journal.set_state(
            terminal_id,
            TerminalState::Terminating,
            None,
            Some("termination_requested"),
            None,
        );
        let deadline = Instant::now() + self.config.termination_timeout;
        if cleanup_until(&self.config, &session, deadline).await {
            Ok(())
        } else {
            spawn_persistent_cleanup(self.config.clone(), session);
            Err(RuntimeError::ControlTimeout)
        }
    }

    async fn inspect(&self, terminal_id: Uuid) -> Result<RuntimeInspect, RuntimeError> {
        self.config
            .terminal_journal
            .inspect(terminal_id)
            .map(RuntimeInspect::from)
            .map_err(map_journal_error)
    }

    async fn pause_workspace(&self, workspace_id: Uuid) -> Result<WorkspacePause, RuntimeError> {
        let runtime = self.clone();
        let (send, receive) = tokio::sync::oneshot::channel();
        tokio::spawn(async move {
            let result = runtime.pause_workspace_owned(workspace_id).await;
            if let Err(result) = send.send(result)
                && let Ok(pause) = result
            {
                let _ = runtime.resume_workspace_owned(&pause).await;
            }
        });
        receive
            .await
            .map_err(|_| RuntimeError::WorkspaceRecoveryFailed)?
    }

    async fn resume_workspace(&self, pause: &WorkspacePause) -> Result<(), RuntimeError> {
        self.resume_workspace_owned(pause).await
    }

    async fn reconcile_recoveries(&self) -> Result<(), RuntimeError> {
        // Attest the restricted network if configured, caching it for start_spec.
        if let Some(ref name) = self.config.restricted_network {
            let needs_attest = self
                .restricted_network_attestation
                .lock()
                .unwrap()
                .is_none();
            if needs_attest {
                let attestation = self.attest_restricted_network(name).await?;
                *self.restricted_network_attestation.lock().unwrap() = Some(attestation);
            }
        }
        self.config
            .terminal_journal
            .reconcile_restart()
            .map_err(map_journal_error)?;
        let records = self
            .config
            .recovery_store
            .list()
            .map_err(|_| RuntimeError::WorkspaceRecoveryFailed)?;
        let managed = self.managed_containers(None).await?;
        let mut removal_failed = false;
        for (terminal_id, _) in &managed {
            removal_failed |= self.terminate_managed(*terminal_id).await.is_err();
        }
        if removal_failed || !self.managed_containers(None).await?.is_empty() {
            return Err(RuntimeError::WorkspaceRecoveryFailed);
        }
        for record in records {
            self.config
                .recovery_store
                .clear(record.workspace_id, record.operation_id)
                .map_err(|_| RuntimeError::WorkspaceRecoveryFailed)?;
        }
        Ok(())
    }

    async fn ensure_workspace_recovered(&self, workspace_id: Uuid) -> Result<(), RuntimeError> {
        let records = self
            .config
            .recovery_store
            .list()
            .map_err(|_| RuntimeError::WorkspaceRecoveryFailed)?;
        if let Some(record) = records
            .into_iter()
            .find(|record| record.workspace_id == workspace_id)
        {
            let terminal_ids = record
                .containers
                .iter()
                .map(|name| {
                    name.strip_prefix("gobrowse-")
                        .and_then(|value| Uuid::parse_str(value).ok())
                        .ok_or(RuntimeError::WorkspaceRecoveryFailed)
                })
                .collect::<Result<Vec<_>, _>>()?;
            self.resume_workspace_owned(&WorkspacePause {
                workspace_id,
                terminal_ids,
                recovery_id: Some(record.operation_id),
            })
            .await?;
        }
        self.remove_workspace_orphans(workspace_id).await
    }

    async fn shutdown(&self) -> Result<(), RuntimeError> {
        let terminal_ids = self
            .registry
            .lock()
            .await
            .sessions
            .values()
            .filter(|session| !session.lifecycle().is_terminal())
            .map(|session| session.terminal_id)
            .collect::<Vec<_>>();
        for terminal_id in terminal_ids {
            let _ = self.terminate(terminal_id).await;
        }
        let remaining = self.managed_containers(None).await?;
        for (terminal_id, _) in remaining {
            self.terminate_managed(terminal_id).await?;
        }
        Ok(())
    }
}

async fn drain_pty_output(
    output: &mut pty_process::OwnedReadPty,
    journal: &TerminalJournal,
    session: &Session,
) -> OutputDrain {
    let mut buffer = [0_u8; MAX_TERMINAL_OUTPUT_CHUNK_BYTES];
    let result = loop {
        match output.read(&mut buffer).await {
            Ok(0) => break OutputDrain::Complete,
            Ok(bytes) => match journal.append_output(session.terminal_id, &buffer[..bytes]) {
                Ok(accepted) if accepted == bytes => session.output_notify.notify_waiters(),
                Ok(_) => break OutputDrain::Failed("terminal_output_limit_exceeded"),
                Err(_) => break OutputDrain::Failed("terminal_output_journal_failed"),
            },
            Err(error) if error.raw_os_error() == Some(5) => break OutputDrain::Complete,
            Err(_) => break OutputDrain::Failed("pty_output_failed"),
        }
    };
    session.output_notify.notify_waiters();
    result
}

fn record_output_failure(journal: &TerminalJournal, session: &Session, reason: &str) {
    let record = journal.inspect(session.terminal_id).ok();
    let _ = journal.set_state(
        session.terminal_id,
        session.lifecycle().terminal_state(),
        record.as_ref().and_then(|record| record.exit_code),
        Some(reason),
        Some(false),
    );
}

fn configure_raw_pty(pty: &pty_process::Pty) -> Result<(), RuntimeError> {
    let mut attributes = rustix::termios::tcgetattr(pty).map_err(|_| RuntimeError::Pty)?;
    attributes.make_raw();
    rustix::termios::tcsetattr(pty, rustix::termios::OptionalActions::Now, &attributes)
        .map_err(|_| RuntimeError::Pty)
}

fn start_fingerprint(start: &ValidatedStart) -> Result<[u8; 32], RuntimeError> {
    let mut encoded =
        serde_json::to_vec(&start.request).map_err(|_| RuntimeError::InvalidConfiguration)?;
    encoded.extend_from_slice(start.network_name.as_bytes());
    Ok(Sha256::digest(encoded).into())
}

fn map_journal_error(error: JournalError) -> RuntimeError {
    match error {
        JournalError::NotFound => RuntimeError::NotFound,
        JournalError::Conflict => RuntimeError::Conflict,
        JournalError::OutcomeUnknown => RuntimeError::OutcomeUnknown,
        error => RuntimeError::Journal(error),
    }
}

fn exit_description(
    status: Option<&std::process::ExitStatus>,
    state: LifecycleState,
) -> (Option<i32>, String) {
    if state == LifecycleState::Terminated {
        return (
            status.and_then(std::process::ExitStatus::code),
            "terminated".into(),
        );
    }
    match status {
        Some(status) if status.code().is_some() => (
            status.code(),
            format!("exit_status_{}", status.code().unwrap_or_default()),
        ),
        Some(status) if status.signal().is_some() => (
            None,
            format!("exit_signal_{}", status.signal().unwrap_or_default()),
        ),
        Some(_) => (None, "exited".into()),
        None => (None, "wait_failed".into()),
    }
}

fn parse_processes(output: &[u8]) -> Result<Vec<SandboxProcess>, RuntimeError> {
    let output = std::str::from_utf8(output).map_err(|_| RuntimeError::PodmanFailed)?;
    let mut processes = Vec::new();
    let mut command_bytes = 0_usize;
    for line in output.lines().skip(1) {
        let mut fields = line.trim().splitn(2, char::is_whitespace);
        let pid = fields
            .next()
            .and_then(|value| value.parse::<u32>().ok())
            .filter(|pid| *pid != 0)
            .ok_or(RuntimeError::PodmanFailed)?;
        let command = fields.next().unwrap_or_default().trim().to_owned();
        command_bytes = command_bytes
            .checked_add(command.len())
            .ok_or(RuntimeError::PodmanFailed)?;
        if command.len() > MAX_TERMINAL_PROCESS_COMMAND_BYTES
            || command_bytes > MAX_TERMINAL_PROCESS_BYTES
            || processes.len() >= MAX_TERMINAL_PROCESSES
        {
            return Err(RuntimeError::PodmanFailed);
        }
        processes.push(SandboxProcess { pid, command });
    }
    Ok(processes)
}

pub fn effective_uid_from_proc_status(status: &str) -> Result<u32, RuntimeError> {
    let uid_line = status
        .lines()
        .find(|line| line.starts_with("Uid:"))
        .ok_or(RuntimeError::InvalidConfiguration)?;
    uid_line
        .split_ascii_whitespace()
        .nth(2)
        .ok_or(RuntimeError::InvalidConfiguration)?
        .parse()
        .map_err(|_| RuntimeError::InvalidConfiguration)
}

fn valid_image_reference(image: &str) -> bool {
    !image.is_empty()
        && image.len() <= 512
        && !image.starts_with('-')
        && image.chars().all(|character| {
            character.is_ascii_alphanumeric()
                || matches!(character, '/' | '.' | ':' | '-' | '_' | '@')
        })
}

fn valid_runtime_network(policy: gobrowse_core::sandbox::NetworkPolicy, name: &str) -> bool {
    match policy {
        gobrowse_core::sandbox::NetworkPolicy::None => name == "none",
        gobrowse_core::sandbox::NetworkPolicy::Full => name == "slirp4netns",
        gobrowse_core::sandbox::NetworkPolicy::Restricted => {
            name.starts_with("gobrowse-restricted-")
                && name.len() > "gobrowse-restricted-".len()
                && name.len() <= 128
                && name.chars().all(|character| {
                    character.is_ascii_alphanumeric() || matches!(character, '-' | '_')
                })
        }
    }
}

fn valid_workspace_volume_name(name: &str) -> bool {
    name.len() == "gobrowse-workspace-".len() + 36
        && name.starts_with("gobrowse-workspace-")
        && Uuid::parse_str(&name["gobrowse-workspace-".len()..]).is_ok()
}

fn cpu_limit(cpu_millis: u32) -> String {
    format!("{}.{:03}", cpu_millis / 1_000, cpu_millis % 1_000)
}

fn container_name(terminal_id: Uuid) -> String {
    format!("gobrowse-{terminal_id}")
}

#[cfg(test)]
mod tests {
    use std::{
        fs,
        io::Write,
        os::unix::fs::PermissionsExt,
        path::Path,
        sync::{Barrier, OnceLock},
    };

    use gobrowse_core::sandbox::{
        HARD_RESOURCE_LIMITS, MAX_TERMINAL_OUTPUT_BYTES, NetworkPolicy, ResourceLimits,
    };

    use super::*;

    async fn read_pty_until(output: &mut pty_process::OwnedReadPty, expected: &[u8]) -> Vec<u8> {
        tokio::time::timeout(Duration::from_secs(3), async {
            let mut collected = Vec::new();
            let mut buffer = [0_u8; 1024];
            while !collected
                .windows(expected.len())
                .any(|window| window == expected)
            {
                let bytes = output.read(&mut buffer).await.unwrap();
                assert_ne!(bytes, 0, "PTY closed before expected output");
                collected.extend_from_slice(&buffer[..bytes]);
                assert!(collected.len() <= 16 * 1024);
            }
            collected
        })
        .await
        .unwrap()
    }

    struct JournalFile(PathBuf);

    impl JournalFile {
        fn new() -> Self {
            Self(std::env::temp_dir().join(format!(
                "gobrowse-local-pty-journal-{}.sqlite3",
                Uuid::new_v4()
            )))
        }
    }

    impl Drop for JournalFile {
        fn drop(&mut self) {
            for suffix in ["", "-wal", "-shm"] {
                let _ = fs::remove_file(format!("{}{}", self.0.display(), suffix));
            }
        }
    }

    #[tokio::test]
    async fn local_pty_is_a_tty_and_supports_input_resize_interrupt_and_background_output() {
        let (pty, pts) = pty_process::open().unwrap();
        configure_raw_pty(&pty).unwrap();
        pty.resize(Size::new(40, 100)).unwrap();
        let script = r#"stty sane
trap 'printf "INTERRUPTED\n"; exit 130' INT
printf 'TTY=%s SIZE=%s\n' "$(test -t 0 && echo yes || echo no)" "$(stty size)"
IFS= read -r line
printf 'INPUT=%s\n' "$line"
(sleep 0.05; printf 'BACKGROUND\n') &
trap 'printf "RESIZED=%s\n" "$(stty size)"' WINCH
while :; do sleep 0.1; done"#;
        let mut child = pty_process::Command::new("/bin/sh")
            .arg("-c")
            .arg(script)
            .env_clear()
            .env("PATH", MINIMAL_PATH)
            .kill_on_drop(false)
            .spawn(pts)
            .unwrap();
        let (mut output, mut input) = pty.into_split();

        let initial = read_pty_until(&mut output, b"SIZE=40 100").await;
        assert!(initial.windows(7).any(|window| window == b"TTY=yes"));
        input.write_all(b"hello pty\n").await.unwrap();
        input.flush().await.unwrap();
        let echoed = read_pty_until(&mut output, b"BACKGROUND").await;
        assert!(
            echoed
                .windows(15)
                .any(|window| window == b"INPUT=hello pty")
        );

        input.resize(Size::new(55, 120)).unwrap();
        let resized = read_pty_until(&mut output, b"RESIZED=55 120").await;
        assert!(
            resized
                .windows(14)
                .any(|window| window == b"RESIZED=55 120")
        );

        input.write_all(&[3]).await.unwrap();
        input.flush().await.unwrap();
        let interrupted = read_pty_until(&mut output, b"INTERRUPTED").await;
        assert!(
            interrupted
                .windows(11)
                .any(|window| window == b"INTERRUPTED")
        );
        let status = tokio::time::timeout(Duration::from_secs(2), child.wait())
            .await
            .unwrap()
            .unwrap();
        assert_eq!(status.code(), Some(130));
    }

    #[tokio::test]
    async fn local_pty_output_replays_by_cursor_after_ack_reopen_and_restart() {
        let file = JournalFile::new();
        let journal = TerminalJournal::open(&file.0).unwrap();
        let terminal_id = Uuid::new_v4();
        let workspace_id = Uuid::new_v4();
        journal
            .begin_start(terminal_id, workspace_id, [5; 32], 80, 24)
            .unwrap();
        journal.set_running(terminal_id).unwrap();

        let (pty, pts) = pty_process::open().unwrap();
        configure_raw_pty(&pty).unwrap();
        let mut child = pty_process::Command::new("/bin/sh")
            .arg("-c")
            .arg("printf alpha; sleep 30")
            .kill_on_drop(false)
            .spawn(pts)
            .unwrap();
        let (mut output, input) = pty.into_split();
        let session = Arc::new(Session::new(terminal_id, workspace_id));
        assert!(session.transition(LifecycleState::Running));
        let output_journal = journal.clone();
        let output_session = Arc::clone(&session);
        let pump = tokio::spawn(async move {
            drain_pty_output(&mut output, &output_journal, &output_session).await
        });

        tokio::time::timeout(Duration::from_secs(3), async {
            while journal.inspect(terminal_id).unwrap().output_end_cursor < 5 {
                tokio::task::yield_now().await;
            }
        })
        .await
        .unwrap();
        let first = journal.read_output(terminal_id, 0, 3).unwrap();
        assert_eq!(first.bytes, b"alp");
        journal.ack_output(terminal_id, first.next_cursor).unwrap();

        let reopened = TerminalJournal::open(&file.0).unwrap();
        let replayed = reopened
            .read_output(terminal_id, first.next_cursor, 32)
            .unwrap();
        assert_eq!(replayed.bytes, b"ha");
        pump.abort();
        assert!(pump.await.unwrap_err().is_cancelled());
        assert_eq!(reopened.reconcile_restart().unwrap(), [terminal_id]);
        let lost = reopened.inspect(terminal_id).unwrap();
        assert_eq!(lost.state, TerminalState::Lost);
        assert!(!lost.output_complete);

        child.kill().await.unwrap();
        drop(input);
        child.wait().await.unwrap();
    }

    #[tokio::test]
    async fn local_pty_output_limit_stops_the_drain_and_is_durable() {
        let file = JournalFile::new();
        let journal = TerminalJournal::open(&file.0).unwrap();
        let terminal_id = Uuid::new_v4();
        let workspace_id = Uuid::new_v4();
        journal
            .begin_start(terminal_id, workspace_id, [6; 32], 80, 24)
            .unwrap();
        journal.set_running(terminal_id).unwrap();
        journal
            .set_output_bytes_for_test(terminal_id, MAX_TERMINAL_OUTPUT_BYTES - 10)
            .unwrap();

        let (pty, pts) = pty_process::open().unwrap();
        configure_raw_pty(&pty).unwrap();
        let mut child = pty_process::Command::new("/bin/sh")
            .arg("-c")
            .arg("printf 0123456789ABCDEFGHIJ; while :; do sleep 1; done")
            .kill_on_drop(false)
            .spawn(pts)
            .unwrap();
        let (mut output, input) = pty.into_split();
        let session = Session::new(terminal_id, workspace_id);
        assert!(session.transition(LifecycleState::Running));
        let result = tokio::time::timeout(
            Duration::from_secs(3),
            drain_pty_output(&mut output, &journal, &session),
        )
        .await
        .unwrap();
        assert_eq!(
            result,
            OutputDrain::Failed("terminal_output_limit_exceeded")
        );
        record_output_failure(&journal, &session, "terminal_output_limit_exceeded");
        let record = journal.inspect(terminal_id).unwrap();
        assert_eq!(record.output_end_cursor, 10);
        assert_eq!(
            record.reason.as_deref(),
            Some("terminal_output_limit_exceeded")
        );
        assert!(!record.output_complete);
        assert_eq!(
            journal.read_output(terminal_id, 0, 32).unwrap().bytes,
            b"0123456789"
        );

        drop(input);
        child.kill().await.unwrap();
        child.wait().await.unwrap();
    }

    #[tokio::test]
    async fn local_pty_concurrent_resize_write_kill_and_exit_do_not_deadlock() {
        let (pty, pts) = pty_process::open().unwrap();
        configure_raw_pty(&pty).unwrap();
        pty.resize(Size::new(24, 80)).unwrap();
        let mut child = pty_process::Command::new("/bin/sh")
            .arg("-c")
            .arg("printf READY; while :; do IFS= read -r line || exit; printf '%s' \"$line\"; done")
            .kill_on_drop(false)
            .spawn(pts)
            .unwrap();
        let (mut output, input) = pty.into_split();
        read_pty_until(&mut output, b"READY").await;
        let input = Arc::new(tokio::sync::Mutex::new(input));
        let start = Arc::new(tokio::sync::Barrier::new(3));

        let writer = {
            let input = Arc::clone(&input);
            let start = Arc::clone(&start);
            tokio::spawn(async move {
                start.wait().await;
                for _ in 0..1_000 {
                    let mut input = input.lock().await;
                    if input.write_all(b"line\n").await.is_err() {
                        break;
                    }
                    drop(input);
                    tokio::task::yield_now().await;
                }
            })
        };
        let resizer = {
            let input = Arc::clone(&input);
            let start = Arc::clone(&start);
            tokio::spawn(async move {
                start.wait().await;
                for index in 0..1_000_u16 {
                    let input = input.lock().await;
                    let _ = input.resize(Size::new(24 + index % 20, 80 + index % 40));
                    drop(input);
                    tokio::task::yield_now().await;
                }
            })
        };
        start.wait().await;
        tokio::task::yield_now().await;
        child.start_kill().unwrap();
        let status = tokio::time::timeout(Duration::from_secs(3), child.wait())
            .await
            .unwrap()
            .unwrap();
        tokio::time::timeout(Duration::from_secs(3), async {
            writer.await.unwrap();
            resizer.await.unwrap();
        })
        .await
        .unwrap();
        assert!(!status.success());
    }

    fn test_filesystem() -> &'static crate::Filesystem {
        static FILESYSTEM: OnceLock<crate::Filesystem> = OnceLock::new();
        FILESYSTEM.get_or_init(|| {
            let storage_root =
                std::env::temp_dir().join(format!("gobrowse-runtime-storage-{}", Uuid::new_v4()));
            fs::create_dir(&storage_root).unwrap();
            fs::set_permissions(&storage_root, fs::Permissions::from_mode(0o700)).unwrap();
            let filesystem = crate::Filesystem::new(&storage_root).unwrap();
            filesystem.ensure_workspace(Uuid::from_u128(1)).unwrap();
            filesystem
        })
    }

    fn test_terminal_journal() -> TerminalJournal {
        static JOURNAL: OnceLock<TerminalJournal> = OnceLock::new();
        JOURNAL
            .get_or_init(|| {
                TerminalJournal::open(std::env::temp_dir().join(format!(
                    "gobrowse-runtime-terminals-{}.sqlite3",
                    Uuid::new_v4()
                )))
                .unwrap()
            })
            .clone()
    }

    async fn recovery_test_lock() -> tokio::sync::MutexGuard<'static, ()> {
        static LOCK: OnceLock<tokio::sync::Mutex<()>> = OnceLock::new();
        LOCK.get_or_init(|| tokio::sync::Mutex::new(()))
            .lock()
            .await
    }

    fn config(provisioning: WorkspaceProvisioning) -> PodmanConfig {
        let filesystem = test_filesystem();
        PodmanConfig {
            executable: PathBuf::from("/usr/bin/podman"),
            image: "registry.example/gobrowse/sandbox@sha256:abc123".into(),
            readiness_timeout: Duration::from_secs(5),
            control_timeout: Duration::from_secs(5),
            termination_timeout: Duration::from_secs(5),
            input_write_timeout: Duration::from_secs(2),
            session_limits: SessionLimits {
                max_active: 4,
                max_active_per_workspace: 2,
                max_retained_records: 8,
            },
            workspace_provisioning: provisioning,
            deployment_id: Uuid::from_u128(2),
            workspace_resolver: filesystem.resolver(),
            recovery_store: filesystem.recovery_store(),
            terminal_journal: test_terminal_journal(),
            restricted_network: None,
        }
    }

    fn runtime() -> PodmanRuntime {
        PodmanRuntime::from_validated_config(config(WorkspaceProvisioning::NamedVolume {
            maximum_bytes: HARD_RESOURCE_LIMITS.writable_storage_bytes,
        }))
        .unwrap()
    }

    fn start(command: Vec<String>, network_name: &str) -> ValidatedStart {
        let network_policy = match network_name {
            "none" => NetworkPolicy::None,
            "slirp4netns" => NetworkPolicy::Full,
            _ => NetworkPolicy::Restricted,
        };
        ValidatedStart {
            terminal_id: Uuid::nil(),
            request: TerminalStartRequest {
                workspace_id: Uuid::from_u128(1),
                command,
                working_directory: "src".into(),
                cols: 80,
                rows: 24,
                network_policy,
                limits: ResourceLimits {
                    cpu_millis: 1_500,
                    memory_bytes: 512 * 1024 * 1024,
                    writable_storage_bytes: 1024 * 1024 * 1024,
                    pids: 64,
                    execution_seconds: 60,
                },
            },
            workspace_storage: test_filesystem()
                .workspace_storage(Uuid::from_u128(1))
                .unwrap(),
            network_name: network_name.into(),
        }
    }

    #[test]
    fn podman_argv_has_mandatory_isolation_and_no_shell() {
        let command = vec!["sh".into(), "-c".into(), "echo $HOST_SECRET; id".into()];
        let spec = runtime()
            .start_spec(&start(command.clone(), "gobrowse-restricted-egress"))
            .unwrap();
        assert_eq!(spec.program, Path::new("/usr/bin/podman"));
        for required in [
            "--interactive",
            "--tty",
            "--sig-proxy=false",
            "--detach-keys=",
            "--init",
            "--pull=never",
            "--log-driver=none",
            "--image-volume=ignore",
            "--http-proxy=false",
            "--cap-drop=ALL",
            "--security-opt=no-new-privileges",
            "--read-only",
            "--userns=keep-id",
            "--pid=private",
            "--ipc=private",
            "--uts=private",
            "--cgroupns=private",
        ] {
            assert!(
                spec.args.iter().any(|argument| argument == required),
                "{required}"
            );
        }
        assert!(!spec.args.iter().any(|argument| argument == "--rm"));
        assert!(!spec.args.iter().any(|argument| argument == "--privileged"));
        for label in [
            "io.gobrowse.managed=true",
            "io.gobrowse.deployment-id=00000000-0000-0000-0000-000000000002",
            "io.gobrowse.workspace-id=00000000-0000-0000-0000-000000000001",
            "io.gobrowse.terminal-id=00000000-0000-0000-0000-000000000000",
        ] {
            assert!(spec.args.iter().any(|argument| argument == label));
        }
        assert!(
            !spec
                .args
                .iter()
                .any(|argument| argument.contains("volume-opt"))
        );
        assert_eq!(&spec.args[spec.args.len() - command.len()..], command);
    }

    #[test]
    fn readiness_uses_positive_running_inspection() {
        assert_eq!(
            runtime().readiness_spec(Uuid::nil()).args,
            [
                "inspect",
                "--format",
                "{{.State.Running}}",
                "gobrowse-00000000-0000-0000-0000-000000000000"
            ]
        );
    }

    #[test]
    fn workspace_volume_identity_is_generated_and_allowlisted() {
        let runtime = runtime();
        let workspace_id = Uuid::from_u128(1);
        assert_eq!(
            runtime.volume_inspect_spec(workspace_id).args,
            [
                "volume",
                "inspect",
                "--format",
                "{{.Name}}|{{.Driver}}|{{json .Options}}|{{.Mountpoint}}",
                "gobrowse-workspace-00000000-0000-0000-0000-000000000001",
            ]
        );
        let spec = runtime
            .start_spec(&start(vec!["true".into()], "none"))
            .unwrap();
        assert!(spec.args.windows(2).any(|pair| {
            pair
                == [
                    "--volume",
                    "gobrowse-workspace-00000000-0000-0000-0000-000000000001:/workspace:rw,nodev,nosuid",
                ]
        }));
    }

    #[test]
    fn unmanaged_or_insufficient_workspace_quota_fails_closed() {
        let unmanaged =
            PodmanRuntime::from_validated_config(config(WorkspaceProvisioning::Unverified))
                .unwrap();
        assert!(matches!(
            unmanaged.start_spec(&start(vec!["true".into()], "none")),
            Err(RuntimeError::WorkspaceQuotaUnavailable)
        ));
        let limited =
            PodmanRuntime::from_validated_config(config(WorkspaceProvisioning::NamedVolume {
                maximum_bytes: 1,
            }))
            .unwrap();
        assert!(matches!(
            limited.start_spec(&start(vec!["true".into()], "none")),
            Err(RuntimeError::WorkspaceQuotaUnavailable)
        ));
    }

    #[tokio::test]
    async fn registry_enforces_global_workspace_and_record_bounds() {
        let limits = SessionLimits {
            max_active: 2,
            max_active_per_workspace: 1,
            max_retained_records: 1,
        };
        let workspace = Uuid::new_v4();
        let other_workspace = Uuid::new_v4();
        let mut registry = Registry::new();
        let make_session =
            |terminal_id, workspace_id| Arc::new(Session::new(terminal_id, workspace_id));
        let first = make_session(Uuid::new_v4(), workspace);
        registry.reserve(Arc::clone(&first), limits).unwrap();
        assert!(matches!(
            registry.reserve(make_session(Uuid::new_v4(), workspace), limits),
            Err(RuntimeError::SessionLimit)
        ));
        let second = make_session(Uuid::new_v4(), other_workspace);
        registry.reserve(Arc::clone(&second), limits).unwrap();
        assert!(matches!(
            registry.reserve(make_session(Uuid::new_v4(), Uuid::new_v4()), limits),
            Err(RuntimeError::SessionLimit)
        ));
        registry.complete(first.terminal_id, limits);
        registry.complete(second.terminal_id, limits);
        assert_eq!(registry.active, 0);
        assert_eq!(registry.completed.len(), 1);
        assert_eq!(registry.sessions.len(), 1);
    }

    #[test]
    fn terminal_transitions_are_monotonic_under_deterministic_races() {
        for final_state in [LifecycleState::Exited, LifecycleState::Terminated] {
            let session = Arc::new(Session::new(Uuid::new_v4(), Uuid::new_v4()));
            assert!(session.transition(LifecycleState::Running));
            let start = Arc::new(Barrier::new(2));
            let terminal_committed = Arc::new(Barrier::new(2));

            let terminal = {
                let session = Arc::clone(&session);
                let start = Arc::clone(&start);
                let terminal_committed = Arc::clone(&terminal_committed);
                std::thread::spawn(move || {
                    start.wait();
                    assert!(session.transition(final_state));
                    terminal_committed.wait();
                })
            };
            let stale_cleanup = {
                let session = Arc::clone(&session);
                std::thread::spawn(move || {
                    start.wait();
                    terminal_committed.wait();
                    assert!(!session.transition(LifecycleState::Terminating));
                    assert!(!session.transition(LifecycleState::Unrecoverable));
                    assert!(!session.transition(LifecycleState::Running));
                })
            };
            terminal.join().unwrap();
            stale_cleanup.join().unwrap();
            assert_eq!(session.lifecycle(), final_state);
        }

        let session = Session::new(Uuid::new_v4(), Uuid::new_v4());
        assert!(session.transition(LifecycleState::Running));
        assert!(session.transition(LifecycleState::Terminating));
        assert!(session.transition(LifecycleState::Unrecoverable));
        assert!(session.transition(LifecycleState::Exited));
        assert!(!session.transition(LifecycleState::Terminated));
        assert_eq!(session.lifecycle(), LifecycleState::Exited);
    }

    #[test]
    fn podman_argv_rejects_option_like_images_and_namespace_network_values() {
        assert!(
            PodmanRuntime::from_validated_config(PodmanConfig {
                image: "--privileged".into(),
                ..config(WorkspaceProvisioning::NamedVolume {
                    maximum_bytes: HARD_RESOURCE_LIMITS.writable_storage_bytes,
                })
            })
            .is_err()
        );
        assert!(
            runtime()
                .start_spec(&start(vec!["true".into()], "container:host"))
                .is_err()
        );
        let spec = runtime()
            .start_spec(&start(vec!["true".into()], "none"))
            .unwrap();
        let volume = spec
            .args
            .windows(2)
            .find(|pair| pair[0] == "--volume")
            .unwrap()[1]
            .clone();
        assert_eq!(
            volume,
            "gobrowse-workspace-00000000-0000-0000-0000-000000000001:/workspace:rw,nodev,nosuid"
        );
    }

    #[test]
    fn podman_process_receives_only_minimal_environment() {
        let spec = runtime()
            .start_spec(&start(vec!["true".into()], "none"))
            .unwrap();
        let command = spec.command();
        let environment = command
            .as_std()
            .get_envs()
            .map(|(key, value)| {
                (
                    key.to_string_lossy().into_owned(),
                    value.map(|value| value.to_string_lossy().into_owned()),
                )
            })
            .collect::<Vec<_>>();
        assert_eq!(
            environment,
            vec![
                ("HOME".into(), Some("/tmp".into())),
                ("PATH".into(), Some("/usr/bin:/bin".into())),
            ]
        );
    }

    #[test]
    fn none_network_is_rendered_as_none() {
        let spec = runtime()
            .start_spec(&start(vec!["true".into()], "none"))
            .unwrap();
        let index = spec
            .args
            .iter()
            .position(|argument| argument == "--network")
            .unwrap();
        assert_eq!(spec.args[index + 1], "none");
    }

    #[test]
    fn effective_uid_parser_uses_effective_not_real_uid() {
        assert_eq!(
            effective_uid_from_proc_status("Name:\ttest\nUid:\t1000\t1001\t1000\t1000\n").unwrap(),
            1001
        );
        assert!(effective_uid_from_proc_status("Name:\ttest\n").is_err());
    }

    #[test]
    fn hard_limits_fit_podman_spec() {
        let mut value = start(vec!["true".into()], "none");
        value.request.limits = HARD_RESOURCE_LIMITS;
        let spec = runtime().start_spec(&value).unwrap();
        assert!(
            spec.args
                .windows(2)
                .any(|pair| pair == ["--pids-limit", "1024"])
        );
    }

    struct FakePodman {
        root: PathBuf,
        executable: PathBuf,
        log: PathBuf,
        marker: PathBuf,
        container_info: PathBuf,
        control_failures: PathBuf,
        pause_block: PathBuf,
        pause_entered: PathBuf,
        pause_inspect_block: PathBuf,
        pause_inspect_entered: PathBuf,
        unpause_failures: PathBuf,
        output_trigger: PathBuf,
        network_info: PathBuf,
    }

    impl FakePodman {
        fn new() -> Self {
            let root =
                std::env::temp_dir().join(format!("gobrowse-fake-podman-{}", Uuid::new_v4()));
            fs::create_dir(&root).unwrap();
            let executable = root.join("podman");
            let temporary_executable = root.join("podman.new");
            let marker = root.join("running");
            let log = root.join("commands.log");
            let control_failures = root.join("control-failures");
            let resize_failure = root.join("resize-failure");
            let slow_readiness = root.join("slow-readiness");
            let paused = root.join("paused");
            let container_info = root.join("container-info");
            let pause_block = root.join("pause-block");
            let pause_entered = root.join("pause-entered");
            let pause_inspect_block = root.join("pause-inspect-block");
            let pause_inspect_entered = root.join("pause-inspect-entered");
            let unpause_failures = root.join("unpause-failures");
            let output_trigger = root.join("output-trigger");
            let network_info = root.join("network-info");
            let volume_mountpoint = test_filesystem()
                .resolver()
                .expected_mountpoint(Uuid::from_u128(1));
            let script = format!(
                r#"#!/bin/sh
marker="{}"
log="{}"
control_failures="{}"
resize_failure="{}"
slow_readiness="{}"
paused="{}"
container_info="{}"
pause_block="{}"
pause_entered="{}"
pause_inspect_block="{}"
pause_inspect_entered="{}"
unpause_failures="{}"
output_trigger="{}"
network_info="{}"
printf '%s\n' "$*" >> "$log"
case "$1" in
  volume)
    [ "$2" = inspect ] || exit 1
    name="$5"
    printf '%s|local|{{}}|{}\n' "$name"
    ;;
  network)
    [ "$2" = inspect ] || exit 1
    if [ -e "$network_info" ]; then
      cat "$network_info"
    else
      exit 1
    fi
    ;;
  run)
    mode=hold
    for argument in "$@"; do
      case "$argument" in
        io.gobrowse.terminal-id=*) terminal="${{argument#*=}}" ;;
        io.gobrowse.workspace-id=*) workspace="${{argument#*=}}" ;;
      esac
      [ "$argument" = natural ] && mode=natural
      [ "$argument" = never-ready ] && mode=never-ready
      [ "$argument" = control-recover ] && printf '2\n' > "$control_failures"
      [ "$argument" = control-persistent ] && printf 'persistent\n' > "$control_failures"
      [ "$argument" = resize-fail ] && : > "$resize_failure"
       [ "$argument" = slow-ready ] && : > "$slow_readiness"
      [ "$argument" = output-overflow ] && mode=output-overflow
    done
    printf 'gobrowse-%s|%s|%s\n' "$terminal" "$terminal" "$workspace" > "$container_info"
    if [ "$mode" = never-ready ]; then sleep 0.30; exit 0; fi
    : > "$marker"
    if [ "$mode" = natural ]; then sleep 0.20; rm -f "$marker"; exit 0; fi
    if [ "$mode" = output-overflow ]; then
      while [ ! -e "$output_trigger" ] && [ -e "$marker" ]; do sleep 0.01; done
      [ -e "$marker" ] || exit 0
      printf '0123456789ABCDEFGHIJ'
    fi
    while [ -e "$marker" ]; do sleep 0.05; done
    ;;
  ps)
    [ -e "$marker" ] && cat "$container_info"
    true
    ;;
  inspect)
    if [ "$3" = "{{{{.State.Paused}}}}" ]; then
      if [ -e "$pause_inspect_block" ]; then
        : > "$pause_inspect_entered"
        while [ -e "$pause_inspect_block" ]; do sleep 0.01; done
      fi
      [ -e "$paused" ] && printf 'true\n' || printf 'false\n'
      exit 0
    fi
    if [ -e "$slow_readiness" ]; then sleep 0.20; rm -f "$slow_readiness"; fi
    [ -e "$marker" ] && printf 'true\n' || exit 1
    ;;
  pause)
    [ -e "$marker" ] || exit 1
    : > "$pause_entered"
    while [ -e "$pause_block" ]; do sleep 0.01; done
    : > "$paused"
    ;;
  unpause)
    [ -e "$paused" ] || exit 1
    if [ -e "$unpause_failures" ]; then
      failures=$(cat "$unpause_failures")
      if [ "$failures" -gt 0 ]; then
        printf '%s\n' "$((failures - 1))" > "$unpause_failures"
        exit 1
      fi
    fi
    rm -f "$paused"
    ;;
  container)
    [ "$2" = resize ] || exit 1
    [ ! -e "$resize_failure" ] || exit 1
    ;;
  stop|rm)
    if [ -e "$control_failures" ]; then
      failures=$(cat "$control_failures")
      [ "$failures" = persistent ] && exit 1
      if [ "$failures" -gt 0 ]; then
        printf '%s\n' "$((failures - 1))" > "$control_failures"
        exit 1
      fi
    fi
    rm -f "$marker" "$paused" "$container_info"
    ;;
esac
"#,
                marker.display(),
                log.display(),
                control_failures.display(),
                resize_failure.display(),
                slow_readiness.display(),
                paused.display(),
                container_info.display(),
                pause_block.display(),
                pause_entered.display(),
                pause_inspect_block.display(),
                pause_inspect_entered.display(),
                unpause_failures.display(),
                output_trigger.display(),
                network_info.display(),
                volume_mountpoint.display(),
            );
            let mut file = fs::File::create(&temporary_executable).unwrap();
            file.write_all(script.as_bytes()).unwrap();
            file.sync_all().unwrap();
            drop(file);
            fs::set_permissions(&temporary_executable, fs::Permissions::from_mode(0o700)).unwrap();
            fs::rename(&temporary_executable, &executable).unwrap();
            fs::File::open(&root).unwrap().sync_all().unwrap();
            Self {
                root,
                executable,
                log,
                marker,
                container_info,
                control_failures,
                pause_block,
                pause_entered,
                pause_inspect_block,
                pause_inspect_entered,
                unpause_failures,
                output_trigger,
                network_info,
            }
        }

        fn runtime(&self, readiness_timeout: Duration) -> PodmanRuntime {
            PodmanRuntime::from_validated_config(PodmanConfig {
                executable: self.executable.clone(),
                readiness_timeout,
                control_timeout: Duration::from_millis(100),
                termination_timeout: Duration::from_millis(300),
                input_write_timeout: Duration::from_millis(50),
                ..config(WorkspaceProvisioning::NamedVolume {
                    maximum_bytes: HARD_RESOURCE_LIMITS.writable_storage_bytes,
                })
            })
            .unwrap()
        }

        fn runtime_restricted(&self, readiness_timeout: Duration, network: &str) -> PodmanRuntime {
            PodmanRuntime::from_validated_config(PodmanConfig {
                executable: self.executable.clone(),
                readiness_timeout,
                control_timeout: Duration::from_millis(100),
                termination_timeout: Duration::from_millis(300),
                input_write_timeout: Duration::from_millis(50),
                restricted_network: Some(network.into()),
                ..config(WorkspaceProvisioning::NamedVolume {
                    maximum_bytes: HARD_RESOURCE_LIMITS.writable_storage_bytes,
                })
            })
            .unwrap()
        }

        fn command_log(&self) -> String {
            fs::read_to_string(&self.log).unwrap_or_default()
        }

        fn allow_control_recovery(&self) {
            let _ = fs::remove_file(&self.control_failures);
        }

        fn block_pause(&self) {
            fs::write(&self.pause_block, b"").unwrap();
        }

        fn release_pause(&self) {
            let _ = fs::remove_file(&self.pause_block);
        }

        fn block_pause_inspect(&self) {
            fs::write(&self.pause_inspect_block, b"").unwrap();
        }

        fn release_pause_inspect(&self) {
            let _ = fs::remove_file(&self.pause_inspect_block);
        }

        fn fail_unpause(&self, attempts: usize) {
            fs::write(&self.unpause_failures, attempts.to_string()).unwrap();
        }

        fn trigger_output(&self) {
            fs::write(&self.output_trigger, b"").unwrap();
        }

        fn seed_survivor(&self, terminal_id: Uuid, workspace_id: Uuid) {
            fs::write(&self.marker, b"").unwrap();
            fs::write(
                &self.container_info,
                format!(
                    "{}|{}|{}\n",
                    container_name(terminal_id),
                    terminal_id,
                    workspace_id
                ),
            )
            .unwrap();
        }

        fn write_network_info(&self, name: &str, subnet: &str, gateway: &str, dns: &str) {
            let line = format!("{name}|{subnet}|{gateway}|{dns}\n");
            fs::write(&self.network_info, line.as_bytes()).unwrap();
            let _ = fs::remove_file(&self.marker);
        }
    }

    impl Drop for FakePodman {
        fn drop(&mut self) {
            let _ = fs::remove_dir_all(&self.root);
        }
    }

    #[tokio::test]
    async fn start_waits_for_readiness_and_terminal_transitions_are_coherent() {
        let _recovery_lock = recovery_test_lock().await;
        let fake = FakePodman::new();
        let runtime = fake.runtime(Duration::from_millis(500));
        let terminal_id = Uuid::new_v4();
        runtime
            .start(ValidatedStart {
                terminal_id,
                ..start(vec!["hold".into()], "none")
            })
            .await
            .unwrap();
        assert_eq!(
            runtime.inspect(terminal_id).await.unwrap().state,
            TerminalState::Running
        );
        assert!(
            fake.command_log()
                .contains("container resize --width 80 --height 24")
        );
        runtime.terminate(terminal_id).await.unwrap();
        assert_eq!(
            runtime.inspect(terminal_id).await.unwrap().state,
            TerminalState::Terminated
        );

        let naturally_exited = Uuid::new_v4();
        runtime
            .start(ValidatedStart {
                terminal_id: naturally_exited,
                ..start(vec!["natural".into()], "none")
            })
            .await
            .unwrap();
        tokio::time::timeout(Duration::from_secs(1), async {
            loop {
                if runtime.inspect(naturally_exited).await.unwrap().state == TerminalState::Exited {
                    break;
                }
                tokio::time::sleep(Duration::from_millis(20)).await;
            }
        })
        .await
        .unwrap();
    }

    #[tokio::test]
    async fn journal_output_limit_terminates_the_foreground_pty_workload() {
        let _recovery_lock = recovery_test_lock().await;
        let fake = FakePodman::new();
        let runtime = fake.runtime(Duration::from_millis(500));
        let terminal_id = Uuid::new_v4();
        runtime
            .start(ValidatedStart {
                terminal_id,
                ..start(vec!["output-overflow".into()], "none")
            })
            .await
            .unwrap();
        runtime
            .config
            .terminal_journal
            .set_output_bytes_for_test(terminal_id, MAX_TERMINAL_OUTPUT_BYTES - 10)
            .unwrap();
        fake.trigger_output();

        tokio::time::timeout(Duration::from_secs(2), async {
            loop {
                let inspected = runtime.inspect(terminal_id).await.unwrap();
                if inspected.state == TerminalState::Terminated {
                    assert_eq!(
                        inspected.reason.as_deref(),
                        Some("terminal_output_limit_exceeded")
                    );
                    assert!(!inspected.output_complete);
                    assert_eq!(inspected.output_end_cursor, 10);
                    break;
                }
                tokio::time::sleep(Duration::from_millis(10)).await;
            }
        })
        .await
        .unwrap();
        assert!(runtime.managed_containers(None).await.unwrap().is_empty());
    }

    #[tokio::test]
    async fn startup_marks_unattachable_pty_lost_and_removes_surviving_container() {
        let _recovery_lock = recovery_test_lock().await;
        let fake = FakePodman::new();
        let runtime = fake.runtime(Duration::from_millis(500));
        let terminal_id = Uuid::new_v4();
        let workspace_id = Uuid::from_u128(1);
        runtime
            .config
            .terminal_journal
            .begin_start(terminal_id, workspace_id, [8; 32], 90, 30)
            .unwrap();
        runtime
            .config
            .terminal_journal
            .set_running(terminal_id)
            .unwrap();
        fake.seed_survivor(terminal_id, workspace_id);

        runtime.reconcile_recoveries().await.unwrap();
        let inspected = runtime.inspect(terminal_id).await.unwrap();
        assert_eq!(inspected.state, TerminalState::Lost);
        assert_eq!(inspected.reason.as_deref(), Some("daemon_restart_pty_lost"));
        assert!(!inspected.output_complete);
        assert!(runtime.managed_containers(None).await.unwrap().is_empty());
    }

    #[tokio::test]
    async fn volume_labels_are_verified_and_active_workspace_pause_is_positive() {
        let _recovery_lock = recovery_test_lock().await;
        let fake = FakePodman::new();
        let runtime = fake.runtime(Duration::from_millis(500));
        let terminal_id = Uuid::new_v4();
        let workspace_id = Uuid::from_u128(1);
        runtime
            .start(ValidatedStart {
                terminal_id,
                ..start(vec!["hold".into()], "none")
            })
            .await
            .unwrap();
        let pause = runtime.pause_workspace(workspace_id).await.unwrap();
        assert_eq!(pause.workspace_id, workspace_id);
        assert_eq!(pause.terminal_ids, [terminal_id]);
        runtime.resume_workspace(&pause).await.unwrap();
        let log = fake.command_log();
        assert!(log.contains(&format!("pause {}", container_name(terminal_id))));
        assert!(log.contains("{{.State.Paused}}"));
        assert!(log.contains(&format!("unpause {}", container_name(terminal_id))));
        runtime.terminate(terminal_id).await.unwrap();

        let invalid_runtime = PodmanRuntime::from_validated_config(PodmanConfig {
            executable: fake.executable.clone(),
            workspace_provisioning: WorkspaceProvisioning::NamedVolume { maximum_bytes: 2 },
            ..config(WorkspaceProvisioning::NamedVolume { maximum_bytes: 2 })
        })
        .unwrap();
        let mut invalid_start = start(vec!["true".into()], "none");
        invalid_start.request.limits.writable_storage_bytes = 1;
        invalid_start.workspace_storage.inode += 1;
        assert!(matches!(
            invalid_runtime.start(invalid_start).await,
            Err(RuntimeError::WorkspaceVolumeInvalid)
        ));
    }

    #[tokio::test]
    async fn pause_cancellation_unpause_retry_and_restart_recovery_are_durable() {
        let _recovery_lock = recovery_test_lock().await;
        let fake = FakePodman::new();
        let runtime = Arc::new(fake.runtime(Duration::from_millis(500)));
        let workspace_id = Uuid::from_u128(1);
        let terminal_id = Uuid::new_v4();
        runtime
            .start(ValidatedStart {
                terminal_id,
                ..start(vec!["hold".into()], "none")
            })
            .await
            .unwrap();

        fake.block_pause();
        let pausing = {
            let runtime = Arc::clone(&runtime);
            tokio::spawn(async move { runtime.pause_workspace(workspace_id).await })
        };
        tokio::time::timeout(Duration::from_secs(1), async {
            while !fake.pause_entered.exists()
                || runtime.config.recovery_store.list().unwrap().is_empty()
            {
                tokio::task::yield_now().await;
            }
        })
        .await
        .unwrap();
        pausing.abort();
        fake.release_pause();
        tokio::time::timeout(Duration::from_secs(1), async {
            while !runtime.config.recovery_store.list().unwrap().is_empty() {
                tokio::time::sleep(Duration::from_millis(10)).await;
            }
        })
        .await
        .unwrap();

        fake.block_pause_inspect();
        let pausing = {
            let runtime = Arc::clone(&runtime);
            tokio::spawn(async move { runtime.pause_workspace(workspace_id).await })
        };
        tokio::time::timeout(Duration::from_secs(1), async {
            while !fake.pause_inspect_entered.exists() {
                tokio::task::yield_now().await;
            }
        })
        .await
        .unwrap();
        pausing.abort();
        fake.release_pause_inspect();
        tokio::time::timeout(Duration::from_secs(1), async {
            while !runtime.config.recovery_store.list().unwrap().is_empty() {
                tokio::time::sleep(Duration::from_millis(10)).await;
            }
        })
        .await
        .unwrap();

        let pause = runtime.pause_workspace(workspace_id).await.unwrap();
        fake.fail_unpause(2);
        runtime.resume_workspace(&pause).await.unwrap();
        assert!(runtime.config.recovery_store.list().unwrap().is_empty());

        let pause = runtime.pause_workspace(workspace_id).await.unwrap();
        fake.fail_unpause(10);
        assert!(matches!(
            runtime.resume_workspace(&pause).await,
            Err(RuntimeError::WorkspaceResumeFailed)
        ));
        assert!(!runtime.config.recovery_store.list().unwrap().is_empty());
        let blocked = runtime
            .start(ValidatedStart {
                terminal_id: Uuid::new_v4(),
                ..start(vec!["hold".into()], "none")
            })
            .await;
        assert!(matches!(blocked, Err(RuntimeError::WorkspaceResumeFailed)));

        let restarted = fake.runtime(Duration::from_millis(500));
        restarted.reconcile_recoveries().await.unwrap();
        assert!(restarted.config.recovery_store.list().unwrap().is_empty());
        assert!(restarted.managed_containers(None).await.unwrap().is_empty());
    }

    #[tokio::test]
    async fn start_fails_when_positive_readiness_is_not_observed() {
        let _recovery_lock = recovery_test_lock().await;
        let fake = FakePodman::new();
        let runtime = fake.runtime(Duration::from_millis(50));
        let result = runtime
            .start(ValidatedStart {
                terminal_id: Uuid::new_v4(),
                ..start(vec!["never-ready".into()], "none")
            })
            .await;
        assert!(
            matches!(result, Err(RuntimeError::ReadinessFailed)),
            "{result:?}"
        );
    }

    #[tokio::test]
    async fn cancellation_during_start_arms_container_cleanup() {
        let _recovery_lock = recovery_test_lock().await;
        let fake = FakePodman::new();
        let runtime = Arc::new(fake.runtime(Duration::from_millis(500)));
        let terminal_id = Uuid::new_v4();
        let starting = {
            let runtime = Arc::clone(&runtime);
            tokio::spawn(async move {
                runtime
                    .start(ValidatedStart {
                        terminal_id,
                        ..start(vec!["slow-ready".into()], "none")
                    })
                    .await
            })
        };
        tokio::time::timeout(Duration::from_secs(1), async {
            while !fake
                .command_log()
                .lines()
                .any(|line| line.starts_with("run "))
            {
                tokio::task::yield_now().await;
            }
        })
        .await
        .unwrap();
        starting.abort();
        assert!(starting.await.unwrap_err().is_cancelled());
        tokio::time::timeout(Duration::from_secs(1), async {
            loop {
                if runtime.inspect(terminal_id).await.unwrap().state == TerminalState::Terminated {
                    break;
                }
                tokio::time::sleep(Duration::from_millis(10)).await;
            }
        })
        .await
        .unwrap();
    }

    #[tokio::test]
    async fn initial_resize_failure_cleans_up_and_fails_start() {
        let _recovery_lock = recovery_test_lock().await;
        let fake = FakePodman::new();
        let runtime = fake.runtime(Duration::from_millis(500));
        let terminal_id = Uuid::new_v4();
        let result = runtime
            .start(ValidatedStart {
                terminal_id,
                ..start(vec!["resize-fail".into()], "none")
            })
            .await;
        assert!(matches!(result, Err(RuntimeError::PodmanFailed)));
        let log = fake.command_log();
        assert!(log.contains("container resize --width 80 --height 24"));
        assert!(log.contains("stop --ignore --time 1"));
    }

    #[tokio::test]
    async fn failed_control_commands_release_ownership_and_recover_within_deadline() {
        let _recovery_lock = recovery_test_lock().await;
        let fake = FakePodman::new();
        let mut runtime = fake.runtime(Duration::from_millis(500));
        runtime.config.control_timeout = Duration::from_millis(500);
        runtime.config.termination_timeout = Duration::from_secs(2);
        let terminal_id = Uuid::new_v4();
        runtime
            .start(ValidatedStart {
                terminal_id,
                ..start(vec!["control-recover".into()], "none")
            })
            .await
            .unwrap();
        runtime.terminate(terminal_id).await.unwrap();
        assert_eq!(
            runtime.inspect(terminal_id).await.unwrap().state,
            TerminalState::Terminated
        );
        assert!(fake.command_log().matches("stop --ignore --time 1").count() >= 2);
    }

    #[tokio::test]
    async fn later_terminate_retries_after_unrecoverable_control_failure() {
        let _recovery_lock = recovery_test_lock().await;
        let fake = FakePodman::new();
        let runtime = fake.runtime(Duration::from_millis(500));
        let terminal_id = Uuid::new_v4();
        runtime
            .start(ValidatedStart {
                terminal_id,
                ..start(vec!["control-persistent".into()], "none")
            })
            .await
            .unwrap();
        assert!(matches!(
            runtime.terminate(terminal_id).await,
            Err(RuntimeError::ControlTimeout)
        ));
        assert_eq!(
            runtime.inspect(terminal_id).await.unwrap().state,
            TerminalState::Unrecoverable
        );
        fake.allow_control_recovery();
        runtime.terminate(terminal_id).await.unwrap();
        assert_eq!(
            runtime.inspect(terminal_id).await.unwrap().state,
            TerminalState::Terminated
        );
    }

    #[tokio::test]
    async fn execution_timeout_keeps_cleaning_after_unrecoverable_deadline() {
        let _recovery_lock = recovery_test_lock().await;
        let fake = FakePodman::new();
        let runtime = fake.runtime(Duration::from_millis(500));
        let terminal_id = Uuid::new_v4();
        runtime
            .start(ValidatedStart {
                terminal_id,
                ..start(vec!["control-persistent".into()], "none")
            })
            .await
            .unwrap();
        let session = runtime.session(terminal_id).await.unwrap();
        runtime.spawn_timeout(session, Duration::from_millis(10));
        tokio::time::timeout(Duration::from_secs(1), async {
            loop {
                if runtime.inspect(terminal_id).await.unwrap().state == TerminalState::Unrecoverable
                {
                    break;
                }
                tokio::time::sleep(Duration::from_millis(10)).await;
            }
        })
        .await
        .unwrap();
        fake.allow_control_recovery();
        tokio::time::timeout(Duration::from_secs(1), async {
            loop {
                if runtime.inspect(terminal_id).await.unwrap().state == TerminalState::Terminated {
                    break;
                }
                tokio::time::sleep(Duration::from_millis(10)).await;
            }
        })
        .await
        .unwrap();
        let commands_after_exit = fake.command_log();
        tokio::time::sleep(Duration::from_millis(100)).await;
        assert_eq!(fake.command_log(), commands_after_exit);
    }

    #[tokio::test]
    async fn partial_terminal_input_is_unknown_and_is_not_retried_or_killed() {
        let _recovery_lock = recovery_test_lock().await;
        let fake = FakePodman::new();
        let runtime = fake.runtime(Duration::from_millis(500));
        let terminal_id = Uuid::new_v4();
        runtime
            .start(ValidatedStart {
                terminal_id,
                ..start(vec!["hold".into()], "none")
            })
            .await
            .unwrap();
        let input_id = Uuid::new_v4();
        assert!(matches!(
            runtime
                .input(terminal_id, input_id, &vec![0; 8 * 1024 * 1024],)
                .await,
            Err(RuntimeError::OutcomeUnknown)
        ));
        assert!(matches!(
            runtime
                .input(terminal_id, input_id, &vec![0; 8 * 1024 * 1024])
                .await,
            Err(RuntimeError::OutcomeUnknown)
        ));
        assert_eq!(
            runtime.inspect(terminal_id).await.unwrap().state,
            TerminalState::Running
        );
        runtime.terminate(terminal_id).await.unwrap();
    }

    #[tokio::test]
    async fn restricted_network_attestation_rejects_metadata_and_private_ranges() {
        let _recovery_lock = recovery_test_lock().await;
        let fake = FakePodman::new();
        // Gateway 10.0.0.1 is a private address – attestation must reject it.
        fake.write_network_info(
            "gobrowse-restricted-bad",
            "10.0.0.0/24",
            "10.0.0.1",
            r#"["8.8.8.8"]"#,
        );
        let runtime =
            fake.runtime_restricted(Duration::from_millis(500), "gobrowse-restricted-bad");
        let result = runtime.reconcile_recoveries().await;
        assert!(
            matches!(result, Err(RuntimeError::InvalidConfiguration)),
            "expected InvalidConfiguration for private gateway, got {result:?}"
        );

        // Also verify that a metadata address (169.254.169.254) in DNS is rejected.
        let fake2 = FakePodman::new();
        fake2.write_network_info(
            "gobrowse-restricted-md",
            "93.184.216.0/24",
            "93.184.216.1",
            r#"["169.254.169.254"]"#,
        );
        let runtime2 =
            fake2.runtime_restricted(Duration::from_millis(500), "gobrowse-restricted-md");
        let result2 = runtime2.reconcile_recoveries().await;
        assert!(
            matches!(result2, Err(RuntimeError::InvalidConfiguration)),
            "expected InvalidConfiguration for metadata DNS, got {result2:?}"
        );
    }

    #[tokio::test]
    async fn restricted_network_attestation_accepts_public_only() {
        let _recovery_lock = recovery_test_lock().await;
        let fake = FakePodman::new();
        // Subnet 93.184.216.0/24, gateway 93.184.216.1, DNS 1.1.1.1 — all public.
        fake.write_network_info(
            "gobrowse-restricted-egress",
            "93.184.216.0/24",
            "93.184.216.1",
            r#"["1.1.1.1"]"#,
        );
        let runtime =
            fake.runtime_restricted(Duration::from_millis(500), "gobrowse-restricted-egress");
        runtime.reconcile_recoveries().await.unwrap();
        // After attestation, start_spec with NetworkPolicy::Restricted must succeed.
        let spec = runtime
            .start_spec(&start(vec!["true".into()], "gobrowse-restricted-egress"))
            .unwrap();
        let index = spec.args.iter().position(|arg| arg == "--network").unwrap();
        assert_eq!(spec.args[index + 1], "gobrowse-restricted-egress");
    }

    #[tokio::test]
    async fn serve_until_reconciles_restart_and_surfaces_lost_state_and_durable_output_through_the_wire()
     {
        use crate::{
            Authenticator, ConnectionConfig, Daemon, DaemonConfig, NetworkPolicyConfig,
            SocketConfig,
        };
        use base64::{Engine, engine::general_purpose::STANDARD as BASE64};
        use gobrowse_core::sandbox::{
            RequestEnvelope, SANDBOX_PROTOCOL_VERSION, SandboxOperation, SandboxResult,
            TerminalState,
        };

        let _recovery_lock = recovery_test_lock().await;
        let fake = FakePodman::new();
        let runtime = fake.runtime(Duration::from_millis(500));

        let terminal_id = Uuid::from_u128(100);
        let workspace_id = Uuid::from_u128(1);
        let pending_input_id = Uuid::from_u128(200);

        // Seed PRE-RESTART state in the shared journal.
        let journal = test_terminal_journal();
        journal
            .begin_start(terminal_id, workspace_id, [7; 32], 80, 24)
            .unwrap();
        journal.set_running(terminal_id).unwrap();
        journal.append_output(terminal_id, b"replay-me").unwrap();
        journal
            .begin_input(terminal_id, pending_input_id, [9; 32])
            .unwrap();

        // Seed a survivor container so reconciliation has something to clean.
        fake.seed_survivor(terminal_id, workspace_id);

        // Build a Daemon sharing the same journal and filesystem root as the runtime.
        let uid = effective_uid_from_proc_status(&fs::read_to_string("/proc/self/status").unwrap())
            .unwrap();
        let socket_dir =
            std::env::temp_dir().join(format!("gobrowse-sandboxd-reconcile-{}", Uuid::new_v4()));
        fs::create_dir(&socket_dir).unwrap();
        fs::set_permissions(&socket_dir, fs::Permissions::from_mode(0o700)).unwrap();

        let daemon = Daemon::new(
            DaemonConfig {
                socket: SocketConfig {
                    path: socket_dir.join("sandboxd.sock"),
                    mode: 0o600,
                    owner_uid: uid,
                    allowed_peer_uid: uid,
                },
                resource_ceiling: HARD_RESOURCE_LIMITS,
                network: NetworkPolicyConfig {
                    restricted_network: None,
                    allow_full: false,
                },
                replay_capacity: 32,
                connections: ConnectionConfig {
                    max_connections: 4,
                    pre_auth_timeout: Duration::from_secs(1),
                    idle_timeout: Duration::from_secs(1),
                    operation_timeout: Duration::from_secs(1),
                },
            },
            Authenticator::new("correct opaque daemon token").unwrap(),
            test_filesystem().clone(),
            Arc::new(runtime),
            journal,
        )
        .unwrap();

        // Run serve_until with immediate shutdown — reconcile must run at startup
        // (daemon.rs:~264), bind loops once, runtime.shutdown() no-ops.
        daemon.clone().serve_until(async { Ok(()) }).await.unwrap();

        // --- Inspect: Lost state surfaces through the wire ---
        let inspected = daemon
            .handle_line(
                &serde_json::to_vec(&RequestEnvelope {
                    version: SANDBOX_PROTOCOL_VERSION,
                    request_id: Uuid::new_v4(),
                    token: "correct opaque daemon token".into(),
                    operation: SandboxOperation::Inspect { terminal_id },
                })
                .unwrap(),
            )
            .await;
        assert!(
            matches!(
                inspected.result,
                Ok(SandboxResult::Inspected {
                    state: TerminalState::Lost,
                    output_complete: false,
                    ..
                })
            ),
            "expected Inspected {{ state: Lost, output_complete: false }}, got {inspected:?}"
        );

        // --- Reconnect: Lost state surfaces ---
        let reconnected = daemon
            .handle_line(
                &serde_json::to_vec(&RequestEnvelope {
                    version: SANDBOX_PROTOCOL_VERSION,
                    request_id: Uuid::new_v4(),
                    token: "correct opaque daemon token".into(),
                    operation: SandboxOperation::Reconnect { terminal_id },
                })
                .unwrap(),
            )
            .await;
        assert!(
            matches!(
                reconnected.result,
                Ok(SandboxResult::Reconnected {
                    state: TerminalState::Lost,
                    ..
                })
            ),
            "expected Reconnected {{ state: Lost }}, got {reconnected:?}"
        );

        // --- ReadOutput: durable output survives reconcile ---
        let output = daemon
            .handle_line(
                &serde_json::to_vec(&RequestEnvelope {
                    version: SANDBOX_PROTOCOL_VERSION,
                    request_id: Uuid::new_v4(),
                    token: "correct opaque daemon token".into(),
                    operation: SandboxOperation::ReadOutput {
                        terminal_id,
                        after_cursor: 0,
                        max_bytes: 16,
                        wait_ms: 0,
                    },
                })
                .unwrap(),
            )
            .await;
        let expected_base64 = BASE64.encode(b"replay-me");
        assert!(
            matches!(
                &output.result,
                Ok(SandboxResult::Output {
                    state: TerminalState::Lost,
                    output_complete: false,
                    data_base64,
                    ..
                }) if data_base64 == &expected_base64
            ),
            "expected Output {{ state: Lost, data_base64: {expected_base64} }}, got {output:?}"
        );

        // --- Survivor marker removed by terminate during reconcile ---
        assert!(
            !fake.marker.exists(),
            "survivor marker should be removed by reconcile"
        );

        // --- Pending input marked UNKNOWN by reconcile_restart ---
        assert!(
            matches!(
                test_terminal_journal().begin_input(terminal_id, pending_input_id, [9; 32]),
                Err(JournalError::OutcomeUnknown)
            ),
            "pending input should be UNKNOWN after reconcile_restart"
        );

        let _ = fs::remove_dir_all(&socket_dir);
    }

    #[tokio::test]
    async fn serve_until_attests_restricted_network_on_restart_then_serves() {
        use crate::{
            Authenticator, ConnectionConfig, Daemon, DaemonConfig, NetworkPolicyConfig,
            SocketConfig,
        };
        use gobrowse_core::sandbox::{
            RequestEnvelope, SANDBOX_PROTOCOL_VERSION, SandboxOperation, SandboxResult,
        };

        let _recovery_lock = recovery_test_lock().await;
        let fake = FakePodman::new();
        // Subnet 93.184.216.0/24, gateway 93.184.216.1, DNS 1.1.1.1 — all public.
        fake.write_network_info(
            "gobrowse-restricted-egress",
            "93.184.216.0/24",
            "93.184.216.1",
            r#"["1.1.1.1"]"#,
        );
        let runtime =
            fake.runtime_restricted(Duration::from_millis(500), "gobrowse-restricted-egress");
        // Clone before moving into Daemon so we can call start_spec afterward.
        let runtime_for_check = runtime.clone();

        let uid = effective_uid_from_proc_status(&fs::read_to_string("/proc/self/status").unwrap())
            .unwrap();
        let socket_dir =
            std::env::temp_dir().join(format!("gobrowse-sandboxd-attest-{}", Uuid::new_v4()));
        fs::create_dir(&socket_dir).unwrap();
        fs::set_permissions(&socket_dir, fs::Permissions::from_mode(0o700)).unwrap();

        let daemon = Daemon::new(
            DaemonConfig {
                socket: SocketConfig {
                    path: socket_dir.join("sandboxd.sock"),
                    mode: 0o600,
                    owner_uid: uid,
                    allowed_peer_uid: uid,
                },
                resource_ceiling: HARD_RESOURCE_LIMITS,
                network: NetworkPolicyConfig {
                    restricted_network: Some("gobrowse-restricted-egress".into()),
                    allow_full: false,
                },
                replay_capacity: 32,
                connections: ConnectionConfig {
                    max_connections: 4,
                    pre_auth_timeout: Duration::from_secs(1),
                    idle_timeout: Duration::from_secs(1),
                    operation_timeout: Duration::from_secs(1),
                },
            },
            Authenticator::new("correct opaque daemon token").unwrap(),
            test_filesystem().clone(),
            Arc::new(runtime),
            test_terminal_journal(),
        )
        .unwrap();

        // serve_until calls reconcile_recoveries which attests the restricted network.
        daemon.clone().serve_until(async { Ok(()) }).await.unwrap();

        // After attestation, start_spec with NetworkPolicy::Restricted must succeed —
        // no longer InvalidConfiguration.
        let spec = runtime_for_check
            .start_spec(&start(vec!["true".into()], "gobrowse-restricted-egress"))
            .unwrap();
        let index = spec.args.iter().position(|arg| arg == "--network").unwrap();
        assert_eq!(spec.args[index + 1], "gobrowse-restricted-egress");

        // The daemon itself is still functional after serve_until returns.
        let health = daemon
            .handle_line(
                &serde_json::to_vec(&RequestEnvelope {
                    version: SANDBOX_PROTOCOL_VERSION,
                    request_id: Uuid::new_v4(),
                    token: "correct opaque daemon token".into(),
                    operation: SandboxOperation::Health,
                })
                .unwrap(),
            )
            .await;
        assert!(
            matches!(health.result, Ok(SandboxResult::Health { .. })),
            "expected Health, got {health:?}"
        );

        let _ = fs::remove_dir_all(&socket_dir);
    }
}
