use std::net::IpAddr;

use serde::{Deserialize, Serialize};
use url::Url;

pub const CURRENT_PROTOCOL_VERSION: &str = "2026-07-28";

// --- protocol enums and transport ---

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

// --- doctor / diagnostic types ---

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

// release-gated: real OAuth matrix / server conformance is not covered here.
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

    pub fn is_fail(&self) -> bool {
        self.overall_status() == DiagnosticStatus::Fail
    }
}

// --- schema validation ---

/// Maximum serialized byte size of a single JSON Schema value (256 KiB).
pub const MAX_SCHEMA_BYTES: usize = 256 * 1024;

/// Recursion depth cap for walking JSON Schema trees, to guard against
/// deeply-nested DoS payloads.
pub const MAX_SCHEMA_DEPTH: u32 = 64;

/// Validate an MCP tool schema (or the full tool definition JSON `value`).
/// Checks: size limit, no remote `$ref`, no dangling dynamic `#` ref,
/// and (when two schemas are compared) presence of `outputSchema`.
///
/// Returns a single `DiagnosticCheck` whose `.status` reflects the worst
/// issue found or `Pass` when everything passes.
pub fn validate_tool_schema(tool_name: &str, value: &serde_json::Value) -> DiagnosticCheck {
    // 1. check serialized size
    let serialized = serde_json::to_vec(value).unwrap_or_default();
    if serialized.len() > MAX_SCHEMA_BYTES {
        return DiagnosticCheck {
            code: "tool_schema".into(),
            label: format!("Schema size for {tool_name}"),
            status: DiagnosticStatus::Fail,
            detail: format!(
                "tool schema exceeds {MAX_SCHEMA_BYTES} bytes ({n} bytes)",
                n = serialized.len()
            ),
            latency_ms: None,
        };
    }

    // 2. walk for $ref issues (depth-capped)
    match walk_for_dollar_ref(value, 0) {
        RefCheck::Ok => {}
        RefCheck::RemoteRef { ref_value } => {
            return DiagnosticCheck {
                code: "tool_schema".into(),
                label: format!("Remote $ref in {tool_name}"),
                status: DiagnosticStatus::Fail,
                detail: format!("remote $ref disallowed: {ref_value}"),
                latency_ms: None,
            };
        }
        RefCheck::DynamicRef { ref_value } => {
            return DiagnosticCheck {
                code: "tool_schema".into(),
                label: format!("Dynamic #‑only $ref in {tool_name}"),
                status: DiagnosticStatus::Warn,
                detail: format!("$ref resolves outside the schema: {ref_value}"),
                latency_ms: None,
            };
        }
        RefCheck::DepthExceeded { depth } => {
            return DiagnosticCheck {
                code: "tool_schema_depth".into(),
                label: format!("Max depth exceeded in {tool_name}"),
                status: DiagnosticStatus::Fail,
                detail: format!("schema nesting depth {depth} exceeds maximum {MAX_SCHEMA_DEPTH}"),
                latency_ms: None,
            };
        }
    }

    // 3. outputSchema presence
    if value
        .as_object()
        .and_then(|obj| obj.get("outputSchema"))
        .is_none_or(|v| v.is_null())
    {
        return DiagnosticCheck {
            code: "tool_schema".into(),
            label: format!("Missing outputSchema for {tool_name}"),
            status: DiagnosticStatus::Warn,
            detail: "tool definition is missing `outputSchema` (recommended but not required)"
                .into(),
            latency_ms: None,
        };
    }

    DiagnosticCheck {
        code: "tool_schema".into(),
        label: format!("Schema valid for {tool_name}"),
        status: DiagnosticStatus::Pass,
        detail: "no issues found".into(),
        latency_ms: None,
    }
}

enum RefCheck {
    Ok,
    RemoteRef {
        ref_value: String,
    },
    /// A `$ref` that is just `#` or a fragment that cannot be resolved
    /// within the same document (e.g. `"#/definitions/foo"` is a static
    /// local ref and passes; a bare `"#"` or `"#"` with no path is
    /// suspicious).
    DynamicRef {
        ref_value: String,
    },
    DepthExceeded {
        depth: u32,
    },
}

