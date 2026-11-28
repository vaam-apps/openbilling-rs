//! The conformance cases of top-ups ([`TopUpLedger`], ADR 0003 amended 2026-11-28): a top-up is recorded once, a payment
//! read back is compared with it, the credit is booked from the recorded terms once, and the pending ones that are due
//! are claimed once.
//!
//! The cases build the provider's answer themselves, as a [`PaymentView`], so they need no provider. A store gives them
//! a factory that takes the policy for a payment no top-up was recorded for.

use std::time::Duration;

use openbilling_core::{
    IdempotencyKey, LedgerStore, MismatchField, PaymentStatus, PaymentTarget, PaymentView,
    ReconcileSettings, Rule, TopUp, TopUpLedger, TopUpSettled, UnrecordedTopUps,
};

use crate::fixtures::{credit_id, eur, label, micros, net, open, owner, rule_of, setup, usd};

/// What a store gives the cases: the two ports.
pub trait TopUpStore: LedgerStore + TopUpLedger + Clone + 'static {}
impl<T: LedgerStore + TopUpLedger + Clone + 'static> TopUpStore for T {}

const AMOUNT: i64 = 5_000_000;

fn top_up(payment: &str) -> TopUp {
    TopUp {
        payment: credit_id(payment),
        key: IdempotencyKey::new(format!("key-{payment}")).expect("a key"),
        to: PaymentTarget {
            owner: owner("o1"),
            label: label("main"),
        },
        currency: eur(),
        amount: micros(AMOUNT),
    }
}

/// The payment as the provider reports it when it agrees with [`top_up`].
fn view(payment: &str, status: PaymentStatus) -> PaymentView {
    PaymentView {
        id: credit_id(payment),
        status,
        amount: micros(AMOUNT),
        currency: eur(),
        to: Some(PaymentTarget {
            owner: owner("o1"),
            label: label("main"),
        }),
    }
}

async fn ready<S: TopUpStore>(store: &S) {
    setup(store).await;
    open(store, "a1", "o1", eur(), "main").await;
    open(store, "a2", "o2", eur(), "main").await;
}

/// Settings that claim at once and never claim twice.
fn due_now() -> ReconcileSettings {
    ReconcileSettings {
        pending_after: Duration::ZERO,
        read_again_after: Duration::from_secs(3600),
        stop_after: Duration::from_secs(3600),
        batch: 100,
    }
}

/// A top-up is recorded once by its payment id: the same terms again are a replay, other terms are a conflict, and an
/// account that does not exist is refused.
pub async fn a_top_up_is_recorded_once_and_other_terms_conflict<S: TopUpStore>(
    make: impl AsyncFn(UnrecordedTopUps) -> Option<S>,
) {
    let Some(store) = make(UnrecordedTopUps::Refuse).await else {
        return;
    };
    ready(&store).await;
    assert!(!store.record_top_up(&top_up("p1")).await.unwrap());
    assert!(
        store.record_top_up(&top_up("p1")).await.unwrap(),
        "a replay"
    );
    let mut other = top_up("p1");
    other.amount = micros(AMOUNT + 1);
    assert_eq!(
        rule_of(store.record_top_up(&other).await),
        Rule::TopUpConflict
    );
    open(&store, "a3", "o1", eur(), "promo").await;
    let mut other = top_up("p1");
    other.to.label = label("promo");
    assert_eq!(
        rule_of(store.record_top_up(&other).await),
        Rule::TopUpConflict
    );
    let mut nobody = top_up("p2");
    nobody.to.owner = owner("nobody");
    assert_eq!(
        rule_of(store.record_top_up(&nobody).await),
        Rule::UnknownAccount
    );
}

