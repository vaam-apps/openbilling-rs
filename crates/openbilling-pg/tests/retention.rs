//! The retention conformance suite of `openbilling-testkit` against the PostgreSQL store, and what only this store
//! can show: the statements the job runs, a run cut short, the lock, and the SQL's own refusals. A fresh database per
//! test.
//!
//! Needs `OPENBILLING_TEST_DATABASE_URL`; skips without it, and fails instead under `OPENBILLING_TEST_REQUIRE_DB=1`.

#![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]

mod support;

use openbilling_core::{PartitionRetention, RetentionMargin, UtcDay};
use openbilling_pg::{LedgerSettings, PgLedger};
use openbilling_testkit::fixtures::setup;
use openbilling_testkit::retention::{Planted, RetentionFixture};
use sqlx::PgPool;

/// A ledger and its pool, which plants old records in the queue the way an intake long ago would have left them.
#[derive(Clone)]
struct PgFixture {
    ledger: PgLedger,
    pool: PgPool,
}

impl RetentionFixture for PgFixture {
    type Store = PgLedger;

    fn store(&self) -> PgLedger {
        self.ledger.clone()
    }

    fn window_days(&self) -> u16 {
        self.ledger.settings().window.days()
    }

    async fn plant(&self, id: &str, days_ago: i64, state: Planted) {
        let days = i32::try_from(days_ago).unwrap();
        // Only that day's partition: the days between are empty, and an empty expired partition is dropped too.
        let text = day_text(self, days_ago).await;
        let (from, to): (String, String) = sqlx::query_as(
            "SELECT to_char(ob_utc_day(now()) - $1::int, 'YYYY-MM-DD'), to_char(ob_utc_day(now()) - $1::int + 1, 'YYYY-MM-DD')",
        )
        .bind(days)
        .fetch_one(&self.pool)
        .await
        .unwrap();
        sqlx::raw_sql(sqlx::AssertSqlSafe(format!(
            "CREATE TABLE IF NOT EXISTS ob_usage_event_{text} PARTITION OF ob_usage_event \
             FOR VALUES FROM ('{from}') TO ('{to}')"
        )))
        .execute(&self.pool)
        .await
        .unwrap();
        let (event_state, applied) = match state {
            Planted::Applied => ("applied", true),
            Planted::Undecided => ("received", false),
        };
        let at =
            "((ob_utc_day(now()) - $2::int)::timestamp + interval '12 hours') AT TIME ZONE 'UTC'";
        sqlx::query(sqlx::AssertSqlSafe(format!(
            "INSERT INTO ob_usage_event (event_id, occurred_on, occurred_at, owner, currency, member, amount, \
             payload_hash, state) \
             VALUES ($1, ob_utc_day(now()) - $2::int, {at}, 'o1', 'EUR', 'm1', 10, decode(repeat('00', 32), 'hex'), $3)"
        )))
        .bind(id)
        .bind(days)
        .bind(event_state)
        .execute(&self.pool)
        .await
        .unwrap();
        if applied {
            sqlx::query(sqlx::AssertSqlSafe(format!(
                "INSERT INTO ob_event_outcome (event_id, occurred_on, occurred_at, owner, currency, member, amount, state) \
                 VALUES ($1, ob_utc_day(now()) - $2::int, {at}, 'o1', 'EUR', 'm1', 10, 'applied')"
            )))
            .bind(id)
            .bind(days)
            .execute(&self.pool)
            .await
            .unwrap();
        }
    }

    async fn partition_days(&self) -> Vec<UtcDay> {
        let names: Vec<String> = sqlx::query_scalar(
            "SELECT c.relname::text FROM pg_inherits i JOIN pg_class c ON c.oid = i.inhrelid \
             WHERE i.inhparent = 'ob_usage_event'::regclass ORDER BY 1",
        )
        .fetch_all(&self.pool)
        .await
        .unwrap();
        names
            .iter()
            .map(|name| {
                let digits = name.strip_prefix("ob_usage_event_").unwrap();
                UtcDay::from_ymd(
                    digits[0..4].parse().unwrap(),
                    digits[4..6].parse().unwrap(),
                    digits[6..8].parse().unwrap(),
                )
                .unwrap()
            })
            .collect()
    }
}

