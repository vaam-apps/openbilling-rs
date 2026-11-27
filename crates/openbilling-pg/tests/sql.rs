//! What only the PostgreSQL store can show: the migrations, the SQL's own invariants, the queue's locks, partitions,
//! the wake-up and the error classes. Each test runs on a fresh database.
//!
//! Needs `OPENBILLING_TEST_DATABASE_URL`; skips without it, and fails instead under `OPENBILLING_TEST_REQUIRE_DB=1`.

#![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]

mod support;

use std::time::Duration;

use openbilling_core::PriceBook;
use openbilling_core::{
    Classify, CreditSource, ErrorClass, EventOutcome, LedgerError, LedgerStore, ReversalKind, Rule,
};
use openbilling_pg::{LedgerSettings, PgLedger, Woken, migrator};
use openbilling_testkit::fixtures::{
    MINUTE, account, credit_id, credit_to, drain_all, eur, event, micros, now, open, owner, pay,
    plus, setup, take,
};
use openbilling_testkit::price_cases::{example_list, raw};
use sqlx::{AssertSqlSafe, PgPool};

async fn ledger_and_pool() -> Option<(PgLedger, PgPool)> {
    let store = support::fresh_store().await?;
    let ledger = PgLedger::new(&store, LedgerSettings::default());
    ledger.prepare_partitions().await.expect("partitions");
    Some((ledger, store.pool().clone()))
}

fn sqlstate(error: &sqlx::Error) -> String {
    match error {
        sqlx::Error::Database(database) => {
            database.code().map(|c| c.into_owned()).unwrap_or_default()
        }
        other => panic!("not a database error: {other:?}"),
    }
}

async fn scalar(pool: &PgPool, sql: &'static str) -> i64 {
    sqlx::query_scalar(sql).fetch_one(pool).await.expect(sql)
}

