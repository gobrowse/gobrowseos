use std::{
    net::{IpAddr, Ipv4Addr, Ipv6Addr, SocketAddr},
    sync::Arc,
    time::Duration,
};

use gobrowse_core::model::{EmbeddingProvider, ProviderError};
use secrecy::{ExposeSecret, SecretString};
use serde::{Deserialize, Serialize};
use sqlx::{PgPool, Postgres, Row, Transaction};
use tokio_util::sync::CancellationToken;
use tracing::{error, info, warn};
use url::Url;
use uuid::Uuid;

use crate::{AppState, error::AppError};

const LEASE_SECONDS: i64 = 300;
const MAX_BATCH_INPUTS: usize = 16;
const MAX_BATCH_BYTES: usize = 512 * 1024;

struct Job {
    id: Uuid,
    book_id: Uuid,
    embedding_model_id: String,
    target_revision: i64,
    attempts: i32,
    max_attempts: i32,
    lease_token: Uuid,
}

struct LoadedModel {
    provider_id: String,
    provider_type: String,
    model_reference: String,
    dimensions: usize,
    base_url: Url,
    secret_reference: Option<String>,
}

struct HttpEmbeddingProvider {
    id: String,
    provider_type: String,
    model: String,
    dimensions: usize,
    base_url: Url,
    token: Option<SecretString>,
    http: reqwest::Client,
}

#[derive(Serialize)]
struct OpenAiRequest<'a> {
    model: &'a str,
    input: &'a [String],
}

#[derive(Deserialize)]
struct OpenAiResponse {
    data: Vec<OpenAiEmbedding>,
}

#[derive(Deserialize)]
struct OpenAiEmbedding {
    index: usize,
    embedding: Vec<f32>,
}

#[derive(Serialize)]
struct OllamaRequest<'a> {
    model: &'a str,
    input: &'a [String],
}

#[derive(Deserialize)]
struct OllamaResponse {
    embeddings: Vec<Vec<f32>>,
}

#[derive(Debug)]
struct JobFailure {
    code: &'static str,
    detail: &'static str,
    retryable: bool,
    retry_after_seconds: Option<u64>,
}

impl From<ProviderError> for JobFailure {
    fn from(error: ProviderError) -> Self {
        match error {
            ProviderError::InvalidCredentials => {
                Self::permanent("invalid_credentials", "provider authentication failed")
            }
            ProviderError::RateLimited {
                retry_after_seconds,
            } => Self {
                code: "rate_limited",
                detail: "provider rate limit",
                retryable: true,
                retry_after_seconds,
            },
            ProviderError::TemporaryUnavailable => {
                Self::retryable("temporary_unavailable", "provider temporarily unavailable")
            }
            ProviderError::Timeout => Self::retryable("timeout", "provider request timed out"),
            ProviderError::Canceled => Self::retryable("canceled", "provider request was canceled"),
            ProviderError::UnsupportedCapability => {
                Self::permanent("unsupported", "embedding capability is unsupported")
            }
            ProviderError::ContextLimit => {
                Self::permanent("context_limit", "embedding input exceeds the model context")
            }
            ProviderError::InvalidResponse => Self::permanent(
                "invalid_response",
                "provider returned an invalid embedding response",
            ),
        }
    }
}

impl JobFailure {
    const fn permanent(code: &'static str, detail: &'static str) -> Self {
        Self {
            code,
            detail,
            retryable: false,
            retry_after_seconds: None,
        }
    }

    const fn retryable(code: &'static str, detail: &'static str) -> Self {
        Self {
            code,
            detail,
            retryable: true,
            retry_after_seconds: None,
        }
    }
}

#[async_trait::async_trait]
impl EmbeddingProvider for HttpEmbeddingProvider {
    fn id(&self) -> &str {
        &self.id
    }

    fn dimensions(&self) -> usize {
        self.dimensions
    }

