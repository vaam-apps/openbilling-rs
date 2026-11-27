//! The vpay adapter of the payment provider port, [`PaymentProvider`] ([ADR 0003]).
//!
//! [vpay](https://github.com/vaam-apps/vpay) is a payment orchestrator with a Stripe-shaped merchant API. A top-up
//! is a vpay PaymentIntent with a hosted Checkout Session for the payer; the intent's `metadata` carries the owner
//! and label of the account it credits, and is read back with it. A succeeded intent becomes a `payment` credit
//! keyed by its `pi_…` id; a succeeded refund becomes a reversal keyed by its `re_…` id. vpay's webhooks are
//! verified exactly as vpay specifies ([`webhook`]) and then read only for the id to fetch again.
//!
//! Every fact about vpay is marked *verified* with the vpay revision and file it was read in; the revision is
//! `8b56831` (vpay's `master` on 2026-11-27) unless a note says otherwise.
//!
//! **Authentication.** vpay's `/v1` takes a signed COSE_Sign1 message, or, where the deployment's `auth.methods`
//! permits `machine_tokens`, a bearer token of audience `vpay:v1` minted by its `/v1/oauth/token` (*verified*,
//! the `vpay-merchant-api` skill of `vaam-apps/vpay-skills` at `19f15ba`, and `backends/crates/vpay-api/src/resource_auth.rs`). This adapter sends a bearer
//! token read from a file on every call, so whatever mints and rotates it (`client_credentials` with an RS256
//! `private_key_jwt`) runs beside the service. Minting it here, and signing requests, are not built.
//!
//! [ADR 0003]: https://github.com/vaam-apps/openbilling-rs/blob/main/docs/adr/0003-credits-from-a-payment-provider.md

use std::fmt;
use std::path::PathBuf;
use std::time::Duration;

use openbilling_core::{
    Amount, Checkout, CreatedPayment, CreditId, CurrencyCode, ErrorClass, PaymentError,
    PaymentProvider, PaymentRefusal, PaymentRequest, PaymentStatus, PaymentTarget, PaymentView,
    RefundStatus, RefundView, Timestamp, WebhookHeaders, WebhookHint,
};

mod client;
mod form;
#[cfg(feature = "stand-in")]
pub mod stand_in;
mod units;
pub mod webhook;
mod wire;

pub use units::MinorUnits;

use client::Client;
use form::Form;

/// The `metadata` key that carries the owner of the account a top-up credits.
pub const OWNER_KEY: &str = "openbilling_owner";
/// The `metadata` key that carries the label of the account a top-up credits.
pub const LABEL_KEY: &str = "openbilling_label";

/// A secret: never printed by `Debug` or `Display`.
#[derive(Clone, PartialEq, Eq)]
pub struct WebhookSecret(String);

impl WebhookSecret {
    /// A secret; `None` when it is empty.
    #[must_use]
    pub fn new(value: impl Into<String>) -> Option<Self> {
        let value = value.into();
        (!value.is_empty()).then_some(Self(value))
    }

    pub(crate) fn expose(&self) -> &str {
        &self.0
    }
}

impl fmt::Debug for WebhookSecret {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str("WebhookSecret(<hidden>)")
    }
}

/// Where the bearer token for vpay's `/v1` comes from.
#[derive(Clone, PartialEq, Eq)]
pub enum Credential {
    /// A file holding the token, read again on every call so that whatever rotates it needs no restart. Surrounding
    /// whitespace is ignored.
    TokenFile(PathBuf),
    /// A token held in memory: for tests.
    Token(String),
}

impl fmt::Debug for Credential {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::TokenFile(path) => f.debug_tuple("TokenFile").field(path).finish(),
            Self::Token(_) => f.write_str("Token(<hidden>)"),
        }
    }
}

/// One currency the adapter takes top-ups in, and its minor-unit exponent at vpay (for example 2 for a currency of
/// cents). vpay's amounts are integers of the currency's minor unit; openbilling's are micro-units.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct CurrencyUnit {
    /// The currency.
    pub currency: CurrencyCode,
    /// Minor units per major unit, as a power of ten: 0 to 6.
    pub exponent: u32,
}

