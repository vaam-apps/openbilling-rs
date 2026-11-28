//! The conformance cases of partition retention ([`PartitionRetention`], ADR 0001, point 10, amended 2026-11-28).
//!
//! Intake refuses an event older than the window, so no case can age a record through the port. A store therefore
//! gives the cases a [`RetentionFixture`]: a way to plant a record in the queue as it would have been left by an
//! intake long ago, and to list the days that have a partition. The cases drive everything else through the ports.
//!
//! Times are days before the store's clock. A margin of `m` keeps every day on or after `today - (window + m)` and
//! drops the days before it.

use std::future::Future;

use openbilling_core::{
    EventOutcome, LedgerStore, PartitionRetention, RefusalReason, RetentionMargin, Rule, UtcDay,
};

use crate::fixtures::{
    DAY, drain_all, eur, event, now, open, pay, plus, refusal_of, remaining, setup, take,
};

/// How a planted record was left.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Planted {
    /// Decided and applied, with its outcome recorded.
    Applied,
    /// Received and not yet decided.
    Undecided,
}

/// What a store gives the retention cases beyond its ports: the store itself, and a way to age its queue. Planted
/// records belong to the owner `o1` and the currency EUR, which the case has configured.
pub trait RetentionFixture: Clone + Send + Sync + 'static {
    /// The store under test.
    type Store: LedgerStore + PartitionRetention + Clone + 'static;

    /// The store under test. Clones share one database.
    fn store(&self) -> Self::Store;

    /// The window the store's intake enforces, in days.
    fn window_days(&self) -> u16;

    /// Plants the event `id`, which occurred `days_ago` UTC days before the store's clock, as `state`, making the
    /// day's partition if it is not there. Bypasses intake.
    fn plant(&self, id: &str, days_ago: i64, state: Planted) -> impl Future<Output = ()> + Send;

    /// The days that have a partition now, oldest first.
    fn partition_days(&self) -> impl Future<Output = Vec<UtcDay>> + Send;
}

async fn day_ago<F: RetentionFixture>(fix: &F, days: i64) -> UtcDay {
    let today = now(&fix.store()).await.utc_day().unix_days();
    UtcDay::from_unix_days(today - days).expect("a day")
}

fn margin(days: u16) -> RetentionMargin {
    RetentionMargin::new(days).expect("a margin")
}

/// The decisive case: dropping old partitions changes nothing for an event still inside the window. Replays of
/// events in the partitions that stay are still replays, and moves nothing; a replay of an event too old for the
/// window is still refused; and an id applied in a dropped partition is still refused when it comes back, by its
/// outcome, which is kept for good.
pub async fn an_event_replayed_inside_the_window_is_still_refused_after_a_drop<
    F: RetentionFixture,
>(
    fix: F,
) {
    let store = fix.store();
    setup(&store).await;
    open(&store, "a1", "o1", eur(), "main").await;
    pay(&store, "p1", "a1", 10_000).await;
    let window = i64::from(fix.window_days());
    let t = now(&store).await;
    // One event today and one on the day before the oldest intake still accepts (a day inside, to be safe at midnight).
    let batch = [
        event("today", "o1", "m1", 1_000, t),
        event("edge", "o1", "m1", 2_000, plus(t, -(window - 2) * DAY)),
    ];
    assert_eq!(take(&store, &batch).await.accepted(), 2);
    assert_eq!(drain_all(&store, 100).await.applied(), 2);
    assert_eq!(remaining(&store, "a1").await, 7_000);

    let old = window + 10;
    fix.plant("old", old, Planted::Applied).await;
    let before = fix.partition_days().await;
    assert!(before.contains(&day_ago(&fix, old).await));

    let report = store
        .drop_expired_partitions(margin(2))
        .await
        .expect("a drop");
    assert!(!report.skipped);
    assert_eq!(report.dropped, [day_ago(&fix, old).await]);
    let after = fix.partition_days().await;
    assert!(!after.contains(&day_ago(&fix, old).await));
    for kept in [0, window - 2, window - 1] {
        assert!(after.contains(&day_ago(&fix, kept).await), "{kept}");
    }

    // Inside the window: still replays, still nothing moved.
    let again = take(&store, &batch).await;
    assert_eq!((again.accepted(), again.replayed()), (0, 2));
    assert!(drain_all(&store, 100).await.decided.is_empty());
    assert_eq!(remaining(&store, "a1").await, 7_000);

    // Too old for the window: refused at intake, as before the drop.
    let too_old = event("old", "o1", "m1", 10, plus(t, -old * DAY));
    let (rule, named) = refusal_of(store.intake(&[too_old]).await);
    assert_eq!((rule, named), (Rule::TooOld, vec!["old".to_owned()]));

    // The same id on a day inside the window is taken in, and the drain refuses it by the outcome that was kept.
    assert_eq!(
        take(&store, &[event("old", "o1", "m1", 10, t)])
            .await
            .accepted(),
        1
    );
    let decided = drain_all(&store, 100).await;
    let outcomes: Vec<_> = decided.decided.iter().map(|d| d.outcome).collect();
    assert_eq!(outcomes, [EventOutcome::Refused(RefusalReason::IdReused)]);
    assert_eq!(remaining(&store, "a1").await, 7_000);

    // A second run has nothing left to do.
    let second = store
        .drop_expired_partitions(margin(2))
        .await
        .expect("a second drop");
    assert!(second.dropped.is_empty());
}

