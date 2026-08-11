use std::time::Duration;

use axum::{
    extract::{
        Path, Query, State, WebSocketUpgrade,
        ws::{Message, WebSocket},
    },
    http::HeaderMap,
    response::Response,
};
use serde::{Deserialize, Serialize};
use sqlx::Row;
use tokio::time::interval;

use crate::{AppState, auth::require_user, error::AppError};

#[derive(Debug, Serialize)]
struct ServerEvent<'a> {
    protocol: &'a str,
    sequence: i64,
    kind: &'a str,
}

#[derive(Debug, Serialize)]
struct RunDelivery {
    protocol: &'static str,
    sequence: i64,
    kind: String,
    run_id: uuid::Uuid,
    payload: serde_json::Value,
}

#[derive(Deserialize)]
pub struct RealtimeQuery {
    after: Option<i64>,
}

pub async fn upgrade(
    State(state): State<AppState>,
    Path(run_id): Path<uuid::Uuid>,
    Query(query): Query<RealtimeQuery>,
    headers: HeaderMap,
    ws: WebSocketUpgrade,
) -> Result<Response, AppError> {
    validate_origin(&state, &headers)?;
    let user = require_user(&state, &headers).await?;
    let authorized: bool = sqlx::query_scalar(
        "SELECT EXISTS(SELECT 1 FROM agent_runs run JOIN conversations conversation ON conversation.id=run.conversation_id \
         WHERE run.id=$1 AND run.profile_id=$2 AND run.run_kind='conversation_turn' AND ( \
         $4 IN ('OWNER','ADMIN') OR (conversation.workspace_id IS NULL AND conversation.created_by_user_id=$3) OR EXISTS( \
         SELECT 1 FROM workspace_memberships member WHERE member.workspace_id=conversation.workspace_id AND member.user_id=$3)))",
    )
    .bind(run_id)
    .bind(user.profile_id)
    .bind(user.id)
    .bind(&user.role)
    .fetch_one(&state.pool)
    .await?;
    if !authorized {
        return Err(AppError::NotFound);
    }
    let session_hash = user.session_hash.ok_or(AppError::Unauthorized)?;
    Ok(ws.max_message_size(256 * 1024).on_upgrade(move |socket| {
        handle_socket(
            socket,
            state,
            session_hash,
            user.id,
            run_id,
            query.after.unwrap_or(0).max(0),
        )
    }))
}

async fn handle_socket(
    mut socket: WebSocket,
    state: AppState,
    session_hash: Vec<u8>,
    user_id: uuid::Uuid,
    run_id: uuid::Uuid,
    mut cursor: i64,
) {
    let hello = ServerEvent {
        protocol: "gobrowse.realtime.v1",
        sequence: cursor,
        kind: "connected",
    };
    if let Ok(encoded) = serde_json::to_string(&hello)
        && socket.send(Message::Text(encoded.into())).await.is_err()
    {
        return;
    }
    let mut heartbeat = interval(Duration::from_secs(25));
    let mut replay = interval(Duration::from_millis(500));
    loop {
        tokio::select! {
            _ = heartbeat.tick() => {
                if socket.send(Message::Ping(Vec::new().into())).await.is_err() {
                    break;
                }
            }
            _ = replay.tick() => {
                let rows = sqlx::query(
                    "SELECT event.sequence,event.event_type,event.payload,event.run_id FROM run_events event \
                     WHERE event.run_id=$1 AND event.sequence>$2 \
                     ORDER BY event.sequence LIMIT 100",
                )
                .bind(run_id)
                .bind(cursor)
                .fetch_all(&state.pool)
                .await;
                let Ok(rows) = rows else { continue; };
                let still_authorized: bool = sqlx::query_scalar(
                    "SELECT EXISTS(SELECT 1 FROM sessions session JOIN users user_row ON user_row.id=session.user_id \
                     JOIN agent_runs run ON run.id=$2 JOIN conversations conversation ON conversation.id=run.conversation_id \
                     WHERE session.token_hash=$1 AND session.auth_epoch=user_row.auth_epoch AND session.expires_at>clock_timestamp() \
                     AND session.absolute_expires_at>clock_timestamp() AND user_row.disabled_at IS NULL AND run.id=$2 AND ( \
                     user_row.role IN ('OWNER','ADMIN') OR (conversation.workspace_id IS NULL AND conversation.created_by_user_id=$3) OR EXISTS( \
                     SELECT 1 FROM workspace_memberships member WHERE member.workspace_id=conversation.workspace_id AND member.user_id=$3)))"
                ).bind(&session_hash).bind(run_id).bind(user_id).fetch_one(&state.pool).await.unwrap_or(false);
                if !still_authorized { break; }
                for row in rows {
                    let event = RunDelivery {
                        protocol: "gobrowse.realtime.v1",
                        sequence: row.get("sequence"),
                        kind: row.get("event_type"),
                        run_id: row.get("run_id"),
                        payload: row.get("payload"),
                    };
                    cursor = event.sequence;
                    let Ok(encoded) = serde_json::to_string(&event) else { continue; };
                    if socket.send(Message::Text(encoded.into())).await.is_err() { return; }
                }
            }
            message = socket.recv() => {
                match message {
                    Some(Ok(Message::Ping(payload))) => {
                        if socket.send(Message::Pong(payload)).await.is_err() { break; }
                    }
                    Some(Ok(Message::Close(_))) | None | Some(Err(_)) => break,
                    Some(Ok(Message::Text(_) | Message::Binary(_) | Message::Pong(_))) => {}
                }
            }
        }
    }
}

fn validate_origin(state: &AppState, headers: &HeaderMap) -> Result<(), AppError> {
    let origin = headers
        .get(axum::http::header::ORIGIN)
        .and_then(|value| value.to_str().ok())
        .ok_or(AppError::Forbidden)?;
    let public_origin = state.settings.http.public_origin.as_str();
    // Parse both as URLs for explicit scheme + host + port comparison.
    let origin_parsed: url::Url = origin.parse().map_err(|_| AppError::Forbidden)?;
    let expected_parsed: url::Url = public_origin.parse().map_err(|_| AppError::Forbidden)?;
    if origin_parsed.scheme() != expected_parsed.scheme()
        || origin_parsed.host() != expected_parsed.host()
        || origin_parsed.port() != expected_parsed.port()
    {
        return Err(AppError::Forbidden);
    }
    Ok(())
}