async fn fixture() -> Option<PgFixture> {
    let store = support::fresh_store().await?;
    let ledger = PgLedger::new(&store, LedgerSettings::default());
    ledger.prepare_partitions().await.expect("partitions");
    Some(PgFixture {
        ledger,
        pool: store.pool().clone(),
    })
}

openbilling_testkit::retention_conformance!(fixture);

fn margin(days: u16) -> RetentionMargin {
    RetentionMargin::new(days).unwrap()
}

async fn names(pool: &PgPool) -> Vec<String> {
    sqlx::query_scalar(
        "SELECT relname::text FROM pg_class WHERE relname ~ '^ob_usage_event_[0-9]{8}$' ORDER BY 1",
    )
    .fetch_all(pool)
    .await
    .unwrap()
}

fn sqlstate(error: &sqlx::Error) -> String {
    match error {
        sqlx::Error::Database(database) => {
            database.code().map(|c| c.into_owned()).unwrap_or_default()
        }
        other => panic!("not a database error: {other:?}"),
    }
}

/// The default margin is ninety days, and the settings' margin is what `run_retention` uses.
#[tokio::test]
async fn the_settings_margin_decides_what_run_retention_drops() {
    let Some(store) = support::fresh_store().await else {
        return;
    };
    let by_default = PgLedger::new(&store, LedgerSettings::default());
    by_default.prepare_partitions().await.unwrap();
    let thirty = PgLedger::new(
        &store,
        LedgerSettings {
            retention_margin: margin(30),
            ..LedgerSettings::default()
        },
    );
    assert_eq!(LedgerSettings::default().retention_margin.days(), 90);
    let fix = PgFixture {
        ledger: by_default.clone(),
        pool: store.pool().clone(),
    };
    setup(&by_default).await;
    let window = i64::from(fix.window_days());
    fix.plant("a", window + 40, Planted::Applied).await;
    fix.plant("b", window + 100, Planted::Applied).await;
    let (a, b) = (
        day_text(&fix, window + 40).await,
        day_text(&fix, window + 100).await,
    );
    // The default, 90 days: only the older one.
    let report = by_default.run_retention().await.unwrap();
    assert_eq!(report.dropped.len(), 1);
    assert!(
        names(&fix.pool)
            .await
            .contains(&format!("ob_usage_event_{a}"))
    );
    assert!(
        !names(&fix.pool)
            .await
            .contains(&format!("ob_usage_event_{b}"))
    );
    // 30 days: the other.
    let report = thirty.run_retention().await.unwrap();
    assert_eq!(report.dropped.len(), 1);
    assert!(
        !names(&fix.pool)
            .await
            .contains(&format!("ob_usage_event_{a}"))
    );
}

