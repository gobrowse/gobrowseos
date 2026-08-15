use std::{
    net::{IpAddr, Ipv4Addr, Ipv6Addr},
    path::{Component, Path, PathBuf},
};

use serde::{Deserialize, Serialize};
use thiserror::Error;
use uuid::Uuid;

pub const SANDBOX_PROTOCOL_VERSION: u16 = 2;
pub const MAX_PROTOCOL_LINE_BYTES: usize = 512 * 1024;
pub const MAX_COMMAND_ARGUMENTS: usize = 128;
pub const MAX_COMMAND_ARGUMENT_BYTES: usize = 4 * 1024;
pub const MAX_COMMAND_BYTES: usize = 32 * 1024;
pub const MAX_WORKSPACE_PATH_BYTES: usize = 4 * 1024;
pub const MAX_WORKSPACE_PATH_COMPONENTS: usize = 128;
pub const MAX_TERMINAL_INPUT_BYTES: usize = 64 * 1024;
pub const MAX_TERMINAL_OUTPUT_CHUNK_BYTES: usize = 32 * 1024;
pub const MAX_TERMINAL_OUTPUT_READ_BYTES: usize = 256 * 1024;
pub const MAX_TERMINAL_OUTPUT_BYTES: u64 = 64 * 1024 * 1024;
pub const MAX_TERMINAL_OUTPUT_WAIT_MS: u32 = 30_000;
pub const MAX_TERMINAL_PROCESSES: usize = 1_024;
pub const MAX_TERMINAL_PROCESS_COMMAND_BYTES: usize = 4 * 1024;
pub const MAX_TERMINAL_PROCESS_BYTES: usize = MAX_PROTOCOL_LINE_BYTES / 2;
pub const MAX_FILE_PAYLOAD_BYTES: usize = 256 * 1024;
pub const MAX_DIRECTORY_ENTRIES: usize = 1_024;
pub const MAX_FILESYSTEM_SEARCH_QUERY_BYTES: usize = 256;
pub const MAX_FILESYSTEM_SEARCH_RESULTS: usize = 1_024;
pub const MAX_FILESYSTEM_SCANNED_ENTRIES: usize = 16_384;
/// A conservative cap leaves room for the response envelope, framing, and future fixed metadata.
pub const MAX_FILESYSTEM_SEARCH_ENCODED_BYTES: usize = MAX_PROTOCOL_LINE_BYTES / 2;

pub const HARD_RESOURCE_LIMITS: ResourceLimits = ResourceLimits {
    cpu_millis: 8_000,
    memory_bytes: 16 * 1024 * 1024 * 1024,
    writable_storage_bytes: 16 * 1024 * 1024 * 1024,
    pids: 1_024,
    execution_seconds: 24 * 60 * 60,
};

/// Returns the only runtime storage identity accepted for a workspace.
#[must_use]
pub fn workspace_volume_name(workspace_id: Uuid) -> String {
    format!("gobrowse-workspace-{workspace_id}")
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct WorkspaceStorageIdentity {
    pub volume_name: String,
    pub device: u64,
    pub inode: u64,
}

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "SCREAMING_SNAKE_CASE")]
pub enum NetworkPolicy {
    None,
    #[default]
    Restricted,
    Full,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ResourceLimits {
    pub cpu_millis: u32,
    pub memory_bytes: u64,
    pub writable_storage_bytes: u64,
    pub pids: u32,
    pub execution_seconds: u64,
}

impl ResourceLimits {
    pub fn validate_hard_ceiling(self) -> Result<Self, SandboxValidationError> {
        if self.cpu_millis == 0
            || self.memory_bytes == 0
            || self.writable_storage_bytes == 0
            || self.pids == 0
            || self.execution_seconds == 0
        {
            return Err(SandboxValidationError::InvalidLimits);
        }
        if self.cpu_millis > HARD_RESOURCE_LIMITS.cpu_millis
            || self.memory_bytes > HARD_RESOURCE_LIMITS.memory_bytes
            || self.writable_storage_bytes > HARD_RESOURCE_LIMITS.writable_storage_bytes
            || self.pids > HARD_RESOURCE_LIMITS.pids
            || self.execution_seconds > HARD_RESOURCE_LIMITS.execution_seconds
        {
            return Err(SandboxValidationError::LimitsExceedHardCeiling);
        }
        Ok(self)
    }

