//! NyxID JWT verification.
//!
//! On startup, fetch the OIDC discovery document, then the JWKS. Cache keys
//! by `kid` for 24h; refresh lazily on a `kid` miss (handles key rotation).
//! `verify` returns a [`NyxIdentity`] populated from the JWT claims.
//!
//! Used by the [`IdentityToken`] axum extractor, which 401s any request that
//! lacks a valid `X-NyxID-Identity-Token` header.

use std::collections::HashMap;
use std::sync::Arc;
use std::time::{Duration, Instant};

use axum::Json;
use axum::extract::{FromRef, FromRequestParts};
use axum::http::StatusCode;
use axum::http::request::Parts;
use charon_core::{IDENTITY_TOKEN_HEADER, NyxIdentity};
use chrono::{DateTime, Utc};
use jsonwebtoken::jwk::JwkSet;
use jsonwebtoken::{Algorithm, DecodingKey, Validation, decode, decode_header};
use serde::Deserialize;
use thiserror::Error;
use tokio::sync::RwLock;
use tracing::{debug, info, warn};

const JWKS_REFRESH_THROTTLE: Duration = Duration::from_secs(60);

#[derive(Debug, Deserialize)]
struct OidcDiscovery {
    jwks_uri: String,
}

#[derive(Debug, Deserialize)]
struct RawClaims {
    // iss / aud are checked by `jsonwebtoken::Validation` directly against the
    // raw JWT, not via this struct, so we don't deserialize them here.
    sub: String,
    exp: i64,
    iat: i64,
    #[serde(default)]
    email: Option<String>,
    #[serde(default)]
    name: Option<String>,
    #[serde(default)]
    roles: Vec<String>,
    #[serde(default)]
    groups: Vec<String>,
    #[serde(default)]
    permissions: Vec<String>,
    #[serde(default)]
    nyx_service_id: Option<String>,
    #[serde(default)]
    nyx_agent_id: Option<String>,
}

