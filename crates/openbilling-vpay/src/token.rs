//! Minting vpay's `/v1` bearer token inside the adapter ([ADR 0003], amended 2026-11-28).
//!
//! An OAuth 2.0 `client_credentials` grant whose client authenticates with an RS256 `private_key_jwt` (RFC 7523).
//! *Verified 2026-11-28* against vpay `7a9fee5` (`master`): `docs/flows/merchant-auth.md` (the handshake, the
//! assertion's claims, the request fields, the `400`/`401` RFC 6749 error body), `backends/crates/vpay-api/src/op/mod.rs`
//! (`ACCESS_TOKEN_TTL_SECS` is 900 and not configurable, `OP_GRANT_TYPES` is `client_credentials` alone,
//! `OP_TOKEN_ENDPOINT_AUTH_METHOD` is `private_key_jwt`, the issuer is `{public_base_url}/v1/oauth`),
//! `op/token.rs` (a request that names no `scope` is granted the client's registered scopes; `resource` is the
//! audience when `audience` is absent) and `op/clients.rs` (the registration, and a test that mints an RS256
//! assertion with `iss = sub = client_id`, `aud` the token endpoint, `jti` a UUID, `exp = iat + 60`).
//!
//! - The token endpoint is `{base}/v1/oauth/token`, a form body: `grant_type=client_credentials`, `client_id`,
//!   `client_assertion_type=urn:ietf:params:oauth:client-assertion-type:jwt-bearer`, `client_assertion`, and
//!   `audience=vpay:v1` (without it the token carries `aud = client_id` and every `/v1` call is a bare `401`),
//!   and `scope` only when configured.
//! - The assertion's `aud` is the OP's own name for itself: its token endpoint URL or its issuer, as a string. It
//!   defaults to the token endpoint this adapter posts to, which is right only when vpay is reached at the URL it
//!   publishes as its own: [`ClientCredentials::assertion_audience`] says otherwise.
//! - `kid` is optional with one registered key and required with several; it is sent when configured.
//! - Lifetime 1 to 300 s (vpay's default is 60, used here); `jti` fresh per assertion and spent once, so an
//!   assertion is never re-sent: a retry mints a new one.
//! - The answer is `{access_token, token_type: "Bearer", expires_in, scope}`; no refresh token (ADR 0010): the next
//!   token is a new assertion.
//!
//! The signature is made with ring, directly. jsonwebtoken's crypto provider is process-wide, and
//! `openbilling-jwt` installs one that verifies and refuses to sign: a second provider here would make one of the
//! two refuse to start. Nothing else of jsonwebtoken is needed to build a three-part JWS.
//!
//! The token is minted on first use, refreshed before it expires (by a configured margin, at most half its
//! lifetime), minted by one caller while the others wait for it, and a transient failure is retried with a bounded
//! backoff. A refusal (`400`, `401`) is a classed error that names vpay's RFC 6749 error code and nothing else.
//!
//! [ADR 0003]: https://github.com/vaam-apps/openbilling-rs/blob/main/docs/adr/0003-credits-from-a-payment-provider.md

use std::fmt;
use std::path::PathBuf;
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use base64::Engine as _;
use base64::engine::general_purpose::{STANDARD, URL_SAFE_NO_PAD};
use openbilling_core::{ErrorClass, PaymentError};
use reqwest::Url;
use ring::rand::{SecureRandom, SystemRandom};
use ring::signature::{RSA_PKCS1_SHA256, RsaKeyPair};
use serde::Deserialize;
use serde_json::json;
use tokio::sync::Mutex;

use crate::ConfigError;
use crate::client::{Fault, checked_base, fault};

/// The audience of a merchant token: vpay's `/v1` (`vpay_config::MERCHANT_AUDIENCE`).
pub const DEFAULT_AUDIENCE: &str = "vpay:v1";
/// How long before a token expires it is replaced, by default.
pub const DEFAULT_REFRESH_SKEW: Duration = Duration::from_secs(30);
/// How many times a transient failure of the token endpoint is retried, by default.
pub const DEFAULT_RETRIES: u32 = 2;
/// The pause before the first retry, by default; it doubles, up to [`MAX_BACKOFF`].
pub const DEFAULT_RETRY_BACKOFF: Duration = Duration::from_millis(200);
/// The longest pause between two attempts.
pub const MAX_BACKOFF: Duration = Duration::from_secs(5);
/// The longest refresh margin accepted: a token lives 15 minutes at vpay.
pub const MAX_REFRESH_SKEW: Duration = Duration::from_secs(600);
/// The most retries accepted.
pub const MAX_RETRIES: u32 = 10;
/// The lifetime of an assertion in seconds: vpay's default; it accepts 1 to 300.
const ASSERTION_LIFETIME_SECS: u64 = 60;
const ASSERTION_TYPE: &str = "urn:ietf:params:oauth:client-assertion-type:jwt-bearer";

