//! Settling against a real ledger: a paid vpay top-up becomes one `payment` credit, a succeeded refund one
//! reversal, and nothing else books anything. Needs `OPENBILLING_TEST_DATABASE_URL`; skips without it.

#![allow(clippy::unwrap_used, clippy::expect_used)]

mod db;
use openbilling_vpay::stand_in as support;

use openbilling_core::{
    Amount, CreatedPayment, LedgerStore, MismatchField, NewAccount, PaymentProvider, PaymentStatus,
    ReconcileSettings, RefundStatus, Rule, SettleError, Settled, TopUp, TopUpLedger,
    UnrecordedTopUps, reconcile_pending, settle_hint, settle_payment, settle_refund,
};
use openbilling_pg::LedgerSettings;
use openbilling_testkit::fixtures::{account, config, eur, label, owner};
use openbilling_testkit::payments::{Alteration, ProviderFixture, request};

async fn ready() -> Option<(openbilling_pg::PgLedger, support::Fixture)> {
    let ledger = db::fresh_ledger().await?;
    ledger.configure(&config()).await.unwrap();
    ledger
        .open_account(&NewAccount {
            id: account("a1"),
            owner: owner("o1"),
            currency: eur(),
            label: label("main"),
        })
        .await
        .unwrap();
    Some((ledger, support::fixture().await))
}

/// What `startTopUp` does: asks the provider for the payment, then records the top-up.
async fn start(
    ledger: &openbilling_pg::PgLedger,
    f: &support::Fixture,
    key: &str,
) -> CreatedPayment {
    let asked = request(f, key, f.amount());
    let created = f.provider.create_payment(&asked).await.unwrap();
    ledger
        .record_top_up(&TopUp {
            payment: created.payment.clone(),
            key: asked.idempotency_key.clone(),
            to: asked.to.clone(),
            currency: asked.currency.clone(),
            amount: asked.amount,
        })
        .await
        .unwrap();
    created
}

async fn available(ledger: &openbilling_pg::PgLedger) -> i64 {
    let at = ledger.now().await.unwrap();
    ledger
        .balance(&owner("o1"), &eur(), at)
        .await
        .unwrap()
        .net()
        .micros()
}

#[tokio::test]
async fn a_paid_top_up_credits_once_however_often_it_is_settled() {
    let Some((ledger, f)) = ready().await else {
        return;
    };
    let created = start(&ledger, &f, "k1").await;
    assert_eq!(
        settle_payment(&ledger, &f.provider, &created.payment)
            .await
            .unwrap(),
        Settled::PaymentNotSettled(PaymentStatus::Pending)
    );
    assert_eq!(available(&ledger).await, 0);

    f.pay(&created.payment).await;
    let at = ledger.now().await.unwrap();
    let hook = f.payment_webhook(&created.payment, at);
    let hint = f
        .provider
        .verify_webhook(&hook.headers, &hook.body, at)
        .unwrap();
    let Settled::Credited(first) = settle_hint(&ledger, &f.provider, &hint).await.unwrap() else {
        panic!("credited")
    };
    assert!(!first.replayed);
    let Settled::Credited(again) = settle_payment(&ledger, &f.provider, &created.payment)
        .await
        .unwrap()
    else {
        panic!("credited")
    };
    assert!(again.replayed, "once-only by vpay's payment id");
    assert_eq!(available(&ledger).await, f.amount().micros());
}

#[tokio::test]
async fn a_foreign_or_canceled_payment_books_nothing() {
    let Some((ledger, f)) = ready().await else {
        return;
    };
    let foreign = f.foreign_payment().await;
    assert_eq!(
        settle_payment(&ledger, &f.provider, &foreign)
            .await
            .unwrap(),
        Settled::NotOurs
    );
    let created = start(&ledger, &f, "k1").await;
    f.cancel(&created.payment).await;
    assert_eq!(
        settle_payment(&ledger, &f.provider, &created.payment)
            .await
            .unwrap(),
        Settled::PaymentNotSettled(PaymentStatus::Canceled)
    );
    assert_eq!(available(&ledger).await, 0);
}

