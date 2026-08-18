//! GitHub release plugin source and marketplace adapter (Lane C).
//!
//! [`GitHubReleaseSource`] implements [`PluginSource`] against the GitHub
//! REST API:
//!
//! * `resolve` — pins an immutable revision: the latest release (or the first
//!   tag when no release exists), or a requested `version` tag, resolved to a
//!   commit SHA via `GET /git/ref/tags/{tag}` (dereferencing annotated tags).
//!   The artifact digest is pinned at the same time (via the `.sha256`
//!   companion endpoint GitHub serves for archive URLs, or by downloading the
//!   artifact when no digest endpoint exists), so an install can never swap
//!   content after the operator previewed it.
//! * `fetch_manifest` — reads `gobrowse-plugin.json` from the resolved commit
//!   via raw.githubusercontent.com (falling back to a release asset whose name
//!   looks like a manifest).
//! * `download_artifact` — downloads the release asset (or the generated
//!   `archive/refs/tags/{tag}.tar.gz` when the release has no usable asset)
//!   and verifies the SHA-256 digest pinned during `resolve`.
//!
//! [`GitHubMarketplace`] implements [`PluginMarketplace`] with the same
//! endpoints: repository search as plugin search, release listing as versions,
//! and delegate resolution for `fetch_manifest`/`resolve_artifact`.
//!
//! # Rate limits
//!
//! The GitHub API is rate-limited per IP; the optional `GITHUB_TOKEN`
//! environment variable (a classic personal access token or fine-grained
//! token with public-repo read) is attached as a `Bearer` credential when
//! present. It is never required and never logged. Rate-limit responses
//! surface as [`PluginSourceError::RateLimited`] /
//! [`MarketplaceError::RateLimited`].
//!
//! # Test overrides
//!
//! Both adapters accept an explicit API base URL and raw-content base URL so
//! integration tests can point them at a local mock server (see
//! `FeatureSettings::github_api_base_url` / `github_raw_base_url`).

use std::path::{Path, PathBuf};

use async_trait::async_trait;
use gobrowse_core::plugin::{
    ArtifactLocation, MarketplaceEntry, MarketplaceError, PluginIdentity, PluginMarketplace,
    PluginSource, PluginSourceError, ResolvedPluginSource, VersionInfo,
};
use percent_encoding::{AsciiSet, NON_ALPHANUMERIC, utf8_percent_encode};
use reqwest::StatusCode;
use serde_json::Value;
use sha2::{Digest, Sha256};
use time::OffsetDateTime;
use url::Url;

use crate::config::FeatureSettings;

/// Production GitHub API base URL.
const PRODUCTION_API_BASE: &str = "https://api.github.com";
/// Production raw-content base URL.
const PRODUCTION_RAW_BASE: &str = "https://raw.githubusercontent.com";
/// Manifest filename fetched from the resolved commit.
const MANIFEST_FILENAME: &str = "gobrowse-plugin.json";
/// Upper bound for artifact downloads (zip/tar.gz bundles are small in
/// practice; the cap prevents unbounded disk/memory growth).
const MAX_ARTIFACT_BYTES: u64 = 512 * 1024 * 1024;
/// Repository search page size.
const SEARCH_PER_PAGE: usize = 10;
/// Release listing page size for `versions`.
const VERSIONS_PER_PAGE: usize = 20;

/// Percent-encode a URL path segment while keeping unreserved characters.
const PATH_SEGMENT: &AsciiSet = &NON_ALPHANUMERIC
    .remove(b'-')
    .remove(b'.')
    .remove(b'_')
    .remove(b'~');

fn encode_segment(segment: &str) -> String {
    utf8_percent_encode(segment, PATH_SEGMENT).to_string()
}

/// GitHub REST/raw endpoints shared by the source and marketplace adapters.
#[derive(Debug, Clone)]
pub(crate) struct GitHubApi {
    base_url: Url,
    raw_base_url: Url,
    http: reqwest::Client,
    token: Option<String>,
}