/// The adapter's configuration.
#[derive(Clone, Debug)]
pub struct VpayConfig {
    /// vpay's base URL, such as `https://vpay.example`; `/v1/…` is appended. `https`, or `http` to a loopback
    /// address only, and with no user name or password in it.
    pub base_url: String,
    /// The bearer token.
    pub credential: Credential,
    /// The secrets vpay signs this endpoint's webhooks with: any one verifies. Two during a rotation.
    pub webhook_secrets: Vec<WebhookSecret>,
    /// How far a webhook's timestamp may be from now, in seconds; vpay's default is 300.
    pub webhook_tolerance_secs: u32,
    /// The currencies top-ups may be in.
    pub currencies: Vec<CurrencyUnit>,
    /// The `payment_method_types` every PaymentIntent offers: vpay rail codes or method types this vpay deployment
    /// enables. vpay refuses an intent with none.
    pub payment_method_types: Vec<String>,
    /// How long one call to vpay may take.
    pub timeout: Duration,
}

/// Why a configuration was refused.
#[derive(Clone, Copy, Debug, PartialEq, Eq, thiserror::Error)]
pub enum ConfigError {
    /// The base URL is not `https`, nor `http` to a loopback address, or carries credentials.
    #[error(
        "the vpay base URL must be https (or http to a loopback address) and hold no credentials"
    )]
    BaseUrl,
    /// No webhook secret.
    #[error("at least one vpay webhook secret is needed")]
    NoWebhookSecret,
    /// No currency, one named twice, or an exponent above 6.
    #[error("the vpay currencies must be named once each, with an exponent from 0 to 6")]
    Currencies,
    /// No payment method type.
    #[error("at least one vpay payment method type is needed")]
    NoPaymentMethodType,
    /// The HTTP client could not be built.
    #[error("the HTTP client could not be built")]
    Client,
}

impl openbilling_core::Classify for ConfigError {
    fn class(&self) -> ErrorClass {
        ErrorClass::Invalid
    }
}

/// The vpay adapter.
#[derive(Clone, Debug)]
pub struct VpayProvider {
    client: Client,
    webhook_secrets: Vec<WebhookSecret>,
    webhook_tolerance_secs: u32,
    currencies: Vec<CurrencyUnit>,
    payment_method_types: Vec<String>,
}

impl VpayProvider {
    /// The adapter for `config`.
    ///
    /// # Errors
    ///
    /// [`ConfigError`] for a configuration that could never work.
    pub fn new(config: VpayConfig) -> Result<Self, ConfigError> {
        if config.webhook_secrets.is_empty() {
            return Err(ConfigError::NoWebhookSecret);
        }
        let mut seen: Vec<&CurrencyCode> = Vec::new();
        for unit in &config.currencies {
            if unit.exponent > 6 || seen.contains(&&unit.currency) {
                return Err(ConfigError::Currencies);
            }
            seen.push(&unit.currency);
        }
        if seen.is_empty() {
            return Err(ConfigError::Currencies);
        }
        if config.payment_method_types.is_empty()
            || config.payment_method_types.iter().any(String::is_empty)
        {
            return Err(ConfigError::NoPaymentMethodType);
        }
        let client = Client::new(&config.base_url, config.credential, config.timeout)?;
        Ok(Self {
            client,
            webhook_secrets: config.webhook_secrets,
            webhook_tolerance_secs: config.webhook_tolerance_secs,
            currencies: config.currencies,
            payment_method_types: config.payment_method_types,
        })
    }

    fn exponent(&self, currency: &CurrencyCode) -> Result<u32, PaymentRefusal> {
        self.currencies
            .iter()
            .find(|unit| &unit.currency == currency)
            .map(|unit| unit.exponent)
            .ok_or(PaymentRefusal::UnsupportedCurrency)
    }

    /// A currency as vpay writes it, lower case, back to ours.
    fn currency_of(&self, wire: &str) -> Result<(CurrencyCode, u32), PaymentError> {
        let currency = CurrencyCode::new(wire.to_ascii_uppercase())
            .map_err(|_| client::unreadable("a currency"))?;
        let exponent = self
            .exponent(&currency)
            .map_err(|_| client::unreadable("a currency this adapter is not configured for"))?;
        Ok((currency, exponent))
    }

    fn micros(minor: i64, exponent: u32) -> Result<Amount, PaymentError> {
        MinorUnits(minor)
            .to_micros(exponent)
            .ok_or_else(|| client::unreadable("an amount"))
    }
}