/// The migrations apply once. Every function the migrations define answers, and none is left untested.
#[tokio::test]
async fn migrations_apply_once_and_every_function_answers() {
    let Some((ledger, pool)) = ledger_and_pool().await else {
        return;
    };
    // Applied by `fresh_store`; a second run applies nothing.
    migrator().run(&pool).await.expect("a second run");
    assert_eq!(
        scalar(&pool, "SELECT count(*) FROM _sqlx_migrations").await,
        7
    );
    // The flag of owners past their debt limit is a view, read as a plain query (ADR 0001, amended, E3).
    assert_eq!(
        scalar(&pool, "SELECT count(*) FROM ob_past_debt_limit").await,
        0
    );

    let functions: Vec<String> = sqlx::query_scalar(
        "SELECT p.proname::text FROM pg_proc p JOIN pg_namespace n ON n.oid = p.pronamespace \
         WHERE n.nspname = current_schema() AND p.proname LIKE 'ob\\_%'",
    )
    .fetch_all(&pool)
    .await
    .unwrap();
    let mut functions = functions;
    functions.sort_unstable();
    let mut expected = [
        "ob_add_debt",
        "ob_allowed",
        "ob_balance_of",
        "ob_check_outcome_decided",
        "ob_check_reversal_cap",
        "ob_check_txn_balanced",
        "ob_clamp",
        "ob_configure",
        "ob_credit",
        "ob_daily_usage",
        "ob_day",
        "ob_dimensions",
        "ob_draw",
        "ob_drain",
        "ob_event_outcome_guard",
        "ob_grant_allowance",
        "ob_in_alphabet",
        "ob_intake",
        "ob_intake_rows",
        "ob_is_dimensions",
        "ob_is_id",
        "ob_is_name",
        "ob_load_price_list",
        "ob_lot_only_remaining_moves",
        "ob_lots_of",
        "ob_micros",
        "ob_open_account",
        "ob_payload_canonical",
        "ob_payload_hash",
        "ob_prepare_partitions",
        "ob_price_back_dated",
        "ob_price_child_of_unsealed",
        "ob_price_content_hash",
        "ob_price_of",
        "ob_price_version_hash",
        "ob_price_version_seal",
        "ob_rate",
        "ob_raw_payload_canonical",
        "ob_raw_payload_hash",
        "ob_refuse_change",
        "ob_repay_from_lot",
        "ob_reverse",
        "ob_round_half_even",
        "ob_set_debt_limit",
        "ob_set_member_cap",
        "ob_ts",
        "ob_usage_event_guard",
        "ob_utc_day",
    ];
    expected.sort_unstable();
    assert_eq!(
        functions, expected,
        "a function was added or removed: test it here"
    );

    // Each callable function, once, through the port where it has one.
    setup(&ledger).await;
    open(&ledger, "a1", "o1", eur(), "main").await;
    ledger
        .set_debt_limit(&owner("o1"), &eur(), None)
        .await
        .unwrap();
    ledger
        .set_member_cap(
            &owner("o1"),
            &eur(),
            &openbilling_core::MemberId::new("m1").unwrap(),
            Some(micros(5)),
        )
        .await
        .unwrap();
    let t = now(&ledger).await;
    ledger
        .grant_allowance(&account("a1"), t.utc_month(), micros(10))
        .await
        .unwrap();
    pay(&ledger, "p1", "a1", 100).await;
    ledger
        .reverse(&openbilling_core::Reversal {
            kind: ReversalKind::Refund,
            id: credit_id("r1"),
            payment: credit_id("p1"),
            amount: micros(1),
        })
        .await
        .unwrap();
    take(&ledger, &[event("e1", "o1", "m1", 3, t)]).await;
    assert_eq!(drain_all(&ledger, 10).await.applied(), 1);
    ledger.balance(&owner("o1"), &eur(), t).await.unwrap();
    ledger
        .allowed(
            &owner("o1"),
            &eur(),
            &openbilling_core::MemberId::new("m1").unwrap(),
            t,
        )
        .await
        .unwrap();
    ledger.lots(&account("a1")).await.unwrap();
    ledger
        .outcome(&openbilling_core::EventId::new("e1").unwrap())
        .await
        .unwrap();
    ledger.daily_usage(&owner("o1"), &eur()).await.unwrap();
    // The price book: load (with the hash, the seal and the back-dating check), rate (with `ob_price_of`), raw intake.
    let loaded = ledger
        .load_price_list(&example_list(plus(t, MINUTE), 150))
        .await
        .unwrap();
    let e = raw(
        "r1",
        "tokens.input",
        1_000_000,
        &[("model", "example-small")],
        plus(t, 2 * MINUTE),
    );
    assert_eq!(ledger.rate(&e).await.unwrap().version, loaded.version);
    assert_eq!(ledger.intake_raw(&[e]).await.unwrap().accepted(), 1);
    assert_eq!(ledger.price_versions().await.unwrap().len(), 1);
    // The helpers.
    let round: i64 = sqlx::query_scalar("SELECT ob_micros(ob_ts(1795737600000042))")
        .fetch_one(&pool)
        .await
        .unwrap();
    assert_eq!(round, 1_795_737_600_000_042);
    let clamped: i64 = sqlx::query_scalar("SELECT ob_clamp(99999999999999999999)")
        .fetch_one(&pool)
        .await
        .unwrap();
    assert_eq!(clamped, i64::MAX);
    let day: i32 = sqlx::query_scalar("SELECT ob_day(ob_utc_day(ob_ts(1795737600000042)))")
        .fetch_one(&pool)
        .await
        .unwrap();
    assert_eq!(day, 20_784);
    let ids: Vec<bool> = sqlx::query_scalar(
        "SELECT ob_is_id(v, 128) FROM unnest(ARRAY['a', '!~', 'a b', '', 'caf\u{e9}']) AS v",
    )
    .fetch_all(&pool)
    .await
    .unwrap();
    assert_eq!(ids, [true, true, false, false, false]);
    // Nothing drifted.
    assert_eq!(scalar(&pool, "SELECT count(*) FROM ob_lot_drift").await, 0);
    assert_eq!(scalar(&pool, "SELECT count(*) FROM ob_debt_drift").await, 0);
}

/// The canonical form and its hash, against the vector computed outside PostgreSQL (`sha256sum` of the text). The
/// same text the core's documentation shows.
#[tokio::test]
async fn the_payload_hash_is_the_test_vector() {
    let Some((_, pool)) = ledger_and_pool().await else {
        return;
    };
    let (canonical, hash): (String, String) = sqlx::query_as(
        "SELECT ob_payload_canonical($1, 'EUR', 'm1', 1500, ob_ts(1795737600000042)), \
                encode(ob_payload_hash($1, 'EUR', 'm1', 1500, ob_ts(1795737600000042)), 'hex')",
    )
    .bind("o\"1\\")
    .fetch_one(&pool)
    .await
    .unwrap();
    assert_eq!(
        canonical,
        r#"{"amount":1500,"currency":"EUR","member":"m1","occurred_at":"2026-11-27T00:00:00.000042Z","owner":"o\"1\\"}"#
    );
    assert_eq!(
        hash,
        "052056fafbe20f920207f2fea64e5894f9aa5535fd9dedf807e971c4617eae7a"
    );
}

