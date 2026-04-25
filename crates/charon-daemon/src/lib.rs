//! Charon daemon library entry point.
//!
//! `serve(config, shutdown)` is the only public surface. Both the
//! `charon-daemon` binary and `charon-cli`'s `daemon start` call it.

pub mod nyxid_jwt;

use std::net::SocketAddr;
use std::sync::Arc;

use anyhow::{Context, Result};
use axum::extract::FromRef;
use axum::{Json, Router, routing::get};
use charon_core::{
    DAEMON_VERSION, DEFAULT_DAEMON_BIND, DEFAULT_NYXID_ISSUER, HealthResponse, WhoAmIResponse,
};
use tokio::net::TcpListener;
use tokio_util::sync::CancellationToken;
use tower_http::trace::TraceLayer;
use tracing::info;

use crate::nyxid_jwt::{IdentityToken, JwksClient};

/// Default `aud` for the existing `charon-echo-poc` UserService — its
/// `endpoint_url` is `http://localhost:18789`. Override with
/// `CHARON_EXPECTED_AUD` if you re-register the UserService.
pub const DEFAULT_EXPECTED_AUD: &str = "http://localhost:18789";

#[derive(Debug, Clone)]
pub struct DaemonConfig {
    pub bind: SocketAddr,
    pub expected_aud: String,
    pub nyxid_issuer: String,
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
        Ok(Self {
            bind,
            expected_aud,
            nyxid_issuer,
        })
    }
}

#[derive(Clone, FromRef)]
pub struct AppState {
    pub jwks: Arc<JwksClient>,
}

pub fn router(state: AppState) -> Router {
    Router::new()
        .route("/api/v1/health", get(health_handler))
        .route("/api/v1/whoami", get(whoami_handler))
        .layer(TraceLayer::new_for_http())
        .with_state(state)
}

pub async fn serve(config: DaemonConfig, shutdown: Option<CancellationToken>) -> Result<()> {
    let jwks = Arc::new(
        JwksClient::new(&config.nyxid_issuer, &config.expected_aud)
            .await
            .context("initialize JwksClient")?,
    );
    let state = AppState { jwks };

    let listener = TcpListener::bind(config.bind)
        .await
        .with_context(|| format!("failed to bind {}", config.bind))?;
    let local = listener.local_addr().unwrap_or(config.bind);
    info!(
        bind = %local,
        version = DAEMON_VERSION,
        issuer = %config.nyxid_issuer,
        expected_aud = %config.expected_aud,
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

#[cfg(test)]
mod tests {
    use super::*;
    use axum::body::to_bytes;
    use axum::http::{Request, StatusCode};
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