/// A partition that still holds an undecided event is kept, however old; once the event is decided it goes.
pub async fn a_partition_with_an_undecided_event_is_kept<F: RetentionFixture>(fix: F) {
    let store = fix.store();
    setup(&store).await;
    let window = i64::from(fix.window_days());
    fix.plant("pending", window + 10, Planted::Undecided).await;
    fix.plant("done", window + 11, Planted::Applied).await;
    let (pending_day, done_day) = (
        day_ago(&fix, window + 10).await,
        day_ago(&fix, window + 11).await,
    );

    let report = store
        .drop_expired_partitions(margin(2))
        .await
        .expect("a drop");
    assert_eq!(report.dropped, [done_day]);
    let days = fix.partition_days().await;
    assert!(days.contains(&pending_day) && !days.contains(&done_day));

    // The drain decides it (no account: refused), and the next run drops its partition.
    assert_eq!(drain_all(&store, 100).await.decided.len(), 1);
    let report = store
        .drop_expired_partitions(margin(2))
        .await
        .expect("a drop");
    assert_eq!(report.dropped, [pending_day]);
    assert!(!fix.partition_days().await.contains(&pending_day));
}

/// A day on the cutoff, and every day inside the window plus the margin, is kept; the day before the cutoff goes.
pub async fn a_partition_inside_the_window_plus_the_margin_is_kept<F: RetentionFixture>(fix: F) {
    let store = fix.store();
    setup(&store).await;
    let window = i64::from(fix.window_days());
    let m = 3;
    for (id, ago) in [
        ("inside", window + m - 2),
        ("edge", window + m),
        ("gone", window + m + 1),
    ] {
        fix.plant(id, ago, Planted::Applied).await;
    }
    let report = store
        .drop_expired_partitions(margin(3))
        .await
        .expect("a drop");
    assert_eq!(report.dropped, [day_ago(&fix, window + m + 1).await]);
    let days = fix.partition_days().await;
    for kept in [window + m - 2, window + m] {
        assert!(days.contains(&day_ago(&fix, kept).await), "{kept}");
    }

    // A longer margin keeps what a shorter one would drop.
    let report = store
        .drop_expired_partitions(margin(30))
        .await
        .expect("a drop");
    assert!(report.dropped.is_empty());
    assert!(
        fix.partition_days()
            .await
            .contains(&day_ago(&fix, window + m).await)
    );
}

/// The current day and every day of the window are never touched, even with the shortest margin.
pub async fn the_window_and_the_current_partition_are_never_touched<F: RetentionFixture>(fix: F) {
    let store = fix.store();
    setup(&store).await;
    let before = fix.partition_days().await;
    let window = i64::from(fix.window_days());
    for kept in 0..window {
        assert!(before.contains(&day_ago(&fix, kept).await), "{kept}");
    }
    let report = store
        .drop_expired_partitions(margin(1))
        .await
        .expect("a drop");
    assert!(report.dropped.is_empty() && !report.skipped);
    assert_eq!(fix.partition_days().await, before);
}

/// Runs at once take turns: every expired partition is dropped exactly once, whoever does it, and a run that finds
/// another holding the lock reports it and does nothing.
pub async fn concurrent_runs_drop_each_partition_once<F: RetentionFixture>(fix: F) {
    let store = fix.store();
    setup(&store).await;
    let window = i64::from(fix.window_days());
    let mut planted = Vec::new();
    for n in 10..16 {
        fix.plant(&format!("old{n}"), window + n, Planted::Applied)
            .await;
        planted.push(day_ago(&fix, window + n).await);
    }
    planted.sort_unstable();
    let tasks: Vec<_> = (0..4)
        .map(|_| {
            let store = store.clone();
            tokio::spawn(async move { store.drop_expired_partitions(margin(2)).await })
        })
        .collect();
    let mut dropped = Vec::new();
    for task in tasks {
        let report = task.await.expect("a task").expect("a drop");
        assert!(!report.skipped || report.dropped.is_empty());
        dropped.extend(report.dropped);
    }
    dropped.sort_unstable();
    assert_eq!(dropped, planted, "each partition dropped once");
    let days = fix.partition_days().await;
    assert!(planted.iter().all(|day| !days.contains(day)));
}
