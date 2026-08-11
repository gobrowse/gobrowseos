use std::time::Duration as StdDuration;

use axum::{
    Json, Router,
    body::Body,
    http::{Method, Request, StatusCode, header},
    response::{IntoResponse, Response},
    routing::post,
};
use futures_util::{StreamExt, stream};
use gobrowse_server::{
    AppState,
    config::{
        AuthSettings, DatabaseSettings, FeatureSettings, HttpSettings, ObservabilitySettings,
        Settings, VaultSettings,
    },
    db, embedding, router, run_api,
};
use http_body_util::BodyExt;
use secrecy::SecretString;
use serde_json::{Value, json};
use sha2::{Digest, Sha256};
use sqlx::{PgPool, Row};
use time::{Duration, OffsetDateTime};
use tokio::net::{TcpListener, TcpStream};
use tokio_tungstenite::{
    MaybeTlsStream, WebSocketStream, connect_async, tungstenite::client::IntoClientRequest,
};
use tokio_util::sync::CancellationToken;
use tower::ServiceExt;
use uuid::Uuid;

#[tokio::test]
async fn milestone3_workflows_are_durable_and_searchable() {
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
    let run_worker_cancel = CancellationToken::new();
    let run_worker = tokio::spawn(run_api::run_worker(
        state.clone(),
        run_worker_cancel.child_token(),
    ));

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
    let (member_id, member_cookie) = authenticated_user(&pool, profile_id, "MEMBER").await;
    let (status, workspace) = request_json(
        &app,
        Method::POST,
        "/api/v1/workspaces",
        &cookie,
        Some(json!({"title":"Isolated workspace","description":"Membership scoped"})),
    )
    .await;
    assert_eq!(status, StatusCode::OK, "{workspace}");
    let workspace_id = uuid_field(&workspace, "id");
    let (status, workspace_book) = request_json(
        &app,
        Method::POST,
        "/api/v1/library/books",
        &cookie,
        Some(json!({
            "title":"Workspace secret","body":"WORKSPACE_ONLY_SENTINEL","book_type":"DOCUMENT",
            "scope":"WORKSPACE","tags":[],"provenance":"USER","trust":"USER_PROVIDED",
            "workspace_id":workspace_id,"conversation_id":null,"security_classification":"INTERNAL","metadata":{}
        })),
    )
    .await;
    assert_eq!(status, StatusCode::OK, "{workspace_book}");
    let workspace_book_id = uuid_field(&workspace_book, "id");
    let (status, hidden_workspace_book) = request_json(
        &app,
        Method::GET,
        &format!("/api/v1/library/books/{workspace_book_id}"),
        &member_cookie,
        None,
    )
    .await;
    assert_eq!(status, StatusCode::NOT_FOUND, "{hidden_workspace_book}");
    sqlx::query(
        "INSERT INTO workspace_memberships (workspace_id,user_id,access) VALUES ($1,$2,'VIEWER')",
    )
    .bind(workspace_id)
    .bind(member_id)
    .execute(&pool)
    .await
    .expect("grant workspace view");
    let (status, visible_workspace_book) = request_json(
        &app,
        Method::GET,
        &format!("/api/v1/library/books/{workspace_book_id}"),
        &member_cookie,
        None,
    )
    .await;
    assert_eq!(status, StatusCode::OK, "{visible_workspace_book}");
    let (status, viewer_conversation) = request_json(
        &app,
        Method::POST,
        "/api/v1/conversations",
        &member_cookie,
        Some(json!({"title":"Viewer conversation","workspace_id":workspace_id})),
    )
    .await;
    assert_eq!(status, StatusCode::FORBIDDEN, "{viewer_conversation}");
    let (status, viewer_write) = request_json(
        &app,
        Method::POST,
        "/api/v1/library/books",
        &member_cookie,
        Some(json!({
            "title":"Viewer write","body":"must fail","book_type":"DOCUMENT","scope":"WORKSPACE",
            "tags":[],"provenance":"USER","trust":"USER_PROVIDED","workspace_id":workspace_id,
            "conversation_id":null,"security_classification":"INTERNAL","metadata":{}
        })),
    )
    .await;
    assert_eq!(status, StatusCode::FORBIDDEN, "{viewer_write}");
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
            "base_url":embedding_url.clone(),
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
    let (status, viewer_workspace) = request_json(
        &app,
        Method::POST,
        "/api/v1/workspaces",
        &viewer_cookie,
        Some(json!({"title":"Viewer workspace","description":"must fail"})),
    )
    .await;
    assert_eq!(status, StatusCode::FORBIDDEN, "{viewer_workspace}");
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

    let (status, successful_model) = request_json(
        &app,
        Method::POST,
        "/api/v1/models/chat",
        &cookie,
        Some(json!({
            "display_name":"Deterministic chat",
            "provider_type":"ollama",
            "base_url":embedding_url.clone(),
            "secret_reference":null,
            "model_reference":"success-model",
            "context_window":8192,
            "output_limit":1024,
            "priority":0,
            "activate":false
        })),
    )
    .await;
    assert_eq!(status, StatusCode::CREATED, "{successful_model}");
    let successful_model_id = successful_model["id"]
        .as_str()
        .expect("successful model id");
    let (status, failing_model) = request_json(
        &app,
        Method::POST,
        "/api/v1/models/chat",
        &cookie,
        Some(json!({
            "display_name":"Rate-limited chat",
            "provider_type":"ollama",
            "base_url":embedding_url.clone(),
            "secret_reference":null,
            "model_reference":"fail-model",
            "context_window":8192,
            "output_limit":1024,
            "priority":100,
            "activate":true,
            "fallback_model_ids":[successful_model_id]
        })),
    )
    .await;
    assert_eq!(status, StatusCode::CREATED, "{failing_model}");
    let (status, user_message) = request_json(
        &app,
        Method::POST,
        &format!("/api/v1/conversations/{fork_id}/messages"),
        &cookie,
        Some(json!({
            "role":"user","text":"Explain the durable execution result",
            "provider":null,"model":null,"usage":null,"tool_calls":null
        })),
    )
    .await;
    assert_eq!(status, StatusCode::CREATED, "{user_message}");
    let (status, run) = request_json(
        &app,
        Method::POST,
        &format!("/api/v1/conversations/{fork_id}/runs"),
        &cookie,
        Some(json!({"input_message_id":user_message["id"],"model_id":null})),
    )
    .await;
    assert_eq!(status, StatusCode::ACCEPTED, "{run}");
    let run_id = uuid_field(&run, "id");
    assert_eq!(wait_for_run(&pool, run_id).await, "completed");
    let selected_model: String =
        sqlx::query_scalar("SELECT selected_model_id FROM agent_runs WHERE id=$1")
            .bind(run_id)
            .fetch_one(&pool)
            .await
            .expect("read selected fallback model");
    assert_eq!(selected_model, successful_model_id);
    let completed_lease_cleared: bool = sqlx::query_scalar(
        "SELECT execution_token IS NULL AND lease_expires_at IS NULL FROM agent_runs WHERE id=$1",
    )
    .bind(run_id)
    .fetch_one(&pool)
    .await
    .expect("check completed run lease");
    assert!(completed_lease_cleared);
    let assistant_text: String = sqlx::query_scalar(
        "SELECT content->>'text' FROM messages WHERE agent_run_id=$1 AND role='assistant'",
    )
    .bind(run_id)
    .fetch_one(&pool)
    .await
    .expect("read correlated assistant message");
    assert_eq!(assistant_text, "Deterministic assistant answer 🦀");
    let (status, events) = request_json(
        &app,
        Method::GET,
        &format!("/api/v1/runs/{run_id}/events"),
        &cookie,
        None,
    )
    .await;
    assert_eq!(status, StatusCode::OK, "{events}");
    let event_types: Vec<_> = events
        .as_array()
        .expect("run event array")
        .iter()
        .filter_map(|event| event["event_type"].as_str())
        .collect();
    assert!(event_types.contains(&"model.text_delta"));
    assert!(event_types.contains(&"model.usage"));
    assert!(event_types.contains(&"run.completed"));
    let context_event = events
        .as_array()
        .expect("run event array")
        .iter()
        .find(|event| event["event_type"] == "run.context_built")
        .expect("context event");
    assert!(context_event["payload"].get("selected").is_none());
    assert!(context_event["payload"].get("omitted").is_none());
    let replay_cursor: i64 = events
        .as_array()
        .expect("run event array")
        .iter()
        .map(|event| event["sequence"].as_i64().expect("event sequence"))
        .max()
        .expect("latest sequence");
    let websocket_listener = TcpListener::bind("127.0.0.1:0")
        .await
        .expect("bind websocket test server");
    let websocket_address = websocket_listener.local_addr().expect("websocket address");
    let websocket_server = tokio::spawn({
        let websocket_app = app.clone();
        async move {
            axum::serve(websocket_listener, websocket_app)
                .await
                .expect("serve websocket app")
        }
    });
    let mut request = format!(
        "ws://{websocket_address}/api/v1/runs/{run_id}/realtime?after={}",
        replay_cursor - 1
    )
    .into_client_request()
    .expect("websocket request");
    request
        .headers_mut()
        .insert(header::ORIGIN, "http://localhost:8080".parse().unwrap());
    request
        .headers_mut()
        .insert(header::COOKIE, cookie.parse().unwrap());
    let (mut socket, _) = connect_async(request).await.expect("connect websocket");
    let hello = next_websocket_json(&mut socket).await;
    assert_eq!(hello["kind"], "connected");
    let replayed = next_websocket_json(&mut socket).await;
    assert_eq!(replayed["sequence"], replay_cursor);
    socket.close(None).await.expect("close websocket");
    sqlx::query("INSERT INTO run_events (run_id,profile_id,event_type,payload) VALUES ($1,$2,'test.reconnect','{}')")
        .bind(run_id).bind(profile_id).execute(&pool).await.expect("append reconnect event");
    let new_sequence: i64 =
        sqlx::query_scalar("SELECT max(sequence) FROM run_events WHERE run_id=$1")
            .bind(run_id)
            .fetch_one(&pool)
            .await
            .expect("read reconnect sequence");
    let mut reconnect_request =
        format!("ws://{websocket_address}/api/v1/runs/{run_id}/realtime?after={replay_cursor}")
            .into_client_request()
            .expect("reconnect request");
    reconnect_request
        .headers_mut()
        .insert(header::ORIGIN, "http://localhost:8080".parse().unwrap());
    reconnect_request
        .headers_mut()
        .insert(header::COOKIE, cookie.parse().unwrap());
    let (mut reconnected, _) = connect_async(reconnect_request)
        .await
        .expect("reconnect websocket");
    let hello = next_websocket_json(&mut reconnected).await;
    assert_eq!(hello["kind"], "connected");
    let delivered = next_websocket_json(&mut reconnected).await;
    assert_eq!(delivered["sequence"], new_sequence);
    reconnected
        .close(None)
        .await
        .expect("close reconnected websocket");
    websocket_server.abort();

    let (status, hidden_conversation) = request_json(
        &app,
        Method::GET,
        &format!("/api/v1/conversations/{fork_id}"),
        &member_cookie,
        None,
    )
    .await;
    assert_eq!(status, StatusCode::NOT_FOUND, "{hidden_conversation}");
    let (status, hidden_run) = request_json(
        &app,
        Method::GET,
        &format!("/api/v1/runs/{run_id}"),
        &member_cookie,
        None,
    )
    .await;
    assert_eq!(status, StatusCode::NOT_FOUND, "{hidden_run}");
    let (status, shared_conversation) = request_json(
        &app,
        Method::POST,
        "/api/v1/conversations",
        &cookie,
        Some(json!({"title":"Shared run visibility","workspace_id":workspace_id})),
    )
    .await;
    assert_eq!(status, StatusCode::CREATED, "{shared_conversation}");
    let shared_conversation_id = uuid_field(&shared_conversation, "id");
    let (status, shared_message) = request_json(
        &app,
        Method::POST,
        &format!("/api/v1/conversations/{shared_conversation_id}/messages"),
        &cookie,
        Some(json!({
            "role":"user","text":"Visible shared run","provider":null,"model":null,
            "usage":null,"tool_calls":null
        })),
    )
    .await;
    assert_eq!(status, StatusCode::CREATED, "{shared_message}");
    let shared_message_id = uuid_field(&shared_message, "id");
    let shared_run_id = Uuid::now_v7();
    sqlx::query(
        "INSERT INTO agent_runs (id,profile_id,conversation_id,requested_by,input_message_id,state,run_kind,execution_owner,execution_token,lease_expires_at) \
         VALUES ($1,$2,$3,$4,$5,'queued','conversation_turn','visibility-test',gen_random_uuid(),now()+interval '1 hour')",
    )
    .bind(shared_run_id)
    .bind(profile_id)
    .bind(shared_conversation_id)
    .bind(user_id)
    .bind(shared_message_id)
    .execute(&pool)
    .await
    .expect("create leased shared run");
    sqlx::query(
        "INSERT INTO run_events (run_id,profile_id,event_type,payload) VALUES ($1,$2,'run.queued','{}')",
    )
    .bind(shared_run_id)
    .bind(profile_id)
    .execute(&pool)
    .await
    .expect("create shared run event");
    let (status, shared_run) = request_json(
        &app,
        Method::GET,
        &format!("/api/v1/runs/{shared_run_id}"),
        &member_cookie,
        None,
    )
    .await;
    assert_eq!(status, StatusCode::OK, "{shared_run}");
    let (status, shared_active_run) = request_json(
        &app,
        Method::GET,
        &format!("/api/v1/conversations/{shared_conversation_id}/runs"),
        &member_cookie,
        None,
    )
    .await;
    assert_eq!(status, StatusCode::OK, "{shared_active_run}");
    assert_eq!(shared_active_run["id"], shared_run_id.to_string());
    let (status, shared_events) = request_json(
        &app,
        Method::GET,
        &format!("/api/v1/runs/{shared_run_id}/events"),
        &member_cookie,
        None,
    )
    .await;
    assert_eq!(status, StatusCode::OK, "{shared_events}");
    sqlx::query(
        "UPDATE agent_runs SET state='canceled',execution_owner=NULL,execution_token=NULL,lease_expires_at=NULL,finished_at=now() WHERE id=$1",
    )
    .bind(shared_run_id)
    .execute(&pool)
    .await
    .expect("retire shared visibility run");
    sqlx::query(
        "UPDATE workspace_memberships SET access='EDITOR' WHERE workspace_id=$1 AND user_id=$2",
    )
    .bind(workspace_id)
    .bind(member_id)
    .execute(&pool)
    .await
    .expect("grant temporary shared write access");
    let revoked_run_id = Uuid::now_v7();
    sqlx::query(
        "INSERT INTO agent_runs (id,profile_id,conversation_id,requested_by,state,run_kind,execution_owner,execution_token,lease_expires_at) \
         VALUES ($1,$2,$3,$4,'queued','conversation_turn','revocation-test',gen_random_uuid(),now()+interval '1 hour')",
    )
    .bind(revoked_run_id)
    .bind(profile_id)
    .bind(shared_conversation_id)
    .bind(member_id)
    .execute(&pool)
    .await
    .expect("create run before membership revocation");
    sqlx::query("DELETE FROM workspace_memberships WHERE workspace_id=$1 AND user_id=$2")
        .bind(workspace_id)
        .bind(member_id)
        .execute(&pool)
        .await
        .expect("revoke shared access");
    let (status, revoked_cancel) = request_json(
        &app,
        Method::POST,
        &format!("/api/v1/runs/{revoked_run_id}/cancel"),
        &member_cookie,
        None,
    )
    .await;
    assert_eq!(status, StatusCode::NOT_FOUND, "{revoked_cancel}");
    sqlx::query(
        "UPDATE agent_runs SET state='canceled',execution_owner=NULL,execution_token=NULL,lease_expires_at=NULL,finished_at=now() WHERE id=$1",
    )
    .bind(revoked_run_id)
    .execute(&pool)
    .await
    .expect("retire revoked run");
    let legacy_agent_id = Uuid::now_v7();
    sqlx::query(
        "INSERT INTO agents (id,workspace_id,name,kind,permissions,status) VALUES ($1,$2,'History agent','coding','{}','paused')",
    )
    .bind(legacy_agent_id)
    .bind(workspace_id)
    .execute(&pool)
    .await
    .expect("create history agent");
    let legacy_run_id = Uuid::now_v7();
    sqlx::query(
        "INSERT INTO agent_runs (id,agent_id,profile_id,conversation_id,state,run_kind) VALUES ($1,$2,$3,$4,'paused','agent')",
    )
    .bind(legacy_run_id)
    .bind(legacy_agent_id)
    .bind(profile_id)
    .bind(shared_conversation_id)
    .execute(&pool)
    .await
    .expect("create legacy conversation-linked run");
    let (status, _) = request_json(
        &app,
        Method::DELETE,
        &format!("/api/v1/conversations/{shared_conversation_id}"),
        &cookie,
        None,
    )
    .await;
    assert_eq!(status, StatusCode::NO_CONTENT);
    let legacy_conversation: Option<Uuid> =
        sqlx::query_scalar("SELECT conversation_id FROM agent_runs WHERE id=$1")
            .bind(legacy_run_id)
            .fetch_one(&pool)
            .await
            .expect("read preserved legacy run");
    assert_eq!(legacy_conversation, None);
    let deleted_turn_exists: bool =
        sqlx::query_scalar("SELECT EXISTS(SELECT 1 FROM agent_runs WHERE id=$1)")
            .bind(revoked_run_id)
            .fetch_one(&pool)
            .await
            .expect("check deleted conversation turn");
    assert!(!deleted_turn_exists);
    let (status, unauthenticated_run) = request_json(
        &app,
        Method::GET,
        &format!("/api/v1/runs/{run_id}"),
        "",
        None,
    )
    .await;
    assert_eq!(status, StatusCode::UNAUTHORIZED, "{unauthenticated_run}");
    let (_, _, foreign_cookie) = authenticated_profile(&pool).await;
    let (status, foreign_run) = request_json(
        &app,
        Method::GET,
        &format!("/api/v1/runs/{run_id}"),
        &foreign_cookie,
        None,
    )
    .await;
    assert_eq!(status, StatusCode::NOT_FOUND, "{foreign_run}");
    let (status, foreign_events) = request_json(
        &app,
        Method::GET,
        &format!("/api/v1/runs/{run_id}/events"),
        &foreign_cookie,
        None,
    )
    .await;
    assert_eq!(status, StatusCode::NOT_FOUND, "{foreign_events}");
    let (status, member_model) = request_json(
        &app,
        Method::POST,
        "/api/v1/models/chat",
        &member_cookie,
        Some(json!({
            "display_name":"Unauthorized model","provider_type":"ollama","base_url":embedding_url.clone(),
            "secret_reference":null,"model_reference":"forbidden","context_window":8192,
            "output_limit":1024,"priority":0,"activate":false
        })),
    )
    .await;
    assert_eq!(status, StatusCode::FORBIDDEN, "{member_model}");
    let (status, unauthorized_start) = request_json(
        &app,
        Method::POST,
        &format!("/api/v1/conversations/{fork_id}/runs"),
        &member_cookie,
        Some(json!({"input_message_id":user_message["id"],"model_id":null})),
    )
    .await;
    assert_eq!(status, StatusCode::NOT_FOUND, "{unauthorized_start}");

    let submission_id = Uuid::now_v7();
    let turn_path = format!("/api/v1/conversations/{fork_id}/turns");
    let turn_body = json!({
        "client_submission_id":submission_id,
        "text":"An idempotent turn is published once",
        "model_id":successful_model_id
    });
    let (first_turn, second_turn) = tokio::join!(
        request_json(
            &app,
            Method::POST,
            &turn_path,
            &cookie,
            Some(turn_body.clone())
        ),
        request_json(&app, Method::POST, &turn_path, &cookie, Some(turn_body)),
    );
    assert!(matches!(
        first_turn.0,
        StatusCode::ACCEPTED | StatusCode::OK
    ));
    assert!(matches!(
        second_turn.0,
        StatusCode::ACCEPTED | StatusCode::OK
    ));
    assert_eq!(first_turn.1["run"]["id"], second_turn.1["run"]["id"]);
    let idempotent_run_id = uuid_field(&first_turn.1["run"], "id");
    assert_eq!(wait_for_run(&pool, idempotent_run_id).await, "completed");
    let idempotent_count: i64 = sqlx::query_scalar(
        "SELECT count(*) FROM agent_runs WHERE conversation_id=$1 AND client_submission_id=$2",
    )
    .bind(fork_id)
    .bind(submission_id)
    .fetch_one(&pool)
    .await
    .expect("count idempotent runs");
    assert_eq!(idempotent_count, 1);

    let distinct_a = json!({"client_submission_id":Uuid::now_v7(),"text":"First competing turn","model_id":successful_model_id});
    let distinct_b = json!({"client_submission_id":Uuid::now_v7(),"text":"Second competing turn","model_id":successful_model_id});
    let before_messages: i64 =
        sqlx::query_scalar("SELECT count(*) FROM messages WHERE conversation_id=$1")
            .bind(fork_id)
            .fetch_one(&pool)
            .await
            .expect("count messages before race");
    let (race_a, race_b) = tokio::join!(
        request_json(&app, Method::POST, &turn_path, &cookie, Some(distinct_a)),
        request_json(&app, Method::POST, &turn_path, &cookie, Some(distinct_b)),
    );
    assert_eq!(
        usize::from(race_a.0.is_success()) + usize::from(race_b.0.is_success()),
        1
    );
    assert!(race_a.0 == StatusCode::CONFLICT || race_b.0 == StatusCode::CONFLICT);
    let accepted = if race_a.0.is_success() {
        &race_a.1
    } else {
        &race_b.1
    };
    assert_eq!(
        wait_for_run(&pool, uuid_field(&accepted["run"], "id")).await,
        "completed"
    );
    let after_messages: i64 =
        sqlx::query_scalar("SELECT count(*) FROM messages WHERE conversation_id=$1")
            .bind(fork_id)
            .fetch_one(&pool)
            .await
            .expect("count messages after race");
    assert_eq!(after_messages, before_messages + 2);

    run_worker_cancel.cancel();
    tokio::time::timeout(StdDuration::from_secs(5), run_worker)
        .await
        .expect("initial run worker stops")
        .expect("initial run worker task succeeds");
    let recovery_worker_cancel = CancellationToken::new();
    let recovery_worker = tokio::spawn(run_api::run_worker(
        state.clone(),
        recovery_worker_cancel.child_token(),
    ));

    let (status, slow_model) = request_json(
        &app,
        Method::POST,
        "/api/v1/models/chat",
        &cookie,
        Some(json!({
            "display_name":"Cancelable chat",
            "provider_type":"ollama",
            "base_url":embedding_url.clone(),
            "secret_reference":null,
            "model_reference":"slow-model",
            "context_window":8192,
            "output_limit":1024,
            "priority":200,
            "activate":true
        })),
    )
    .await;
    assert_eq!(status, StatusCode::CREATED, "{slow_model}");
    let slow_model_id = slow_model["id"].as_str().expect("slow model id");
    let shutdown_submission = Uuid::now_v7();
    let (status, shutdown_run) = request_json(
        &app,
        Method::POST,
        &format!("/api/v1/conversations/{fork_id}/turns"),
        &cookie,
        Some(json!({
            "client_submission_id":shutdown_submission,
            "text":"The owning instance will stop mid-stream",
            "model_id":slow_model_id
        })),
    )
    .await;
    assert_eq!(status, StatusCode::ACCEPTED, "{shutdown_run}");
    let shutdown_run_id = uuid_field(&shutdown_run["run"], "id");
    wait_for_event(&pool, shutdown_run_id, "model.text_delta").await;
    recovery_worker_cancel.cancel();
    tokio::time::timeout(StdDuration::from_secs(5), recovery_worker)
        .await
        .expect("recovery worker stops mid-stream")
        .expect("recovery worker task succeeds");
    wait_for_released_lease(&pool, shutdown_run_id).await;
    let takeover_worker_cancel = CancellationToken::new();
    let takeover_worker = tokio::spawn(run_api::run_worker(
        state.clone(),
        takeover_worker_cancel.child_token(),
    ));
    assert_eq!(wait_for_run(&pool, shutdown_run_id).await, "completed");
    let reset_events: i64 = sqlx::query_scalar(
        "SELECT count(*) FROM run_events WHERE run_id=$1 AND event_type='run.output_reset'",
    )
    .bind(shutdown_run_id)
    .fetch_one(&pool)
    .await
    .expect("count output reset events");
    assert_eq!(reset_events, 1);
    let shutdown_outputs: i64 =
        sqlx::query_scalar("SELECT count(*) FROM messages WHERE agent_run_id=$1")
            .bind(shutdown_run_id)
            .fetch_one(&pool)
            .await
            .expect("count recovered outputs");
    assert_eq!(shutdown_outputs, 1);
    let (status, cancel_message) = request_json(
        &app,
        Method::POST,
        &format!("/api/v1/conversations/{fork_id}/messages"),
        &cookie,
        Some(json!({
            "role":"user","text":"This run will be canceled",
            "provider":null,"model":null,"usage":null,"tool_calls":null
        })),
    )
    .await;
    assert_eq!(status, StatusCode::CREATED, "{cancel_message}");
    let (status, cancel_run) = request_json(
        &app,
        Method::POST,
        &format!("/api/v1/conversations/{fork_id}/runs"),
        &cookie,
        Some(json!({"input_message_id":cancel_message["id"],"model_id":slow_model_id})),
    )
    .await;
    assert_eq!(status, StatusCode::ACCEPTED, "{cancel_run}");
    let cancel_run_id = uuid_field(&cancel_run, "id");
    wait_for_event(&pool, cancel_run_id, "model.text_delta").await;
    let (status, active_run) = request_json(
        &app,
        Method::GET,
        &format!("/api/v1/conversations/{fork_id}/runs"),
        &cookie,
        None,
    )
    .await;
    assert_eq!(status, StatusCode::OK, "{active_run}");
    assert_eq!(active_run["id"], cancel_run_id.to_string());
    let cancel_state = AppState::new(pool.clone(), test_settings(&database_url))
        .await
        .expect("create independent cancellation instance");
    let cancel_app = router(cancel_state);
    let (status, _) = request_json(
        &cancel_app,
        Method::POST,
        &format!("/api/v1/runs/{cancel_run_id}/cancel"),
        &cookie,
        None,
    )
    .await;
    assert_eq!(status, StatusCode::ACCEPTED);
    assert_eq!(wait_for_run(&pool, cancel_run_id).await, "canceled");
    let (status, duplicate_cancel) = request_json(
        &cancel_app,
        Method::POST,
        &format!("/api/v1/runs/{cancel_run_id}/cancel"),
        &cookie,
        None,
    )
    .await;
    assert_eq!(status, StatusCode::ACCEPTED, "{duplicate_cancel}");
    let canceled_lease_cleared: bool = sqlx::query_scalar(
        "SELECT execution_token IS NULL AND lease_expires_at IS NULL FROM agent_runs WHERE id=$1",
    )
    .bind(cancel_run_id)
    .fetch_one(&pool)
    .await
    .expect("check canceled run lease");
    assert!(canceled_lease_cleared);
    let canceled_output: bool =
        sqlx::query_scalar("SELECT EXISTS(SELECT 1 FROM messages WHERE agent_run_id=$1)")
            .bind(cancel_run_id)
            .fetch_one(&pool)
            .await
            .expect("check canceled output");
    assert!(!canceled_output);
    let terminal_events: i64 = sqlx::query_scalar(
        "SELECT count(*) FROM run_events WHERE run_id=$1 AND event_type IN ('run.completed','run.failed','run.canceled')",
    )
    .bind(cancel_run_id)
    .fetch_one(&pool)
    .await
    .expect("count terminal events");
    assert_eq!(terminal_events, 1);

    worker_cancel.cancel();
    takeover_worker_cancel.cancel();
    tokio::time::timeout(StdDuration::from_secs(5), worker)
        .await
        .expect("worker stops")
        .expect("worker task succeeds");
    tokio::time::timeout(StdDuration::from_secs(5), takeover_worker)
        .await
        .expect("run worker stops")
        .expect("run worker task succeeds");
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
            local_models: true,
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
    // State-changing requests from a browser always include an Origin
    // header. The default public_origin is http://localhost:8080.
    let is_state_change = matches!(
        method,
        Method::POST | Method::PUT | Method::PATCH | Method::DELETE
    );
    let mut builder = Request::builder()
        .method(method)
        .uri(uri)
        .header(header::COOKIE, cookie);
    if is_state_change {
        builder = builder.header(header::ORIGIN, "http://localhost:8080");
    }
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
    async fn chat(Json(request): Json<Value>) -> Response {
        match request["model"].as_str() {
            Some("fail-model") => (
                StatusCode::TOO_MANY_REQUESTS,
                Json(json!({"error":"rate limited"})),
            )
                .into_response(),
            Some("slow-model") => {
                let chunks = stream::unfold(0_u8, |step| async move {
                    match step {
                        0 => Some((Ok::<_, std::io::Error>("{\"message\":{\"content\":\"Too late to publish\"},\"done\":false}\n"), 1)),
                        1 => {
                            tokio::time::sleep(StdDuration::from_secs(1)).await;
                            Some((Ok("{\"message\":{\"content\":\"\"},\"done\":true,\"prompt_eval_count\":8,\"eval_count\":4}\n"), 2))
                        }
                        _ => None,
                    }
                });
                Body::from_stream(chunks).into_response()
            }
            _ => Body::from(
                "{\"message\":{\"content\":\"Deterministic assistant answer 🦀\"},\"done\":false}\n\
                 {\"message\":{\"content\":\"\"},\"done\":true,\"prompt_eval_count\":12,\"eval_count\":5}\n",
            )
            .into_response(),
        }
    }
    let listener = TcpListener::bind("127.0.0.1:0")
        .await
        .expect("bind fake embedding server");
    let address = listener.local_addr().expect("fake server address");
    let server = tokio::spawn(async move {
        axum::serve(
            listener,
            Router::new()
                .route("/v1/api/embed", post(embeddings))
                .route("/v1/api/chat", post(chat)),
        )
        .await
        .expect("serve fake embeddings");
    });
    (format!("http://{address}/v1"), server)
}

