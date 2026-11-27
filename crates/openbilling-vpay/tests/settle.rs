//! Settling against a real ledger: a paid vpay top-up becomes one `payment` credit, a succeeded refund one
//! reversal, and nothing else books anything. Needs `OPENBILLING_TEST_DATABASE_URL`; skips without it.

#![allow(clippy::unwrap_used, clippy::expect_used)]

mod db;
use openbilling_vpay::stand_in as support;

use openbilling_core::{
    Amount, LedgerStore, NewAccount, PaymentProvider, PaymentStatus, RefundStatus, Settled,
    settle_hint, settle_payment, settle_refund,
};
use openbilling_testkit::fixtures::{account, config, eur, label, owner};
use openbilling_testkit::payments::{ProviderFixture, request};

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
    let created = f
        .provider
        .create_payment(&request(&f, "k1", f.amount()))
        .await
        .unwrap();
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
    let created = f
        .provider
        .create_payment(&request(&f, "k1", f.amount()))
        .await
        .unwrap();
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
    let created = f
        .provider
        .create_payment(&request(&f, "k1", f.amount()))
        .await
        .unwrap();
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
    f.pay(&created.payment).await;
    let error = settle_payment(&ledger, &f.provider, &created.payment)
        .await
        .unwrap_err();
    assert!(matches!(
        error,
        openbilling_core::SettleError::Ledger(ref e) if e.rule() == Some(openbilling_core::Rule::UnknownAccount)
    ));
}