/// A bearer token. It cannot be printed: `Debug` hides it and there is no `Display`.
#[derive(Clone, PartialEq, Eq)]
pub(crate) struct AccessToken(String);

impl AccessToken {
    /// A token; `None` when it is empty or holds anything but printable ASCII.
    pub(crate) fn new(value: impl Into<String>) -> Option<Self> {
        let value = value.into();
        (!value.is_empty() && value.bytes().all(|b| b.is_ascii_graphic())).then_some(Self(value))
    }

    pub(crate) fn expose(&self) -> &str {
        &self.0
    }
}

impl fmt::Debug for AccessToken {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str("AccessToken(<hidden>)")
    }
}

/// Where the client's RSA private key comes from.
#[derive(Clone, PartialEq, Eq)]
pub enum KeySource {
    /// A PEM file (PKCS#8 `PRIVATE KEY` or PKCS#1 `RSA PRIVATE KEY`, 2048 to 8192 bits), read each time a token is
    /// minted, so a rotated key needs no restart.
    File(PathBuf),
    /// A PEM held in memory: for tests. Never configured from a deployment's settings.
    Pem(String),
}

impl fmt::Debug for KeySource {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::File(path) => f.debug_tuple("File").field(path).finish(),
            Self::Pem(_) => f.write_str("Pem(<hidden>)"),
        }
    }
}

/// The settings of the `client_credentials` grant. Every field but the first two has a documented default.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ClientCredentials {
    /// vpay's `client_id` for this merchant (`merchant_clients[].client_id`).
    pub client_id: String,
    /// The client's RSA private key; its public half is registered at vpay as a JWK.
    pub key: KeySource,
    /// The `kid` of the registered key. Optional with one registered key, required with several.
    pub key_id: Option<String>,
    /// The token endpoint. Default: `{base_url}/v1/oauth/token`.
    pub token_url: Option<String>,
    /// The assertion's `aud`: the OP's own name for itself, its token endpoint URL or its issuer. Default: the
    /// token endpoint above; set it when vpay is reached by a URL other than the one it publishes as its own.
    pub assertion_audience: Option<String>,
    /// The audience of the token asked for. Default [`DEFAULT_AUDIENCE`].
    pub audience: String,
    /// The `scope` asked for. Default none: vpay then grants the client's registered scopes.
    pub scope: Option<String>,
    /// How long before expiry a token is replaced; at most half its lifetime in effect. Default
    /// [`DEFAULT_REFRESH_SKEW`], at most [`MAX_REFRESH_SKEW`].
    pub refresh_skew: Duration,
    /// How many times a transient failure is retried. Default [`DEFAULT_RETRIES`], at most [`MAX_RETRIES`].
    pub retries: u32,
    /// The pause before the first retry; it doubles up to [`MAX_BACKOFF`]. Default [`DEFAULT_RETRY_BACKOFF`].
    pub retry_backoff: Duration,
}

impl ClientCredentials {
    /// The settings for `client_id` and `key`, every other value at its default.
    #[must_use]
    pub fn new(client_id: impl Into<String>, key: KeySource) -> Self {
        Self {
            client_id: client_id.into(),
            key,
            key_id: None,
            token_url: None,
            assertion_audience: None,
            audience: DEFAULT_AUDIENCE.to_owned(),
            scope: None,
            refresh_skew: DEFAULT_REFRESH_SKEW,
            retries: DEFAULT_RETRIES,
            retry_backoff: DEFAULT_RETRY_BACKOFF,
        }
    }
}

fn token_text(value: &str) -> bool {
    !value.is_empty() && value.bytes().all(|b| b.is_ascii_graphic())
}

fn scope_text(value: &str) -> bool {
    !value.is_empty()
        && value.bytes().all(|b| b == b' ' || b.is_ascii_graphic())
        && !value.starts_with(' ')
        && !value.ends_with(' ')
}

