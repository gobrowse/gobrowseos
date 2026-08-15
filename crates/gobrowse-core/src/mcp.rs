use std::net::IpAddr;

use base64::{Engine as _, engine::general_purpose::URL_SAFE_NO_PAD};
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use subtle::ConstantTimeEq;
use url::Url;

pub const CURRENT_PROTOCOL_VERSION: &str = "2026-07-28";

// --- protocol enums and transport ---

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum McpProtocolEra {
    #[serde(rename = "2026-07-28")]
    Modern20260728,
    #[serde(rename = "2025-11-25")]
    Legacy20251125,
    #[serde(rename = "2025-06-18")]
    Legacy20250618,
    #[serde(rename = "2025-03-26")]
    Legacy20250326,
    #[serde(rename = "2024-11-05")]
    Legacy20241105,
}

/// Protocol versions implemented by Gobrowse, in local preference order.
pub const SUPPORTED_PROTOCOL_ERAS: [McpProtocolEra; 2] = [
    McpProtocolEra::Modern20260728,
    McpProtocolEra::Legacy20251125,
];

#[path = "mcp/capabilities.rs"]
pub mod capabilities;
#[path = "mcp/lifecycle.rs"]
pub mod lifecycle;
#[path = "mcp/model.rs"]
pub mod model;
#[path = "mcp/server.rs"]
pub mod server;
#[path = "mcp/validation.rs"]
pub mod validation;
#[path = "mcp/wire.rs"]
pub mod wire;

impl McpProtocolEra {
    /// Return the exact date-based version identifier used on the wire.
    pub const fn wire_version(self) -> &'static str {
        match self {
            Self::Modern20260728 => CURRENT_PROTOCOL_VERSION,
            Self::Legacy20251125 => "2025-11-25",
            Self::Legacy20250618 => "2025-06-18",
            Self::Legacy20250326 => "2025-03-26",
            Self::Legacy20241105 => "2024-11-05",
        }
    }

    /// Map a wire version to its isolated protocol adapter.
    pub fn from_wire_version(version: &str) -> Option<Self> {
        SUPPORTED_PROTOCOL_ERAS
            .into_iter()
            .find(|era| era.wire_version() == version)
    }

    /// Whether this version uses stateless per-request metadata rather than
    /// the legacy `initialize` handshake.
    pub const fn is_modern(self) -> bool {
        matches!(self, Self::Modern20260728)
    }
}

#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum McpVersionSelectionError {
    #[error("server did not advertise any MCP protocol versions")]
    NoVersionsAdvertised,
    #[error("server and client do not share a supported MCP protocol version")]
    NoMutuallySupportedVersion,
}

