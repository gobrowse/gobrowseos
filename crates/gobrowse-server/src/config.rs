use std::{net::SocketAddr, path::PathBuf, time::Duration};

use secrecy::SecretString;
use serde::Deserialize;
use thiserror::Error;
use url::Url;

#[derive(Deserialize)]
pub struct Settings {
    #[serde(default)]
    pub http: HttpSettings,
    pub database: DatabaseSettings,
    #[serde(default)]
    pub auth: AuthSettings,
    #[serde(default)]
    pub vault: VaultSettings,
    #[serde(default)]
    pub features: FeatureSettings,
    #[serde(default)]
    pub observability: ObservabilitySettings,
}

#[derive(Clone, Deserialize)]
pub struct VaultSettings {
    pub master_key_file: Option<PathBuf>,
    pub master_key_base64: Option<SecretString>,
    #[serde(default = "default_key_version")]
    pub key_version: i32,
    pub previous_master_key_file: Option<PathBuf>,
    pub previous_master_key_base64: Option<SecretString>,
    pub previous_key_version: Option<i32>,
}

impl Default for VaultSettings {
    fn default() -> Self {
        Self {
            master_key_file: None,
            master_key_base64: None,
            key_version: default_key_version(),
            previous_master_key_file: None,
            previous_master_key_base64: None,
            previous_key_version: None,
        }
    }
}

const fn default_key_version() -> i32 {
    1
}

#[derive(Debug, Clone, Deserialize)]
pub struct HttpSettings {
    pub bind: SocketAddr,
    pub public_origin: Url,
    pub secure_cookies: bool,
    pub static_dir: PathBuf,
    pub request_body_limit_bytes: usize,
}

impl Default for HttpSettings {
    fn default() -> Self {
        Self {
            bind: ([0, 0, 0, 0], 8080).into(),
            public_origin: Url::parse("http://localhost:8080").expect("default URL is valid"),
            secure_cookies: false,
            static_dir: PathBuf::from("dist"),
            request_body_limit_bytes: 2 * 1024 * 1024,
        }
    }
}

#[derive(Deserialize)]
pub struct DatabaseSettings {
    pub url: SecretString,
    #[serde(default = "default_max_connections")]
    pub max_connections: u32,
}

const fn default_max_connections() -> u32 {
    20
}

#[derive(Debug, Clone, Deserialize)]
pub struct AuthSettings {
    pub session_idle_minutes: i64,
    pub session_absolute_hours: i64,
    pub argon2_memory_kib: u32,
    pub argon2_iterations: u32,
    pub argon2_parallelism: u32,
    pub max_parallel_hashes: usize,
    #[serde(default = "default_login_throttle_max_attempts")]
    pub login_throttle_max_attempts: u32,
    #[serde(default = "default_login_throttle_window_secs")]
    pub login_throttle_window_secs: i64,
}

const fn default_login_throttle_max_attempts() -> u32 {
    5
}
const fn default_login_throttle_window_secs() -> i64 {
    300
}

impl Default for AuthSettings {
    fn default() -> Self {
        Self {
            session_idle_minutes: 120,
            session_absolute_hours: 24 * 14,
            argon2_memory_kib: 19_456,
            argon2_iterations: 2,
            argon2_parallelism: 1,
            max_parallel_hashes: 4,
            login_throttle_max_attempts: default_login_throttle_max_attempts(),
            login_throttle_window_secs: default_login_throttle_window_secs(),
        }
    }
}

#[derive(Debug, Clone, Deserialize)]
pub struct FeatureSettings {
    pub sandbox: bool,
    pub browser: bool,
    pub messaging: bool,
    pub voice: bool,
    pub media: bool,
    pub lsp: bool,
    pub otel: bool,
    pub local_embeddings: bool,
    #[serde(default)]
    pub local_models: bool,
    #[serde(default)]
    pub webhook_scheduler_enabled: bool,
    #[serde(default = "default_webhook_scheduler_max_attempts")]
    pub webhook_scheduler_max_attempts: u32,
    /// Unix socket path of the sandboxd instance to talk to. When set together
    /// with `sandbox_auth_token` while `sandbox` is enabled, the server wires a
    /// `SandboxClient` into `AppState`.
    #[serde(default)]
    pub sandbox_socket_path: Option<PathBuf>,
    /// Shared-secret token that must match sandboxd's `--auth-token-file`.
    #[serde(default)]
    pub sandbox_auth_token: Option<SecretString>,
    /// Per-operation timeout for sandbox socket I/O.
    #[serde(default = "default_sandbox_socket_timeout_seconds")]
    pub sandbox_socket_timeout_seconds: u64,
    /// Default sandbox image for plugin execution (informational for Lane D;
    /// consumed by plugin install flows).
    #[serde(default)]
    pub sandbox_plugin_image: Option<String>,
}

