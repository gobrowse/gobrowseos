//! Portable domain model and provider-neutral runtime contracts for Gobrowse OS.

pub mod activity;
pub mod agent;
pub mod context;
pub mod fake_model;
pub mod library;
pub mod mcp;
pub mod model;
pub mod plugin;
pub mod policy;
pub mod redaction;
pub mod sandbox;
pub mod scheduler;
pub mod skills;
pub mod tools;
pub mod worktrees;

use serde::{Deserialize, Serialize};
use uuid::Uuid;

/// A user-visible API error. Internal causes and secrets must not be serialized here.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ApiError {
    pub code: String,
    pub message: String,
    pub correlation_id: Uuid,
}

/// Version information returned by diagnostics.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct VersionInfo {
    pub version: String,
    pub api_version: String,
    pub schema_version: i64,
    pub build_commit: Option<String>,
}
