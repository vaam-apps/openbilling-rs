//! A JWT bearer verifier for the API of openbilling-rs: [`JwtVerifier`], an implementation of
//! [`TokenVerifier`](openbilling_http::auth::TokenVerifier) against one issuer's keys ([ADR 0003], amended).
//!
//! **Fail closed.** A token is accepted only when every one of these holds:
//!
//! - it is a compact JWS whose header names `RS256`, `ES256` or `EdDSA` (never `none`, never an `HS*` algorithm),
//!   carries a `kid`, and carries no `crit`;
//! - the `kid` is a signature key of the issuer's JWKS, of the header's algorithm, and the signature verifies (ring);
//! - `iss` is exactly the configured issuer and `aud` holds exactly the configured audience;
//! - `exp` is there and has not passed, and `nbf`, when there, has (60 seconds of leeway, both ways);
//! - `sub` is there and is an owner id (1 to 255 bytes of printable ASCII).
//!
//! The roles are read from a configured claim (a dotted path, `roles` by default, such as `realm_access.roles`), an
//! array of strings or one space-separated string, and mapped through a [`RoleMap`] to the API's two roles,
//! `operator` and `producer`; a value the map does not name is dropped. An owner needs no role: the `sub` is the
//! owner whose accounts the caller reads.
//!
//! **The keys.** The JWKS is found through the issuer's discovery document (or a configured URL), fetched on first
//! use, kept for [`JwtConfig::refresh`], and fetched again early when a token names a `kid` it lacks, at most once
//! per [`JwtConfig::min_refetch`]. While the issuer cannot be reached the last keys serve for
//! [`JwtConfig::stale_for`] more; after that, and before any key was ever read, a token is
//! [`TokenRefusal::Unavailable`] (a `503`), never accepted.
//!
//! The signature is ring's, through a jsonwebtoken crypto provider of this crate's ([`provider`]); jsonwebtoken
//! checks the claims. With the feature `test-issuer`, [`test_issuer`] is an issuer on a loopback port for tests.
//!
//! [ADR 0003]: https://github.com/vaam-apps/openbilling-rs/blob/main/docs/adr/0003-credits-from-a-payment-provider.md

pub mod jwks;
pub mod provider;
#[cfg(feature = "test-issuer")]
pub mod test_issuer;

use std::collections::BTreeMap;
use std::sync::Arc;
use std::time::{Duration, Instant};

use base64::Engine;
use base64::engine::general_purpose::URL_SAFE_NO_PAD;
use jsonwebtoken::Validation;
use jsonwebtoken::errors::ErrorKind;
use openbilling_core::{Classify, ErrorClass, OwnerId};
use openbilling_http::auth::{Principal, TokenRefusal, TokenVerifier};
use serde_json::{Map, Value};
use tokio::sync::RwLock;
use url::Url;

use crate::jwks::Key;

/// The longest token looked at; a longer one is refused unread.
pub const MAX_TOKEN_BYTES: usize = 16 * 1024;

/// One of the API's roles, as its schema's policies name them.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub enum Role {
    /// Reads and runs everything.
    Operator,
    /// Sends usage and asks whether a member may spend.
    Producer,
}

impl Role {
    /// The role's name in the schema: `operator` or `producer`.
    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Operator => "operator",
            Self::Producer => "producer",
        }
    }
}

/// How the issuer's role values become the API's roles. A value it does not name gives no role.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct RoleMap(BTreeMap<String, Role>);

impl Default for RoleMap {
    /// `operator` is [`Role::Operator`] and `producer` is [`Role::Producer`].
    fn default() -> Self {
        Self(BTreeMap::from([
            ("operator".to_owned(), Role::Operator),
            ("producer".to_owned(), Role::Producer),
        ]))
    }
}

impl RoleMap {
    /// A map of the issuer's values to roles.
    #[must_use]
    pub fn new(entries: impl IntoIterator<Item = (String, Role)>) -> Self {
        Self(entries.into_iter().collect())
    }

    /// Reads `value=role,value=role`, such as `billing-admin=operator,meter=producer`.
    ///
    /// # Errors
    ///
    /// [`SetupError::RoleMap`] for an entry that is not `value=operator` or `value=producer`.
    pub fn parse(text: &str) -> Result<Self, SetupError> {
        text.split(',')
            .map(str::trim)
            .filter(|entry| !entry.is_empty())
            .map(|entry| {
                let (value, role) = entry.split_once('=').ok_or(SetupError::RoleMap)?;
                let role = match role.trim() {
                    "operator" => Role::Operator,
                    "producer" => Role::Producer,
                    _ => return Err(SetupError::RoleMap),
                };
                let value = value.trim();
                if value.is_empty() {
                    return Err(SetupError::RoleMap);
                }
                Ok((value.to_owned(), role))
            })
            .collect::<Result<BTreeMap<_, _>, _>>()
            .map(Self)
    }

