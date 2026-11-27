//! The payment provider port, [`PaymentProvider`], and what goes in and out of it (ADR 0003).
//!
//! openbilling never touches card or wallet data. It asks a provider for a payment (a top-up), and credits the
//! owner's account only when the provider, read back, says the payment succeeded. A provider's callback is a hint:
//! [`PaymentProvider::verify_webhook`] answers which object to read again, never a status.
//!
//! Only a [`CreditSource::Payment`](crate::CreditSource::Payment) credit comes from a provider. Promotions and
//! adjustments are the operator's.

use std::fmt;
use std::future::Future;

use crate::amount::Amount;
use crate::error::{Classify, ErrorClass};
use crate::id::{AccountLabel, CreditId, CurrencyCode, OwnerId};
use crate::time::Timestamp;

/// The longest [`IdempotencyKey`], in bytes.
pub const MAX_IDEMPOTENCY_KEY_LEN: usize = 128;

/// A caller's idempotency key: 1 to [`MAX_IDEMPOTENCY_KEY_LEN`] bytes of printable ASCII (`!` to `~`), the alphabet
/// of every other caller-given id, so it can be logged and passed on to a provider unchanged.
#[derive(Clone, PartialEq, Eq, Hash)]
pub struct IdempotencyKey(String);

impl IdempotencyKey {
    /// Checks `value` and wraps it; `None` when it is empty, too long, or holds a byte outside `!` to `~`.
    #[must_use]
    pub fn new(value: impl Into<String>) -> Option<Self> {
        let value = value.into();
        let fits = !value.is_empty()
            && value.len() <= MAX_IDEMPOTENCY_KEY_LEN
            && value.bytes().all(|b| b.is_ascii_graphic());
        fits.then_some(Self(value))
    }

    /// The key as text.
    #[must_use]
    pub fn as_str(&self) -> &str {
        &self.0
    }
}

impl fmt::Debug for IdempotencyKey {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_tuple("IdempotencyKey").field(&self.0).finish()
    }
}

impl fmt::Display for IdempotencyKey {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.0)
    }
}

/// The account a payment credits once it succeeds: an owner's account in a currency with a label (ADR 0002).
///
/// The payment's own currency is the account's: a provider carries the target with the payment and reads it back,
/// and the credit names the account by owner, the payment's currency and the label.
#[derive(Clone, Debug, PartialEq, Eq, Hash)]
pub struct PaymentTarget {
    /// The owner.
    pub owner: OwnerId,
    /// The label of the owner's account in the payment's currency.
    pub label: AccountLabel,
}

/// Where the provider sends the payer back from its checkout page.
#[derive(Clone, Debug, PartialEq, Eq, Hash)]
pub struct ReturnUrls {
    /// After the payer paid, or left the page with the payment under way.
    pub success: String,
    /// After the payer gave up.
    pub cancel: String,
}

/// A top-up to ask a provider for.
#[derive(Clone, Debug, PartialEq, Eq, Hash)]
pub struct PaymentRequest {
    /// The caller's key. The same key again asks for the same payment, within the provider's own window.
    pub idempotency_key: IdempotencyKey,
    /// The account it credits.
    pub to: PaymentTarget,
    /// The currency.
    pub currency: CurrencyCode,
    /// How much, in micro-units of `currency`. Positive. A provider refuses an amount it cannot represent exactly
    /// in its own unit ([`PaymentRefusal::InexactAmount`]).
    pub amount: Amount,
    /// Where the payer goes afterwards.
    pub return_to: ReturnUrls,
}

/// Where a payment is, as the provider says when it is read back. Closed on purpose.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum PaymentStatus {
    /// Not paid yet; it may still succeed.
    Pending,
    /// The money moved. The one status that credits.
    Succeeded,
    /// It will never succeed: canceled, expired or refused for good.
    Canceled,
}

impl PaymentStatus {
    /// The status's stable name.
    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Pending => "pending",
            Self::Succeeded => "succeeded",
            Self::Canceled => "canceled",
        }
    }
}

/// The page the payer is sent to.
#[derive(Clone, PartialEq, Eq, Hash)]
pub struct Checkout {
    /// The provider's id of the checkout.
    pub id: String,
    /// The URL to send the payer to. It may carry a credential of the checkout (a provider's client secret), so it
    /// is never logged; its `Debug` hides it.
    pub url: Option<String>,
}

impl fmt::Debug for Checkout {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("Checkout")
            .field("id", &self.id)
            .field("url", &self.url.as_ref().map(|_| "<hidden>"))
            .finish()
    }
}

/// What asking for a payment answered.
#[derive(Clone, Debug, PartialEq, Eq, Hash)]
pub struct CreatedPayment {
    /// The provider's payment id: the once-only id of the `payment` credit it becomes.
    pub payment: CreditId,
    /// Where to send the payer.
    pub checkout: Checkout,
    /// Its status now.
    pub status: PaymentStatus,
}