    async fn embed(&self, inputs: &[String]) -> Result<Vec<Vec<f32>>, ProviderError> {
        if inputs.is_empty()
            || inputs.len() > MAX_BATCH_INPUTS
            || inputs.iter().map(String::len).sum::<usize>() > MAX_BATCH_BYTES
        {
            return Err(ProviderError::ContextLimit);
        }
        let (endpoint, body) = match self.provider_type.as_str() {
            "openai_compatible" => (
                endpoint(&self.base_url, "embeddings")?,
                serde_json::to_value(OpenAiRequest {
                    model: &self.model,
                    input: inputs,
                })
                .map_err(|_| ProviderError::InvalidResponse)?,
            ),
            "ollama" => (
                endpoint(&self.base_url, "api/embed")?,
                serde_json::to_value(OllamaRequest {
                    model: &self.model,
                    input: inputs,
                })
                .map_err(|_| ProviderError::InvalidResponse)?,
            ),
            _ => return Err(ProviderError::UnsupportedCapability),
        };
        let mut request = self.http.post(endpoint).json(&body);
        if let Some(token) = &self.token {
            request = request.bearer_auth(token.expose_secret());
        }
        let response = request.send().await.map_err(map_transport_error)?;
        let status = response.status();
        if status == reqwest::StatusCode::UNAUTHORIZED || status == reqwest::StatusCode::FORBIDDEN {
            return Err(ProviderError::InvalidCredentials);
        }
        if status == reqwest::StatusCode::TOO_MANY_REQUESTS {
            let retry_after_seconds = response
                .headers()
                .get(reqwest::header::RETRY_AFTER)
                .and_then(|value| value.to_str().ok())
                .and_then(|value| value.parse().ok());
            return Err(ProviderError::RateLimited {
                retry_after_seconds,
            });
        }
        if status.is_server_error() {
            return Err(ProviderError::TemporaryUnavailable);
        }
        if !status.is_success() {
            return Err(ProviderError::InvalidResponse);
        }
        let vectors = if self.provider_type == "openai_compatible" {
            let mut data = response
                .json::<OpenAiResponse>()
                .await
                .map_err(|_| ProviderError::InvalidResponse)?
                .data;
            data.sort_by_key(|entry| entry.index);
            if data
                .iter()
                .enumerate()
                .any(|(index, entry)| entry.index != index)
            {
                return Err(ProviderError::InvalidResponse);
            }
            data.into_iter().map(|entry| entry.embedding).collect()
        } else {
            response
                .json::<OllamaResponse>()
                .await
                .map_err(|_| ProviderError::InvalidResponse)?
                .embeddings
        };
        validate_vectors(inputs.len(), self.dimensions, &vectors)?;
        Ok(vectors)
    }
}

pub async fn enqueue_book(
    tx: &mut Transaction<'_, Postgres>,
    profile_id: Uuid,
    book_id: Uuid,
    revision: i64,
) -> Result<(), AppError> {
    sqlx::query(
        "INSERT INTO embedding_jobs (id, book_id, embedding_model_id, target_revision) \
         SELECT $1, $2, active_embedding_model_id, $3 FROM profiles \
         WHERE id=$4 AND active_embedding_model_id IS NOT NULL ON CONFLICT DO NOTHING",
    )
    .bind(Uuid::now_v7())
    .bind(book_id)
    .bind(revision)
    .bind(profile_id)
    .execute(&mut **tx)
    .await?;
    Ok(())
}

pub async fn embed_query(
    state: &AppState,
    profile_id: Uuid,
    query: &str,
) -> Result<Option<(String, Vec<f32>)>, AppError> {
    let model_id: Option<String> =
        sqlx::query_scalar("SELECT active_embedding_model_id FROM profiles WHERE id=$1")
            .bind(profile_id)
            .fetch_optional(&state.pool)
            .await?
            .flatten();
    let Some(model_id) = model_id else {
        return Ok(None);
    };
    let provider = match load_provider(state, profile_id, &model_id).await {
        Ok(provider) => provider,
        Err(error) => {
            warn!(profile_id=%profile_id, model_id, error=%error, "semantic provider unavailable; using lexical retrieval");
            return Ok(None);
        }
    };
    let vectors = provider.embed(&[query.to_owned()]).await.map_err(|error| {
        warn!(profile_id=%profile_id, model_id, error=%error, "semantic query embedding failed; using lexical retrieval");
        AppError::Internal(anyhow::anyhow!(error))
    });
    match vectors {
        Ok(mut vectors) => Ok(vectors.pop().map(|vector| (model_id, vector))),
        Err(_) => Ok(None),
    }
}