/// The ledger is append-only; a lot changes only its remainder.
#[tokio::test]
async fn the_ledger_is_append_only() {
    let Some((ledger, pool)) = ledger_and_pool().await else {
        return;
    };
    setup(&ledger).await;
    open(&ledger, "a1", "o1", eur(), "main").await;
    pay(&ledger, "p1", "a1", 100).await;
    for statement in [
        "UPDATE ob_entry SET amount = amount",
        "DELETE FROM ob_entry",
        "TRUNCATE ob_entry",
        "UPDATE ob_txn SET kind = kind",
        "DELETE FROM ob_credit",
        "UPDATE ob_credit SET amount = 1",
        "DELETE FROM ob_lot",
        "UPDATE ob_lot SET amount = amount + 1",
        "TRUNCATE ob_event_outcome CASCADE",
    ] {
        let error = sqlx::query(AssertSqlSafe(statement))
            .execute(&pool)
            .await
            .expect_err(statement);
        assert_eq!(sqlstate(&error), "OB901", "{statement}");
    }
}

/// A ledger transaction that does not balance cannot commit, and a fault of the SQL is `internal`.
#[tokio::test]
async fn an_unbalanced_transaction_cannot_commit() {
    let Some((ledger, pool)) = ledger_and_pool().await else {
        return;
    };
    setup(&ledger).await;
    let mut tx = pool.begin().await.unwrap();
    sqlx::query(
        "WITH t AS (INSERT INTO ob_txn (kind, currency) VALUES ('credit', 'EUR') RETURNING txn_id) \
         INSERT INTO ob_entry (txn_id, currency, book, amount) SELECT t.txn_id, 'EUR', 'revenue', 5 FROM t",
    )
    .execute(&mut *tx)
    .await
    .unwrap();
    let error = tx.commit().await.expect_err("does not balance");
    assert_eq!(sqlstate(&error), "OB900");
    let fault = ledger.drain(1).await;
    assert!(fault.is_ok(), "the failed transaction left nothing behind");
    assert_eq!(
        openbilling_pg::sqlstate::meaning_of("OB900"),
        Some(openbilling_pg::sqlstate::Meaning::Fault(
            ErrorClass::Internal
        ))
    );
}

/// The reversal cap is a fact of the database, whoever writes the row.
#[tokio::test]
async fn the_reversal_cap_holds_whoever_writes() {
    let Some((ledger, pool)) = ledger_and_pool().await else {
        return;
    };
    setup(&ledger).await;
    open(&ledger, "a1", "o1", eur(), "main").await;
    pay(&ledger, "p1", "a1", 100).await;
    let error = sqlx::query(
        "INSERT INTO ob_reversal (kind, reversal_id, payment_id, amount, txn_id) \
         VALUES ('refund', 'r-direct', 'p1', 101, uuidv7())",
    )
    .execute(&pool)
    .await
    .expect_err("past the payment");
    assert_eq!(sqlstate(&error), "OB004");
}

/// A drain that fails at its last
/// write leaves every event received and every lot whole; the next drain applies each event once.
#[tokio::test]
async fn a_drain_that_dies_books_nothing_and_the_next_applies_once() {
    let Some((ledger, pool)) = ledger_and_pool().await else {
        return;
    };
    setup(&ledger).await;
    open(&ledger, "a1", "o1", eur(), "main").await;
    pay(&ledger, "p1", "a1", 5_000).await;
    let t = now(&ledger).await;
    take(
        &ledger,
        &[
            event("e1", "o1", "m1", 1_000, t),
            event("e2", "o1", "m1", 500, t),
        ],
    )
    .await;
    sqlx::query(
        "CREATE FUNCTION fail_once() RETURNS trigger LANGUAGE plpgsql AS $$ \
         BEGIN RAISE EXCEPTION 'the worker died'; END $$",
    )
    .execute(&pool)
    .await
    .unwrap();
    // The last write of a drain moves the event rows to their final state.
    sqlx::query("CREATE TRIGGER fail_once BEFORE UPDATE ON ob_usage_event FOR EACH ROW EXECUTE FUNCTION fail_once()")
        .execute(&pool)
        .await
        .unwrap();
    let error = ledger.drain(10).await.expect_err("the worker died");
    assert_eq!(error.class(), ErrorClass::Internal);
    assert_eq!(
        scalar(&pool, "SELECT count(*) FROM ob_event_outcome").await,
        0
    );
    assert_eq!(
        scalar(&pool, "SELECT count(*) FROM ob_txn WHERE kind = 'usage'").await,
        0
    );
    assert_eq!(
        scalar(&pool, "SELECT sum(remaining)::bigint FROM ob_lot").await,
        5_000
    );

    sqlx::query("DROP TRIGGER fail_once ON ob_usage_event")
        .execute(&pool)
        .await
        .unwrap();
    let report = drain_all(&ledger, 10).await;
    assert_eq!(report.applied(), 2);
    assert!(drain_all(&ledger, 10).await.decided.is_empty());
    assert_eq!(
        scalar(&pool, "SELECT sum(remaining)::bigint FROM ob_lot").await,
        3_500
    );
    assert_eq!(scalar(&pool, "SELECT count(*) FROM ob_lot_drift").await, 0);
}

