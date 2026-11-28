//! The HTTP half: one request per call to vpay's `/v1`, and how vpay's answers map to the port's errors.
//!
//! *Verified 2026-11-27*, vpay `8b56831`, the `vpay-merchant-api` skill of `vaam-apps/vpay-skills` (`19f15ba`) and its `references/errors.md`, and
//! `backends/crates/vpay-api/src/v1/mod.rs`:
//!
//! - every `POST` under `/v1` needs an `Idempotency-Key` (at most 255 bytes of printable ASCII); a replay answers
//!   the first answer, and the same key with another body is a `400` of type `idempotency_error`;
//! - bodies are `application/x-www-form-urlencoded`, bracket-nested ([`crate::form`]);
//! - a token caller gets plain JSON, and errors as `{"error": {"type", "code", "message", "param"}}`;
//! - statuses: `400` invalid request (or idempotency), `401`/`403` the credential, `404` no such object (the same
//!   bytes for another merchant's), `409` a state conflict, `429` rate limited, `502`/`503` retry after a backoff,
//!   `500`/`501` a fault.
//!
//! No redirect is followed, and no error message holds the token or a response body.

use std::sync::Arc;
use std::time::Duration;

use openbilling_core::{ErrorClass, PaymentError, PaymentRefusal};
use reqwest::{StatusCode, Url, redirect};
use serde::de::DeserializeOwned;

use crate::form::{Form, percent_encode};
use crate::token::{AccessToken, Minter};
use crate::wire::ErrorEnvelope;
use crate::{ConfigError, Credential};

