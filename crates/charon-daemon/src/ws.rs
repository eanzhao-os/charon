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
use base64::{Engine, engine::general_purpose::STANDARD as BASE64};
use charon_core::{
    ClientFrame, DAEMON_VERSION, HelloPayload, ListWorkspacesResponse, NyxIdentity, ServerFrame,
    ServerInfo, TerminalKeysAck, TerminalKillAck, TerminalResizeAck, WsError,
};
use chrono::Utc;
use tokio::sync::broadcast::error::RecvError;
use tracing::{debug, info, warn};

use crate::nyxid_jwt::OwnerIdentity;
use crate::terminal::{TerminalEvent, TerminalManager};
use crate::workspace::WorkspaceManager;
use crate::{AppState, diff, files};

const HEARTBEAT: Duration = Duration::from_secs(60);

pub async fn ws_upgrade_handler(
    ws: WebSocketUpgrade,
    State(state): State<AppState>,
    OwnerIdentity(identity): OwnerIdentity,
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

    let mut terminal_events = state.terminals.subscribe();

    loop {
        tokio::select! {
            msg = socket.recv() => {
                match msg {
                    Some(Ok(Message::Text(text))) => {
                        dispatch(&mut socket, &state.workspaces, &state.terminals, text.as_str()).await;
                    }
                    Some(Ok(Message::Binary(_))) => {
                        send_error(&mut socket, None, "unsupported_binary",
                            "client→server binary frames not used in M2.4 (terminal output uses base64-in-JSON)").await;
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
            event = terminal_events.recv() => {
                match event {
                    Ok(TerminalEvent::Output { terminal_id, data, seq }) => {
                        let frame = ServerFrame::TerminalOutput {
                            terminal_id,
                            data_b64: BASE64.encode(&data),
                            seq,
                        };
                        send_or_warn(&mut socket, &frame).await;
                    }
                    Ok(TerminalEvent::Exited { terminal_id, exit_code }) => {
                        send_or_warn(
                            &mut socket,
                            &ServerFrame::TerminalExited { terminal_id, exit_code },
                        )
                        .await;
                    }
                    Err(RecvError::Lagged(n)) => {
                        warn!(missed = n, "WS session lagged on terminal events; client should refetch scrollback");
                    }
                    Err(RecvError::Closed) => {
                        debug!("terminal broadcast closed; ending session");
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

async fn dispatch(
    socket: &mut WebSocket,
    workspaces: &Arc<WorkspaceManager>,
    terminals: &Arc<TerminalManager>,
    text: &str,
) {
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
        ClientFrame::FileList { id, payload } => {
            let ws = match workspaces.get(&payload.workspace_id).await {
                Some(ws) => ws,
                None => {
                    send_error(
                        socket,
                        Some(id),
                        "not_found",
                        format!("workspace {} not found", payload.workspace_id),
                    )
                    .await;
                    return;
                }
            };
            match files::tree(&ws, &payload.path, payload.depth).await {
                Ok(result) => {
                    send_or_warn(socket, &ServerFrame::FileListed { id, result }).await;
                }
                Err(e) => send_error(socket, Some(id), "tree_failed", e.to_string()).await,
            }
        }
        ClientFrame::FileRead { id, payload } => {
            let ws = match workspaces.get(&payload.workspace_id).await {
                Some(ws) => ws,
                None => {
                    send_error(
                        socket,
                        Some(id),
                        "not_found",
                        format!("workspace {} not found", payload.workspace_id),
                    )
                    .await;
                    return;
                }
            };
            match files::read(&ws, &payload.path).await {
                Ok(result) => {
                    send_or_warn(socket, &ServerFrame::FileContent { id, result }).await;
                }
                Err(e) => send_error(socket, Some(id), "read_failed", e.to_string()).await,
            }
        }
        ClientFrame::FileWrite { id, payload } => {
            let ws = match workspaces.get(&payload.workspace_id).await {
                Some(ws) => ws,
                None => {
                    send_error(
                        socket,
                        Some(id),
                        "not_found",
                        format!("workspace {} not found", payload.workspace_id),
                    )
                    .await;
                    return;
                }
            };
            match files::write(&ws, &payload.path, &payload.content).await {
                Ok(result) => {
                    send_or_warn(socket, &ServerFrame::FileWritten { id, result }).await;
                }
                Err(e) => send_error(socket, Some(id), "write_failed", e.to_string()).await,
            }
        }
        ClientFrame::DiffGet { id, payload } => {
            let ws = match workspaces.get(&payload.workspace_id).await {
                Some(ws) => ws,
                None => {
                    send_error(
                        socket,
                        Some(id),
                        "not_found",
                        format!("workspace {} not found", payload.workspace_id),
                    )
                    .await;
                    return;
                }
            };
            match diff::diff(&ws).await {
                Ok(result) => {
                    send_or_warn(socket, &ServerFrame::DiffSnapshot { id, result }).await;
                }
                Err(e) => send_error(socket, Some(id), "diff_failed", e.to_string()).await,
            }
        }
        ClientFrame::TerminalCreate { id, payload } => {
            let ws = match workspaces.get(&payload.workspace_id).await {
                Some(ws) => ws,
                None => {
                    send_error(
                        socket,
                        Some(id),
                        "not_found",
                        format!("workspace {} not found", payload.workspace_id),
                    )
                    .await;
                    return;
                }
            };
            match terminals.create(&ws, payload).await {
                Ok(result) => {
                    send_or_warn(socket, &ServerFrame::TerminalCreated { id, result }).await;
                }
                Err(e) => {
                    send_error(socket, Some(id), "terminal_create_failed", e.to_string()).await
                }
            }
        }
        ClientFrame::TerminalSendKeys { id, payload } => {
            let bytes = match BASE64.decode(payload.data_b64.as_bytes()) {
                Ok(b) => b,
                Err(e) => {
                    send_error(socket, Some(id), "bad_base64", e.to_string()).await;
                    return;
                }
            };
            match terminals.send_keys(&payload.terminal_id, bytes).await {
                Ok(bytes_written) => {
                    send_or_warn(
                        socket,
                        &ServerFrame::TerminalKeysSent {
                            id,
                            result: TerminalKeysAck {
                                terminal_id: payload.terminal_id,
                                bytes_written,
                            },
                        },
                    )
                    .await;
                }
                Err(e) => send_error(socket, Some(id), "send_keys_failed", e.to_string()).await,
            }
        }
        ClientFrame::TerminalResize { id, payload } => {
            match terminals
                .resize(&payload.terminal_id, payload.cols, payload.rows)
                .await
            {
                Ok(()) => {
                    send_or_warn(
                        socket,
                        &ServerFrame::TerminalResized {
                            id,
                            result: TerminalResizeAck {
                                terminal_id: payload.terminal_id,
                                cols: payload.cols,
                                rows: payload.rows,
                            },
                        },
                    )
                    .await;
                }
                Err(e) => send_error(socket, Some(id), "resize_failed", e.to_string()).await,
            }
        }
        ClientFrame::TerminalKill { id, payload } => {
            match terminals.kill(&payload.terminal_id).await {
                Ok(()) => {
                    send_or_warn(
                        socket,
                        &ServerFrame::TerminalKilled {
                            id,
                            result: TerminalKillAck {
                                terminal_id: payload.terminal_id,
                            },
                        },
                    )
                    .await;
                }
                Err(e) => send_error(socket, Some(id), "kill_failed", e.to_string()).await,
            }
        }
        ClientFrame::TerminalList { id, payload } => {
            let result = terminals
                .list_response(payload.workspace_id.as_deref())
                .await;
            send_or_warn(socket, &ServerFrame::TerminalListed { id, result }).await;
        }
        ClientFrame::TerminalScrollback { id, payload } => {
            match terminals.scrollback(&payload.terminal_id).await {
                Ok(result) => {
                    send_or_warn(
                        socket,
                        &ServerFrame::TerminalScrollbackSnapshot { id, result },
                    )
                    .await;
                }
                Err(e) => send_error(socket, Some(id), "scrollback_failed", e.to_string()).await,
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
