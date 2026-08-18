//! Plugin manifest, marketplace, and source contracts.
//!
//! This module defines the versioned plugin manifest types (matching the
//! manifest JSON schema in PLAN.md), the marketplace adapter trait, and the
//! abstract [`PluginSource`] trait used by plugin installation flows. It is a
//! pure domain module: no database or server dependencies.
//!
//! # Deserialization philosophy
//!
//! Deserialization is deliberately permissive: unknown top-level fields are
//! ignored for forward compatibility, and semantically required string fields
//! default to empty so that a structurally broken manifest still deserializes.
//! [`PluginManifestV1::validate`] is the semantic gate that reports missing
//! required fields, version-format violations, empty component lists, and
//! duplicate permission scopes as typed [`PluginValidationError`]s.
//! Enum-valued fields (component `type`, sandbox `network`) are typed and
//! strict: unknown values are rejected at deserialization time, matching the
//! JSON Schema `enum` semantics.

use std::{
    collections::HashSet,
    path::{Path, PathBuf},
};

use async_trait::async_trait;
use serde::{Deserialize, Serialize};
use time::OffsetDateTime;
use url::Url;

use crate::sandbox::NetworkPolicy;

/// The only manifest version this crate understands.
pub const PLUGIN_MANIFEST_VERSION: u32 = 1;

/// Canonical plugin version pattern: `major.minor.patch` with an optional
/// `-prerelease` suffix of `[a-zA-Z0-9.]` characters.
pub const MANIFEST_VERSION_PATTERN: &str = r"^[0-9]+\.[0-9]+\.[0-9]+(-[a-zA-Z0-9.]+)?$";

/// Default `self_test.timeout_seconds` when the manifest omits it.
const DEFAULT_SELF_TEST_TIMEOUT_SECONDS: u32 = 30;

/// The kind of a component provided by a plugin.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum PluginComponentType {
    Skill,
    McpServer,
    SourceBook,
    Executable,
    Asset,
    Schema,
}

/// Publisher identity for a plugin manifest.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct PluginPublisher {
    #[serde(default)]
    pub name: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub url: Option<Url>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub email: Option<String>,
}

/// Sandbox policy requested by a plugin manifest.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct PluginSandbox {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub image: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub entrypoint: Option<Vec<String>>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub network: Option<NetworkPolicy>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub resource_limits: Option<PluginResourceLimits>,
}

/// Optional resource limits for a plugin sandbox. Absent fields fall back to
/// the daemon defaults at runtime.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct PluginResourceLimits {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub cpu_millis: Option<u32>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub memory_bytes: Option<u64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub writable_storage_bytes: Option<u64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub pids: Option<u32>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub execution_seconds: Option<u64>,
}

/// Permission allowlist declared by a plugin manifest. Scope strings for
/// filesystem/network/secrets domains are references (paths, host:port pairs,
/// vault secret reference ids), never values.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct PluginPermissions {
    #[serde(default)]
    pub filesystem_read: Vec<String>,
    #[serde(default)]
    pub filesystem_write: Vec<String>,
    #[serde(default)]
    pub network: Vec<String>,
    #[serde(default)]
    pub secrets: Vec<String>,
    #[serde(default)]
    pub subprocess: bool,
    #[serde(default)]
    pub admin: bool,
}

/// A single component declared by a plugin manifest.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct PluginComponent {
    pub r#type: PluginComponentType,
    #[serde(default)]
    pub name: String,
    #[serde(default)]
    pub r#ref: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub description: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub metadata: Option<serde_json::Value>,
}

/// Optional self-test executed during plugin installation.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct PluginSelfTest {
    #[serde(default)]
    pub command: Vec<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub expected_exit: Option<i32>,
    #[serde(default = "default_self_test_timeout_seconds")]
    pub timeout_seconds: u32,
}

fn default_self_test_timeout_seconds() -> u32 {
    DEFAULT_SELF_TEST_TIMEOUT_SECONDS
}

