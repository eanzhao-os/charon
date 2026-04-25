//! Charon daemon library entry point.
//!
//! `serve(config, shutdown)` is the only public surface. Both the
//! `charon-daemon` binary and `charon-cli`'s `daemon start` call it.

pub mod diff;
pub mod files;
pub mod nyxid_jwt;
pub mod workspace;
pub mod ws;

use std::net::SocketAddr;
use std::path::PathBuf;
use std::sync::Arc;

use anyhow::{Context, Result};
use axum::extract::{FromRef, Path, Query, State};
use axum::http::StatusCode;
use axum::routing::{get, post};
use axum::{Json, Router};
use charon_core::{
    CreateWorkspaceRequest, DAEMON_VERSION, DEFAULT_DAEMON_BIND, DEFAULT_NYXID_ISSUER,
    DiffResponse, FileContent, FileTreeResponse, HealthResponse, ListWorkspacesResponse, WS_PATH,
    WhoAmIResponse, Workspace, WriteFileRequest,
};
use serde::{Deserialize, Serialize};
use tokio::net::TcpListener;
use tokio_util::sync::CancellationToken;
use tower_http::trace::TraceLayer;
use tracing::info;

use crate::nyxid_jwt::{IdentityToken, JwksClient};
use crate::workspace::WorkspaceManager;

/// Default `aud` for the existing `charon-echo-poc` UserService — its
/// `endpoint_url` is `http://localhost:18789`. Override with
/// `CHARON_EXPECTED_AUD` if you re-register the UserService.
pub const DEFAULT_EXPECTED_AUD: &str = "http://localhost:18789";

/// Default home dir name (under $HOME) for daemon state.
pub const DEFAULT_HOME_DIRNAME: &str = ".charon";

#[derive(Debug, Clone)]
pub struct DaemonConfig {
    pub bind: SocketAddr,
    pub expected_aud: String,
    pub nyxid_issuer: String,
    pub home: PathBuf,
}

impl DaemonConfig {
    pub fn from_env() -> Result<Self> {
        let bind: SocketAddr = std::env::var("CHARON_BIND")
            .unwrap_or_else(|_| DEFAULT_DAEMON_BIND.to_string())
            .parse()
            .context("invalid CHARON_BIND")?;
        let expected_aud = std::env::var("CHARON_EXPECTED_AUD")
            .unwrap_or_else(|_| DEFAULT_EXPECTED_AUD.to_string());
        let nyxid_issuer = std::env::var("CHARON_NYXID_ISSUER")
            .unwrap_or_else(|_| DEFAULT_NYXID_ISSUER.to_string());
        let home = match std::env::var_os("CHARON_HOME") {
            Some(p) => PathBuf::from(p),
            None => default_home()?,
        };
        Ok(Self {
            bind,
            expected_aud,
            nyxid_issuer,
            home,
        })
    }
}

pub fn default_home() -> Result<PathBuf> {
    let home = std::env::var_os("HOME").context("HOME env var not set")?;
    Ok(PathBuf::from(home).join(DEFAULT_HOME_DIRNAME))
}

#[derive(Clone, FromRef)]
pub struct AppState {
    pub jwks: Arc<JwksClient>,
    pub workspaces: Arc<WorkspaceManager>,
}

#[derive(Debug, Serialize)]
pub struct ErrorBody {
    pub error: &'static str,
    pub message: String,
}

pub fn err(
    status: StatusCode,
    code: &'static str,
    message: impl Into<String>,
) -> (StatusCode, Json<ErrorBody>) {
    (
        status,
        Json(ErrorBody {
            error: code,
            message: message.into(),
        }),
    )
}