pub async fn run_worker(state: AppState, cancellation: CancellationToken) {
    let owner = format!("{}:{}", hostname(), Uuid::now_v7());
    info!(worker=%owner, "embedding worker started");
    loop {
        if cancellation.is_cancelled() {
            break;
        }
        if let Err(error) = reclaim_expired(&state.pool).await {
            error!(error=%error, "failed to reclaim expired embedding leases");
        }
        match claim(&state.pool, &owner).await {
            Ok(Some(job)) => {
                if let Err(error) = process(&state, &job).await
                    && let Err(update_error) = record_failure(&state.pool, &job, &error).await
                {
                    error!(job_id=%job.id, error=%update_error, "failed to record embedding job failure");
                }
            }
            Ok(None) => tokio::select! {
                () = cancellation.cancelled() => break,
                () = tokio::time::sleep(Duration::from_secs(1)) => {},
            },
            Err(error) => {
                error!(error=%error, "embedding worker claim failed");
                tokio::select! {
                    () = cancellation.cancelled() => break,
                    () = tokio::time::sleep(Duration::from_secs(2)) => {},
                }
            }
        }
    }
    info!(worker=%owner, "embedding worker stopped");
}

async fn claim(pool: &PgPool, owner: &str) -> Result<Option<Job>, sqlx::Error> {
    let lease_token = Uuid::now_v7();
    let row = sqlx::query(
        "WITH candidate AS (SELECT id FROM embedding_jobs \
             WHERE status IN ('queued','retry') AND available_at <= now() AND attempts < max_attempts \
             ORDER BY available_at, created_at FOR UPDATE SKIP LOCKED LIMIT 1) \
         UPDATE embedding_jobs AS job SET status='running', attempts=attempts+1, locked_at=now(), \
             lease_owner=$1, lease_token=$2, lease_expires_at=now()+make_interval(secs => $3), updated_at=now() \
         FROM candidate WHERE job.id=candidate.id \
         RETURNING job.id, job.book_id, job.embedding_model_id, job.target_revision, job.attempts, job.max_attempts, job.lease_token",
    )
    .bind(owner).bind(lease_token).bind(LEASE_SECONDS as f64).fetch_optional(pool).await?;
    Ok(row.map(|row| Job {
        id: row.get("id"),
        book_id: row.get("book_id"),
        embedding_model_id: row.get("embedding_model_id"),
        target_revision: row.get("target_revision"),
        attempts: row.get("attempts"),
        max_attempts: row.get("max_attempts"),
        lease_token: row.get("lease_token"),
    }))
}

async fn reclaim_expired(pool: &PgPool) -> Result<(), sqlx::Error> {
    let exhausted = sqlx::query(
        "UPDATE embedding_jobs SET status='failed',lease_owner=NULL,lease_token=NULL,lease_expires_at=NULL, \
         last_error_code='lease_expired',last_error_detail='Worker lease expired at retry limit',updated_at=now() \
         WHERE status='running' AND lease_expires_at<now() AND attempts>=max_attempts \
         RETURNING book_id,embedding_model_id,target_revision",
    )
    .fetch_all(pool)
    .await?;
    for row in exhausted {
        sqlx::query(
            "UPDATE books SET embedding_status='failed' WHERE id=$1 AND revision=$2 \
             AND EXISTS(SELECT 1 FROM profiles WHERE id=books.profile_id AND active_embedding_model_id=$3)",
        )
        .bind(row.get::<Uuid, _>("book_id"))
        .bind(row.get::<i64, _>("target_revision"))
        .bind(row.get::<String, _>("embedding_model_id"))
        .execute(pool)
        .await?;
    }
    sqlx::query(
        "UPDATE embedding_jobs SET status='retry', available_at=now(), lease_owner=NULL, lease_token=NULL, \
         lease_expires_at=NULL, last_error_code='lease_expired', last_error_detail='worker lease expired', updated_at=now() \
         WHERE status='running' AND lease_expires_at < now() AND attempts < max_attempts",
    ).execute(pool).await?;
    Ok(())
}

