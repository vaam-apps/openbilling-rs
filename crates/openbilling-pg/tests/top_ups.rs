//! The top-up conformance suite of `openbilling-testkit` against the PostgreSQL store, and what only this store can
//! show: the guard on the table, and the SQL functions answering as the migration documents. A fresh database per case.
//!
//! Needs `OPENBILLING_TEST_DATABASE_URL`; skips without it, and fails instead under `OPENBILLING_TEST_REQUIRE_DB=1`.

#![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]

mod support;

use openbilling_core::UnrecordedTopUps;
use openbilling_pg::{LedgerSettings, PgLedger};
use openbilling_testkit::fixtures::setup;

async fn fixture(unrecorded: UnrecordedTopUps) -> Option<PgLedger> {
    let store = support::fresh_store().await?;
    let ledger = PgLedger::new(
        &store,
        LedgerSettings {
            unrecorded_top_ups: unrecorded,
            ..LedgerSettings::default()
        },
    );
    ledger.prepare_partitions().await.expect("partitions");
    Some(ledger)
}

openbilling_testkit::top_up_conformance!(fixture);

fn sqlstate(error: &sqlx::Error) -> String {
    match error {
        sqlx::Error::Database(database) => {
            database.code().map(|c| c.into_owned()).unwrap_or_default()
        }
        other => panic!("not a database error: {other:?}"),
    }
}

/// A top-up's terms never change, it is never deleted, and a final state is final: the guard says OB901.
#[tokio::test]
async fn the_table_refuses_what_the_functions_never_do() {
    let Some(store) = support::fresh_store().await else {
        return;
    };
    let ledger = PgLedger::new(&store, LedgerSettings::default());
    setup(&ledger).await;
    let pool = store.pool();
    sqlx::query("SELECT ob_open_account('a1', 'o1', 'EUR', 'main')")
        .execute(pool)
        .await
        .unwrap();
    sqlx::query("SELECT ob_record_top_up('p1', 'k', 'o1', 'EUR', 'main', 10)")
        .execute(pool)
        .await
        .unwrap();
    for sql in [
        "UPDATE ob_top_up SET amount = 11",
        "UPDATE ob_top_up SET owner = 'o2'",
        "UPDATE ob_top_up SET state = 'pending', mismatch_field = 'owner'",
        "DELETE FROM ob_top_up",
        "TRUNCATE ob_top_up",
    ] {
        let error = sqlx::query(sql).execute(pool).await.unwrap_err();
        assert_eq!(sqlstate(&error), "OB901", "{sql}");
    }
    // The state and the mismatch field go together (a check), and a touch is allowed while pending.
    assert_eq!(
        sqlstate(
            &sqlx::query("UPDATE ob_top_up SET state = 'mismatched'")
                .execute(pool)
                .await
                .unwrap_err()
        ),
        "23514"
    );
    sqlx::query("UPDATE ob_top_up SET last_read_at = now()")
        .execute(pool)
        .await
        .unwrap();
    sqlx::query("UPDATE ob_top_up SET state = 'canceled'")
        .execute(pool)
        .await
        .unwrap();
    let error = sqlx::query("UPDATE ob_top_up SET state = 'credited'")
        .execute(pool)
        .await
        .unwrap_err();
    assert_eq!(sqlstate(&error), "OB901", "a final state is final");
}

fn due(batch: u32) -> openbilling_core::ReconcileSettings {
    openbilling_core::ReconcileSettings {
        pending_after: std::time::Duration::ZERO,
        read_again_after: std::time::Duration::ZERO,
        stop_after: std::time::Duration::from_secs(3600),
        batch,
        ..Default::default()
    }
}

