use std::{
    net::{IpAddr, Ipv4Addr, Ipv6Addr},
    path::{Component, Path, PathBuf},
};

use serde::{Deserialize, Serialize};
use thiserror::Error;
use uuid::Uuid;

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "SCREAMING_SNAKE_CASE")]
pub enum NetworkPolicy {
    None,
    #[default]
    Restricted,
    Full,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ResourceLimits {
    pub cpu_millis: u32,
    pub memory_bytes: u64,
    pub pids: u32,
    pub execution_seconds: u64,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct TerminalStartRequest {
    pub request_id: Uuid,
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
    #[error("workspace path must be relative and cannot traverse parents")]
    UnsafePath,
    #[error("terminal dimensions are outside the supported range")]
    InvalidTerminalSize,
    #[error("resource limits must be positive")]
    InvalidLimits,
}

impl TerminalStartRequest {
    pub fn validate(&self) -> Result<PathBuf, SandboxValidationError> {
        if self.command.is_empty() || self.command[0].is_empty() {
            return Err(SandboxValidationError::EmptyCommand);
        }
        let path = validate_workspace_path(&self.working_directory)?;
        if !(20..=1_000).contains(&self.cols) || !(5..=500).contains(&self.rows) {
            return Err(SandboxValidationError::InvalidTerminalSize);
        }
        if self.limits.cpu_millis == 0
            || self.limits.memory_bytes == 0
            || self.limits.pids == 0
            || self.limits.execution_seconds == 0
        {
            return Err(SandboxValidationError::InvalidLimits);
        }
        Ok(path)
    }
}

pub fn validate_workspace_path(value: &str) -> Result<PathBuf, SandboxValidationError> {
    if value.is_empty() || value.as_bytes().contains(&0) {
        return Err(SandboxValidationError::UnsafePath);
    }
    let path = Path::new(value);
    if path.is_absolute()
        || path.components().any(|component| {
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
    !(ip.is_unspecified()
        || ip.is_loopback()
        || ip.is_multicast()
        || (segments[0] & 0xfe00) == 0xfc00
        || (segments[0] & 0xffc0) == 0xfe80
        || (segments[0] & 0xffc0) == 0xfec0
        || (segments[0] == 0x2001 && segments[1] == 0x0db8))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn workspace_paths_reject_traversal_and_absolute_paths() {
        assert!(validate_workspace_path("src/lib.rs").is_ok());
        assert!(validate_workspace_path("../host").is_err());
        assert!(validate_workspace_path("/etc/passwd").is_err());
    }

    #[test]
    fn restricted_egress_blocks_metadata_and_private_networks() {
        for address in [
            "169.254.169.254",
            "127.0.0.1",
            "10.0.0.1",
            "192.168.1.1",
            "::1",
            "fd00::1",
        ] {
            assert!(
                !is_public_destination(address.parse().unwrap()),
                "{address}"
            );
        }
        assert!(is_public_destination("1.1.1.1".parse().unwrap()));
    }
}