async fn process(state: &AppState, job: &Job) -> Result<(), JobFailure> {
    let row = sqlx::query("SELECT profile_id, revision FROM books WHERE id=$1")
        .bind(job.book_id)
        .fetch_optional(&state.pool)
        .await
        .map_err(|_| JobFailure::retryable("database", "database operation failed"))?
        .ok_or_else(|| JobFailure::permanent("book_missing", "Book no longer exists"))?;
    let profile_id: Uuid = row.get("profile_id");
    let current_revision: i64 = row.get("revision");
    if current_revision != job.target_revision {
        cancel_stale(&state.pool, job, current_revision)
            .await
            .map_err(|_| JobFailure::retryable("database", "database operation failed"))?;
        return Ok(());
    }
    let chunks = sqlx::query("SELECT id, text FROM book_chunks WHERE book_id=$1 ORDER BY ordinal")
        .bind(job.book_id)
        .fetch_all(&state.pool)
        .await
        .map_err(|_| JobFailure::retryable("database", "database operation failed"))?;
    let provider = load_provider(state, profile_id, &job.embedding_model_id)
        .await
        .map_err(|_| {
            JobFailure::permanent(
                "provider_configuration",
                "embedding provider configuration is unavailable",
            )
        })?;
    let mut all_vectors = Vec::with_capacity(chunks.len());
    for batch in chunks.chunks(MAX_BATCH_INPUTS) {
        renew_lease(&state.pool, job).await?;
        let inputs: Vec<String> = batch.iter().map(|row| row.get("text")).collect();
        all_vectors.extend(provider.embed(&inputs).await.map_err(JobFailure::from)?);
    }
    let mut tx = state
        .pool
        .begin()
        .await
        .map_err(|_| JobFailure::retryable("database", "database operation failed"))?;
    let valid_lease = sqlx::query(
        "SELECT j.id FROM embedding_jobs j JOIN books b ON b.id=j.book_id \
         WHERE j.id=$1 AND j.lease_token=$2 AND j.status='running' AND j.lease_expires_at>now() \
           AND b.revision=j.target_revision FOR UPDATE OF j,b",
    )
    .bind(job.id)
    .bind(job.lease_token)
    .fetch_optional(&mut *tx)
    .await
    .map_err(|_| JobFailure::retryable("database", "database operation failed"))?;
    if valid_lease.is_none() {
        return Err(JobFailure::retryable(
            "lease_lost",
            "embedding worker lease was lost",
        ));
    }
    for (row, vector) in chunks.iter().zip(all_vectors) {
        sqlx::query(
            "INSERT INTO book_chunk_embeddings (chunk_id, embedding_model_id, dimensions, book_revision, embedding) \
             VALUES ($1,$2,$3,$4,($5::text)::vector) ON CONFLICT (chunk_id, embedding_model_id) DO UPDATE SET \
             dimensions=EXCLUDED.dimensions, book_revision=EXCLUDED.book_revision, embedding=EXCLUDED.embedding, created_at=now()",
        )
        .bind(row.get::<Uuid, _>("id")).bind(&job.embedding_model_id)
        .bind(i32::try_from(provider.dimensions()).map_err(|_| JobFailure::permanent("dimensions", "invalid embedding dimensions"))?)
        .bind(job.target_revision).bind(vector_literal(&vector)).execute(&mut *tx).await
        .map_err(|_| JobFailure::retryable("database", "database operation failed"))?;
    }
    let completed = sqlx::query(
        "UPDATE embedding_jobs SET status='completed', completed_at=now(), lease_owner=NULL, lease_token=NULL, \
         lease_expires_at=NULL, last_error_code=NULL, last_error_detail=NULL, updated_at=now() WHERE id=$1 AND lease_token=$2"
    ).bind(job.id).bind(job.lease_token).execute(&mut *tx).await
        .map_err(|_| JobFailure::retryable("database", "database operation failed"))?;
    if completed.rows_affected() != 1 {
        return Err(JobFailure::retryable(
            "lease_lost",
            "embedding worker lease was lost",
        ));
    }
    sqlx::query(
        "UPDATE books SET embedding_status='ready', embedding_model_id=$1 WHERE id=$2 AND revision=$3 \
         AND EXISTS(SELECT 1 FROM profiles WHERE id=books.profile_id AND active_embedding_model_id=$1)"
    ).bind(&job.embedding_model_id).bind(job.book_id).bind(job.target_revision).execute(&mut *tx).await
        .map_err(|_| JobFailure::retryable("database", "database operation failed"))?;
    tx.commit()
        .await
        .map_err(|_| JobFailure::retryable("database", "database operation failed"))?;
    Ok(())
}