/// A payment that reads back differently from its top-up in any field credits nothing, and the refusal is final: the
/// payment agreeing later does not credit it.
pub async fn a_payment_that_differs_credits_nothing_and_stays_refused<S: TopUpStore>(
    make: impl AsyncFn(UnrecordedTopUps) -> Option<S>,
) {
    let Some(store) = make(UnrecordedTopUps::Credit).await else {
        return;
    };
    ready(&store).await;
    type Alter = fn(&mut PaymentView);
    let cases: [(&str, MismatchField, Alter); 6] = [
        ("p-owner", MismatchField::Owner, |v| {
            v.to.as_mut().unwrap().owner = owner("o2");
        }),
        ("p-no-owner", MismatchField::Owner, |v| v.to = None),
        ("p-label", MismatchField::Label, |v| {
            v.to.as_mut().unwrap().label = label("promo");
        }),
        ("p-currency", MismatchField::Currency, |v| {
            v.currency = usd()
        }),
        ("p-amount", MismatchField::Amount, |v| {
            v.amount = micros(AMOUNT * 100);
        }),
        // Even a pending payment that already differs is refused for good.
        ("p-pending", MismatchField::Amount, |v| {
            v.status = PaymentStatus::Pending;
            v.amount = micros(1);
        }),
    ];
    for (id, field, alter) in cases {
        assert!(!store.record_top_up(&top_up(id)).await.unwrap());
        let mut altered = view(id, PaymentStatus::Succeeded);
        alter(&mut altered);
        assert_eq!(
            store.settle_top_up(&altered).await.unwrap(),
            TopUpSettled::Mismatch(field),
            "{id}"
        );
        // Final, whatever the provider says next, even when it now agrees.
        assert_eq!(
            store
                .settle_top_up(&view(id, PaymentStatus::Succeeded))
                .await
                .unwrap(),
            TopUpSettled::Mismatch(field),
            "{id}: sticky"
        );
    }
    assert_eq!(net(&store, "o1").await, 0, "nothing was credited");
    assert_eq!(net(&store, "o2").await, 0);
}

/// A payment no top-up was recorded for follows the policy: refused (the default) or credited from what the provider
/// reports; one with no owner reference is not ours either way.
pub async fn an_unrecorded_payment_follows_the_policy<S: TopUpStore>(
    make: impl AsyncFn(UnrecordedTopUps) -> Option<S>,
) {
    let (Some(refuse), Some(credit)) = (
        make(UnrecordedTopUps::Refuse).await,
        make(UnrecordedTopUps::Credit).await,
    ) else {
        return;
    };
    for store in [&refuse, &credit] {
        ready(store).await;
    }
    let paid = view("u1", PaymentStatus::Succeeded);
    assert_eq!(
        refuse.settle_top_up(&paid).await.unwrap(),
        TopUpSettled::Mismatch(MismatchField::Unrecorded)
    );
    assert_eq!(net(&refuse, "o1").await, 0);
    let TopUpSettled::Credited(first) = credit.settle_top_up(&paid).await.unwrap() else {
        panic!("credited")
    };
    assert!(!first.replayed);
    let TopUpSettled::Credited(again) = credit.settle_top_up(&paid).await.unwrap() else {
        panic!("credited")
    };
    assert!(again.replayed);
    assert_eq!(net(&credit, "o1").await, AMOUNT);

    let mut foreign = view("u2", PaymentStatus::Succeeded);
    foreign.to = None;
    for store in [&refuse, &credit] {
        assert_eq!(
            store.settle_top_up(&foreign).await.unwrap(),
            TopUpSettled::NotOurs
        );
        // A payment still under way, or canceled, is just that, whatever the policy.
        assert_eq!(
            store
                .settle_top_up(&view("u3", PaymentStatus::Pending))
                .await
                .unwrap(),
            TopUpSettled::Pending
        );
        assert_eq!(
            store
                .settle_top_up(&view("u4", PaymentStatus::Canceled))
                .await
                .unwrap(),
            TopUpSettled::Canceled
        );
    }
}

/// A succeeded payment that agrees credits the recorded terms once, however many settle it at once.
pub async fn a_succeeded_payment_credits_the_recorded_terms_once<S: TopUpStore>(
    make: impl AsyncFn(UnrecordedTopUps) -> Option<S>,
) {
    let Some(store) = make(UnrecordedTopUps::Refuse).await else {
        return;
    };
    ready(&store).await;
    store.record_top_up(&top_up("p1")).await.unwrap();
    assert_eq!(
        store
            .settle_top_up(&view("p1", PaymentStatus::Pending))
            .await
            .unwrap(),
        TopUpSettled::Pending
    );
    assert_eq!(net(&store, "o1").await, 0);

    let mut tasks = Vec::new();
    for _ in 0..8 {
        let store = store.clone();
        tasks.push(tokio::spawn(async move {
            store
                .settle_top_up(&view("p1", PaymentStatus::Succeeded))
                .await
                .unwrap()
        }));
    }
    let mut booked = 0;
    for task in tasks {
        let TopUpSettled::Credited(credited) = task.await.unwrap() else {
            panic!("credited")
        };
        booked += i32::from(!credited.replayed);
    }
    assert_eq!(booked, 1, "one booking, the rest replays");
    assert_eq!(net(&store, "o1").await, AMOUNT);
}