    fn role(&self, value: &str) -> Option<Role> {
        self.0.get(value).copied()
    }
}

/// What the verifier checks tokens against.
#[derive(Clone, Debug)]
pub struct JwtConfig {
    /// The issuer: `iss` must be exactly this text.
    pub issuer: String,
    /// The audience: `aud` must hold exactly this text.
    pub audience: String,
    /// The JWKS. `None`: the `jwks_uri` of the issuer's discovery document.
    pub jwks_url: Option<String>,
    /// The claim that holds the roles, as a dotted path into the claims (`roles`, `realm_access.roles`).
    pub roles_claim: String,
    /// How its values map to roles.
    pub roles: RoleMap,
    /// How long fetched keys are used before they are fetched again.
    pub refresh: Duration,
    /// How long past `refresh` the last keys still serve while the issuer cannot be reached.
    pub stale_for: Duration,
    /// The least time between two fetches, so an unknown `kid` cannot make the verifier hammer the issuer.
    pub min_refetch: Duration,
    /// How long one fetch may take.
    pub timeout: Duration,
}

impl JwtConfig {
    /// A configuration for `issuer` and `audience`, with every other setting at its default: discovery, the claim
    /// `roles`, the default [`RoleMap`], keys refreshed every 5 minutes, served stale for an hour, fetched at most
    /// every 30 seconds, each fetch at most 5 seconds.
    #[must_use]
    pub fn new(issuer: impl Into<String>, audience: impl Into<String>) -> Self {
        Self {
            issuer: issuer.into(),
            audience: audience.into(),
            jwks_url: None,
            roles_claim: "roles".to_owned(),
            roles: RoleMap::default(),
            refresh: Duration::from_secs(300),
            stale_for: Duration::from_secs(3600),
            min_refetch: Duration::from_secs(30),
            timeout: Duration::from_secs(5),
        }
    }
}

/// Why a verifier could not be built. The messages are fixed text.
#[derive(Clone, Copy, Debug, PartialEq, Eq, thiserror::Error)]
pub enum SetupError {
    /// The issuer is not an `https` URL (or `http` to a loopback host), or carries credentials.
    #[error("the issuer is not an https URL")]
    Issuer,
    /// The audience is empty.
    #[error("the audience is empty")]
    Audience,
    /// The JWKS URL is not an `https` URL (or `http` to a loopback host).
    #[error("the JWKS URL is not an https URL")]
    JwksUrl,
    /// The roles claim is not a dotted path of names.
    #[error("the roles claim is not a dotted path")]
    RolesClaim,
    /// The role map has an entry that is not `value=operator` or `value=producer`.
    #[error("the role map is not a list of value=operator or value=producer")]
    RoleMap,
    /// Another jsonwebtoken crypto provider was installed in this process.
    #[error("another jsonwebtoken crypto provider is installed")]
    Provider,
    /// The HTTP client could not be built.
    #[error("the HTTP client could not be built")]
    Client,
}

impl Classify for SetupError {
    fn class(&self) -> ErrorClass {
        match self {
            Self::Provider | Self::Client => ErrorClass::Internal,
            _ => ErrorClass::Invalid,
        }
    }
}

/// Why a token was refused: each check has its own reason, so a test can tell them apart. The API answers every
/// one of them as a `401`, except [`JwtRefusal::Unavailable`], a `503`. A reason never holds the token or a claim.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, thiserror::Error)]
pub enum JwtRefusal {
    /// Not a compact JWS, too long, or a header with `crit`.
    #[error("the token is malformed")]
    Malformed,
    /// An algorithm that is not `RS256`, `ES256` or `EdDSA`, or not the algorithm of the key it names.
    #[error("the token's algorithm is not accepted")]
    Algorithm,
    /// No `kid`, or one the issuer's JWKS does not hold.
    #[error("the token names no known key")]
    UnknownKey,
    /// The signature does not verify.
    #[error("the token's signature does not verify")]
    Signature,
    /// `exp` has passed.
    #[error("the token has expired")]
    Expired,
    /// `nbf` has not come yet.
    #[error("the token is not valid yet")]
    NotYetValid,
    /// `iss` is not the issuer.
    #[error("the token's issuer is not accepted")]
    Issuer,
    /// `aud` does not hold the audience.
    #[error("the token's audience is not accepted")]
    Audience,
    /// A claim that must be there is not (`exp`, `iss`, `aud`, `sub`).
    #[error("the token lacks a required claim")]
    MissingClaim,
    /// `sub` is not an owner id.
    #[error("the token's subject is not an owner id")]
    Subject,
    /// The issuer's keys cannot be read now, and none recent enough are held.
    #[error("the issuer's keys cannot be read now")]
    Unavailable,
}