/// A payment, read back from the provider.
#[derive(Clone, Debug, PartialEq, Eq, Hash)]
pub struct PaymentView {
    /// The provider's payment id.
    pub id: CreditId,
    /// Its status.
    pub status: PaymentStatus,
    /// Its amount, in micro-units of `currency`.
    pub amount: Amount,
    /// Its currency.
    pub currency: CurrencyCode,
    /// The account it credits, or `None` for a payment openbilling did not ask for: it credits nothing.
    pub to: Option<PaymentTarget>,
}

/// Where a refund is, as the provider says when it is read back. Closed on purpose.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum RefundStatus {
    /// Asked for; the money has not gone back yet.
    Pending,
    /// The money went back to the payer. The one status that reverses credit.
    Succeeded,
    /// Refused by the rail, for good.
    Failed,
    /// Withdrawn before the rail acted.
    Canceled,
}

impl RefundStatus {
    /// The status's stable name.
    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Pending => "pending",
            Self::Succeeded => "succeeded",
            Self::Failed => "failed",
            Self::Canceled => "canceled",
        }
    }
}

/// A refund of a payment, read back from the provider.
#[derive(Clone, Debug, PartialEq, Eq, Hash)]
pub struct RefundView {
    /// The provider's refund id: the once-only id of the reversal it becomes.
    pub id: CreditId,
    /// The payment it refunds.
    pub payment: CreditId,
    /// Its status.
    pub status: RefundStatus,
    /// What it gives back, in micro-units of `currency`.
    pub amount: Amount,
    /// Its currency.
    pub currency: CurrencyCode,
}

/// A webhook's headers, names in lower case.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct WebhookHeaders(Vec<(String, String)>);

impl WebhookHeaders {
    /// Headers from `(name, value)` pairs; names are lower-cased.
    #[must_use]
    pub fn new(pairs: impl IntoIterator<Item = (String, String)>) -> Self {
        Self(
            pairs
                .into_iter()
                .map(|(name, value)| (name.to_ascii_lowercase(), value))
                .collect(),
        )
    }

    /// Every value of the header `name` (any case), in order.
    pub fn all<'a>(&'a self, name: &'a str) -> impl Iterator<Item = &'a str> + 'a {
        self.0
            .iter()
            .filter(move |(n, _)| n.eq_ignore_ascii_case(name))
            .map(|(_, value)| value.as_str())
    }
}

/// What a verified webhook names: an object to read back, never a status (ADR 0003). Closed on purpose.
#[derive(Clone, Debug, PartialEq, Eq, Hash)]
pub enum WebhookHint {
    /// A payment may have changed.
    Payment(CreditId),
    /// A refund may have changed.
    Refund(CreditId),
    /// An event about nothing that moves credit; verified, and nothing to do.
    Ignored,
}

/// Why a provider refused a request. Closed on purpose.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum PaymentRefusal {
    /// The provider refused the request as malformed, or one of its values (a URL, the amount).
    InvalidRequest,
    /// The amount has no exact value in the provider's unit for the currency.
    InexactAmount,
    /// A currency the provider is not configured for.
    UnsupportedCurrency,
    /// The idempotency key was used before for another request, or the object is in a state that refuses it.
    Conflict,
    /// No such payment or refund.
    NotFound,
    /// A webhook whose signature does not verify, or is missing.
    WebhookSignature,
    /// A webhook signed too long ago, or too far ahead.
    WebhookTimestamp,
    /// A webhook whose verified body is not an event the provider sends.
    WebhookBody,
}

impl PaymentRefusal {
    /// The refusal's stable name.
    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::InvalidRequest => "invalid_request",
            Self::InexactAmount => "inexact_amount",
            Self::UnsupportedCurrency => "unsupported_currency",
            Self::Conflict => "conflict",
            Self::NotFound => "not_found",
            Self::WebhookSignature => "webhook_signature",
            Self::WebhookTimestamp => "webhook_timestamp",
            Self::WebhookBody => "webhook_body",
        }
    }

    /// What it says, in fixed text.
    #[must_use]
    pub const fn describe(self) -> &'static str {
        match self {
            Self::InvalidRequest => "the payment provider refused the request",
            Self::InexactAmount => "the amount has no exact value in the provider's unit",
            Self::UnsupportedCurrency => "the payment provider is not configured for the currency",
            Self::Conflict => "the payment provider refused it as a conflict",
            Self::NotFound => "no such payment or refund at the payment provider",
            Self::WebhookSignature => "the webhook's signature does not verify",
            Self::WebhookTimestamp => "the webhook's timestamp is outside the tolerance",
            Self::WebhookBody => "the webhook's body is not an event",
        }
    }
}

impl Classify for PaymentRefusal {
    fn class(&self) -> ErrorClass {
        match self {
            Self::Conflict => ErrorClass::Conflict,
            Self::InvalidRequest
            | Self::InexactAmount
            | Self::UnsupportedCurrency
            | Self::NotFound
            | Self::WebhookSignature
            | Self::WebhookTimestamp
            | Self::WebhookBody => ErrorClass::Invalid,
        }
    }
}

