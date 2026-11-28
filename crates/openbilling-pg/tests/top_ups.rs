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