/// Internal error taxonomy for GitHub API calls; mapped onto
/// [`PluginSourceError`] / [`MarketplaceError`] by the adapters.
#[derive(Debug, thiserror::Error)]
pub(crate) enum GitHubError {
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
    #[error("artifact exceeds the size bound")]
    TooLarge,
}

impl GitHubError {
    pub(crate) fn into_source(self) -> PluginSourceError {
        match self {
            Self::NotFound => PluginSourceError::NotFound,
            Self::Unavailable => PluginSourceError::Unavailable,
            Self::RateLimited {
                retry_after_seconds,
            } => PluginSourceError::RateLimited {
                retry_after_seconds,
            },
            Self::InvalidResponse => PluginSourceError::InvalidResponse,
            Self::Timeout => PluginSourceError::Timeout,
            Self::TooLarge => {
                PluginSourceError::Download("artifact exceeds the 512 MiB download bound".into())
            }
        }
    }

    pub(crate) fn into_marketplace(self) -> MarketplaceError {
        match self {
            Self::NotFound => MarketplaceError::NotFound,
            Self::Unavailable => MarketplaceError::Unavailable,
            Self::RateLimited {
                retry_after_seconds,
            } => MarketplaceError::RateLimited {
                retry_after_seconds,
            },
            Self::InvalidResponse => MarketplaceError::InvalidResponse,
            Self::Timeout => MarketplaceError::Timeout,
            Self::TooLarge => MarketplaceError::InvalidResponse,
        }
    }
}

impl GitHubApi {
    /// Builds the shared client. `api_base_url` must end without a trailing
    /// slash; paths are appended verbatim.
    fn new(api_base_url: Url, raw_base_url: Url) -> Self {
        let token = std::env::var("GITHUB_TOKEN")
            .ok()
            .filter(|value| !value.is_empty());
        let http = reqwest::Client::builder()
            .connect_timeout(std::time::Duration::from_secs(5))
            .timeout(std::time::Duration::from_secs(20))
            .user_agent(concat!(
                "gobrowse-plugin-installer/",
                env!("CARGO_PKG_VERSION")
            ))
            .build()
            .expect("reqwest client builder with static options cannot fail");
        Self {
            base_url: api_base_url,
            raw_base_url,
            http,
            token,
        }
    }

    fn api_url(&self, path: &str) -> Url {
        let mut url = self.base_url.clone();
        // Merge the path onto the base URL (base has no path in production).
        let joined = format!("{}{}", self.base_url.path().trim_end_matches('/'), path);
        url.set_path(&joined);
        url
    }

    fn raw_url(&self, path: &str) -> Url {
        let mut url = self.raw_base_url.clone();
        let joined = format!("{}{}", self.raw_base_url.path().trim_end_matches('/'), path);
        url.set_path(&joined);
        url
    }

    async fn get_json(&self, path: &str) -> Result<Value, GitHubError> {
        let response = self
            .http
            .get(self.api_url(path))
            .header(reqwest::header::ACCEPT, "application/vnd.github+json")
            .header("X-GitHub-Api-Version", "2022-11-28")
            .headers(self.auth_headers())
            .send()
            .await
            .map_err(|error| {
                if error.is_timeout() {
                    GitHubError::Timeout
                } else if error.is_connect() {
                    GitHubError::Unavailable
                } else {
                    GitHubError::InvalidResponse
                }
            })?;
        classify_status(&response)?;
        response
            .json::<Value>()
            .await
            .map_err(|_| GitHubError::InvalidResponse)
    }

    /// GET an arbitrary URL (artifact download, `.sha256` companion, raw
    /// manifest) with a size bound. `max_bytes` of 0 means no bound.
    async fn get_bytes_from_url(&self, url: &Url, max_bytes: u64) -> Result<Vec<u8>, GitHubError> {
        let response = self
            .http
            .get(url.clone())
            .header(reqwest::header::ACCEPT, "application/octet-stream")
            .headers(self.auth_headers())
            .send()
            .await
            .map_err(|error| {
                if error.is_timeout() {
                    GitHubError::Timeout
                } else if error.is_connect() {
                    GitHubError::Unavailable
                } else {
                    GitHubError::InvalidResponse
                }
            })?;
        classify_status(&response)?;
        if max_bytes > 0 {
            let remaining = response.content_length().unwrap_or(0);
            if remaining > max_bytes {
                return Err(GitHubError::TooLarge);
            }
        }
        let mut bytes = Vec::new();
        let mut stream = response.bytes_stream();
        use futures_util::StreamExt;
        while let Some(chunk) = stream.next().await {
            let chunk = chunk.map_err(|_| GitHubError::InvalidResponse)?;
            if max_bytes > 0 && bytes.len() as u64 + chunk.len() as u64 > max_bytes {
                return Err(GitHubError::TooLarge);
            }
            bytes.extend_from_slice(&chunk);
        }
        Ok(bytes)
    }