impl Classify for JwtRefusal {
    fn class(&self) -> ErrorClass {
        match self {
            Self::Unavailable => ErrorClass::Unavailable,
            _ => ErrorClass::Invalid,
        }
    }
}

struct Fetched {
    keys: Vec<Key>,
    at: Instant,
}

#[derive(Default)]
struct State {
    jwks_url: Option<Url>,
    fetched: Option<Fetched>,
    last_attempt: Option<Instant>,
}

struct Inner {
    config: JwtConfig,
    issuer: Url,
    claim_path: Vec<String>,
    http: reqwest::Client,
    state: RwLock<State>,
}

/// The verifier. Clones share one key cache.
#[derive(Clone)]
pub struct JwtVerifier {
    inner: Arc<Inner>,
}

impl std::fmt::Debug for JwtVerifier {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("JwtVerifier")
            .field("issuer", &self.inner.config.issuer)
            .field("audience", &self.inner.config.audience)
            .finish_non_exhaustive()
    }
}

fn claim_path(claim: &str) -> Option<Vec<String>> {
    let parts: Vec<String> = claim.split('.').map(str::to_owned).collect();
    let name = |part: &String| {
        !part.is_empty()
            && part.len() <= 64
            && part
                .bytes()
                .all(|b| b.is_ascii_alphanumeric() || matches!(b, b'_' | b'-' | b':'))
    };
    (!parts.is_empty() && parts.len() <= 8 && parts.iter().all(name)).then_some(parts)
}

fn refusal_of(error: &jsonwebtoken::errors::Error) -> JwtRefusal {
    match error.kind() {
        ErrorKind::InvalidSignature => JwtRefusal::Signature,
        ErrorKind::InvalidAlgorithm | ErrorKind::InvalidAlgorithmName => JwtRefusal::Algorithm,
        ErrorKind::ExpiredSignature => JwtRefusal::Expired,
        ErrorKind::ImmatureSignature => JwtRefusal::NotYetValid,
        ErrorKind::InvalidIssuer => JwtRefusal::Issuer,
        ErrorKind::InvalidAudience => JwtRefusal::Audience,
        ErrorKind::MissingRequiredClaim(_) => JwtRefusal::MissingClaim,
        _ => JwtRefusal::Malformed,
    }
}

/// The header's own JSON, read before jsonwebtoken reads it: a header with `crit` is refused (nothing here
/// understands an extension), and so is an `alg` outside [`provider::ALGORITHMS`], `none` among them.
fn check_header(token: &str) -> Result<(), JwtRefusal> {
    let encoded = token.split('.').next().ok_or(JwtRefusal::Malformed)?;
    let bytes = URL_SAFE_NO_PAD
        .decode(encoded)
        .map_err(|_| JwtRefusal::Malformed)?;
    let header: Map<String, Value> =
        serde_json::from_slice(&bytes).map_err(|_| JwtRefusal::Malformed)?;
    if header.contains_key("crit") {
        return Err(JwtRefusal::Malformed);
    }
    match header.get("alg").and_then(Value::as_str) {
        Some("RS256" | "ES256" | "EdDSA") => Ok(()),
        Some(_) => Err(JwtRefusal::Algorithm),
        None => Err(JwtRefusal::Malformed),
    }
}