async fn record_failure(pool: &PgPool, job: &Job, failure: &JobFailure) -> Result<(), sqlx::Error> {
    let final_failure = !failure.retryable || job.attempts >= job.max_attempts;
    let delay = failure
        .retry_after_seconds
        .unwrap_or_else(|| 2_u64.saturating_pow(job.attempts.clamp(1, 10) as u32))
        .min(900);
    let status = if final_failure { "failed" } else { "retry" };
    let updated = sqlx::query(
        "UPDATE embedding_jobs SET status=$1, available_at=now()+make_interval(secs => $2), \
         lease_owner=NULL, lease_token=NULL, lease_expires_at=NULL, last_error_code=$3, last_error_detail=$4, updated_at=now() \
         WHERE id=$5 AND lease_token=$6",
    ).bind(status).bind(delay as f64).bind(failure.code).bind(failure.detail)
    .bind(job.id).bind(job.lease_token).execute(pool).await?;
    if final_failure && updated.rows_affected() == 1 {
        sqlx::query(
            "UPDATE books SET embedding_status='failed' WHERE id=$1 AND revision=$2 \
             AND EXISTS(SELECT 1 FROM profiles WHERE id=books.profile_id AND active_embedding_model_id=$3)",
        )
            .bind(job.book_id)
            .bind(job.target_revision)
            .bind(&job.embedding_model_id)
            .execute(pool)
            .await?;
    }
    warn!(job_id=%job.id, code=failure.code, retryable=failure.retryable, "embedding job failed");
    Ok(())
}

async fn renew_lease(pool: &PgPool, job: &Job) -> Result<(), JobFailure> {
    let result = sqlx::query(
        "UPDATE embedding_jobs SET lease_expires_at=now()+make_interval(secs => $1),updated_at=now() \
         WHERE id=$2 AND lease_token=$3 AND status='running' AND lease_expires_at>now()",
    )
    .bind(LEASE_SECONDS as f64)
    .bind(job.id)
    .bind(job.lease_token)
    .execute(pool)
    .await
    .map_err(|_| JobFailure::retryable("database", "database operation failed"))?;
    if result.rows_affected() != 1 {
        return Err(JobFailure::retryable(
            "lease_lost",
            "embedding worker lease was lost",
        ));
    }
    Ok(())
}

async fn cancel_stale(pool: &PgPool, job: &Job, current_revision: i64) -> Result<(), sqlx::Error> {
    let mut tx = pool.begin().await?;
    sqlx::query(
        "UPDATE embedding_jobs SET status='canceled', lease_owner=NULL, lease_token=NULL, lease_expires_at=NULL, \
         last_error_code='stale_revision', last_error_detail='Book changed before embedding completed', updated_at=now() \
         WHERE id=$1 AND lease_token=$2"
    ).bind(job.id).bind(job.lease_token).execute(&mut *tx).await?;
    let profile_id = sqlx::query_scalar("SELECT profile_id FROM books WHERE id=$1")
        .bind(job.book_id)
        .fetch_one(&mut *tx)
        .await?;
    enqueue_book(&mut tx, profile_id, job.book_id, current_revision)
        .await
        .map_err(|error| match error {
            AppError::Database(error) => error,
            _ => unreachable!(),
        })?;
    tx.commit().await?;
    Ok(())
}

async fn load_provider(
    state: &AppState,
    profile_id: Uuid,
    model_id: &str,
) -> Result<Arc<dyn EmbeddingProvider>, AppError> {
    let model = load_model(&state.pool, profile_id, model_id).await?;
    let token = match &model.secret_reference {
        Some(secret_id) => Some(
            state
                .vault
                .resolve(&state.pool, profile_id, secret_id)
                .await?,
        ),
        None => None,
    };
    let http = provider_http_client(
        &model.base_url,
        &model.provider_type,
        token.is_some(),
        state.settings.features.local_embeddings,
    )
    .await?;
    Ok(Arc::new(HttpEmbeddingProvider {
        id: model.provider_id,
        provider_type: model.provider_type,
        model: model.model_reference,
        dimensions: model.dimensions,
        base_url: model.base_url,
        token,
        http,
    }))
}