    fn auth_headers(&self) -> reqwest::header::HeaderMap {
        let mut headers = reqwest::header::HeaderMap::new();
        if let (Some(token), Ok(value)) = (
            &self.token,
            reqwest::header::HeaderValue::from_str(
                &self
                    .token
                    .as_ref()
                    .map(|token| format!("Bearer {token}"))
                    .unwrap_or_default(),
            ),
        ) {
            let _ = token;
            headers.insert(reqwest::header::AUTHORIZATION, value);
        }
        headers
    }
}

fn classify_status(response: &reqwest::Response) -> Result<(), GitHubError> {
    match response.status() {
        StatusCode::OK | StatusCode::NO_CONTENT => Ok(()),
        StatusCode::NOT_FOUND => Err(GitHubError::NotFound),
        StatusCode::FORBIDDEN | StatusCode::TOO_MANY_REQUESTS => {
            let retry_after = response
                .headers()
                .get(reqwest::header::RETRY_AFTER)
                .and_then(|value| value.to_str().ok())
                .and_then(|value| value.parse::<u64>().ok());
            Err(GitHubError::RateLimited {
                retry_after_seconds: retry_after,
            })
        }
        status if status.is_server_error() => Err(GitHubError::Unavailable),
        _ => Err(GitHubError::InvalidResponse),
    }
}

/// Parse `owner/repo` from a plugin source URI. Accepts `owner/repo`,
/// `https://github.com/owner/repo`, optional trailing slash and `.git`
/// suffix. Returns [`PluginSourceError::InvalidResponse`] for malformed URIs
/// (the API layer turns that into a `Validation` error).
fn parse_owner_repo(source_uri: &str) -> Result<(String, String), PluginSourceError> {
    let trimmed = source_uri.trim().trim_end_matches('/');
    if trimmed.is_empty() || trimmed.len() > 2_048 {
        return Err(PluginSourceError::InvalidResponse);
    }
    let after_host = trimmed
        .split_once("github.com/")
        .map(|(_, rest)| rest)
        .unwrap_or(trimmed);
    let after_host = after_host.strip_suffix(".git").unwrap_or(after_host);
    let mut parts = after_host.split('/');
    let owner = parts.next().filter(|part| !part.is_empty());
    let repo = parts.next().filter(|part| !part.is_empty());
    let (Some(owner), Some(repo)) = (owner, repo) else {
        return Err(PluginSourceError::InvalidResponse);
    };
    if parts.next().is_some() || owner.contains([':', '?', '#']) || repo.contains([':', '?', '#']) {
        return Err(PluginSourceError::InvalidResponse);
    }
    Ok((owner.to_owned(), repo.to_owned()))
}

pub(crate) fn sha256_hex(bytes: &[u8]) -> String {
    let digest = Sha256::digest(bytes);
    let mut out = String::with_capacity(digest.len() * 2);
    for byte in digest {
        use std::fmt::Write;
        let _ = write!(out, "{byte:02x}");
    }
    out
}

/// A [`PluginSource`] backed by GitHub releases.
#[derive(Debug, Clone)]
pub struct GitHubReleaseSource {
    api: GitHubApi,
}

impl GitHubReleaseSource {
    /// Production adapter; honors the optional `GITHUB_TOKEN` env var.
    pub fn production() -> Self {
        Self::new(
            Url::parse(PRODUCTION_API_BASE).expect("static GitHub API URL is valid"),
            Url::parse(PRODUCTION_RAW_BASE).expect("static GitHub raw URL is valid"),
        )
    }

