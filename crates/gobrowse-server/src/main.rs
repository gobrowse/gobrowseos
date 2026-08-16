use std::{path::PathBuf, process::ExitCode};

use anyhow::Context;
use clap::{Parser, Subcommand};
use gobrowse_server::{
    AppState, config::Settings, db, doctor, embedding, outbound_http::WebhookDeliveryDeps,
    router, run_api, webhook_scheduler,
};
use tokio::net::TcpListener;
use tokio_util::sync::CancellationToken;
use tracing::{error, info};
use tracing_subscriber::EnvFilter;

#[derive(Debug, Parser)]
#[command(
    name = "gobrowse",
    version,
    about = "Gobrowse OS server and operations CLI"
)]
struct Cli {
    #[arg(long, env = "GOBROWSE_CONFIG")]
    config: Option<PathBuf>,
    #[command(subcommand)]
    command: Command,
}

#[derive(Debug, Subcommand)]
enum Command {
    /// Start the HTTP and WebSocket server.
    Serve,
    /// Apply forward database migrations.
    Migrate,
    /// Check runtime dependencies and configuration.
    Doctor {
        #[arg(long)]
        json: bool,
    },
    /// Inspect security-sensitive configuration.
    Security {
        #[command(subcommand)]
        command: SecurityCommand,
    },
    /// Print effective non-secret configuration metadata.
    Config,
}

#[derive(Debug, Subcommand)]
enum SecurityCommand {
    Audit {
        #[arg(long)]
        json: bool,
    },
}

#[tokio::main]
async fn main() -> ExitCode {
    match run().await {
        Ok(()) => ExitCode::SUCCESS,
        Err(error) => {
            error!(error = %error, "gobrowse command failed");
            eprintln!("error: {error:#}");
            ExitCode::FAILURE
        }
    }
}

async fn run() -> anyhow::Result<()> {
    let cli = Cli::parse();
    let settings = Settings::load(cli.config.as_deref()).context("load configuration")?;
    init_tracing(&settings);
    match cli.command {
        Command::Serve => serve(settings).await,
        Command::Migrate => {
            let pool = db::connect(&settings.database)
                .await
                .context("connect to database")?;
            db::migrate(&pool).await.context("apply migrations")?;
            info!("migrations applied");
            Ok(())
        }
        Command::Doctor { json } => {
            let pool = db::connect(&settings.database).await.ok();
            let checks = doctor::run(&settings, pool.as_ref()).await;
            print_checks(&checks, json)?;
            anyhow::ensure!(
                checks
                    .iter()
                    .all(|check| !matches!(check.status, doctor::Status::Fail)),
                "one or more diagnostics failed"
            );
            Ok(())
        }
        Command::Security {
            command: SecurityCommand::Audit { json },
        } => {
            let checks = security_checks(&settings);
            print_checks(&checks, json)?;
            anyhow::ensure!(
                checks
                    .iter()
                    .all(|check| !matches!(check.status, doctor::Status::Fail)),
                "security audit failed"
            );
            Ok(())
        }
        Command::Config => {
            println!("bind={}", settings.http.bind);
            println!("public_origin={}", settings.http.public_origin);
            println!("secure_cookies={}", settings.http.secure_cookies);
            println!(
                "vault={}",
                if settings.vault.master_key_file.is_some()
                    || settings.vault.master_key_base64.is_some()
                {
                    "configured"
                } else {
                    "unconfigured"
                }
            );
            println!("sandbox={}", settings.features.sandbox);
            Ok(())
        }
    }
}

async fn serve(settings: Settings) -> anyhow::Result<()> {
    let pool = db::connect(&settings.database)
        .await
        .context("connect to database")?;
    db::migrate(&pool).await.context("apply migrations")?;
    let bind = settings.http.bind;
    let state = AppState::new(pool, settings)
        .await
        .context("initialize application")?;
    let listener = TcpListener::bind(bind)
        .await
        .context("bind HTTP listener")?;
    info!(%bind, "Gobrowse OS listening");
    let cancellation = CancellationToken::new();
    let embedding_worker = tokio::spawn(embedding::run_worker(
        state.clone(),
        cancellation.child_token(),
    ));
    let run_worker = tokio::spawn(run_api::run_worker(
        state.clone(),
        cancellation.child_token(),
    ));
    let webhook_scheduler_worker = if state.settings.features.webhook_scheduler_enabled {
        Some(tokio::spawn(webhook_scheduler::run_worker(
            state.clone(),
            cancellation.child_token(),
            WebhookDeliveryDeps::production(),
        )))
    } else {
        None
    };
    let shutdown = cancellation.clone();
    let result = axum::serve(listener, router(state))
        .with_graceful_shutdown(async move {
            shutdown_signal().await;
            shutdown.cancel();
        })
        .await
        .context("serve HTTP");
    cancellation.cancel();
    if tokio::time::timeout(Settings::shutdown_timeout(), embedding_worker)
        .await
        .is_err()
    {
        error!("embedding worker did not stop before the shutdown deadline");
    }
    if tokio::time::timeout(Settings::shutdown_timeout(), run_worker)
        .await
        .is_err()
    {
        error!("run worker did not stop before the shutdown deadline");
    }
    if let Some(worker) = webhook_scheduler_worker
        && tokio::time::timeout(Settings::shutdown_timeout(), worker)
            .await
            .is_err()
    {
        error!("webhook scheduler worker did not stop before the shutdown deadline");
    }
    result
}

async fn shutdown_signal() {
    let ctrl_c = async {
        tokio::signal::ctrl_c()
            .await
            .expect("install Ctrl-C handler")
    };
    #[cfg(unix)]
    let terminate = async {
        tokio::signal::unix::signal(tokio::signal::unix::SignalKind::terminate())
            .expect("install SIGTERM handler")
            .recv()
            .await;
    };
    #[cfg(not(unix))]
    let terminate = std::future::pending::<()>();
    tokio::select! { () = ctrl_c => {}, () = terminate => {} }
    info!("graceful shutdown requested");
}