#[derive(Debug, Error)]
pub enum JwtError {
    #[error("missing {0} header")]
    MissingHeader(&'static str),
    #[error("non-ASCII {0} header")]
    NonAsciiHeader(&'static str),
    #[error("malformed JWT: {0}")]
    Malformed(#[source] jsonwebtoken::errors::Error),
    #[error("JWT header missing kid")]
    MissingKid,
    #[error("unknown JWKS kid {0}")]
    UnknownKid(String),
    #[error("JWT validation failed: {0}")]
    Invalid(#[source] jsonwebtoken::errors::Error),
    #[error("JWKS fetch failed: {0}")]
    JwksFetch(#[source] reqwest::Error),
    #[error("OIDC discovery failed: {0}")]
    Discovery(#[source] reqwest::Error),
    #[error("JWKS HTTP {0}")]
    JwksStatus(reqwest::StatusCode),
}

#[derive(Debug, Error)]
pub enum OwnerAuthError {
    #[error("owner user id must not be empty")]
    EmptyOwnerUserId,
    #[error("identity user_id '{actual}' is not configured owner")]
    UserMismatch { actual: String },
}

#[derive(Debug)]
pub struct OwnerAuthorizer {
    owner_user_id: String,
}

impl OwnerAuthorizer {
    pub fn new(owner_user_id: impl Into<String>) -> Result<Self, OwnerAuthError> {
        let owner_user_id = owner_user_id.into().trim().to_string();
        if owner_user_id.is_empty() {
            return Err(OwnerAuthError::EmptyOwnerUserId);
        }
        Ok(Self { owner_user_id })
    }

    pub fn owner_user_id(&self) -> &str {
        &self.owner_user_id
    }

    pub fn authorize(&self, identity: &NyxIdentity) -> Result<(), OwnerAuthError> {
        if identity.user_id == self.owner_user_id {
            return Ok(());
        }
        Err(OwnerAuthError::UserMismatch {
            actual: identity.user_id.clone(),
        })
    }
}

pub struct JwksClient {
    issuer: String,
    expected_aud: String,
    http: reqwest::Client,
    inner: RwLock<Cache>,
}

struct Cache {
    jwks_uri: String,
    keys: HashMap<String, Arc<DecodingKey>>,
    fetched_at: Instant,
}

impl JwksClient {
    pub async fn new(
        issuer: impl Into<String>,
        expected_aud: impl Into<String>,
    ) -> Result<Self, JwtError> {
        let issuer = issuer.into();
        let expected_aud = expected_aud.into();
        let http = reqwest::Client::builder()
            .user_agent(concat!("charon-daemon/", env!("CARGO_PKG_VERSION")))
            .timeout(Duration::from_secs(10))
            .build()
            .expect("reqwest client");

        let discovery_url = format!(
            "{}/.well-known/openid-configuration",
            issuer.trim_end_matches('/')
        );
        let discovery: OidcDiscovery = http
            .get(&discovery_url)
            .send()
            .await
            .map_err(JwtError::Discovery)?
            .error_for_status()
            .map_err(JwtError::Discovery)?
            .json()
            .await
            .map_err(JwtError::Discovery)?;

        let keys = fetch_jwks(&http, &discovery.jwks_uri).await?;
        info!(
            issuer = %issuer,
            jwks_uri = %discovery.jwks_uri,
            expected_aud = %expected_aud,
            key_count = keys.len(),
            "JWKS loaded"
        );

        Ok(Self {
            issuer,
            expected_aud,
            http,
            inner: RwLock::new(Cache {
                jwks_uri: discovery.jwks_uri,
                keys,
                fetched_at: Instant::now(),
            }),
        })
    }

    pub fn expected_aud(&self) -> &str {
        &self.expected_aud
    }

    pub async fn verify(&self, token: &str) -> Result<NyxIdentity, JwtError> {
        let header = decode_header(token).map_err(JwtError::Malformed)?;
        let kid = header.kid.ok_or(JwtError::MissingKid)?;

        let key = match self.lookup_key(&kid).await {
            Some(k) => k,
            None => {
                debug!(kid = %kid, "kid miss; refreshing JWKS");
                self.refresh().await?;
                self.lookup_key(&kid)
                    .await
                    .ok_or_else(|| JwtError::UnknownKid(kid.clone()))?
            }
        };

        let mut validation = Validation::new(Algorithm::RS256);
        validation.set_issuer(&[&self.issuer]);
        validation.set_audience(&[&self.expected_aud]);
        // Tolerate ~30s of clock skew between NyxID and this host.
        validation.leeway = 30;

        let data = decode::<RawClaims>(token, &key, &validation).map_err(JwtError::Invalid)?;
        Ok(claims_to_identity(data.claims))
    }

    async fn lookup_key(&self, kid: &str) -> Option<Arc<DecodingKey>> {
        let cache = self.inner.read().await;
        cache.keys.get(kid).cloned()
    }

    async fn refresh(&self) -> Result<(), JwtError> {
        let mut cache = self.inner.write().await;
        // Throttle: if another task just refreshed, skip.
        if cache.fetched_at.elapsed() < JWKS_REFRESH_THROTTLE {
            return Ok(());
        }
        let keys = fetch_jwks(&self.http, &cache.jwks_uri).await?;
        let n = keys.len();
        cache.keys = keys;
        cache.fetched_at = Instant::now();
        info!(key_count = n, "JWKS refreshed");
        Ok(())
    }
}

#[cfg(test)]
impl JwksClient {
    pub(crate) fn for_test(
        issuer: impl Into<String>,
        expected_aud: impl Into<String>,
        kid: impl Into<String>,
        key: Arc<DecodingKey>,
    ) -> Self {
        let mut keys = HashMap::new();
        keys.insert(kid.into(), key);
        Self {
            issuer: issuer.into(),
            expected_aud: expected_aud.into(),
            http: reqwest::Client::new(),
            inner: RwLock::new(Cache {
                jwks_uri: "test://jwks".to_string(),
                keys,
                fetched_at: Instant::now(),
            }),
        }
    }
}

async fn fetch_jwks(
    http: &reqwest::Client,
    jwks_uri: &str,
) -> Result<HashMap<String, Arc<DecodingKey>>, JwtError> {
    let resp = http
        .get(jwks_uri)
        .send()
        .await
        .map_err(JwtError::JwksFetch)?;
    let status = resp.status();
    if !status.is_success() {
        return Err(JwtError::JwksStatus(status));
    }
    let set: JwkSet = resp.json().await.map_err(JwtError::JwksFetch)?;
    let mut out = HashMap::new();
    for jwk in set.keys {
        let kid = match jwk.common.key_id.clone() {
            Some(k) => k,
            None => {
                warn!("JWKS entry without kid; skipping");
                continue;
            }
        };
        match DecodingKey::from_jwk(&jwk) {
            Ok(key) => {
                out.insert(kid, Arc::new(key));
            }
            Err(e) => warn!(kid = %kid, error = %e, "failed to materialize JWK; skipping"),
        }
    }
    Ok(out)
}

fn claims_to_identity(raw: RawClaims) -> NyxIdentity {
    NyxIdentity {
        user_id: raw.sub,
        email: raw.email,
        name: raw.name,
        roles: raw.roles,
        permissions: raw.permissions,
        groups: raw.groups,
        nyx_service_id: raw.nyx_service_id,
        agent_id: raw.nyx_agent_id,
        issued_at: DateTime::<Utc>::from_timestamp(raw.iat, 0).unwrap_or_else(Utc::now),
        expires_at: DateTime::<Utc>::from_timestamp(raw.exp, 0).unwrap_or_else(Utc::now),
    }
}

// ----- axum extractor -----

pub struct IdentityToken(pub NyxIdentity);

pub struct OwnerIdentity(pub NyxIdentity);

impl<S> FromRequestParts<S> for IdentityToken
where
    S: Send + Sync,
    Arc<JwksClient>: FromRef<S>,
{
    type Rejection = (StatusCode, Json<crate::ErrorBody>);

    async fn from_request_parts(parts: &mut Parts, state: &S) -> Result<Self, Self::Rejection> {
        let raw = parts.headers.get(IDENTITY_TOKEN_HEADER).ok_or_else(|| {
            reject(
                StatusCode::UNAUTHORIZED,
                "missing_identity_token",
                JwtError::MissingHeader(IDENTITY_TOKEN_HEADER),
            )
        })?;
        let token = raw.to_str().map_err(|_| {
            reject(
                StatusCode::BAD_REQUEST,
                "malformed_header",
                JwtError::NonAsciiHeader(IDENTITY_TOKEN_HEADER),
            )
        })?;

        let jwks: Arc<JwksClient> = FromRef::from_ref(state);
        match jwks.verify(token).await {
            Ok(identity) => Ok(IdentityToken(identity)),
            Err(e) => {
                let status = match &e {
                    JwtError::JwksFetch(_) | JwtError::JwksStatus(_) | JwtError::Discovery(_) => {
                        StatusCode::SERVICE_UNAVAILABLE
                    }
                    _ => StatusCode::UNAUTHORIZED,
                };
                Err(reject(status, "invalid_token", e))
            }
        }
    }
}

impl<S> FromRequestParts<S> for OwnerIdentity
where
    S: Send + Sync,
    Arc<JwksClient>: FromRef<S>,
    Arc<OwnerAuthorizer>: FromRef<S>,
{
    type Rejection = (StatusCode, Json<crate::ErrorBody>);

    async fn from_request_parts(parts: &mut Parts, state: &S) -> Result<Self, Self::Rejection> {
        let IdentityToken(identity) = IdentityToken::from_request_parts(parts, state).await?;
        let owner: Arc<OwnerAuthorizer> = FromRef::from_ref(state);
        owner.authorize(&identity).map_err(|e| {
            warn!(user_id = %identity.user_id, error = %e, "rejecting non-owner request");
            (
                StatusCode::FORBIDDEN,
                Json(crate::ErrorBody {
                    error: "not_owner",
                    message: e.to_string(),
                }),
            )
        })?;
        Ok(OwnerIdentity(identity))
    }
}

fn reject(
    status: StatusCode,
    code: &'static str,
    err: JwtError,
) -> (StatusCode, Json<crate::ErrorBody>) {
    warn!(error = %err, "rejecting request");
    (
        status,
        Json(crate::ErrorBody {
            error: code,
            message: err.to_string(),
        }),
    )
}