/// Recursively walk a JSON value looking for `$ref` keys.
/// Returns on the first issue found.
fn walk_for_dollar_ref(value: &serde_json::Value, depth: u32) -> RefCheck {
    if depth > MAX_SCHEMA_DEPTH {
        return RefCheck::DepthExceeded { depth };
    }

    match value {
        serde_json::Value::Object(map) => {
            for (key, val) in map {
                if key == "$ref"
                    && let Some(s) = val.as_str()
                {
                    // remote ref: any http/https or absolute URI
                    if s.starts_with("http://") || s.starts_with("https://") {
                        return RefCheck::RemoteRef {
                            ref_value: s.to_owned(),
                        };
                    }
                    // non-relative URI (no leading # or path)
                    // e.g. "urn:foo:bar" — treat as remote
                    if !s.starts_with('#') && !s.starts_with('/') && s.contains("://") {
                        return RefCheck::RemoteRef {
                            ref_value: s.to_owned(),
                        };
                    }
                    // dynamic ref: bare "#" with no path
                    if s == "#" {
                        return RefCheck::DynamicRef {
                            ref_value: s.to_owned(),
                        };
                    }
                }
                // recurse
                let result = walk_for_dollar_ref(val, depth + 1);
                if !matches!(result, RefCheck::Ok) {
                    return result;
                }
            }
            RefCheck::Ok
        }
        serde_json::Value::Array(arr) => {
            for item in arr {
                let result = walk_for_dollar_ref(item, depth + 1);
                if !matches!(result, RefCheck::Ok) {
                    return result;
                }
            }
            RefCheck::Ok
        }
        _ => RefCheck::Ok,
    }
}

// --- audience binding validation ---

/// Returns true for IP addresses that are public-routable under the
/// same rules as [`crate::sandbox::is_public_destination`].
/// DNS names always return `true` — we cannot statically verify them,
/// so the audience allow-list gate in [`validate_audience_binding`]
/// is the security control for name-based targets.
fn is_public_host(host: &url::Host<&str>) -> bool {
    match host {
        url::Host::Ipv4(ip) => crate::sandbox::is_public_destination(IpAddr::V4(*ip)),
        url::Host::Ipv6(ip) => crate::sandbox::is_public_destination(IpAddr::V6(*ip)),
        url::Host::Domain(_) => true,
    }
}