/// ADR 0001, point 4: intake takes no row lock a drain holds, so a replay is answered while a drain holds its row.
#[tokio::test]
async fn a_replay_is_not_blocked_by_a_drain_holding_the_row() {
    let Some((ledger, pool)) = ledger_and_pool().await else {
        return;
    };
    setup(&ledger).await;
    let t = now(&ledger).await;
    let batch = [event("held", "o1", "m1", 1, t)];
    take(&ledger, &batch).await;
    let mut drain = pool.begin().await.unwrap();
    sqlx::query("SELECT 1 FROM ob_usage_event WHERE event_id = 'held' FOR UPDATE")
        .execute(&mut *drain)
        .await
        .unwrap();
    let replay = tokio::time::timeout(Duration::from_secs(5), ledger.intake(&batch))
        .await
        .expect("the replay does not wait for the lock")
        .expect("a replay");
    assert_eq!(replay.replayed(), 1);
    drain.rollback().await.unwrap();
}

/// ADR 0001, point 10: an event for a day without a partition is unavailable, not refused, and writes nothing.
#[tokio::test]
async fn a_day_without_a_partition_is_unavailable() {
    let Some((ledger, pool)) = ledger_and_pool().await else {
        return;
    };
    setup(&ledger).await;
    let t = now(&ledger).await;
    let name = format!(
        "ob_usage_event_{}",
        t.utc_day().to_string().replace('-', "")
    );
    sqlx::query(AssertSqlSafe(format!("DROP TABLE {name}")))
        .execute(&pool)
        .await
        .unwrap();
    let error = ledger
        .intake(&[event("e1", "o1", "m1", 1, t)])
        .await
        .expect_err("no partition");
    assert_eq!(error.class(), ErrorClass::Unavailable);
    assert!(error.rule().is_none());
    assert_eq!(
        ledger.prepare_partitions().await.unwrap(),
        1,
        "the job puts it back"
    );
    assert_eq!(
        take(&ledger, &[event("e1", "o1", "m1", 1, t)])
            .await
            .accepted(),
        1
    );
}

/// A closed pool is `unavailable`.
#[tokio::test]
async fn a_closed_pool_is_unavailable() {
    let Some((ledger, pool)) = ledger_and_pool().await else {
        return;
    };
    pool.close().await;
    let error = ledger.now().await.expect_err("closed");
    assert_eq!(error.class(), ErrorClass::Unavailable);
    let error = ledger
        .allowed(
            &owner("o1"),
            &eur(),
            &openbilling_core::MemberId::new("m1").unwrap(),
            openbilling_core::Timestamp::EPOCH,
        )
        .await
        .expect_err("closed");
    assert!(matches!(
        error,
        LedgerError::Store {
            class: ErrorClass::Unavailable,
            ..
        }
    ));
}

/// ADR 0001, points 16 and 17: intake wakes a listening worker; with nothing new the wait ends at the poll.
#[tokio::test]
async fn intake_wakes_a_listening_worker() {
    let Some((ledger, _)) = ledger_and_pool().await else {
        return;
    };
    setup(&ledger).await;
    let mut wakeup = ledger.wakeup().await.expect("listen");
    assert_eq!(
        wakeup.wait(Duration::from_millis(200)).await.unwrap(),
        Woken::Polled
    );
    let t = now(&ledger).await;
    take(&ledger, &[event("e1", "o1", "m1", 1, t)]).await;
    assert_eq!(
        wakeup.wait(Duration::from_secs(5)).await.unwrap(),
        Woken::Notified
    );
}

