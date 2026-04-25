//! NyxID JWT verification.
//!
//! On startup, fetch the OIDC discovery document, then the JWKS. Startup is
//! fail-closed: if discovery or initial JWKS fetch fails, the daemon does not
//! start, and it does not persist a last-known-good JWKS.
//!
//! Runtime cache policy:
//! - cache materialized keys by `kid` for 24h;
//! - once the TTL expires, refresh before using even a known `kid`;
//! - refresh lazily on an unknown `kid`, with a 60s throttle for repeated
//!   unknown-kid attempts.
//!
//! `verify` returns a [`NyxIdentity`] populated from the JWT claims.
//!
//! Used by the [`IdentityToken`] axum extractor, which 401s any request that
//! lacks a valid `X-NyxID-Identity-Token` header.

use std::collections::HashMap;
use std::future::Future;
use std::pin::Pin;
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

const JWKS_CACHE_TTL: Duration = Duration::from_secs(24 * 60 * 60);
const JWKS_KID_MISS_REFRESH_THROTTLE: Duration = Duration::from_secs(60);

type KeyMap = HashMap<String, Arc<DecodingKey>>;
type BoxJwksFetch<'a> = Pin<Box<dyn Future<Output = Result<KeyMap, JwtError>> + Send + 'a>>;

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
    jwks_fetcher: Arc<dyn JwksFetcher>,
    clock: Arc<dyn Clock>,
    inner: RwLock<Cache>,
}

struct Cache {
    jwks_uri: String,
    keys: KeyMap,
    fetched_at: Instant,
    last_kid_miss_refresh_at: Option<Instant>,
}

trait JwksFetcher: Send + Sync {
    fn fetch<'a>(&'a self, http: &'a reqwest::Client, jwks_uri: &'a str) -> BoxJwksFetch<'a>;
}

struct HttpJwksFetcher;

impl JwksFetcher for HttpJwksFetcher {
    fn fetch<'a>(&'a self, http: &'a reqwest::Client, jwks_uri: &'a str) -> BoxJwksFetch<'a> {
        Box::pin(fetch_jwks(http, jwks_uri))
    }
}

trait Clock: Send + Sync {
    fn now(&self) -> Instant;
}

struct SystemClock;

impl Clock for SystemClock {
    fn now(&self) -> Instant {
        Instant::now()
    }
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

