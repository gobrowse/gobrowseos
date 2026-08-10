use std::time::Duration as StdDuration;

use axum::{
    Json, Router,
    body::Body,
    http::{Method, Request, StatusCode, header},
    routing::post,
};
use gobrowse_server::{
    AppState,
    config::{
        AuthSettings, DatabaseSettings, FeatureSettings, HttpSettings, ObservabilitySettings,
        Settings, VaultSettings,
    },
    db, embedding, router,
};
use http_body_util::BodyExt;
use secrecy::SecretString;
use serde_json::{Value, json};
use sha2::{Digest, Sha256};
use sqlx::{PgPool, Row};
use time::{Duration, OffsetDateTime};
use tokio::net::TcpListener;
use tokio_util::sync::CancellationToken;
use tower::ServiceExt;
use uuid::Uuid;

#[tokio::test]
async fn milestone2_workflows_are_durable_and_searchable() {
    let Some(database_url) = std::env::var("GOBROWSE_TEST_DATABASE_URL").ok() else {
        eprintln!("GOBROWSE_TEST_DATABASE_URL is unset; skipping Milestone 2 integration test");
        return;
    };
    let pool = PgPool::connect(&database_url)
        .await
        .expect("connect to test PostgreSQL");
    db::migrate(&pool).await.expect("apply migrations");
    let (profile_id, user_id, cookie) = authenticated_profile(&pool).await;
    let state = AppState::new(pool.clone(), test_settings(&database_url))
        .await
        .expect("create app state");
    let app = router(state.clone());

    let (status, conversation) = request_json(
        &app,
        Method::POST,
        "/api/v1/conversations",
        &cookie,
        Some(json!({"title":"Durable conversation","workspace_id":null})),
    )
    .await;
    assert_eq!(status, StatusCode::CREATED, "{conversation}");
    let conversation_id = uuid_field(&conversation, "id");
    let (status, message) = request_json(
        &app,
        Method::POST,
        &format!("/api/v1/conversations/{conversation_id}/messages"),
        &cookie,
        Some(json!({
            "role":"user",
            "text":"Rootless sandboxes preserve durable terminal state",
            "provider":null,
            "model":null,
            "usage":null,
            "tool_calls":null
        })),
    )
    .await;
    assert_eq!(status, StatusCode::CREATED, "{message}");
    let projection = sqlx::query(
        "SELECT id,body,revision FROM books WHERE conversation_id=$1 AND book_type='CONVERSATION'",
    )
    .bind(conversation_id)
    .fetch_one(&pool)
    .await
    .expect("read conversation projection");
    assert!(projection.get::<String, _>("body").contains("Rootless"));
    assert_eq!(projection.get::<i64, _>("revision"), 2);
    let projection_id: Uuid = projection.get("id");
    let (_, member_cookie) = authenticated_user(&pool, profile_id, "MEMBER").await;
    let (status, managed_update) = request_json(
        &app,
        Method::PUT,
        &format!("/api/v1/library/books/{projection_id}"),
        &member_cookie,
        Some(json!({
            "title":"Forged projection",
            "body":"This must not replace durable messages",
            "tags":[],
            "metadata":{},
            "expected_revision":2,
            "reason":"Attempt managed-book bypass"
        })),
    )
    .await;
    assert_eq!(status, StatusCode::NOT_FOUND, "{managed_update}");
    let (status, forged_trust) = request_json(
        &app,
        Method::POST,
        "/api/v1/library/books",
        &member_cookie,
        Some(json!({
            "title":"Forged trust",
            "body":"Member-authored data",
            "book_type":"NOTE",
            "scope":"PROFILE",
            "tags":[],
            "provenance":"SYSTEM",
            "trust":"VERIFIED",
            "workspace_id":null,
            "conversation_id":null,
            "security_classification":"INTERNAL",
            "metadata":{}
        })),
    )
    .await;
    assert_eq!(status, StatusCode::FORBIDDEN, "{forged_trust}");

    let (status, fork) = request_json(
        &app,
        Method::POST,
        &format!("/api/v1/conversations/{conversation_id}/fork"),
        &cookie,
        Some(json!({"title":"Durable fork","at_message_id":message["id"]})),
    )
    .await;
    assert_eq!(status, StatusCode::CREATED, "{fork}");
    let fork_id = uuid_field(&fork, "id");
    let (status, _) = request_json(
        &app,
        Method::DELETE,
        &format!("/api/v1/conversations/{conversation_id}"),
        &cookie,
        None,
    )
    .await;
    assert_eq!(status, StatusCode::NO_CONTENT);
    let source_projection_exists: bool =
        sqlx::query_scalar("SELECT EXISTS(SELECT 1 FROM books WHERE conversation_id=$1)")
            .bind(conversation_id)
            .fetch_one(&pool)
            .await
            .expect("check deleted projection");
    assert!(!source_projection_exists);
    let fork_messages: i64 =
        sqlx::query_scalar("SELECT count(*) FROM messages WHERE conversation_id=$1")
            .bind(fork_id)
            .fetch_one(&pool)
            .await
            .expect("count fork messages");
    assert_eq!(fork_messages, 1);

    let autobiography_id = Uuid::now_v7();
    sqlx::query(
        "INSERT INTO books (id,profile_id,title,body,book_type,scope,provenance,trust,author,security_classification) \
         VALUES ($1,$2,'Autobiography','Initial identity','AUTOBIOGRAPHY','PROFILE','USER','USER_PROVIDED','Test Owner','CONFIDENTIAL')",
    )
    .bind(autobiography_id)
    .bind(profile_id)
    .execute(&pool)
    .await
    .expect("create autobiography");
    sqlx::query(
        "INSERT INTO book_revisions (id,book_id,revision,title,body,tags,metadata,changed_by,change_reason) \
         VALUES ($1,$2,1,'Autobiography','Initial identity','{}','{}',$3,'Initial autobiography')",
    )
    .bind(Uuid::now_v7())
    .bind(autobiography_id)
    .bind(user_id)
    .execute(&pool)
    .await
    .expect("snapshot autobiography");
    let (status, proposal) = request_json(
        &app,
        Method::POST,
        "/api/v1/autobiography/proposals",
        &cookie,
        Some(json!({
            "after_body":"Proposed identity",
            "reason":"Learned from verified interaction",
            "source_book_ids":[],
            "source_conversation_ids":[fork_id]
        })),
    )
    .await;
    assert_eq!(status, StatusCode::CREATED, "{proposal}");
    let proposal_id = uuid_field(&proposal, "id");
    let (status, _) = request_json(
        &app,
        Method::PUT,
        "/api/v1/autobiography/policy",
        &cookie,
        Some(json!({"policy":"manual"})),
    )
    .await;
    assert_eq!(status, StatusCode::NO_CONTENT);
    let (status, manual_review) = request_json(
        &app,
        Method::POST,
        &format!("/api/v1/autobiography/proposals/{proposal_id}/review"),
        &cookie,
        Some(json!({"decision":"accept","reason":"Must respect manual policy"})),
    )
    .await;
    assert_eq!(status, StatusCode::CONFLICT, "{manual_review}");
    let (status, _) = request_json(
        &app,
        Method::PUT,
        "/api/v1/autobiography/policy",
        &cookie,
        Some(json!({"policy":"propose"})),
    )
    .await;
    assert_eq!(status, StatusCode::NO_CONTENT);
    sqlx::query("UPDATE books SET revision=2,body='Concurrent identity' WHERE id=$1")
        .bind(autobiography_id)
        .execute(&pool)
        .await
        .expect("simulate concurrent autobiography edit");
    let (status, stale_review) = request_json(
        &app,
        Method::POST,
        &format!("/api/v1/autobiography/proposals/{proposal_id}/review"),
        &cookie,
        Some(json!({"decision":"accept","reason":"Accept verified update"})),
    )
    .await;
    assert_eq!(status, StatusCode::CONFLICT, "{stale_review}");
    let (status, rejection) = request_json(
        &app,
        Method::POST,
        &format!("/api/v1/autobiography/proposals/{proposal_id}/review"),
        &cookie,
        Some(json!({"decision":"reject","reason":"Superseded by newer identity data"})),
    )
    .await;
    assert_eq!(status, StatusCode::OK, "{rejection}");

    let (embedding_url, fake_server) = fake_embedding_server().await;
    let worker_cancel = CancellationToken::new();
    let worker = tokio::spawn(embedding::run_worker(
        state.clone(),
        worker_cancel.child_token(),
    ));
    let (status, configuration) = request_json(
        &app,
        Method::POST,
        "/api/v1/embeddings/configurations",
        &cookie,
        Some(json!({
            "display_name":"Deterministic test embeddings",
            "provider_type":"ollama",
            "base_url":embedding_url,
            "secret_reference":null,
            "model_reference":"test-embedding-3",
            "dimensions":3,
            "activate":true
        })),
    )
    .await;
    assert_eq!(status, StatusCode::CREATED, "{configuration}");
    let model_id = configuration["id"].as_str().expect("model id");
    let (_, viewer_cookie) = authenticated_user(&pool, profile_id, "VIEWER").await;
    let (status, _) = request_json(
        &app,
        Method::GET,
        "/api/v1/embeddings/configurations",
        &viewer_cookie,
        None,
    )
    .await;
    assert_eq!(status, StatusCode::FORBIDDEN);
    let (status, book) = request_json(
        &app,
        Method::POST,
        "/api/v1/library/books",
        &cookie,
        Some(json!({
            "title":"Semantic-only target",
            "body":"Chromatic observability signals reveal scheduler pressure",
            "book_type":"DOCUMENT",
            "scope":"PROFILE",
            "tags":["operations"],
            "provenance":"USER",
            "trust":"VERIFIED",
            "workspace_id":null,
            "conversation_id":null,
            "security_classification":"INTERNAL",
            "metadata":{}
        })),
    )
    .await;
    assert_eq!(status, StatusCode::OK, "{book}");
    let book_id = uuid_field(&book, "id");
    let (status, trusted_overwrite) = request_json(
        &app,
        Method::PUT,
        &format!("/api/v1/library/books/{book_id}"),
        &member_cookie,
        Some(json!({
            "title":"Trusted content overwritten by member",
            "body":"Attacker-controlled replacement",
            "tags":[],
            "metadata":{},
            "expected_revision":1,
            "reason":"Attempt trusted-content overwrite"
        })),
    )
    .await;
    assert_eq!(status, StatusCode::NOT_FOUND, "{trusted_overwrite}");
    wait_for_embedding(&pool, book_id, model_id).await;
    let (status, search) = request_json(
        &app,
        Method::GET,
        "/api/v1/library/search?q=words-not-present-lexically&limit=100",
        &cookie,
        None,
    )
    .await;
    assert_eq!(status, StatusCode::OK, "{search}");
    let result = search
        .as_array()
        .expect("search result array")
        .iter()
        .find(|candidate| candidate["id"] == book_id.to_string())
        .expect("semantic result includes target Book");
    assert_eq!(result["retrieval_mode"], "semantic");
    assert!(result["semantic_score"].is_number());

    worker_cancel.cancel();
    tokio::time::timeout(StdDuration::from_secs(5), worker)
        .await
        .expect("worker stops")
        .expect("worker task succeeds");
    fake_server.abort();
}