/// A run cut short after the detach leaves a table with no parent; the next run finds it and drops it. A partition
/// left pending-detach is finished the same way.
#[tokio::test]
async fn a_run_cut_short_is_finished_by_the_next() {
    let Some(fix) = fixture().await else {
        return;
    };
    setup(&fix.ledger).await;
    let window = i64::from(fix.window_days());
    fix.plant("a", window + 10, Planted::Applied).await;
    fix.plant("b", window + 11, Planted::Applied).await;
    let (a, b) = (
        format!("ob_usage_event_{}", day_text(&fix, window + 10).await),
        format!("ob_usage_event_{}", day_text(&fix, window + 11).await),
    );
    // Cut short after the detach: a detached table, not yet dropped.
    sqlx::raw_sql(sqlx::AssertSqlSafe(format!(
        "ALTER TABLE ob_usage_event DETACH PARTITION {a} CONCURRENTLY"
    )))
    .execute(&fix.pool)
    .await
    .unwrap();
    assert!(names(&fix.pool).await.contains(&a));
    // Cut short in the detach: `b` is left marked pending-detach (a detach waiting on a transaction that holds the
    // table is cancelled by its own timeout).
    let mut holder = fix.pool.begin().await.unwrap();
    sqlx::query(sqlx::AssertSqlSafe(format!(
        "LOCK TABLE {b} IN ACCESS SHARE MODE"
    )))
    .execute(&mut *holder)
    .await
    .unwrap();
    let mut detacher = fix.pool.acquire().await.unwrap();
    sqlx::raw_sql("SET statement_timeout = '300ms'")
        .execute(&mut *detacher)
        .await
        .unwrap();
    let cut = sqlx::raw_sql(sqlx::AssertSqlSafe(format!(
        "ALTER TABLE ob_usage_event DETACH PARTITION {b} CONCURRENTLY"
    )))
    .execute(&mut *detacher)
    .await;
    assert!(cut.is_err(), "the detach was cancelled");
    holder.rollback().await.unwrap();
    drop(detacher);
    let pending: bool = sqlx::query_scalar(
        "SELECT inhdetachpending FROM pg_inherits WHERE inhrelid = $1::regclass",
    )
    .bind(&b)
    .fetch_one(&fix.pool)
    .await
    .unwrap();
    assert!(pending, "left pending-detach");

    let report = fix.ledger.drop_expired_partitions(margin(2)).await.unwrap();
    assert_eq!(report.dropped.len(), 2, "{report:?}");
    let left = names(&fix.pool).await;
    assert!(!left.contains(&a) && !left.contains(&b));
}

async fn day_text(fix: &PgFixture, days_ago: i64) -> String {
    sqlx::query_scalar("SELECT to_char(ob_utc_day(now()) - $1::int, 'YYYYMMDD')")
        .bind(i32::try_from(days_ago).unwrap())
        .fetch_one(&fix.pool)
        .await
        .unwrap()
}

/// A run that finds the lock held does nothing and says so; the lock is the session's and is released after a run.
#[tokio::test]
async fn a_run_with_the_lock_held_elsewhere_does_nothing() {
    let Some(fix) = fixture().await else {
        return;
    };
    setup(&fix.ledger).await;
    let window = i64::from(fix.window_days());
    fix.plant("a", window + 10, Planted::Applied).await;
    let mut other = fix.pool.acquire().await.unwrap();
    let got: bool = sqlx::query_scalar("SELECT ob_retention_try_lock()")
        .fetch_one(&mut *other)
        .await
        .unwrap();
    assert!(got);
    let before = names(&fix.pool).await;
    let report = fix.ledger.drop_expired_partitions(margin(2)).await.unwrap();
    assert!(report.skipped && report.dropped.is_empty());
    assert_eq!(names(&fix.pool).await, before);
    let released: bool = sqlx::query_scalar("SELECT ob_retention_unlock()")
        .fetch_one(&mut *other)
        .await
        .unwrap();
    assert!(released);
    let report = fix.ledger.drop_expired_partitions(margin(2)).await.unwrap();
    assert_eq!(report.dropped.len(), 1);
    // The run released its lock: this session takes it at once.
    let again: bool = sqlx::query_scalar("SELECT ob_retention_try_lock()")
        .fetch_one(&mut *other)
        .await
        .unwrap();
    assert!(again);
}

