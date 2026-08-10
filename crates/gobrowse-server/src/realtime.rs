use std::time::Duration;

use axum::{
    extract::{
        State, WebSocketUpgrade,
        ws::{Message, WebSocket},
    },
    http::HeaderMap,
    response::Response,
};
use serde::Serialize;
use tokio::time::interval;

use crate::{AppState, auth::require_user, error::AppError};

#[derive(Debug, Serialize)]
struct ServerEvent<'a> {
    protocol: &'a str,
    sequence: i64,
    kind: &'a str,
}

pub async fn upgrade(
    State(state): State<AppState>,
    headers: HeaderMap,
    ws: WebSocketUpgrade,
) -> Result<Response, AppError> {
    validate_origin(&state, &headers)?;
    require_user(&state, &headers).await?;
    Ok(ws.max_message_size(256 * 1024).on_upgrade(handle_socket))
}

async fn handle_socket(mut socket: WebSocket) {
    let hello = ServerEvent {
        protocol: "gobrowse.realtime.v1",
        sequence: 0,
        kind: "connected",
    };
    if let Ok(encoded) = serde_json::to_string(&hello)
        && socket.send(Message::Text(encoded.into())).await.is_err()
    {
        return;
    }
    let mut heartbeat = interval(Duration::from_secs(25));
    loop {
        tokio::select! {
            _ = heartbeat.tick() => {
                if socket.send(Message::Ping(Vec::new().into())).await.is_err() {
                    break;
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
    if origin.trim_end_matches('/')
        != state
            .settings
            .http
            .public_origin
            .as_str()
            .trim_end_matches('/')
    {
        return Err(AppError::Forbidden);
    }
    Ok(())
}