async fn load_model(
    pool: &PgPool,
    profile_id: Uuid,
    model_id: &str,
) -> Result<LoadedModel, AppError> {
    let row = sqlx::query(
        "SELECT p.id AS provider_id, p.provider_type, p.base_url, p.secret_reference, \
         em.model_reference, em.dimensions FROM embedding_models em JOIN providers p ON p.id=em.provider_id \
         WHERE em.id=$1 AND p.profile_id=$2 AND p.enabled AND em.enabled",
    ).bind(model_id).bind(profile_id).fetch_optional(pool).await?.ok_or(AppError::NotFound)?;
    let dimensions = usize::try_from(row.get::<i32, _>("dimensions"))
        .map_err(|error| AppError::Internal(anyhow::anyhow!(error)))?;
    let base_url_value: Option<String> = row
        .try_get("base_url")
        .map_err(|error| AppError::Internal(anyhow::anyhow!(error)))?;
    let base_url = Url::parse(
        base_url_value
            .as_deref()
            .ok_or_else(|| AppError::Validation("embedding provider has no base URL".into()))?,
    )
    .map_err(|error| AppError::Internal(anyhow::anyhow!(error)))?;
    Ok(LoadedModel {
        provider_id: row.get("provider_id"),
        provider_type: row.get("provider_type"),
        model_reference: row.get("model_reference"),
        dimensions,
        base_url,
        secret_reference: row.get("secret_reference"),
    })
}

fn endpoint(base: &Url, path: &str) -> Result<Url, ProviderError> {
    let mut base = base.clone();
    if !base.path().ends_with('/') {
        base.set_path(&format!("{}/", base.path()));
    }
    base.join(path).map_err(|_| ProviderError::InvalidResponse)
}

pub async fn validate_provider_endpoint(
    base_url: &Url,
    provider_type: &str,
    authenticated: bool,
    local_embeddings: bool,
) -> Result<(), AppError> {
    provider_http_client(base_url, provider_type, authenticated, local_embeddings)
        .await
        .map(|_| ())
}

async fn provider_http_client(
    base_url: &Url,
    provider_type: &str,
    authenticated: bool,
    local_embeddings: bool,
) -> Result<reqwest::Client, AppError> {
    let host = base_url
        .host_str()
        .ok_or_else(|| AppError::Validation("embedding provider URL requires a host".into()))?;
    let port = base_url.port_or_known_default().ok_or_else(|| {
        AppError::Validation("embedding provider URL requires a known port".into())
    })?;
    let addresses: Vec<IpAddr> = if let Ok(address) = host.parse() {
        vec![address]
    } else {
        tokio::net::lookup_host((host, port))
            .await
            .map_err(|_| {
                AppError::Validation("embedding provider host could not be resolved".into())
            })?
            .map(|address| address.ip())
            .collect()
    };
    if addresses.is_empty() || addresses.iter().any(|address| forbidden_address(*address)) {
        return Err(AppError::Validation(
            "embedding provider resolves to a prohibited network address".into(),
        ));
    }
    let has_local = addresses.iter().any(|address| local_address(*address));
    if has_local && !addresses.iter().all(|address| local_address(*address)) {
        return Err(AppError::Validation(
            "embedding provider DNS cannot mix public and private addresses".into(),
        ));
    }
    let allow_local = provider_type == "ollama"
        && local_embeddings
        && !authenticated
        && addresses.iter().all(|address| local_address(*address));
    if has_local && !allow_local {
        return Err(AppError::Validation(
            "private embedding endpoints require credential-free Ollama and features.local_embeddings"
                .into(),
        ));
    }
    if base_url.scheme() != "https" && !allow_local {
        return Err(AppError::Validation(
            "non-local embedding providers must use HTTPS".into(),
        ));
    }
    let pinned = addresses[0];
    let mut builder = reqwest::Client::builder()
        .connect_timeout(Duration::from_secs(5))
        .timeout(Duration::from_secs(30))
        .redirect(reqwest::redirect::Policy::none())
        .no_proxy()
        .user_agent(concat!("gobrowse-os/", env!("CARGO_PKG_VERSION")));
    if host.parse::<IpAddr>().is_err() {
        builder = builder.resolve(host, SocketAddr::new(pinned, port));
    }
    builder
        .build()
        .map_err(|error| AppError::Internal(anyhow::anyhow!(error)))
}