async fn wait_for_run(pool: &PgPool, run_id: Uuid) -> String {
    for _ in 0..100 {
        let state: String = sqlx::query_scalar("SELECT state FROM agent_runs WHERE id=$1")
            .bind(run_id)
            .fetch_one(pool)
            .await
            .expect("read run state");
        if matches!(state.as_str(), "completed" | "failed" | "canceled") {
            if state == "failed" {
                let row = sqlx::query("SELECT error_code,error_detail FROM agent_runs WHERE id=$1")
                    .bind(run_id)
                    .fetch_one(pool)
                    .await
                    .expect("read run failure");
                panic!(
                    "run failed: {:?} {:?}",
                    row.get::<Option<String>, _>("error_code"),
                    row.get::<Option<String>, _>("error_detail")
                );
            }
            return state;
        }
        tokio::time::sleep(StdDuration::from_millis(50)).await;
    }
    panic!("run did not reach a terminal state");
}

async fn wait_for_event(pool: &PgPool, run_id: Uuid, event_type: &str) {
    for _ in 0..100 {
        let exists: bool = sqlx::query_scalar(
            "SELECT EXISTS(SELECT 1 FROM run_events WHERE run_id=$1 AND event_type=$2)",
        )
        .bind(run_id)
        .bind(event_type)
        .fetch_one(pool)
        .await
        .expect("read run event");
        if exists {
            return;
        }
        tokio::time::sleep(StdDuration::from_millis(25)).await;
    }
    panic!("run event did not appear");
}

async fn wait_for_released_lease(pool: &PgPool, run_id: Uuid) {
    for _ in 0..100 {
        let released: bool = sqlx::query_scalar(
            "SELECT execution_token IS NULL AND lease_expires_at IS NULL FROM agent_runs WHERE id=$1",
        )
        .bind(run_id)
        .fetch_one(pool)
        .await
        .expect("read run lease");
        if released {
            return;
        }
        tokio::time::sleep(StdDuration::from_millis(25)).await;
    }
    panic!("run lease was not released");
}

async fn next_websocket_json(socket: &mut WebSocketStream<MaybeTlsStream<TcpStream>>) -> Value {
    loop {
        let message = socket
            .next()
            .await
            .expect("websocket message")
            .expect("valid websocket message");
        if message.is_text() {
            return serde_json::from_str(message.to_text().expect("text websocket message"))
                .expect("JSON websocket message");
        }
    }
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