/// The error classes: each `OB` SQLSTATE reaches the caller as its refusal, through the real SQL.
#[tokio::test]
async fn the_sql_refusals_reach_the_caller_with_their_class() {
    let Some((ledger, _)) = ledger_and_pool().await else {
        return;
    };
    setup(&ledger).await;
    open(&ledger, "a1", "o1", eur(), "main").await;
    pay(&ledger, "p1", "a1", 10).await;
    let conflict = ledger
        .credit(&credit_to(CreditSource::Payment, "p1", "a1", 11, None))
        .await
        .expect_err("a conflict");
    assert_eq!(
        (conflict.rule(), conflict.class()),
        (Some(Rule::CreditConflict), ErrorClass::Conflict)
    );
    let invalid = ledger
        .credit(&credit_to(CreditSource::Payment, "p2", "a1", 0, None))
        .await
        .expect_err("invalid");
    assert_eq!(
        (invalid.rule(), invalid.class()),
        (Some(Rule::InvalidAmount), ErrorClass::Invalid)
    );
    let t = now(&ledger).await;
    take(&ledger, &[event("e1", "o1", "m1", 1, t)]).await;
    let other = event("e1", "o1", "m1", 2, t);
    let named = ledger
        .intake(&[other])
        .await
        .expect_err("a payload conflict");
    let refusal = named.refusal().expect("a refusal");
    assert_eq!(refusal.rule(), Rule::PayloadConflict);
    assert_eq!(
        refusal
            .events()
            .iter()
            .map(|e| e.as_str())
            .collect::<Vec<_>>(),
        ["e1"]
    );
    assert_eq!(named.class(), ErrorClass::Conflict);
    let report = drain_all(&ledger, 10).await;
    assert_eq!(report.decided[0].outcome, EventOutcome::Applied);
}