const fn default_sandbox_socket_timeout_seconds() -> u64 {
    30
}

impl Default for FeatureSettings {
    fn default() -> Self {
        Self {
            sandbox: false,
            browser: false,
            messaging: false,
            voice: false,
            media: false,
            lsp: false,
            otel: false,
            local_embeddings: false,
            local_models: false,
            webhook_scheduler_enabled: false,
            webhook_scheduler_max_attempts: default_webhook_scheduler_max_attempts(),
            sandbox_socket_path: None,
            sandbox_auth_token: None,
            sandbox_socket_timeout_seconds: default_sandbox_socket_timeout_seconds(),
            sandbox_plugin_image: None,
        }
    }
}

const fn default_webhook_scheduler_max_attempts() -> u32 {
    5
}

#[derive(Debug, Clone, Deserialize)]
pub struct ObservabilitySettings {
    pub json_logs: bool,
    pub log_filter: String,
}

impl Default for ObservabilitySettings {
    fn default() -> Self {
        Self {
            json_logs: false,
            log_filter: "gobrowse=info,tower_http=info".into(),
        }
    }
}

#[derive(Debug, Error)]
pub enum SettingsError {
    #[error("configuration could not be loaded: {0}")]
    Load(#[from] config::ConfigError),
    #[error("public_origin must use http or https")]
    InvalidPublicOrigin,
    #[error("secure cookies are required when public_origin uses https")]
    InsecureCookieConfiguration,
    #[error("session and Argon2 settings must be positive")]
    InvalidSecuritySetting,
    #[error("configure at most one vault master-key source and use a positive key version")]
    InvalidVaultSetting,
}

impl Settings {
    pub fn load(path: Option<&std::path::Path>) -> Result<Self, SettingsError> {
        let mut builder = config::Config::builder();
        if let Some(path) = path {
            builder = builder.add_source(config::File::from(path).required(true));
        } else {
            builder = builder.add_source(config::File::with_name("config.toml").required(false));
        }
        let settings: Self = builder
            .add_source(
                config::Environment::with_prefix("GOBROWSE")
                    .separator("__")
                    .try_parsing(true),
            )
            .build()?
            .try_deserialize()?;
        settings.validate()?;
        Ok(settings)
    }

    fn validate(&self) -> Result<(), SettingsError> {
        if !matches!(self.http.public_origin.scheme(), "http" | "https") {
            return Err(SettingsError::InvalidPublicOrigin);
        }
        if self.http.public_origin.scheme() == "https" && !self.http.secure_cookies {
            return Err(SettingsError::InsecureCookieConfiguration);
        }
        if self.auth.session_idle_minutes <= 0
            || self.auth.session_absolute_hours <= 0
            || self.auth.argon2_memory_kib == 0
            || self.auth.argon2_memory_kib < 8192
            || self.auth.argon2_memory_kib > 4_194_304
            || self.auth.argon2_iterations == 0
            || self.auth.argon2_iterations < 2
            || self.auth.argon2_parallelism == 0
            || self.auth.argon2_parallelism < 1
            || self.auth.max_parallel_hashes == 0
        {
            return Err(SettingsError::InvalidSecuritySetting);
        }
        let current_sources = usize::from(self.vault.master_key_file.is_some())
            + usize::from(self.vault.master_key_base64.is_some());
        let previous_sources = usize::from(self.vault.previous_master_key_file.is_some())
            + usize::from(self.vault.previous_master_key_base64.is_some());
        if self.vault.key_version <= 0
            || current_sources > 1
            || previous_sources > 1
            || (previous_sources == 1) != self.vault.previous_key_version.is_some()
            || (previous_sources == 1 && current_sources != 1)
            || self.vault.previous_key_version == Some(self.vault.key_version)
            || self
                .vault
                .previous_key_version
                .is_some_and(|version| version <= 0)
        {
            return Err(SettingsError::InvalidVaultSetting);
        }
        Ok(())
    }

