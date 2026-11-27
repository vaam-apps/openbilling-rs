//! The conformance cases. Each takes a fresh, empty store and drives it through the port alone.
//!
//! Amounts are micro-units of EUR unless a case says otherwise. Times come from the store's own clock, because the
//! window is the store's to check.

use openbilling_core::{
    Amount, CreditSource, CreditTarget, EventOutcome, Intake, LedgerConfig, LedgerStore, LotKind,
    LotOrder, NotAllowed, RefusalReason, Reversal, ReversalKind, Rule, UsageEvent, UtcDay, Window,
    WithinKind,
};

use crate::fixtures::{
    DAY, HOUR, MINUTE, account, allowed_at, allowed_now, config, credit_id, credit_to, debt,
    drain_all, eur, event, event_id, label, lots, member, micros, net, now, open, owner, pay, plus,
    refusal_of, remaining, remainings, rule_of, setup, take, usd,
};

/// The same batch applied twice debits once; a mixed batch debits only the new id; a repeat inside one batch
/// counts once.
pub async fn the_same_batch_twice_debits_once<S: LedgerStore>(store: S) {
    setup(&store).await;
    open(&store, "a1", "o1", eur(), "main").await;
    pay(&store, "p1", "a1", 10_000).await;
    let t = now(&store).await;
    let batch = [
        event("e1", "o1", "m1", 1_500, t),
        event("e2", "o1", "m2", 500, t),
    ];

    let first = take(&store, &batch).await;
    assert_eq!((first.accepted(), first.replayed()), (2, 0));
    let again = take(&store, &batch).await;
    assert_eq!((again.accepted(), again.replayed()), (0, 2));
    let drained = drain_all(&store, 100).await;
    assert_eq!((drained.applied(), drained.debited()), (2, micros(2_000)));
    assert!(store.drain(100).await.expect("drain").decided.is_empty());
    assert_eq!(remaining(&store, "a1").await, 8_000);

    // A replay after the drain is still a replay, and moves nothing.
    assert_eq!(take(&store, &batch).await.replayed(), 2);
    assert!(drain_all(&store, 100).await.decided.is_empty());

    let mixed = take(
        &store,
        &[
            event("e2", "o1", "m2", 500, t),
            event("e3", "o1", "m1", 100, t),
        ],
    )
    .await;
    let answers: Vec<_> = mixed
        .answers
        .iter()
        .map(|(id, a)| (id.as_str(), *a))
        .collect();
    assert_eq!(
        answers,
        [("e2", Intake::Replayed), ("e3", Intake::Accepted)]
    );
    assert_eq!(drain_all(&store, 100).await.debited(), micros(100));
    assert_eq!(remaining(&store, "a1").await, 7_900);

    let e4 = event("e4", "o1", "m1", 10, t);
    let dup = take(&store, &[e4.clone(), e4]).await;
    assert_eq!((dup.accepted(), dup.replayed()), (1, 1));
    assert_eq!(drain_all(&store, 100).await.debited(), micros(10));
    assert_eq!(remaining(&store, "a1").await, 7_890);
}

/// A bad event refuses the whole batch, names it, and nothing is recorded or debited.
pub async fn a_bad_event_refuses_the_whole_batch_and_writes_nothing<S: LedgerStore>(store: S) {
    setup(&store).await;
    open(&store, "a1", "o1", eur(), "main").await;
    pay(&store, "p1", "a1", 1_000).await;
    let t = now(&store).await;
    let negative = event("e2", "o1", "m1", -1, t);
    let (rule, named) = refusal_of(
        store
            .intake(&[event("e1", "o1", "m1", 5, t), negative])
            .await,
    );
    assert_eq!((rule, named), (Rule::NegativeAmount, vec!["e2".to_owned()]));

    let mut foreign = event("e3", "o1", "m1", 5, t);
    foreign.currency = openbilling_core::CurrencyCode::new("GBP").expect("a currency");
    let (rule, named) = refusal_of(
        store
            .intake(&[event("e1", "o1", "m1", 5, t), foreign])
            .await,
    );
    assert_eq!(
        (rule, named),
        (Rule::UnknownCurrency, vec!["e3".to_owned()])
    );

    // e1 was written by neither call: it is accepted now, not replayed.
    assert!(store.drain(10).await.expect("drain").decided.is_empty());
    assert_eq!(
        take(&store, &[event("e1", "o1", "m1", 5, t)])
            .await
            .accepted(),
        1
    );
    assert_eq!(remaining(&store, "a1").await, 1_000);
}

/// Concurrent batches for one account sum correctly. 20 intakes at once, each with 10 events of its own and
/// the same 5 shared ones, then 4 drains at once: every id applies once.
pub async fn concurrent_intake_and_drains_sum_exactly<S: LedgerStore + Clone + 'static>(store: S) {
    setup(&store).await;
    open(&store, "a1", "o1", eur(), "main").await;
    pay(&store, "p1", "a1", 1_000_000).await;
    let t = now(&store).await;
    let mut tasks = Vec::new();
    for batch in 0..20 {
        let store = store.clone();
        tasks.push(tokio::spawn(async move {
            let mut events: Vec<UsageEvent> = (0..10)
                .map(|n| event(&format!("own-{batch}-{n}"), "o1", "m1", 100, t))
                .collect();
            events.extend((0..5).map(|n| event(&format!("shared-{n}"), "o1", "m1", 7, t)));
            store.intake(&events).await.expect("intake").accepted()
        }));
    }
    let mut accepted = 0;
    for task in tasks {
        accepted += task.await.expect("an intake task");
    }
    assert_eq!(accepted, 20 * 10 + 5);
    let drains: Vec<_> = (0..4)
        .map(|_| {
            let store = store.clone();
            tokio::spawn(async move { drain_all(&store, 7).await })
        })
        .collect();
    let mut applied = 0;
    let mut debited = 0;
    for drain in drains {
        let report = drain.await.expect("a drain task");
        applied += report.applied();
        debited += report.debited().micros();
    }
    let rest = drain_all(&store, 1000).await;
    applied += rest.applied();
    debited += rest.debited().micros();
    let expected = 20 * 10 * 100 + 5 * 7;
    assert_eq!((applied, debited), (205, expected));
    assert_eq!(remaining(&store, "a1").await, 1_000_000 - expected);
}

/// Concurrent debits past the credit split it and the debt exactly. 20 000 of debits, drained by 8 drains at
/// once, against 5 000 of credit: the lot ends at exactly 0 and the debt is exactly the rest.
pub async fn concurrent_drains_with_scarce_credit_split_credit_and_debt_exactly<
    S: LedgerStore + Clone + 'static,
