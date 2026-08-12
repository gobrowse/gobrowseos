//! Outbound webhook delivery scheduler.
//!
//! Claims pending deliveries from `webhook_deliveries`, signs them with
//! HMAC-SHA256, POSTs to the target URL, and tracks attempts with
//! exponential backoff. Dead-letter after `max_attempts` retryable failures.
//!
//! The claim loop mirrors `run_api::claim_runs` (WITH candidates MATERIALIZED,
//! FOR UPDATE SKIP LOCKED, RETURNING).

use std::time::Duration;

use sqlx::{PgPool, Row};
use tokio::task::JoinSet;
use tokio::time::MissedTickBehavior;
use tokio_util::sync::CancellationToken;
use tracing::error;
use uuid::Uuid;

use crate::AppState;
use crate::webhooks::{hex_encode, hmac_sha256};

/// How often the scheduler scans for pending deliveries.
const SCAN_INTERVAL_MS: u64 = 750;

/// Maximum concurrent outbound deliveries.
const MAX_CONCURRENT: usize = 4;

/// HTTP request timeout for each outbound delivery POST.
const DELIVERY_TIMEOUT_SECS: u64 = 5;

/// Maximum backoff interval in seconds (cap).
const MAX_BACKOFF_SECS: f64 = 60.0;

/// A delivery row claimed for outbound processing.
#[derive(Debug)]
pub struct ClaimedDelivery {
    pub webhook_id: Uuid,
    pub delivery_id: String,
    pub target_url: String,
    pub secret_key: Option<Vec<u8>>,
    pub attempts: i32,
}

/// The result of attempting one outbound POST.
pub struct DeliveryOutcome {
    pub response_code: Option<u16>,
    pub success: bool,
    pub retryable: bool,
    pub error: Option<String>,
}

/// Background worker: scans for pending deliveries, claims them, and delivers.
///
/// Mirrors `run_api::run_worker` structure: interval scan, `JoinSet`, graceful
/// shutdown via `CancellationToken`, and drain-on-exit.
pub async fn run_worker(state: AppState, shutdown: CancellationToken) {
    let owner = format!("gobrowse-server:{}:{}", std::process::id(), Uuid::now_v7());
    let max_attempts = state.settings.features.webhook_scheduler_max_attempts;
    let http = reqwest::Client::builder()
        .connect_timeout(Duration::from_secs(5))
        .timeout(Duration::from_secs(DELIVERY_TIMEOUT_SECS))
        .redirect(reqwest::redirect::Policy::none())
        .no_proxy()
        .user_agent(concat!("gobrowse-os/", env!("CARGO_PKG_VERSION")))
        .build()
        .expect("reqwest client for webhook scheduler");

    let mut tasks = JoinSet::new();
    let mut scan = tokio::time::interval(Duration::from_millis(SCAN_INTERVAL_MS));
    scan.set_missed_tick_behavior(MissedTickBehavior::Delay);

    loop {
        tokio::select! {
            () = shutdown.cancelled() => break,
            Some(result) = tasks.join_next(), if !tasks.is_empty() => {
                if let Err(error) = result {
                    error!(error=%error, "webhook delivery task panicked");
                }
            }
            _ = scan.tick(), if tasks.len() < MAX_CONCURRENT => {
                let limit = (MAX_CONCURRENT - tasks.len()) as i64;
                match claim_deliveries(&state.pool, &owner, limit).await {
                    Ok(claimed) => {
                        for delivery in claimed {
                            let delivery_pool = state.pool.clone();
                            let delivery_http = http.clone();
                            tasks.spawn(async move {
                                let outcome = deliver_one(&delivery_http, &delivery).await;
                                persist_outcome(
                                    &delivery_pool,
                                    &delivery,
                                    &outcome,
                                    max_attempts,
                                )
                                .await;
                            });
                        }
                    }
                    Err(error) => error!(error=%error, "webhook delivery claim scan failed"),
                }
            }
        }
    }

    // Graceful drain: wait for in-flight deliveries to finish.
    while tasks.join_next().await.is_some() {}
}

/// Claim pending deliveries using `FOR UPDATE SKIP LOCKED`.
///
/// Mirrors `run_api::claim_runs` — `WITH candidates AS MATERIALIZED` followed
/// by `UPDATE ... FROM candidates ... RETURNING`.
pub async fn claim_deliveries(
    pool: &PgPool,
    owner: &str,
    limit: i64,
) -> Result<Vec<ClaimedDelivery>, sqlx::Error> {
    let _ = owner; // reserved for future use (owner column not in schema yet)
    let rows = sqlx::query(
        "WITH candidates AS MATERIALIZED ( \
         SELECT webhook_id, delivery_id \
         FROM webhook_deliveries \
         WHERE status IN ('queued','accepted') \
           AND next_attempt_at <= clock_timestamp() \
           AND target_url IS NOT NULL \
         ORDER BY next_attempt_at \
         LIMIT $1 \
         FOR UPDATE SKIP LOCKED) \
         UPDATE webhook_deliveries d \
         SET status='running', attempts=d.attempts+1, next_attempt_at=NULL \
         FROM candidates c \
         WHERE d.webhook_id=c.webhook_id AND d.delivery_id=c.delivery_id \
         RETURNING d.webhook_id, d.delivery_id, d.target_url, d.secret_key, d.attempts",
    )
    .bind(limit)
    .fetch_all(pool)
    .await?;

    Ok(rows
        .into_iter()
        .map(|row| ClaimedDelivery {
            webhook_id: row.get("webhook_id"),
            delivery_id: row.get("delivery_id"),
            target_url: row.get("target_url"),
            secret_key: row.get("secret_key"),
            attempts: row.get("attempts"),
        })
        .collect())
}

