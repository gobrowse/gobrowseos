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
use crate::outbound_http::{TransportError, WebhookDeliveryDeps, resolve_target_with};
use crate::webhooks::{hex_encode, hmac_sha256};

/// How often the scheduler scans for pending deliveries.
const SCAN_INTERVAL_MS: u64 = 750;

/// Maximum concurrent outbound deliveries.
const MAX_CONCURRENT: usize = 4;

/// Maximum backoff interval in seconds (cap).
const MAX_BACKOFF_SECS: f64 = 60.0;

/// Worker lease duration in seconds. A running delivery whose lease expires
/// is considered crashed and is reclaimed by the reaper.
pub const LEASE_SECONDS: i64 = 120;

/// A delivery row claimed for outbound processing.
#[derive(Debug)]
pub struct ClaimedDelivery {
    pub webhook_id: Uuid,
    pub delivery_id: String,
    /// Identity of this specific claim. Outcomes must present this token while
    /// the lease is still active, otherwise a reclaimed worker owns the row.
    pub lease_token: Uuid,
    pub target_url: String,
    pub secret_key: Option<Vec<u8>>,
    pub attempts: i32,
}

/// The result of attempting one outbound POST.
#[derive(Debug, PartialEq, Eq)]
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
    let deps = WebhookDeliveryDeps::production();

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
                // Reap stuck deliveries before claiming new work.
                if let Err(error) = recover_stuck_deliveries(&state.pool, max_attempts).await {
                    error!(error=%error, "webhook stuck-delivery reaper failed");
                }
                let limit = (MAX_CONCURRENT - tasks.len()) as i64;
                match claim_deliveries(&state.pool, &owner, limit).await {
                    Ok(claimed) => {
                        for delivery in claimed {
                            let delivery_pool = state.pool.clone();
                            let delivery_deps = deps.clone();
                            tasks.spawn(async move {
                                let outcome = deliver_one_with(&delivery_deps, &delivery).await;
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
         SET status='running', attempts=d.attempts+1, next_attempt_at=NULL, \
             lease_token=gen_random_uuid(), \
             lease_expires_at=clock_timestamp() + make_interval(secs => $2) \
         FROM candidates c \
         WHERE d.webhook_id=c.webhook_id AND d.delivery_id=c.delivery_id \
         RETURNING d.webhook_id, d.delivery_id, d.lease_token, d.target_url, d.secret_key, d.attempts",
    )
    .bind(limit)
    .bind(LEASE_SECONDS as f64)
    .fetch_all(pool)
    .await?;

    Ok(rows
        .into_iter()
        .map(|row| ClaimedDelivery {
            webhook_id: row.get("webhook_id"),
            delivery_id: row.get("delivery_id"),
            lease_token: row.get("lease_token"),
            target_url: row.get("target_url"),
            secret_key: row.get("secret_key"),
            attempts: row.get("attempts"),
        })
        .collect())
}

/// Reclaim deliveries whose worker lease has expired (crashed worker).
///
/// Mirrors `embedding::reclaim_expired`:
/// - Dead-letter deliveries that have exhausted `max_attempts` as `dead`.
/// - Re-queue deliveries that still have attempts remaining.
///
/// Returns the total number of rows reclaimed (dead-lettered + re-queued).
pub async fn recover_stuck_deliveries(
    pool: &PgPool,
    max_attempts: u32,
) -> Result<u64, sqlx::Error> {
    // Dead-letter: max attempts reached, transition to 'dead'.
    let dead = sqlx::query(
        "UPDATE webhook_deliveries \
         SET status='dead', lease_token=NULL, lease_expires_at=NULL, \
             last_error='lease expired', last_response_code=NULL, next_attempt_at=NULL \
         WHERE status='running' \
           AND lease_expires_at < clock_timestamp() \
           AND attempts >= $1",
    )
    .bind(max_attempts as i32)
    .execute(pool)
    .await?;

    // Re-queue: still within max_attempts, make immediately claimable.
    // No backoff — this is crash recovery, not a delivery failure.
    let requeue = sqlx::query(
        "UPDATE webhook_deliveries \
         SET status='queued', lease_token=NULL, lease_expires_at=NULL, next_attempt_at=clock_timestamp() \
         WHERE status='running' \
           AND lease_expires_at < clock_timestamp() \
           AND attempts < $1",
    )
    .bind(max_attempts as i32)
    .execute(pool)
    .await?;

    Ok(dead.rows_affected() + requeue.rows_affected())
}

/// POST the canonical outbound payload to the target URL with HMAC signature.
pub async fn deliver_one_with(
    deps: &WebhookDeliveryDeps,
    delivery: &ClaimedDelivery,
) -> DeliveryOutcome {
    let target = match resolve_target_with(deps.resolver.as_ref(), &delivery.target_url).await {
        Ok(target) => target,
        Err(error) => {
            return DeliveryOutcome {
                response_code: None,
                success: false,
                retryable: false,
                error: Some(error.code().to_owned()),
            };
        }
    };

    let payload = format!("{}\n{}", delivery.target_url, delivery.delivery_id);
    let signature = delivery.secret_key.as_ref().map(|secret| {
        let mac = hmac_sha256(secret, payload.as_bytes());
        format!("sha256={}", hex_encode(&mac))
    });
    match deps
        .transport
        .send(
            &target,
            &delivery.delivery_id,
            &payload,
            signature.as_deref(),
        )
        .await
    {
        Ok(response) => classify_http_response(response.status),
        Err(error) => DeliveryOutcome {
            response_code: None,
            success: false,
            retryable: true,
            error: Some(
                match error {
                    TransportError::Timeout => "target_timeout",
                    TransportError::Connection => "target_connection_failed",
                    TransportError::Other => "target_transport_failed",
                }
                .to_owned(),
            ),
        },
    }
}

/// Map a target HTTP status into the scheduler's accepted outcome matrix.
pub(crate) fn classify_http_response(status: u16) -> DeliveryOutcome {
    let success = (200..300).contains(&status);
    let retryable = matches!(status, 408 | 429) || status >= 500;
    DeliveryOutcome {
        response_code: Some(status),
        success,
        retryable,
        error: if success || retryable {
            None
        } else {
            Some(format!("target returned non-retryable HTTP {status}"))
        },
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
             SET status='succeeded', last_response_code=$1, last_error=NULL, next_attempt_at=NULL, \
                 lease_token=NULL, lease_expires_at=NULL \
             WHERE webhook_id=$2 AND delivery_id=$3 \
               AND status='running' AND lease_token=$4 \
               AND lease_expires_at > clock_timestamp()",
        )
        .bind(outcome.response_code.map(|c| c as i32))
        .bind(delivery.webhook_id)
        .bind(&delivery.delivery_id)
        .bind(delivery.lease_token)
        .execute(pool)
        .await;
        if let Err(error) = result {
            error!(error=%error, webhook_id=%delivery.webhook_id, delivery_id=%delivery.delivery_id,
                   "failed to persist successful delivery");
        }
    } else if !outcome.retryable {
        let result = sqlx::query(
            "UPDATE webhook_deliveries \
             SET status='failed', last_response_code=$1, last_error=$2, next_attempt_at=NULL, \
                 lease_token=NULL, lease_expires_at=NULL \
             WHERE webhook_id=$3 AND delivery_id=$4 \
               AND status='running' AND lease_token=$5 \
               AND lease_expires_at > clock_timestamp()",
        )
        .bind(outcome.response_code.map(|c| c as i32))
        .bind(&outcome.error)
        .bind(delivery.webhook_id)
        .bind(&delivery.delivery_id)
        .bind(delivery.lease_token)
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
                 next_attempt_at=COALESCE($4, next_attempt_at), \
                 lease_token=NULL, lease_expires_at=NULL \
             WHERE webhook_id=$5 AND delivery_id=$6 \
               AND status='running' AND lease_token=$7 \
               AND lease_expires_at > clock_timestamp()",
        )
        .bind(new_status)
        .bind(outcome.response_code.map(|c| c as i32))
        .bind(&outcome.error)
        .bind(next_attempt)
        .bind(delivery.webhook_id)
        .bind(&delivery.delivery_id)
        .bind(delivery.lease_token)
        .execute(pool)
        .await;
        if let Err(error) = result {
            error!(error=%error, webhook_id=%delivery.webhook_id, delivery_id=%delivery.delivery_id,
                   "failed to persist retryable delivery outcome");
        }
    }
}

#[cfg(test)]
mod tests {
    use std::{
        net::{IpAddr, SocketAddr},
        sync::{
            Arc,
            atomic::{AtomicUsize, Ordering},
        },
    };

    use super::*;
    use crate::outbound_http::{
        OutboundResolver, OutboundTransport, ResolverFuture, TransportFuture, TransportResponse,
        ValidatedOutboundTarget, WebhookDeliveryDeps,
    };
    use sqlx::{Connection, PgConnection, PgPool};

    #[derive(Clone)]
    struct FixedResolver {
        address: IpAddr,
        calls: Arc<AtomicUsize>,
    }

    impl OutboundResolver for FixedResolver {
        fn resolve<'a>(&'a self, _host: &'a str, port: u16) -> ResolverFuture<'a> {
            self.calls.fetch_add(1, Ordering::SeqCst);
            let address = SocketAddr::new(self.address, port);
            Box::pin(async move { Ok(vec![address]) })
        }
    }

    type RequestRecord = (String, String, Option<String>);
    type RequestLog = Arc<std::sync::Mutex<Option<RequestRecord>>>;

    #[derive(Clone)]
    struct RecordingTransport {
        calls: Arc<AtomicUsize>,
        payload: RequestLog,
        status: u16,
    }

    impl OutboundTransport for RecordingTransport {
        fn send<'a>(
            &'a self,
            _target: &'a ValidatedOutboundTarget,
            delivery_id: &'a str,
            body: &'a str,
            signature: Option<&'a str>,
        ) -> TransportFuture<'a> {
            self.calls.fetch_add(1, Ordering::SeqCst);
            let payload = self.payload.clone();
            let delivery_id = delivery_id.to_owned();
            let body = body.to_owned();
            let signature = signature.map(str::to_owned);
            let status = self.status;
            Box::pin(async move {
                *payload.lock().expect("record payload") = Some((delivery_id, body, signature));
                Ok(TransportResponse { status })
            })
        }
    }

    fn delivery(secret_key: Option<Vec<u8>>) -> ClaimedDelivery {
        ClaimedDelivery {
            webhook_id: Uuid::now_v7(),
            delivery_id: "test-delivery".to_owned(),
            lease_token: Uuid::now_v7(),
            target_url: "https://hooks.example.test/deliver".to_owned(),
            secret_key,
            attempts: 1,
        }
    }

    fn deps_for_status(status: u16) -> WebhookDeliveryDeps {
        WebhookDeliveryDeps {
            resolver: Arc::new(FixedResolver {
                address: "1.1.1.1".parse().expect("public address"),
                calls: Arc::new(AtomicUsize::new(0)),
            }),
            transport: Arc::new(RecordingTransport {
                calls: Arc::new(AtomicUsize::new(0)),
                payload: Arc::new(std::sync::Mutex::new(None)),
                status,
            }),
        }
    }

    #[tokio::test]
    async fn response_statuses_preserve_scheduler_matrix() {
        for (status, success, retryable) in [
            (200, true, false),
            (204, true, false),
            (300, false, false),
            (302, false, false),
            (307, false, false),
            (400, false, false),
            (404, false, false),
            (408, false, true),
            (429, false, true),
            (500, false, true),
            (503, false, true),
        ] {
            let outcome = deliver_one_with(&deps_for_status(status), &delivery(None)).await;
            assert_eq!(outcome.response_code, Some(status));
            assert_eq!(outcome.success, success, "status={status}");
            assert_eq!(outcome.retryable, retryable, "status={status}");
        }
    }

    #[tokio::test]
    async fn policy_denial_is_terminal_and_never_calls_transport() {
        let resolver_calls = Arc::new(AtomicUsize::new(0));
        let transport_calls = Arc::new(AtomicUsize::new(0));
        let deps = WebhookDeliveryDeps {
            resolver: Arc::new(FixedResolver {
                address: "127.0.0.1".parse().expect("loopback"),
                calls: resolver_calls.clone(),
            }),
            transport: Arc::new(RecordingTransport {
                calls: transport_calls.clone(),
                payload: Arc::new(std::sync::Mutex::new(None)),
                status: 200,
            }),
        };
        let outcome = deliver_one_with(&deps, &delivery(None)).await;
        assert!(!outcome.success);
        assert!(!outcome.retryable);
        assert_eq!(outcome.error.as_deref(), Some("target_policy_denied"));
        assert_eq!(resolver_calls.load(Ordering::SeqCst), 1);
        assert_eq!(transport_calls.load(Ordering::SeqCst), 0);
    }

    #[tokio::test]
    async fn delivery_preserves_canonical_payload_id_and_hmac() {
        let payload = Arc::new(std::sync::Mutex::new(None));
        let deps = WebhookDeliveryDeps {
            resolver: Arc::new(FixedResolver {
                address: "1.1.1.1".parse().expect("public address"),
                calls: Arc::new(AtomicUsize::new(0)),
            }),
            transport: Arc::new(RecordingTransport {
                calls: Arc::new(AtomicUsize::new(0)),
                payload: payload.clone(),
                status: 204,
            }),
        };
        let secret = b"test-secret".to_vec();
        let delivery = delivery(Some(secret.clone()));
        let expected_body = format!("{}\n{}", delivery.target_url, delivery.delivery_id);
        let outcome = deliver_one_with(&deps, &delivery).await;
        assert!(outcome.success);
        assert_eq!(outcome.response_code, Some(204));
        let (id, body, signature) = payload
            .lock()
            .expect("read payload")
            .clone()
            .expect("request");
        assert_eq!(id, delivery.delivery_id);
        assert_eq!(body, expected_body);
        assert_eq!(
            signature.as_deref(),
            Some("sha256=7501aff799a68a731c5e250c9fa7d995b5491be28819843527a91b7ad5d8a307")
        );
    }

    async fn database_fixture() -> Option<(PgPool, PgConnection, Uuid, Uuid, Vec<u8>)> {
        let url = std::env::var("GOBROWSE_TEST_DATABASE_URL").ok()?;
        let mut lock = PgConnection::connect(&url)
            .await
            .expect("database lock connection");
        sqlx::query("SELECT pg_advisory_lock($1::bigint)")
            .bind(2_025_080_801_i64)
            .execute(&mut lock)
            .await
            .expect("database test lock");
        let pool = PgPool::connect(&url).await.expect("database pool");
        crate::db::migrate(&pool)
            .await
            .expect("database migrations");
        let profile_id = Uuid::now_v7();
        sqlx::query("INSERT INTO profiles (id,name) VALUES ($1,$2)")
            .bind(profile_id)
            .bind("scheduler-unit-db-profile")
            .execute(&pool)
            .await
            .expect("profile");
        let webhook_id = Uuid::now_v7();
        let secret = b"test-secret".to_vec();
        sqlx::query(
            "INSERT INTO webhooks (id,profile_id,name,secret_key,target,event_filter,enabled) VALUES ($1,$2,$3,$4,$5,$6,true)",
        )
        .bind(webhook_id)
        .bind(profile_id)
        .bind("scheduler-unit-db-webhook")
        .bind(&secret)
        .bind(serde_json::json!({"url":"https://hooks.example.test/deliver"}))
        .bind(serde_json::json!({"events":["*"]}))
        .execute(&pool)
        .await
        .expect("webhook");
        Some((pool, lock, webhook_id, profile_id, secret))
    }

    #[tokio::test]
    async fn database_delivery_path_persists_classifier_hmac_and_policy() {
        let Some((pool, _lock, webhook_id, profile_id, secret)) = database_fixture().await else {
            eprintln!("GOBROWSE_TEST_DATABASE_URL is unset; skipping");
            return;
        };
        let target_url = "https://hooks.example.test/deliver";
        let delivery_id = "test-delivery";
        sqlx::query(
            "INSERT INTO webhook_deliveries (webhook_id,delivery_id,status,attempts,next_attempt_at,target_url,secret_key,last_error) VALUES ($1,$2,'queued',0,clock_timestamp(),$3,$4,'prior failure')",
        )
        .bind(webhook_id)
        .bind(delivery_id)
        .bind(target_url)
        .bind(&secret)
        .execute(&pool)
        .await
        .expect("delivery");
        let claimed = claim_deliveries(&pool, "unit-db-owner", 1)
            .await
            .expect("claim")
            .into_iter()
            .next()
            .expect("claimed delivery");
        let payload = Arc::new(std::sync::Mutex::new(None));
        let deps = WebhookDeliveryDeps {
            resolver: Arc::new(FixedResolver {
                address: "1.1.1.1".parse().expect("public address"),
                calls: Arc::new(AtomicUsize::new(0)),
            }),
            transport: Arc::new(RecordingTransport {
                calls: Arc::new(AtomicUsize::new(0)),
                payload: payload.clone(),
                status: 204,
            }),
        };
        let outcome = deliver_one_with(&deps, &claimed).await;
        assert_eq!(outcome.response_code, Some(204));
        assert!(outcome.success);
        let request = payload.lock().expect("payload").clone().expect("request");
        assert_eq!(request.0, delivery_id);
        assert_eq!(request.1, format!("{target_url}\n{delivery_id}"));
        assert_eq!(
            request.2.as_deref(),
            Some("sha256=7501aff799a68a731c5e250c9fa7d995b5491be28819843527a91b7ad5d8a307")
        );
        persist_outcome(&pool, &claimed, &outcome, 5).await;
        let row = sqlx::query("SELECT status,last_response_code,last_error,next_attempt_at,lease_token,lease_expires_at FROM webhook_deliveries WHERE webhook_id=$1 AND delivery_id=$2")
            .bind(webhook_id)
            .bind(delivery_id)
            .fetch_one(&pool)
            .await
            .expect("success row");
        assert_eq!(row.get::<String, _>("status"), "succeeded");
        assert_eq!(row.get::<Option<i32>, _>("last_response_code"), Some(204));
        assert!(row.get::<Option<String>, _>("last_error").is_none());
        assert!(
            row.get::<Option<time::OffsetDateTime>, _>("next_attempt_at")
                .is_none()
        );
        assert!(row.get::<Option<Uuid>, _>("lease_token").is_none());
        assert!(
            row.get::<Option<time::OffsetDateTime>, _>("lease_expires_at")
                .is_none()
        );

        let policy_id = "policy-delivery";
        sqlx::query("INSERT INTO webhook_deliveries (webhook_id,delivery_id,status,attempts,next_attempt_at,target_url,secret_key) VALUES ($1,$2,'queued',0,clock_timestamp(),$3,$4)")
            .bind(webhook_id).bind(policy_id).bind(target_url).bind(&secret).execute(&pool).await.expect("policy delivery");
        let policy_claim = claim_deliveries(&pool, "unit-db-policy", 1)
            .await
            .expect("policy claim")
            .into_iter()
            .next()
            .expect("policy claimed");
        let policy_calls = Arc::new(AtomicUsize::new(0));
        let policy_deps = WebhookDeliveryDeps {
            resolver: Arc::new(FixedResolver {
                address: "127.0.0.1".parse().expect("loopback"),
                calls: Arc::new(AtomicUsize::new(0)),
            }),
            transport: Arc::new(RecordingTransport {
                calls: policy_calls.clone(),
                payload: Arc::new(std::sync::Mutex::new(None)),
                status: 200,
            }),
        };
        let policy = deliver_one_with(&policy_deps, &policy_claim).await;
        assert_eq!(policy.error.as_deref(), Some("target_policy_denied"));
        assert!(!policy.retryable);
        assert_eq!(policy_calls.load(Ordering::SeqCst), 0);
        persist_outcome(&pool, &policy_claim, &policy, 5).await;
        let row = sqlx::query("SELECT status,attempts,last_response_code,last_error,next_attempt_at,lease_token FROM webhook_deliveries WHERE webhook_id=$1 AND delivery_id=$2")
            .bind(webhook_id).bind(policy_id).fetch_one(&pool).await.expect("policy row");
        assert_eq!(row.get::<String, _>("status"), "failed");
        assert_eq!(row.get::<i32, _>("attempts"), 1);
        assert!(row.get::<Option<i32>, _>("last_response_code").is_none());
        assert_eq!(
            row.get::<Option<String>, _>("last_error").as_deref(),
            Some("target_policy_denied")
        );
        assert!(
            row.get::<Option<time::OffsetDateTime>, _>("next_attempt_at")
                .is_none()
        );
        assert!(row.get::<Option<Uuid>, _>("lease_token").is_none());

        for status in [200_u16, 204, 300, 302, 307, 400, 404, 408, 429, 500, 503] {
            let matrix_id = format!("matrix-{status}");
            sqlx::query("INSERT INTO webhook_deliveries (webhook_id,delivery_id,status,attempts,next_attempt_at,target_url,secret_key) VALUES ($1,$2,'queued',0,clock_timestamp(),$3,$4)")
                .bind(webhook_id).bind(&matrix_id).bind(target_url).bind(&secret).execute(&pool).await.expect("matrix delivery");
            let matrix_claim = claim_deliveries(&pool, "unit-db-matrix", 100)
                .await
                .expect("matrix claim")
                .into_iter()
                .find(|d| d.delivery_id == matrix_id)
                .expect("matrix claimed");
            let matrix_outcome = deliver_one_with(&deps_for_status(status), &matrix_claim).await;
            assert_eq!(matrix_outcome, classify_http_response(status));
            persist_outcome(&pool, &matrix_claim, &matrix_outcome, 5).await;
            let matrix_row = sqlx::query("SELECT status,attempts,last_response_code,last_error,next_attempt_at,lease_token,lease_expires_at FROM webhook_deliveries WHERE webhook_id=$1 AND delivery_id=$2")
                .bind(webhook_id).bind(&matrix_id).fetch_one(&pool).await.expect("matrix row");
            let expected_status = if (200..300).contains(&status) {
                "succeeded"
            } else if matches!(status, 408 | 429) || status >= 500 {
                "queued"
            } else {
                "failed"
            };
            assert_eq!(matrix_row.get::<String, _>("status"), expected_status);
            assert_eq!(matrix_row.get::<i32, _>("attempts"), 1);
            assert_eq!(
                matrix_row.get::<Option<i32>, _>("last_response_code"),
                Some(status as i32)
            );
            assert!(matrix_row.get::<Option<Uuid>, _>("lease_token").is_none());
            assert!(
                matrix_row
                    .get::<Option<time::OffsetDateTime>, _>("lease_expires_at")
                    .is_none()
            );
            if expected_status == "queued" {
                assert!(
                    matrix_row
                        .get::<Option<time::OffsetDateTime>, _>("next_attempt_at")
                        .is_some()
                );
                assert!(matrix_row.get::<Option<String>, _>("last_error").is_none());
            } else {
                assert!(
                    matrix_row
                        .get::<Option<time::OffsetDateTime>, _>("next_attempt_at")
                        .is_none()
                );
            }
        }
        let dead_id = "matrix-dead";
        sqlx::query("INSERT INTO webhook_deliveries (webhook_id,delivery_id,status,attempts,next_attempt_at,target_url,secret_key) VALUES ($1,$2,'queued',4,clock_timestamp(),$3,$4)")
            .bind(webhook_id).bind(dead_id).bind(target_url).bind(&secret).execute(&pool).await.expect("dead matrix delivery");
        let dead_claim = claim_deliveries(&pool, "unit-db-dead", 100)
            .await
            .expect("dead claim")
            .into_iter()
            .find(|d| d.delivery_id == dead_id)
            .expect("dead claimed");
        let dead_outcome = deliver_one_with(&deps_for_status(503), &dead_claim).await;
        persist_outcome(&pool, &dead_claim, &dead_outcome, 5).await;
        let dead_row = sqlx::query("SELECT status,attempts,next_attempt_at,lease_token,lease_expires_at FROM webhook_deliveries WHERE webhook_id=$1 AND delivery_id=$2")
            .bind(webhook_id).bind(dead_id).fetch_one(&pool).await.expect("dead matrix row");
        assert_eq!(dead_row.get::<String, _>("status"), "dead");
        assert_eq!(dead_row.get::<i32, _>("attempts"), 5);
        assert!(
            dead_row
                .get::<Option<time::OffsetDateTime>, _>("next_attempt_at")
                .is_none()
        );
        assert!(dead_row.get::<Option<Uuid>, _>("lease_token").is_none());
        assert!(
            dead_row
                .get::<Option<time::OffsetDateTime>, _>("lease_expires_at")
                .is_none()
        );
        sqlx::query("DELETE FROM webhooks WHERE id=$1")
            .bind(webhook_id)
            .execute(&pool)
            .await
            .expect("cleanup webhook");
        sqlx::query("DELETE FROM profiles WHERE id=$1")
            .bind(profile_id)
            .execute(&pool)
            .await
            .expect("cleanup profile");
    }
}