>(
    store: S,
) {
    setup(&store).await;
    open(&store, "a1", "o1", eur(), "main").await;
    pay(&store, "p1", "a1", 5_000).await;
    let t = now(&store).await;
    for batch in 0..4 {
        let events: Vec<UsageEvent> = (0..50)
            .map(|n| event(&format!("e-{batch}-{n}"), "o1", "m1", 100, t))
            .collect();
        take(&store, &events).await;
    }
    let drains: Vec<_> = (0..8)
        .map(|_| {
            let store = store.clone();
            tokio::spawn(async move { drain_all(&store, 10).await })
        })
        .collect();
    for drain in drains {
        drain.await.expect("a drain task");
    }
    drain_all(&store, 1000).await;
    assert_eq!(remaining(&store, "a1").await, 0);
    assert_eq!(debt(&store, "o1").await, 15_000);
    assert_eq!(net(&store, "o1").await, -15_000);
    let daily: i64 = store
        .daily_usage(&owner("o1"), &eur())
        .await
        .expect("daily usage")
        .iter()
        .map(|(_, a)| a.micros())
        .sum();
    assert_eq!(daily, 20_000);
}

/// Concurrent batches over overlapping ids and accounts neither deadlock nor lose money. Twelve intakes of the
/// same 18 events in alternating orders over three owners, then six drains of four events at once while each owner
/// is paid five more times.
pub async fn concurrent_drains_over_many_owners_neither_deadlock_nor_lose_money<
    S: LedgerStore + Clone + 'static,
>(
    store: S,
) {
    setup(&store).await;
    let owners = ["oa", "ob", "oc"];
    for who in owners {
        open(&store, &format!("acc-{who}"), who, eur(), "main").await;
        pay(
            &store,
            &format!("pay-{who}"),
            &format!("acc-{who}"),
            100_000,
        )
        .await;
    }
    let t = now(&store).await;
    let mut intakes = Vec::new();
    for round in 0..12 {
        let store = store.clone();
        intakes.push(tokio::spawn(async move {
            let mut events: Vec<UsageEvent> = (0..6)
                .flat_map(|n| owners.map(|who| event(&format!("{who}-{n}"), who, "m1", 10, t)))
                .collect();
            if round % 2 == 1 {
                events.reverse();
            }
            store.intake(&events).await.expect("intake").accepted()
        }));
    }
    let mut accepted = 0;
    for intake in intakes {
        accepted += intake.await.expect("an intake task");
    }
    assert_eq!(accepted, 18, "each of the 18 distinct ids accepted once");
    let mut tasks = Vec::new();
    for _ in 0..6 {
        let store = store.clone();
        tasks.push(tokio::spawn(
            async move { drain_all(&store, 4).await.applied() },
        ));
    }
    for (n, who) in owners.iter().cycle().take(15).enumerate() {
        let store = store.clone();
        let who = (*who).to_owned();
        tasks.push(tokio::spawn(async move {
            pay(&store, &format!("more-{who}-{n}"), &format!("acc-{who}"), 1).await;
            0
        }));
    }
    let mut applied = 0;
    for task in tasks {
        applied += task.await.expect("a task");
    }
    applied += drain_all(&store, 100).await.applied();
    assert_eq!(applied, 18);
    for who in owners {
        assert_eq!(
            remaining(&store, &format!("acc-{who}")).await,
            100_000 - 60 + 5
        );
    }
}

/// A payment applied twice credits once, by retries one after the other and at once; the same payment id
/// for another amount or account is a conflict.
pub async fn a_payment_credits_once<S: LedgerStore + Clone + 'static>(store: S) {
    setup(&store).await;
    for (id, who) in [("a1", "o1"), ("a2", "o2"), ("a3", "o3")] {
        open(&store, id, who, eur(), "main").await;
    }
    let first = pay(&store, "pay-42", "a1", 5_000).await;
    assert!(!first.replayed);
    let second = pay(&store, "pay-42", "a1", 5_000).await;
    assert!(second.replayed);
    assert_eq!(second.transaction, first.transaction);
    assert_eq!(remaining(&store, "a1").await, 5_000);
    assert_eq!(lots(&store, "a1").await.len(), 1);
    for (to, amount) in [("a1", 9_000), ("a2", 5_000)] {
        let other = credit_to(CreditSource::Payment, "pay-42", to, amount, None);
        assert_eq!(
            rule_of(store.credit(&other).await),
            Rule::CreditConflict,
            "{to} {amount}"
        );
    }
    let tries: Vec<_> = (0..8)
        .map(|_| {
            let store = store.clone();
            tokio::spawn(async move { pay(&store, "pay-race", "a3", 700).await.replayed })
        })
        .collect();
    let mut firsts = 0;
    for attempt in tries {
        if !attempt.await.expect("a credit task") {
            firsts += 1;
        }
    }
    assert_eq!(firsts, 1);
    assert_eq!(remaining(&store, "a3").await, 700);
    assert_eq!(remaining(&store, "a2").await, 0);
}

/// A payment id credits once whichever way it names its account, by id
/// or by owner, currency and label. The same id under another source is another credit.
pub async fn a_payment_id_credits_once_by_either_route<S: LedgerStore>(store: S) {
    setup(&store).await;
    open(&store, "a1", "o1", eur(), "main").await;
    let by_label = |id: &str, amount: i64| openbilling_core::Credit {
        source: CreditSource::Payment,
        id: credit_id(id),
        to: CreditTarget::Label {
            owner: owner("o1"),
            currency: eur(),
            label: label("main"),
        },
        amount: micros(amount),
        expires_at: None,
    };
    let first = pay(&store, "p1", "a1", 5_000).await;
    let again = store
        .credit(&by_label("p1", 5_000))
        .await
        .expect("a replay");
    assert!(again.replayed);
    assert_eq!(
        (again.transaction, again.account.as_str()),
        (first.transaction, "a1")
    );
    assert_eq!(
        rule_of(store.credit(&by_label("p1", 9_000)).await),
        Rule::CreditConflict
    );

    let second = store
        .credit(&by_label("p2", 2_000))
        .await
        .expect("a credit");
    assert!(!second.replayed);
    let again = pay(&store, "p2", "a1", 2_000).await;
    assert!(again.replayed);
    assert_eq!(again.transaction, second.transaction);
    assert_eq!(
        rule_of(
            store
                .credit(&credit_to(CreditSource::Payment, "p2", "a1", 7_000, None))
                .await
        ),
        Rule::CreditConflict
    );
    assert_eq!(remaining(&store, "a1").await, 7_000);

    // Ids are once-only within their source: a promotion may reuse a payment's id.
    let promo = store
        .credit(&credit_to(CreditSource::Promotion, "p1", "a1", 300, None))
        .await
        .expect("a promotion");
    assert!(!promo.replayed);
    assert_eq!(remaining(&store, "a1").await, 7_300);
    assert_eq!(lots(&store, "a1").await.len(), 3);
}

