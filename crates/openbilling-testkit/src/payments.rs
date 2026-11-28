//! The conformance suite of the payment provider port, [`PaymentProvider`] (ADR 0003).
//!
//! An adapter runs it against a stand-in for its provider: a server that answers as the provider answers, which the
//! adapter reaches over the network exactly as it reaches the real one. The stand-in is driven through
//! [`ProviderFixture`]: it can settle, cancel and refund a payment the way only the provider can, and it signs
//! webhooks the way the provider does.

use std::future::Future;

use openbilling_core::{
    AccountLabel, Amount, CreditId, CurrencyCode, IdempotencyKey, OwnerId, PaymentError,
    PaymentProvider, PaymentRefusal, PaymentRequest, PaymentStatus, PaymentTarget, RefundStatus,
    ReturnUrls, Timestamp, WebhookHeaders, WebhookHint,
};

/// A signed webhook, as the provider would deliver it.
#[derive(Clone, Debug)]
pub struct SignedWebhook {
    /// Its headers.
    pub headers: WebhookHeaders,
    /// Its raw body.
    pub body: Vec<u8>,
}

/// What the provider's side can change about a payment that already exists, after openbilling asked for it: what a
/// payer, an operator of the provider or an attacker with its API can do. A read back must report it faithfully; the
/// comparison with what was asked for is the ledger's (ADR 0003, amended 2026-11-28).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Alteration {
    /// The owner reference now names another owner.
    Owner,
    /// The label reference now names another label.
    Label,
    /// The amount is now larger.
    Amount,
    /// The references are gone.
    StripMetadata,
}

/// An adapter under test and the stand-in for its provider.
pub trait ProviderFixture: Send + Sync {
    /// The adapter.
    type Provider: PaymentProvider;

    /// The adapter, configured against the stand-in.
    fn provider(&self) -> &Self::Provider;

    /// A currency the adapter is configured for.
    fn currency(&self) -> CurrencyCode;

    /// An amount of `currency` the provider represents exactly.
    fn amount(&self) -> Amount;

    /// An amount of `currency` the provider cannot represent exactly, if it has one.
    fn inexact_amount(&self) -> Option<Amount>;

    /// How long after it was signed a webhook is still accepted, in microseconds, either way.
    fn tolerance_micros(&self) -> i64;

    /// The provider's side: the payer paid.
    fn pay(&self, payment: &CreditId) -> impl Future<Output = ()> + Send;

    /// The provider's side: `payment` was changed as `alteration` says. A fixture against a real provider, which
    /// cannot do this, panics, and its case list leaves out the cases that call it.
    fn alter(&self, payment: &CreditId, alteration: Alteration) -> impl Future<Output = ()> + Send;

    /// The provider's side: the payment was canceled.
    fn cancel(&self, payment: &CreditId) -> impl Future<Output = ()> + Send;

    /// The provider's side: a refund of `payment` for `amount`, which reached `status`. Returns its id.
    fn refund(
        &self,
        payment: &CreditId,
        amount: Amount,
        status: RefundStatus,
    ) -> impl Future<Output = CreditId> + Send;

    /// The provider's side: a payment someone other than openbilling asked for, already paid.
    fn foreign_payment(&self) -> impl Future<Output = CreditId> + Send;

    /// The webhook the provider sends when a payment succeeds, signed at `at`.
    fn payment_webhook(&self, payment: &CreditId, at: Timestamp) -> SignedWebhook;

    /// The webhook the provider sends about a refund, signed at `at`.
    fn refund_webhook(&self, refund: &CreditId, at: Timestamp) -> SignedWebhook;

    /// A webhook about something that moves no credit, signed at `at`.
    fn other_webhook(&self, at: Timestamp) -> SignedWebhook;

    /// The payment webhook signed with a secret the adapter does not hold.
    fn forged_webhook(&self, payment: &CreditId, at: Timestamp) -> SignedWebhook;

    /// Any body, signed at `at` as the provider signs.
    fn sign(&self, body: &[u8], at: Timestamp) -> SignedWebhook;
}

