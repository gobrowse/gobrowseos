#![forbid(unsafe_code)]

use std::{path::PathBuf, sync::Arc, time::Duration};

use clap::Parser;
use gobrowse_core::sandbox::ResourceLimits;
use gobrowse_sandboxd::{
    Authenticator, ConnectionConfig, Daemon, DaemonConfig, Filesystem, NetworkPolicyConfig,
    PodmanConfig, PodmanRuntime, SessionLimits, SocketConfig, TerminalJournal,
    WorkspaceProvisioning, effective_uid_from_proc_status,
};
use zeroize::Zeroizing;

#[derive(Debug, Parser)]
#[command(name = "gobrowse-sandboxd")]
struct Args {
    #[arg(long)]
    socket: PathBuf,
    #[arg(long)]
    workspace_root: PathBuf,
    #[arg(long)]
    terminal_journal: Option<PathBuf>,
    #[arg(long)]
    auth_token_file: PathBuf,
    #[arg(long, value_parser = parse_socket_mode)]
    socket_mode: u32,
    #[arg(long)]
    allowed_peer_uid: u32,
    #[arg(long)]
    deployment_id: uuid::Uuid,
    #[arg(long, default_value = "/usr/bin/podman")]
    podman: PathBuf,
    #[arg(long)]
    image: String,
    #[arg(long)]
    restricted_network: Option<String>,
    #[arg(long, default_value_t = false)]
    allow_full_network: bool,
    #[arg(long, default_value_t = 2_000)]
    max_cpu_millis: u32,
    #[arg(long, default_value_t = 2_147_483_648)]
    max_memory_bytes: u64,
    #[arg(long, default_value_t = 2_147_483_648)]
    max_writable_storage_bytes: u64,
    #[arg(long, default_value_t = 256)]
    max_pids: u32,
    #[arg(long, default_value_t = 3_600)]
    max_execution_seconds: u64,
    #[arg(long, default_value_t = 32)]
    max_active_sessions: usize,
    #[arg(long, default_value_t = 4)]
    max_active_sessions_per_workspace: usize,
    #[arg(long, default_value_t = 256)]
    max_retained_terminal_records: usize,
    #[arg(long, default_value_t = 4_096)]
    replay_cache_entries: usize,
    #[arg(long, default_value_t = 64)]
    max_connections: usize,
    #[arg(long, default_value_t = 5)]
    pre_auth_timeout_seconds: u64,
    #[arg(long, default_value_t = 60)]
    connection_idle_seconds: u64,
    #[arg(long, default_value_t = 30)]
    operation_timeout_seconds: u64,
    #[arg(long, default_value_t = 5)]
    podman_readiness_seconds: u64,
    #[arg(long, default_value_t = 5)]
    podman_control_seconds: u64,
    #[arg(long, default_value_t = 10)]
    podman_termination_seconds: u64,
    #[arg(long, default_value_t = 2)]
    terminal_input_timeout_seconds: u64,
    #[arg(long, default_value_t = false)]
    quota_managed_workspaces: bool,
}

#[tokio::main]
async fn main() -> Result<(), Box<dyn std::error::Error>> {
    let args = Args::parse();
    if args.terminal_input_timeout_seconds >= args.operation_timeout_seconds {
        return Err("terminal input timeout must be shorter than operation timeout".into());
    }
    let status = std::fs::read_to_string("/proc/self/status")?;
    let effective_uid = effective_uid_from_proc_status(&status)?;
    if effective_uid == 0 {
        return Err("gobrowse-sandboxd refuses to run as root".into());
    }

    let token = Zeroizing::new(std::fs::read_to_string(&args.auth_token_file)?);
    let auth = Authenticator::new(token.trim())?;
    let filesystem = Filesystem::new(&args.workspace_root)?;
    let terminal_journal = TerminalJournal::open(
        args.terminal_journal
            .unwrap_or_else(|| args.workspace_root.join(".sandboxd-terminals.sqlite3")),
    )?;
    let workspace_resolver = filesystem.resolver();
    let recovery_store = filesystem.recovery_store();
    let runtime = Arc::new(PodmanRuntime::new(PodmanConfig {
        executable: args.podman,
        image: args.image,
        readiness_timeout: Duration::from_secs(args.podman_readiness_seconds),
        control_timeout: Duration::from_secs(args.podman_control_seconds),
        termination_timeout: Duration::from_secs(args.podman_termination_seconds),
        input_write_timeout: Duration::from_secs(args.terminal_input_timeout_seconds),
        session_limits: SessionLimits {
            max_active: args.max_active_sessions,
            max_active_per_workspace: args.max_active_sessions_per_workspace,
            max_retained_records: args.max_retained_terminal_records,
        },
        workspace_provisioning: if args.quota_managed_workspaces {
            WorkspaceProvisioning::NamedVolume {
                maximum_bytes: args.max_writable_storage_bytes,
            }
        } else {
            WorkspaceProvisioning::Unverified
        },
        deployment_id: args.deployment_id,
        workspace_resolver,
        recovery_store,
        terminal_journal: terminal_journal.clone(),
    })?);
    let daemon = Daemon::new(
        DaemonConfig {
            socket: SocketConfig {
                path: args.socket,
                mode: args.socket_mode,
                owner_uid: effective_uid,
                allowed_peer_uid: args.allowed_peer_uid,
            },
            resource_ceiling: ResourceLimits {
                cpu_millis: args.max_cpu_millis,
                memory_bytes: args.max_memory_bytes,
                writable_storage_bytes: args.max_writable_storage_bytes,
                pids: args.max_pids,
                execution_seconds: args.max_execution_seconds,
            },
            network: NetworkPolicyConfig {
                restricted_network: args.restricted_network,
                allow_full: args.allow_full_network,
            },
            replay_capacity: args.replay_cache_entries,
            connections: ConnectionConfig {
                max_connections: args.max_connections,
                pre_auth_timeout: Duration::from_secs(args.pre_auth_timeout_seconds),
                idle_timeout: Duration::from_secs(args.connection_idle_seconds),
                operation_timeout: Duration::from_secs(args.operation_timeout_seconds),
            },
        },
        auth,
        filesystem,
        runtime,
        terminal_journal,
    )?;
    daemon.serve().await?;
    Ok(())
}

fn parse_socket_mode(value: &str) -> Result<u32, String> {
    u32::from_str_radix(value, 8).map_err(|_| "socket mode must be octal (for example 660)".into())
}