/// Grants, refunds and chargebacks, and the rules of each; a chargeback after the credit was spent leaves debt,
/// and the next payment repays it first.
pub async fn sources_reversals_and_debt_repayment<S: LedgerStore>(store: S) {
    setup(&store).await;
    open(&store, "a1", "o1", eur(), "main").await;
    open(&store, "a2", "o2", eur(), "main").await;
    store
        .credit(&credit_to(CreditSource::Promotion, "g1", "a1", 300, None))
        .await
        .expect("a promotion");
    let promo = lots(&store, "a1").await;
    assert_eq!(promo.len(), 1);
    assert_eq!(
        (promo[0].kind, promo[0].credit.as_ref().map(|c| c.as_str())),
        (LotKind::Promotion, Some("g1"))
    );

    pay(&store, "p9", "a2", 1_000).await;
    let refund = |id: &str, amount: i64| Reversal {
        kind: ReversalKind::Refund,
        id: credit_id(id),
        payment: credit_id("p9"),
        amount: micros(amount),
    };
    let first = store.reverse(&refund("re1", 400)).await.expect("a refund");
    assert!(!first.replayed);
    assert_eq!(first.account, account("a2"));
    assert_eq!(remaining(&store, "a2").await, 600);
    assert!(
        store
            .reverse(&refund("re1", 400))
            .await
            .expect("a replay")
            .replayed
    );
    assert_eq!(remaining(&store, "a2").await, 600);
    let unknown = Reversal {
        payment: credit_id("p-none"),
        ..refund("re2", 1)
    };
    assert_eq!(rule_of(store.reverse(&unknown).await), Rule::UnknownPayment);
    // A promotion is not a payment: it cannot be reversed.
    let of_promotion = Reversal {
        payment: credit_id("g1"),
        ..refund("re3", 1)
    };
    assert_eq!(
        rule_of(store.reverse(&of_promotion).await),
        Rule::UnknownPayment
    );

    let t = now(&store).await;
    take(&store, &[event("e1", "o2", "m1", 500, t)]).await;
    drain_all(&store, 10).await;
    assert_eq!(remaining(&store, "a2").await, 100);
    let chargeback = Reversal {
        kind: ReversalKind::Chargeback,
        id: credit_id("cb1"),
        payment: credit_id("p9"),
        amount: micros(600),
    };
    let charged = store.reverse(&chargeback).await.expect("a chargeback");
    assert_eq!(charged.debt_added, micros(500));
    assert_eq!(remaining(&store, "a2").await, 0);
    assert_eq!(debt(&store, "o2").await, 500);
    let read = allowed_now(&store, "o2", "m1").await;
    assert_eq!(
        (read.refused, read.balance),
        (Some(NotAllowed::Balance), micros(-500))
    );

    let next = pay(&store, "p10", "a2", 2_000).await;
    assert_eq!(next.repaid, micros(500));
    assert_eq!(debt(&store, "o2").await, 0);
    assert_eq!(remaining(&store, "a2").await, 1_500);
    let read = allowed_now(&store, "o2", "m1").await;
    assert!(read.allowed());
    assert_eq!(read.balance, micros(1_500));
}

/// A reversal never exceeds what the payment has left to reverse; refunds and chargebacks count together; a
/// partial refund after partial spend takes the unspent part from the lot and the rest is debt.
pub async fn reversals_are_capped_at_the_payment<S: LedgerStore>(store: S) {
    setup(&store).await;
    open(&store, "a1", "o1", eur(), "main").await;
    open(&store, "a2", "o2", eur(), "main").await;
    pay(&store, "p1", "a1", 1_000).await;
    let reversal = |kind, id: &str, amount: i64| Reversal {
        kind,
        id: credit_id(id),
        payment: credit_id("p1"),
        amount: micros(amount),
    };
    use ReversalKind::{Chargeback, Refund};
    assert_eq!(
        rule_of(store.reverse(&reversal(Refund, "big", 1_001)).await),
        Rule::ReversalCap
    );
    assert_eq!(
        (remaining(&store, "a1").await, debt(&store, "o1").await),
        (1_000, 0)
    );
    store
        .reverse(&reversal(Refund, "r1", 600))
        .await
        .expect("600 of 1000");
    assert_eq!(
        rule_of(store.reverse(&reversal(Refund, "r2", 500)).await),
        Rule::ReversalCap
    );
    assert_eq!(
        rule_of(store.reverse(&reversal(Chargeback, "c1", 401)).await),
        Rule::ReversalCap
    );
    store
        .reverse(&reversal(Refund, "r3", 400))
        .await
        .expect("exactly the rest");
    assert_eq!(
        rule_of(store.reverse(&reversal(Refund, "r4", 1)).await),
        Rule::ReversalCap
    );
    assert_eq!(
        (remaining(&store, "a1").await, debt(&store, "o1").await),
        (0, 0)
    );
    pay(&store, "p2", "a2", 1_000).await;
    let t = now(&store).await;
    take(&store, &[event("e1", "o2", "m1", 700, t)]).await;
    drain_all(&store, 10).await;
    // `r2` was refused above, which left nothing behind: the id is free for this refund.
    let partial = Reversal {
        kind: Refund,
        id: credit_id("r2"),
        payment: credit_id("p2"),
        amount: micros(500),
    };
    let done = store.reverse(&partial).await.expect("a partial refund");
    assert!(!done.replayed);
    assert_eq!(done.debt_added, micros(200));
    assert_eq!(
        (remaining(&store, "a2").await, debt(&store, "o2").await),
        (0, 200)
    );
}

/// Eight refunds of 300 at once against a payment of 1 000: exactly three pass the cap.
pub async fn concurrent_reversals_never_pass_the_cap<S: LedgerStore + Clone + 'static>(store: S) {
    setup(&store).await;
    open(&store, "a1", "o1", eur(), "main").await;
    pay(&store, "p1", "a1", 1_000).await;
    let tasks: Vec<_> = (0..8)
        .map(|n| {
            let store = store.clone();
            tokio::spawn(async move {
                store
                    .reverse(&Reversal {
                        kind: ReversalKind::Refund,
                        id: credit_id(&format!("r{n}")),
                        payment: credit_id("p1"),
                        amount: micros(300),
                    })
                    .await
                    .map_err(|e| e.rule())
            })
        })
        .collect();
    let mut passed = 0;
    for task in tasks {
        match task.await.expect("a reversal task") {
            Ok(_) => passed += 1,
            Err(rule) => assert_eq!(rule, Some(Rule::ReversalCap)),
        }
    }
    assert_eq!(passed, 3);
    assert_eq!(remaining(&store, "a1").await, 100);
}