/// Version-1 plugin manifest matching the manifest JSON schema in PLAN.md.
///
/// Deserialization is permissive (see the module docs); call [`validate`]
/// before trusting a manifest.
///
/// [`validate`]: PluginManifestV1::validate
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct PluginManifestV1 {
    #[serde(default)]
    pub manifest_version: u32,
    #[serde(default)]
    pub name: String,
    #[serde(default)]
    pub version: String,
    #[serde(default)]
    pub description: String,
    #[serde(default)]
    pub publisher: Option<PluginPublisher>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub license: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub homepage: Option<Url>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub repository: Option<Url>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub sandbox: Option<PluginSandbox>,
    #[serde(default)]
    pub permissions: PluginPermissions,
    #[serde(default)]
    pub components: Vec<PluginComponent>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub self_test: Option<PluginSelfTest>,
}

impl PluginManifestV1 {
    /// Validates required fields, the version format, component shape, and
    /// permission-scope uniqueness.
    pub fn validate(&self) -> Result<(), PluginValidationError> {
        if self.manifest_version != PLUGIN_MANIFEST_VERSION {
            return Err(PluginValidationError::UnsupportedManifestVersion(
                self.manifest_version,
            ));
        }
        if self.name.is_empty() {
            return Err(PluginValidationError::MissingName);
        }
        if self.version.is_empty() {
            return Err(PluginValidationError::MissingVersion);
        }
        if !is_valid_manifest_version(&self.version) {
            return Err(PluginValidationError::InvalidVersionFormat(
                self.version.clone(),
            ));
        }
        let Some(publisher) = &self.publisher else {
            return Err(PluginValidationError::MissingPublisher);
        };
        if publisher.name.is_empty() {
            return Err(PluginValidationError::MissingPublisherName);
        }
        if self.components.is_empty() {
            return Err(PluginValidationError::EmptyComponents);
        }
        for component in &self.components {
            if component.name.is_empty() {
                return Err(PluginValidationError::EmptyComponentName);
            }
            if component.r#ref.is_empty() {
                return Err(PluginValidationError::EmptyComponentRef);
            }
        }
        self.permissions.validate_no_duplicate_scopes()?;
        Ok(())
    }
}

impl PluginPermissions {
    fn validate_no_duplicate_scopes(&self) -> Result<(), PluginValidationError> {
        for (domain, scopes) in [
            ("filesystem_read", self.filesystem_read.as_slice()),
            ("filesystem_write", self.filesystem_write.as_slice()),
            ("network", self.network.as_slice()),
            ("secrets", self.secrets.as_slice()),
        ] {
            if let Some(scope) = first_duplicate(scopes) {
                return Err(PluginValidationError::DuplicatePermissionScope {
                    domain: domain.to_owned(),
                    scope: scope.to_owned(),
                });
            }
        }
        Ok(())
    }
}

fn first_duplicate(scopes: &[String]) -> Option<&str> {
    let mut seen = HashSet::with_capacity(scopes.len());
    scopes
        .iter()
        .find(|scope| !seen.insert(scope.as_str()))
        .map(String::as_str)
}

/// Returns true when `version` matches [`MANIFEST_VERSION_PATTERN`].
fn is_valid_manifest_version(version: &str) -> bool {
    let (core, suffix) = match version.split_once('-') {
        Some((core, suffix)) => (core, Some(suffix)),
        None => (version, None),
    };
    let mut parts = core.split('.');
    let major = parts.next();
    let minor = parts.next();
    let patch = parts.next();
    if parts.next().is_some() {
        return false;
    }
    let (Some(major), Some(minor), Some(patch)) = (major, minor, patch) else {
        return false;
    };
    if major.is_empty() || minor.is_empty() || patch.is_empty() {
        return false;
    }
    let numeric = |part: &str| part.bytes().all(|byte| byte.is_ascii_digit());
    if !numeric(major) || !numeric(minor) || !numeric(patch) {
        return false;
    }
    match suffix {
        Some(suffix) => {
            !suffix.is_empty()
                && suffix
                    .bytes()
                    .all(|byte| byte.is_ascii_alphanumeric() || byte == b'.')
        }
        None => true,
    }
}

