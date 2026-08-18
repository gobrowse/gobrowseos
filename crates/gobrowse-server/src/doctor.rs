use std::{path::Path, process::Stdio, time::Duration, time::Instant};

use serde::Serialize;
use sqlx::{PgPool, Row};
use tokio::process::Command;

use crate::{
    config::Settings,
    sandbox_client::{SandboxClient, SandboxConfig},
    vault,
};

const VAULT_FAILURE_DETAIL: &str = "configured vault key material is invalid or unavailable";
const MCP_METADATA_QUERY_FAILURE_DETAIL: &str = "MCP OAuth metadata is unavailable";
pub(crate) const MCP_METADATA_CAP: usize = 1_000;

#[derive(Debug, Clone, Copy, Serialize)]
#[serde(rename_all = "UPPERCASE")]
pub enum Status {
    Pass,
    Warn,
    Fail,
}

#[derive(Debug, Serialize)]
pub struct Check {
    pub name: &'static str,
    pub status: Status,
    pub detail: String,
    pub latency_ms: Option<u128>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct McpCredentialMetadata {
    pub purpose: String,
    pub allowed_hosts: Vec<String>,
    pub backend: String,
    pub key_version: Option<i32>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct McpMetadataCounts {
    pub total: usize,
    pub ready: usize,
    pub invalid: usize,
}

pub(crate) fn classify_mcp_metadata(
    rows: &[McpCredentialMetadata],
    vault_available: bool,
    current_key_version: i32,
    previous_key_version: Option<i32>,
) -> McpMetadataCounts {
    let mut counts = McpMetadataCounts {
        total: rows.len(),
        ready: 0,
        invalid: 0,
    };
    for row in rows {
        let version_ready = row.key_version.is_some_and(|version| {
            version == current_key_version || Some(version) == previous_key_version
        });
        let ready = vault_available
            && row.backend == "encrypted_database"
            && version_ready
            && vault::validate_secret_metadata(&row.purpose, &row.allowed_hosts).is_ok();
        if ready {
            counts.ready += 1;
        } else {
            counts.invalid += 1;
        }
    }
    counts
}

fn metadata_status(counts: McpMetadataCounts) -> Status {
    if counts.total == 0 {
        Status::Warn
    } else if counts.invalid == 0 {
        Status::Pass
    } else {
        Status::Fail
    }
}
pub async fn run(settings: &Settings, pool: Option<&PgPool>) -> Vec<Check> {
    let vault_result = vault::Vault::from_settings(&settings.vault).await;
    let current_sources = usize::from(settings.vault.master_key_file.is_some())
        + usize::from(settings.vault.master_key_base64.is_some());
    let previous_sources = usize::from(settings.vault.previous_master_key_file.is_some())
        + usize::from(settings.vault.previous_master_key_base64.is_some());
    let key_versions_valid = settings.vault.key_version > 0
        && current_sources <= 1
        && previous_sources <= 1
        && (previous_sources == 1) == settings.vault.previous_key_version.is_some()
        && (previous_sources == 0 || current_sources == 1)
        && settings
            .vault
            .previous_key_version
            .is_none_or(|version| version > 0 && version != settings.vault.key_version);
    let vault_available =
        key_versions_valid && vault_result.as_ref().is_ok_and(vault::Vault::is_available);
    let mut checks = Vec::new();
    if let Some(pool) = pool {
        let started = Instant::now();
        let db = sqlx::query_scalar::<_, i32>("SELECT 1")
            .fetch_one(pool)
            .await;
        checks.push(Check {
            name: "PostgreSQL",
            status: if db.is_ok() {
                Status::Pass
            } else {
                Status::Fail
            },
            detail: db.map_or_else(
                |error| format!("connection failed: {error}"),
                |_| "connected".into(),
            ),
            latency_ms: Some(started.elapsed().as_millis()),
        });
        let vector = sqlx::query_scalar::<_, bool>(
            "SELECT EXISTS(SELECT 1 FROM pg_extension WHERE extname = 'vector')",
        )
        .fetch_one(pool)
        .await;
        checks.push(Check {
            name: "pgvector",
            status: if matches!(vector, Ok(true)) {
                Status::Pass
            } else {
                Status::Fail
            },
            detail: vector.map_or_else(
                |error| format!("check failed: {error}"),
                |enabled| {
                    if enabled {
                        "extension enabled".into()
                    } else {
                        "extension missing".into()
                    }
                },
            ),
            latency_ms: None,
        });
        let queue = sqlx::query(
            "SELECT count(*) FILTER (WHERE status IN ('queued','retry','running')) AS active, \
             count(*) FILTER (WHERE status='failed') AS failed, \
             count(*) FILTER (WHERE status='running' AND lease_expires_at<now()) AS expired \
             FROM embedding_jobs",
        )
        .fetch_one(pool)
        .await;
        checks.push(Check {
            name: "Embedding queue",
            status: match &queue {
                Ok(row) if sqlx::Row::get::<i64, _>(row, "expired") == 0 => Status::Pass,
                Ok(_) => Status::Warn,
                Err(_) => Status::Fail,
            },
            detail: queue.map_or_else(
                |error| format!("check failed: {error}"),
                |row| {
                    format!(
                        "active={}, failed={}, expired_leases={}",
                        sqlx::Row::get::<i64, _>(&row, "active"),
                        sqlx::Row::get::<i64, _>(&row, "failed"),
                        sqlx::Row::get::<i64, _>(&row, "expired")
                    )
                },
            ),
            latency_ms: None,
        });
    } else {
        checks.push(Check {
            name: "PostgreSQL",
            status: Status::Fail,
            detail: "not connected".into(),
            latency_ms: None,
        });
    }
    checks.push(command_check("Git", "git", &["--version"]).await);
    checks.push(Check {
        name: "Static assets",
        status: if Path::new(&settings.http.static_dir).exists() {
            Status::Pass
        } else {
            Status::Warn
        },
        detail: settings.http.static_dir.display().to_string(),
        latency_ms: None,
    });
    checks.push(Check {
        name: "Credential vault",
        status: if !key_versions_valid {
            Status::Fail
        } else {
            match &vault_result {
                Ok(vault) if vault.is_available() => Status::Pass,
                Ok(_) => Status::Warn,
                Err(_) => Status::Fail,
            }
        },
        detail: if !key_versions_valid {
            VAULT_FAILURE_DETAIL.into()
        } else {
            match &vault_result {
                Ok(vault) if vault.is_available() => "configured key material validated".into(),
                Ok(_) => "not configured; authenticated providers remain unavailable".into(),
                Err(_) => VAULT_FAILURE_DETAIL.into(),
            }
        },
        latency_ms: None,
    });
    checks.push(
        mcp_metadata_check(
            pool,
            vault_available,
            settings.vault.key_version,
            settings.vault.previous_key_version,
        )
        .await,
    );
    let sandbox_start = Instant::now();
    let sandbox_check = async {
        if !settings.features.sandbox {
            return Check {
                name: "Sandbox",
                status: Status::Pass,
                detail: "disabled by feature policy".into(),
                latency_ms: None,
            };
        }
        let (Some(socket_path), Some(auth_token)) = (
            &settings.features.sandbox_socket_path,
            &settings.features.sandbox_auth_token,
        ) else {
            return Check {
                name: "Sandbox",
                status: Status::Warn,
                detail: "enabled; sandboxd connectivity is validated when configured".into(),
                latency_ms: None,
            };
        };
        match SandboxClient::connect(SandboxConfig {
            socket_path: socket_path.clone(),
            auth_token: auth_token.clone(),
            timeout: Duration::from_secs(settings.features.sandbox_socket_timeout_seconds),
        })
        .await
        {
            Ok(client) => match client.health().await {
                Ok(status) => Check {
                    name: "Sandbox",
                    status: Status::Pass,
                    detail: format!("sandboxd health check returned {status:?}"),
                    latency_ms: Some(sandbox_start.elapsed().as_millis()),
                },
                Err(error) => Check {
                    name: "Sandbox",
                    status: Status::Fail,
                    detail: format!("sandboxd unreachable: {error}"),
                    latency_ms: Some(sandbox_start.elapsed().as_millis()),
                },
            },
            Err(error) => Check {
                name: "Sandbox",
                status: Status::Fail,
                detail: format!("sandbox configuration invalid: {error}"),
                latency_ms: None,
            },
        }
    }
    .await;
    checks.push(sandbox_check);
    checks
}

async fn mcp_metadata_check(
    pool: Option<&PgPool>,
    vault_available: bool,
    current_key_version: i32,
    previous_key_version: Option<i32>,
) -> Check {
    let Some(pool) = pool else {
        return Check {
            name: "MCP OAuth vault metadata",
            status: Status::Fail,
            detail: MCP_METADATA_QUERY_FAILURE_DETAIL.into(),
            latency_ms: None,
        };
    };
    let rows = sqlx::query(
        "SELECT purpose,allowed_hosts,backend,key_version \
         FROM secret_references WHERE left(purpose,4)='mcp_' \
         LIMIT 1001",
    )
    .fetch_all(pool)
    .await;
    let rows = match rows {
        Ok(rows) => rows
            .into_iter()
            .map(|row| McpCredentialMetadata {
                purpose: row.get("purpose"),
                allowed_hosts: row.get("allowed_hosts"),
                backend: row.get("backend"),
                key_version: row.get("key_version"),
            })
            .collect::<Vec<_>>(),
        Err(_) => {
            return Check {
                name: "MCP OAuth vault metadata",
                status: Status::Fail,
                detail: MCP_METADATA_QUERY_FAILURE_DETAIL.into(),
                latency_ms: None,
            };
        }
    };
    if rows.len() > MCP_METADATA_CAP {
        return Check {
            name: "MCP OAuth vault metadata",
            status: Status::Fail,
            detail: format!(
                "credentials>{}, ready=0, invalid>{}",
                MCP_METADATA_CAP, MCP_METADATA_CAP
            ),
            latency_ms: None,
        };
    }
    let counts = classify_mcp_metadata(
        &rows,
        vault_available,
        current_key_version,
        previous_key_version,
    );
    let status = metadata_status(counts);
    Check {
        name: "MCP OAuth vault metadata",
        status,
        detail: format!(
            "credentials={}, ready={}, invalid={}",
            counts.total, counts.ready, counts.invalid
        ),
        latency_ms: None,
    }
}

async fn command_check(name: &'static str, command: &str, args: &[&str]) -> Check {
    let result = Command::new(command)
        .args(args)
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .output()
        .await;
    match result {
        Ok(output) if output.status.success() => Check {
            name,
            status: Status::Pass,
            detail: String::from_utf8_lossy(&output.stdout).trim().into(),
            latency_ms: None,
        },
        Ok(_) => Check {
            name,
            status: Status::Fail,
            detail: "command failed".into(),
            latency_ms: None,
        },
        Err(error) => Check {
            name,
            status: Status::Fail,
            detail: error.to_string(),
            latency_ms: None,
        },
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::{
        AuthSettings, DatabaseSettings, FeatureSettings, HttpSettings, ObservabilitySettings,
        VaultSettings,
    };
    use base64::Engine as _;
    use secrecy::SecretString;

    fn metadata(
        purpose: &str,
        host: &str,
        backend: &str,
        key_version: i32,
    ) -> McpCredentialMetadata {
        McpCredentialMetadata {
            purpose: purpose.into(),
            allowed_hosts: vec![host.into()],
            backend: backend.into(),
            key_version: Some(key_version),
        }
    }

    fn base_settings() -> Settings {
        Settings {
            http: HttpSettings::default(),
            database: DatabaseSettings {
                url: "postgres://invalid".into(),
                max_connections: 1,
            },
            auth: AuthSettings::default(),
            vault: VaultSettings::default(),
            features: FeatureSettings::default(),
            observability: ObservabilitySettings::default(),
        }
    }

    #[test]
    fn classifier_reports_only_aggregate_counts_and_accepts_current_previous_versions() {
        let rows = vec![
            metadata(
                vault::MCP_OAUTH_ACCESS_TOKEN,
                "example.com",
                "encrypted_database",
                7,
            ),
            metadata(
                vault::MCP_OAUTH_REFRESH_TOKEN,
                "example.com",
                "encrypted_database",
                6,
            ),
            metadata("mcp_unknown", "example.com", "encrypted_database", 7),
            metadata(
                vault::MCP_OAUTH_CLIENT_SECRET,
                "127.0.0.1",
                "encrypted_database",
                7,
            ),
        ];
        assert_eq!(
            classify_mcp_metadata(&rows, true, 7, Some(6)),
            McpMetadataCounts {
                total: 4,
                ready: 2,
                invalid: 2,
            }
        );
    }

    #[test]
    fn classifier_marks_null_key_version_metadata_invalid() {
        let rows = vec![McpCredentialMetadata {
            purpose: vault::MCP_OAUTH_ACCESS_TOKEN.into(),
            allowed_hosts: vec!["example.com".into()],
            backend: "encrypted_database".into(),
            key_version: None,
        }];
        assert_eq!(
            classify_mcp_metadata(&rows, true, 7, Some(6)),
            McpMetadataCounts {
                total: 1,
                ready: 0,
                invalid: 1,
            }
        );
        assert!(matches!(
            metadata_status(classify_mcp_metadata(&rows, true, 7, Some(6))),
            Status::Fail
        ));
    }

    #[test]
    fn classifier_marks_credentials_invalid_when_vault_is_unavailable() {
        let rows = vec![metadata(
            vault::MCP_OAUTH_PKCE_VERIFIER,
            "example.com",
            "encrypted_database",
            1,
        )];
        assert_eq!(
            classify_mcp_metadata(&rows, false, 1, None),
            McpMetadataCounts {
                total: 1,
                ready: 0,
                invalid: 1,
            }
        );
    }

    #[test]
    fn metadata_status_covers_pass_warn_and_fail_aggregates() {
        assert!(matches!(
            metadata_status(McpMetadataCounts {
                total: 2,
                ready: 2,
                invalid: 0,
            }),
            Status::Pass
        ));
        assert!(matches!(
            metadata_status(McpMetadataCounts {
                total: 0,
                ready: 0,
                invalid: 0,
            }),
            Status::Warn
        ));
        assert!(matches!(
            metadata_status(McpMetadataCounts {
                total: 2,
                ready: 1,
                invalid: 1,
            }),
            Status::Fail
        ));
        assert_eq!(MCP_METADATA_CAP, 1_000);
    }

    #[tokio::test]
    async fn vault_check_uses_real_key_validation_and_redacts_failures() {
        let mut settings = base_settings();
        settings.vault.master_key_base64 = Some(SecretString::from("not-base64"));
        let checks = run(&settings, None).await;
        let check = checks
            .iter()
            .find(|check| check.name == "Credential vault")
            .expect("vault check");
        assert!(matches!(check.status, Status::Fail));
        assert_eq!(check.detail, VAULT_FAILURE_DETAIL);
        assert!(!check.detail.contains("not-base64"));
    }

    #[tokio::test]
    async fn vault_check_warns_when_unconfigured() {
        let settings = base_settings();
        let checks = run(&settings, None).await;
        let check = checks
            .iter()
            .find(|check| check.name == "Credential vault")
            .expect("vault check");
        assert!(matches!(check.status, Status::Warn));
        assert_eq!(
            check.detail,
            "not configured; authenticated providers remain unavailable"
        );
    }

    #[tokio::test]
    async fn vault_check_fails_redacted_when_previous_key_has_no_current_key() {
        let mut settings = base_settings();
        settings.vault.previous_master_key_base64 = Some(SecretString::from(
            base64::engine::general_purpose::STANDARD.encode([2_u8; 32]),
        ));
        settings.vault.previous_key_version = Some(2);
        let checks = run(&settings, None).await;
        let check = checks
            .iter()
            .find(|check| check.name == "Credential vault")
            .expect("vault check");
        assert!(matches!(check.status, Status::Fail));
        assert_eq!(check.detail, VAULT_FAILURE_DETAIL);
    }
}
