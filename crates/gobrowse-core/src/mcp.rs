use serde::{Deserialize, Serialize};
use url::Url;

pub const CURRENT_PROTOCOL_VERSION: &str = "2026-07-28";

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum McpProtocolEra {
    Modern20260728,
    Legacy20251125,
    Legacy20250618,
    Legacy20250326,
    Legacy20241105,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "transport", rename_all = "snake_case")]
pub enum McpTransport {
    Stdio { command: String, args: Vec<String> },
    StreamableHttp { endpoint: Url },
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "SCREAMING_SNAKE_CASE")]
pub enum DiagnosticStatus {
    Pass,
    Warn,
    Fail,
    Skipped,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct DiagnosticCheck {
    pub code: String,
    pub label: String,
    pub status: DiagnosticStatus,
    pub detail: String,
    pub latency_ms: Option<u64>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct McpDoctorReport {
    pub server_name: String,
    pub transport: McpTransport,
    pub negotiated_protocol: Option<String>,
    pub checks: Vec<DiagnosticCheck>,
    pub tool_count: Option<usize>,
    pub resource_count: Option<usize>,
    pub prompt_count: Option<usize>,
}

impl McpDoctorReport {
    pub fn overall_status(&self) -> DiagnosticStatus {
        if self
            .checks
            .iter()
            .any(|check| check.status == DiagnosticStatus::Fail)
        {
            DiagnosticStatus::Fail
        } else if self
            .checks
            .iter()
            .any(|check| check.status == DiagnosticStatus::Warn)
        {
            DiagnosticStatus::Warn
        } else {
            DiagnosticStatus::Pass
        }
    }
}