/// Errors reported by [`PluginManifestV1::validate`].
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum PluginValidationError {
    #[error("unsupported manifest_version {0} (expected 1)")]
    UnsupportedManifestVersion(u32),
    #[error("plugin name is required")]
    MissingName,
    #[error("plugin version is required")]
    MissingVersion,
    #[error(
        "invalid plugin version `{0}`; expected major.minor.patch with optional -prerelease suffix"
    )]
    InvalidVersionFormat(String),
    #[error("publisher is required")]
    MissingPublisher,
    #[error("publisher name is required")]
    MissingPublisherName,
    #[error("components must contain at least one entry")]
    EmptyComponents,
    #[error("component name must not be empty")]
    EmptyComponentName,
    #[error("component ref must not be empty")]
    EmptyComponentRef,
    #[error("duplicate permission scope `{scope}` in domain `{domain}`")]
    DuplicatePermissionScope { domain: String, scope: String },
}

/// A versioned plugin manifest.
///
/// Deserialization dispatches on the JSON `manifest_version` field so an
/// unknown future version fails with a clear error instead of being silently
/// parsed as version 1.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
#[serde(untagged)]
pub enum PluginManifest {
    V1(PluginManifestV1),
}

impl PluginManifest {
    /// Validates the contained manifest.
    pub fn validate(&self) -> Result<(), PluginValidationError> {
        match self {
            PluginManifest::V1(manifest) => manifest.validate(),
        }
    }

    /// Returns the version-1 payload when this manifest is version 1.
    pub fn as_v1(&self) -> Option<&PluginManifestV1> {
        match self {
            PluginManifest::V1(manifest) => Some(manifest),
        }
    }
}

impl<'de> Deserialize<'de> for PluginManifest {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: serde::Deserializer<'de>,
    {
        let value = serde_json::Value::deserialize(deserializer)?;
        match value
            .get("manifest_version")
            .and_then(serde_json::Value::as_u64)
        {
            Some(1) => serde_json::from_value(value)
                .map(PluginManifest::V1)
                .map_err(serde::de::Error::custom),
            Some(version) => Err(serde::de::Error::custom(format!(
                "unsupported manifest_version: {version}"
            ))),
            None => Err(serde::de::Error::custom(
                "missing required field `manifest_version`",
            )),
        }
    }
}

/// A plugin as returned by marketplace search and inspection.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct MarketplaceEntry {
    pub id: String,
    pub name: String,
    pub description: String,
    pub publisher: String,
    pub latest_version: String,
    pub download_count: u64,
    pub verified: bool,
    pub categories: Vec<String>,
}

/// A published version of a marketplace plugin.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct VersionInfo {
    pub version: String,
    pub published_at: OffsetDateTime,
    pub digest: String,
    pub changelog: Option<String>,
}

/// A resolved, immutable artifact download location.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ArtifactLocation {
    pub url: Url,
    /// Expected SHA-256 hex digest.
    pub digest: String,
    pub size_bytes: Option<u64>,
    pub content_type: String,
}

/// Errors returned by a [`PluginMarketplace`] implementation.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum MarketplaceError {
    #[error("plugin not found")]
    NotFound,
    #[error("marketplace unavailable")]
    Unavailable,
    #[error("rate limited")]
    RateLimited { retry_after_seconds: Option<u64> },
    #[error("invalid response from marketplace")]
    InvalidResponse,
    #[error("request timed out")]
    Timeout,
}