async fn authenticated_profile(pool: &PgPool) -> (Uuid, Uuid, String) {
    let profile_id = Uuid::now_v7();
    sqlx::query("INSERT INTO profiles (id,name) VALUES ($1,$2)")
        .bind(profile_id)
        .bind(format!("Milestone 2 test {profile_id}"))
        .execute(pool)
        .await
        .expect("create test profile");
    let (user_id, cookie) = authenticated_user(pool, profile_id, "OWNER").await;
    (profile_id, user_id, cookie)
}

async fn authenticated_user(pool: &PgPool, profile_id: Uuid, role: &str) -> (Uuid, String) {
    let user_id = Uuid::now_v7();
    let token = format!("test-session-{}", Uuid::now_v7());
    sqlx::query(
        "INSERT INTO users (id,email,display_name,password_hash,role,primary_profile_id) \
         VALUES ($1,$2,'Test User','unused',$3,$4)",
    )
    .bind(user_id)
    .bind(format!("{user_id}@example.test"))
    .bind(role)
    .bind(profile_id)
    .execute(pool)
    .await
    .expect("create test user");
    let now = OffsetDateTime::now_utc();
    sqlx::query(
        "INSERT INTO sessions (token_hash,user_id,auth_epoch,expires_at,absolute_expires_at) VALUES ($1,$2,1,$3,$4)",
    )
    .bind(Sha256::digest(token.as_bytes()).to_vec())
    .bind(user_id)
    .bind(now + Duration::hours(1))
    .bind(now + Duration::hours(2))
    .execute(pool)
    .await
    .expect("create test session");
    (user_id, format!("gobrowse_session={token}"))
}