    /// Adapter against explicit base URLs (test mirrors / self-hosted
    /// proxies). `api_base_url` receives `/repos/...`-style paths;
    /// `raw_base_url` receives `/{owner}/{repo}/{sha}/{file}` manifest paths.
    pub fn new(api_base_url: Url, raw_base_url: Url) -> Self {
        Self {
            api: GitHubApi::new(api_base_url, raw_base_url),
        }
    }

    /// Builds the adapter from feature settings, honoring test overrides.
    pub fn from_settings(settings: &FeatureSettings) -> Self {
        match (&settings.github_api_base_url, &settings.github_raw_base_url) {
            (Some(api), Some(raw)) => Self::new(api.clone(), raw.clone()),
            _ => Self::production(),
        }
    }

    /// The release tag to use: the requested version, or the latest release's
    /// tag, or the first tag when no release exists.
    async fn resolve_tag(
        &self,
        owner: &str,
        repo: &str,
        version: Option<&str>,
    ) -> Result<String, GitHubError> {
        if let Some(version) = version {
            return Ok(version.to_owned());
        }
        let latest = self
            .api
            .get_json(&format!("/repos/{owner}/{repo}/releases/latest"))
            .await;
        match latest {
            Ok(release) => release
                .get("tag_name")
                .and_then(Value::as_str)
                .map(str::to_owned)
                .ok_or(GitHubError::InvalidResponse),
            Err(GitHubError::NotFound) => {
                let tags = self
                    .api
                    .get_json(&format!("/repos/{owner}/{repo}/tags"))
                    .await?;
                let list = tags.as_array().ok_or(GitHubError::InvalidResponse)?;
                list.first()
                    .and_then(|tag| tag.get("name"))
                    .and_then(Value::as_str)
                    .map(str::to_owned)
                    .ok_or(GitHubError::NotFound)
            }
            Err(other) => Err(other),
        }
    }

    /// Resolves a git ref (tag) to an immutable commit SHA, dereferencing
    /// annotated tag objects.
    async fn resolve_ref_to_commit(
        &self,
        owner: &str,
        repo: &str,
        tag: &str,
    ) -> Result<String, GitHubError> {
        let encoded_tag = encode_segment(tag);
        let reference = self
            .api
            .get_json(&format!("/repos/{owner}/{repo}/git/ref/tags/{encoded_tag}"))
            .await?;
        let object = reference
            .get("object")
            .ok_or(GitHubError::InvalidResponse)?;
        let object_type = object
            .get("type")
            .and_then(Value::as_str)
            .unwrap_or_default();
        let sha = object
            .get("sha")
            .and_then(Value::as_str)
            .ok_or(GitHubError::InvalidResponse)?;
        match object_type {
            "commit" => Ok(sha.to_owned()),
            "tag" => {
                let tag_object = self
                    .api
                    .get_json(&format!("/repos/{owner}/{repo}/git/tags/{sha}"))
                    .await?;
                let inner = tag_object
                    .get("object")
                    .ok_or(GitHubError::InvalidResponse)?;
                if inner.get("type").and_then(Value::as_str) != Some("commit") {
                    return Err(GitHubError::InvalidResponse);
                }
                inner
                    .get("sha")
                    .and_then(Value::as_str)
                    .map(str::to_owned)
                    .ok_or(GitHubError::InvalidResponse)
            }
            _ => Err(GitHubError::InvalidResponse),
        }
    }

    /// Release asset list for a tag (empty when the tag has no release).
    async fn release_assets(
        &self,
        owner: &str,
        repo: &str,
        tag: &str,
    ) -> Result<Vec<Value>, GitHubError> {
        let encoded_tag = encode_segment(tag);
        match self
            .api
            .get_json(&format!(
                "/repos/{owner}/{repo}/releases/tags/{encoded_tag}"
            ))
            .await
        {
            Ok(release) => Ok(release
                .get("assets")
                .and_then(Value::as_array)
                .cloned()
                .unwrap_or_default()),
            Err(GitHubError::NotFound) => Ok(Vec::new()),
            Err(other) => Err(other),
        }
    }