/// A top-up request for `fixture`'s currency and amount, under `key`.
pub fn request<F: ProviderFixture>(fixture: &F, key: &str, amount: Amount) -> PaymentRequest {
    PaymentRequest {
        idempotency_key: IdempotencyKey::new(key).expect("a key"),
        to: target(),
        currency: fixture.currency(),
        amount,
        return_to: ReturnUrls {
            success: "https://shop.example/paid".to_owned(),
            cancel: "https://shop.example/cancel".to_owned(),
        },
    }
}

/// The account every top-up of the suite credits: owner `o1`, label `main`.
pub fn target() -> PaymentTarget {
    PaymentTarget {
        owner: OwnerId::new("o1").expect("an owner"),
        label: AccountLabel::new("main").expect("a label"),
    }
}

/// An instant: the suite signs webhooks at fixed times and checks them against fixed times.
fn at(micros: i64) -> Timestamp {
    Timestamp::from_unix_micros(micros).expect("an instant")
}

/// 2026-11-27T00:00:00Z.
const T0: i64 = 1_795_737_600_000_000;

fn refusal<T: std::fmt::Debug>(result: Result<T, PaymentError>) -> PaymentRefusal {
    match result {
        Err(PaymentError::Refused(refusal)) => refusal,
        other => panic!("expected a refusal, got {other:?}"),
    }
}

/// A payment is created once per key, read back pending with its amount, currency and target, and comes with a
/// checkout.
pub async fn a_payment_is_created_once_per_key_and_read_back_pending<F: ProviderFixture>(
    fixture: F,
) {
    let provider = fixture.provider();
    let first = provider
        .create_payment(&request(&fixture, "k1", fixture.amount()))
        .await
        .expect("a payment");
    assert_eq!(first.status, PaymentStatus::Pending);
    assert!(!first.checkout.id.is_empty());
    let again = provider
        .create_payment(&request(&fixture, "k1", fixture.amount()))
        .await
        .expect("the same payment");
    assert_eq!(again.payment, first.payment, "once-only by the key");
    let other = provider
        .create_payment(&request(&fixture, "k2", fixture.amount()))
        .await
        .expect("another payment");
    assert_ne!(other.payment, first.payment);

    let view = provider.read_payment(&first.payment).await.expect("read");
    assert_eq!(view.id, first.payment);
    assert_eq!(view.status, PaymentStatus::Pending);
    assert_eq!(view.amount, fixture.amount());
    assert_eq!(view.currency, fixture.currency());
    assert_eq!(view.to, Some(target()));
}

/// The provider's word decides the status: paid reads succeeded, canceled reads canceled.
pub async fn a_paid_payment_reads_succeeded_and_a_canceled_one_canceled<F: ProviderFixture>(
    fixture: F,
) {
    let provider = fixture.provider();
    let paid = provider
        .create_payment(&request(&fixture, "k1", fixture.amount()))
        .await
        .expect("a payment");
    let canceled = provider
        .create_payment(&request(&fixture, "k2", fixture.amount()))
        .await
        .expect("a payment");
    fixture.pay(&paid.payment).await;
    fixture.cancel(&canceled.payment).await;
    let status = |id| async move { provider.read_payment(id).await.expect("read").status };
    assert_eq!(status(&paid.payment).await, PaymentStatus::Succeeded);
    assert_eq!(status(&canceled.payment).await, PaymentStatus::Canceled);
}