/// A credit replay must match the expiry, the amount and the payment reversed.
pub async fn a_credit_replay_must_match_its_terms<S: LedgerStore>(store: S) {
    setup(&store).await;
    open(&store, "a1", "o1", eur(), "main").await;
    let t = now(&store).await;
    let (soon, later) = (plus(t, 30 * DAY), plus(t, 60 * DAY));
    let grant = |expires| credit_to(CreditSource::Promotion, "g1", "a1", 300, expires);
    assert!(
        !store
            .credit(&grant(Some(soon)))
            .await
            .expect("a promotion")
            .replayed
    );
    assert!(
        store
            .credit(&grant(Some(soon)))
            .await
            .expect("a replay")
            .replayed
    );
    assert_eq!(
        rule_of(store.credit(&grant(Some(later))).await),
        Rule::CreditConflict
    );
    assert_eq!(
        rule_of(store.credit(&grant(None)).await),
        Rule::CreditConflict
    );
    assert_eq!(remaining(&store, "a1").await, 300);

    pay(&store, "pa", "a1", 1_000).await;
    pay(&store, "pb", "a1", 1_000).await;
    let refund = |payment: &str, amount: i64| Reversal {
        kind: ReversalKind::Refund,
        id: credit_id("r1"),
        payment: credit_id(payment),
        amount: micros(amount),
    };
    store.reverse(&refund("pa", 100)).await.expect("a refund");
    assert!(
        store
            .reverse(&refund("pa", 100))
            .await
            .expect("a replay")
            .replayed
    );
    assert_eq!(
        rule_of(store.reverse(&refund("pb", 100)).await),
        Rule::CreditConflict
    );
    assert_eq!(
        rule_of(store.reverse(&refund("pa", 200)).await),
        Rule::CreditConflict
    );
    // A chargeback has ids of its own.
    let chargeback = Reversal {
        kind: ReversalKind::Chargeback,
        ..refund("pb", 100)
    };
    assert!(
        !store
            .reverse(&chargeback)
            .await
            .expect("a chargeback")
            .replayed
    );
    assert_eq!(remaining(&store, "a1").await, 300 + 900 + 900);
}

/// A debit draws the allowance, then promotions by soonest expiry, then payments oldest first; past all credit
/// the rest is debt. Across an owner's accounts, the label order comes first.
pub async fn the_debit_draws_in_the_configured_order<S: LedgerStore>(store: S) {
    setup(&store).await;
    open(&store, "a1", "o1", eur(), "main").await;
    // Booked in the opposite of the order they are spent in.
    pay(&store, "pay-old", "a1", 1_000).await;
    pay(&store, "pay-new", "a1", 1_000).await;
    let t = now(&store).await;
    for (id, days) in [("g-late", 60), ("g-soon", 10)] {
        store
            .credit(&credit_to(
                CreditSource::Promotion,
                id,
                "a1",
                1_000,
                Some(plus(t, days * DAY)),
            ))
            .await
            .expect("a promotion");
    }
    store
        .grant_allowance(&account("a1"), t.utc_month(), micros(500))
        .await
        .expect("an allowance");
    let order: Vec<_> = lots(&store, "a1")
        .await
        .iter()
        .map(|lot| (lot.kind, lot.credit.as_ref().map(|c| c.as_str().to_owned())))
        .collect();
    assert_eq!(
        order,
        [
            (LotKind::Allowance, None),
            (LotKind::Promotion, Some("g-soon".to_owned())),
            (LotKind::Promotion, Some("g-late".to_owned())),
            (LotKind::Payment, Some("pay-old".to_owned())),
            (LotKind::Payment, Some("pay-new".to_owned())),
        ]
    );
    let spend = |id: &str, amount: i64| event(id, "o1", "m1", amount, t);
    take(&store, &[spend("e1", 1_700)]).await;
    drain_all(&store, 10).await;
    assert_eq!(remainings(&store, "a1").await, [0, 0, 800, 1_000, 1_000]);
    take(&store, &[spend("e2", 1_000)]).await;
    drain_all(&store, 10).await;
    assert_eq!(remainings(&store, "a1").await, [0, 0, 0, 800, 1_000]);
    take(&store, &[spend("e3", 900)]).await;
    drain_all(&store, 10).await;
    assert_eq!(remainings(&store, "a1").await, [0, 0, 0, 0, 900]);
    take(&store, &[spend("e4", 1_000)]).await;
    drain_all(&store, 10).await;
    assert_eq!(remainings(&store, "a1").await, [0, 0, 0, 0, 0]);
    assert_eq!(
        (debt(&store, "o1").await, net(&store, "o1").await),
        (100, -100)
    );

    // Two accounts of one owner: `promo` is drawn before `main`, whatever their lots.
    open(&store, "b-main", "o2", eur(), "main").await;
    open(&store, "b-promo", "o2", eur(), "promo").await;
    pay(&store, "b-pay", "b-main", 500).await;
    store
        .credit(&credit_to(
            CreditSource::Promotion,
            "b-g",
            "b-promo",
            300,
            None,
        ))
        .await
        .expect("a promotion");
    take(&store, &[event("f1", "o2", "m1", 400, t)]).await;
    drain_all(&store, 10).await;
    assert_eq!(
        (
            remaining(&store, "b-promo").await,
            remaining(&store, "b-main").await
        ),
        (0, 400)
    );
    let balance = store
        .balance(&owner("o2"), &eur(), now(&store).await)
        .await
        .expect("balance");
    let labels: Vec<_> = balance.accounts.iter().map(|a| a.label.as_str()).collect();
    assert_eq!(labels, ["promo", "main"]);
    assert_eq!(balance.available, micros(400));
}