/// A row another connection holds locked is skipped by a claim, not waited for, and does not count against the limit:
/// the claim takes the rows that are free, and the locked one is claimed once it is released.
#[tokio::test]
async fn a_claim_skips_a_row_another_connection_holds_locked() {
    use openbilling_core::TopUpLedger;
    let Some(store) = support::fresh_store().await else {
        return;
    };
    let ledger = PgLedger::new(&store, LedgerSettings::default());
    setup(&ledger).await;
    let pool = store.pool();
    sqlx::query("SELECT ob_open_account('a1', 'o1', 'EUR', 'main')")
        .execute(pool)
        .await
        .unwrap();
    for id in ["p1", "p2", "p3"] {
        sqlx::query("SELECT ob_record_top_up($1, $2, 'o1', 'EUR', 'main', 10)")
            .bind(id)
            .bind(format!("k-{id}"))
            .execute(pool)
            .await
            .unwrap();
    }
    // The oldest row is held by another connection, in an open transaction (as a settling webhook would hold it).
    let mut holder = pool.begin().await.unwrap();
    sqlx::query("SELECT payment_id FROM ob_top_up WHERE payment_id = 'p1' FOR UPDATE")
        .execute(&mut *holder)
        .await
        .unwrap();
    let claimed = tokio::time::timeout(
        std::time::Duration::from_secs(10),
        ledger.claim_pending_top_ups(&due(2)),
    )
    .await
    .expect("a claim does not wait for a locked row")
    .unwrap();
    let mut ids: Vec<_> = claimed.iter().map(|c| c.as_str().to_owned()).collect();
    ids.sort_unstable();
    assert_eq!(
        ids,
        ["p2", "p3"],
        "the locked row is skipped and the limit is met by the others"
    );
    holder.rollback().await.unwrap();
    let claimed = ledger.claim_pending_top_ups(&due(10)).await.unwrap();
    assert_eq!(claimed.len(), 3, "once released it is claimed again");
}

/// Under `refuse`, a succeeded payment nobody recorded is kept as a mismatched top-up of the field `unrecorded`, with
/// the values read back, so an operator can list it; reading it again changes nothing and it is never claimed.
#[tokio::test]
async fn an_unrecorded_payment_is_kept_for_the_operator() {
    use openbilling_core::{
        Amount, CreditId, PaymentStatus, PaymentTarget, PaymentView, TopUpLedger,
    };
    use openbilling_testkit::fixtures::{eur, label, owner};
    let Some(store) = support::fresh_store().await else {
        return;
    };
    let ledger = PgLedger::new(&store, LedgerSettings::default());
    setup(&ledger).await;
    let view = PaymentView {
        id: CreditId::new("u1").unwrap(),
        status: PaymentStatus::Succeeded,
        amount: Amount::from_micros(7).unwrap(),
        currency: eur(),
        to: Some(PaymentTarget {
            owner: owner("o9"),
            label: label("main"),
        }),
    };
    for _ in 0..2 {
        ledger.settle_top_up(&view).await.unwrap();
    }
    let pool = store.pool();
    let row: (String, Option<String>, String, String, String, i64, Option<String>) = sqlx::query_as(
        "SELECT state, mismatch_field, owner, currency, label, amount, idem_key FROM ob_top_up WHERE payment_id = 'u1'",
    )
    .fetch_one(pool)
    .await
    .unwrap();
    assert_eq!(
        row,
        (
            "mismatched".into(),
            Some("unrecorded".into()),
            "o9".into(),
            "EUR".into(),
            "main".into(),
            7,
            None
        )
    );
    let (n,): (i64,) = sqlx::query_as("SELECT count(*) FROM ob_top_up")
        .fetch_one(pool)
        .await
        .unwrap();
    assert_eq!(n, 1, "one row however often it is read");
    assert!(
        ledger
            .claim_pending_top_ups(&due(10))
            .await
            .unwrap()
            .is_empty()
    );
}