    pub fn shutdown_timeout() -> Duration {
        Duration::from_secs(20)
    }
}

#[cfg(test)]
mod tests {
    use secrecy::ExposeSecret;

    use super::*;

    /// Build a `Settings` with all required fields set to sane defaults
    /// so that only the caller's deliberate mutation triggers validation
    /// rejections.
    fn base_test_settings() -> Settings {
        Settings {
            http: HttpSettings::default(),
            database: DatabaseSettings {
                url: "postgres://test/placeholder".into(),
                max_connections: 2,
            },
            auth: AuthSettings::default(),
            vault: VaultSettings::default(),
            features: FeatureSettings::default(),
            observability: ObservabilitySettings::default(),
        }
    }

    #[test]
    fn vault_defaults_to_first_key_version() {
        assert_eq!(VaultSettings::default().key_version, 1);
    }

    #[test]
    fn milestone2_feature_config_defaults_local_models_off() {
        let features: FeatureSettings = serde_json::from_value(serde_json::json!({
            "sandbox":false,"browser":false,"messaging":false,"voice":false,
            "media":false,"lsp":false,"otel":false,"local_embeddings":true
        }))
        .expect("deserialize Milestone 2 feature settings");
        assert!(!features.local_models);
    }

    #[test]
    fn sandbox_feature_settings_default_to_unconfigured_with_thirty_second_timeout() {
        let features: FeatureSettings = serde_json::from_value(serde_json::json!({
            "sandbox": true, "browser": false, "messaging": false, "voice": false,
            "media": false, "lsp": false, "otel": false, "local_embeddings": false
        }))
        .expect("sandbox-only feature settings deserialize");
        assert!(features.sandbox);
        assert!(features.sandbox_socket_path.is_none());
        assert!(features.sandbox_auth_token.is_none());
        assert_eq!(features.sandbox_socket_timeout_seconds, 30);
        assert!(features.sandbox_plugin_image.is_none());
        assert_eq!(
            FeatureSettings::default().sandbox_socket_timeout_seconds,
            30
        );
    }

    #[test]
    fn sandbox_feature_settings_deserialize_socket_and_token() {
        let features: FeatureSettings = serde_json::from_value(serde_json::json!({
            "sandbox": true, "browser": false, "messaging": false, "voice": false,
            "media": false, "lsp": false, "otel": false, "local_embeddings": false,
            "sandbox_socket_path": "/tmp/gbsbx/sandboxd.sock",
            "sandbox_auth_token": "opaque-token",
            "sandbox_socket_timeout_seconds": 7,
            "sandbox_plugin_image": "localhost/gobrowse-workspace:v1"
        }))
        .expect("full sandbox feature settings deserialize");
        assert_eq!(
            features.sandbox_socket_path,
            Some(PathBuf::from("/tmp/gbsbx/sandboxd.sock"))
        );
        assert_eq!(
            features.sandbox_auth_token.unwrap().expose_secret(),
            "opaque-token"
        );
        assert_eq!(features.sandbox_socket_timeout_seconds, 7);
        assert_eq!(
            features.sandbox_plugin_image.as_deref(),
            Some("localhost/gobrowse-workspace:v1")
        );
    }

    #[test]
    fn config_rejects_argon2_memory_kib_below_floor() {
        let mut settings = base_test_settings();
        settings.auth.argon2_memory_kib = 4096;
        assert!(matches!(
            settings.validate(),
            Err(SettingsError::InvalidSecuritySetting)
        ));
    }

    #[test]
    fn config_rejects_argon2_iterations_below_floor() {
        let mut settings = base_test_settings();
        settings.auth.argon2_iterations = 1;
        assert!(matches!(
            settings.validate(),
            Err(SettingsError::InvalidSecuritySetting)
        ));
    }

    #[test]
    fn config_rejects_argon2_memory_kib_above_ceiling() {
        let mut settings = base_test_settings();
        settings.auth.argon2_memory_kib = 8_388_608;
        assert!(matches!(
            settings.validate(),
            Err(SettingsError::InvalidSecuritySetting)
        ));
    }

    #[test]
    fn config_accepts_baseline_argon2_params() {
        let settings = base_test_settings();
        assert!(settings.validate().is_ok());
    }
}