/// A private key in PEM, PKCS#8 or PKCS#1, as ring reads it.
pub(crate) fn parse_key(pem: &str) -> Option<RsaKeyPair> {
    for (label, pkcs8) in [("PRIVATE KEY", true), ("RSA PRIVATE KEY", false)] {
        let begin = format!("-----BEGIN {label}-----");
        let end = format!("-----END {label}-----");
        let Some(start) = pem.find(&begin) else {
            continue;
        };
        let body = &pem[start + begin.len()..];
        let body = &body[..body.find(&end)?];
        let der = STANDARD
            .decode(
                body.bytes()
                    .filter(|b| !b.is_ascii_whitespace())
                    .collect::<Vec<_>>(),
            )
            .ok()?;
        return if pkcs8 {
            RsaKeyPair::from_pkcs8(&der).ok()
        } else {
            RsaKeyPair::from_der(&der).ok()
        };
    }
    None
}

fn load_sync(source: &KeySource) -> Option<RsaKeyPair> {
    match source {
        KeySource::File(path) => parse_key(&std::fs::read_to_string(path).ok()?),
        KeySource::Pem(pem) => parse_key(pem),
    }
}

/// Seconds since the Unix epoch.
fn now_secs() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_or(0, |d| d.as_secs())
}

/// A compact RS256 JWS over `claims`.
fn sign_jwt(
    key: &RsaKeyPair,
    key_id: Option<&str>,
    claims: &serde_json::Value,
) -> Result<String, ()> {
    let mut header = json!({"alg": "RS256", "typ": "JWT"});
    if let Some(kid) = key_id {
        header["kid"] = json!(kid);
    }
    let part = |value: &serde_json::Value| -> Result<String, ()> {
        serde_json::to_vec(value)
            .map(|bytes| URL_SAFE_NO_PAD.encode(bytes))
            .map_err(|_| ())
    };
    let signing_input = format!("{}.{}", part(&header)?, part(claims)?);
    let mut signature = vec![0; key.public().modulus_len()];
    key.sign(
        &RSA_PKCS1_SHA256,
        &SystemRandom::new(),
        signing_input.as_bytes(),
        &mut signature,
    )
    .map_err(|_| ())?;
    Ok(format!(
        "{signing_input}.{}",
        URL_SAFE_NO_PAD.encode(signature)
    ))
}

fn random_id() -> Result<String, ()> {
    let mut bytes = [0u8; 16];
    SystemRandom::new().fill(&mut bytes).map_err(|_| ())?;
    Ok(bytes.iter().map(|b| format!("{b:02x}")).collect())
}

#[derive(Deserialize)]
struct Issued {
    access_token: String,
    token_type: String,
    expires_in: u64,
}

#[derive(Deserialize)]
struct Refusal {
    error: Option<String>,
}

/// The RFC 6749 §5.2 error codes (and RFC 8707's `invalid_target`): the only text of vpay's answer that is ever
/// kept, because it is a closed vocabulary. `error_description` is never read.
fn error_code(code: &str) -> &'static str {
    match code {
        "invalid_request" => "invalid_request",
        "invalid_client" => "invalid_client",
        "invalid_grant" => "invalid_grant",
        "unauthorized_client" => "unauthorized_client",
        "unsupported_grant_type" => "unsupported_grant_type",
        "invalid_scope" => "invalid_scope",
        "invalid_target" => "invalid_target",
        _ => "unknown",
    }
}

struct Cached {
    token: AccessToken,
    refresh_at: Instant,
}

/// Mints, caches and refreshes the token. One mint at a time: callers that need a token while one is being
/// minted wait for it and use its result.
pub(crate) struct Minter {
    http: reqwest::Client,
    settings: ClientCredentials,
    token_url: Url,
    assertion_audience: String,
    cache: Mutex<Option<Cached>>,
}

impl fmt::Debug for Minter {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("Minter")
            .field("client_id", &self.settings.client_id)
            .field("token_url", &self.token_url.as_str())
            .finish_non_exhaustive()
    }
}