/// A fault of the adapter or of vpay, with fixed text.
#[derive(Debug, thiserror::Error)]
pub(crate) enum Fault {
    #[error("the vpay client key could not be used")]
    ClientKey,
    #[error("vpay refused to issue a token ({0})")]
    TokenRefused(&'static str),
    #[error("vpay's token endpoint answered {0}")]
    TokenStatus(StatusCode),
    #[error("vpay could not be reached")]
    Transport(#[source] reqwest::Error),
    #[error("the vpay token could not be read")]
    Token,
    #[error("vpay refused the credential")]
    Credential,
    #[error("vpay answered {0}")]
    Status(StatusCode),
    #[error("vpay answered something the adapter cannot read: {0}")]
    Unreadable(&'static str),
}

pub(crate) fn fault(class: ErrorClass, fault: Fault) -> PaymentError {
    PaymentError::Provider {
        class,
        source: Box::new(fault),
    }
}

/// An answer the adapter cannot read: a fault of the integration, not the caller's to fix.
pub(crate) fn unreadable(what: &'static str) -> PaymentError {
    fault(ErrorClass::Internal, Fault::Unreadable(what))
}

#[derive(Clone, Debug)]
pub(crate) struct Client {
    http: reqwest::Client,
    base: Url,
    auth: Auth,
}

/// Where a request's bearer token comes from.
#[derive(Clone, Debug)]
enum Auth {
    Token(String),
    File(std::path::PathBuf),
    Minted(Arc<Minter>),
}

pub(crate) fn checked_base(raw: &str) -> Result<Url, ConfigError> {
    let url = Url::parse(raw).map_err(|_| ConfigError::BaseUrl)?;
    let loopback = match url.host() {
        Some(url::Host::Ipv4(ip)) => ip.is_loopback(),
        Some(url::Host::Ipv6(ip)) => ip.is_loopback(),
        Some(url::Host::Domain(name)) => name == "localhost",
        None => false,
    };
    let scheme_ok = url.scheme() == "https" || (url.scheme() == "http" && loopback);
    if !scheme_ok
        || !url.username().is_empty()
        || url.password().is_some()
        || url.query().is_some()
        || url.fragment().is_some()
        || url.cannot_be_a_base()
    {
        return Err(ConfigError::BaseUrl);
    }
    Ok(url)
}

impl Client {
    pub(crate) fn new(
        base_url: &str,
        credential: Credential,
        timeout: Duration,
    ) -> Result<Self, ConfigError> {
        let base = checked_base(base_url)?;
        // reqwest is built without a crypto provider of its own (as CrateStack's client is): rustls uses the
        // process default, ring. Installing it again, or after another crate did, changes nothing.
        let _ = rustls::crypto::ring::default_provider().install_default();
        let http = reqwest::Client::builder()
            .timeout(timeout)
            .redirect(redirect::Policy::none())
            .build()
            .map_err(|_| ConfigError::Client)?;
        let auth = match credential {
            Credential::ClientCredentials(settings) => {
                Auth::Minted(Arc::new(Minter::new(http.clone(), &base, settings)?))
            }
            Credential::Token(token) => Auth::Token(token),
            Credential::TokenFile(path) => Auth::File(path),
        };
        Ok(Self { http, base, auth })
    }

    fn url(&self, path: &str) -> Result<Url, PaymentError> {
        let base = self.base.as_str().trim_end_matches('/');
        Url::parse(&format!("{base}{path}")).map_err(|_| unreadable("a URL"))
    }

    async fn token(&self) -> Result<AccessToken, PaymentError> {
        let token = match &self.auth {
            Auth::Minted(minter) => return minter.token().await,
            Auth::Token(token) => token.clone(),
            Auth::File(path) => tokio::fs::read_to_string(path)
                .await
                .map_err(|_| fault(ErrorClass::Unavailable, Fault::Token))?
                .trim()
                .to_owned(),
        };
        AccessToken::new(token).ok_or_else(|| fault(ErrorClass::Internal, Fault::Token))
    }

    /// Sends `request` with a bearer token, and drops a minted token vpay refused.
    async fn send<T: DeserializeOwned>(
        &self,
        request: impl FnOnce(&str) -> reqwest::RequestBuilder,
    ) -> Result<T, PaymentError> {
        let token = self.token().await?;
        let sent = request(token.expose()).send().await;
        if let (Auth::Minted(minter), Ok(response)) = (&self.auth, &sent)
            && response.status() == StatusCode::UNAUTHORIZED
        {
            minter.invalidate(&token).await;
        }
        answer(sent).await
    }

    /// `POST {path}` with `form` under `key`.
    pub(crate) async fn post<T: DeserializeOwned>(
        &self,
        path: &str,
        key: &str,
        form: &Form,
    ) -> Result<T, PaymentError> {
        let url = self.url(path)?;
        self.send(|token| {
            self.http
                .post(url)
                .bearer_auth(token)
                .header("idempotency-key", key)
                .header("content-type", "application/x-www-form-urlencoded")
                .body(form.encode())
        })
        .await
    }

    /// `GET {collection}/{id}`, the id escaped as a path segment.
    pub(crate) async fn get<T: DeserializeOwned>(
        &self,
        collection: &str,
        id: &str,
    ) -> Result<T, PaymentError> {
        let url = self.url(&format!("{collection}/{}", percent_encode(id)))?;
        self.send(|token| self.http.get(url).bearer_auth(token))
            .await
    }
}

async fn answer<T: DeserializeOwned>(
    sent: Result<reqwest::Response, reqwest::Error>,
) -> Result<T, PaymentError> {
    let response = sent.map_err(|error| fault(ErrorClass::Unavailable, Fault::Transport(error)))?;
    let status = response.status();
    let body = response
        .bytes()
        .await
        .map_err(|error| fault(ErrorClass::Unavailable, Fault::Transport(error)))?;
    if status.is_success() {
        return serde_json::from_slice(&body).map_err(|_| unreadable("a JSON object"));
    }
    let kind = serde_json::from_slice::<ErrorEnvelope>(&body)
        .ok()
        .and_then(|envelope| envelope.error.kind);
    Err(error_of(status, kind.as_deref()))
}

/// The port's error for a vpay error answer.
pub(crate) fn error_of(status: StatusCode, kind: Option<&str>) -> PaymentError {
    match status.as_u16() {
        400 if kind == Some("idempotency_error") => PaymentRefusal::Conflict.into(),
        400 | 422 => PaymentRefusal::InvalidRequest.into(),
        404 => PaymentRefusal::NotFound.into(),
        409 => PaymentRefusal::Conflict.into(),
        401 | 403 => fault(ErrorClass::Internal, Fault::Credential),
        408 | 429 | 502..=504 => fault(ErrorClass::Unavailable, Fault::Status(status)),
        _ => fault(ErrorClass::Internal, Fault::Status(status)),
    }
}

#[cfg(test)]
mod tests {
    use openbilling_core::Classify;

    use super::*;

    #[test]
    fn vpay_answers_map_to_classes() {
        let class =
            |code: u16, kind| error_of(StatusCode::from_u16(code).expect("a status"), kind).class();
        assert_eq!(class(400, None), ErrorClass::Invalid);
        assert_eq!(class(400, Some("idempotency_error")), ErrorClass::Conflict);
        assert_eq!(class(404, None), ErrorClass::Invalid);
        assert_eq!(class(409, None), ErrorClass::Conflict);
        assert_eq!(class(401, None), ErrorClass::Internal);
        assert_eq!(class(403, None), ErrorClass::Internal);
        assert_eq!(class(429, None), ErrorClass::Unavailable);
        assert_eq!(class(503, None), ErrorClass::Unavailable);
        assert_eq!(class(500, None), ErrorClass::Internal);
        assert_eq!(class(501, None), ErrorClass::Internal);
        assert_eq!(
            error_of(StatusCode::NOT_FOUND, None).refusal(),
            Some(PaymentRefusal::NotFound)
        );
    }

    #[test]
    fn a_base_url_is_https_or_loopback_http_with_no_credentials() {
        assert!(checked_base("https://vpay.example").is_ok());
        assert!(checked_base("https://vpay.example/prefix").is_ok());
        assert!(checked_base("http://localhost:1").is_ok());
        assert!(checked_base("http://[::1]:1").is_ok());
        for bad in [
            "http://vpay.example",
            "https://u:p@vpay.example",
            "https://vpay.example/?q",
            "mailto:x@y",
        ] {
            assert!(checked_base(bad).is_err(), "{bad}");
        }
    }
}