/// The label order and the lot order are configuration.
pub async fn the_draw_order_is_configuration<S: LedgerStore>(store: S) {
    setup(&store).await;
    let reordered = LedgerConfig {
        labels: vec![label("main"), label("promo")],
        lot_order: LotOrder(vec![
            (LotKind::Payment, WithinKind::OldestFirst),
            (LotKind::Promotion, WithinKind::SoonestExpiryFirst),
            (LotKind::Allowance, WithinKind::SoonestExpiryFirst),
            (LotKind::Adjustment, WithinKind::OldestFirst),
        ]),
        ..config()
    };
    store.configure(&reordered).await.expect("a new order");
    open(&store, "a-main", "o1", eur(), "main").await;
    open(&store, "a-promo", "o1", eur(), "promo").await;
    pay(&store, "p1", "a-main", 100).await;
    store
        .credit(&credit_to(
            CreditSource::Promotion,
            "g1",
            "a-main",
            100,
            None,
        ))
        .await
        .expect("a promotion");
    store
        .credit(&credit_to(
            CreditSource::Promotion,
            "g2",
            "a-promo",
            100,
            None,
        ))
        .await
        .expect("a promotion");
    let t = now(&store).await;
    take(&store, &[event("e1", "o1", "m1", 150, t)]).await;
    drain_all(&store, 10).await;
    let main: Vec<_> = lots(&store, "a-main")
        .await
        .iter()
        .map(|l| (l.kind, l.remaining.micros()))
        .collect();
    assert_eq!(main, [(LotKind::Payment, 0), (LotKind::Promotion, 50)]);
    assert_eq!(remaining(&store, "a-promo").await, 100);
    // A configuration may not drop a label or a currency accounts may name.
    let dropping = LedgerConfig {
        labels: vec![label("main")],
        ..config()
    };
    assert_eq!(
        rule_of(store.configure(&dropping).await),
        Rule::IncompleteConfig
    );
    let more = LedgerConfig {
        currencies: vec![
            eur(),
            usd(),
            openbilling_core::CurrencyCode::new("GBP").expect("a currency"),
        ],
        ..reordered
    };
    store.configure(&more).await.expect("a currency added");
}

/// A balance at zero or below is refused and above zero is allowed; an owner the ledger has never seen reads
/// as zero; an expired lot is not spendable.
pub async fn allowed_refuses_at_zero_or_below<S: LedgerStore>(store: S) {
    setup(&store).await;
    let unknown = allowed_now(&store, "o-new", "m1").await;
    assert_eq!(
        (unknown.refused, unknown.balance),
        (Some(NotAllowed::Balance), Amount::ZERO)
    );

    open(&store, "a1", "o1", eur(), "main").await;
    pay(&store, "p1", "a1", 100).await;
    assert!(allowed_now(&store, "o1", "m1").await.allowed());
    let t = now(&store).await;
    take(&store, &[event("e1", "o1", "m1", 100, t)]).await;
    drain_all(&store, 10).await;
    let at_zero = allowed_now(&store, "o1", "m1").await;
    assert_eq!(
        (at_zero.refused, at_zero.balance),
        (Some(NotAllowed::Balance), Amount::ZERO)
    );
    take(&store, &[event("e2", "o1", "m1", 40, t)]).await;
    drain_all(&store, 10).await;
    let below = allowed_now(&store, "o1", "m1").await;
    assert_eq!(
        (below.refused, below.balance),
        (Some(NotAllowed::Balance), micros(-40))
    );
    pay(&store, "p2", "a1", 41).await;
    let above = allowed_now(&store, "o1", "m1").await;
    assert_eq!((above.refused, above.balance), (None, micros(1)));

    open(&store, "a-old", "o-old", eur(), "main").await;
    store
        .credit(&credit_to(
            CreditSource::Promotion,
            "g",
            "a-old",
            50,
            Some(plus(t, -DAY)),
        ))
        .await
        .expect("an expired promotion is still booked");
    assert!(!allowed_now(&store, "o-old", "m1").await.allowed());
    assert_eq!(remaining(&store, "a-old").await, 50);
    let balance = store
        .balance(&owner("o-old"), &eur(), now(&store).await)
        .await
        .expect("balance");
    assert_eq!(balance.available, Amount::ZERO);
}

/// The member cap is per owner, currency, member and calendar month (UTC).
pub async fn the_member_cap_is_per_owner_currency_member_and_month<S: LedgerStore>(store: S) {
    setup(&store).await;
    open(&store, "a1", "o1", eur(), "main").await;
    pay(&store, "p1", "a1", 10_000).await;
    store
        .set_member_cap(&owner("o1"), &eur(), &member("m1"), Some(micros(300)))
        .await
        .expect("a cap");
    let t = now(&store).await;
    assert_eq!(
        allowed_at(&store, "o1", "m2", t).await.member_remaining,
        None
    );
    assert_eq!(
        allowed_at(&store, "o1", "m1", t).await.member_remaining,
        Some(micros(300))
    );

    take(
        &store,
        &[
            event("e1", "o1", "m1", 200, t),
            event("e2", "o1", "m2", 900, t),
        ],
    )
    .await;
    drain_all(&store, 10).await;
    let partly = allowed_at(&store, "o1", "m1", t).await;
    assert_eq!(
        (partly.refused, partly.member_remaining),
        (None, Some(micros(100)))
    );
    take(&store, &[event("e3", "o1", "m1", 100, t)]).await;
    drain_all(&store, 10).await;
    let capped = allowed_at(&store, "o1", "m1", t).await;
    assert_eq!(
        (capped.refused, capped.member_remaining),
        (Some(NotAllowed::MemberCap), Some(Amount::ZERO))
    );
    assert!(capped.balance.is_positive());
    assert!(allowed_at(&store, "o1", "m2", t).await.allowed());

    // The next calendar month starts the count again.
    let next_month = t.utc_month().end();
    let fresh = allowed_at(&store, "o1", "m1", next_month).await;
    assert_eq!(
        (fresh.refused, fresh.member_remaining),
        (None, Some(micros(300)))
    );
    // The cap is per currency.
    let other = store
        .allowed(&owner("o1"), &usd(), &member("m1"), t)
        .await
        .expect("allowed");
    assert_eq!(
        (other.refused, other.member_remaining),
        (Some(NotAllowed::Balance), None)
    );
    // And it can be removed.
    store
        .set_member_cap(&owner("o1"), &eur(), &member("m1"), None)
        .await
        .expect("no cap");
    assert_eq!(
        allowed_at(&store, "o1", "m1", t).await.member_remaining,
        None
    );
}