pub fn router(state: AppState) -> Router {
    Router::new()
        .route("/api/v1/health", get(health_handler))
        .route("/api/v1/whoami", get(whoami_handler))
        .route(
            "/api/v1/workspaces",
            post(create_workspace_handler).get(list_workspaces_handler),
        )
        .route("/api/v1/workspaces/{id}", get(get_workspace_handler))
        .route(
            "/api/v1/workspaces/{id}/archive",
            post(archive_workspace_handler),
        )
        .route("/api/v1/workspaces/{id}/tree", get(tree_handler))
        .route(
            "/api/v1/workspaces/{id}/file",
            get(file_read_handler).put(file_write_handler),
        )
        .route("/api/v1/workspaces/{id}/diff", get(diff_handler))
        .route(WS_PATH, get(ws::ws_upgrade_handler))
        .layer(TraceLayer::new_for_http())
        .with_state(state)
}

pub async fn serve(config: DaemonConfig, shutdown: Option<CancellationToken>) -> Result<()> {
    let jwks = Arc::new(
        JwksClient::new(&config.nyxid_issuer, &config.expected_aud)
            .await
            .context("initialize JwksClient")?,
    );
    let workspaces = Arc::new(
        WorkspaceManager::new(config.home.clone())
            .await
            .context("initialize WorkspaceManager")?,
    );
    let state = AppState { jwks, workspaces };

    let listener = TcpListener::bind(config.bind)
        .await
        .with_context(|| format!("failed to bind {}", config.bind))?;
    let local = listener.local_addr().unwrap_or(config.bind);
    info!(
        bind = %local,
        version = DAEMON_VERSION,
        issuer = %config.nyxid_issuer,
        expected_aud = %config.expected_aud,
        home = %config.home.display(),
        "charon-daemon listening"
    );

    let server = axum::serve(listener, router(state));
    match shutdown {
        Some(token) => server
            .with_graceful_shutdown(async move { token.cancelled().await })
            .await
            .context("axum::serve")?,
        None => server.await.context("axum::serve")?,
    }
    Ok(())
}

// ---------- handlers ----------

async fn health_handler() -> Json<HealthResponse> {
    Json(HealthResponse {
        ok: true,
        version: DAEMON_VERSION.to_string(),
    })
}

async fn whoami_handler(IdentityToken(identity): IdentityToken) -> Json<WhoAmIResponse> {
    Json(WhoAmIResponse {
        ok: true,
        version: DAEMON_VERSION.to_string(),
        identity,
    })
}

async fn create_workspace_handler(
    State(workspaces): State<Arc<WorkspaceManager>>,
    IdentityToken(_identity): IdentityToken,
    Json(req): Json<CreateWorkspaceRequest>,
) -> Result<(StatusCode, Json<Workspace>), (StatusCode, Json<ErrorBody>)> {
    workspaces
        .create(req)
        .await
        .map(|w| (StatusCode::CREATED, Json(w)))
        .map_err(|e| err(StatusCode::BAD_REQUEST, "create_failed", e.to_string()))
}

#[derive(Debug, Deserialize)]
struct ListQuery {
    #[serde(default)]
    include_archived: bool,
}

async fn list_workspaces_handler(
    State(workspaces): State<Arc<WorkspaceManager>>,
    IdentityToken(_identity): IdentityToken,
    Query(q): Query<ListQuery>,
) -> Json<ListWorkspacesResponse> {
    Json(ListWorkspacesResponse {
        workspaces: workspaces.list(q.include_archived).await,
    })
}

async fn get_workspace_handler(
    State(workspaces): State<Arc<WorkspaceManager>>,
    IdentityToken(_identity): IdentityToken,
    Path(id): Path<String>,
) -> Result<Json<Workspace>, (StatusCode, Json<ErrorBody>)> {
    workspaces.get(&id).await.map(Json).ok_or_else(|| {
        err(
            StatusCode::NOT_FOUND,
            "not_found",
            format!("workspace {id}"),
        )
    })
}

async fn archive_workspace_handler(
    State(workspaces): State<Arc<WorkspaceManager>>,
    IdentityToken(_identity): IdentityToken,
    Path(id): Path<String>,
) -> Result<Json<Workspace>, (StatusCode, Json<ErrorBody>)> {
    workspaces
        .archive(&id)
        .await
        .map(Json)
        .map_err(|e| err(StatusCode::NOT_FOUND, "archive_failed", e.to_string()))
}