fn test_settings(database_url: &str) -> Settings {
    Settings {
        http: HttpSettings::default(),
        database: DatabaseSettings {
            url: SecretString::from(database_url.to_owned()),
            max_connections: 10,
        },
        auth: AuthSettings::default(),
        vault: VaultSettings::default(),
        features: FeatureSettings {
            local_embeddings: true,
            ..FeatureSettings::default()
        },
        observability: ObservabilitySettings::default(),
    }
}

async fn request_json(
    app: &Router,
    method: Method,
    uri: &str,
    cookie: &str,
    body: Option<Value>,
) -> (StatusCode, Value) {
    let mut builder = Request::builder()
        .method(method)
        .uri(uri)
        .header(header::COOKIE, cookie);
    let body = if let Some(body) = body {
        builder = builder.header(header::CONTENT_TYPE, "application/json");
        Body::from(body.to_string())
    } else {
        Body::empty()
    };
    let response = app
        .clone()
        .oneshot(builder.body(body).expect("build request"))
        .await
        .expect("route request");
    let status = response.status();
    let bytes = response
        .into_body()
        .collect()
        .await
        .expect("collect response")
        .to_bytes();
    let value = if bytes.is_empty() {
        Value::Null
    } else {
        serde_json::from_slice(&bytes).expect("JSON response")
    };
    (status, value)
}