fn payment_status(wire: &str) -> Result<PaymentStatus, PaymentError> {
    // *Verified*: `IntentStatus` in `backends/crates/vpay-core/src/state.rs`, five values and no `failed`: a
    // declined charge falls back to `requires_payment_method`.
    match wire {
        "requires_payment_method" | "requires_action" | "processing" => Ok(PaymentStatus::Pending),
        "succeeded" => Ok(PaymentStatus::Succeeded),
        "canceled" => Ok(PaymentStatus::Canceled),
        _ => Err(client::unreadable("a payment status")),
    }
}

fn refund_status(wire: &str) -> Result<RefundStatus, PaymentError> {
    // *Verified*: `RefundStatus` in `backends/crates/vpay-core/src/state.rs`.
    match wire {
        "pending" => Ok(RefundStatus::Pending),
        "succeeded" => Ok(RefundStatus::Succeeded),
        "failed" => Ok(RefundStatus::Failed),
        "canceled" => Ok(RefundStatus::Canceled),
        _ => Err(client::unreadable("a refund status")),
    }
}

fn id(wire: String) -> Result<CreditId, PaymentError> {
    CreditId::new(wire).map_err(|_| client::unreadable("an id"))
}

impl PaymentProvider for VpayProvider {
    fn name(&self) -> &'static str {
        "vpay"
    }

    async fn create_payment(
        &self,
        request: &PaymentRequest,
    ) -> Result<CreatedPayment, PaymentError> {
        let exponent = self.exponent(&request.currency)?;
        let minor = MinorUnits::from_micros(request.amount, exponent)?;
        let currency = request.currency.as_str().to_ascii_lowercase();
        let key = request.idempotency_key.as_str();

        // *Verified*: `CreateParams` of `backends/crates/vpay-api/src/v1/payment_intents.rs`: `amount` in minor
        // units, `currency`, at least one `payment_method_types[]`, `metadata` of at most 50 keys of 40 characters
        // and values of 500. Our keys are 17 characters, an owner at most 255 and a label at most 64.
        let intent_form = Form::default()
            .field("amount", &minor.0.to_string())
            .field("currency", &currency)
            .list(
                "payment_method_types",
                self.payment_method_types.iter().map(String::as_str),
            )
            .entry("metadata", OWNER_KEY, request.to.owner.as_str())
            .entry("metadata", LABEL_KEY, request.to.label.as_str());
        let intent: wire::PaymentIntent = self
            .client
            .post(
                "/v1/payment_intents",
                &format!("openbilling:{key}:payment_intent"),
                &intent_form,
            )
            .await?;

        // *Verified*: `CreateParams` and `UiMode` of `backends/crates/vpay-api/src/v1/checkout_sessions.rs`: a
        // session references an existing intent, `ui_mode` `hosted` needs `success_url` and `cancel_url`, and
        // answers a `url` (which carries the session's client secret in its fragment).
        let session_form = Form::default()
            .field("payment_intent", &intent.id)
            .field("ui_mode", "hosted")
            .field("success_url", &request.return_to.success)
            .field("cancel_url", &request.return_to.cancel);
        let session: wire::CheckoutSession = self
            .client
            .post(
                "/v1/checkout/sessions",
                &format!("openbilling:{key}:checkout_session"),
                &session_form,
            )
            .await?;
        Ok(CreatedPayment {
            payment: id(intent.id)?,
            checkout: Checkout {
                id: session.id,
                url: session.url,
            },
            status: payment_status(&intent.status)?,
        })
    }

    async fn read_payment(&self, payment: &CreditId) -> Result<PaymentView, PaymentError> {
        let intent: wire::PaymentIntent = self
            .client
            .get("/v1/payment_intents", payment.as_str())
            .await?;
        let (currency, exponent) = self.currency_of(&intent.currency)?;
        let owner = intent.metadata.get(OWNER_KEY);
        let label = intent.metadata.get(LABEL_KEY);
        // Both keys, and both valid, or the payment is not one openbilling asked for.
        let to = match (owner, label) {
            (Some(owner), Some(label)) => match (
                openbilling_core::OwnerId::new(owner.clone()),
                openbilling_core::AccountLabel::new(label.clone()),
            ) {
                (Ok(owner), Ok(label)) => Some(PaymentTarget { owner, label }),
                _ => None,
            },
            _ => None,
        };
        if intent.id != payment.as_str() {
            return Err(client::unreadable("another payment than the one asked for"));
        }
        Ok(PaymentView {
            id: payment.clone(),
            status: payment_status(&intent.status)?,
            amount: Self::micros(intent.amount, exponent)?,
            currency,
            to,
        })
    }

    async fn read_refund(&self, refund: &CreditId) -> Result<RefundView, PaymentError> {
        let wire: wire::Refund = self.client.get("/v1/refunds", refund.as_str()).await?;
        let (currency, exponent) = self.currency_of(&wire.currency)?;
        if wire.id != refund.as_str() {
            return Err(client::unreadable("another refund than the one asked for"));
        }
        Ok(RefundView {
            id: refund.clone(),
            payment: id(wire.payment_intent)?,
            status: refund_status(&wire.status)?,
            amount: Self::micros(wire.amount, exponent)?,
            currency,
        })
    }

    fn verify_webhook(
        &self,
        headers: &WebhookHeaders,
        body: &[u8],
        now: Timestamp,
    ) -> Result<WebhookHint, PaymentError> {
        webhook::verify(
            headers,
            body,
            &self.webhook_secrets,
            self.webhook_tolerance_secs,
            now,
        )?;
        Ok(webhook::hint(body)?)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn config() -> VpayConfig {
        VpayConfig {
            base_url: "https://vpay.example".to_owned(),
            credential: Credential::Token("t".to_owned()),
            webhook_secrets: vec![WebhookSecret::new("s").expect("a secret")],
            webhook_tolerance_secs: 300,
            currencies: vec![CurrencyUnit {
                currency: CurrencyCode::new("EUR").expect("a currency"),
                exponent: 2,
            }],
            payment_method_types: vec!["card".to_owned()],
            timeout: Duration::from_secs(5),
        }
    }

    #[test]
    fn a_configuration_that_could_never_work_is_refused() {
        assert!(VpayProvider::new(config()).is_ok());
        let mut no_secret = config();
        no_secret.webhook_secrets.clear();
        assert_eq!(
            VpayProvider::new(no_secret).err(),
            Some(ConfigError::NoWebhookSecret)
        );
        let mut twice = config();
        twice.currencies.push(twice.currencies[0].clone());
        assert_eq!(
            VpayProvider::new(twice).err(),
            Some(ConfigError::Currencies)
        );
        let mut wide = config();
        wide.currencies[0].exponent = 7;
        assert_eq!(VpayProvider::new(wide).err(), Some(ConfigError::Currencies));
        let mut none = config();
        none.payment_method_types.clear();
        assert_eq!(
            VpayProvider::new(none).err(),
            Some(ConfigError::NoPaymentMethodType)
        );
        for url in [
            "http://vpay.example",
            "https://user:pass@vpay.example",
            "ftp://vpay.example",
            "not a url",
        ] {
            let mut bad = config();
            bad.base_url = url.to_owned();
            assert_eq!(
                VpayProvider::new(bad).err(),
                Some(ConfigError::BaseUrl),
                "{url}"
            );
        }
        let mut local = config();
        local.base_url = "http://127.0.0.1:8080".to_owned();
        assert!(VpayProvider::new(local).is_ok());
    }

    #[test]
    fn secrets_and_tokens_are_never_printed() {
        let shown = format!("{:?}", config());
        assert!(
            !shown.contains("\"t\"") && !shown.contains("\"s\""),
            "{shown}"
        );
        assert!(shown.contains("<hidden>"));
    }

    #[test]
    fn statuses_map_as_vpay_defines_them() {
        assert_eq!(
            payment_status("processing").ok(),
            Some(PaymentStatus::Pending)
        );
        assert_eq!(
            payment_status("requires_action").ok(),
            Some(PaymentStatus::Pending)
        );
        assert_eq!(
            payment_status("succeeded").ok(),
            Some(PaymentStatus::Succeeded)
        );
        assert_eq!(
            payment_status("canceled").ok(),
            Some(PaymentStatus::Canceled)
        );
        assert!(
            payment_status("failed").is_err(),
            "vpay has no failed intent"
        );
        assert_eq!(refund_status("failed").ok(), Some(RefundStatus::Failed));
        assert!(refund_status("other").is_err());
    }
}