/// An event outside the time window refuses the whole batch, naming it and not the good one; just inside both
/// edges is accepted. Assumes the default window: seven days and five minutes.
pub async fn an_event_outside_the_window_refuses_the_batch_naming_it<S: LedgerStore>(store: S) {
    setup(&store).await;
    let window = Window::default();
    let t = now(&store).await;
    let first_day = UtcDay::from_unix_days(t.utc_day().unix_days() - i64::from(window.days()) + 1)
        .expect("a day")
        .start();
    let fine = event("fine", "o1", "m1", 5, t);
    // A day earlier than needed, and a minute later than needed, so a test that crosses midnight still holds.
    for (id, at, rule) in [
        ("too-old", plus(first_day, -DAY), Rule::TooOld),
        (
            "too-new",
            plus(t, window.skew_micros() + MINUTE),
            Rule::TooFarAhead,
        ),
    ] {
        let (refused, named) = refusal_of(
            store
                .intake(&[fine.clone(), event(id, "o1", "m1", 5, at)])
                .await,
        );
        assert_eq!((refused, named), (rule, vec![id.to_owned()]));
    }
    assert_eq!(
        take(&store, &[fine]).await.accepted(),
        1,
        "nothing was written by a refused batch"
    );
    let edges = take(
        &store,
        &[
            event("old-ok", "o1", "m1", 1, plus(first_day, HOUR)),
            event(
                "new-ok",
                "o1",
                "m1",
                1,
                plus(t, window.skew_micros() - MINUTE),
            ),
        ],
    )
    .await;
    assert_eq!(edges.accepted(), 2);
}

/// A batch whose sums overflow is refused, naming the owner's events, not a fault. A debt never
/// overflows; the debit that would make it is refused `limit`.
pub async fn sums_that_overflow_are_refused_and_a_debt_never_overflows<S: LedgerStore>(store: S) {
    setup(&store).await;
    open(&store, "a1", "o1", eur(), "main").await;
    pay(&store, "p1", "a1", 10_000).await;
    let t = now(&store).await;
    let big = |id: &str, who: &str| event(id, who, "m1", i64::MAX, t);
    let (rule, named) = refusal_of(
        store
            .intake(&[big("big1", "o1"), big("other", "o2"), big("big2", "o1")])
            .await,
    );
    assert_eq!(
        (rule, named),
        (Rule::Overflow, vec!["big1".to_owned(), "big2".to_owned()])
    );
    assert!(store.drain(10).await.expect("drain").decided.is_empty());
    assert_eq!(remaining(&store, "a1").await, 10_000);

    // Each alone fits; together, the second would take the debt past the largest amount.
    take(&store, &[big("big1", "o1")]).await;
    take(&store, &[big("big2", "o1")]).await;
    drain_all(&store, 10).await;
    let first = store
        .outcome(&event_id("big1"))
        .await
        .expect("outcome")
        .expect("decided");
    let second = store
        .outcome(&event_id("big2"))
        .await
        .expect("outcome")
        .expect("decided");
    assert_eq!(first.outcome, EventOutcome::Applied);
    assert_eq!(second.outcome, EventOutcome::Refused(RefusalReason::Limit));
    assert_eq!(debt(&store, "o1").await, i64::MAX - 10_000);
    assert_eq!(pay(&store, "p2", "a1", 5).await.repaid, micros(5));
}

/// A repeated event id in one batch counts once unless the copies differ. An id already stored for its
/// day with another payload is a conflict, naming it, and the batch writes nothing.
pub async fn a_repeat_counts_once_and_a_differing_payload_is_refused<S: LedgerStore>(store: S) {
    setup(&store).await;
    let t = now(&store).await;
    let e = event("dup", "o1", "m1", 10, t);
    let same = take(&store, &[e.clone(), e.clone()]).await;
    assert_eq!((same.accepted(), same.replayed()), (1, 1));

    let first = event("dup2", "o1", "m1", 10, t);
    let mut variants = Vec::new();
    let mut copy = first.clone();
    copy.owner = owner("o2");
    variants.push(copy);
    let mut copy = first.clone();
    copy.member = member("m9");
    variants.push(copy);
    let mut copy = first.clone();
    copy.amount = micros(99);
    variants.push(copy);
    let mut copy = first.clone();
    copy.currency = usd();
    variants.push(copy);
    let mut copy = first.clone();
    copy.occurred_at = plus(t, -HOUR);
    variants.push(copy);
    for copy in variants {
        let (rule, named) = refusal_of(
            store
                .intake(&[event("fine", "o1", "m1", 1, t), first.clone(), copy])
                .await,
        );
        assert_eq!(
            (rule, named),
            (Rule::DifferingDuplicate, vec!["dup2".to_owned()])
        );
    }

    let mut stored = e.clone();
    stored.amount = micros(11);
    let (rule, named) = refusal_of(
        store
            .intake(&[event("fine", "o1", "m1", 1, t), stored])
            .await,
    );
    assert_eq!(
        (rule, named),
        (Rule::PayloadConflict, vec!["dup".to_owned()])
    );
    let after = take(&store, &[event("fine", "o1", "m1", 1, t), first, e]).await;
    let answers: Vec<_> = after
        .answers
        .iter()
        .map(|(id, a)| (id.as_str(), *a))
        .collect();
    assert_eq!(
        answers,
        [
            ("fine", Intake::Accepted),
            ("dup2", Intake::Accepted),
            ("dup", Intake::Replayed)
        ]
    );
}

/// ADR 0001, point 15: the same id on another occurrence day is accepted at intake and refused `id_reused` at
/// the drain; the first received wins, in one drain or a later one.
pub async fn the_same_id_on_another_day_is_refused_as_id_reused<S: LedgerStore>(store: S) {
    setup(&store).await;
    open(&store, "a1", "o1", eur(), "main").await;
    pay(&store, "p1", "a1", 1_000).await;
    let t = now(&store).await;
    let (two_days_ago, yesterday) = (plus(t, -2 * DAY), plus(t, -DAY));
    take(&store, &[event("x", "o1", "m1", 10, two_days_ago)]).await;
    take(&store, &[event("x", "o1", "m1", 20, yesterday)]).await;
    let report = drain_all(&store, 10).await;
    let decided: Vec<_> = report
        .decided
        .iter()
        .map(|d| (d.occurred_on, d.outcome))
        .collect();
    assert_eq!(
        decided,
        [
            (two_days_ago.utc_day(), EventOutcome::Applied),
            (
                yesterday.utc_day(),
                EventOutcome::Refused(RefusalReason::IdReused)
            ),
        ]
    );
    assert_eq!(remaining(&store, "a1").await, 990);
    let kept = store
        .outcome(&event_id("x"))
        .await
        .expect("outcome")
        .expect("decided");
    assert_eq!(
        (kept.occurred_on, kept.amount),
        (two_days_ago.utc_day(), micros(10))
    );
    assert_eq!(
        take(&store, &[event("x", "o1", "m1", 20, yesterday)])
            .await
            .replayed(),
        1
    );
    assert!(drain_all(&store, 10).await.decided.is_empty());

    take(&store, &[event("y", "o1", "m1", 5, two_days_ago)]).await;
    drain_all(&store, 10).await;
    take(&store, &[event("y", "o1", "m1", 6, yesterday)]).await;
    let later = drain_all(&store, 10).await;
    assert_eq!(later.decided.len(), 1);
    assert_eq!(
        later.decided[0].outcome,
        EventOutcome::Refused(RefusalReason::IdReused)
    );
    assert_eq!(remaining(&store, "a1").await, 985);
}