impl Minter {
    /// Checks `settings` and the key, which must be readable now; it is read again at each mint.
    pub(crate) fn new(
        http: reqwest::Client,
        base: &Url,
        settings: ClientCredentials,
    ) -> Result<Self, ConfigError> {
        let text_ok = |s: &str| token_text(s);
        if !text_ok(&settings.client_id)
            || settings.key_id.as_deref().is_some_and(|k| !text_ok(k))
            || !text_ok(&settings.audience)
            || settings.scope.as_deref().is_some_and(|s| !scope_text(s))
            || settings
                .assertion_audience
                .as_deref()
                .is_some_and(|a| !text_ok(a))
            || settings.refresh_skew > MAX_REFRESH_SKEW
            || settings.retries > MAX_RETRIES
            || settings.retry_backoff > MAX_BACKOFF
        {
            return Err(ConfigError::ClientCredentials);
        }
        let token_url = match &settings.token_url {
            Some(raw) => checked_base(raw).map_err(|_| ConfigError::TokenUrl)?,
            None => {
                let base = base.as_str().trim_end_matches('/');
                Url::parse(&format!("{base}/v1/oauth/token")).map_err(|_| ConfigError::TokenUrl)?
            }
        };
        if load_sync(&settings.key).is_none() {
            return Err(ConfigError::ClientKey);
        }
        let assertion_audience = settings
            .assertion_audience
            .clone()
            .unwrap_or_else(|| token_url.as_str().to_owned());
        Ok(Self {
            http,
            settings,
            token_url,
            assertion_audience,
            cache: Mutex::new(None),
        })
    }

    /// A token that is valid now: the cached one before its refresh time, else a new one.
    pub(crate) async fn token(&self) -> Result<AccessToken, PaymentError> {
        let mut cache = self.cache.lock().await;
        if let Some(cached) = cache.as_ref()
            && Instant::now() < cached.refresh_at
        {
            return Ok(cached.token.clone());
        }
        let (token, lifetime) = self.mint_with_retries().await?;
        let skew = self.settings.refresh_skew.min(lifetime / 2);
        *cache = Some(Cached {
            token: token.clone(),
            refresh_at: Instant::now() + (lifetime - skew),
        });
        Ok(token)
    }

    /// Forgets `token` if it is the cached one: vpay refused it.
    pub(crate) async fn invalidate(&self, token: &AccessToken) {
        let mut cache = self.cache.lock().await;
        if cache.as_ref().is_some_and(|c| &c.token == token) {
            *cache = None;
        }
    }

    async fn mint_with_retries(&self) -> Result<(AccessToken, Duration), PaymentError> {
        let mut pause = self.settings.retry_backoff;
        let mut attempt = 0;
        loop {
            match self.mint_once().await {
                Ok(minted) => return Ok(minted),
                Err(Attempt::Transient(_)) if attempt < self.settings.retries => {
                    attempt += 1;
                    tokio::time::sleep(pause).await;
                    pause = (pause * 2).min(MAX_BACKOFF);
                }
                Err(Attempt::Transient(error) | Attempt::Permanent(error)) => return Err(error),
            }
        }
    }

    async fn assertion(&self) -> Result<String, PaymentError> {
        let key = match &self.settings.key {
            KeySource::File(path) => tokio::fs::read_to_string(path)
                .await
                .map_err(|_| fault(ErrorClass::Unavailable, Fault::ClientKey))?,
            KeySource::Pem(pem) => pem.clone(),
        };
        let key = parse_key(&key).ok_or_else(|| fault(ErrorClass::Internal, Fault::ClientKey))?;
        let now = now_secs();
        let jti = random_id().map_err(|()| fault(ErrorClass::Internal, Fault::ClientKey))?;
        let claims = json!({
            "iss": self.settings.client_id,
            "sub": self.settings.client_id,
            "aud": self.assertion_audience,
            "jti": jti,
            "iat": now,
            "exp": now + ASSERTION_LIFETIME_SECS,
        });
        sign_jwt(&key, self.settings.key_id.as_deref(), &claims)
            .map_err(|()| fault(ErrorClass::Internal, Fault::ClientKey))
    }