/// Adapter trait over a plugin marketplace (GitHub releases today; other
/// marketplaces later). Implementations must be cheap to share across async
/// tasks.
#[async_trait]
pub trait PluginMarketplace: Send + Sync {
    /// Search plugins by free-text query.
    async fn search(&self, query: &str) -> Result<Vec<MarketplaceEntry>, MarketplaceError>;
    /// Inspect a single plugin by id.
    async fn inspect(&self, id: &str) -> Result<MarketplaceEntry, MarketplaceError>;
    /// List published versions of a plugin.
    async fn versions(&self, id: &str) -> Result<Vec<VersionInfo>, MarketplaceError>;
    /// Fetch the raw manifest JSON for a plugin version.
    async fn fetch_manifest(
        &self,
        id: &str,
        version: &str,
    ) -> Result<serde_json::Value, MarketplaceError>;
    /// Resolve the immutable artifact location for a plugin version.
    async fn resolve_artifact(
        &self,
        id: &str,
        version: &str,
    ) -> Result<ArtifactLocation, MarketplaceError>;
}

/// Identity of a plugin source: `github_release`, `generic_git`,
/// `marketplace`, or `local_package`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct PluginIdentity {
    pub source_type: String,
    pub source_uri: String,
    pub commit_sha: Option<String>,
    pub version: Option<String>,
}

/// A plugin source resolved to an immutable revision.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ResolvedPluginSource {
    pub identity: PluginIdentity,
    /// Commit SHA for git sources; artifact digest for package sources.
    pub resolved_revision: String,
    /// Path of the manifest within the resolved revision.
    pub manifest_path: PathBuf,
    /// Expected SHA-256 hex digest of the artifact bundle.
    pub artifact_digest: String,
}

/// Errors returned by a [`PluginSource`] implementation.
///
/// Not `Clone`/`PartialEq` because of the [`PluginSourceError::Io`] variant
/// (`std::io::Error` is neither).
#[derive(Debug, thiserror::Error)]
pub enum PluginSourceError {
    #[error("plugin source not found")]
    NotFound,
    #[error("plugin source unavailable")]
    Unavailable,
    #[error("rate limited")]
    RateLimited { retry_after_seconds: Option<u64> },
    #[error("invalid response from plugin source")]
    InvalidResponse,
    #[error("request timed out")]
    Timeout,
    #[error("invalid plugin manifest: {0}")]
    ManifestInvalid(String),
    #[error("artifact digest mismatch")]
    DigestMismatch,
    #[error("artifact download failed: {0}")]
    Download(String),
    #[error("io error: {0}")]
    Io(#[from] std::io::Error),
}

/// Abstract plugin source: resolves immutable revisions, fetches manifests,
/// and downloads artifact bundles. Implemented by GitHub release, generic
/// git, local package, and marketplace-backed sources.
#[async_trait]
pub trait PluginSource: Send + Sync {
    /// Resolve a specific immutable revision from an identity with optional
    /// version pin.
    async fn resolve(
        &self,
        identity: &PluginIdentity,
    ) -> Result<ResolvedPluginSource, PluginSourceError>;

    /// Fetch the plugin manifest at the resolved revision.
    async fn fetch_manifest(
        &self,
        resolved: &ResolvedPluginSource,
    ) -> Result<serde_json::Value, PluginSourceError>;

    /// Download the artifact bundle to a temp workspace path.
    async fn download_artifact(
        &self,
        resolved: &ResolvedPluginSource,
        dest: &Path,
    ) -> Result<(), PluginSourceError>;
}

/// A flattened permission granted to a plugin. Domains match the
/// `plugin_permissions` CHECK constraint
/// (`filesystem_read`, `filesystem_write`, `network`, `secrets`,
/// `subprocess`, `admin`).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct PluginPermission {
    pub domain: String,
    pub scope_value: String,
}