        let jwks_fetcher = Arc::new(HttpJwksFetcher);
        let clock = Arc::new(SystemClock);
        let keys = jwks_fetcher.fetch(&http, &discovery.jwks_uri).await?;
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
            jwks_fetcher,
            clock: clock.clone(),
            inner: RwLock::new(Cache {
                jwks_uri: discovery.jwks_uri,
                keys,
                fetched_at: clock.now(),
                last_kid_miss_refresh_at: None,
            }),
        })
    }

    pub fn expected_aud(&self) -> &str {
        &self.expected_aud
    }

    pub async fn verify(&self, token: &str) -> Result<NyxIdentity, JwtError> {
        let header = decode_header(token).map_err(JwtError::Malformed)?;
        let kid = header.kid.ok_or(JwtError::MissingKid)?;
        let refreshed_for_ttl = self.refresh_if_expired().await?;

        let key = match self.lookup_key(&kid).await {
            Some(k) => k,
            None => {
                debug!(kid = %kid, "kid miss; refreshing JWKS");
                if !refreshed_for_ttl {
                    self.refresh_for_kid_miss().await?;
                }
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

    async fn refresh_if_expired(&self) -> Result<bool, JwtError> {
        let mut cache = self.inner.write().await;
        if self.clock.now().duration_since(cache.fetched_at) < JWKS_CACHE_TTL {
            return Ok(false);
        }

        debug!("JWKS cache TTL expired; refreshing before verification");
        self.refresh_locked(&mut cache).await?;
        Ok(true)
    }

    async fn refresh_for_kid_miss(&self) -> Result<bool, JwtError> {
        let mut cache = self.inner.write().await;
        let now = self.clock.now();
        if let Some(last) = cache.last_kid_miss_refresh_at
            && now.duration_since(last) < JWKS_KID_MISS_REFRESH_THROTTLE
        {
            return Ok(false);
        }

        cache.last_kid_miss_refresh_at = Some(now);
        self.refresh_locked(&mut cache).await?;
        Ok(true)
    }

    async fn refresh_locked(&self, cache: &mut Cache) -> Result<(), JwtError> {
        let keys = self.jwks_fetcher.fetch(&self.http, &cache.jwks_uri).await?;
        let n = keys.len();
        cache.keys = keys;
        cache.fetched_at = self.clock.now();
        info!(key_count = n, "JWKS refreshed");
        Ok(())
    }

    #[cfg(test)]
    fn new_for_test(
        issuer: impl Into<String>,
        expected_aud: impl Into<String>,
        jwks_uri: impl Into<String>,
        keys: KeyMap,
        jwks_fetcher: Arc<dyn JwksFetcher>,
        clock: Arc<dyn Clock>,
    ) -> Self {
        Self {
            issuer: issuer.into(),
            expected_aud: expected_aud.into(),
            http: reqwest::Client::new(),
            jwks_fetcher,
            clock: clock.clone(),
            inner: RwLock::new(Cache {
                jwks_uri: jwks_uri.into(),
                keys,
                fetched_at: clock.now(),
                last_kid_miss_refresh_at: None,
            }),
        }
    }
}

#[cfg(test)]
impl JwksClient {
    /// Convenience constructor used by lib.rs router tests — installs a single
    /// (kid → key) entry and uses the production HTTP fetcher + system clock.
    /// For verifier-internal tests with injected fetchers / manual clocks see
    /// `new_for_test` in the tests module.
    pub(crate) fn for_test(
        issuer: impl Into<String>,
        expected_aud: impl Into<String>,
        kid: impl Into<String>,
        key: Arc<DecodingKey>,
    ) -> Self {
        let mut keys = KeyMap::new();
        keys.insert(kid.into(), key);
        let clock: Arc<dyn Clock> = Arc::new(SystemClock);
        Self {
            issuer: issuer.into(),
            expected_aud: expected_aud.into(),
            http: reqwest::Client::new(),
            jwks_fetcher: Arc::new(HttpJwksFetcher),
            clock: clock.clone(),
            inner: RwLock::new(Cache {
                jwks_uri: "test://jwks".to_string(),
                keys,
                fetched_at: clock.now(),
                last_kid_miss_refresh_at: None,
            }),
        }
    }
}

async fn fetch_jwks(http: &reqwest::Client, jwks_uri: &str) -> Result<KeyMap, JwtError> {
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
    Ok(materialize_jwks(set))
}

fn materialize_jwks(set: JwkSet) -> KeyMap {
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
    out
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

#[cfg(test)]
mod tests {
    use super::*;
    use jsonwebtoken::{EncodingKey, Header, encode};
    use serde::Serialize;
    use std::collections::VecDeque;
    use std::sync::Mutex;
    use std::sync::atomic::{AtomicUsize, Ordering};

    const ISSUER: &str = "https://issuer.test";
    const AUD: &str = "http://localhost:18789";
    const JWKS_URI: &str = "https://issuer.test/.well-known/jwks.json";

    const KEY1_N: &str = "hOnfyqHETfIMA7PQf2QQjw3vm41TxPVKYVxNctUk5CquBktUGl_K8-voxPMkT4JLfYip6RDs4KY6F3KREASUQcxywNyUl5yhi1bTXpV8DYYu4x4IhHZcPCJwDPHr2ln9XmAoODK_CFduhoy2Q6WpaLneGTdsd1451EboDGVfx3slCIIOdIDPXdOwDFzwHn4m80PkoGbgQUikIwFTjCARp660rO98qYJr39w12RrTrz8QVu5UTlVoDaNdQD4WAeXEC4n1JdOeKRNyuRi05MdfO-og1dT24f-9ep2YUwha9oqasbX1Bwtjs1eYJpz39oLcAbvPqQmE_TMugEItES8pjQ";
    const KEY2_N: &str = "ohPauhRNbQUnopIUj29BtMifA2MYvUTIsp1ZSAO9oCUrpsJESVKnnw2fyPdbaM86npZcktKv2x6zyXSVZtf9to295Rlu_mVnChKj6yZLqn6jjIkcfdsab8OR9bYI2DAIVYX4LVLvLZEWXmMCt2Qqx2fZ2WceJLn6xt-TKZ01gk5R-JZXC20jWlgJ7PfEZh0aSduhVcNkhg1BZYX0LGFVhdBCtgN7w9LKW9CWllQNdqlREwdaZmKyd7QbpJd95wbpv4zeotlQC_dmcKgn5jKRVX4q0Lj-MOveccZVNd5fbftzhf91aplPi1meSBb_LO3NyunSHNr7PEOoC-bp3DVcJQ";

    // Static test-only RSA keys for deterministic JWT fixtures.
    const KEY1_PEM: &str = r#"-----BEGIN PRIVATE KEY-----
MIIEvQIBADANBgkqhkiG9w0BAQEFAASCBKcwggSjAgEAAoIBAQCE6d/KocRN8gwD
s9B/ZBCPDe+bjVPE9UphXE1y1STkKq4GS1QaX8rz6+jE8yRPgkt9iKnpEOzgpjoX
cpEQBJRBzHLA3JSXnKGLVtNelXwNhi7jHgiEdlw8InAM8evaWf1eYCg4Mr8IV26G
jLZDpaloud4ZN2x3XjnURugMZV/HeyUIgg50gM9d07AMXPAefibzQ+SgZuBBSKQj
AVOMIBGnrrSs73ypgmvf3DXZGtOvPxBW7lROVWgNo11APhYB5cQLifUl054pE3K5
GLTkx1876iDV1Pbh/716nZhTCFr2ipqxtfUHC2OzV5gmnPf2gtwBu8+pCYT9My6A
Qi0RLymNAgMBAAECggEAA8o6cif6NcHG420jbxp+mWGrmSsmvhlDeXK9F57pyiLI
axAHUig0nI93x/Pp72V2+xmkRKvRoVdEFUqURdlnk9e9VvADQa568cL0TIBlNOqE
WEARPJu2ZhWSTeAxGj0SKziBNRcHWPjLQ0VsZhHpen3ATkZFDsNOUVYDRGU4nbLD
9nXBGlC/BQBqV4xezCt9uHrth6p5uQkLdVTDNr/tLzxtrgZJgTuXsanUKVrbCEWe
wBAISP1+NvOjyh2flimW23pLMFNxOBzlLenWzDdMQwPiuXhXDyQnR0W2wj8Q41Di
VwI2mhvPv6QY7Jhy8+eQFO9QnRQ/sNEtreEFo388qQKBgQC7PScPGZiPspc4At6z
+NH0MLCGFmjwvwWZCyCQrrfAi3IvbIbhjRlpDKLxASVR+dYgxrrpyU8p/utCUJ35
Iiwk3tQO3lD2neJqf79pM/RwXKUdS+nk9kpXilHt/Qv8dZHsikXQPgTh6VKIqZOZ
8LGVnW+gnBYIR2lB+3M5wLe62QKBgQC1uXLTPBrsYRlrv0ixZ7JPMH1hijsd6acU
xKBV4z6cxmWPf389TesyHrx+ux7KVIC8GTh4qSbRZdSZr2xfEcVU6rqr3vgVOxrv
zCpQPPObtemklxg9CnfMR8PEXpJa0jzYf6b+NnU/qequVgvK0BS+kQn6DMx5/ttJ
ssZ0Y/hr1QKBgQCbHiB7vALOGXB58La7dsnJeYTksTAjMr3aeoNyGa0VkPD6JPjh
Z1nD07ox23cloMsqwDkdca9p5UzV1Z/qQ8s6iHg6ESgWB9sJy+exql85rycDTF7r
VrdkKq2RcnA5qNVJl4wa5yZ4WioMGiC0Cdm1T4apEmaWWUL0bPKax/PukQKBgGu3
jF+3rgHVoJrknLND00braDasGFSnzjkaQCwI8nE9jK/dlE+DY1mnLHY2do7aPiDB
Fl83bOIMaVPbzvIfd4fZR2NfXFBBY7smmyJKrt/qmZ7NTTnJfa9iDqHUqQ4atqRi
Lltbbm6ZSpmiOYUziEhZcr98XKwnrFZoGQiexX8tAoGAQpER93fBXJJ5jX+kqjYS
vhbBdkXLt3lXDuDq4NjBeyGi0yAMtZeDtjYISBSB/PT6tfvccWdGM8iyhbMQFxG0
uf4rhLCc4NrDYPmw8+zxyXxCDU1w7N9TrmdfMawtJUjdHuRyIftbgqrDcJxPlVBg
ASJg5hbNV2Cz8h8vWQtJEuE=
-----END PRIVATE KEY-----"#;

    const KEY2_PEM: &str = r#"-----BEGIN PRIVATE KEY-----
MIIEvgIBADANBgkqhkiG9w0BAQEFAASCBKgwggSkAgEAAoIBAQCiE9q6FE1tBSei
khSPb0G0yJ8DYxi9RMiynVlIA72gJSumwkRJUqefDZ/I91tozzqellyS0q/bHrPJ
dJVm1/22jb3lGW7+ZWcKEqPrJkuqfqOMiRx92xpvw5H1tgjYMAhVhfgtUu8tkRZe
YwK3ZCrHZ9nZZx4kufrG35MpnTWCTlH4llcLbSNaWAns98RmHRpJ26FVw2SGDUFl
hfQsYVWF0EK2A3vD0spb0JaWVA12qVETB1pmYrJ3tBukl33nBum/jN6i2VAL92Zw
qCfmMpFVfirQuP4w695xxlU13l9t+3OF/3VqmU+LWZ5IFv8s7c3K6dIc2vs8Q6gL
5uncNVwlAgMBAAECggEACikCd33EU3GU8tJY1ZuyMRoOgHuFgSqUW5YU6GMg+llk
1iGsKYiJQiWW1FX6oKRfrAddqu/oNEKayfsz+RbQgdF64Py/giprKNd9om/BIzao
cVaQYay1iIy9qVCNrLe+HgEK9hRn3UxmFulQB0e74so2KYnBpahppG6UNBygeRhE
NhV0itAHf/zVHIEuf9hroyHqVuVvPnrxSOyQXLGPFqNET8NDW3DgKLZO6OiGJNk6
RZo1ZDFpdzBp+TiDVaycA0Ap+hWfQJnwWyNbueLlDa9IUFhHmiK4euwxWtJ7fhHF
3/ESFiE/Kt28FDtHS3OjXC4y4xdAVpSQa4oQxC7C4QKBgQDZFbHx4VDAPRxESNeX
GsZ39mkMCX1jGW/Z7IHX88AT8/LODfFFGjTyJuS8a1h7ClYxYyQo7xECILXhYjuR
Xwgzk0SWyHBLwz+7/WqltSQu3OQM8QjAznvuAMaJA2Q3kfWM9DccSONhMoJXe9wI
aqncIfWPh0wnk1hSKOvI02v3mQKBgQC/Ic6RTnc4GMnhLtXmfCee1/OaS2+ZyIjl
tINVkveNNGp4xG3TdXz38wx8ZSJ471gvMVJmgqwVdMPfTlBB+HjnqGB254ACgS5q
Q3w5Wp8T5o/7CyqP8XyCO6jQJIeDMh9JOkyoBwUsgclHzAAWY4i0//LtMr4drTmU
O+7K/HpwbQKBgQDVm1JwZrwVnUw+KMry5abbDf1JmeDmbXYxIlaVj0S2nXmSpgd9
bo8go4K5oIr87yvnBt3i5XJ//H3bm9Rvc+pXDZcVI3/UHPiO24pgKcDD2BkSXu61
AbjSdbLlyQ+I2rebDgdYbqRG1POKb9cP9RzU/hlqNMCLxKHInnl8MAVyKQKBgQCT
Jxsb4naFWQhs95s1pdb3Q7pIy9VzZ+KGP9Fx3AH91CI5Mrp/uI/rclPlnhPJWjTh
uK6BQA/vQQPg9DF0aTHk4UzLnvZ+dyjeJXEJ00xwjO3DUViGlFzRA8+32LgAeWF/
BoSoRSdlmdL3FQfoNN+2wuwsVQnsXUbcarwxyesWjQKBgH5f2Dqjcyz9Y1Q7jS+f
QAfAjocPvQGINFMKKf25nQ7yyKELrYPzFltLe5/e6jFQcy7IXU7jrIOQmuXWlv3l
S6r7AUP5tzIWHrx5NJxf788/HK2d/xS/JxGEsmlACcLEGBLj1hn/LdUYnzXiMAi+
acIKunZfdeu3s95nsCD3HfSe
-----END PRIVATE KEY-----"#;

    #[derive(Clone, Debug, Serialize)]
    struct TestClaims {
        iss: String,
        aud: String,
        sub: String,
        exp: i64,
        iat: i64,
        email: Option<String>,
        name: Option<String>,
        roles: Vec<String>,
        groups: Vec<String>,
        permissions: Vec<String>,
        nyx_service_id: Option<String>,
        nyx_agent_id: Option<String>,
    }

    struct ManualClock {
        now: Mutex<Instant>,
    }

    impl ManualClock {
        fn new() -> Arc<Self> {
            Arc::new(Self {
                now: Mutex::new(Instant::now()),
            })
        }

        fn advance(&self, duration: Duration) {
            let mut now = self.now.lock().unwrap();
            *now += duration;
        }
    }

    impl Clock for ManualClock {
        fn now(&self) -> Instant {
            *self.now.lock().unwrap()
        }
    }

    struct StubJwksFetcher {
        responses: Mutex<VecDeque<KeyMap>>,
        fetch_count: AtomicUsize,
    }

    impl StubJwksFetcher {
        fn new(responses: Vec<KeyMap>) -> Arc<Self> {
            Arc::new(Self {
                responses: Mutex::new(VecDeque::from(responses)),
                fetch_count: AtomicUsize::new(0),
            })
        }

        fn fetch_count(&self) -> usize {
            self.fetch_count.load(Ordering::SeqCst)
        }
    }

    impl JwksFetcher for StubJwksFetcher {
        fn fetch<'a>(&'a self, _http: &'a reqwest::Client, _jwks_uri: &'a str) -> BoxJwksFetch<'a> {
            Box::pin(async move {
                self.fetch_count.fetch_add(1, Ordering::SeqCst);
                Ok(self
                    .responses
                    .lock()
                    .unwrap()
                    .pop_front()
                    .expect("stub JWKS response"))
            })
        }
    }

    fn test_claims() -> TestClaims {
        let now = Utc::now().timestamp();
        TestClaims {
            iss: ISSUER.to_string(),
            aud: AUD.to_string(),
            sub: "user_123".to_string(),
            exp: now + 300,
            iat: now - 10,
            email: Some("user@example.test".to_string()),
            name: Some("Test User".to_string()),
            roles: vec!["developer".to_string()],
            groups: vec!["eng".to_string()],
            permissions: vec!["workspace:read".to_string()],
            nyx_service_id: Some("svc_123".to_string()),
            nyx_agent_id: Some("agent_123".to_string()),
        }
    }

    fn keys_for(kid: &str, n: &str) -> KeyMap {
        let jwks = format!(
            r#"{{
                "keys": [{{
                    "kty": "RSA",
                    "use": "sig",
                    "kid": "{kid}",
                    "alg": "RS256",
                    "n": "{n}",
                    "e": "AQAB"
                }}]
            }}"#
        );
        let keys = materialize_jwks(serde_json::from_str::<JwkSet>(&jwks).unwrap());
        assert_eq!(keys.len(), 1);
        keys
    }

    fn encode_token(kid: Option<&str>, private_pem: &str, claims: &TestClaims) -> String {
        let mut header = Header::new(Algorithm::RS256);
        header.kid = kid.map(str::to_string);
        encode(
            &header,
            claims,
            &EncodingKey::from_rsa_pem(private_pem.as_bytes()).unwrap(),
        )
        .unwrap()
    }

    fn client_with(
        keys: KeyMap,
        fetcher: Arc<dyn JwksFetcher>,
        clock: Arc<dyn Clock>,
    ) -> JwksClient {
        JwksClient::new_for_test(ISSUER, AUD, JWKS_URI, keys, fetcher, clock)
    }

    #[tokio::test]
    async fn valid_token_returns_identity() {
        let clock = ManualClock::new();
        let fetcher = StubJwksFetcher::new(vec![]);
        let client = client_with(keys_for("kid-1", KEY1_N), fetcher, clock);
        let token = encode_token(Some("kid-1"), KEY1_PEM, &test_claims());

        let identity = client.verify(&token).await.unwrap();

        assert_eq!(identity.user_id, "user_123");
        assert_eq!(identity.email.as_deref(), Some("user@example.test"));
        assert_eq!(identity.name.as_deref(), Some("Test User"));
        assert_eq!(identity.roles, ["developer"]);
        assert_eq!(identity.groups, ["eng"]);
        assert_eq!(identity.permissions, ["workspace:read"]);
        assert_eq!(identity.nyx_service_id.as_deref(), Some("svc_123"));
        assert_eq!(identity.agent_id.as_deref(), Some("agent_123"));
    }

    #[tokio::test]
    async fn rejects_wrong_issuer_and_audience() {
        let clock = ManualClock::new();
        let fetcher = StubJwksFetcher::new(vec![]);
        let client = client_with(keys_for("kid-1", KEY1_N), fetcher, clock);

        let mut wrong_issuer = test_claims();
        wrong_issuer.iss = "https://other-issuer.test".to_string();
        let wrong_issuer_token = encode_token(Some("kid-1"), KEY1_PEM, &wrong_issuer);
        assert!(matches!(
            client.verify(&wrong_issuer_token).await,
            Err(JwtError::Invalid(_))
        ));

        let mut wrong_aud = test_claims();
        wrong_aud.aud = "http://other-audience.test".to_string();
        let wrong_aud_token = encode_token(Some("kid-1"), KEY1_PEM, &wrong_aud);
        assert!(matches!(
            client.verify(&wrong_aud_token).await,
            Err(JwtError::Invalid(_))
        ));
    }

    #[tokio::test]
    async fn rejects_expired_token() {
        let clock = ManualClock::new();
        let fetcher = StubJwksFetcher::new(vec![]);
        let client = client_with(keys_for("kid-1", KEY1_N), fetcher, clock);

        let mut expired = test_claims();
        expired.exp = Utc::now().timestamp() - 120;
        let token = encode_token(Some("kid-1"), KEY1_PEM, &expired);

        assert!(matches!(
            client.verify(&token).await,
            Err(JwtError::Invalid(_))
        ));
    }

    #[tokio::test]
    async fn unknown_kid_triggers_jwks_refresh() {
        let clock = ManualClock::new();
        let fetcher = StubJwksFetcher::new(vec![keys_for("kid-2", KEY2_N)]);
        let client = client_with(keys_for("kid-1", KEY1_N), fetcher.clone(), clock);
        let token = encode_token(Some("kid-2"), KEY2_PEM, &test_claims());

        let identity = client.verify(&token).await.unwrap();

        assert_eq!(identity.user_id, "user_123");
        assert_eq!(fetcher.fetch_count(), 1);
    }

    #[tokio::test]
    async fn known_kid_refreshes_after_cache_ttl() {
        let clock = ManualClock::new();
        let fetcher = StubJwksFetcher::new(vec![keys_for("shared-kid", KEY2_N)]);
        let client = client_with(
            keys_for("shared-kid", KEY1_N),
            fetcher.clone(),
            clock.clone(),
        );
        let token = encode_token(Some("shared-kid"), KEY2_PEM, &test_claims());

        assert!(matches!(
            client.verify(&token).await,
            Err(JwtError::Invalid(_))
        ));
        assert_eq!(fetcher.fetch_count(), 0);

        clock.advance(JWKS_CACHE_TTL + Duration::from_secs(1));
        let identity = client.verify(&token).await.unwrap();

        assert_eq!(identity.user_id, "user_123");
        assert_eq!(fetcher.fetch_count(), 1);
    }

    #[tokio::test]
    async fn rejects_malformed_token_and_missing_kid() {
        let clock = ManualClock::new();
        let fetcher = StubJwksFetcher::new(vec![]);
        let client = client_with(keys_for("kid-1", KEY1_N), fetcher, clock);

        assert!(matches!(
            client.verify("not-a-jwt").await,
            Err(JwtError::Malformed(_))
        ));

        let token = encode_token(None, KEY1_PEM, &test_claims());
        assert!(matches!(
            client.verify(&token).await,
            Err(JwtError::MissingKid)
        ));
    }
}