fn local_address(address: IpAddr) -> bool {
    match address {
        IpAddr::V4(address) => address.is_loopback() || address.is_private(),
        IpAddr::V6(address) => address.to_ipv4_mapped().map_or_else(
            || address.is_loopback() || (address.segments()[0] & 0xfe00) == 0xfc00,
            |mapped| mapped.is_loopback() || mapped.is_private(),
        ),
    }
}

fn forbidden_address(address: IpAddr) -> bool {
    match address {
        IpAddr::V4(address) => {
            address.is_unspecified()
                || address.is_link_local()
                || address.is_multicast()
                || address.is_broadcast()
                || address.is_documentation()
                || address.octets()[0] == 0
                || carrier_grade_nat(address)
                || benchmark_v4(address)
        }
        IpAddr::V6(address) => address.to_ipv4_mapped().map_or_else(
            || {
                address.is_unspecified()
                    || address.is_multicast()
                    || (address.segments()[0] & 0xffc0) == 0xfe80
                    || documentation_v6(address)
            },
            |mapped| forbidden_address(IpAddr::V4(mapped)),
        ),
    }
}

fn carrier_grade_nat(address: Ipv4Addr) -> bool {
    let [first, second, ..] = address.octets();
    first == 100 && (64..=127).contains(&second)
}

fn benchmark_v4(address: Ipv4Addr) -> bool {
    let [first, second, ..] = address.octets();
    first == 198 && matches!(second, 18 | 19)
}

fn documentation_v6(address: Ipv6Addr) -> bool {
    address.segments()[0] == 0x2001 && address.segments()[1] == 0x0db8
}

fn map_transport_error(error: reqwest::Error) -> ProviderError {
    if error.is_timeout() {
        ProviderError::Timeout
    } else {
        ProviderError::TemporaryUnavailable
    }
}

fn validate_vectors(
    expected_count: usize,
    dimensions: usize,
    vectors: &[Vec<f32>],
) -> Result<(), ProviderError> {
    if vectors.len() != expected_count
        || vectors.iter().any(|vector| {
            vector.len() != dimensions
                || vector.iter().any(|value| !value.is_finite())
                || vector.iter().all(|value| *value == 0.0)
        })
    {
        return Err(ProviderError::InvalidResponse);
    }
    Ok(())
}

pub fn vector_literal(vector: &[f32]) -> String {
    let values = vector
        .iter()
        .map(|value| value.to_string())
        .collect::<Vec<_>>()
        .join(",");
    format!("[{values}]")
}

fn hostname() -> String {
    std::env::var("HOSTNAME").unwrap_or_else(|_| "gobrowse".into())
}

pub async fn queue_depth(pool: &PgPool) -> Result<i64, sqlx::Error> {
    sqlx::query_scalar(
        "SELECT count(*) FROM embedding_jobs WHERE status IN ('queued','retry','running')",
    )
    .fetch_one(pool)
    .await
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn embedding_response_validation_rejects_bad_shapes_and_non_finite_values() {
        assert!(validate_vectors(1, 2, &[vec![1.0, 2.0]]).is_ok());
        assert!(validate_vectors(2, 2, &[vec![1.0, 2.0]]).is_err());
        assert!(validate_vectors(1, 2, &[vec![1.0]]).is_err());
        assert!(validate_vectors(1, 2, &[vec![f32::NAN, 1.0]]).is_err());
        assert!(validate_vectors(1, 2, &[vec![0.0, 0.0]]).is_err());
    }

    #[tokio::test]
    async fn provider_network_policy_blocks_metadata_even_for_local_ollama() {
        let metadata = Url::parse("http://169.254.169.254/latest").unwrap();
        assert!(
            validate_provider_endpoint(&metadata, "ollama", false, true)
                .await
                .is_err()
        );
        let mapped_metadata = Url::parse("http://[::ffff:169.254.169.254]/latest").unwrap();
        assert!(
            validate_provider_endpoint(&mapped_metadata, "ollama", false, true)
                .await
                .is_err()
        );
    }
}
