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
    pub features: FeatureSettings,
    #[serde(default)]
    pub observability: ObservabilitySettings,
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
        }
    }
}

#[derive(Debug, Clone, Default, Deserialize)]
pub struct FeatureSettings {
    pub sandbox: bool,
    pub browser: bool,
    pub messaging: bool,
    pub voice: bool,
    pub media: bool,
    pub lsp: bool,
    pub otel: bool,
    pub local_embeddings: bool,
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
            || self.auth.argon2_iterations == 0
            || self.auth.argon2_parallelism == 0
            || self.auth.max_parallel_hashes == 0
        {
            return Err(SettingsError::InvalidSecuritySetting);
        }
        Ok(())
    }

    pub fn shutdown_timeout() -> Duration {
        Duration::from_secs(20)
    }
}