    /// Picks the artifact URL for a tag: the first release asset that looks
    /// like a package bundle (zip/tar.gz), preferring names mentioning the
    /// plugin; otherwise the GitHub-generated archive tarball.
    fn pick_artifact_url(
        &self,
        owner: &str,
        repo: &str,
        tag: &str,
        assets: &[Value],
    ) -> Result<Url, GitHubError> {
        let is_bundle = |name: &str| {
            name.ends_with(".zip") || name.ends_with(".tar.gz") || name.ends_with(".tgz")
        };
        let candidate = assets
            .iter()
            .filter_map(|asset| {
                let name = asset.get("name").and_then(Value::as_str)?;
                let url = asset.get("browser_download_url").and_then(Value::as_str)?;
                is_bundle(name).then_some((name, url))
            })
            .max_by_key(|(name, _)| {
                // Prefer names that clearly identify a plugin package.
                let mentions_plugin = ["gobrowse", "plugin", "package", "bundle"]
                    .iter()
                    .filter(|needle| name.to_ascii_lowercase().contains(**needle))
                    .count();
                (mentions_plugin, name.len())
            });
        if let Some((_, url)) = candidate {
            return Url::parse(url).map_err(|_| GitHubError::InvalidResponse);
        }
        let encoded_tag = encode_segment(tag);
        Url::parse(&format!(
            "https://github.com/{owner}/{repo}/archive/refs/tags/{encoded_tag}.tar.gz"
        ))
        .map_err(|_| GitHubError::InvalidResponse)
    }

    /// Computes the artifact digest for a resolved artifact URL: prefers the
    /// GitHub `.sha256` companion endpoint, falling back to downloading the
    /// artifact and hashing it.
    async fn compute_artifact_digest(&self, url: &Url) -> Result<String, GitHubError> {
        let mut digest_url = url.clone();
        digest_url.set_path(&format!("{}.sha256", url.path()));
        match self.api.get_bytes_from_url(&digest_url, 4 * 1024).await {
            Ok(bytes) => {
                let text = String::from_utf8_lossy(&bytes);
                if let Some(first) = text.split_whitespace().next()
                    && first.len() == 64
                    && first.bytes().all(|byte| byte.is_ascii_hexdigit())
                {
                    return Ok(first.to_ascii_lowercase());
                }
                Err(GitHubError::InvalidResponse)
            }
            Err(GitHubError::NotFound) => {
                let bytes = self.api.get_bytes_from_url(url, MAX_ARTIFACT_BYTES).await?;
                Ok(sha256_hex(&bytes))
            }
            Err(other) => Err(other),
        }
    }

    /// The artifact download location for a resolved source (used by
    /// `download_artifact` and the marketplace adapter).
    async fn artifact_location(
        &self,
        resolved: &ResolvedPluginSource,
    ) -> Result<ArtifactLocation, PluginSourceError> {
        let (owner, repo) = parse_owner_repo(&resolved.identity.source_uri)?;
        let tag = resolved
            .identity
            .version
            .as_deref()
            .ok_or(PluginSourceError::InvalidResponse)?;
        let assets = self
            .release_assets(&owner, &repo, tag)
            .await
            .map_err(GitHubError::into_source)?;
        let url = self
            .pick_artifact_url(&owner, &repo, tag, &assets)
            .map_err(GitHubError::into_source)?;
        let size_bytes = assets
            .iter()
            .find(|asset| {
                asset.get("browser_download_url").and_then(Value::as_str) == Some(url.as_str())
            })
            .and_then(|asset| asset.get("size"))
            .and_then(Value::as_u64);
        let content_type = assets
            .iter()
            .find(|asset| {
                asset.get("browser_download_url").and_then(Value::as_str) == Some(url.as_str())
            })
            .and_then(|asset| asset.get("content_type"))
            .and_then(Value::as_str)
            .unwrap_or("application/octet-stream")
            .to_owned();
        Ok(ArtifactLocation {
            url,
            digest: resolved.artifact_digest.clone(),
            size_bytes,
            content_type,
        })
    }
}

