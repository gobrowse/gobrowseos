use std::{
    collections::{HashMap, VecDeque},
    path::PathBuf,
    process::Stdio,
    sync::{
        Arc,
        atomic::{AtomicBool, Ordering},
    },
    time::{Duration, Instant},
};

use async_trait::async_trait;
use gobrowse_core::sandbox::{TerminalStartRequest, TerminalState};
use thiserror::Error;
use tokio::{
    io::AsyncWriteExt,
    process::{ChildStdin, Command},
    sync::{Mutex, watch},
};
use uuid::Uuid;

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
    UnmanagedBindMount,
    /// The deployment provisioner guarantees a hard per-workspace quota before sandboxd starts.
    /// Podman bind mounts have no portable quota flag, so the adapter never emulates this mode.
    QuotaManaged {
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
}

#[derive(Debug, Clone)]
pub struct ValidatedStart {
    pub terminal_id: Uuid,
    pub request: TerminalStartRequest,
    pub workspace_path: PathBuf,
    pub network_name: String,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct RuntimeInspect {
    pub terminal_id: Uuid,
    pub workspace_id: Uuid,
    pub state: TerminalState,
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
    #[error("runtime I/O failed")]
    Io(#[source] std::io::Error),
}

#[async_trait]
pub trait SandboxRuntime: Send + Sync {
    async fn start(&self, start: ValidatedStart) -> Result<(), RuntimeError>;
    async fn input(&self, terminal_id: Uuid, bytes: &[u8]) -> Result<(), RuntimeError>;
    async fn resize(&self, terminal_id: Uuid, cols: u16, rows: u16) -> Result<(), RuntimeError>;
    async fn terminate(&self, terminal_id: Uuid) -> Result<(), RuntimeError>;
    async fn inspect(&self, terminal_id: Uuid) -> Result<RuntimeInspect, RuntimeError>;
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
    stdin: Mutex<Option<ChildStdin>>,
    termination_requested: AtomicBool,
    termination_owner: AtomicBool,
    background_cleanup_started: AtomicBool,
}

impl Session {
    fn new(terminal_id: Uuid, workspace_id: Uuid) -> Self {
        let (state, _) = watch::channel(LifecycleState::Starting);
        Self {
            terminal_id,
            workspace_id,
            state,
            stdin: Mutex::new(None),
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

pub struct PodmanRuntime {
    config: PodmanConfig,
    registry: Arc<Mutex<Registry>>,
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
        {
            return Err(RuntimeError::InvalidConfiguration);
        }
        if let WorkspaceProvisioning::QuotaManaged { maximum_bytes } = config.workspace_provisioning
            && maximum_bytes == 0
        {
            return Err(RuntimeError::InvalidConfiguration);
        }
        Ok(Self {
            config,
            registry: Arc::new(Mutex::new(Registry::new())),
        })
    }

    pub fn start_spec(&self, start: &ValidatedStart) -> Result<ProcessSpec, RuntimeError> {
        let WorkspaceProvisioning::QuotaManaged { maximum_bytes } =
            self.config.workspace_provisioning
        else {
            return Err(RuntimeError::WorkspaceQuotaUnavailable);
        };
        if start.request.limits.writable_storage_bytes > maximum_bytes {
            return Err(RuntimeError::WorkspaceQuotaUnavailable);
        }
        let workspace = start
            .workspace_path
            .to_str()
            .ok_or(RuntimeError::InvalidConfiguration)?;
        if workspace.contains([':', ',', '\n', '\r']) || !start.workspace_path.is_absolute() {
            return Err(RuntimeError::InvalidConfiguration);
        }
        if !valid_runtime_network(start.request.network_policy, &start.network_name) {
            return Err(RuntimeError::InvalidConfiguration);
        }

        let limits = start.request.limits;
        let mut args = vec![
            "run".into(),
            "--rm".into(),
            "--interactive".into(),
            "--tty".into(),
            "--name".into(),
            container_name(start.terminal_id),
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

    async fn inspect_ready(&self, terminal_id: Uuid) -> Result<bool, RuntimeError> {
        let output = tokio::time::timeout(
            self.config.control_timeout,
            self.readiness_spec(terminal_id)
                .command()
                .stdin(Stdio::null())
                .stderr(Stdio::null())
                .output(),
        )
        .await
        .map_err(|_| RuntimeError::ControlTimeout)?
        .map_err(RuntimeError::Io)?;
        Ok(output.status.success() && output.stdout.as_slice() == b"true\n")
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
    let mut state = session.state.subscribe();
    loop {
        if state.borrow_and_update().is_terminal() {
            return true;
        }
        if Instant::now() >= deadline {
            session.transition(LifecycleState::Unrecoverable);
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

#[async_trait]
impl SandboxRuntime for PodmanRuntime {
    async fn start(&self, start: ValidatedStart) -> Result<(), RuntimeError> {
        let spec = self.start_spec(&start)?;
        let session = Arc::new(Session::new(start.terminal_id, start.request.workspace_id));
        self.registry
            .lock()
            .await
            .reserve(Arc::clone(&session), self.config.session_limits)?;

        let spawned = spec
            .command()
            .stdin(Stdio::piped())
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .spawn();
        let mut child = match spawned {
            Ok(child) => child,
            Err(error) => {
                self.registry
                    .lock()
                    .await
                    .cancel_reservation(session.terminal_id);
                return Err(RuntimeError::Io(error));
            }
        };
        let Some(stdin) = child.stdin.take() else {
            self.registry
                .lock()
                .await
                .cancel_reservation(session.terminal_id);
            return Err(RuntimeError::PodmanFailed);
        };
        session
            .stdin
            .try_lock()
            .expect("new session stdin lock is uncontended")
            .replace(stdin);

        let monitor_session = Arc::clone(&session);
        let registry = Arc::clone(&self.registry);
        let limits = self.config.session_limits;
        tokio::spawn(async move {
            let _ = child.wait().await;
            monitor_session.stdin.lock().await.take();
            let final_state = if monitor_session
                .termination_requested
                .load(Ordering::Acquire)
            {
                LifecycleState::Terminated
            } else {
                LifecycleState::Exited
            };
            registry
                .lock()
                .await
                .complete(monitor_session.terminal_id, limits);
            monitor_session.transition(final_state);
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
            return Err(error);
        }
        cancellation_guard.disarm();
        self.spawn_timeout(
            Arc::clone(&session),
            Duration::from_secs(start.request.limits.execution_seconds),
        );
        Ok(())
    }

    async fn input(&self, terminal_id: Uuid, bytes: &[u8]) -> Result<(), RuntimeError> {
        let session = self.session(terminal_id).await?;
        if session.lifecycle() != LifecycleState::Running {
            return Err(RuntimeError::NotRunning);
        }
        let mut cancellation_guard = CleanupOnDrop {
            config: self.config.clone(),
            session: Arc::clone(&session),
            armed: true,
        };
        let mut stdin = session.stdin.lock().await;
        if session.lifecycle() != LifecycleState::Running {
            return Err(RuntimeError::NotRunning);
        }
        let write_result = tokio::time::timeout(self.config.input_write_timeout, async {
            let stdin = stdin.as_mut().ok_or(RuntimeError::NotRunning)?;
            stdin.write_all(bytes).await.map_err(RuntimeError::Io)?;
            stdin.flush().await.map_err(RuntimeError::Io)
        })
        .await;
        match write_result {
            Ok(Ok(())) => {
                cancellation_guard.disarm();
                Ok(())
            }
            Ok(Err(error)) => Err(error),
            Err(_) => {
                stdin.take();
                drop(stdin);
                session.begin_termination();
                spawn_persistent_cleanup(self.config.clone(), session);
                cancellation_guard.disarm();
                Err(RuntimeError::InputTimeout)
            }
        }
    }

    async fn resize(&self, terminal_id: Uuid, cols: u16, rows: u16) -> Result<(), RuntimeError> {
        let session = self.session(terminal_id).await?;
        if session.lifecycle() != LifecycleState::Running {
            return Err(RuntimeError::NotRunning);
        }
        self.run_checked(self.resize_spec(terminal_id, cols, rows))
            .await
    }

    async fn terminate(&self, terminal_id: Uuid) -> Result<(), RuntimeError> {
        let session = self.session(terminal_id).await?;
        session.begin_termination();
        spawn_persistent_cleanup(self.config.clone(), Arc::clone(&session));
        let deadline = Instant::now() + self.config.termination_timeout;
        if cleanup_until(&self.config, &session, deadline).await {
            Ok(())
        } else {
            spawn_persistent_cleanup(self.config.clone(), session);
            Err(RuntimeError::ControlTimeout)
        }
    }

    async fn inspect(&self, terminal_id: Uuid) -> Result<RuntimeInspect, RuntimeError> {
        let session = self.session(terminal_id).await?;
        Ok(RuntimeInspect {
            terminal_id,
            workspace_id: session.workspace_id,
            state: session.lifecycle().terminal_state(),
        })
    }
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

fn cpu_limit(cpu_millis: u32) -> String {
    format!("{}.{:03}", cpu_millis / 1_000, cpu_millis % 1_000)
}

fn container_name(terminal_id: Uuid) -> String {
    format!("gobrowse-{terminal_id}")
}

#[cfg(test)]
mod tests {
    use std::{fs, io::Write, os::unix::fs::PermissionsExt, path::Path, sync::Barrier};

    use gobrowse_core::sandbox::{HARD_RESOURCE_LIMITS, NetworkPolicy, ResourceLimits};

    use super::*;

    fn config(provisioning: WorkspaceProvisioning) -> PodmanConfig {
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
        }
    }

    fn runtime() -> PodmanRuntime {
        PodmanRuntime::from_validated_config(config(WorkspaceProvisioning::QuotaManaged {
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
                workspace_id: Uuid::nil(),
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
            workspace_path: PathBuf::from("/srv/gobrowse/workspaces/workspace"),
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
        assert!(!spec.args.iter().any(|argument| argument == "--privileged"));
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
    fn unmanaged_or_insufficient_workspace_quota_fails_closed() {
        let unmanaged =
            PodmanRuntime::from_validated_config(config(WorkspaceProvisioning::UnmanagedBindMount))
                .unwrap();
        assert!(matches!(
            unmanaged.start_spec(&start(vec!["true".into()], "none")),
            Err(RuntimeError::WorkspaceQuotaUnavailable)
        ));
        let limited =
            PodmanRuntime::from_validated_config(config(WorkspaceProvisioning::QuotaManaged {
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
        let make_session = |terminal_id, workspace_id| {
            let mut child = Command::new("/bin/true")
                .stdin(Stdio::piped())
                .spawn()
                .unwrap();
            let session = Arc::new(Session::new(terminal_id, workspace_id));
            session
                .stdin
                .try_lock()
                .unwrap()
                .replace(child.stdin.take().unwrap());
            session
        };
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
                ..config(WorkspaceProvisioning::QuotaManaged {
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
        assert!(
            runtime()
                .start_spec(&ValidatedStart {
                    workspace_path: PathBuf::from("/tmp/unsafe:volume"),
                    ..start(vec!["true".into()], "none")
                })
                .is_err()
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
        control_failures: PathBuf,
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
            let script = format!(
                r#"#!/bin/sh
marker="{}"
log="{}"
control_failures="{}"
resize_failure="{}"
slow_readiness="{}"
printf '%s\n' "$*" >> "$log"
case "$1" in
  run)
    mode=hold
    for argument in "$@"; do
      [ "$argument" = natural ] && mode=natural
      [ "$argument" = never-ready ] && mode=never-ready
      [ "$argument" = control-recover ] && printf '2\n' > "$control_failures"
      [ "$argument" = control-persistent ] && printf 'persistent\n' > "$control_failures"
      [ "$argument" = resize-fail ] && : > "$resize_failure"
      [ "$argument" = slow-ready ] && : > "$slow_readiness"
    done
    if [ "$mode" = never-ready ]; then sleep 0.30; exit 0; fi
    : > "$marker"
    if [ "$mode" = natural ]; then sleep 0.20; rm -f "$marker"; exit 0; fi
    while [ -e "$marker" ]; do sleep 0.05; done
    ;;
  inspect)
    if [ -e "$slow_readiness" ]; then sleep 0.20; rm -f "$slow_readiness"; fi
    [ -e "$marker" ] && printf 'true\n' || exit 1
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
    rm -f "$marker"
    ;;
esac
"#,
                marker.display(),
                log.display(),
                control_failures.display(),
                resize_failure.display(),
                slow_readiness.display(),
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
                control_failures,
            }
        }

        fn runtime(&self, readiness_timeout: Duration) -> PodmanRuntime {
            PodmanRuntime::from_validated_config(PodmanConfig {
                executable: self.executable.clone(),
                readiness_timeout,
                control_timeout: Duration::from_millis(100),
                termination_timeout: Duration::from_millis(300),
                input_write_timeout: Duration::from_millis(50),
                ..config(WorkspaceProvisioning::QuotaManaged {
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
    }

    impl Drop for FakePodman {
        fn drop(&mut self) {
            let _ = fs::remove_dir_all(&self.root);
        }
    }

    #[tokio::test]
    async fn start_waits_for_readiness_and_terminal_transitions_are_coherent() {
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
    async fn start_fails_when_positive_readiness_is_not_observed() {
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
        let fake = FakePodman::new();
        let runtime = fake.runtime(Duration::from_millis(500));
        let terminal_id = Uuid::new_v4();
        assert!(
            tokio::time::timeout(
                Duration::from_millis(10),
                runtime.start(ValidatedStart {
                    terminal_id,
                    ..start(vec!["slow-ready".into()], "none")
                })
            )
            .await
            .is_err()
        );
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
        let fake = FakePodman::new();
        let runtime = fake.runtime(Duration::from_millis(500));
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
    async fn terminal_input_backpressure_times_out_and_cleans_up() {
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
        assert!(matches!(
            runtime.input(terminal_id, &vec![0; 8 * 1024 * 1024]).await,
            Err(RuntimeError::InputTimeout)
        ));
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
}