/// An event for an owner with no account in its currency is refused `unknown_account` and moves nothing.
pub async fn an_owner_without_an_account_in_the_currency_is_refused<S: LedgerStore>(store: S) {
    setup(&store).await;
    open(&store, "a1", "o1", eur(), "main").await;
    pay(&store, "p1", "a1", 100).await;
    let t = now(&store).await;
    let mut in_usd = event("u2", "o1", "m1", 5, t);
    in_usd.currency = usd();
    take(&store, &[event("u1", "o-none", "m1", 5, t), in_usd]).await;
    let report = drain_all(&store, 10).await;
    assert_eq!(report.applied(), 0);
    for decided in &report.decided {
        assert_eq!(
            decided.outcome,
            EventOutcome::Refused(RefusalReason::UnknownAccount)
        );
    }
    assert_eq!(report.decided.len(), 2);
    assert_eq!(remaining(&store, "a1").await, 100);
    assert!(
        store
            .daily_usage(&owner("o1"), &usd())
            .await
            .expect("usage")
            .is_empty()
    );
}

/// ADR 0001, point 23: a debit that would take the owner's debt past its limit is refused `limit`, not lost.
pub async fn a_debit_past_the_debt_limit_is_refused<S: LedgerStore>(store: S) {
    setup(&store).await;
    open(&store, "a1", "o1", eur(), "main").await;
    pay(&store, "p1", "a1", 100).await;
    store
        .set_debt_limit(&owner("o1"), &eur(), Some(micros(50)))
        .await
        .expect("a limit");
    let t = now(&store).await;
    let mut outcomes = Vec::new();
    for (id, amount) in [("e1", 120), ("e2", 40), ("e3", 30)] {
        take(&store, &[event(id, "o1", "m1", amount, t)]).await;
        outcomes.push(drain_all(&store, 10).await.decided[0].outcome);
    }
    assert_eq!(
        outcomes,
        [
            EventOutcome::Applied,
            EventOutcome::Refused(RefusalReason::Limit),
            EventOutcome::Applied
        ]
    );
    assert_eq!(
        (remaining(&store, "a1").await, debt(&store, "o1").await),
        (0, 50)
    );
    store
        .set_debt_limit(&owner("o1"), &eur(), None)
        .await
        .expect("no limit");
    take(&store, &[event("e4", "o1", "m1", 1_000, t)]).await;
    assert_eq!(drain_all(&store, 10).await.applied(), 1);
    assert_eq!(debt(&store, "o1").await, 1_050);
    assert_eq!(
        rule_of(
            store
                .set_debt_limit(&owner("o-none"), &eur(), Some(micros(1)))
                .await
        ),
        Rule::UnknownAccount
    );
}

/// The allowance is granted once per account and month; a month that is not this one is not spendable now;
/// new allowance repays debt first.
pub async fn the_allowance_is_granted_once_per_account_and_month<S: LedgerStore>(store: S) {
    setup(&store).await;
    open(&store, "a1", "o1", eur(), "main").await;
    open(&store, "a2", "o2", eur(), "main").await;
    let t = now(&store).await;
    let month = t.utc_month();
    let first = store
        .grant_allowance(&account("a1"), month, micros(1_600))
        .await
        .expect("an allowance");
    assert!(!first.replayed);
    let again = store
        .grant_allowance(&account("a1"), month, micros(1_600))
        .await
        .expect("a replay");
    assert!(again.replayed);
    assert_eq!(again.transaction, first.transaction);
    assert_eq!(
        rule_of(
            store
                .grant_allowance(&account("a1"), month, micros(1))
                .await
        ),
        Rule::CreditConflict
    );
    store
        .grant_allowance(&account("a2"), month, micros(1_600))
        .await
        .expect("another account");
    let next = month.next().expect("a next month");
    store
        .grant_allowance(&account("a1"), next, micros(1_600))
        .await
        .expect("next month");
    let months: Vec<_> = lots(&store, "a1")
        .await
        .iter()
        .map(|l| (l.kind, l.month))
        .collect();
    assert_eq!(
        months,
        [
            (LotKind::Allowance, Some(month)),
            (LotKind::Allowance, Some(next))
        ]
    );
    let balance = store
        .balance(&owner("o1"), &eur(), t)
        .await
        .expect("balance");
    assert_eq!(
        balance.available,
        micros(1_600),
        "next month's allowance is not spendable this month"
    );
    assert_eq!(
        rule_of(
            store
                .grant_allowance(&account("a-none"), month, micros(1))
                .await
        ),
        Rule::UnknownAccount
    );

    open(&store, "a3", "o3", eur(), "main").await;
    take(&store, &[event("e1", "o3", "m1", 400, t)]).await;
    drain_all(&store, 10).await;
    assert_eq!(debt(&store, "o3").await, 400);
    let granted = store
        .grant_allowance(&account("a3"), month, micros(1_000))
        .await
        .expect("an allowance");
    assert_eq!(granted.repaid, micros(400));
    assert_eq!(
        (debt(&store, "o3").await, remaining(&store, "a3").await),
        (0, 600)
    );
}

/// A positive adjustment opens a lot; a negative one takes credit from its
/// account's lots in lot order, and what they lack is debt; both are once-only.
pub async fn adjustments_add_and_take_credit<S: LedgerStore>(store: S) {
    setup(&store).await;
    open(&store, "a1", "o1", eur(), "main").await;
    open(&store, "a-promo", "o1", eur(), "promo").await;
    store
        .credit(&credit_to(
            CreditSource::Promotion,
            "g1",
            "a-promo",
            100,
            None,
        ))
        .await
        .expect("a promotion");
    let adjust =
        |id: &str, amount: i64| credit_to(CreditSource::Adjustment, id, "a1", amount, None);
    assert!(
        !store
            .credit(&adjust("adj1", 500))
            .await
            .expect("an adjustment")
            .replayed
    );
    assert!(
        store
            .credit(&adjust("adj1", 500))
            .await
            .expect("a replay")
            .replayed
    );
    assert_eq!(lots(&store, "a1").await[0].kind, LotKind::Adjustment);
    pay(&store, "p1", "a1", 300).await;

    let taken = store
        .credit(&adjust("adj2", -600))
        .await
        .expect("a negative adjustment");
    assert_eq!(taken.debt_added, Amount::ZERO);
    let left: Vec<_> = lots(&store, "a1")
        .await
        .iter()
        .map(|l| (l.kind, l.remaining.micros()))
        .collect();
    assert_eq!(left, [(LotKind::Adjustment, 0), (LotKind::Payment, 200)]);
    let short = store
        .credit(&adjust("adj3", -1_000))
        .await
        .expect("a negative adjustment");
    assert_eq!(short.debt_added, micros(800));
    assert_eq!(
        (remaining(&store, "a1").await, debt(&store, "o1").await),
        (0, 800)
    );
    assert!(
        store
            .credit(&adjust("adj3", -1_000))
            .await
            .expect("a replay")
            .replayed
    );
    assert_eq!(
        rule_of(store.credit(&adjust("adj3", -999)).await),
        Rule::CreditConflict
    );
    assert_eq!(debt(&store, "o1").await, 800);
    // Only the adjusted account's lots were touched.
    assert_eq!(remaining(&store, "a-promo").await, 100);
}