/// POST the canonical outbound payload to the target URL with HMAC signature.
pub async fn deliver_one(http: &reqwest::Client, delivery: &ClaimedDelivery) -> DeliveryOutcome {
    // Canonical outbound signing payload: {target}\n{delivery_id}
    let payload = format!("{}\n{}", delivery.target_url, delivery.delivery_id);

    let mut request = http.post(&delivery.target_url).body(payload.clone());

    // Attach delivery identifier header.
    request = request.header("X-Gobrowse-Delivery", &delivery.delivery_id);

    // HMAC sign if secret is configured.
    if let Some(ref secret) = delivery.secret_key {
        let mac = hmac_sha256(secret, payload.as_bytes());
        let sig = format!("sha256={}", hex_encode(&mac));
        request = request.header("X-Gobrowse-Signature", &sig);
    }

    match request.send().await {
        Ok(response) => {
            let status = response.status().as_u16();
            let success = (200..300).contains(&status);
            let retryable = match status {
                408 | 429 => true,
                s if s >= 500 => true,
                _ => false,
            };
            let error = if success || retryable {
                None
            } else {
                Some(format!("target returned non-retryable HTTP {status}"))
            };
            DeliveryOutcome {
                response_code: Some(status),
                success,
                retryable,
                error,
            }
        }
        Err(err) => {
            // Network-level errors are retryable.
            let retryable = !(err.is_timeout() && cfg!(test));
            // In practice, all transport errors are retryable except perhaps
            // some unrecoverable ones. Even timeouts are retryable.
            let _ = retryable;
            DeliveryOutcome {
                response_code: None,
                success: false,
                retryable: true,
                error: Some(err.to_string()),
            }
        }
    }
}

/// Persist the delivery outcome to the database.
///
/// - Success → `status='succeeded'`
/// - Non-retryable fail → `status='failed'`
/// - Retryable, attempts < max → `status='queued'` with exponential backoff
/// - Retryable, attempts >= max → `status='dead'` (dead-letter)
pub async fn persist_outcome(
    pool: &PgPool,
    delivery: &ClaimedDelivery,
    outcome: &DeliveryOutcome,
    max_attempts: u32,
) {
    if outcome.success {
        let result = sqlx::query(
            "UPDATE webhook_deliveries \
             SET status='succeeded', last_response_code=$1, next_attempt_at=NULL \
             WHERE webhook_id=$2 AND delivery_id=$3",
        )
        .bind(outcome.response_code.map(|c| c as i32))
        .bind(delivery.webhook_id)
        .bind(&delivery.delivery_id)
        .execute(pool)
        .await;
        if let Err(error) = result {
            error!(error=%error, webhook_id=%delivery.webhook_id, delivery_id=%delivery.delivery_id,
                   "failed to persist successful delivery");
        }
    } else if !outcome.retryable {
        let result = sqlx::query(
            "UPDATE webhook_deliveries \
             SET status='failed', last_response_code=$1, last_error=$2, next_attempt_at=NULL \
             WHERE webhook_id=$3 AND delivery_id=$4",
        )
        .bind(outcome.response_code.map(|c| c as i32))
        .bind(&outcome.error)
        .bind(delivery.webhook_id)
        .bind(&delivery.delivery_id)
        .execute(pool)
        .await;
        if let Err(error) = result {
            error!(error=%error, webhook_id=%delivery.webhook_id, delivery_id=%delivery.delivery_id,
                   "failed to persist non-retryable delivery failure");
        }
    } else {
        // Retryable: apply exponential backoff or dead-letter.
        let current_attempts = delivery.attempts as u32;
        let new_status: &str;
        let next_attempt: Option<time::OffsetDateTime>;

        if current_attempts >= max_attempts {
            new_status = "dead";
            next_attempt = None;
        } else {
            new_status = "queued";
            let backoff_secs = 2f64.powi(current_attempts as i32).min(MAX_BACKOFF_SECS);
            next_attempt =
                Some(time::OffsetDateTime::now_utc() + time::Duration::seconds_f64(backoff_secs));
        }

        let result = sqlx::query(
            "UPDATE webhook_deliveries \
             SET status=$1, last_response_code=$2, last_error=$3, \
                 next_attempt_at=COALESCE($4, next_attempt_at) \
             WHERE webhook_id=$5 AND delivery_id=$6",
        )
        .bind(new_status)
        .bind(outcome.response_code.map(|c| c as i32))
        .bind(&outcome.error)
        .bind(next_attempt)
        .bind(delivery.webhook_id)
        .bind(&delivery.delivery_id)
        .execute(pool)
        .await;
        if let Err(error) = result {
            error!(error=%error, webhook_id=%delivery.webhook_id, delivery_id=%delivery.delivery_id,
                   "failed to persist retryable delivery outcome");
        }
    }
}