/// Select the locally preferred protocol adapter advertised by a server.
///
/// This handles both `server/discover`'s `supportedVersions` and the
/// `UnsupportedProtocolVersionError.data.supported` list. Unknown versions
/// are ignored rather than interpreted as date strings: a version is usable
/// only when Gobrowse has an explicit wire adapter for it. The caller must
/// dispatch legacy selections through initialization rather than retrying a
/// modern request with legacy wire semantics.
///
/// # Errors
/// Returns [`McpVersionSelectionError::NoVersionsAdvertised`] for an empty
/// server list, or [`McpVersionSelectionError::NoMutuallySupportedVersion`]
/// when the list contains no version implemented by Gobrowse.
pub fn select_protocol_version(
    server_supported: &[impl AsRef<str>],
) -> Result<McpProtocolEra, McpVersionSelectionError> {
    if server_supported.is_empty() {
        return Err(McpVersionSelectionError::NoVersionsAdvertised);
    }

    SUPPORTED_PROTOCOL_ERAS
        .into_iter()
        .find(|era| {
            server_supported
                .iter()
                .any(|version| version.as_ref() == era.wire_version())
        })
        .ok_or(McpVersionSelectionError::NoMutuallySupportedVersion)
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

// --- transport endpoint validation ---

/// Validate that an [`McpTransport`] uses a supported scheme or a non-empty
/// command.  This is a pure static check; it does not resolve the endpoint.
///
/// * `Stdio` → Pass iff the command string is non-empty.
/// * `StreamableHttp` → Pass iff the endpoint scheme is `http` or `https`;
///   any other scheme (file, ftp, unix, etc.) is Fail.
pub fn validate_transport_endpoint(transport: &McpTransport) -> DiagnosticCheck {
    match transport {
        McpTransport::Stdio { command, .. } => {
            if command.is_empty() {
                DiagnosticCheck {
                    code: "mcp.transport".into(),
                    label: "transport endpoint".into(),
                    status: DiagnosticStatus::Fail,
                    detail: "stdio command is empty".into(),
                    latency_ms: None,
                }
            } else {
                DiagnosticCheck {
                    code: "mcp.transport".into(),
                    label: "transport endpoint".into(),
                    status: DiagnosticStatus::Pass,
                    detail: format!("stdio transport via {command}"),
                    latency_ms: None,
                }
            }
        }
        McpTransport::StreamableHttp { endpoint } => match endpoint.scheme() {
            "http" | "https" => DiagnosticCheck {
                code: "mcp.transport".into(),
                label: "transport endpoint".into(),
                status: DiagnosticStatus::Pass,
                detail: format!("streamable HTTP endpoint {endpoint}"),
                latency_ms: None,
            },
            _ => DiagnosticCheck {
                code: "mcp.transport".into(),
                label: "transport endpoint".into(),
                status: DiagnosticStatus::Fail,
                detail: format!("disallowed endpoint scheme: {endpoint}"),
                latency_ms: None,
            },
        },
    }
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

// release-gated: real RS256/JWKS rotation against live OIDC providers is
// not covered here; this is an HS256 mock-IdP simulation.
//
// ── OAuth token validation (HS256 mock conformance) ─────────────
//
// Pure JWT-claim validation logic + manual HMAC-SHA256 verification.
// The HS256 choice reuses the workspace deps (sha2, subtle, base64)
// without adding jsonwebtoken or ring.  No I/O — all functions below
// are pure and suitable for use in both unit tests and the MCP doctor
// conformance harness.

/// HMAC-SHA256 block size in bytes (RFC 2104 §2).
const HMAC_SHA256_BLOCK_SIZE: usize = 64;

/// Inner pad byte (RFC 2104 §2).
const HMAC_IPAD: u8 = 0x36;

/// Outer pad byte (RFC 2104 §2).
const HMAC_OPAD: u8 = 0x5c;

/// Error variants for MCP OAuth token validation.
///
/// Every variant carries a human-readable description via `#[error]`.
/// The order matches the validation pipeline: format → signature →
/// temporal → binding → identity.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum McpAuthError {
    #[error("token format is invalid (not a 3-part JWT)")]
    InvalidFormat,
    #[error("token signature does not match")]
    InvalidSignature,
    #[error("token has expired (exp < now)")]
    Expired,
    #[error("token audience does not match any expected audience")]
    AudienceMismatch,
    #[error("token issuer does not match expected issuer")]
    IssuerMismatch,
    #[error("token is not yet valid (nbf > now)")]
    NotYetValid,
}

/// Parsed JWT claims relevant to MCP audience-binding validation.
///
/// `aud` is always normalized to a `Vec<String>` during parsing.
#[derive(Debug, Clone)]
pub struct McpTokenClaims {
    pub iss: String,
    pub aud: Vec<String>,
    pub exp: i64,
    pub nbf: Option<i64>,
    pub sub: String,
}

// ── private helpers ─────────────────────────────────────────────

/// Compute HMAC-SHA256(key, message) per RFC 2104 using only [`sha2::Sha256`].
///
/// This is a private copy of the same algorithm used in the server crate's
/// [`webhooks::hmac_sha256`], kept here to avoid a core↔server dependency.
///
/// # Panics
/// Cannot panic — `Sha256::new()` + `update` + `finalize` never fail.
fn hmac_sha256(key: &[u8], message: &[u8]) -> [u8; 32] {
    // Steps 1-2: normalize key to exactly block-size bytes.
    let mut key_block = [0u8; HMAC_SHA256_BLOCK_SIZE];

    if key.len() > HMAC_SHA256_BLOCK_SIZE {
        let mut h = Sha256::new();
        h.update(key);
        let hashed = h.finalize();
        key_block[..32].copy_from_slice(&hashed);
    } else {
        key_block[..key.len()].copy_from_slice(key);
    }

    // Step 3: inner hash = H((key XOR ipad) || message)
    let mut ipad_block = [HMAC_IPAD; HMAC_SHA256_BLOCK_SIZE];
    for i in 0..HMAC_SHA256_BLOCK_SIZE {
        ipad_block[i] ^= key_block[i];
    }
    let mut inner = Sha256::new();
    inner.update(ipad_block);
    inner.update(message);
    let inner_hash = inner.finalize();

    // Step 4: outer hash = H((key XOR opad) || inner_hash)
    let mut opad_block = [HMAC_OPAD; HMAC_SHA256_BLOCK_SIZE];
    for i in 0..HMAC_SHA256_BLOCK_SIZE {
        opad_block[i] ^= key_block[i];
    }
    let mut outer = Sha256::new();
    outer.update(opad_block);
    outer.update(inner_hash);
    outer.finalize().into()
}

/// Base64url-no-pad decode helper.  Maps decode failures to [`McpAuthError::InvalidFormat`].
fn base64url_decode(input: &str) -> Result<Vec<u8>, McpAuthError> {
    URL_SAFE_NO_PAD
        .decode(input)
        .map_err(|_| McpAuthError::InvalidFormat)
}

// ── public API ──────────────────────────────────────────────────

/// Parse a JWT into its three components **without verifying the signature**.
///
/// Returns `(header_json, payload_json, raw_signature_b64_bytes)` where the
/// third element is a slice of the original `token` string.
///
/// # Errors
/// Returns [`McpAuthError::InvalidFormat`] if the token does not contain
/// exactly two '.' separators or if the header/payload are not valid
/// base64url-encoded JSON.
pub fn parse_jwt_unverified(
    token: &str,
) -> Result<(serde_json::Value, serde_json::Value, &[u8]), McpAuthError> {
    let mut parts = token.splitn(3, '.');
    let header_b64 = parts.next().ok_or(McpAuthError::InvalidFormat)?;
    let payload_b64 = parts.next().ok_or(McpAuthError::InvalidFormat)?;
    let sig_b64 = parts.next().ok_or(McpAuthError::InvalidFormat)?;

    // Ensure exactly 3 parts (no extra '.' after the signature).
    if parts.next().is_some() {
        return Err(McpAuthError::InvalidFormat);
    }

    let header_bytes = base64url_decode(header_b64)?;
    let header: serde_json::Value =
        serde_json::from_slice(&header_bytes).map_err(|_| McpAuthError::InvalidFormat)?;

    let payload_bytes = base64url_decode(payload_b64)?;
    let payload: serde_json::Value =
        serde_json::from_slice(&payload_bytes).map_err(|_| McpAuthError::InvalidFormat)?;

    Ok((header, payload, sig_b64.as_bytes()))
}

/// Validate JWT claims against expected values.
///
/// This is the security-relevant pure logic:
/// - **Audience binding** prevents token confusion across MCP servers.
/// - **Expiry** prevents stale-token replay.
/// - **Issuer** prevents malicious-IdP tokens.
/// - **nbf** (not-before) prevents premature token use.
///
/// # Arguments
/// * `claims` — parsed claims from the token.
/// * `expected_issuer` — the exact issuer string the token must carry.
/// * `expected_audiences` — the set of acceptable audience values;
///   the token is accepted if **any** of its `aud` values appears here.
/// * `now` — current Unix timestamp (seconds).
pub fn validate_jwt_claims(
    claims: &McpTokenClaims,
    expected_issuer: &str,
    expected_audiences: &[&str],
    now: i64,
) -> Result<(), McpAuthError> {
    if claims.iss != expected_issuer {
        return Err(McpAuthError::IssuerMismatch);
    }

    if !claims
        .aud
        .iter()
        .any(|a| expected_audiences.contains(&a.as_str()))
    {
        return Err(McpAuthError::AudienceMismatch);
    }

    if claims.exp < now {
        return Err(McpAuthError::Expired);
    }

    if let Some(nbf) = claims.nbf
        && nbf > now
    {
        return Err(McpAuthError::NotYetValid);
    }

    Ok(())
}

/// Verify an HS256 (HMAC-SHA256) JWT signature.
///
/// Splits the token, recomputes `HMAC-SHA256(secret, header_b64.payload_b64)`,
/// and compares the result against the decoded signature bytes using
/// constant-time comparison via [`subtle::ConstantTimeEq`].
///
/// # Returns
/// * `Ok(true)` — signature matches.
/// * `Ok(false)` — signature does not match (tampered token).
/// * `Err(InvalidFormat)` — token cannot be parsed as a 3-part JWT.
pub fn verify_jwt_hs256(token: &str, secret: &[u8]) -> Result<bool, McpAuthError> {
    // Locate the two '.' separators without allocating.
    let first_dot = token.find('.').ok_or(McpAuthError::InvalidFormat)?;
    let second_dot = token[first_dot + 1..]
        .find('.')
        .map(|p| first_dot + 1 + p)
        .ok_or(McpAuthError::InvalidFormat)?;

    // Ensure no third '.' (extra segments).
    if token[second_dot + 1..].contains('.') {
        return Err(McpAuthError::InvalidFormat);
    }

    let header_b64 = &token[..first_dot];
    let payload_b64 = &token[first_dot + 1..second_dot];
    let sig_b64 = &token[second_dot + 1..];

    let sig_bytes = base64url_decode(sig_b64)?;

    let signing_input = format!("{header_b64}.{payload_b64}");
    let computed_mac = hmac_sha256(secret, signing_input.as_bytes());

    Ok(computed_mac.as_slice().ct_eq(&sig_bytes).into())
}

/// Full MCP OAuth token validation pipeline.
///
/// 1. Verify the HS256 signature (reject tampered tokens).
/// 2. Parse the claims from the payload.
/// 3. Validate claims (audience, expiry, issuer, nbf).
///
/// Returns the parsed claims on success.
///
/// # Example
/// ```ignore
/// use gobrowse_core::mcp::validate_mcp_oauth_token;
///
/// let claims = validate_mcp_oauth_token(
///     token,
///     b"shared-secret",
///     "https://idp.test/",
///     &["mcp://my-server"],
///     1_700_000_000,
/// )?;
/// ```
pub fn validate_mcp_oauth_token(
    token: &str,
    secret: &[u8],
    expected_issuer: &str,
    expected_audiences: &[&str],
    now: i64,
) -> Result<McpTokenClaims, McpAuthError> {
    // 1. Verify signature.
    if !verify_jwt_hs256(token, secret)? {
        return Err(McpAuthError::InvalidSignature);
    }

    // 2. Parse claims from the (now-trusted) payload.
    let (_, payload, _) = parse_jwt_unverified(token)?;

    // 3. Normalize `aud` — JWT allows either a string or an array.
    let aud = match &payload["aud"] {
        serde_json::Value::String(s) => vec![s.clone()],
        serde_json::Value::Array(arr) => arr
            .iter()
            .filter_map(|v| v.as_str().map(String::from))
            .collect(),
        _ => return Err(McpAuthError::InvalidFormat),
    };

    let claims = McpTokenClaims {
        iss: payload["iss"]
            .as_str()
            .ok_or(McpAuthError::InvalidFormat)?
            .to_string(),
        aud,
        exp: payload["exp"].as_i64().ok_or(McpAuthError::InvalidFormat)?,
        nbf: payload["nbf"].as_i64(),
        sub: payload["sub"]
            .as_str()
            .ok_or(McpAuthError::InvalidFormat)?
            .to_string(),
    };

    // 4. Validate claims.
    validate_jwt_claims(&claims, expected_issuer, expected_audiences, now)?;

    Ok(claims)
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

    // ── protocol version selection tests ──────────────────────

    #[test]
    fn protocol_eras_map_to_exact_wire_versions() {
        let cases = [
            (McpProtocolEra::Modern20260728, "2026-07-28"),
            (McpProtocolEra::Legacy20251125, "2025-11-25"),
        ];

        for (era, wire_version) in cases {
            assert_eq!(era.wire_version(), wire_version);
            assert_eq!(McpProtocolEra::from_wire_version(wire_version), Some(era));
            assert_eq!(
                serde_json::to_string(&era).unwrap(),
                format!("\"{wire_version}\"")
            );
            assert_eq!(
                serde_json::from_str::<McpProtocolEra>(&format!("\"{wire_version}\"")).unwrap(),
                era
            );
        }

        assert!(McpProtocolEra::Modern20260728.is_modern());
        assert!(!McpProtocolEra::Legacy20251125.is_modern());
        assert_eq!(McpProtocolEra::from_wire_version("2099-01-01"), None);
    }

    #[test]
    fn protocol_selection_uses_local_preference_not_server_order() {
        let server_supported = vec![
            "2025-03-26".to_string(),
            "2024-11-05".to_string(),
            CURRENT_PROTOCOL_VERSION.to_string(),
        ];

        assert_eq!(
            select_protocol_version(&server_supported).unwrap(),
            McpProtocolEra::Modern20260728
        );
    }

    #[test]
    fn protocol_selection_can_dispatch_to_a_legacy_adapter() {
        let server_supported = ["unknown-future-version", "2025-11-25"];

        assert_eq!(
            select_protocol_version(&server_supported).unwrap(),
            McpProtocolEra::Legacy20251125
        );
    }

    #[test]
    fn protocol_selection_distinguishes_empty_and_incompatible_lists() {
        let empty: [&str; 0] = [];
        assert_eq!(
            select_protocol_version(&empty).unwrap_err(),
            McpVersionSelectionError::NoVersionsAdvertised
        );
        assert_eq!(
            select_protocol_version(&["2099-01-01", "1.0"]).unwrap_err(),
            McpVersionSelectionError::NoMutuallySupportedVersion
        );
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

    // ── MCP OAuth HS256 mock-conformance tests ──────────────────

    /// Build a complete HS256-signed JWT string from header JSON,
    /// payload JSON, and a shared secret.
    ///
    /// Steps: base64url-no-pad each part, join with '.',
    /// append HMAC-SHA256 signature (also base64url-no-pad).
    fn make_hs256_jwt(
        header: &serde_json::Value,
        payload: &serde_json::Value,
        secret: &[u8],
    ) -> String {
        let header_b64 = URL_SAFE_NO_PAD.encode(serde_json::to_string(header).unwrap());
        let payload_b64 = URL_SAFE_NO_PAD.encode(serde_json::to_string(payload).unwrap());

        let signing_input = format!("{header_b64}.{payload_b64}");
        let sig = hmac_sha256(secret, signing_input.as_bytes());
        let sig_b64 = URL_SAFE_NO_PAD.encode(sig);

        format!("{header_b64}.{payload_b64}.{sig_b64}")
    }

    /// Standard JWT header for HS256 tokens.
    fn hs256_header() -> serde_json::Value {
        serde_json::json!({"alg": "HS256", "typ": "JWT"})
    }

    /// All HS256 JWT tests share this secret.
    const TEST_SECRET: &[u8] = b"shared-secret-for-mcp-oauth-tests";

    // ── happy path ──────────────────────────────────────────────

    #[test]
    fn valid_hs256_token_passes_validation() {
        let now: i64 = 1_700_000_000;
        let header = hs256_header();
        let payload = serde_json::json!({
            "iss": "https://idp.test/",
            "aud": ["mcp://my-server"],
            "exp": now + 3600,
            "sub": "user-42"
        });
        let token = make_hs256_jwt(&header, &payload, TEST_SECRET);

        let result = validate_mcp_oauth_token(
            &token,
            TEST_SECRET,
            "https://idp.test/",
            &["mcp://my-server"],
            now,
        );
        assert!(result.is_ok(), "expected Ok but got {result:?}");
        let claims = result.unwrap();
        assert_eq!(claims.iss, "https://idp.test/");
        assert_eq!(claims.aud, vec!["mcp://my-server"]);
        assert_eq!(claims.exp, now + 3600);
        assert_eq!(claims.sub, "user-42");
        assert!(claims.nbf.is_none());
    }

    #[test]
    fn token_with_nbf_passes_when_now_is_after_nbf() {
        let now: i64 = 1_700_000_000;
        let header = hs256_header();
        let payload = serde_json::json!({
            "iss": "https://idp.test/",
            "aud": ["mcp://my-server"],
            "exp": now + 3600,
            "nbf": now - 60,
            "sub": "user-42"
        });
        let token = make_hs256_jwt(&header, &payload, TEST_SECRET);

        let result = validate_mcp_oauth_token(
            &token,
            TEST_SECRET,
            "https://idp.test/",
            &["mcp://my-server"],
            now,
        );
        assert!(result.is_ok(), "expected Ok but got {result:?}");
    }

    // ── signature checks ────────────────────────────────────────

    #[test]
    fn tampered_signature_is_rejected() {
        let now: i64 = 1_700_000_000;
        let header = hs256_header();
        let payload = serde_json::json!({
            "iss": "https://idp.test/",
            "aud": ["mcp://my-server"],
            "exp": now + 3600,
            "sub": "user-42"
        });
        let token = make_hs256_jwt(&header, &payload, TEST_SECRET);

        // Tamper the *decoded* signature bytes so base64url remains valid.
        let second_dot = token.rfind('.').unwrap();
        let sig_b64 = &token[second_dot + 1..];
        let mut sig_bytes = URL_SAFE_NO_PAD.decode(sig_b64).unwrap();
        if let Some(b) = sig_bytes.last_mut() {
            *b ^= 0x01;
        }
        let tampered_sig_b64 = URL_SAFE_NO_PAD.encode(&sig_bytes);
        let token = format!("{}.{tampered_sig_b64}", &token[..second_dot]);

        let result = validate_mcp_oauth_token(
            &token,
            TEST_SECRET,
            "https://idp.test/",
            &["mcp://my-server"],
            now,
        );
        assert_eq!(result.unwrap_err(), McpAuthError::InvalidSignature);
    }

    #[test]
    fn wrong_secret_is_rejected() {
        let now: i64 = 1_700_000_000;
        let header = hs256_header();
        let payload = serde_json::json!({
            "iss": "https://idp.test/",
            "aud": ["mcp://my-server"],
            "exp": now + 3600,
            "sub": "user-42"
        });
        let token = make_hs256_jwt(&header, &payload, TEST_SECRET);

        let result = validate_mcp_oauth_token(
            &token,
            b"wrong-secret",
            "https://idp.test/",
            &["mcp://my-server"],
            now,
        );
        assert_eq!(result.unwrap_err(), McpAuthError::InvalidSignature);
    }

    // ── temporal checks ─────────────────────────────────────────

    #[test]
    fn expired_token_is_rejected() {
        let now: i64 = 1_700_000_000;
        let header = hs256_header();
        let payload = serde_json::json!({
            "iss": "https://idp.test/",
            "aud": ["mcp://my-server"],
            "exp": now - 1,
            "sub": "user-42"
        });
        let token = make_hs256_jwt(&header, &payload, TEST_SECRET);

        let result = validate_mcp_oauth_token(
            &token,
            TEST_SECRET,
            "https://idp.test/",
            &["mcp://my-server"],
            now,
        );
        assert_eq!(result.unwrap_err(), McpAuthError::Expired);
    }

    #[test]
    fn not_yet_valid_token_is_rejected() {
        let now: i64 = 1_700_000_000;
        let header = hs256_header();
        let payload = serde_json::json!({
            "iss": "https://idp.test/",
            "aud": ["mcp://my-server"],
            "exp": now + 3600,
            "nbf": now + 60,
            "sub": "user-42"
        });
        let token = make_hs256_jwt(&header, &payload, TEST_SECRET);

        let result = validate_mcp_oauth_token(
            &token,
            TEST_SECRET,
            "https://idp.test/",
            &["mcp://my-server"],
            now,
        );
        assert_eq!(result.unwrap_err(), McpAuthError::NotYetValid);
    }

    #[test]
    fn token_valid_at_exact_exp_boundary() {
        let now: i64 = 1_700_000_000;
        let header = hs256_header();
        let payload = serde_json::json!({
            "iss": "https://idp.test/",
            "aud": ["mcp://my-server"],
            "exp": now,
            "sub": "user-42"
        });
        let token = make_hs256_jwt(&header, &payload, TEST_SECRET);

        // At exactly `exp == now`, the token is still valid
        // (exp is inclusive per our implementation: exp >= now).
        let result = validate_mcp_oauth_token(
            &token,
            TEST_SECRET,
            "https://idp.test/",
            &["mcp://my-server"],
            now,
        );
        assert!(result.is_ok(), "expected Ok but got {result:?}");
    }

    // ── audience checks ─────────────────────────────────────────

    #[test]
    fn audience_mismatch_is_rejected() {
        let now: i64 = 1_700_000_000;
        let header = hs256_header();
        let payload = serde_json::json!({
            "iss": "https://idp.test/",
            "aud": ["other-server"],
            "exp": now + 3600,
            "sub": "user-42"
        });
        let token = make_hs256_jwt(&header, &payload, TEST_SECRET);

        let result = validate_mcp_oauth_token(
            &token,
            TEST_SECRET,
            "https://idp.test/",
            &["mcp://my-server"],
            now,
        );
        assert_eq!(result.unwrap_err(), McpAuthError::AudienceMismatch);
    }

    #[test]
    fn token_with_multiple_audiences_accepts_if_any_matches() {
        let now: i64 = 1_700_000_000;
        let header = hs256_header();
        let payload = serde_json::json!({
            "iss": "https://idp.test/",
            "aud": ["other", "mcp://my-server", "third"],
            "exp": now + 3600,
            "sub": "user-42"
        });
        let token = make_hs256_jwt(&header, &payload, TEST_SECRET);

        let result = validate_mcp_oauth_token(
            &token,
            TEST_SECRET,
            "https://idp.test/",
            &["mcp://my-server"],
            now,
        );
        assert!(result.is_ok(), "expected Ok but got {result:?}");
    }

    // ── issuer checks ───────────────────────────────────────────

    #[test]
    fn issuer_mismatch_is_rejected() {
        let now: i64 = 1_700_000_000;
        let header = hs256_header();
        let payload = serde_json::json!({
            "iss": "https://evil.test/",
            "aud": ["mcp://my-server"],
            "exp": now + 3600,
            "sub": "user-42"
        });
        let token = make_hs256_jwt(&header, &payload, TEST_SECRET);

        let result = validate_mcp_oauth_token(
            &token,
            TEST_SECRET,
            "https://idp.test/",
            &["mcp://my-server"],
            now,
        );
        assert_eq!(result.unwrap_err(), McpAuthError::IssuerMismatch);
    }

    // ── format checks ───────────────────────────────────────────

    #[test]
    fn malformed_token_is_rejected() {
        let result = validate_mcp_oauth_token(
            "not-a-jwt",
            TEST_SECRET,
            "https://idp.test/",
            &["mcp://my-server"],
            1_700_000_000,
        );
        assert_eq!(result.unwrap_err(), McpAuthError::InvalidFormat);
    }

    #[test]
    fn token_with_four_parts_is_rejected() {
        let now: i64 = 1_700_000_000;
        let header = hs256_header();
        let payload = serde_json::json!({
            "iss": "https://idp.test/",
            "aud": ["mcp://my-server"],
            "exp": now + 3600,
            "sub": "user-42"
        });
        let mut token = make_hs256_jwt(&header, &payload, TEST_SECRET);
        token.push_str(".extra");

        let result = validate_mcp_oauth_token(
            &token,
            TEST_SECRET,
            "https://idp.test/",
            &["mcp://my-server"],
            now,
        );
        assert_eq!(result.unwrap_err(), McpAuthError::InvalidFormat);
    }

    #[test]
    fn token_with_non_base64url_signature_is_rejected() {
        let now: i64 = 1_700_000_000;
        let header = hs256_header();
        let payload = serde_json::json!({
            "iss": "https://idp.test/",
            "aud": ["mcp://my-server"],
            "exp": now + 3600,
            "sub": "user-42"
        });
        let header_b64 = URL_SAFE_NO_PAD.encode(serde_json::to_string(&header).unwrap());
        let payload_b64 = URL_SAFE_NO_PAD.encode(serde_json::to_string(&payload).unwrap());
        // Signature with invalid base64url chars.
        let token = format!("{header_b64}.{payload_b64}.!!!not-valid!!!");

        let result = validate_mcp_oauth_token(
            &token,
            TEST_SECRET,
            "https://idp.test/",
            &["mcp://my-server"],
            now,
        );
        assert_eq!(result.unwrap_err(), McpAuthError::InvalidFormat);
    }

    // ── parse_jwt_unverified unit tests ─────────────────────────

    #[test]
    fn parse_jwt_unverified_extracts_all_three_parts() {
        let header = hs256_header();
        let payload = serde_json::json!({"sub": "test"});
        let token = make_hs256_jwt(&header, &payload, TEST_SECRET);

        let (h, p, sig) = parse_jwt_unverified(&token).unwrap();
        assert_eq!(h["alg"], "HS256");
        assert_eq!(p["sub"], "test");
        assert!(!sig.is_empty(), "signature should not be empty");
    }

    // ── verify_jwt_hs256 unit tests ─────────────────────────────

    #[test]
    fn verify_jwt_hs256_detects_valid_signature() {
        let header = hs256_header();
        let payload = serde_json::json!({"sub": "test"});
        let token = make_hs256_jwt(&header, &payload, TEST_SECRET);

        assert!(verify_jwt_hs256(&token, TEST_SECRET).unwrap());
    }

    #[test]
    fn verify_jwt_hs256_detects_invalid_signature() {
        let header = hs256_header();
        let payload = serde_json::json!({"sub": "test"});

        let header_b64 = URL_SAFE_NO_PAD.encode(serde_json::to_string(&header).unwrap());
        let payload_b64 = URL_SAFE_NO_PAD.encode(serde_json::to_string(&payload).unwrap());
        let sig_b64 = URL_SAFE_NO_PAD.encode([0u8; 32]);
        let token = format!("{header_b64}.{payload_b64}.{sig_b64}");

        assert!(!verify_jwt_hs256(&token, TEST_SECRET).unwrap());
    }

    #[test]
    fn verify_jwt_hs256_rejects_malformed_token() {
        assert_eq!(
            verify_jwt_hs256("not-a-jwt", TEST_SECRET).unwrap_err(),
            McpAuthError::InvalidFormat
        );
    }

    // ── audit: audience binding vs oauth tokens ─────────────────

    #[test]
    fn audience_binding_and_oauth_token_are_independent_pass() {
        // Prove that the old static audience check and the new
        // OAuth token validation are independent and can both Pass
        // for a valid configuration.

        // 1. Static audience binding (passing).
        let target = Url::parse("https://api.example.com").unwrap();
        let static_check = validate_audience_binding("test_tool", &target, &["api.example.com"]);
        assert_eq!(static_check.status, DiagnosticStatus::Pass);

        // 2. OAuth token validation (independent, also passing).
        let now: i64 = 1_700_000_000;
        let header = hs256_header();
        let payload = serde_json::json!({
            "iss": "https://idp.test/",
            "aud": ["mcp://my-server"],
            "exp": now + 3600,
            "sub": "user-42"
        });
        let token = make_hs256_jwt(&header, &payload, TEST_SECRET);
        let result = validate_mcp_oauth_token(
            &token,
            TEST_SECRET,
            "https://idp.test/",
            &["mcp://my-server"],
            now,
        );
        assert!(result.is_ok(), "oauth token validation failed: {result:?}");
    }

    // ── validate_transport_endpoint tests ────────────────────────

    #[test]
    fn streamable_http_accepts_https_rejects_file_ftp_unix() {
        // HTTPS → Pass
        let https = McpTransport::StreamableHttp {
            endpoint: Url::parse("https://mcp.example.com/rpc").unwrap(),
        };
        let check = validate_transport_endpoint(&https);
        assert_eq!(check.status, DiagnosticStatus::Pass);

        // HTTP → Pass
        let http = McpTransport::StreamableHttp {
            endpoint: Url::parse("http://localhost:3000/mcp").unwrap(),
        };
        let check = validate_transport_endpoint(&http);
        assert_eq!(check.status, DiagnosticStatus::Pass);

        // file:// → Fail
        let file = McpTransport::StreamableHttp {
            endpoint: Url::parse("file:///etc/passwd").unwrap(),
        };
        let check = validate_transport_endpoint(&file);
        assert_eq!(check.status, DiagnosticStatus::Fail);
        assert!(check.detail.contains("disallowed endpoint scheme"));

        // ftp:// → Fail
        let ftp = McpTransport::StreamableHttp {
            endpoint: Url::parse("ftp://evil.example/tool").unwrap(),
        };
        let check = validate_transport_endpoint(&ftp);
        assert_eq!(check.status, DiagnosticStatus::Fail);
        assert!(check.detail.contains("disallowed endpoint scheme"));
    }

    #[test]
    fn stdio_passes_when_command_nonempty() {
        let stdio = McpTransport::Stdio {
            command: "npx".into(),
            args: vec![
                "-y".into(),
                "@modelcontextprotocol/server-filesystem".into(),
            ],
        };
        let check = validate_transport_endpoint(&stdio);
        assert_eq!(check.status, DiagnosticStatus::Pass);
        assert!(check.detail.contains("npx"));
    }

    #[test]
    fn stdio_fails_when_command_is_empty() {
        let stdio = McpTransport::Stdio {
            command: String::new(),
            args: vec![],
        };
        let check = validate_transport_endpoint(&stdio);
        assert_eq!(check.status, DiagnosticStatus::Fail);
        assert!(check.detail.contains("empty"));
    }

    // ── validate_mcp_oauth_token: string aud + missing claims ───

    #[test]
    fn validate_mcp_oauth_token_handles_string_aud_and_missing_claims() {
        let now: i64 = 1_700_000_000;
        let header = hs256_header();

        // String aud (not array) → Ok
        let payload = serde_json::json!({
            "iss": "https://idp.test/",
            "aud": "mcp://my-server",
            "exp": now + 3600,
            "sub": "user-42"
        });
        let token = make_hs256_jwt(&header, &payload, TEST_SECRET);
        let result = validate_mcp_oauth_token(
            &token,
            TEST_SECRET,
            "https://idp.test/",
            &["mcp://my-server"],
            now,
        );
        assert!(
            result.is_ok(),
            "string aud should be accepted, got {result:?}"
        );

        // Missing iss → InvalidFormat
        let payload = serde_json::json!({
            "aud": ["mcp://my-server"],
            "exp": now + 3600,
            "sub": "user-42"
        });
        let token = make_hs256_jwt(&header, &payload, TEST_SECRET);
        let result = validate_mcp_oauth_token(
            &token,
            TEST_SECRET,
            "https://idp.test/",
            &["mcp://my-server"],
            now,
        );
        assert_eq!(result.unwrap_err(), McpAuthError::InvalidFormat);

        // Missing exp → InvalidFormat
        let payload = serde_json::json!({
            "iss": "https://idp.test/",
            "aud": ["mcp://my-server"],
            "sub": "user-42"
        });
        let token = make_hs256_jwt(&header, &payload, TEST_SECRET);
        let result = validate_mcp_oauth_token(
            &token,
            TEST_SECRET,
            "https://idp.test/",
            &["mcp://my-server"],
            now,
        );
        assert_eq!(result.unwrap_err(), McpAuthError::InvalidFormat);

        // Missing sub → InvalidFormat
        let payload = serde_json::json!({
            "iss": "https://idp.test/",
            "aud": ["mcp://my-server"],
            "exp": now + 3600
        });
        let token = make_hs256_jwt(&header, &payload, TEST_SECRET);
        let result = validate_mcp_oauth_token(
            &token,
            TEST_SECRET,
            "https://idp.test/",
            &["mcp://my-server"],
            now,
        );
        assert_eq!(result.unwrap_err(), McpAuthError::InvalidFormat);

        // Non-string, non-array aud → InvalidFormat
        let payload = serde_json::json!({
            "iss": "https://idp.test/",
            "aud": 42,
            "exp": now + 3600,
            "sub": "user-42"
        });
        let token = make_hs256_jwt(&header, &payload, TEST_SECRET);
        let result = validate_mcp_oauth_token(
            &token,
            TEST_SECRET,
            "https://idp.test/",
            &["mcp://my-server"],
            now,
        );
        assert_eq!(result.unwrap_err(), McpAuthError::InvalidFormat);
    }
}