#[tokio::test]
async fn only_a_succeeded_refund_reverses_and_only_once() {
    let Some((ledger, f)) = ready().await else {
        return;
    };
    let created = start(&ledger, &f, "k1").await;
    f.pay(&created.payment).await;
    let part = Amount::from_micros(2_000_000).unwrap();
    let pending = f
        .refund(&created.payment, part, RefundStatus::Pending)
        .await;
    assert_eq!(
        settle_refund(&ledger, &f.provider, &pending).await.unwrap(),
        Settled::RefundNotSettled(RefundStatus::Pending)
    );
    // The payment was never settled on its own: the refund settles it first, then reverses.
    let done = f
        .refund(&created.payment, part, RefundStatus::Succeeded)
        .await;
    let Settled::Reversed(first) = settle_refund(&ledger, &f.provider, &done).await.unwrap() else {
        panic!("reversed")
    };
    assert!(!first.replayed);
    let Settled::Reversed(again) = settle_refund(&ledger, &f.provider, &done).await.unwrap() else {
        panic!("reversed")
    };
    assert!(again.replayed);
    assert_eq!(
        available(&ledger).await,
        f.amount().micros() - part.micros()
    );
}

#[tokio::test]
async fn a_top_up_to_a_label_the_owner_lacks_is_refused_by_the_ledger() {
    let Some((ledger, f)) = ready().await else {
        return;
    };
    let mut asked = request(&f, "k1", f.amount());
    asked.to.label = label("promo");
    let created = f.provider.create_payment(&asked).await.unwrap();
    // Recording refuses it: the owner has no such account.
    let error = ledger
        .record_top_up(&TopUp {
            payment: created.payment.clone(),
            key: asked.idempotency_key.clone(),
            to: asked.to.clone(),
            currency: asked.currency.clone(),
            amount: asked.amount,
        })
        .await
        .unwrap_err();
    assert_eq!(error.rule(), Some(Rule::UnknownAccount));
    // And a payment never recorded, credited by the deployment's choice, is refused at the credit just the same.
    let Some(lenient) = db::fresh_ledger_with(LedgerSettings {
        unrecorded_top_ups: UnrecordedTopUps::Credit,
        ..LedgerSettings::default()
    })
    .await
    else {
        return;
    };
    lenient.configure(&config()).await.unwrap();
    f.pay(&created.payment).await;
    let error = settle_payment(&lenient, &f.provider, &created.payment)
        .await
        .unwrap_err();
    assert!(matches!(
        error,
        SettleError::Ledger(ref e) if e.rule() == Some(Rule::UnknownAccount)
    ));
}

fn mismatches(field: &'static str) -> u64 {
    openbilling_metrics::global().snapshot().counter(
        openbilling_metrics::Counter::PaymentMismatches,
        Some(&[("field", field)]),
    )
}

/// The provider's side changes a payment after it was asked for: whatever differs, nothing is credited, the error is
/// a conflict that names the field and no value, and the refusal is counted.
#[tokio::test]
async fn an_altered_top_up_is_refused_with_a_conflict_and_counted() {
    use openbilling_core::{Classify, ErrorClass};
    let Some((ledger, f)) = ready().await else {
        return;
    };
    let before = mismatches("amount");
    for (key, alteration, field) in [
        ("k-amount", Alteration::Amount, MismatchField::Amount),
        ("k-owner", Alteration::Owner, MismatchField::Owner),
        ("k-label", Alteration::Label, MismatchField::Label),
        ("k-none", Alteration::StripMetadata, MismatchField::Owner),
    ] {
        let created = start(&ledger, &f, key).await;
        f.alter(&created.payment, alteration).await;
        f.pay(&created.payment).await;
        let error = settle_payment(&ledger, &f.provider, &created.payment)
            .await
            .unwrap_err();
        assert!(matches!(error, SettleError::ReadBackMismatch(got) if got == field));
        // Answered again, the mark is sticky and is not a new mismatch: the counter does not move a second time.
        let again = settle_payment(&ledger, &f.provider, &created.payment)
            .await
            .unwrap_err();
        assert!(matches!(again, SettleError::ReadBackMismatch(got) if got == field));
        assert_eq!(error.class(), ErrorClass::Conflict);
        let said = error.to_string();
        assert!(
            !said.contains("someone-else") && !said.contains("elsewhere"),
            "{said}"
        );
    }
    assert_eq!(available(&ledger).await, 0, "nothing was credited");
    assert_eq!(
        mismatches("amount"),
        before + 1,
        "settled twice, counted once"
    );
}