impl From<&PluginManifestV1> for Vec<PluginPermission> {
    /// Flattens manifest permissions into a unique, order-preserving list of
    /// `(domain, scope_value)` pairs. Boolean permissions (`subprocess`,
    /// `admin`) contribute `("subprocess", "true")` / `("admin", "true")`
    /// only when enabled. Duplicate scopes are collapsed to the first
    /// occurrence; [`PluginManifestV1::validate`] rejects duplicates before
    /// installation.
    fn from(manifest: &PluginManifestV1) -> Self {
        let mut permissions = Vec::new();
        for (domain, scopes) in [
            (
                "filesystem_read",
                manifest.permissions.filesystem_read.as_slice(),
            ),
            (
                "filesystem_write",
                manifest.permissions.filesystem_write.as_slice(),
            ),
            ("network", manifest.permissions.network.as_slice()),
            ("secrets", manifest.permissions.secrets.as_slice()),
        ] {
            for scope in scopes {
                let permission = PluginPermission {
                    domain: domain.to_owned(),
                    scope_value: scope.clone(),
                };
                if !permissions.contains(&permission) {
                    permissions.push(permission);
                }
            }
        }
        for (domain, enabled) in [
            ("subprocess", manifest.permissions.subprocess),
            ("admin", manifest.permissions.admin),
        ] {
            if enabled {
                let permission = PluginPermission {
                    domain: domain.to_owned(),
                    scope_value: "true".to_owned(),
                };
                if !permissions.contains(&permission) {
                    permissions.push(permission);
                }
            }
        }
        permissions
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn valid_manifest_json() -> serde_json::Value {
        json!({
            "manifest_version": 1,
            "name": "github-actions-runner",
            "version": "1.2.3",
            "description": "Self-hosted GitHub Actions runner",
            "publisher": {
                "name": "Gobrowse Labs",
                "url": "https://example.com",
                "email": "plugins@example.com"
            },
            "license": "Apache-2.0",
            "homepage": "https://example.com/runner",
            "repository": "https://github.com/example/runner",
            "sandbox": {
                "image": "localhost/gobrowse-workspace:v1",
                "entrypoint": ["/bin/sh", "-c"],
                "network": "RESTRICTED",
                "resource_limits": {
                    "cpu_millis": 2000,
                    "memory_bytes": 1073741824,
                    "writable_storage_bytes": 1073741824,
                    "pids": 256,
                    "execution_seconds": 3600
                }
            },
            "permissions": {
                "filesystem_read": ["/workspace/**", "/tmp/**"],
                "filesystem_write": ["/workspace/**"],
                "network": ["api.example.com:443"],
                "secrets": ["GITHUB_TOKEN"],
                "subprocess": true,
                "admin": false
            },
            "components": [
                {
                    "type": "skill",
                    "name": "run-actions",
                    "ref": "skills/run-actions.md",
                    "description": "Runs GitHub Actions jobs",
                    "metadata": { "languages": ["bash"] }
                },
                {
                    "type": "mcp_server",
                    "name": "actions-mcp",
                    "ref": "mcp/actions",
                    "description": "MCP server for Actions"
                },
                {
                    "type": "source_book",
                    "name": "runner-runbook",
                    "ref": "docs/runbook.md"
                },
                {
                    "type": "executable",
                    "name": "runner",
                    "ref": "bin/runner"
                },
                {
                    "type": "asset",
                    "name": "icon",
                    "ref": "assets/icon.png"
                },
                {
                    "type": "schema",
                    "name": "workflow",
                    "ref": "schemas/workflow.json"
                }
            ],
            "self_test": {
                "command": ["bin/runner", "--self-test"],
                "expected_exit": 0
            },
            "future_field": { "forward": "compat" }
        })
    }

    fn manifest_with(mutate: impl FnOnce(&mut serde_json::Value)) -> PluginManifestV1 {
        let mut value = valid_manifest_json();
        mutate(&mut value);
        serde_json::from_value(value).unwrap()
    }

    #[test]
    fn valid_manifest_deserializes_and_validates() {
        let manifest: PluginManifestV1 = serde_json::from_value(valid_manifest_json()).unwrap();
        manifest.validate().unwrap();
        assert_eq!(manifest.manifest_version, 1);
        assert_eq!(manifest.name, "github-actions-runner");
        assert_eq!(manifest.version, "1.2.3");
        assert_eq!(manifest.components.len(), 6);
        assert_eq!(manifest.components[0].r#type, PluginComponentType::Skill);
        assert_eq!(
            manifest.sandbox.as_ref().unwrap().network,
            Some(NetworkPolicy::Restricted)
        );
        assert_eq!(manifest.self_test.as_ref().unwrap().timeout_seconds, 30);
        assert!(manifest.permissions.subprocess);
    }

    #[test]
    fn versioned_wrapper_round_trips() {
        let manifest: PluginManifest = serde_json::from_value(valid_manifest_json()).unwrap();
        manifest.validate().unwrap();
        assert!(manifest.as_v1().is_some());
        let encoded = serde_json::to_value(&manifest).unwrap();
        let decoded: PluginManifest = serde_json::from_value(encoded).unwrap();
        assert_eq!(manifest, decoded);
    }

    #[test]
    fn future_manifest_version_is_rejected() {
        let mut value = valid_manifest_json();
        value["manifest_version"] = json!(2);
        let err = serde_json::from_value::<PluginManifest>(value).unwrap_err();
        assert!(err.to_string().contains("unsupported manifest_version: 2"));
    }

    #[test]
    fn missing_manifest_version_is_rejected() {
        let mut value = valid_manifest_json();
        value.as_object_mut().unwrap().remove("manifest_version");
        let err = serde_json::from_value::<PluginManifest>(value).unwrap_err();
        assert!(
            err.to_string()
                .contains("missing required field `manifest_version`")
        );
    }

    #[test]
    fn missing_required_fields_fail_validate() {
        let cases = [
            ("name", PluginValidationError::MissingName),
            ("version", PluginValidationError::MissingVersion),
            ("publisher", PluginValidationError::MissingPublisher),
            ("components", PluginValidationError::EmptyComponents),
        ];
        for (field, expected) in cases {
            let manifest = manifest_with(|value| {
                value.as_object_mut().unwrap().remove(field);
            });
            assert_eq!(manifest.validate(), Err(expected), "field {field}");
        }
    }

    #[test]
    fn missing_manifest_version_fails_validate() {
        let manifest = manifest_with(|value| {
            value.as_object_mut().unwrap().remove("manifest_version");
        });
        assert_eq!(
            manifest.validate(),
            Err(PluginValidationError::UnsupportedManifestVersion(0))
        );
    }

    #[test]
    fn empty_components_array_fails_validate() {
        let manifest = manifest_with(|value| {
            value["components"] = json!([]);
        });
        assert_eq!(
            manifest.validate(),
            Err(PluginValidationError::EmptyComponents)
        );
    }

    #[test]
    fn empty_publisher_name_fails_validate() {
        let manifest = manifest_with(|value| {
            value["publisher"]["name"] = json!("");
        });
        assert_eq!(
            manifest.validate(),
            Err(PluginValidationError::MissingPublisherName)
        );
    }

    #[test]
    fn empty_component_ref_fails_validate() {
        let manifest = manifest_with(|value| {
            value["components"][0]["ref"] = json!("");
        });
        assert_eq!(
            manifest.validate(),
            Err(PluginValidationError::EmptyComponentRef)
        );
    }

    #[test]
    fn empty_component_name_fails_validate() {
        let manifest = manifest_with(|value| {
            value["components"][0]["name"] = json!("");
        });
        assert_eq!(
            manifest.validate(),
            Err(PluginValidationError::EmptyComponentName)
        );
    }

    #[test]
    fn valid_version_formats_are_accepted() {
        for version in [
            "1.0.0",
            "0.0.1",
            "10.20.30",
            "1.2.3-alpha",
            "1.2.3-alpha.1",
            "1.2.3-beta2",
            "1.0.0-rc.1.2",
        ] {
            let manifest = manifest_with(|value| {
                value["version"] = json!(version);
            });
            manifest.validate().unwrap_or_else(|err| {
                panic!("version {version} should be valid, got {err}");
            });
        }
    }

    #[test]
    fn invalid_version_formats_are_rejected() {
        for version in [
            "1.0",
            "1.0.0.0",
            "v1.0.0",
            "1..0",
            "a.b.c",
            "1.0.0-",
            "1.0.0-alpha-2",
            "1.0.0_alpha",
            "1.0.0 alpha",
        ] {
            let manifest = manifest_with(|value| {
                value["version"] = json!(version);
            });
            assert_eq!(
                manifest.validate(),
                Err(PluginValidationError::InvalidVersionFormat(
                    version.to_owned()
                )),
                "version {version} should be rejected"
            );
        }
    }

    #[test]
    fn empty_version_fails_validate() {
        let manifest = manifest_with(|value| {
            value["version"] = json!("");
        });
        assert_eq!(
            manifest.validate(),
            Err(PluginValidationError::MissingVersion)
        );
    }

    #[test]
    fn duplicate_permission_scopes_fail_validate() {
        let manifest = manifest_with(|value| {
            value["permissions"]["filesystem_read"] =
                json!(["/workspace/**", "/tmp", "/workspace/**"]);
        });
        assert_eq!(
            manifest.validate(),
            Err(PluginValidationError::DuplicatePermissionScope {
                domain: "filesystem_read".to_owned(),
                scope: "/workspace/**".to_owned(),
            })
        );
    }

    #[test]
    fn duplicate_network_scopes_fail_validate() {
        let manifest = manifest_with(|value| {
            value["permissions"]["network"] = json!(["api.example.com:443", "api.example.com:443"]);
        });
        assert_eq!(
            manifest.validate(),
            Err(PluginValidationError::DuplicatePermissionScope {
                domain: "network".to_owned(),
                scope: "api.example.com:443".to_owned(),
            })
        );
    }

    #[test]
    fn invalid_component_type_is_rejected_at_deserialization() {
        let mut value = valid_manifest_json();
        value["components"][0]["type"] = json!("hologram");
        let err = serde_json::from_value::<PluginManifestV1>(value).unwrap_err();
        assert!(err.to_string().contains("unknown variant"));
    }

    #[test]
    fn invalid_sandbox_network_policy_is_rejected_at_deserialization() {
        let mut value = valid_manifest_json();
        value["sandbox"]["network"] = json!("SOMETIMES");
        let err = serde_json::from_value::<PluginManifestV1>(value).unwrap_err();
        assert!(err.to_string().contains("unknown variant"));
    }

    #[test]
    fn marketplace_trait_is_object_safe() {
        fn assert_object_safe<T: PluginMarketplace + ?Sized>() {}
        assert_object_safe::<dyn PluginMarketplace>();
    }

    #[test]
    fn plugin_source_trait_is_object_safe() {
        fn assert_object_safe<T: PluginSource + ?Sized>() {}
        assert_object_safe::<dyn PluginSource>();
    }

    #[test]
    fn permissions_flatten_in_domain_order_with_booleans() {
        let manifest = manifest_with(|_| {});
        let permissions: Vec<PluginPermission> = Vec::from(&manifest);
        assert_eq!(
            permissions,
            vec![
                PluginPermission {
                    domain: "filesystem_read".to_owned(),
                    scope_value: "/workspace/**".to_owned(),
                },
                PluginPermission {
                    domain: "filesystem_read".to_owned(),
                    scope_value: "/tmp/**".to_owned(),
                },
                PluginPermission {
                    domain: "filesystem_write".to_owned(),
                    scope_value: "/workspace/**".to_owned(),
                },
                PluginPermission {
                    domain: "network".to_owned(),
                    scope_value: "api.example.com:443".to_owned(),
                },
                PluginPermission {
                    domain: "secrets".to_owned(),
                    scope_value: "GITHUB_TOKEN".to_owned(),
                },
                PluginPermission {
                    domain: "subprocess".to_owned(),
                    scope_value: "true".to_owned(),
                },
            ]
        );
    }

    #[test]
    fn permissions_dedupe_and_skip_false_booleans() {
        let manifest = manifest_with(|value| {
            value["permissions"]["filesystem_read"] = json!(["/a", "/a", "/b"]);
            value["permissions"]["subprocess"] = json!(false);
            value["permissions"]["admin"] = json!(true);
        });
        let permissions: Vec<PluginPermission> = Vec::from(&manifest);
        assert_eq!(
            permissions,
            vec![
                PluginPermission {
                    domain: "filesystem_read".to_owned(),
                    scope_value: "/a".to_owned(),
                },
                PluginPermission {
                    domain: "filesystem_read".to_owned(),
                    scope_value: "/b".to_owned(),
                },
                PluginPermission {
                    domain: "filesystem_write".to_owned(),
                    scope_value: "/workspace/**".to_owned(),
                },
                PluginPermission {
                    domain: "network".to_owned(),
                    scope_value: "api.example.com:443".to_owned(),
                },
                PluginPermission {
                    domain: "secrets".to_owned(),
                    scope_value: "GITHUB_TOKEN".to_owned(),
                },
                PluginPermission {
                    domain: "admin".to_owned(),
                    scope_value: "true".to_owned(),
                },
            ]
        );
    }

    #[test]
    fn permissions_skip_duplicate_boolean_domains() {
        let manifest = manifest_with(|value| {
            value["permissions"]["admin"] = json!(true);
        });
        let permissions: Vec<PluginPermission> = Vec::from(&manifest);
        let admin: Vec<_> = permissions
            .iter()
            .filter(|permission| permission.domain == "admin")
            .collect();
        assert_eq!(admin.len(), 1);
        assert_eq!(admin[0].scope_value, "true");
    }

    #[test]
    fn marketplace_dtos_round_trip() {
        let entry = MarketplaceEntry {
            id: "example/runner".to_owned(),
            name: "github-actions-runner".to_owned(),
            description: "Self-hosted runner".to_owned(),
            publisher: "Gobrowse Labs".to_owned(),
            latest_version: "1.2.3".to_owned(),
            download_count: 42,
            verified: false,
            categories: vec!["ci".to_owned(), "devops".to_owned()],
        };
        let decoded: MarketplaceEntry =
            serde_json::from_value(serde_json::to_value(&entry).unwrap()).unwrap();
        assert_eq!(entry, decoded);

        let info = VersionInfo {
            version: "1.2.3".to_owned(),
            published_at: OffsetDateTime::from_unix_timestamp(1_700_000_000).unwrap(),
            digest: "9a4bfda42f02c261506749e9b52f59d98fa4636c98c4fabbf561fd26ae1bdb2a".to_owned(),
            changelog: Some("fixes".to_owned()),
        };
        let decoded: VersionInfo =
            serde_json::from_value(serde_json::to_value(&info).unwrap()).unwrap();
        assert_eq!(info, decoded);

        let artifact = ArtifactLocation {
            url: Url::parse("https://example.com/runner-1.2.3.tar.gz").unwrap(),
            digest: "9a4bfda42f02c261506749e9b52f59d98fa4636c98c4fabbf561fd26ae1bdb2a".to_owned(),
            size_bytes: Some(4096),
            content_type: "application/gzip".to_owned(),
        };
        let decoded: ArtifactLocation =
            serde_json::from_value(serde_json::to_value(&artifact).unwrap()).unwrap();
        assert_eq!(artifact, decoded);
    }

    #[test]
    fn marketplace_error_is_clone_and_eq() {
        let error = MarketplaceError::RateLimited {
            retry_after_seconds: Some(60),
        };
        assert_eq!(error.clone(), error);
        assert_ne!(error, MarketplaceError::NotFound);
    }
}