/// A decided-looking guard of its own: an outcome still pending for a day keeps the day's partition. A pending
/// outcome cannot be committed (a deferred trigger refuses it), so the test switches that trigger off to make one.
#[tokio::test]
async fn a_pending_outcome_keeps_its_partition() {
    let Some(fix) = fixture().await else {
        return;
    };
    setup(&fix.ledger).await;
    let window = i64::from(fix.window_days());
    fix.plant("applied", window + 10, Planted::Applied).await;
    sqlx::raw_sql("ALTER TABLE ob_event_outcome DISABLE TRIGGER ob_event_outcome_decided")
        .execute(&fix.pool)
        .await
        .unwrap();
    sqlx::query(
        "INSERT INTO ob_event_outcome (event_id, occurred_on, occurred_at, owner, currency, member, amount, state) \
         SELECT 'p', ob_utc_day(now()) - $1::int, now() - ($1::int * interval '1 day'), 'o1', 'EUR', 'm1', 1, 'pending'",
    )
    .bind(i32::try_from(window + 10).unwrap())
    .execute(&fix.pool)
    .await
    .unwrap();
    let report = fix.ledger.drop_expired_partitions(margin(2)).await.unwrap();
    assert!(report.dropped.is_empty(), "{report:?}");
}

/// What the SQL refuses when it is called directly: a bad window or margin, a name that is not a partition's, a day
/// inside the margin, an attached partition, and a table that holds an undecided event. Nothing is dropped.
#[tokio::test]
async fn the_drop_function_refuses_what_is_not_droppable() {
    let Some(fix) = fixture().await else {
        return;
    };
    setup(&fix.ledger).await;
    let window = i64::from(fix.window_days());
    fix.plant("applied", window + 10, Planted::Applied).await;
    fix.plant("received", window + 11, Planted::Undecided).await;
    let (applied, received) = (
        format!("ob_usage_event_{}", day_text(&fix, window + 10).await),
        format!("ob_usage_event_{}", day_text(&fix, window + 11).await),
    );
    let today = format!("ob_usage_event_{}", day_text(&fix, 0).await);
    let before = names(&fix.pool).await;
    let w = i32::from(fix.window_days());
    for (name, expect) in [
        ("pg_class", "OB904"),
        ("ob_usage_event_20260230", "OB904"),
        ("ob_usage_event_20990101", "OB904"),
        (today.as_str(), "OB904"),
        (applied.as_str(), "OB904"), // still attached
        (received.as_str(), "OB904"),
    ] {
        let error = sqlx::query_scalar::<_, i32>("SELECT ob_day(ob_drop_partition($1, $2, 2))")
            .bind(name)
            .bind(w)
            .fetch_one(&fix.pool)
            .await
            .unwrap_err();
        assert_eq!(sqlstate(&error), expect, "{name}");
    }
    for (window, margin) in [(0, 2), (7, -1)] {
        let error = sqlx::query("SELECT * FROM ob_expired_partitions($1, $2)")
            .bind(window)
            .bind(margin)
            .fetch_all(&fix.pool)
            .await
            .unwrap_err();
        assert_eq!(sqlstate(&error), "22023");
    }
    assert_eq!(names(&fix.pool).await, before, "nothing was dropped");
    // The detached table of an undecided event is refused too, and a table that is not ours is never listed.
    sqlx::raw_sql(sqlx::AssertSqlSafe(format!(
        "ALTER TABLE ob_usage_event DETACH PARTITION {received} CONCURRENTLY"
    )))
    .execute(&fix.pool)
    .await
    .unwrap();
    let error = sqlx::query_scalar::<_, i32>("SELECT ob_day(ob_drop_partition($1, $2, 2))")
        .bind(&received)
        .bind(w)
        .fetch_one(&fix.pool)
        .await
        .unwrap_err();
    assert_eq!(sqlstate(&error), "OB904");
    sqlx::raw_sql("CREATE TABLE ob_usage_event_20200101 (x int)")
        .execute(&fix.pool)
        .await
        .unwrap();
    let report = fix.ledger.drop_expired_partitions(margin(2)).await.unwrap();
    assert_eq!(report.dropped.len(), 1, "only the applied one: {report:?}");
    assert!(
        names(&fix.pool)
            .await
            .contains(&"ob_usage_event_20200101".to_owned())
    );
}