/// A succeeded payment nobody recorded, under the default `refuse`: refused every time it is read, counted once.
#[tokio::test]
async fn an_unrecorded_payment_is_refused_each_time_and_counted_once() {
    let Some((ledger, f)) = ready().await else {
        return;
    };
    let before = mismatches("unrecorded");
    let asked = request(&f, "k-unrecorded", f.amount());
    let created = f.provider.create_payment(&asked).await.unwrap();
    f.pay(&created.payment).await;
    for _ in 0..3 {
        let error = settle_payment(&ledger, &f.provider, &created.payment)
            .await
            .unwrap_err();
        assert!(matches!(
            error,
            SettleError::ReadBackMismatch(MismatchField::Unrecorded)
        ));
    }
    assert_eq!(available(&ledger).await, 0);
    assert_eq!(mismatches("unrecorded"), before + 1);
}

/// A paid top-up whose webhook never came is credited once by the reconciler; the webhook arriving after it books
/// nothing more, and a paid one the webhook booked first is not claimed at all. The order is fixed here, step by step;
/// the two settling a single locked row at once is `openbilling-pg`'s
/// `a_webhook_and_a_reconcile_pass_settling_one_row_credit_once`, which holds the row and releases it.
#[tokio::test]
async fn the_reconciler_and_a_webhook_credit_a_paid_top_up_once() {
    let Some((ledger, f)) = ready().await else {
        return;
    };
    let paid = start(&ledger, &f, "k-paid").await;
    let hooked_first = start(&ledger, &f, "k-hooked").await;
    let waiting = start(&ledger, &f, "k-waiting").await;
    let canceled = start(&ledger, &f, "k-canceled").await;
    f.pay(&paid.payment).await;
    f.pay(&hooked_first.payment).await;
    f.cancel(&canceled.payment).await;
    let settings = ReconcileSettings {
        pending_after: std::time::Duration::ZERO,
        read_again_after: std::time::Duration::from_secs(3600),
        stop_after: std::time::Duration::from_secs(3600),
        batch: 100,
    };
    let at = ledger.now().await.unwrap();
    let hint_of = |payment: &openbilling_core::CreditId| {
        let hook = f.payment_webhook(payment, at);
        f.provider
            .verify_webhook(&hook.headers, &hook.body, at)
            .unwrap()
    };
    // The webhook of one lands before any pass: that top-up is credited and never claimed.
    let Settled::Credited(first) =
        settle_hint(&ledger, &f.provider, &hint_of(&hooked_first.payment))
            .await
            .unwrap()
    else {
        panic!("the webhook credits")
    };
    assert!(!first.replayed);
    let report = reconcile_pending(&ledger, &f.provider, &settings)
        .await
        .unwrap();
    assert_eq!(
        (
            report.credited,
            report.replayed,
            report.pending,
            report.canceled,
            report.mismatched,
            report.failed
        ),
        (1, 0, 1, 1, 0, 0),
        "the pass credits `paid`, finds `waiting` unpaid, closes `canceled`, and does not see the one already booked: {report:?}"
    );
    // The late webhook of the one the pass credited is the replay.
    let Settled::Credited(late) = settle_hint(&ledger, &f.provider, &hint_of(&paid.payment))
        .await
        .unwrap()
    else {
        panic!("the late webhook answers credited")
    };
    assert!(late.replayed, "booked by the pass already");
    assert_eq!(
        available(&ledger).await,
        2 * f.amount().micros(),
        "once each"
    );
    // The unpaid one is not read again at once, and when it is paid the next pass credits it.
    f.pay(&waiting.payment).await;
    let none = reconcile_pending(&ledger, &f.provider, &settings)
        .await
        .unwrap();
    assert_eq!(none.total(), 0);
    let later = ReconcileSettings {
        read_again_after: std::time::Duration::ZERO,
        ..settings
    };
    let report = reconcile_pending(&ledger, &f.provider, &later)
        .await
        .unwrap();
    assert_eq!(report.credited, 1);
    assert_eq!(available(&ledger).await, 3 * f.amount().micros());
}
