//! WebSocket protocol (M2.2).
//!
//! `GET /api/v1/ws` upgrades to a WebSocket if the request has a valid
//! `X-NyxID-Identity-Token` header (per M2 decision: trust JWT once at upgrade).
//! After upgrade we push a `Hello` frame, then dispatch client request frames
//! to the same business-logic functions the REST handlers use. Server pings
//! every 60s to stay under NyxID's 300s WS idle timeout.

use std::sync::Arc;
use std::time::Duration;

use axum::extract::State;
use axum::extract::ws::{Message, Utf8Bytes, WebSocket, WebSocketUpgrade};
use axum::response::Response;
use charon_core::{
    ClientFrame, DAEMON_VERSION, HelloPayload, ListWorkspacesResponse, NyxIdentity, ServerFrame,
    ServerInfo, WsError,
};
use chrono::Utc;
use tracing::{debug, info, warn};

use crate::AppState;
use crate::nyxid_jwt::IdentityToken;
use crate::workspace::WorkspaceManager;

const HEARTBEAT: Duration = Duration::from_secs(60);

pub async fn ws_upgrade_handler(
    ws: WebSocketUpgrade,
    State(state): State<AppState>,
    IdentityToken(identity): IdentityToken,
) -> Response {
    info!(user_id = %identity.user_id, "WS upgrade");
    ws.on_upgrade(move |socket| run_session(socket, state, identity))
}

async fn run_session(mut socket: WebSocket, state: AppState, identity: NyxIdentity) {
    if let Err(e) = send_frame(
        &mut socket,
        &ServerFrame::Hello {
            payload: HelloPayload {
                server: ServerInfo {
                    version: DAEMON_VERSION.to_string(),
                    server_time: Utc::now(),
                },
                identity: identity.clone(),
            },
        },
    )
    .await
    {
        warn!(error = %e, "failed to send Hello; closing");
        return;
    }

    let mut heartbeat = tokio::time::interval(HEARTBEAT);
    heartbeat.tick().await; // skip the immediate tick

    loop {
        tokio::select! {
            msg = socket.recv() => {
                match msg {
                    Some(Ok(Message::Text(text))) => {
                        dispatch(&mut socket, &state.workspaces, text.as_str()).await;
                    }
                    Some(Ok(Message::Binary(_))) => {
                        send_error(&mut socket, None, "unsupported_binary",
                            "binary frames land in M2.3/M2.4").await;
                    }
                    Some(Ok(Message::Ping(payload))) => {
                        let _ = socket.send(Message::Pong(payload)).await;
                    }
                    Some(Ok(Message::Pong(_))) => {}
                    Some(Ok(Message::Close(_))) | None => {
                        debug!("WS close from client");
                        return;
                    }
                    Some(Err(e)) => {
                        warn!(error = %e, "WS recv error; closing");
                        return;
                    }
                }
            }
            _ = heartbeat.tick() => {
                if let Err(e) = socket.send(Message::Ping(vec![].into())).await {
                    warn!(error = %e, "ping send failed; closing");
                    return;
                }
            }
        }
    }
}

async fn dispatch(socket: &mut WebSocket, workspaces: &Arc<WorkspaceManager>, text: &str) {
    let frame: ClientFrame = match serde_json::from_str(text) {
        Ok(f) => f,
        Err(e) => {
            send_error(socket, None, "malformed_frame", e.to_string()).await;
            return;
        }
    };

    match frame {
        ClientFrame::WorkspaceList { id, payload } => {
            let workspaces = workspaces.list(payload.include_archived).await;
            send_or_warn(
                socket,
                &ServerFrame::WorkspaceListed {
                    id,
                    result: ListWorkspacesResponse { workspaces },
                },
            )
            .await;
        }
        ClientFrame::WorkspaceCreate { id, payload } => match workspaces.create(payload).await {
            Ok(w) => {
                send_or_warn(socket, &ServerFrame::WorkspaceCreated { id, result: w }).await;
            }
            Err(e) => send_error(socket, Some(id), "create_failed", e.to_string()).await,
        },
        ClientFrame::WorkspaceGet { id, payload } => {
            match workspaces.get(&payload.workspace_id).await {
                Some(w) => {
                    send_or_warn(socket, &ServerFrame::WorkspaceGot { id, result: w }).await;
                }
                None => {
                    send_error(
                        socket,
                        Some(id),
                        "not_found",
                        format!("workspace {} not found", payload.workspace_id),
                    )
                    .await
                }
            }
        }
        ClientFrame::WorkspaceArchive { id, payload } => {
            match workspaces.archive(&payload.workspace_id).await {
                Ok(w) => {
                    send_or_warn(socket, &ServerFrame::WorkspaceArchived { id, result: w }).await;
                }
                Err(e) => send_error(socket, Some(id), "archive_failed", e.to_string()).await,
            }
        }
    }
}

async fn send_frame(socket: &mut WebSocket, frame: &ServerFrame) -> anyhow::Result<()> {
    let text = serde_json::to_string(frame)?;
    socket.send(Message::Text(Utf8Bytes::from(text))).await?;
    Ok(())
}

async fn send_or_warn(socket: &mut WebSocket, frame: &ServerFrame) {
    if let Err(e) = send_frame(socket, frame).await {
        warn!(error = %e, "WS send failed");
    }
}

async fn send_error(
    socket: &mut WebSocket,
    id: Option<String>,
    code: &str,
    message: impl Into<String>,
) {
    let frame = ServerFrame::Error {
        id,
        error: WsError {
            code: code.to_string(),
            message: message.into(),
        },
    };
    send_or_warn(socket, &frame).await;
}