/// The cutoff is the UTC day `window + margin` days ago whatever the session's TimeZone, around a DST change
/// (an interval of days on a timestamptz follows the zone; date arithmetic does not).
#[tokio::test]
async fn the_cutoff_does_not_move_with_the_session_time_zone() {
    let Some(fix) = fixture().await else {
        return;
    };
    let mut conn = fix.pool.acquire().await.unwrap();
    let plain: i32 = sqlx::query_scalar("SELECT ob_day(ob_retention_cutoff(7, 90))")
        .fetch_one(&mut *conn)
        .await
        .unwrap();
    let today: i32 = sqlx::query_scalar("SELECT ob_day(ob_utc_day(now()))")
        .fetch_one(&mut *conn)
        .await
        .unwrap();
    assert_eq!(plain, today - 97);
    for zone in [
        "UTC",
        "America/Los_Angeles",
        "Pacific/Auckland",
        "Pacific/Kiritimati",
        "Europe/Berlin",
    ] {
        sqlx::raw_sql(sqlx::AssertSqlSafe(format!("SET TimeZone = '{zone}'")))
            .execute(&mut *conn)
            .await
            .unwrap();
        let cutoff: i32 = sqlx::query_scalar("SELECT ob_day(ob_retention_cutoff(7, 90))")
            .fetch_one(&mut *conn)
            .await
            .unwrap();
        assert_eq!(cutoff, plain, "{zone}");
        // A window and a margin that span a DST change in each zone: the day count stays exact.
        let spans: i32 = sqlx::query_scalar("SELECT ob_day(ob_retention_cutoff(200, 200))")
            .fetch_one(&mut *conn)
            .await
            .unwrap();
        assert_eq!(spans, today - 400, "{zone}");
    }
}

/// A pending-detach partition that is not the oldest is finished first, so the others can be detached; and one
/// partition failing (here a detach cancelled by the lock timeout) does not starve the rest.
#[tokio::test]
async fn a_pending_partition_that_is_not_the_oldest_is_finalized_first() {
    let Some(fix) = fixture().await else {
        return;
    };
    setup(&fix.ledger).await;
    let window = i64::from(fix.window_days());
    for (id, ago) in [("a", 20), ("b", 21), ("c", 22)] {
        fix.plant(id, window + ago, Planted::Applied).await;
    }
    // `b` (not the oldest, which is `c`) is left pending-detach by a cancelled detach.
    let b = format!("ob_usage_event_{}", day_text(&fix, window + 21).await);
    let mut holder = fix.pool.begin().await.unwrap();
    sqlx::query(sqlx::AssertSqlSafe(format!(
        "LOCK TABLE {b} IN ACCESS SHARE MODE"
    )))
    .execute(&mut *holder)
    .await
    .unwrap();
    let mut detacher = fix.pool.acquire().await.unwrap();
    sqlx::raw_sql("SET statement_timeout = '300ms'")
        .execute(&mut *detacher)
        .await
        .unwrap();
    assert!(
        sqlx::raw_sql(sqlx::AssertSqlSafe(format!(
            "ALTER TABLE ob_usage_event DETACH PARTITION {b} CONCURRENTLY"
        )))
        .execute(&mut *detacher)
        .await
        .is_err()
    );
    holder.rollback().await.unwrap();
    drop(detacher);
    // Oldest-first alone would try `c` and fail with "already pending detach": the plan puts `finalize` first.
    let plan: Vec<(String, String)> =
        sqlx::query_as("SELECT partition_name, action FROM ob_expired_partitions(7, 2)")
            .fetch_all(&fix.pool)
            .await
            .unwrap();
    assert_eq!(plan[0], (b.clone(), "finalize".to_owned()), "{plan:?}");
    let report = fix.ledger.drop_expired_partitions(margin(2)).await.unwrap();
    assert_eq!(report.dropped.len(), 3, "{report:?}");
}

