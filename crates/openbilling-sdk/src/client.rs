//! The facade: base URL, bearer token, idempotency keys, [`Error`].

use std::fmt;
use std::sync::{Arc, RwLock};

use cratestack::client_rust::{
    AuthorizationRequest, ClientConfig, ClientError, CratestackClient, JsonCodec, RequestAuthorizer,
};
use url::Url;

use crate::error::Error;
use crate::generated::cratestack_schema::{client::Client as Generated, procedures};

/// Where the API is mounted on the service.
const API_PREFIX: &str = "v1";

/// The key a write is sent under (`Idempotency-Key`). The API replays the first answer to a key it has seen, so a
/// retry of a write that may or may not have arrived is safe under the same key, and unsafe under a new one.
#[derive(Clone, Debug, PartialEq, Eq, Hash)]
pub struct IdempotencyKey(String);

impl IdempotencyKey {
    /// A key of 1 to 255 visible ASCII characters.
    ///
    /// # Errors
    /// [`Error::IdempotencyKey`] for anything else.
    pub fn new(key: impl Into<String>) -> Result<Self, Error> {
        let key = key.into();
        if key.is_empty() {
            return Err(Error::IdempotencyKey("empty"));
        }
        if key.len() > 255 {
            return Err(Error::IdempotencyKey("longer than 255 characters"));
        }
        if !key.bytes().all(|b| b.is_ascii_graphic()) {
            return Err(Error::IdempotencyKey("not visible ASCII"));
        }
        Ok(Self(key))
    }

    /// The key as sent.
    #[must_use]
    pub fn as_str(&self) -> &str {
        &self.0
    }
}

/// The token, behind a lock so [`Client::set_token`] works on a shared client. Its `Debug` says nothing of it.
#[derive(Default)]
struct Token(RwLock<Option<String>>);

impl fmt::Debug for Token {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str("Token(..)")
    }
}

#[async_trait::async_trait]
impl RequestAuthorizer for Token {
    async fn authorize(
        &self,
        _request: &AuthorizationRequest,
    ) -> Result<Vec<(String, String)>, ClientError> {
        let token = self
            .0
            .read()
            .map_or_else(|e| e.into_inner().clone(), |g| g.clone());
        Ok(token
            .map(|t| vec![("authorization".to_owned(), format!("Bearer {t}"))])
            .unwrap_or_default())
    }
}

/// A client of the openbilling API. Cheap to clone; clones share the token.
#[derive(Clone)]
pub struct Client {
    raw: Generated<JsonCodec>,
    token: Arc<Token>,
}

impl fmt::Debug for Client {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str("Client(..)")
    }
}

impl Client {
    /// A client of the service at `base_url` (its root, such as `https://billing.example.com`: `/v1` is added).
    /// It sends no token until [`Client::set_token`] is called.
    ///
    /// # Errors
    /// [`Error::BaseUrl`] when it is not an `http` or `https` URL.
    pub fn new(base_url: &str) -> Result<Self, Error> {
        let mut url = Url::parse(base_url).map_err(|e| Error::BaseUrl(e.to_string()))?;
        if !matches!(url.scheme(), "http" | "https") || url.cannot_be_a_base() {
            return Err(Error::BaseUrl("not an http or https URL".to_owned()));
        }
        if url.username() != "" || url.password().is_some() {
            return Err(Error::BaseUrl("a URL with credentials in it".to_owned()));
        }
        let path = format!("{}/{API_PREFIX}", url.path().trim_end_matches('/'));
        url.set_path(&path);
        let token = Arc::new(Token::default());
        let runtime = CratestackClient::new(ClientConfig::new(url), JsonCodec)
            .with_request_authorizer(token.clone());
        Ok(Self {
            raw: Generated::new(runtime),
            token,
        })
    }

    /// Sets the bearer token every later call sends, replacing the last. A token is a JWT the service's issuer
    /// signed, for the service's audience; refreshing one is the caller's, by calling this again.
    pub fn set_token(&self, token: impl Into<String>) {
        let mut slot = self.token.0.write().unwrap_or_else(|e| e.into_inner());
        *slot = Some(token.into());
    }

    /// Stops sending a token.
    pub fn clear_token(&self) {
        let mut slot = self.token.0.write().unwrap_or_else(|e| e.into_inner());
        *slot = None;
    }

    /// The generated client, for the models (`accounts`, `lots`, `usage_outcomes`: list and get) and for anything the
    /// facade does not wrap. The bearer token is attached; a write made through it needs its own `Idempotency-Key`
    /// header.
    #[must_use]
    pub fn raw(&self) -> &Generated<JsonCodec> {
        &self.raw
    }
}

/// A read: `args` in, the generated answer out.
macro_rules! reads {
    ($($(#[$doc:meta])* $method:ident),* $(,)?) => {$(
        $(#[$doc])*
        ///
        /// # Errors
        /// [`Error`]; its [`class`](crate::Classify::class) says what to do.
        pub async fn $method(
            &self,
            args: &procedures::$method::Args,
        ) -> Result<procedures::$method::Output, Error> {
            Ok(self.raw.procedures().$method(args, &[]).await?)
        }
    )*};
}

/// A write: `args` and the key in, the generated answer out. The key travels as `Idempotency-Key`.
macro_rules! writes {
    ($($(#[$doc:meta])* $method:ident),* $(,)?) => {$(
        $(#[$doc])*
        ///
        /// # Errors
        /// [`Error`]; its [`class`](crate::Classify::class) says what to do. After an
        /// [`Unavailable`](crate::ErrorClass::Unavailable) error, send the same call again under the same key.
        pub async fn $method(
            &self,
            args: &procedures::$method::Args,
            key: &IdempotencyKey,
        ) -> Result<procedures::$method::Output, Error> {
            Ok(self
                .raw
                .procedures()
                .$method(args, &[("idempotency-key", key.as_str())])
                .await?)
        }
    )*};
}

impl Client {
    reads! {
        /// `getBalance`: the drawable credit and the debt of an owner's accounts in a currency.
        get_balance,
        /// `getAllowed`: whether a member may spend now, with the numbers the verdict was made from.
        get_allowed,
        /// `listCredits`: an owner's credit lots.
        list_credits,
        /// `getStatement`: a month's statement lines.
        get_statement,
        /// `getUsageSeries`: usage per day.
        get_usage_series,
    }

    writes! {
        /// `configure`: the currencies, account labels and draw order.
        configure,
        /// `openAccount`: opens an account for an owner.
        open_account,
        /// `grantCredit`: books a promotion or an adjustment.
        grant_credit,
        /// `recordUsage`: takes in a batch of pre-priced usage events.
        record_usage,
        /// `recordRawUsage`: takes in raw usage events, rated by the price book at intake.
        record_raw_usage,
        /// `setDebtLimit`: an owner's debt limit.
        set_debt_limit,
        /// `setMemberCap`: a member's monthly cap.
        set_member_cap,
        /// `loadPriceList`: loads a YAML price list as a new version.
        load_price_list,
        /// `startTopUp`: asks the payment provider for a payment and its checkout.
        start_top_up,
        /// `reconcilePayment`: reads a payment back and books it if it succeeded.
        reconcile_payment,
        /// `resolveTopUp`: closes a mismatched top-up once.
        resolve_top_up,
        /// `reconcileRefund`: reads a refund back and books it if it succeeded.
        reconcile_refund,
    }
}