/// A canceled top-up is final: a later success is not credited.
pub async fn a_canceled_top_up_is_final<S: TopUpStore>(
    make: impl AsyncFn(UnrecordedTopUps) -> Option<S>,
) {
    let Some(store) = make(UnrecordedTopUps::Credit).await else {
        return;
    };
    ready(&store).await;
    store.record_top_up(&top_up("p1")).await.unwrap();
    assert_eq!(
        store
            .settle_top_up(&view("p1", PaymentStatus::Canceled))
            .await
            .unwrap(),
        TopUpSettled::Canceled
    );
    assert_eq!(
        store
            .settle_top_up(&view("p1", PaymentStatus::Succeeded))
            .await
            .unwrap(),
        TopUpSettled::Canceled
    );
    assert_eq!(net(&store, "o1").await, 0);
}

/// Claims take the pending top-ups that are due, once: not before `pending_after`, not after `stop_after`, not again
/// before `read_again_after`, and never the same top-up to two callers at once. A settled one is not claimed.
pub async fn claims_respect_the_windows_and_are_disjoint<S: TopUpStore>(
    make: impl AsyncFn(UnrecordedTopUps) -> Option<S>,
) {
    let Some(store) = make(UnrecordedTopUps::Refuse).await else {
        return;
    };
    ready(&store).await;
    for i in 0..30 {
        store
            .record_top_up(&top_up(&format!("c{i}")))
            .await
            .unwrap();
    }
    // Too young.
    let young = ReconcileSettings {
        pending_after: Duration::from_secs(3600),
        ..due_now()
    };
    assert!(
        store
            .claim_pending_top_ups(&young)
            .await
            .unwrap()
            .is_empty()
    );

    // Two callers at once take disjoint sets that together are everything.
    let (a, b) = {
        let (s1, s2) = (store.clone(), store.clone());
        let one = tokio::spawn(async move {
            s1.claim_pending_top_ups(&ReconcileSettings {
                batch: 20,
                ..due_now()
            })
            .await
            .unwrap()
        });
        let two = tokio::spawn(async move {
            s2.claim_pending_top_ups(&ReconcileSettings {
                batch: 20,
                ..due_now()
            })
            .await
            .unwrap()
        });
        (one.await.unwrap(), two.await.unwrap())
    };
    let mut all: Vec<_> = a
        .iter()
        .chain(b.iter())
        .map(|id| id.as_str().to_owned())
        .collect();
    all.sort_unstable();
    let before = all.len();
    all.dedup();
    assert_eq!(before, all.len(), "no top-up was claimed twice");
    assert_eq!(all.len(), 30, "together, every due top-up");
    // Read again only after the window.
    assert!(
        store
            .claim_pending_top_ups(&due_now())
            .await
            .unwrap()
            .is_empty()
    );
    let again = ReconcileSettings {
        read_again_after: Duration::ZERO,
        ..due_now()
    };
    assert_eq!(store.claim_pending_top_ups(&again).await.unwrap().len(), 30);

    // A settled top-up is no longer claimed; an abandoned one (older than `stop_after`) is not either.
    store
        .settle_top_up(&view("c0", PaymentStatus::Canceled))
        .await
        .unwrap();
    assert_eq!(store.claim_pending_top_ups(&again).await.unwrap().len(), 29);
    tokio::time::sleep(Duration::from_millis(60)).await;
    let abandoned = ReconcileSettings {
        stop_after: Duration::from_millis(50),
        ..again
    };
    assert!(
        store
            .claim_pending_top_ups(&abandoned)
            .await
            .unwrap()
            .is_empty()
    );
    // The limit holds.
    assert_eq!(
        store
            .claim_pending_top_ups(&ReconcileSettings { batch: 3, ..again })
            .await
            .unwrap()
            .len(),
        3
    );
}

/// A payment settled before its top-up was recorded (`refuse`: kept as `unrecorded`, uncredited) is not lost when the
/// record arrives late with the very terms read back: it becomes a recorded, pending top-up, and the next settle
/// credits it once. Other terms stay refused (`TopUpConflict`) and the row stays mismatched.
pub async fn a_late_record_upgrades_an_unrecorded_payment_only_when_the_terms_agree<
    S: TopUpStore,
