//! Who is calling: a bearer token, checked by a [`TokenVerifier`], becomes CrateStack's `auth()`.
//!
//! Fail closed. A request with no bearer token, or one the verifier does not accept, is a `401` before any read
//! or write runs: [`BearerAuth`] never answers an anonymous context. The verifier the service ships with,
//! [`RefuseAll`], accepts nothing, so until a real issuer is configured the API answers `401` to everyone (ADR
//! 0003, *Still open*: verifying a JWT against an issuer's keys).
//!
//! A [`Principal`] becomes `auth().sub` (the owner whose accounts the caller may read) and `auth().roles`
//! (`operator`, `producer`), as the schema's policies read them.

use std::collections::BTreeMap;
use std::future::Future;
use std::sync::Arc;

use cratestack::{AuthProvider, AuthRefusal, CratestackContext, CratestackError, RequestContext};

/// What a verified token says about its holder.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Principal {
    /// The subject: an owner id, such as an OpenID Connect `sub`.
    pub sub: String,
    /// Its roles: `operator`, `producer`, or none.
    pub roles: Vec<String>,
}

/// Why a token was refused. The token itself is never kept.
#[derive(Clone, Copy, Debug, PartialEq, Eq, thiserror::Error)]
pub enum TokenRefusal {
    /// The token is not one this verifier accepts.
    #[error("the bearer token is not accepted")]
    Invalid,
    /// The verifier cannot decide now (its issuer's keys cannot be read).
    #[error("the bearer token cannot be verified now")]
    Unavailable,
}

/// The port: turns a bearer token into a [`Principal`]. A JWT verifier against an issuer's keys is the
/// implementation still to come (ADR 0003).
pub trait TokenVerifier: Clone + Send + Sync + 'static {
    /// Verifies `token`.
    fn verify(&self, token: &str) -> impl Future<Output = Result<Principal, TokenRefusal>> + Send;
}

/// The default verifier: refuses every token, so the API serves nothing until a real verifier is wired.
#[derive(Clone, Copy, Debug, Default)]
pub struct RefuseAll;

impl TokenVerifier for RefuseAll {
    async fn verify(&self, _token: &str) -> Result<Principal, TokenRefusal> {
        Err(TokenRefusal::Invalid)
    }
}

/// A fixed table of tokens: **for tests only**. The service binary never builds one.
#[derive(Clone, Debug, Default)]
pub struct StaticTokens(Arc<BTreeMap<String, Principal>>);

impl StaticTokens {
    /// The table `tokens`.
    #[must_use]
    pub fn new(tokens: impl IntoIterator<Item = (String, Principal)>) -> Self {
        Self(Arc::new(tokens.into_iter().collect()))
    }
}

impl TokenVerifier for StaticTokens {
    async fn verify(&self, token: &str) -> Result<Principal, TokenRefusal> {
        self.0.get(token).cloned().ok_or(TokenRefusal::Invalid)
    }
}

/// CrateStack's `AuthProvider` over a [`TokenVerifier`]: `Authorization: Bearer <token>`, or a `401`.
#[derive(Clone, Debug)]
pub struct BearerAuth<V> {
    verifier: V,
}

impl<V: TokenVerifier> BearerAuth<V> {
    /// The provider over `verifier`.
    #[must_use]
    pub const fn new(verifier: V) -> Self {
        Self { verifier }
    }
}

/// The token of an `Authorization: Bearer <token>` header, or `None`.
#[must_use]
pub fn bearer(headers: &cratestack::axum::http::HeaderMap) -> Option<&str> {
    let mut values = headers
        .get_all(cratestack::axum::http::header::AUTHORIZATION)
        .iter();
    let (Some(value), None) = (values.next(), values.next()) else {
        return None;
    };
    let token = value.to_str().ok()?.strip_prefix("Bearer ")?.trim();
    (!token.is_empty()).then_some(token)
}

fn refused(message: &str) -> CratestackError {
    AuthRefusal::unauthenticated(message)
        .with_challenge("Bearer")
        .into_error()
}

impl<V: TokenVerifier> AuthProvider for BearerAuth<V> {
    type Error = CratestackError;

    async fn authenticate(
        &self,
        request: &RequestContext<'_>,
    ) -> Result<CratestackContext, CratestackError> {
        let Some(token) = bearer(request.headers) else {
            return Err(refused("a bearer token is required"));
        };
        match self.verifier.verify(token).await {
            Ok(principal) => Ok(CratestackContext::builder(principal.sub)
                .roles(principal.roles)
                .build()),
            Err(TokenRefusal::Invalid) => Err(refused("the bearer token is not accepted")),
            Err(TokenRefusal::Unavailable) => Err(CratestackError::Unavailable(
                "the bearer token cannot be verified now".to_owned(),
            )),
        }
    }

    fn credential<'r>(&self, request: &RequestContext<'r>) -> cratestack::Credential<'r> {
        cratestack::Credential::bearer(request.headers)
    }
}

#[cfg(test)]
mod tests {
    use cratestack::axum::http::{HeaderMap, HeaderValue, header};

    use super::*;

    #[test]
    fn a_bearer_token_is_one_header_with_one_scheme() {
        let mut headers = HeaderMap::new();
        assert_eq!(bearer(&headers), None);
        headers.insert(
            header::AUTHORIZATION,
            HeaderValue::from_static("Bearer abc"),
        );
        assert_eq!(bearer(&headers), Some("abc"));
        headers.insert(header::AUTHORIZATION, HeaderValue::from_static("Basic abc"));
        assert_eq!(bearer(&headers), None);
        headers.insert(header::AUTHORIZATION, HeaderValue::from_static("Bearer "));
        assert_eq!(bearer(&headers), None);
        headers.insert(header::AUTHORIZATION, HeaderValue::from_static("Bearer a"));
        headers.append(header::AUTHORIZATION, HeaderValue::from_static("Bearer b"));
        assert_eq!(bearer(&headers), None, "two credentials are none");
    }

    #[tokio::test]
    async fn the_default_verifier_refuses_everything() {
        assert_eq!(
            RefuseAll.verify("anything").await,
            Err(TokenRefusal::Invalid)
        );
        let tokens = StaticTokens::new([(
            "t".to_owned(),
            Principal {
                sub: "o1".to_owned(),
                roles: vec![],
            },
        )]);
        assert_eq!(
            tokens.verify("t").await.map(|p| p.sub).ok(),
            Some("o1".to_owned())
        );
        assert_eq!(tokens.verify("u").await, Err(TokenRefusal::Invalid));
    }
}