/// A webhook and the reconciler settling the same paid top-up book it once, whichever gets the row first, and the test
/// does not leave the order to the scheduler: the row is held `FOR UPDATE` by a third connection, the reconciler has
/// claimed it already, both settles are started and seen waiting on the lock in `pg_stat_activity`, and only then is
/// the lock released. Exactly one credit reaches the ledger, and the two answers are one credit and one replay.
#[tokio::test]
async fn a_webhook_and_a_reconcile_pass_settling_one_row_credit_once() {
    use openbilling_core::{
        Amount, CreditId, IdempotencyKey, PaymentStatus, PaymentTarget, PaymentView, TopUp,
        TopUpLedger, TopUpSettled,
    };
    use openbilling_testkit::fixtures::{eur, label, owner};
    let Some(store) = support::fresh_store().await else {
        return;
    };
    let ledger = PgLedger::new(&store, LedgerSettings::default());
    setup(&ledger).await;
    let pool = store.pool();
    sqlx::query("SELECT ob_open_account('a1', 'o1', 'EUR', 'main')")
        .execute(pool)
        .await
        .unwrap();
    let target = PaymentTarget {
        owner: owner("o1"),
        label: label("main"),
    };
    ledger
        .record_top_up(&TopUp {
            payment: CreditId::new("race").unwrap(),
            key: IdempotencyKey::new("k-race").unwrap(),
            to: target.clone(),
            currency: eur(),
            amount: Amount::from_micros(9).unwrap(),
        })
        .await
        .unwrap();
    // The reconciler's claim comes first (it marks the row read).
    let claimed = ledger.claim_pending_top_ups(&due(10)).await.unwrap();
    assert_eq!(claimed.len(), 1);
    let view = PaymentView {
        id: CreditId::new("race").unwrap(),
        status: PaymentStatus::Succeeded,
        amount: Amount::from_micros(9).unwrap(),
        currency: eur(),
        to: Some(target),
    };
    // A third connection holds the row.
    let mut holder = pool.begin().await.unwrap();
    sqlx::query("SELECT payment_id FROM ob_top_up WHERE payment_id = 'race' FOR UPDATE")
        .execute(&mut *holder)
        .await
        .unwrap();
    // While it is held, another claim skips the row and does not wait.
    let again = ReconcileSettingsAlways::get();
    let during = tokio::time::timeout(
        std::time::Duration::from_secs(10),
        ledger.claim_pending_top_ups(&again),
    )
    .await
    .expect("a claim does not wait for a held row")
    .unwrap();
    assert!(during.is_empty(), "the held row is skipped");

    let webhook = tokio::spawn({
        let (ledger, view) = (ledger.clone(), view.clone());
        async move { ledger.settle_top_up(&view).await }
    });
    let reconciler = tokio::spawn({
        let (ledger, view) = (ledger.clone(), view.clone());
        async move { ledger.settle_top_up(&view).await }
    });
    // Both are waiting on the row before it is released.
    let mut waiting = 0_i64;
    for _ in 0..200 {
        (waiting,) = sqlx::query_as(
            "SELECT count(*) FROM pg_stat_activity \
             WHERE wait_event_type = 'Lock' AND query LIKE '%ob_settle_top_up%'",
        )
        .fetch_one(pool)
        .await
        .unwrap();
        if waiting == 2 {
            break;
        }
        tokio::time::sleep(std::time::Duration::from_millis(50)).await;
    }
    assert_eq!(waiting, 2, "both settles wait for the held row");
    holder.commit().await.unwrap();
    let results = [
        webhook.await.unwrap().unwrap(),
        reconciler.await.unwrap().unwrap(),
    ];
    let replays: Vec<bool> = results
        .iter()
        .map(|r| match r {
            TopUpSettled::Credited(c) => c.replayed,
            other => panic!("not credited: {other:?}"),
        })
        .collect();
    assert_eq!(
        replays.iter().filter(|r| !**r).count(),
        1,
        "exactly one fresh credit: {replays:?}"
    );
    assert_eq!(
        replays.iter().filter(|r| **r).count(),
        1,
        "the other is a replay: {replays:?}"
    );
    let (credits, total): (i64, Option<i64>) = sqlx::query_as(
        "SELECT count(*), sum(amount)::bigint FROM ob_credit WHERE source = 'payment'",
    )
    .fetch_one(pool)
    .await
    .unwrap();
    assert_eq!((credits, total), (1, Some(9)), "one credit in the ledger");
}

struct ReconcileSettingsAlways;
impl ReconcileSettingsAlways {
    fn get() -> openbilling_core::ReconcileSettings {
        openbilling_core::ReconcileSettings {
            read_again_after: std::time::Duration::ZERO,
            ..due(10)
        }
    }
}