#[derive(Debug, Deserialize)]
struct TreeQuery {
    #[serde(default)]
    path: String,
    #[serde(default = "default_tree_depth")]
    depth: u32,
}

fn default_tree_depth() -> u32 {
    1
}

#[derive(Debug, Deserialize)]
struct FileQuery {
    path: String,
}

async fn fetch_workspace_or_404(
    workspaces: &WorkspaceManager,
    id: &str,
) -> Result<Workspace, (StatusCode, Json<ErrorBody>)> {
    workspaces.get(id).await.ok_or_else(|| {
        err(
            StatusCode::NOT_FOUND,
            "not_found",
            format!("workspace {id}"),
        )
    })
}

async fn tree_handler(
    State(workspaces): State<Arc<WorkspaceManager>>,
    IdentityToken(_identity): IdentityToken,
    Path(id): Path<String>,
    Query(q): Query<TreeQuery>,
) -> Result<Json<FileTreeResponse>, (StatusCode, Json<ErrorBody>)> {
    let ws = fetch_workspace_or_404(&workspaces, &id).await?;
    files::tree(&ws, &q.path, q.depth)
        .await
        .map(Json)
        .map_err(|e| err(StatusCode::BAD_REQUEST, "tree_failed", e.to_string()))
}

async fn file_read_handler(
    State(workspaces): State<Arc<WorkspaceManager>>,
    IdentityToken(_identity): IdentityToken,
    Path(id): Path<String>,
    Query(q): Query<FileQuery>,
) -> Result<Json<FileContent>, (StatusCode, Json<ErrorBody>)> {
    let ws = fetch_workspace_or_404(&workspaces, &id).await?;
    files::read(&ws, &q.path)
        .await
        .map(Json)
        .map_err(|e| err(StatusCode::BAD_REQUEST, "read_failed", e.to_string()))
}

async fn file_write_handler(
    State(workspaces): State<Arc<WorkspaceManager>>,
    IdentityToken(_identity): IdentityToken,
    Path(id): Path<String>,
    Query(q): Query<FileQuery>,
    Json(body): Json<WriteFileRequest>,
) -> Result<Json<FileContent>, (StatusCode, Json<ErrorBody>)> {
    let ws = fetch_workspace_or_404(&workspaces, &id).await?;
    files::write(&ws, &q.path, &body.content)
        .await
        .map(Json)
        .map_err(|e| err(StatusCode::BAD_REQUEST, "write_failed", e.to_string()))
}

async fn diff_handler(
    State(workspaces): State<Arc<WorkspaceManager>>,
    IdentityToken(_identity): IdentityToken,
    Path(id): Path<String>,
) -> Result<Json<DiffResponse>, (StatusCode, Json<ErrorBody>)> {
    let ws = fetch_workspace_or_404(&workspaces, &id).await?;
    diff::diff(&ws)
        .await
        .map(Json)
        .map_err(|e| err(StatusCode::BAD_REQUEST, "diff_failed", e.to_string()))
}

#[cfg(test)]
mod tests {
    use super::*;
    use axum::body::to_bytes;
    use axum::http::Request;
    use tower::ServiceExt;

    /// Stateless slice of the router so /health can be tested without a live JWKS.
    fn health_only_router() -> Router {
        Router::new().route("/api/v1/health", get(health_handler))
    }

    #[tokio::test]
    async fn health_returns_ok_true() {
        let app = health_only_router();
        let resp = app
            .oneshot(
                Request::builder()
                    .uri("/api/v1/health")
                    .body(axum::body::Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(resp.status(), StatusCode::OK);
        let body = to_bytes(resp.into_body(), 64 * 1024).await.unwrap();
        let parsed: HealthResponse = serde_json::from_slice(&body).unwrap();
        assert!(parsed.ok);
        assert_eq!(parsed.version, DAEMON_VERSION);
    }
}