    async fn mint_once(&self) -> Result<(AccessToken, Duration), Attempt> {
        let assertion = self.assertion().await.map_err(Attempt::Permanent)?;
        let body = {
            let mut form = url::form_urlencoded::Serializer::new(String::new());
            form.append_pair("grant_type", "client_credentials")
                .append_pair("client_id", &self.settings.client_id)
                .append_pair("client_assertion_type", ASSERTION_TYPE)
                .append_pair("client_assertion", &assertion)
                .append_pair("audience", &self.settings.audience);
            if let Some(scope) = &self.settings.scope {
                form.append_pair("scope", scope);
            }
            form.finish()
        };
        let response = self
            .http
            .post(self.token_url.clone())
            .header("content-type", "application/x-www-form-urlencoded")
            .header("accept", "application/json")
            .body(body)
            .send()
            .await
            .map_err(|e| Attempt::Transient(fault(ErrorClass::Unavailable, Fault::Transport(e))))?;
        let status = response.status();
        let body = response
            .bytes()
            .await
            .map_err(|e| Attempt::Transient(fault(ErrorClass::Unavailable, Fault::Transport(e))))?;
        if status.is_success() {
            return issued(&body).map_err(Attempt::Permanent);
        }
        match status.as_u16() {
            400 | 401 => {
                let code = serde_json::from_slice::<Refusal>(&body)
                    .ok()
                    .and_then(|r| r.error)
                    .map_or("unknown", |c| error_code(&c));
                Err(Attempt::Permanent(fault(
                    ErrorClass::Internal,
                    Fault::TokenRefused(code),
                )))
            }
            408 | 429 | 500 | 502..=504 => Err(Attempt::Transient(fault(
                ErrorClass::Unavailable,
                Fault::TokenStatus(status),
            ))),
            _ => Err(Attempt::Permanent(fault(
                ErrorClass::Internal,
                Fault::TokenStatus(status),
            ))),
        }
    }
}

enum Attempt {
    Transient(PaymentError),
    Permanent(PaymentError),
}

fn issued(body: &[u8]) -> Result<(AccessToken, Duration), PaymentError> {
    let unreadable = || fault(ErrorClass::Internal, Fault::Token);
    let issued: Issued = serde_json::from_slice(body).map_err(|_| unreadable())?;
    if !issued.token_type.eq_ignore_ascii_case("bearer") || issued.expires_in == 0 {
        return Err(unreadable());
    }
    let token = AccessToken::new(issued.access_token).ok_or_else(unreadable)?;
    Ok((token, Duration::from_secs(issued.expires_in)))
}

#[cfg(test)]
mod tests {
    use super::*;

    const KEY: &str = include_str!("../tests/vectors/test-client-key.pem");
    const OTHER_KEY: &str = include_str!("../tests/vectors/test-other-key.pem");

    #[test]
    fn both_pem_forms_load() {
        assert!(parse_key(KEY).is_some(), "PKCS#8");
        assert!(parse_key(OTHER_KEY).is_some(), "PKCS#1");
        assert!(
            parse_key("-----BEGIN PRIVATE KEY-----\nAAAA\n-----END PRIVATE KEY-----").is_none()
        );
        assert!(parse_key("not a key").is_none());
    }

    #[test]
    fn an_assertion_has_three_parts_and_vpays_claims() {
        let key = parse_key(KEY).unwrap();
        let claims = json!({"iss": "c", "sub": "c", "aud": "a", "jti": "j", "iat": 1, "exp": 61});
        let jwt = sign_jwt(&key, Some("k1"), &claims).unwrap();
        let parts: Vec<&str> = jwt.split('.').collect();
        assert_eq!(parts.len(), 3);
        let header: serde_json::Value =
            serde_json::from_slice(&URL_SAFE_NO_PAD.decode(parts[0]).unwrap()).unwrap();
        assert_eq!(header["alg"], "RS256");
        assert_eq!(header["kid"], "k1");
        assert_eq!(URL_SAFE_NO_PAD.decode(parts[2]).unwrap().len(), 256);
    }

    #[test]
    fn a_token_and_a_key_are_never_printed() {
        let token = AccessToken::new("secret-token-value").unwrap();
        assert_eq!(format!("{token:?}"), "AccessToken(<hidden>)");
        let source = KeySource::Pem(KEY.to_owned());
        assert!(!format!("{source:?}").contains("BEGIN"));
        let settings = ClientCredentials::new("client", source);
        assert!(!format!("{settings:?}").contains("MIIE"));
    }

    #[test]
    fn a_token_with_whitespace_or_nothing_is_refused() {
        assert!(AccessToken::new("").is_none());
        assert!(AccessToken::new("a b").is_none());
        assert!(AccessToken::new("a\nb").is_none());
        assert!(AccessToken::new("abc.def-ghi_").is_some());
    }

    #[test]
    fn only_rfc_6749_error_codes_are_kept() {
        assert_eq!(error_code("invalid_client"), "invalid_client");
        assert_eq!(error_code("anything with a secret in it"), "unknown");
    }
}