/// A read back reports the payment as the provider holds it now, whatever it was asked for: a changed owner, label or
/// amount, or no references at all. The adapter compares nothing and corrects nothing.
pub async fn a_read_back_reports_the_payment_as_the_provider_holds_it_now<F: ProviderFixture>(
    fixture: F,
) {
    let provider = fixture.provider();
    let asked = request(&fixture, "k1", fixture.amount());
    let created = provider.create_payment(&asked).await.expect("a payment");
    let first = provider.read_payment(&created.payment).await.expect("read");

    fixture.alter(&created.payment, Alteration::Owner).await;
    let view = provider.read_payment(&created.payment).await.expect("read");
    let to = view.to.expect("still referenced");
    assert_ne!(to.owner, asked.to.owner);
    assert_eq!(to.label, asked.to.label);

    fixture.alter(&created.payment, Alteration::Label).await;
    let view = provider.read_payment(&created.payment).await.expect("read");
    assert_ne!(view.to.expect("still referenced").label, asked.to.label);

    fixture.alter(&created.payment, Alteration::Amount).await;
    let view = provider.read_payment(&created.payment).await.expect("read");
    assert!(view.amount.micros() > first.amount.micros());
    assert_eq!(view.currency, first.currency);

    fixture
        .alter(&created.payment, Alteration::StripMetadata)
        .await;
    let view = provider.read_payment(&created.payment).await.expect("read");
    assert_eq!(view.to, None);
}

/// A pending payment reads back the same every time, until the provider moves it: only its status changes.
pub async fn a_pending_payment_reads_back_unchanged_until_the_provider_moves_it<
    F: ProviderFixture,
>(
    fixture: F,
) {
    let provider = fixture.provider();
    let created = provider
        .create_payment(&request(&fixture, "k1", fixture.amount()))
        .await
        .expect("a payment");
    let first = provider.read_payment(&created.payment).await.expect("read");
    let second = provider.read_payment(&created.payment).await.expect("read");
    assert_eq!(first, second);
    assert_eq!(first.status, PaymentStatus::Pending);
    fixture.pay(&created.payment).await;
    let paid = provider.read_payment(&created.payment).await.expect("read");
    assert_eq!(paid.status, PaymentStatus::Succeeded);
    assert_eq!(
        (paid.amount, paid.currency, paid.to),
        (first.amount, first.currency, first.to)
    );
}

/// A payment or a refund the provider does not know is `not_found`; an amount it cannot represent is refused
/// before anything is asked.
pub async fn unknown_objects_and_inexact_amounts_are_refused<F: ProviderFixture>(fixture: F) {
    let provider = fixture.provider();
    let missing = CreditId::new("missing-1").expect("an id");
    assert_eq!(
        refusal(provider.read_payment(&missing).await),
        PaymentRefusal::NotFound
    );
    assert_eq!(
        refusal(provider.read_refund(&missing).await),
        PaymentRefusal::NotFound
    );
    if let Some(inexact) = fixture.inexact_amount() {
        assert_eq!(
            refusal(
                provider
                    .create_payment(&request(&fixture, "k1", inexact))
                    .await
            ),
            PaymentRefusal::InexactAmount
        );
    }
}

/// A payment openbilling did not ask for reads back with no target.
pub async fn a_foreign_payment_has_no_target<F: ProviderFixture>(fixture: F) {
    let id = fixture.foreign_payment().await;
    let view = fixture.provider().read_payment(&id).await.expect("read");
    assert_eq!(view.status, PaymentStatus::Succeeded);
    assert_eq!(view.to, None);
}

/// A refund reads back with its payment, amount, currency and status.
pub async fn a_refund_is_read_back_with_its_payment_and_status<F: ProviderFixture>(fixture: F) {
    let provider = fixture.provider();
    let created = provider
        .create_payment(&request(&fixture, "k1", fixture.amount()))
        .await
        .expect("a payment");
    fixture.pay(&created.payment).await;
    for status in [
        RefundStatus::Pending,
        RefundStatus::Succeeded,
        RefundStatus::Failed,
        RefundStatus::Canceled,
    ] {
        let id = fixture
            .refund(&created.payment, fixture.amount(), status)
            .await;
        let view = provider.read_refund(&id).await.expect("read");
        assert_eq!(
            (view.id, view.payment.clone(), view.status, view.amount),
            (id, created.payment.clone(), status, fixture.amount())
        );
        assert_eq!(view.currency, fixture.currency());
    }
}