fn uuid_field(value: &Value, field: &str) -> Uuid {
    Uuid::parse_str(value[field].as_str().expect("UUID string")).expect("valid UUID")
}

async fn fake_embedding_server() -> (String, tokio::task::JoinHandle<()>) {
    async fn embeddings(Json(request): Json<Value>) -> Json<Value> {
        let count = request["input"].as_array().expect("embedding inputs").len();
        Json(json!({
            "embeddings": (0..count).map(|_| json!([1.0,0.5,0.25])).collect::<Vec<_>>()
        }))
    }
    let listener = TcpListener::bind("127.0.0.1:0")
        .await
        .expect("bind fake embedding server");
    let address = listener.local_addr().expect("fake server address");
    let server = tokio::spawn(async move {
        axum::serve(
            listener,
            Router::new().route("/v1/api/embed", post(embeddings)),
        )
        .await
        .expect("serve fake embeddings");
    });
    (format!("http://{address}/v1"), server)
}

async fn wait_for_embedding(pool: &PgPool, book_id: Uuid, model_id: &str) {
    for _ in 0..100 {
        let completed: bool = sqlx::query_scalar(
            "SELECT EXISTS(SELECT 1 FROM embedding_jobs WHERE book_id=$1 AND embedding_model_id=$2 AND status='completed')",
        )
        .bind(book_id)
        .bind(model_id)
        .fetch_one(pool)
        .await
        .expect("check embedding job");
        if completed {
            return;
        }
        tokio::time::sleep(StdDuration::from_millis(100)).await;
    }
    let rows = sqlx::query(
        "SELECT status,last_error_code,last_error_detail FROM embedding_jobs WHERE book_id=$1",
    )
    .bind(book_id)
    .fetch_all(pool)
    .await
    .expect("read failed jobs");
    panic!(
        "embedding did not complete: {:?}",
        rows.iter()
            .map(|row| (
                row.get::<String, _>("status"),
                row.get::<Option<String>, _>("last_error_code"),
                row.get::<Option<String>, _>("last_error_detail")
            ))
            .collect::<Vec<_>>()
    );
}