/// One partition failing does not starve the others, and what was dropped before is reported by the counter.
#[tokio::test]
async fn a_failing_partition_does_not_stop_the_rest() {
    let Some(store) = support::fresh_store().await else {
        return;
    };
    let ledger = PgLedger::new(
        &store,
        LedgerSettings {
            retention_timeout: std::time::Duration::from_millis(400),
            ..LedgerSettings::default()
        },
    );
    ledger.prepare_partitions().await.unwrap();
    let fix = PgFixture {
        ledger: ledger.clone(),
        pool: store.pool().clone(),
    };
    setup(&ledger).await;
    let window = i64::from(fix.window_days());
    // `held` is the oldest, so its detach is first and hits the timeout; `skipped` would need a detach PostgreSQL
    // refuses while `held` is pending, so it waits; `orphan` is already detached and is dropped after the failure.
    fix.plant("held", window + 22, Planted::Applied).await;
    fix.plant("skipped", window + 21, Planted::Applied).await;
    fix.plant("orphan", window + 20, Planted::Applied).await;
    let held = format!("ob_usage_event_{}", day_text(&fix, window + 22).await);
    let skipped = format!("ob_usage_event_{}", day_text(&fix, window + 21).await);
    let orphan = format!("ob_usage_event_{}", day_text(&fix, window + 20).await);
    sqlx::raw_sql(sqlx::AssertSqlSafe(format!(
        "ALTER TABLE ob_usage_event DETACH PARTITION {orphan} CONCURRENTLY"
    )))
    .execute(&fix.pool)
    .await
    .unwrap();
    let mut holder = fix.pool.begin().await.unwrap();
    sqlx::query(sqlx::AssertSqlSafe(format!(
        "LOCK TABLE {held} IN ACCESS SHARE MODE"
    )))
    .execute(&mut *holder)
    .await
    .unwrap();
    let error = ledger.drop_expired_partitions(margin(2)).await.unwrap_err();
    assert_eq!(
        openbilling_core::Classify::class(&error),
        openbilling_core::ErrorClass::Unavailable
    );
    let left = names(&fix.pool).await;
    assert!(!left.contains(&orphan), "dropped after the failure");
    assert!(left.contains(&held) && left.contains(&skipped));
    holder.rollback().await.unwrap();
    // The next run finishes `held` (pending) first, then detaches and drops `skipped`.
    let report = ledger.drop_expired_partitions(margin(2)).await.unwrap();
    assert_eq!(report.dropped.len(), 2, "{report:?}");
    let left = names(&fix.pool).await;
    assert!(!left.contains(&held) && !left.contains(&skipped));
}

/// `ob_drop_partition` refuses a table that is not an ordinary table of our shape, with `OB904`.
#[tokio::test]
async fn the_drop_function_refuses_a_foreign_table_and_a_wrong_shape() {
    let Some(fix) = fixture().await else {
        return;
    };
    for ddl in [
        "CREATE TABLE ob_usage_event_20200101 (x int)",
        "CREATE VIEW ob_usage_event_20200102 AS SELECT 1 AS x",
        "CREATE FOREIGN DATA WRAPPER ob_w",
    ] {
        let _ = sqlx::raw_sql(ddl).execute(&fix.pool).await;
    }
    sqlx::raw_sql("CREATE SERVER ob_s FOREIGN DATA WRAPPER ob_w; CREATE FOREIGN TABLE ob_usage_event_20200103 (x int) SERVER ob_s")
        .execute(&fix.pool)
        .await
        .unwrap();
    for name in [
        "ob_usage_event_20200101",
        "ob_usage_event_20200102",
        "ob_usage_event_20200103",
    ] {
        let error = sqlx::query_scalar::<_, i32>("SELECT ob_day(ob_drop_partition($1, 7, 2))")
            .bind(name)
            .fetch_one(&fix.pool)
            .await
            .unwrap_err();
        assert_eq!(sqlstate(&error), "OB904", "{name}");
    }
}