/// The error of every [`PaymentProvider`] call.
#[derive(Debug, thiserror::Error)]
pub enum PaymentError {
    /// The provider, or the adapter before it, refused the request.
    #[error("refused: {}", .0.describe())]
    Refused(PaymentRefusal),
    /// The provider could not be asked, or answered something the adapter cannot read. The message is fixed text;
    /// the cause is the [`source`](std::error::Error::source), and never holds a credential.
    #[error("the payment provider failed ({class})")]
    Provider {
        /// What a caller should do about it.
        class: ErrorClass,
        /// The adapter's own error.
        #[source]
        source: Box<dyn std::error::Error + Send + Sync>,
    },
}

impl PaymentError {
    /// The refusal, when it is one.
    #[must_use]
    pub const fn refusal(&self) -> Option<PaymentRefusal> {
        match self {
            Self::Refused(refusal) => Some(*refusal),
            Self::Provider { .. } => None,
        }
    }
}

impl From<PaymentRefusal> for PaymentError {
    fn from(refusal: PaymentRefusal) -> Self {
        Self::Refused(refusal)
    }
}

impl Classify for PaymentError {
    fn class(&self) -> ErrorClass {
        match self {
            Self::Refused(refusal) => refusal.class(),
            Self::Provider { class, .. } => *class,
        }
    }
}

/// A payment provider: where top-ups are paid (ADR 0003). `openbilling-vpay` implements it; `openbilling-testkit`
/// holds the conformance suite every implementation runs.
pub trait PaymentProvider: Send + Sync {
    /// The provider's stable name, such as `vpay`.
    fn name(&self) -> &'static str;

    /// Asks the provider for a payment that credits `request.to` once it succeeds, and a checkout for the payer.
    /// Once-only by the request's idempotency key, within the provider's window.
    fn create_payment(
        &self,
        request: &PaymentRequest,
    ) -> impl Future<Output = Result<CreatedPayment, PaymentError>> + Send;

    /// Reads a payment back. The only source of a payment's status.
    fn read_payment(
        &self,
        id: &CreditId,
    ) -> impl Future<Output = Result<PaymentView, PaymentError>> + Send;

    /// Reads a refund back. The only source of a refund's status.
    fn read_refund(
        &self,
        id: &CreditId,
    ) -> impl Future<Output = Result<RefundView, PaymentError>> + Send;

    /// Verifies a webhook's signature over its raw body at `now`, then names what it is about. A webhook is a hint:
    /// what it names is read back before anything is credited.
    ///
    /// # Errors
    ///
    /// [`PaymentRefusal::WebhookSignature`], [`PaymentRefusal::WebhookTimestamp`] or
    /// [`PaymentRefusal::WebhookBody`]; the body is parsed only after the signature verifies.
    fn verify_webhook(
        &self,
        headers: &WebhookHeaders,
        body: &[u8],
        now: Timestamp,
    ) -> Result<WebhookHint, PaymentError>;
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn an_idempotency_key_is_printable_ascii_up_to_128_bytes() {
        assert!(IdempotencyKey::new("k-1").is_some());
        assert!(IdempotencyKey::new("").is_none());
        assert!(IdempotencyKey::new("a b").is_none());
        assert!(IdempotencyKey::new("caf\u{e9}").is_none());
        assert!(IdempotencyKey::new("x".repeat(128)).is_some());
        assert!(IdempotencyKey::new("x".repeat(129)).is_none());
    }

    #[test]
    fn refusals_have_classes_and_fixed_text() {
        assert_eq!(PaymentRefusal::Conflict.class(), ErrorClass::Conflict);
        assert_eq!(
            PaymentRefusal::WebhookSignature.class(),
            ErrorClass::Invalid
        );
        let error = PaymentError::from(PaymentRefusal::NotFound);
        assert_eq!(error.refusal(), Some(PaymentRefusal::NotFound));
        assert_eq!(
            error.to_string(),
            "refused: no such payment or refund at the payment provider"
        );
        let provider = PaymentError::Provider {
            class: ErrorClass::Unavailable,
            source: "Bearer secret-token".into(),
        };
        assert_eq!(provider.class(), ErrorClass::Unavailable);
        assert!(!provider.to_string().contains("secret"));
    }

    #[test]
    fn headers_are_found_in_any_case() {
        let headers = WebhookHeaders::new([
            ("Vpay-Signature".to_owned(), "a".to_owned()),
            ("other".to_owned(), "b".to_owned()),
            ("VPAY-SIGNATURE".to_owned(), "c".to_owned()),
        ]);
        let found: Vec<_> = headers.all("vpay-signature").collect();
        assert_eq!(found, ["a", "c"]);
    }

    #[test]
    fn a_checkout_url_is_hidden_from_debug() {
        let checkout = Checkout {
            id: "cs_1".to_owned(),
            url: Some("https://pay.example/c#secret".to_owned()),
        };
        let shown = format!("{checkout:?}");
        assert!(!shown.contains("secret"), "{shown}");
        assert!(shown.contains("cs_1"));
    }

    #[test]
    fn statuses_have_stable_names() {
        assert_eq!(PaymentStatus::Succeeded.as_str(), "succeeded");
        assert_eq!(RefundStatus::Failed.as_str(), "failed");
    }
}