/// The guard's transitions: an expired top-up can be settled but not revived or touched, and a `mismatched` row is
/// final unless it is the `unrecorded` one a late record upgrades with the very terms (and nothing else changes).
#[tokio::test]
async fn the_guard_allows_exactly_the_documented_transitions() {
    let Some(store) = support::fresh_store().await else {
        return;
    };
    let ledger = PgLedger::new(&store, LedgerSettings::default());
    setup(&ledger).await;
    let pool = store.pool();
    sqlx::query("SELECT ob_open_account('a1', 'o1', 'EUR', 'main')")
        .execute(pool)
        .await
        .unwrap();
    for id in ["e1", "e2"] {
        sqlx::query("SELECT ob_record_top_up($1, $2, 'o1', 'EUR', 'main', 10)")
            .bind(id)
            .bind(format!("k-{id}"))
            .execute(pool)
            .await
            .unwrap();
    }
    sqlx::query("UPDATE ob_top_up SET state = 'expired'")
        .execute(pool)
        .await
        .unwrap();
    for sql in [
        "UPDATE ob_top_up SET state = 'pending'",
        "UPDATE ob_top_up SET last_read_at = now()",
        "UPDATE ob_top_up SET state = 'expired'",
    ] {
        let error = sqlx::query(sql).execute(pool).await.unwrap_err();
        assert_eq!(sqlstate(&error), "OB901", "{sql}");
    }
    sqlx::query("UPDATE ob_top_up SET state = 'credited' WHERE payment_id = 'e1'")
        .execute(pool)
        .await
        .unwrap();
    sqlx::query("UPDATE ob_top_up SET state = 'canceled' WHERE payment_id = 'e2'")
        .execute(pool)
        .await
        .unwrap();
    // A refused row of another field is final; only `unrecorded` is upgradable, and only to a pending row with a key.
    sqlx::query(
        "INSERT INTO ob_top_up (payment_id, idem_key, owner, currency, label, amount, state, mismatch_field) \
         VALUES ('m1', 'k', 'o1', 'EUR', 'main', 10, 'mismatched', 'amount'), \
                ('m2', NULL, 'o1', 'EUR', 'main', 10, 'mismatched', 'unrecorded')",
    )
    .execute(pool)
    .await
    .unwrap();
    for sql in [
        "UPDATE ob_top_up SET state = 'pending', mismatch_field = NULL WHERE payment_id = 'm1'",
        "UPDATE ob_top_up SET state = 'pending', mismatch_field = NULL WHERE payment_id = 'm2'",
        "UPDATE ob_top_up SET state = 'pending', mismatch_field = NULL, idem_key = 'k', amount = 11 \
         WHERE payment_id = 'm2'",
        "UPDATE ob_top_up SET state = 'credited', mismatch_field = NULL, idem_key = 'k' WHERE payment_id = 'm2'",
    ] {
        let error = sqlx::query(sql).execute(pool).await.unwrap_err();
        assert_eq!(sqlstate(&error), "OB901", "{sql}");
    }
    sqlx::query(
        "UPDATE ob_top_up SET state = 'pending', mismatch_field = NULL, idem_key = 'k' WHERE payment_id = 'm2'",
    )
    .execute(pool)
    .await
    .unwrap();
}

/// A webhook holding the row (its credit not yet committed) while the move to `expired` waits: the move re-reads the
/// row once the lock is released, finds it credited, answers `false` and never reaches the guard (no `OB901`).
#[tokio::test]
async fn a_webhook_holding_the_row_while_a_top_up_is_expired_ends_credited() {
    use openbilling_core::{Amount, CreditId, IdempotencyKey, PaymentTarget, TopUp, TopUpLedger};
    use openbilling_testkit::fixtures::{eur, label, owner};
    let Some(store) = support::fresh_store().await else {
        return;
    };
    let ledger = PgLedger::new(&store, LedgerSettings::default());
    setup(&ledger).await;
    let pool = store.pool();
    sqlx::query("SELECT ob_open_account('a1', 'o1', 'EUR', 'main')")
        .execute(pool)
        .await
        .unwrap();
    ledger
        .record_top_up(&TopUp {
            payment: CreditId::new("last").unwrap(),
            key: IdempotencyKey::new("k-last").unwrap(),
            to: PaymentTarget {
                owner: owner("o1"),
                label: label("main"),
            },
            currency: eur(),
            amount: Amount::from_micros(9).unwrap(),
        })
        .await
        .unwrap();
    let settings = openbilling_core::ReconcileSettings {
        stop_after: std::time::Duration::ZERO,
        ..due(10)
    };
    let mut holder = pool.begin().await.unwrap();
    sqlx::query(
        "SELECT o_outcome FROM ob_settle_top_up('last', 'succeeded', 'o1', 'EUR', 'main', 9, 'refuse')",
    )
    .execute(&mut *holder)
    .await
    .unwrap();
    let expire = tokio::spawn({
        let ledger = ledger.clone();
        async move {
            ledger
                .expire_top_up(&CreditId::new("last").unwrap(), &settings)
                .await
        }
    });
    let mut waiting = 0_i64;
    for _ in 0..200 {
        (waiting,) = sqlx::query_as(
            "SELECT count(*) FROM pg_stat_activity \
             WHERE wait_event_type = 'Lock' AND query LIKE '%ob_expire_top_up%'",
        )
        .fetch_one(pool)
        .await
        .unwrap();
        if waiting == 1 {
            break;
        }
        tokio::time::sleep(std::time::Duration::from_millis(50)).await;
    }
    assert_eq!(waiting, 1, "the move waits for the held row");
    holder.commit().await.unwrap();
    assert!(!expire.await.unwrap().unwrap(), "settled meanwhile");
    let (state,): (String,) =
        sqlx::query_as("SELECT state FROM ob_top_up WHERE payment_id = 'last'")
            .fetch_one(pool)
            .await
            .unwrap();
    assert_eq!(state, "credited");
}