fn init_tracing(settings: &Settings) {
    let filter = EnvFilter::try_new(&settings.observability.log_filter)
        .unwrap_or_else(|_| EnvFilter::new("info"));
    if settings.observability.json_logs {
        tracing_subscriber::fmt()
            .with_env_filter(filter)
            .json()
            .init();
    } else {
        tracing_subscriber::fmt()
            .with_env_filter(filter)
            .compact()
            .init();
    }
}

fn security_checks(settings: &Settings) -> Vec<doctor::Check> {
    vec![
        doctor::Check {
            name: "Secure cookies",
            status: if settings.http.public_origin.scheme() == "https"
                && settings.http.secure_cookies
            {
                doctor::Status::Pass
            } else if settings.http.public_origin.host_str() == Some("localhost") {
                doctor::Status::Warn
            } else {
                doctor::Status::Fail
            },
            detail: "production origins require HTTPS and Secure cookies".into(),
            latency_ms: None,
        },
        doctor::Check {
            name: "Credential vault",
            status: if settings.vault.master_key_file.is_some()
                || settings.vault.master_key_base64.is_some()
            {
                doctor::Status::Pass
            } else {
                doctor::Status::Warn
            },
            detail: "provider credentials require an external 256-bit master key".into(),
            latency_ms: None,
        },
        doctor::Check {
            name: "Sandbox boundary",
            status: if settings.features.sandbox {
                doctor::Status::Warn
            } else {
                doctor::Status::Pass
            },
            detail: "the app configuration contains no container runtime socket".into(),
            latency_ms: None,
        },
        doctor::Check {
            name: "External telemetry",
            status: if settings.features.otel {
                doctor::Status::Warn
            } else {
                doctor::Status::Pass
            },
            detail: if settings.features.otel {
                "explicitly enabled".into()
            } else {
                "disabled".into()
            },
            latency_ms: None,
        },
    ]
}

fn print_checks(checks: &[doctor::Check], json: bool) -> anyhow::Result<()> {
    if json {
        println!("{}", serde_json::to_string_pretty(checks)?);
    } else {
        for check in checks {
            println!("{:<24} {:<5?} {}", check.name, check.status, check.detail);
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use gobrowse_server::config::{
        AuthSettings, DatabaseSettings, FeatureSettings, HttpSettings, ObservabilitySettings,
        Settings, VaultSettings,
    };
    use secrecy::SecretString;

    fn base_test_settings() -> Settings {
        Settings {
            http: HttpSettings {
                public_origin: url::Url::parse("http://localhost:8080").expect("valid URL"),
                secure_cookies: false,
                ..Default::default()
            },
            database: DatabaseSettings {
                url: SecretString::from("postgres://test/placeholder"),
                max_connections: 2,
            },
            auth: AuthSettings::default(),
            vault: VaultSettings::default(),
            features: FeatureSettings::default(),
            observability: ObservabilitySettings::default(),
        }
    }

    #[test]
    fn security_checks_passes_for_https_origin_with_secure_cookies() {
        let mut settings = base_test_settings();
        settings.http.public_origin =
            url::Url::parse("https://gobrowse.example.com").expect("valid URL");
        settings.http.secure_cookies = true;
        let checks = security_checks(&settings);
        let cookie_check = checks
            .iter()
            .find(|c| c.name == "Secure cookies")
            .expect("Secure cookies check exists");
        assert!(
            matches!(cookie_check.status, doctor::Status::Pass),
            "expected Pass for HTTPS+secure_cookies, got {:?}",
            cookie_check.status
        );
    }

    #[test]
    fn security_checks_fails_for_plain_http_non_localhost_production_origin() {
        let mut settings = base_test_settings();
        settings.http.public_origin =
            url::Url::parse("http://gobrowse.example.com").expect("valid URL");
        settings.http.secure_cookies = false;
        let checks = security_checks(&settings);
        let cookie_check = checks
            .iter()
            .find(|c| c.name == "Secure cookies")
            .expect("Secure cookies check exists");
        assert!(
            matches!(cookie_check.status, doctor::Status::Fail),
            "expected Fail for non-localhost HTTP, got {:?}",
            cookie_check.status
        );
    }

    #[test]
    fn security_checks_warns_for_localhost_http_dev_origin() {
        let settings = base_test_settings(); // default: http://localhost:8080
        let checks = security_checks(&settings);
        let cookie_check = checks
            .iter()
            .find(|c| c.name == "Secure cookies")
            .expect("Secure cookies check exists");
        assert!(
            matches!(cookie_check.status, doctor::Status::Warn),
            "expected Warn for localhost HTTP, got {:?}",
            cookie_check.status
        );
    }

    #[test]
    fn security_checks_warns_when_vault_unconfigured_and_sandbox_enabled() {
        let mut settings = base_test_settings();
        settings.features.sandbox = true;
        // No vault master key configured (default)
        let checks = security_checks(&settings);

        let vault_check = checks
            .iter()
            .find(|c| c.name == "Credential vault")
            .expect("Credential vault check exists");
        assert!(
            matches!(vault_check.status, doctor::Status::Warn),
            "expected Warn for unconfigured vault, got {:?}",
            vault_check.status
        );

        let sandbox_check = checks
            .iter()
            .find(|c| c.name == "Sandbox boundary")
            .expect("Sandbox boundary check exists");
        assert!(
            matches!(sandbox_check.status, doctor::Status::Warn),
            "expected Warn for sandbox enabled, got {:?}",
            sandbox_check.status
        );
    }
}