impl JwtVerifier {
    /// A verifier for `config`. Nothing is fetched yet: the keys are read on the first token.
    ///
    /// # Errors
    ///
    /// A [`SetupError`] for a configuration it will not run with.
    pub fn new(config: JwtConfig) -> Result<Self, SetupError> {
        let issuer = jwks::checked_url(&config.issuer).ok_or(SetupError::Issuer)?;
        if config.audience.trim().is_empty() {
            return Err(SetupError::Audience);
        }
        let jwks_url = match &config.jwks_url {
            Some(raw) => Some(jwks::checked_url(raw).ok_or(SetupError::JwksUrl)?),
            None => None,
        };
        let claim_path = claim_path(&config.roles_claim).ok_or(SetupError::RolesClaim)?;
        if !provider::install() {
            return Err(SetupError::Provider);
        }
        // reqwest is built without a crypto provider of its own: rustls uses the process default, ring.
        let _ = rustls::crypto::ring::default_provider().install_default();
        let http = reqwest::Client::builder()
            .timeout(config.timeout)
            .redirect(reqwest::redirect::Policy::none())
            .build()
            .map_err(|_| SetupError::Client)?;
        Ok(Self {
            inner: Arc::new(Inner {
                issuer,
                claim_path,
                http,
                state: RwLock::new(State {
                    jwks_url,
                    ..State::default()
                }),
                config,
            }),
        })
    }

    /// The configuration.
    #[must_use]
    pub fn config(&self) -> &JwtConfig {
        &self.inner.config
    }

    /// Verifies `token` and answers its principal, or why it is refused.
    ///
    /// # Errors
    ///
    /// The [`JwtRefusal`] of the first check that fails.
    pub async fn check(&self, token: &str) -> Result<Principal, JwtRefusal> {
        if token.len() > MAX_TOKEN_BYTES {
            return Err(JwtRefusal::Malformed);
        }
        check_header(token)?;
        let header = jsonwebtoken::decode_header(token).map_err(|_| JwtRefusal::Malformed)?;
        if !provider::ALGORITHMS.contains(&header.alg) {
            return Err(JwtRefusal::Algorithm);
        }
        let kid = header.kid.as_deref().ok_or(JwtRefusal::UnknownKey)?;
        let key = self.key(kid).await?;
        if key.algorithm != header.alg {
            return Err(JwtRefusal::Algorithm);
        }
        let config = &self.inner.config;
        let mut validation = Validation::new(key.algorithm);
        validation.set_issuer(&[config.issuer.as_str()]);
        validation.set_audience(&[config.audience.as_str()]);
        validation.set_required_spec_claims(&["exp", "iss", "aud", "sub"]);
        validation.validate_exp = true;
        validation.validate_nbf = true;
        validation.leeway = 60;
        let data = jsonwebtoken::decode::<Map<String, Value>>(token, &key.decoding, &validation)
            .map_err(|e| refusal_of(&e))?;
        let claims = data.claims;
        let sub = claims
            .get("sub")
            .and_then(Value::as_str)
            .ok_or(JwtRefusal::Subject)?;
        let sub = OwnerId::new(sub).map_err(|_| JwtRefusal::Subject)?;
        Ok(Principal {
            sub: sub.as_str().to_owned(),
            roles: self.roles(&claims),
        })
    }

    fn roles(&self, claims: &Map<String, Value>) -> Vec<String> {
        let mut value = None;
        let mut level = Some(claims);
        for part in &self.inner.claim_path {
            value = level.and_then(|map| map.get(part));
            level = value.and_then(Value::as_object);
        }
        let values: Vec<&str> = match value {
            Some(Value::Array(items)) => items.iter().filter_map(Value::as_str).collect(),
            Some(Value::String(text)) => text.split_ascii_whitespace().collect(),
            _ => Vec::new(),
        };
        let mut roles: Vec<Role> = values
            .into_iter()
            .filter_map(|v| self.inner.config.roles.role(v))
            .collect();
        roles.sort_unstable();
        roles.dedup();
        roles.into_iter().map(|r| r.as_str().to_owned()).collect()
    }