    #[must_use]
    pub fn narrow_to(self, ceiling: Self) -> Self {
        Self {
            cpu_millis: self.cpu_millis.min(ceiling.cpu_millis),
            memory_bytes: self.memory_bytes.min(ceiling.memory_bytes),
            writable_storage_bytes: self
                .writable_storage_bytes
                .min(ceiling.writable_storage_bytes),
            pids: self.pids.min(ceiling.pids),
            execution_seconds: self.execution_seconds.min(ceiling.execution_seconds),
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct TerminalStartRequest {
    pub workspace_id: Uuid,
    pub command: Vec<String>,
    pub working_directory: String,
    pub cols: u16,
    pub rows: u16,
    pub network_policy: NetworkPolicy,
    pub limits: ResourceLimits,
}

#[derive(Debug, Error, PartialEq, Eq)]
pub enum SandboxValidationError {
    #[error("command must not be empty")]
    EmptyCommand,
    #[error("command exceeds the supported bounds")]
    CommandTooLarge,
    #[error("workspace path must be relative and cannot traverse parents")]
    UnsafePath,
    #[error("workspace path exceeds the supported bounds")]
    PathTooLarge,
    #[error("terminal dimensions are outside the supported range")]
    InvalidTerminalSize,
    #[error("resource limits must be positive")]
    InvalidLimits,
    #[error("resource limits exceed the hard safety ceiling")]
    LimitsExceedHardCeiling,
    #[error("restricted network attestation failed")]
    InvalidNetwork,
}

impl TerminalStartRequest {
    pub fn validate(&self) -> Result<PathBuf, SandboxValidationError> {
        validate_command(&self.command)?;
        let path = validate_workspace_path(&self.working_directory)?;
        if !(20..=1_000).contains(&self.cols) || !(5..=500).contains(&self.rows) {
            return Err(SandboxValidationError::InvalidTerminalSize);
        }
        self.limits.validate_hard_ceiling()?;
        Ok(path)
    }
}

pub fn validate_command(command: &[String]) -> Result<(), SandboxValidationError> {
    if command.is_empty() || command[0].is_empty() {
        return Err(SandboxValidationError::EmptyCommand);
    }
    if command.len() > MAX_COMMAND_ARGUMENTS
        || command.iter().any(|arg| {
            arg.is_empty() || arg.as_bytes().contains(&0) || arg.len() > MAX_COMMAND_ARGUMENT_BYTES
        })
        || command.iter().map(String::len).sum::<usize>() > MAX_COMMAND_BYTES
    {
        return Err(SandboxValidationError::CommandTooLarge);
    }
    Ok(())
}

pub fn validate_workspace_path(value: &str) -> Result<PathBuf, SandboxValidationError> {
    if value.is_empty() || value.as_bytes().contains(&0) {
        return Err(SandboxValidationError::UnsafePath);
    }
    if value.len() > MAX_WORKSPACE_PATH_BYTES {
        return Err(SandboxValidationError::PathTooLarge);
    }
    let path = Path::new(value);
    let components = path.components().collect::<Vec<_>>();
    if components.len() > MAX_WORKSPACE_PATH_COMPONENTS {
        return Err(SandboxValidationError::PathTooLarge);
    }
    if path.is_absolute()
        || components.iter().any(|component| {
            matches!(
                component,
                Component::ParentDir | Component::RootDir | Component::Prefix(_)
            )
        })
    {
        return Err(SandboxValidationError::UnsafePath);
    }
    Ok(path.to_path_buf())
}

#[derive(Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct RequestEnvelope {
    pub version: u16,
    /// Terminal mutation responses are durable. Other mutation responses have only sandboxd's
    /// bounded process-local replay horizon and must not be treated as durable idempotency records.
    pub request_id: Uuid,
    pub token: String,
    pub operation: SandboxOperation,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "op", rename_all = "snake_case", deny_unknown_fields)]
pub enum SandboxOperation {
    Health,
    Start {
        terminal_id: Uuid,
        request: TerminalStartRequest,
    },
    Input {
        terminal_id: Uuid,
        input_id: Uuid,
        data_base64: String,
    },
    ReadOutput {
        terminal_id: Uuid,
        after_cursor: u64,
        max_bytes: u32,
        wait_ms: u32,
    },
    AckOutput {
        terminal_id: Uuid,
        cursor: u64,
    },
    Resize {
        terminal_id: Uuid,
        cols: u16,
        rows: u16,
    },
    Interrupt {
        terminal_id: Uuid,
    },
    Processes {
        terminal_id: Uuid,
    },
    Kill {
        terminal_id: Uuid,
        pid: u32,
    },
    Terminate {
        terminal_id: Uuid,
    },
    Inspect {
        terminal_id: Uuid,
    },
    Reconnect {
        terminal_id: Uuid,
    },
    FsList {
        workspace_id: Uuid,
        path: String,
    },
    FsRead {
        workspace_id: Uuid,
        path: String,
    },
    FsMetadata {
        workspace_id: Uuid,
        path: String,
    },
    FsSearch {
        workspace_id: Uuid,
        path: String,
        query: String,
    },
    FsWrite {
        workspace_id: Uuid,
        path: String,
        data_base64: String,
    },
    FsPatch {
        workspace_id: Uuid,
        path: String,
        expected_sha256: String,
        data_base64: String,
    },
    FsMkdir {
        workspace_id: Uuid,
        path: String,
    },
    FsMove {
        workspace_id: Uuid,
        from: String,
        to: String,
    },
    FsCopy {
        workspace_id: Uuid,
        from: String,
        to: String,
    },
    FsDelete {
        workspace_id: Uuid,
        path: String,
    },
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ResponseEnvelope {
    pub version: u16,
    pub request_id: Uuid,
    pub result: Result<SandboxResult, SandboxProtocolError>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "op", rename_all = "snake_case", deny_unknown_fields)]
pub enum SandboxResult {
    Health {
        status: String,
    },
    Started {
        terminal_id: Uuid,
        limits: ResourceLimits,
        network_policy: NetworkPolicy,
    },
    InputAccepted {
        input_id: Uuid,
        bytes: usize,
        replayed: bool,
    },
    Output {
        terminal_id: Uuid,
        start_cursor: u64,
        next_cursor: u64,
        data_base64: String,
        state: TerminalState,
        output_complete: bool,
    },
    OutputAcked {
        cursor: u64,
    },
    Resized,
    Interrupted,
    Processes {
        processes: Vec<SandboxProcess>,
    },
    Killed {
        pid: u32,
    },
    Terminated,
    Inspected {
        terminal_id: Uuid,
        workspace_id: Uuid,
        state: TerminalState,
        cols: u16,
        rows: u16,
        exit_code: Option<i32>,
        reason: Option<String>,
        output_start_cursor: u64,
        output_end_cursor: u64,
        acked_cursor: u64,
        output_complete: bool,
    },
    Reconnected {
        terminal_id: Uuid,
        workspace_id: Uuid,
        state: TerminalState,
        cols: u16,
        rows: u16,
        exit_code: Option<i32>,
        reason: Option<String>,
        output_start_cursor: u64,
        output_end_cursor: u64,
        acked_cursor: u64,
        output_complete: bool,
    },
    FsList {
        entries: Vec<FilesystemEntry>,
    },
    FsRead {
        data_base64: String,
        sha256: String,
    },
    FsMetadata {
        metadata: FilesystemMetadata,
    },
    FsSearch {
        matches: Vec<FilesystemSearchMatch>,
    },
    FsWritten {
        bytes: usize,
        sha256: String,
    },
    FsPatched {
        bytes: usize,
        sha256: String,
    },
    FsCreated,
    FsMoved,
    FsCopied,
    FsDeleted,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "SCREAMING_SNAKE_CASE")]
pub enum TerminalState {
    Running,
    Terminating,
    Unrecoverable,
    Lost,
    Exited,
    Terminated,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct SandboxProcess {
    pub pid: u32,
    pub command: String,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct FilesystemEntry {
    pub name: String,
    pub kind: FilesystemEntryKind,
    pub size: u64,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "SCREAMING_SNAKE_CASE")]
pub enum FilesystemEntryKind {
    File,
    Directory,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct FilesystemMetadata {
    pub kind: FilesystemEntryKind,
    pub size: u64,
    pub mode: u32,
    pub modified_unix_seconds: i64,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct FilesystemSearchMatch {
    pub path: String,
    pub kind: FilesystemEntryKind,
    pub size: u64,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct SandboxProtocolError {
    pub code: SandboxErrorCode,
    pub message: String,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "SCREAMING_SNAKE_CASE")]
pub enum SandboxErrorCode {
    InvalidRequest,
    UnsupportedVersion,
    Unauthorized,
    NotFound,
    PolicyDenied,
    LimitExceeded,
    DeadlineExceeded,
    Conflict,
    OutcomeUnknown,
    Internal,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct RestrictedNetworkAttestation {
    pub network_name: String,
    pub subnet: IpAddr,
    pub gateway: IpAddr,
    pub dns: Vec<IpAddr>,
}

impl RestrictedNetworkAttestation {
    pub fn validate(&self) -> Result<(), SandboxValidationError> {
        if !valid_restricted_network_name(&self.network_name) {
            return Err(SandboxValidationError::InvalidNetwork);
        }
        if !is_public_destination(self.gateway) {
            return Err(SandboxValidationError::InvalidNetwork);
        }
        for &addr in &self.dns {
            if !is_public_destination(addr) {
                return Err(SandboxValidationError::InvalidNetwork);
            }
        }
        Ok(())
    }
}

/// Returns true only for addresses suitable for restricted public egress.
pub fn is_public_destination(ip: IpAddr) -> bool {
    match ip {
        IpAddr::V4(ip) => is_public_v4(ip),
        IpAddr::V6(ip) => is_public_v6(ip),
    }
}

fn is_public_v4(ip: Ipv4Addr) -> bool {
    let [a, b, c, _] = ip.octets();
    !(a == 0
        || a == 10
        || a == 127
        || (a == 100 && (64..=127).contains(&b))
        || (a == 169 && b == 254)
        || (a == 172 && (16..=31).contains(&b))
        || (a == 192 && b == 0 && c == 0)
        || (a == 192 && b == 0 && c == 2)
        || (a == 192 && b == 168)
        || (a == 198 && (b == 18 || b == 19))
        || (a == 198 && b == 51 && c == 100)
        || (a == 203 && b == 0 && c == 113)
        || a >= 224)
}

fn is_public_v6(ip: Ipv6Addr) -> bool {
    let segments = ip.segments();
    !(ip.to_ipv4_mapped().is_some()
        || ip.is_unspecified()
        || ip.is_loopback()
        || ip.is_multicast()
        || (segments[0] & 0xfe00) == 0xfc00
        || (segments[0] & 0xffc0) == 0xfe80
        || (segments[0] & 0xffc0) == 0xfec0
        || (segments[..6].iter().all(|segment| *segment == 0))
        || (segments[0] == 0x0064 && segments[1] == 0xff9b)
        || (segments[0] == 0x2001 && segments[1] == 0)
        || segments[0] == 0x2002
        || (segments[0] == 0x2001 && segments[1] == 0x0db8))
}

pub fn valid_restricted_network_name(name: &str) -> bool {
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
    name.starts_with("gobrowse-restricted-")
        && name.len() > "gobrowse-restricted-".len()
        && name.len() <= 128
        && !RESERVED.contains(&lower.as_str())
        && name
            .chars()
            .all(|character| character.is_ascii_alphanumeric() || matches!(character, '-' | '_'))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn workspace_paths_reject_traversal_absolute_and_oversized_paths() {
        assert!(validate_workspace_path("src/lib.rs").is_ok());
        assert!(validate_workspace_path(".").is_ok());
        assert!(validate_workspace_path("../host").is_err());
        assert!(validate_workspace_path("/etc/passwd").is_err());
        assert!(validate_workspace_path("nul\0path").is_err());
        assert_eq!(
            validate_workspace_path(&"x".repeat(MAX_WORKSPACE_PATH_BYTES + 1)),
            Err(SandboxValidationError::PathTooLarge)
        );
    }

    #[test]
    fn command_bounds_reject_empty_nul_and_oversized_arguments() {
        assert!(validate_command(&["sh".into(), "-c".into(), "true".into()]).is_ok());
        assert_eq!(
            validate_command(&[]),
            Err(SandboxValidationError::EmptyCommand)
        );
        assert_eq!(
            validate_command(&["a\0b".into()]),
            Err(SandboxValidationError::CommandTooLarge)
        );
        assert_eq!(
            validate_command(&["x".repeat(MAX_COMMAND_ARGUMENT_BYTES + 1)]),
            Err(SandboxValidationError::CommandTooLarge)
        );
        assert_eq!(
            validate_command(&vec!["x".into(); MAX_COMMAND_ARGUMENTS + 1]),
            Err(SandboxValidationError::CommandTooLarge)
        );
    }

    #[test]
    fn resource_limits_are_positive_and_have_hard_ceilings() {
        assert_eq!(
            ResourceLimits {
                cpu_millis: HARD_RESOURCE_LIMITS.cpu_millis + 1,
                ..HARD_RESOURCE_LIMITS
            }
            .validate_hard_ceiling(),
            Err(SandboxValidationError::LimitsExceedHardCeiling)
        );
        assert_eq!(
            ResourceLimits {
                pids: 0,
                ..HARD_RESOURCE_LIMITS
            }
            .validate_hard_ceiling(),
            Err(SandboxValidationError::InvalidLimits)
        );
        assert_eq!(
            ResourceLimits {
                writable_storage_bytes: 0,
                ..HARD_RESOURCE_LIMITS
            }
            .validate_hard_ceiling(),
            Err(SandboxValidationError::InvalidLimits)
        );
    }

    #[test]
    fn restricted_egress_blocks_metadata_private_and_mapped_networks() {
        for address in [
            "169.254.169.254",
            "127.0.0.1",
            "10.0.0.1",
            "192.168.1.1",
            "::1",
            "fd00::1",
            "::ffff:127.0.0.1",
            "::ffff:1.1.1.1",
            "::1.2.3.4",
            "64:ff9b::a9fe:a9fe",
            "64:ff9b::7f00:1",
            "64:ff9b:1::a9fe:a9fe",
            "2001::1",
            "2002:c000:0204::1",
        ] {
            assert!(
                !is_public_destination(address.parse().unwrap()),
                "{address}"
            );
        }
        assert!(is_public_destination("1.1.1.1".parse().unwrap()));
        assert!(is_public_destination(
            "2606:4700:4700::1111".parse().unwrap()
        ));
    }

    #[test]
    fn restricted_network_name_rejects_reserved_and_invalid_shapes() {
        assert!(valid_restricted_network_name("gobrowse-restricted-egress"));
        assert!(valid_restricted_network_name(
            "gobrowse-restricted-my_net-01"
        ));
        assert!(!valid_restricted_network_name("gobrowse-restricted-"));
        assert!(!valid_restricted_network_name("host"));
        assert!(!valid_restricted_network_name("none"));
        assert!(!valid_restricted_network_name("podman"));
        assert!(!valid_restricted_network_name("gobrowse-restricted-   "));
        assert!(!valid_restricted_network_name(&"x".repeat(129)));
    }

    #[test]
    fn restricted_network_attestation_rejects_metadata_and_private_ranges() {
        let attestation = RestrictedNetworkAttestation {
            network_name: "gobrowse-restricted-bad-gw".into(),
            subnet: "192.168.0.0".parse().unwrap(),
            gateway: "10.0.0.1".parse().unwrap(),
            dns: vec![],
        };
        assert_eq!(
            attestation.validate(),
            Err(SandboxValidationError::InvalidNetwork)
        );

        let attestation = RestrictedNetworkAttestation {
            network_name: "gobrowse-restricted-bad-dns".into(),
            subnet: "93.184.216.0".parse().unwrap(),
            gateway: "93.184.216.1".parse().unwrap(),
            dns: vec!["169.254.169.254".parse().unwrap()],
        };
        assert_eq!(
            attestation.validate(),
            Err(SandboxValidationError::InvalidNetwork)
        );

        let attestation = RestrictedNetworkAttestation {
            network_name: "gobrowse-restricted-bad-name".into(),
            subnet: "93.184.216.0".parse().unwrap(),
            gateway: "169.254.169.254".parse().unwrap(), // metadata
            dns: vec![],
        };
        assert_eq!(
            attestation.validate(),
            Err(SandboxValidationError::InvalidNetwork)
        );

        let attestation = RestrictedNetworkAttestation {
            network_name: "gobrowse-restricted-loopback".into(),
            subnet: "127.0.0.0".parse().unwrap(),
            gateway: "127.0.0.1".parse().unwrap(), // loopback
            dns: vec![],
        };
        assert_eq!(
            attestation.validate(),
            Err(SandboxValidationError::InvalidNetwork)
        );
    }

    #[test]
    fn restricted_network_attestation_accepts_public_only() {
        let attestation = RestrictedNetworkAttestation {
            network_name: "gobrowse-restricted-egress".into(),
            subnet: "93.184.216.0".parse().unwrap(),
            gateway: "93.184.216.1".parse().unwrap(),
            dns: vec!["1.1.1.1".parse().unwrap(), "8.8.8.8".parse().unwrap()],
        };
        assert_eq!(attestation.validate(), Ok(()));
    }

    #[test]
    fn protocol_rejects_unknown_fields() {
        let request = format!(
            r#"{{"version":1,"request_id":"{}","token":"token","operation":{{"op":"health"}},"extra":true}}"#,
            Uuid::nil()
        );
        assert!(serde_json::from_str::<RequestEnvelope>(&request).is_err());
    }
}