#[async_trait]
impl PluginSource for GitHubReleaseSource {
    async fn resolve(
        &self,
        identity: &PluginIdentity,
    ) -> Result<ResolvedPluginSource, PluginSourceError> {
        let (owner, repo) = parse_owner_repo(&identity.source_uri)?;
        let tag = self
            .resolve_tag(&owner, &repo, identity.version.as_deref())
            .await
            .map_err(GitHubError::into_source)?;
        let commit_sha = self
            .resolve_ref_to_commit(&owner, &repo, &tag)
            .await
            .map_err(GitHubError::into_source)?;
        let assets = self
            .release_assets(&owner, &repo, &tag)
            .await
            .map_err(GitHubError::into_source)?;
        let artifact_url = self
            .pick_artifact_url(&owner, &repo, &tag, &assets)
            .map_err(GitHubError::into_source)?;
        let artifact_digest = self
            .compute_artifact_digest(&artifact_url)
            .await
            .map_err(GitHubError::into_source)?;
        Ok(ResolvedPluginSource {
            identity: PluginIdentity {
                source_type: identity.source_type.clone(),
                source_uri: identity.source_uri.clone(),
                commit_sha: Some(commit_sha.clone()),
                version: Some(tag),
            },
            resolved_revision: commit_sha,
            manifest_path: PathBuf::from(MANIFEST_FILENAME),
            artifact_digest,
        })
    }

    async fn fetch_manifest(
        &self,
        resolved: &ResolvedPluginSource,
    ) -> Result<serde_json::Value, PluginSourceError> {
        let (owner, repo) = parse_owner_repo(&resolved.identity.source_uri)?;
        let commit_sha = resolved
            .identity
            .commit_sha
            .as_deref()
            .unwrap_or(&resolved.resolved_revision);
        let raw_path = format!(
            "/{owner}/{repo}/{commit_sha}/{}",
            encode_segment(MANIFEST_FILENAME)
        );
        let raw_bytes = match self
            .api
            .get_bytes_from_url(&self.api.raw_url(&raw_path), 2 * 1024 * 1024)
            .await
        {
            Ok(bytes) => bytes,
            Err(GitHubError::NotFound) => {
                // Fall back to a release asset that looks like a manifest.
                let tag = resolved
                    .identity
                    .version
                    .as_deref()
                    .ok_or(PluginSourceError::InvalidResponse)?;
                let assets = self
                    .release_assets(&owner, &repo, tag)
                    .await
                    .map_err(GitHubError::into_source)?;
                let manifest_asset = assets.iter().find_map(|asset| {
                    let name = asset.get("name").and_then(Value::as_str)?;
                    let url = asset.get("browser_download_url").and_then(Value::as_str)?;
                    (name.ends_with(".json")
                        && (name.contains("manifest") || name.contains("gobrowse-plugin")))
                    .then(|| url.to_owned())
                });
                let Some(url) = manifest_asset else {
                    return Err(PluginSourceError::ManifestInvalid(
                        "no gobrowse-plugin.json at the resolved commit and no manifest release asset".into(),
                    ));
                };
                let url = Url::parse(&url).map_err(|_| PluginSourceError::InvalidResponse)?;
                self.api
                    .get_bytes_from_url(&url, 2 * 1024 * 1024)
                    .await
                    .map_err(GitHubError::into_source)?
            }
            Err(other) => return Err(other.into_source()),
        };
        serde_json::from_slice(&raw_bytes).map_err(|error| {
            PluginSourceError::ManifestInvalid(format!(
                "gobrowse-plugin.json is not valid JSON: {error}"
            ))
        })
    }

    async fn download_artifact(
        &self,
        resolved: &ResolvedPluginSource,
        dest: &Path,
    ) -> Result<(), PluginSourceError> {
        let location = self.artifact_location(resolved).await?;
        let bytes = self
            .api
            .get_bytes_from_url(&location.url, MAX_ARTIFACT_BYTES)
            .await
            .map_err(GitHubError::into_source)?;
        let digest = sha256_hex(&bytes);
        if digest != resolved.artifact_digest {
            return Err(PluginSourceError::DigestMismatch);
        }
        if let Some(parent) = dest.parent() {
            tokio::fs::create_dir_all(parent)
                .await
                .map_err(PluginSourceError::Io)?;
        }
        tokio::fs::write(dest, bytes)
            .await
            .map_err(PluginSourceError::Io)?;
        Ok(())
    }
}

