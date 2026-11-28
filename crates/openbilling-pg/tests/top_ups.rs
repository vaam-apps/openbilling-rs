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