>(
    make: impl AsyncFn(UnrecordedTopUps) -> Option<S>,
) {
    let Some(store) = make(UnrecordedTopUps::Refuse).await else {
        return;
    };
    ready(&store).await;
    // Differing terms: refused, and the row is still the refused one.
    let paid = view("late", PaymentStatus::Succeeded);
    assert_eq!(
        store.settle_top_up(&paid).await.unwrap(),
        TopUpSettled::Mismatch(MismatchField::Unrecorded)
    );
    for change in [
        (|t: &mut TopUp| t.amount = micros(AMOUNT + 1)) as fn(&mut TopUp),
        |t| t.to.owner = owner("o2"),
    ] {
        let mut other = top_up("late");
        change(&mut other);
        assert_eq!(
            rule_of(store.record_top_up(&other).await),
            Rule::TopUpConflict
        );
    }
    assert_eq!(
        store.settle_top_up(&paid).await.unwrap(),
        TopUpSettled::Mismatch(MismatchField::Unrecorded)
    );
    assert_eq!(net(&store, "o1").await, 0);
    // The same terms: recorded now, and the next settle credits once.
    assert!(
        !store.record_top_up(&top_up("late")).await.unwrap(),
        "this call recorded it"
    );
    assert!(
        store.record_top_up(&top_up("late")).await.unwrap(),
        "and the record is once-only from then on"
    );
    let TopUpSettled::Credited(first) = store.settle_top_up(&paid).await.unwrap() else {
        panic!("credited")
    };
    assert!(!first.replayed);
    let TopUpSettled::Credited(again) = store.settle_top_up(&paid).await.unwrap() else {
        panic!("credited")
    };
    assert!(again.replayed);
    assert_eq!(net(&store, "o1").await, AMOUNT, "credited once");
}

/// A top-up older than `stop_after` becomes `expired`: it is not claimed any more, and a later settle (a webhook, an
/// operator's `reconcilePayment`) still credits it, or closes it as canceled or mismatched.
pub async fn an_expired_top_up_is_not_claimed_and_is_still_settled<S: TopUpStore>(
    make: impl AsyncFn(UnrecordedTopUps) -> Option<S>,
) {
    let Some(store) = make(UnrecordedTopUps::Refuse).await else {
        return;
    };
    ready(&store).await;
    for id in ["x1", "x2", "x3"] {
        store.record_top_up(&top_up(id)).await.unwrap();
    }
    tokio::time::sleep(Duration::from_millis(60)).await;
    let stale = ReconcileSettings {
        stop_after: Duration::from_millis(50),
        read_again_after: Duration::ZERO,
        ..due_now()
    };
    assert_eq!(store.expire_stale_top_ups(&stale).await.unwrap(), 3);
    assert_eq!(store.expire_stale_top_ups(&stale).await.unwrap(), 0, "once");
    // Neither the narrow window nor a wide one claims an expired top-up.
    assert!(
        store
            .claim_pending_top_ups(&stale)
            .await
            .unwrap()
            .is_empty()
    );
    assert!(
        store
            .claim_pending_top_ups(&ReconcileSettings {
                stop_after: Duration::from_secs(3600),
                read_again_after: Duration::ZERO,
                ..due_now()
            })
            .await
            .unwrap()
            .is_empty()
    );
    // Still settled: credited once, canceled, or refused on a difference.
    let TopUpSettled::Credited(first) = store
        .settle_top_up(&view("x1", PaymentStatus::Succeeded))
        .await
        .unwrap()
    else {
        panic!("credited")
    };
    assert!(!first.replayed);
    assert_eq!(
        store
            .settle_top_up(&view("x2", PaymentStatus::Canceled))
            .await
            .unwrap(),
        TopUpSettled::Canceled
    );
    let mut differs = view("x3", PaymentStatus::Succeeded);
    differs.amount = micros(AMOUNT + 1);
    assert_eq!(
        store.settle_top_up(&differs).await.unwrap(),
        TopUpSettled::Mismatch(MismatchField::Amount)
    );
    // An unpaid read of an expired one changes nothing.
    assert_eq!(net(&store, "o1").await, AMOUNT);
}

/// With more top-ups due than a batch, successive claims cover every one before any is read again: the least recently
/// read go first.
pub async fn successive_claims_cover_every_due_top_up_before_repeating<S: TopUpStore>(
    make: impl AsyncFn(UnrecordedTopUps) -> Option<S>,
) {
    let Some(store) = make(UnrecordedTopUps::Refuse).await else {
        return;
    };
    ready(&store).await;
    for i in 0..10 {
        store
            .record_top_up(&top_up(&format!("f{i}")))
            .await
            .unwrap();
    }
    let always = ReconcileSettings {
        read_again_after: Duration::ZERO,
        batch: 3,
        ..due_now()
    };
    let mut seen = std::collections::BTreeSet::new();
    for round in 0..4 {
        let claimed = store.claim_pending_top_ups(&always).await.unwrap();
        assert_eq!(claimed.len(), 3, "round {round}");
        for id in claimed {
            if round < 3 {
                assert!(
                    seen.insert(id.as_str().to_owned()),
                    "{id:?} was read again before every top-up was read once"
                );
            } else {
                // The fourth round takes the last unread one, then wraps to the least recently read.
                seen.insert(id.as_str().to_owned());
            }
        }
    }
    assert_eq!(seen.len(), 10, "every due top-up was claimed");
}