/// A [`PluginMarketplace`] backed by GitHub repository search and releases.
#[derive(Debug, Clone)]
pub struct GitHubMarketplace {
    source: GitHubReleaseSource,
}

impl GitHubMarketplace {
    /// Production adapter; honors the optional `GITHUB_TOKEN` env var.
    pub fn production() -> Self {
        Self {
            source: GitHubReleaseSource::production(),
        }
    }

    /// Adapter against explicit base URLs (test mirrors).
    pub fn new(api_base_url: Url, raw_base_url: Url) -> Self {
        Self {
            source: GitHubReleaseSource::new(api_base_url, raw_base_url),
        }
    }

    /// Builds the adapter from feature settings, honoring test overrides.
    pub fn from_settings(settings: &FeatureSettings) -> Self {
        match (&settings.github_api_base_url, &settings.github_raw_base_url) {
            (Some(api), Some(raw)) => Self::new(api.clone(), raw.clone()),
            _ => Self::production(),
        }
    }

    fn api(&self) -> &GitHubApi {
        &self.source.api
    }

    /// Splits a marketplace id (`owner/repo`) into its parts.
    fn parse_id(&self, id: &str) -> Result<(String, String), MarketplaceError> {
        parse_owner_repo(id).map_err(|_| MarketplaceError::InvalidResponse)
    }

    fn entry_from_repo(value: &Value) -> Option<MarketplaceEntry> {
        let full_name = value.get("full_name").and_then(Value::as_str)?;
        let name = value
            .get("name")
            .and_then(Value::as_str)
            .unwrap_or(full_name)
            .to_owned();
        let publisher = value
            .get("owner")
            .and_then(|owner| owner.get("login"))
            .and_then(Value::as_str)
            .unwrap_or_default()
            .to_owned();
        Some(MarketplaceEntry {
            id: full_name.to_owned(),
            name,
            description: value
                .get("description")
                .and_then(Value::as_str)
                .unwrap_or_default()
                .to_owned(),
            publisher,
            latest_version: "latest".to_owned(),
            download_count: value
                .get("stargazers_count")
                .and_then(Value::as_u64)
                .unwrap_or(0),
            verified: false,
            categories: value
                .get("topics")
                .and_then(Value::as_array)
                .map(|topics| {
                    topics
                        .iter()
                        .filter_map(Value::as_str)
                        .map(str::to_owned)
                        .collect()
                })
                .unwrap_or_default(),
        })
    }
}

#[async_trait]
impl PluginMarketplace for GitHubMarketplace {
    async fn search(&self, query: &str) -> Result<Vec<MarketplaceEntry>, MarketplaceError> {
        let encoded_query: String =
            url::form_urlencoded::byte_serialize(query.as_bytes()).collect();
        let value = self
            .api()
            .get_json(&format!(
                "/search/repositories?q={encoded_query}&per_page={SEARCH_PER_PAGE}"
            ))
            .await
            .map_err(GitHubError::into_marketplace)?;
        let items = value
            .get("items")
            .and_then(Value::as_array)
            .ok_or(MarketplaceError::InvalidResponse)?;
        Ok(items.iter().filter_map(Self::entry_from_repo).collect())
    }

    async fn inspect(&self, id: &str) -> Result<MarketplaceEntry, MarketplaceError> {
        let (owner, repo) = self.parse_id(id)?;
        let value = self
            .api()
            .get_json(&format!("/repos/{owner}/{repo}"))
            .await
            .map_err(GitHubError::into_marketplace)?;
        Self::entry_from_repo(&value).ok_or(MarketplaceError::InvalidResponse)
    }