    /// The key `kid` names: from the cache while it is fresh, fetched again when it is stale or lacks `kid` (at most
    /// once per `min_refetch`), and from a stale cache while the issuer cannot be reached, for `stale_for`.
    async fn key(&self, kid: &str) -> Result<Key, JwtRefusal> {
        let config = &self.inner.config;
        let find = |fetched: &Fetched| fetched.keys.iter().find(|k| k.kid == kid).cloned();
        let may_fetch = |state: &State| {
            state
                .last_attempt
                .is_none_or(|at| at.elapsed() >= config.min_refetch)
        };
        {
            let state = self.inner.state.read().await;
            if let Some(fetched) = &state.fetched
                && fetched.at.elapsed() < config.refresh
            {
                if let Some(key) = find(fetched) {
                    return Ok(key);
                }
                if !may_fetch(&state) {
                    return Err(JwtRefusal::UnknownKey);
                }
            }
        }
        let mut state = self.inner.state.write().await;
        let fresh = state
            .fetched
            .as_ref()
            .is_some_and(|f| f.at.elapsed() < config.refresh);
        if fresh && let Some(key) = state.fetched.as_ref().and_then(find) {
            return Ok(key);
        }
        let mut just_fetched = false;
        if may_fetch(&state) {
            state.last_attempt = Some(Instant::now());
            if let Some(keys) = self.fetch(&mut state).await {
                state.fetched = Some(Fetched {
                    keys,
                    at: Instant::now(),
                });
                just_fetched = true;
            }
        }
        match &state.fetched {
            Some(fetched)
                if just_fetched || fetched.at.elapsed() < config.refresh + config.stale_for =>
            {
                find(fetched).ok_or(JwtRefusal::UnknownKey)
            }
            _ => Err(JwtRefusal::Unavailable),
        }
    }

    /// Reads the JWKS, through discovery the first time when no URL is configured. `None` when anything fails.
    async fn fetch(&self, state: &mut State) -> Option<Vec<Key>> {
        let url = match &state.jwks_url {
            Some(url) => url.clone(),
            None => {
                let discovery = jwks::discovery_url(&self.inner.issuer)?;
                let document = self.get(discovery).await?;
                let url = jwks::jwks_uri(&document, &self.inner.config.issuer)?;
                state.jwks_url = Some(url.clone());
                url
            }
        };
        jwks::parse(&self.get(url).await?)
    }

    /// `GET url`: a `200` with at most [`jwks::MAX_DOCUMENT_BYTES`], or `None`.
    async fn get(&self, url: Url) -> Option<Vec<u8>> {
        let mut response = self.inner.http.get(url).send().await.ok()?;
        if response.status() != reqwest::StatusCode::OK {
            return None;
        }
        let mut body = Vec::new();
        while let Some(chunk) = response.chunk().await.ok()? {
            if body.len() + chunk.len() > jwks::MAX_DOCUMENT_BYTES {
                return None;
            }
            body.extend_from_slice(&chunk);
        }
        Some(body)
    }
}

impl TokenVerifier for JwtVerifier {
    async fn verify(&self, token: &str) -> Result<Principal, TokenRefusal> {
        self.check(token).await.map_err(|refusal| match refusal {
            JwtRefusal::Unavailable => TokenRefusal::Unavailable,
            _ => TokenRefusal::Invalid,
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_role_map_names_only_the_two_roles() {
        let map = RoleMap::parse("billing-admin=operator, meter=producer").unwrap();
        assert_eq!(map.role("billing-admin"), Some(Role::Operator));
        assert_eq!(map.role("meter"), Some(Role::Producer));
        assert_eq!(map.role("operator"), None);
        for bad in ["x", "x=owner", "=operator", "x=Operator"] {
            assert_eq!(RoleMap::parse(bad), Err(SetupError::RoleMap), "{bad}");
        }
        assert_eq!(RoleMap::parse("").unwrap(), RoleMap::new([]));
    }

    #[test]
    fn a_roles_claim_is_a_dotted_path_of_names() {
        assert_eq!(
            claim_path("realm_access.roles").unwrap(),
            ["realm_access", "roles"]
        );
        assert!(claim_path("https://example.com/roles").is_none());
        assert!(claim_path("a..b").is_none());
        assert!(claim_path("").is_none());
    }

    #[test]
    fn the_configuration_is_checked_before_anything_is_fetched() {
        let ok = JwtConfig::new("https://idp.example", "billing");
        assert!(JwtVerifier::new(ok.clone()).is_ok());
        let plain = JwtConfig::new("http://idp.example", "billing");
        assert_eq!(JwtVerifier::new(plain).err(), Some(SetupError::Issuer));
        let blank = JwtConfig::new("https://idp.example", " ");
        assert_eq!(JwtVerifier::new(blank).err(), Some(SetupError::Audience));
        let mut jwks = ok.clone();
        jwks.jwks_url = Some("http://idp.example/keys".to_owned());
        assert_eq!(JwtVerifier::new(jwks).err(), Some(SetupError::JwksUrl));
        let mut claim = ok;
        claim.roles_claim = "a b".to_owned();
        assert_eq!(JwtVerifier::new(claim).err(), Some(SetupError::RolesClaim));
    }
}
