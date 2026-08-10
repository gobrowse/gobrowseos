use std::{path::Path, process::Stdio, time::Instant};

use serde::Serialize;
use sqlx::PgPool;
use tokio::process::Command;

use crate::config::Settings;

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

pub async fn run(settings: &Settings, pool: Option<&PgPool>) -> Vec<Check> {
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
        name: "Sandbox",
        status: if settings.features.sandbox {
            Status::Warn
        } else {
            Status::Pass
        },
        detail: if settings.features.sandbox {
            "enabled; sandboxd connectivity is validated when configured".into()
        } else {
            "disabled by feature policy".into()
        },
        latency_ms: None,
    });
    checks
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