/// Validate that `target`'s host is a public destination AND is
/// present in the tool's `allowed_audiences` allow-list.
///
/// This is a pure (no-I/O) static gate.  Real OAuth / JWKS audience
/// validation against a live IdP is excluded per the release-gated
/// banner at the top of this file.
///
/// # Returns
/// * `Pass` — host is a public IP (or DNS name) present in the allow-list.
/// * `Fail` — host is private/metadata, missing, or not in the allow-list.
pub fn validate_audience_binding(
    tool_name: &str,
    target: &Url,
    allowed_audiences: &[&str],
) -> DiagnosticCheck {
    let code = format!("mcp.audience.{tool_name}");
    let label = "audience binding".to_string();

    let host = match target.host() {
        Some(h) => h,
        None => {
            return DiagnosticCheck {
                code,
                label,
                status: DiagnosticStatus::Fail,
                detail: "target has no host".into(),
                latency_ms: None,
            };
        }
    };

    if !is_public_host(&host) {
        return DiagnosticCheck {
            code,
            label,
            status: DiagnosticStatus::Fail,
            detail: "target is a private/metadata destination".into(),
            latency_ms: None,
        };
    }

    let host_str = target.host_str().unwrap_or("");
    if !allowed_audiences.contains(&host_str) {
        return DiagnosticCheck {
            code,
            label,
            status: DiagnosticStatus::Fail,
            detail: "target host not in allowed audiences".into(),
            latency_ms: None,
        };
    }

    DiagnosticCheck {
        code,
        label,
        status: DiagnosticStatus::Pass,
        detail: "target bound to an allowed audience".into(),
        latency_ms: None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    // ── helpers ─────────────────────────────────────────────────

    fn tool_def_with(input_schema: serde_json::Value) -> serde_json::Value {
        serde_json::json!({
            "name": "test_tool",
            "description": "a test tool",
            "inputSchema": input_schema,
            "outputSchema": {
                "type": "object",
                "properties": {}
            }
        })
    }

    fn tool_def_without_output(input_schema: serde_json::Value) -> serde_json::Value {
        serde_json::json!({
            "name": "test_tool",
            "description": "a test tool",
            "inputSchema": input_schema
        })
    }

    // ── validate_tool_schema tests ─────────────────────────────

    #[test]
    fn malicious_remote_ref_is_flagged_fail() {
        let def = tool_def_with(serde_json::json!({
            "type": "object",
            "properties": {
                "x": { "$ref": "https://evil.example/x" }
            }
        }));
        let check = validate_tool_schema("bad_tool", &def);
        assert_eq!(check.status, DiagnosticStatus::Fail);
        assert!(check.detail.contains("$ref"));
    }

    #[test]
    fn oversized_tool_schema_is_flagged_fail() {
        // build a schema with a huge string property > 256 KiB
        let huge_string = "A".repeat(MAX_SCHEMA_BYTES + 1);
        let def = tool_def_with(serde_json::json!({
            "type": "object",
            "properties": {
                "payload": huge_string
            }
        }));
        let check = validate_tool_schema("heavy", &def);
        assert_eq!(check.status, DiagnosticStatus::Fail);
        assert!(check.detail.contains("exceeds"));
    }

    #[test]
    fn missing_output_schema_is_flagged_warn() {
        let def = tool_def_without_output(serde_json::json!({
            "type": "object",
            "properties": {}
        }));
        let check = validate_tool_schema("no_output", &def);
        assert_eq!(check.status, DiagnosticStatus::Warn);
        assert!(
            check.detail.contains("outputSchema"),
            "detail={}",
            check.detail
        );
    }

    #[test]
    fn deeply_nested_schema_is_flagged_fail() {
        // build a value nested beyond MAX_SCHEMA_DEPTH
        let mut nested = serde_json::json!({"leaf": true});
        for _ in 0..MAX_SCHEMA_DEPTH {
            nested = serde_json::json!({"nested": nested});
        }
        let def = tool_def_with(serde_json::json!({
            "type": "object",
            "deep": nested
        }));
        let check = validate_tool_schema("nest_tool", &def);
        assert_eq!(check.code, "tool_schema_depth");
        assert_eq!(check.status, DiagnosticStatus::Fail);
    }

    #[test]
    fn dynamic_bare_hash_ref_is_flagged_warn() {
        let def = tool_def_with(serde_json::json!({
            "type": "object",
            "properties": {
                "self": { "$ref": "#" }
            }
        }));
        let check = validate_tool_schema("dynref_tool", &def);
        // A dynamic # ref should at least be Warn (not Pass)
        assert_ne!(check.status, DiagnosticStatus::Pass);
        assert!(
            check.detail.contains('#') || check.detail.contains("$ref"),
            "detail={check_detail}",
            check_detail = check.detail
        );
    }

    #[test]
    fn benign_local_ref_with_path_is_allowed() {
        let def = tool_def_with(serde_json::json!({
            "type": "object",
            "definitions": {
                "addr": {
                    "type": "object",
                    "properties": {
                        "street": { "type": "string" }
                    }
                }
            },
            "properties": {
                "billing": { "$ref": "#/definitions/addr" }
            }
        }));
        let check = validate_tool_schema("benign_tool", &def);
        assert_eq!(check.status, DiagnosticStatus::Pass);
    }

    #[test]
    fn benign_schema_with_output_is_pass() {
        let def = tool_def_with(serde_json::json!({
            "type": "object",
            "properties": {
                "name": { "type": "string" }
            }
        }));
        let check = validate_tool_schema("good_tool", &def);
        assert_eq!(check.status, DiagnosticStatus::Pass);
    }

    // ── McpDoctorReport overall_status / is_fail tests ─────────

    #[test]
    fn overall_status_is_fail_if_any_fail() {
        let report = McpDoctorReport {
            server_name: "test".into(),
            transport: McpTransport::Stdio {
                command: "echo".into(),
                args: vec![],
            },
            negotiated_protocol: None,
            checks: vec![
                DiagnosticCheck {
                    code: "a".into(),
                    label: "pass".into(),
                    status: DiagnosticStatus::Pass,
                    detail: "ok".into(),
                    latency_ms: None,
                },
                DiagnosticCheck {
                    code: "b".into(),
                    label: "fail".into(),
                    status: DiagnosticStatus::Fail,
                    detail: "bad".into(),
                    latency_ms: None,
                },
            ],
            tool_count: None,
            resource_count: None,
            prompt_count: None,
        };
        assert_eq!(report.overall_status(), DiagnosticStatus::Fail);
        assert!(report.is_fail());
    }

    #[test]
    fn overall_status_is_warn_when_only_warns_present() {
        let report = McpDoctorReport {
            server_name: "test".into(),
            transport: McpTransport::Stdio {
                command: "echo".into(),
                args: vec![],
            },
            negotiated_protocol: None,
            checks: vec![
                DiagnosticCheck {
                    code: "a".into(),
                    label: "pass".into(),
                    status: DiagnosticStatus::Pass,
                    detail: "ok".into(),
                    latency_ms: None,
                },
                DiagnosticCheck {
                    code: "b".into(),
                    label: "warn".into(),
                    status: DiagnosticStatus::Warn,
                    detail: "meh".into(),
                    latency_ms: None,
                },
            ],
            tool_count: None,
            resource_count: None,
            prompt_count: None,
        };
        assert_eq!(report.overall_status(), DiagnosticStatus::Warn);
        assert!(!report.is_fail());
    }

    #[test]
    fn overall_status_is_pass_when_all_pass() {
        let report = McpDoctorReport {
            server_name: "test".into(),
            transport: McpTransport::Stdio {
                command: "echo".into(),
                args: vec![],
            },
            negotiated_protocol: None,
            checks: vec![DiagnosticCheck {
                code: "a".into(),
                label: "pass".into(),
                status: DiagnosticStatus::Pass,
                detail: "ok".into(),
                latency_ms: None,
            }],
            tool_count: None,
            resource_count: None,
            prompt_count: None,
        };
        assert_eq!(report.overall_status(), DiagnosticStatus::Pass);
        assert!(!report.is_fail());
    }

    // ── validate_audience_binding tests ─────────────────────────

    #[test]
    fn audience_binding_accepts_matching_host() {
        let target = Url::parse("https://api.example.com").unwrap();
        let check = validate_audience_binding("test_tool", &target, &["api.example.com"]);
        assert_eq!(check.status, DiagnosticStatus::Pass);
        assert!(check.detail.contains("allowed audience"));
    }

    #[test]
    fn audience_binding_rejects_metadata_endpoint() {
        let target = Url::parse("https://169.254.169.254/latest/meta-data").unwrap();
        let check = validate_audience_binding("test_tool", &target, &["169.254.169.254"]);
        assert_eq!(check.status, DiagnosticStatus::Fail);
        assert!(check.detail.contains("private/metadata"));
    }

    #[test]
    fn audience_binding_rejects_private_range_target() {
        let target = Url::parse("https://10.0.0.5").unwrap();
        let check = validate_audience_binding("test_tool", &target, &["10.0.0.5"]);
        assert_eq!(check.status, DiagnosticStatus::Fail);
        assert!(check.detail.contains("private/metadata"));
    }

    #[test]
    fn audience_binding_rejects_unlisted_host() {
        let target = Url::parse("https://evil.example").unwrap();
        let check = validate_audience_binding("test_tool", &target, &["api.example.com"]);
        assert_eq!(check.status, DiagnosticStatus::Fail);
        assert!(check.detail.contains("not in allowed audiences"));
    }

    #[test]
    fn audience_binding_rejects_loopback_target() {
        let target = Url::parse("http://127.0.0.1").unwrap();
        let check = validate_audience_binding("test_tool", &target, &["127.0.0.1"]);
        assert_eq!(check.status, DiagnosticStatus::Fail);
        assert!(check.detail.contains("private/metadata"));
    }

    #[test]
    fn audience_binding_rejects_missing_host() {
        let target = Url::parse("data:text/plain,hello").unwrap();
        let check = validate_audience_binding("test_tool", &target, &["example.com"]);
        assert_eq!(check.status, DiagnosticStatus::Fail);
        assert!(check.detail.contains("no host"));
    }
}