/// The raw payload's canonical form and hash (ADR 0004), against the vector computed outside PostgreSQL (Python's
/// `json.dumps(..., separators=(",", ":"), sort_keys=True)` and `hashlib.sha256`).
#[tokio::test]
async fn the_raw_payload_hash_is_the_test_vector() {
    let Some((_, pool)) = ledger_and_pool().await else {
        return;
    };
    let (canonical, hash): (String, String) = sqlx::query_as(
        "SELECT ob_raw_payload_canonical($1, 'EUR', 'm1', 'tokens.input', 1500, $2::jsonb, ob_ts(1795737600000042)), \
                encode(ob_raw_payload_hash($1, 'EUR', 'm1', 'tokens.input', 1500, $2::jsonb, \
                                           ob_ts(1795737600000042)), 'hex')",
    )
    .bind("o\"1\\")
    .bind(r#"{"tier": "b", "model": "example-small"}"#)
    .fetch_one(&pool)
    .await
    .unwrap();
    assert_eq!(
        canonical,
        r#"{"currency":"EUR","dimensions":{"model":"example-small","tier":"b"},"member":"m1","meter":"tokens.input","occurred_at":"2026-11-27T00:00:00.000042Z","owner":"o\"1\\","quantity":1500}"#
    );
    assert_eq!(
        hash,
        "9dabd16959da9c93d5deb884e95affe1429d9a6175e54627c4ab57d85968cc3d"
    );
}

/// Half to even, at the boundary and with numerators past the range of an `i64`.
#[tokio::test]
async fn the_rounding_is_half_to_even() {
    let Some((_, pool)) = ledger_and_pool().await else {
        return;
    };
    let rounded: Vec<String> = sqlx::query_scalar(
        "SELECT ob_round_half_even(n, d)::text FROM (VALUES (0, 1), (1, 2), (3, 2), (5, 2), (7, 2), (4999, 10000), \
         (5000, 10000), (5001, 10000), (15000, 10000), (85070591730234615847396907784232501249, 1000000), \
         (85070591730234615847396907784232500000, 1000000), (17, 1)) AS t(n, d)",
    )
    .fetch_all(&pool)
    .await
    .unwrap();
    assert_eq!(
        rounded,
        [
            "0",
            "0",
            "2",
            "2",
            "4",
            "0",
            "0",
            "1",
            "2",
            "85070591730234615847396907784233",
            "85070591730234615847396907784232",
            "17"
        ]
    );
}

/// A version cannot be modified, whoever writes: its rows are append-only, nothing is added to a sealed version, a
/// version row must carry its rows' hash, and a version written directly is still never back-dated.
#[tokio::test]
async fn a_version_cannot_be_modified() {
    let Some((ledger, pool)) = ledger_and_pool().await else {
        return;
    };
    setup(&ledger).await;
    let t = now(&ledger).await;
    ledger
        .load_price_list(&example_list(plus(t, MINUTE), 150))
        .await
        .unwrap();
    for statement in [
        "UPDATE ob_price SET price_micros = 1",
        "DELETE FROM ob_price",
        "TRUNCATE ob_price",
        "UPDATE ob_price_meter SET unit = NULL",
        "DELETE FROM ob_price_dimension",
        "UPDATE ob_price_version SET effective_from = effective_from - interval '1 day'",
        "DELETE FROM ob_price_version",
        "TRUNCATE ob_price_version CASCADE",
        "INSERT INTO ob_price_meter (version_id, meter) SELECT version_id, 'sneak' FROM ob_price_version",
    ] {
        let error = sqlx::query(AssertSqlSafe(statement))
            .execute(&pool)
            .await
            .expect_err(statement);
        assert_eq!(sqlstate(&error), "OB901", "{statement}");
    }
    let error = sqlx::query(
        "INSERT INTO ob_price_version (effective_from, content_hash) \
         VALUES (now() + interval '1 day', sha256('not its rows'))",
    )
    .execute(&pool)
    .await
    .expect_err("a hash that is not its rows'");
    assert_eq!(sqlstate(&error), "OB903");
    let error = sqlx::query(
        "INSERT INTO ob_price_version (effective_from, content_hash) \
         VALUES (now() - interval '1 day', ob_price_content_hash(now() - interval '1 day', '[]', '[]'))",
    )
    .execute(&pool)
    .await
    .expect_err("back-dated");
    assert_eq!(sqlstate(&error), "OB205");
    assert_eq!(ledger.price_versions().await.unwrap().len(), 1);
}

/// A version is written while no intake rates: a load waits for an intake that holds the price book's ROW SHARE
/// lock, and goes on when it ends.
#[tokio::test]
async fn a_load_waits_for_an_intake_that_is_rating() {
    let Some((ledger, pool)) = ledger_and_pool().await else {
        return;
    };
    setup(&ledger).await;
    let t = now(&ledger).await;
    let mut rating = pool.begin().await.unwrap();
    sqlx::query("LOCK TABLE ob_price_version IN ROW SHARE MODE")
        .execute(&mut *rating)
        .await
        .unwrap();
    let loader = ledger.clone();
    let list = example_list(plus(t, MINUTE), 150);
    let load = tokio::spawn(async move { loader.load_price_list(&list).await });
    tokio::time::sleep(Duration::from_millis(500)).await;
    assert!(!load.is_finished(), "the load waits for the lock");
    rating.rollback().await.unwrap();
    let loaded = tokio::time::timeout(Duration::from_secs(5), load)
        .await
        .expect("the load goes on")
        .unwrap()
        .unwrap();
    assert!(!loaded.replayed);
}

/// The price book's refusals reach the caller with their rule, class and position, through the real SQL.
#[tokio::test]
async fn the_price_book_refusals_reach_the_caller_with_their_class() {
    let Some((ledger, _)) = ledger_and_pool().await else {
        return;
    };
    setup(&ledger).await;
    let t = now(&ledger).await;
    let list = example_list(plus(t, MINUTE), 150);
    ledger.load_price_list(&list).await.unwrap();
    let conflict = ledger
        .load_price_list(&example_list(plus(t, MINUTE), 151))
        .await
        .expect_err("another list at the same time");
    assert_eq!(
        (conflict.rule(), conflict.class()),
        (Some(Rule::VersionConflict), ErrorClass::Conflict)
    );
    let refusal = conflict.refusal().expect("a refusal");
    let at = refusal.position().expect("a position");
    assert_eq!((at.line(), at.key()), (2, Some("effective_from")));
    assert!(refusal.to_string().contains("line 2"), "{refusal}");
    let unpriced = ledger
        .rate(&raw("x", "images.generated", 1, &[], plus(t, 2 * MINUTE)))
        .await
        .expect_err("unpriced");
    assert_eq!(
        (unpriced.rule(), unpriced.class()),
        (Some(Rule::UnpricedMeter), ErrorClass::Invalid)
    );
}
