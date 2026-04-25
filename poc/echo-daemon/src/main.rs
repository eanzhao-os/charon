// Charon PoC echo daemon.
//
// Goal: verify whether NyxID's reverse proxy (and node-routed proxy) preserves
// the X-NyxID-* identity headers when forwarding HTTP and WebSocket traffic
// to a localhost downstream.
//
// Endpoints:
//   GET  /                  — dump request headers as JSON, also log to stdout
//   ANY  /any/*tail         — same as / but lets us probe any path/method
//   GET  /ws                — WebSocket upgrade. First text frame sent back to
//                              the client is the upgrade-time HTTP header dump.
//                              After that it echoes whatever you send.

use std::collections::BTreeMap;
use std::net::SocketAddr;

use axum::{
    extract::{
        ws::{Message, WebSocket, WebSocketUpgrade},
        ConnectInfo,
    },
    http::{HeaderMap, Method, Uri},
    response::IntoResponse,
    routing::{any, get},
    Json, Router,
};
use futures_util::{sink::SinkExt, stream::StreamExt};
use serde_json::json;

const NYX_PREFIX: &str = "x-nyxid-";

fn headers_to_map(headers: &HeaderMap) -> BTreeMap<String, String> {
    let mut out = BTreeMap::new();
    for (name, value) in headers.iter() {
        let n = name.as_str().to_string();
        let v = value.to_str().unwrap_or("<non-utf8>").to_string();
        out.insert(n, v);
    }
    out
}

fn highlight_nyx(headers: &BTreeMap<String, String>) -> Vec<(String, String)> {
    headers
        .iter()
        .filter(|(k, _)| k.to_lowercase().starts_with(NYX_PREFIX))
        .map(|(k, v)| (k.clone(), v.clone()))
        .collect()
}

async fn dump_http(
    method: Method,
    uri: Uri,
    headers: HeaderMap,
    ConnectInfo(peer): ConnectInfo<SocketAddr>,
) -> impl IntoResponse {
    let map = headers_to_map(&headers);
    let nyx = highlight_nyx(&map);

    tracing::info!(
        method = %method,
        path = %uri.path(),
        peer = %peer,
        nyx_headers = nyx.len(),
        "HTTP request"
    );
    for (k, v) in &nyx {
        tracing::info!("  {} = {}", k, v);
    }

    Json(json!({
        "scheme": "http",
        "method": method.as_str(),
        "path": uri.path(),
        "query": uri.query().unwrap_or(""),
        "peer": peer.to_string(),
        "headers": map,
        "nyx_headers_seen": nyx.iter().map(|(k,_)| k).collect::<Vec<_>>(),
    }))
}

async fn ws_handler(
    ws: WebSocketUpgrade,
    headers: HeaderMap,
    ConnectInfo(peer): ConnectInfo<SocketAddr>,
) -> impl IntoResponse {
    let map = headers_to_map(&headers);
    let nyx = highlight_nyx(&map);

    tracing::info!(
        peer = %peer,
        nyx_headers = nyx.len(),
        "WS upgrade requested"
    );
    for (k, v) in &nyx {
        tracing::info!("  ws-upgrade  {} = {}", k, v);
    }

    let snapshot = json!({
        "scheme": "ws",
        "peer": peer.to_string(),
        "headers": map,
        "nyx_headers_seen": nyx.iter().map(|(k,_)| k).collect::<Vec<_>>(),
    })
    .to_string();

    ws.on_upgrade(move |socket| handle_ws(socket, snapshot))
}

async fn handle_ws(socket: WebSocket, header_snapshot: String) {
    let (mut tx, mut rx) = socket.split();

    if let Err(e) = tx.send(Message::Text(header_snapshot.into())).await {
        tracing::warn!(error = ?e, "failed to send header snapshot");
        return;
    }

    while let Some(msg) = rx.next().await {
        match msg {
            Ok(Message::Text(t)) => {
                tracing::info!(text = %t, "ws got text");
                if tx
                    .send(Message::Text(format!("echo: {t}").into()))
                    .await
                    .is_err()
                {
                    break;
                }
            }
            Ok(Message::Binary(b)) => {
                tracing::info!(bytes = b.len(), "ws got binary");
                if tx.send(Message::Binary(b)).await.is_err() {
                    break;
                }
            }
            Ok(Message::Ping(p)) => {
                let _ = tx.send(Message::Pong(p)).await;
            }
            Ok(Message::Pong(_)) | Ok(Message::Close(_)) => break,
            Err(e) => {
                tracing::warn!(error = ?e, "ws recv err");
                break;
            }
        }
    }
    tracing::info!("ws closed");
}

#[tokio::main]
async fn main() {
    tracing_subscriber::fmt()
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_default_env()
                .unwrap_or_else(|_| tracing_subscriber::EnvFilter::new("info")),
        )
        .with_target(false)
        .compact()
        .init();

    let app = Router::new()
        .route("/", any(dump_http))
        .route("/ws", get(ws_handler))
        .fallback(any(dump_http));

    let bind: SocketAddr = std::env::var("ECHO_BIND")
        .unwrap_or_else(|_| "127.0.0.1:18789".to_string())
        .parse()
        .expect("ECHO_BIND must be a valid SocketAddr");

    let listener = tokio::net::TcpListener::bind(bind)
        .await
        .expect("bind failed");
    tracing::info!(%bind, "charon echo daemon listening");

    axum::serve(
        listener,
        app.into_make_service_with_connect_info::<SocketAddr>(),
    )
    .await
    .expect("serve");
}