    async fn versions(&self, id: &str) -> Result<Vec<VersionInfo>, MarketplaceError> {
        let (owner, repo) = self.parse_id(id)?;
        let value = self
            .api()
            .get_json(&format!(
                "/repos/{owner}/{repo}/releases?per_page={VERSIONS_PER_PAGE}"
            ))
            .await
            .map_err(GitHubError::into_marketplace)?;
        let releases = value.as_array().ok_or(MarketplaceError::InvalidResponse)?;
        let mut versions = Vec::with_capacity(releases.len());
        for release in releases.iter().take(VERSIONS_PER_PAGE) {
            let tag = release
                .get("tag_name")
                .and_then(Value::as_str)
                .ok_or(MarketplaceError::InvalidResponse)?;
            let published_at = release
                .get("published_at")
                .and_then(Value::as_str)
                .and_then(|text| {
                    OffsetDateTime::parse(text, &time::format_description::well_known::Rfc3339).ok()
                })
                .unwrap_or_else(OffsetDateTime::now_utc);
            // Resolve the tag to its commit SHA as an immutable revision id.
            let digest = self
                .source
                .resolve_ref_to_commit(&owner, &repo, tag)
                .await
                .map_err(GitHubError::into_marketplace)?;
            versions.push(VersionInfo {
                version: tag.to_owned(),
                published_at,
                digest,
                changelog: release
                    .get("body")
                    .and_then(Value::as_str)
                    .map(str::to_owned),
            });
        }
        Ok(versions)
    }

    async fn fetch_manifest(
        &self,
        id: &str,
        version: &str,
    ) -> Result<serde_json::Value, MarketplaceError> {
        let identity = PluginIdentity {
            source_type: "github_release".into(),
            source_uri: id.to_owned(),
            commit_sha: None,
            version: Some(version.to_owned()),
        };
        let resolved = self
            .source
            .resolve(&identity)
            .await
            .map_err(|_| MarketplaceError::Unavailable)?;
        self.source
            .fetch_manifest(&resolved)
            .await
            .map_err(|_| MarketplaceError::InvalidResponse)
    }

    async fn resolve_artifact(
        &self,
        id: &str,
        version: &str,
    ) -> Result<ArtifactLocation, MarketplaceError> {
        let identity = PluginIdentity {
            source_type: "github_release".into(),
            source_uri: id.to_owned(),
            commit_sha: None,
            version: Some(version.to_owned()),
        };
        let resolved = self
            .source
            .resolve(&identity)
            .await
            .map_err(|_| MarketplaceError::Unavailable)?;
        self.source
            .artifact_location(&resolved)
            .await
            .map_err(|_| MarketplaceError::InvalidResponse)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parse_owner_repo_accepts_uri_shapes() {
        for (uri, expected) in [
            ("owner/repo", ("owner", "repo")),
            ("https://github.com/owner/repo", ("owner", "repo")),
            ("https://github.com/owner/repo/", ("owner", "repo")),
            ("owner/repo.git", ("owner", "repo")),
            ("https://github.com/owner/repo.git", ("owner", "repo")),
        ] {
            assert_eq!(
                parse_owner_repo(uri).unwrap(),
                (expected.0.to_owned(), expected.1.to_owned()),
                "uri: {uri}"
            );
        }
    }

    #[test]
    fn parse_owner_repo_rejects_malformed_uris() {
        for uri in [
            "",
            "owner",
            "https://github.com/",
            "a/b/c",
            "https://example.com/a/b",
            "a:b",
        ] {
            assert!(parse_owner_repo(uri).is_err(), "uri: {uri}");
        }
    }

    #[test]
    fn sha256_hex_matches_known_digest() {
        assert_eq!(
            sha256_hex(b"hello world"),
            "b94d27b9934d3e08a52e52d7da7dabfac484efe37a5380ee9088f7ace2efcde9"
        );
    }

    #[test]
    fn from_settings_uses_overrides_when_both_present() {
        let settings = FeatureSettings {
            github_api_base_url: Some(Url::parse("http://mock/api").unwrap()),
            github_raw_base_url: Some(Url::parse("http://mock/raw").unwrap()),
            ..FeatureSettings::default()
        };
        let source = GitHubReleaseSource::from_settings(&settings);
        assert_eq!(source.api.base_url.as_str(), "http://mock/api");
        assert_eq!(source.api.raw_base_url.as_str(), "http://mock/raw");
    }
}
