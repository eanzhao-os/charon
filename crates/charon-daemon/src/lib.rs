//! Charon daemon library entry point.
//!
//! `serve(config, shutdown)` is the only public surface. Both the
//! `charon-daemon` binary and `charon-cli`'s `daemon start` call it.

pub mod diff;
pub mod files;
pub mod nyxid_jwt;
pub mod terminal;
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

use crate::nyxid_jwt::{IdentityToken, JwksClient, OwnerAuthorizer, OwnerIdentity};
use crate::terminal::TerminalManager;
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
    pub owner_user_id: String,
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
        let owner_user_id = std::env::var("CHARON_OWNER_USER_ID")
            .context("CHARON_OWNER_USER_ID env var not set")?;
        let owner_user_id = owner_user_id.trim().to_string();
        if owner_user_id.is_empty() {
            anyhow::bail!("CHARON_OWNER_USER_ID must not be empty");
        }
        let home = match std::env::var_os("CHARON_HOME") {
            Some(p) => PathBuf::from(p),
            None => default_home()?,
        };
        Ok(Self {
            bind,
            expected_aud,
            nyxid_issuer,
            owner_user_id,
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
    pub owner: Arc<OwnerAuthorizer>,
    pub workspaces: Arc<WorkspaceManager>,
    pub terminals: Arc<TerminalManager>,
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
    let owner = Arc::new(
        OwnerAuthorizer::new(config.owner_user_id.clone()).context("initialize OwnerAuthorizer")?,
    );
    let terminals = Arc::new(TerminalManager::new());
    let state = AppState {
        jwks,
        owner,
        workspaces,
        terminals,
    };

    let listener = TcpListener::bind(config.bind)
        .await
        .with_context(|| format!("failed to bind {}", config.bind))?;
    let local = listener.local_addr().unwrap_or(config.bind);
    info!(
        bind = %local,
        version = DAEMON_VERSION,
        issuer = %config.nyxid_issuer,
        expected_aud = %config.expected_aud,
        owner_user_id = %config.owner_user_id,
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
    OwnerIdentity(_owner): OwnerIdentity,
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
    OwnerIdentity(_owner): OwnerIdentity,
    Query(q): Query<ListQuery>,
) -> Json<ListWorkspacesResponse> {
    Json(ListWorkspacesResponse {
        workspaces: workspaces.list(q.include_archived).await,
    })
}

async fn get_workspace_handler(
    State(workspaces): State<Arc<WorkspaceManager>>,
    OwnerIdentity(_owner): OwnerIdentity,
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
    OwnerIdentity(_owner): OwnerIdentity,
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
    OwnerIdentity(_owner): OwnerIdentity,
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
    OwnerIdentity(_owner): OwnerIdentity,
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
    OwnerIdentity(_owner): OwnerIdentity,
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
    OwnerIdentity(_owner): OwnerIdentity,
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
    use charon_core::IDENTITY_TOKEN_HEADER;
    use chrono::Utc;
    use jsonwebtoken::{Algorithm, DecodingKey, EncodingKey, Header, encode};
    use serde_json::json;
    use tower::ServiceExt;

    const TEST_ISSUER: &str = "https://issuer.test";
    const TEST_AUD: &str = "http://localhost:18789";
    const TEST_KID: &str = "owner-test-key";
    // Throwaway keypair used only for route-level JWT tests.
    const TEST_PRIVATE_KEY: &str = r#"-----BEGIN PRIVATE KEY-----
MIIEvAIBADANBgkqhkiG9w0BAQEFAASCBKYwggSiAgEAAoIBAQCUCp7gm330NPzi
7nogF8C5mGs0N0oIe8ec3aj+EQsG7TP/bBu3dpior8WJkq9uZcvmt/0z/quqgyqH
2ktzMAxc9rpOIpI3mMng4Mj9ktC+zPkUo+tZZPraBYz8XuOZaPiSlwbExZ+2M3M3
8KLg0rob61woGnwsAewlDqLjeFaWZmcVRIx+RUf7Tbk6wWM2FZ6UoBf1sCjtqlvg
i2qNOktjFLRwytWL+nBpEAyl4by6rthIMhK8beK43Ps4A6m24noZcQ+AYenOHIV4
dRCApiLs3019VNFOsVljnyHFnGHjW3kt8vPGbq1SS+oOp8h0SloVYK1qJxMH/6AB
uuFDZCBFAgMBAAECggEAD5BWR7LROR1hANKlkD4vCtQVYTX22JF62OkM3TkZea7y
aoYJG+6h+goQsHf1bZvSJf1t50t87L5BeGrgx8ljY1qlF5XW3XV4s+Wt+8q1m3md
LihVk95j6QvwWI/5SaWZjH/IPGOyeMtL77OizBQbcNf7plOyfkXtd6/kPBnosIMG
kq5b3Tm/23PcnNIWeI+QWFoGxuKHAIiq/axpTpwmwJwGob8A2kxnueyKO+cdqBfR
jAWAyol2086HNMBeBeOHck+9k02wUq4kXbfCxHiNrcshb9wBLGR9AIiAAZ1wAIvW
hm6VIutFQYtgmiLA9mDbmO/bDKDabcCvqZNxpqwCOQKBgQDM3ohKPdNAj9KAx+U6
Q/tsyyu/GonV9pl9gxKY/rbbnVh1yvmlwYLGFXwCxUlHu1JVU1GoX75CIFdCmVAr
VPQ/BEgJ+fb1AokFEEfja9tsr+RvhBZ4LzgsmB9qkcqjQWlP6i3K+/o4jCx7x1xN
EFOf6itP0fYzQvux3JojyqCgVwKBgQC4/UI4dUtr6twxgy6ryFamTyAce2OicCFP
FOSIUqOjBLjHwl2vjoa/3UQTE0ytNKlcsUoOERpJYdqL1mApqLDf+9m8g2X9xelK
nHyFk5dIivna9uLoYQchdjV1lPbD+8sbuuxWSZXQqnSo2nQp1GcC7S3vX7fudehO
9kVnam8ywwKBgBoTwVlh4T/4jpzh1OXDvX8tpVXf9OeNSiBVzMo4seHmd1oXCgv1
Q8Ye+fgIULmWuHYv8tbxyO/12eWaSkAZwjU7QEg0zyCEwBgq6FukYPvGr9caAxot
OINEocsY36hELTmE32tVA5arEQZ4a+FLULmsPvMcELCZuBv9rokbw7JlAoGAD0yC
qYCp2Cb4Ru/+cB6FbAOnODPMLabwWkX0EIIlHlpJndupO9ehtURrWNiDwt9UEmJn
KXqoneEF3gLAuTFGT3/YpgqH6NDxVkZS1gk6vbkgqMc6RNWhbVcFXNARCGxOg+CV
ox060qMGOuC2Mq9qRYewANf9si72I3Gik8bto1kCgYBNVsMZ3QBrPePZiXd1NW2J
sGLn8+lOJN6nHCQZxZpo2swcpldWDvrqFHHxm7XNBFvsJXkBd4KZnJtRPBL3yiMJ
6M9Aicg9CMlnhZ0Gt31T7pA+IkveJrBG/psN5YwOTVKtrmx+NIKy6qD/ob0AF4XU
CWIxQNx1v9uhuNNSXp3lzA==
-----END PRIVATE KEY-----"#;
    const TEST_PUBLIC_KEY: &str = r#"-----BEGIN PUBLIC KEY-----
MIIBIjANBgkqhkiG9w0BAQEFAAOCAQ8AMIIBCgKCAQEAlAqe4Jt99DT84u56IBfA
uZhrNDdKCHvHnN2o/hELBu0z/2wbt3aYqK/FiZKvbmXL5rf9M/6rqoMqh9pLczAM
XPa6TiKSN5jJ4ODI/ZLQvsz5FKPrWWT62gWM/F7jmWj4kpcGxMWftjNzN/Ci4NK6
G+tcKBp8LAHsJQ6i43hWlmZnFUSMfkVH+025OsFjNhWelKAX9bAo7apb4ItqjTpL
YxS0cMrVi/pwaRAMpeG8uq7YSDISvG3iuNz7OAOptuJ6GXEPgGHpzhyFeHUQgKYi
7N9NfVTRTrFZY58hxZxh41t5LfLzxm6tUkvqDqfIdEpaFWCtaicTB/+gAbrhQ2Qg
RQIDAQAB
-----END PUBLIC KEY-----"#;

    /// Stateless slice of the router so /health can be tested without a live JWKS.
    fn health_only_router() -> Router {
        Router::new().route("/api/v1/health", get(health_handler))
    }

    async fn owner_test_router(owner_user_id: &str) -> (Router, tempfile::TempDir) {
        let tmp = tempfile::tempdir().unwrap();
        let jwks = Arc::new(JwksClient::for_test(
            TEST_ISSUER,
            TEST_AUD,
            TEST_KID,
            Arc::new(DecodingKey::from_rsa_pem(TEST_PUBLIC_KEY.as_bytes()).unwrap()),
        ));
        let owner = Arc::new(OwnerAuthorizer::new(owner_user_id).unwrap());
        let workspaces = Arc::new(
            WorkspaceManager::new(tmp.path().join("home"))
                .await
                .unwrap(),
        );
        let terminals = Arc::new(TerminalManager::new());
        (
            router(AppState {
                jwks,
                owner,
                workspaces,
                terminals,
            }),
            tmp,
        )
    }

    fn signed_token(user_id: &str) -> String {
        let now = Utc::now().timestamp();
        let claims = json!({
            "iss": TEST_ISSUER,
            "aud": TEST_AUD,
            "sub": user_id,
            "iat": now,
            "exp": now + 3600,
        });
        let mut header = Header::new(Algorithm::RS256);
        header.kid = Some(TEST_KID.to_string());
        encode(
            &header,
            &claims,
            &EncodingKey::from_rsa_pem(TEST_PRIVATE_KEY.as_bytes()).unwrap(),
        )
        .unwrap()
    }

    fn list_workspaces_request(token: Option<String>) -> Request<axum::body::Body> {
        let mut builder = Request::builder().uri("/api/v1/workspaces");
        if let Some(token) = token {
            builder = builder.header(IDENTITY_TOKEN_HEADER, token);
        }
        builder.body(axum::body::Body::empty()).unwrap()
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

    #[tokio::test]
    async fn workspace_routes_allow_configured_owner() {
        let (app, _tmp) = owner_test_router("owner-123").await;
        let resp = app
            .oneshot(list_workspaces_request(Some(signed_token("owner-123"))))
            .await
            .unwrap();

        assert_eq!(resp.status(), StatusCode::OK);
        let body = to_bytes(resp.into_body(), 64 * 1024).await.unwrap();
        let parsed: ListWorkspacesResponse = serde_json::from_slice(&body).unwrap();
        assert!(parsed.workspaces.is_empty());
    }

    #[tokio::test]
    async fn workspace_routes_reject_non_owner() {
        let (app, _tmp) = owner_test_router("owner-123").await;
        let resp = app
            .oneshot(list_workspaces_request(Some(signed_token("other-user"))))
            .await
            .unwrap();

        assert_eq!(resp.status(), StatusCode::FORBIDDEN);
    }

    #[tokio::test]
    async fn workspace_routes_reject_missing_token() {
        let (app, _tmp) = owner_test_router("owner-123").await;
        let resp = app.oneshot(list_workspaces_request(None)).await.unwrap();

        assert_eq!(resp.status(), StatusCode::UNAUTHORIZED);
    }
}