/// A verified webhook names the object to read back, and nothing else.
pub async fn a_signed_webhook_names_its_object_and_nothing_else<F: ProviderFixture>(fixture: F) {
    let provider = fixture.provider();
    let created = provider
        .create_payment(&request(&fixture, "k1", fixture.amount()))
        .await
        .expect("a payment");
    let verify = |hook: SignedWebhook| provider.verify_webhook(&hook.headers, &hook.body, at(T0));
    assert_eq!(
        verify(fixture.payment_webhook(&created.payment, at(T0))).expect("verified"),
        WebhookHint::Payment(created.payment.clone())
    );
    fixture.pay(&created.payment).await;
    let refund = fixture
        .refund(&created.payment, fixture.amount(), RefundStatus::Pending)
        .await;
    assert_eq!(
        verify(fixture.refund_webhook(&refund, at(T0))).expect("verified"),
        WebhookHint::Refund(refund)
    );
    assert_eq!(
        verify(fixture.other_webhook(at(T0))).expect("verified"),
        WebhookHint::Ignored
    );
}

/// A webhook that is forged, tampered with, unsigned or outside the tolerance is refused, and its body is read only
/// after its signature verifies.
pub async fn a_forged_stale_or_tampered_webhook_is_refused<F: ProviderFixture>(fixture: F) {
    let provider = fixture.provider();
    let payment = CreditId::new("any-payment").expect("an id");
    let tolerance = fixture.tolerance_micros();
    let check = |hook: &SignedWebhook, now: i64| {
        refusal(provider.verify_webhook(&hook.headers, &hook.body, at(now)))
    };

    let forged = fixture.forged_webhook(&payment, at(T0));
    assert_eq!(check(&forged, T0), PaymentRefusal::WebhookSignature);

    let good = fixture.payment_webhook(&payment, at(T0));
    let mut tampered = good.clone();
    tampered.body.push(b' ');
    assert_eq!(check(&tampered, T0), PaymentRefusal::WebhookSignature);

    let unsigned = SignedWebhook {
        headers: WebhookHeaders::default(),
        body: good.body.clone(),
    };
    assert_eq!(check(&unsigned, T0), PaymentRefusal::WebhookSignature);

    assert!(
        provider
            .verify_webhook(&good.headers, &good.body, at(T0 + tolerance))
            .is_ok(),
        "at the edge of the tolerance"
    );
    assert_eq!(
        check(&good, T0 + tolerance + 1_000_000),
        PaymentRefusal::WebhookTimestamp
    );
    assert_eq!(
        check(&good, T0 - tolerance - 1_000_000),
        PaymentRefusal::WebhookTimestamp
    );

    let mut swapped = good.clone();
    swapped.body = b"not an event".to_vec();
    assert_eq!(
        check(&swapped, T0),
        PaymentRefusal::WebhookSignature,
        "a body is parsed only after its signature verifies"
    );
    let signed_garbage = fixture.sign(b"not an event", at(T0));
    assert_eq!(check(&signed_garbage, T0), PaymentRefusal::WebhookBody);
}

/// Runs every case of the provider suite against the fixture `$factory` makes: `async fn() -> F`, a fresh stand-in
/// and adapter per case.
#[macro_export]
macro_rules! payment_conformance {
    ($factory:path) => {
        $crate::__payment_cases!(
            $factory;
            a_payment_is_created_once_per_key_and_read_back_pending,
            a_paid_payment_reads_succeeded_and_a_canceled_one_canceled,
            unknown_objects_and_inexact_amounts_are_refused,
            a_read_back_reports_the_payment_as_the_provider_holds_it_now,
            a_pending_payment_reads_back_unchanged_until_the_provider_moves_it,
            a_foreign_payment_has_no_target,
            a_refund_is_read_back_with_its_payment_and_status,
            a_signed_webhook_names_its_object_and_nothing_else,
            a_forged_stale_or_tampered_webhook_is_refused,
        );
    };
}

#[doc(hidden)]
#[macro_export]
macro_rules! __payment_cases {
    ($factory:path; $($case:ident),* $(,)?) => {
        $(
            #[::tokio::test(flavor = "multi_thread", worker_threads = 2)]
            async fn $case() {
                $crate::payments::$case($factory().await).await;
            }
        )*
    };
}