/// ADR 0002: accounts are once-only by id, and an owner has one account per currency and label.
pub async fn accounts_are_once_only_and_one_per_owner_currency_and_label<S: LedgerStore>(store: S) {
    setup(&store).await;
    let new = |id: &str, who: &str, currency, name: &str| openbilling_core::NewAccount {
        id: account(id),
        owner: owner(who),
        currency,
        label: label(name),
    };
    assert!(
        !store
            .open_account(&new("a1", "o1", eur(), "main"))
            .await
            .expect("open")
            .replayed
    );
    assert!(
        store
            .open_account(&new("a1", "o1", eur(), "main"))
            .await
            .expect("replay")
            .replayed
    );
    assert_eq!(
        rule_of(store.open_account(&new("a1", "o1", usd(), "main")).await),
        Rule::AccountConflict
    );
    assert_eq!(
        rule_of(store.open_account(&new("a9", "o1", eur(), "main")).await),
        Rule::AccountConflict
    );
    let gbp = openbilling_core::CurrencyCode::new("GBP").expect("a currency");
    assert_eq!(
        rule_of(store.open_account(&new("a2", "o1", gbp, "main")).await),
        Rule::UnknownCurrency
    );
    assert_eq!(
        rule_of(store.open_account(&new("a2", "o1", eur(), "vip")).await),
        Rule::UnknownLabel
    );
    assert!(
        !store
            .open_account(&new("a2", "o1", usd(), "main"))
            .await
            .expect("another currency")
            .replayed
    );
    assert!(
        !store
            .open_account(&new("a3", "o1", eur(), "promo"))
            .await
            .expect("another label")
            .replayed
    );

    let to_nowhere = credit_to(CreditSource::Payment, "p1", "a-none", 1, None);
    assert_eq!(
        rule_of(store.credit(&to_nowhere).await),
        Rule::UnknownAccount
    );
    let by_missing_label = openbilling_core::Credit {
        to: CreditTarget::Label {
            owner: owner("o2"),
            currency: eur(),
            label: label("main"),
        },
        ..credit_to(CreditSource::Payment, "p1", "a1", 1, None)
    };
    assert_eq!(
        rule_of(store.credit(&by_missing_label).await),
        Rule::UnknownAccount
    );
    let dropping = LedgerConfig {
        currencies: vec![eur()],
        ..config()
    };
    assert_eq!(
        rule_of(store.configure(&dropping).await),
        Rule::IncompleteConfig
    );
}

/// Debits are summed per owner, currency and UTC day of occurrence, and refused events are not counted.
pub async fn daily_usage_sums_applied_debits_per_utc_day<S: LedgerStore>(store: S) {
    setup(&store).await;
    open(&store, "a1", "o1", eur(), "main").await;
    pay(&store, "p1", "a1", 100_000).await;
    store
        .set_debt_limit(&owner("o1"), &eur(), Some(Amount::ZERO))
        .await
        .expect("a limit");
    let t = now(&store).await;
    let noon = |days_ago: i64| {
        plus(
            UtcDay::from_unix_days(t.utc_day().unix_days() - days_ago)
                .expect("a day")
                .start(),
            12 * HOUR,
        )
    };
    take(
        &store,
        &[
            event("a", "o1", "m1", 10, noon(2)),
            event("b", "o1", "m1", 20, noon(2)),
            event("c", "o1", "m1", 5, noon(1)),
            event("too-much", "o1", "m1", 1_000_000, noon(1)),
        ],
    )
    .await;
    drain_all(&store, 10).await;
    let days = store
        .daily_usage(&owner("o1"), &eur())
        .await
        .expect("usage");
    assert_eq!(
        days,
        [
            (noon(2).utc_day(), micros(30)),
            (noon(1).utc_day(), micros(5))
        ]
    );
}

/// The store refuses amounts with the wrong sign for their source or kind, and a malformed configuration.
pub async fn invalid_amounts_and_configurations_are_refused<S: LedgerStore>(store: S) {
    setup(&store).await;
    open(&store, "a1", "o1", eur(), "main").await;
    let t = now(&store).await;
    for (source, amount, expires) in [
        (CreditSource::Payment, 0, None),
        (CreditSource::Payment, -5, None),
        (CreditSource::Promotion, -5, None),
        (CreditSource::Adjustment, 0, None),
        (CreditSource::Adjustment, -5, Some(t)),
    ] {
        let credit = credit_to(source, "bad", "a1", amount, expires);
        assert_eq!(
            rule_of(store.credit(&credit).await),
            Rule::InvalidAmount,
            "{source} {amount}"
        );
    }
    pay(&store, "p1", "a1", 10).await;
    for amount in [0, -1] {
        let reversal = Reversal {
            kind: ReversalKind::Refund,
            id: credit_id("r"),
            payment: credit_id("p1"),
            amount: micros(amount),
        };
        assert_eq!(rule_of(store.reverse(&reversal).await), Rule::InvalidAmount);
    }
    assert_eq!(
        rule_of(
            store
                .grant_allowance(&account("a1"), t.utc_month(), Amount::ZERO)
                .await
        ),
        Rule::InvalidAmount
    );
    for bad in [
        LedgerConfig {
            currencies: vec![],
            ..config()
        },
        LedgerConfig {
            labels: vec![label("main"), label("main")],
            ..config()
        },
        LedgerConfig {
            lot_order: LotOrder(LotOrder::default().0[..3].to_vec()),
            ..config()
        },
    ] {
        assert_eq!(rule_of(store.configure(&bad).await), Rule::InvalidConfig);
    }
    let too_many: Vec<UsageEvent> = (0..1001)
        .map(|n| event(&format!("e{n}"), "o1", "m1", 1, t))
        .collect();
    assert_eq!(rule_of(store.intake(&too_many).await), Rule::TooManyEvents);
    assert!(store.intake(&too_many[..1000]).await.is_ok());
}
