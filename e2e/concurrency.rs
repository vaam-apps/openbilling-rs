//! Scenario 5: concurrency. Three `serve` instances on one database, each running its own drain, take and work
//! through 10 000 events sent to all three at once. Every event is decided once, the ledger sums to zero per
//! currency, and each owner's balance is its credit less its usage, to the micro-unit.

use std::collections::BTreeMap;
use std::time::Duration;

use crate::support::{Deployment, Who, now, priced, read_only_pool};

const INSTANCES: u32 = 3;
const OWNERS: usize = 20;
const EVENTS: usize = 10_000;
/// The API takes at most 1 000 events per call.
const BATCH: usize = 1_000;
const CREDIT: i64 = 2_000_000;

fn owner(i: usize) -> String {
    format!("tenant-cc-{i:02}")
}

#[tokio::test(flavor = "multi_thread")]
async fn three_services_drain_ten_thousand_events_and_each_is_applied_exactly_once() {
    let Some(deployment) = Deployment::start(INSTANCES, &Vec::new()).await else {
        return;
    };
    let d = &deployment;
    d.configure().await;
    for i in 0..OWNERS {
        d.open(&format!("acct-cc-{i:02}"), &owner(i)).await;
        d.grant(
            "adjustment",
            &format!("prepaid-cc-{i:02}"),
            &owner(i),
            CREDIT,
        )
        .await;
    }

    // Every event is a few hundred micro-units; owners spend past their credit, so the drains also book debt.
    let t = now();
    let mut usage: BTreeMap<String, i64> = BTreeMap::new();
    let events: Vec<_> = (0..EVENTS)
        .map(|n| {
            let who = owner(n % OWNERS);
            let amount = 300 + i64::try_from(n % 97).unwrap() * 7;
            *usage.entry(who.clone()).or_default() += amount;
            priced(
                &format!("cc-{n:05}"),
                &who,
                &format!("member-{}", n % 5),
                amount,
                t,
            )
        })
        .collect();
    // All ten batches at once, spread over the three services, while their drains already run.
    let calls: Vec<_> = events
        .chunks(BATCH)
        .enumerate()
        .map(|(i, chunk)| {
            let body = serde_json::json!({ "events": chunk });
            d.spawn_call(
                i % d.services.len(),
                "recordUsage",
                Who::Producer,
                Some(&d.key()),
                &body,
            )
        })
        .collect();
    for call in calls {
        let answer = call.await.unwrap().ok();
        assert_eq!(
            answer.body["accepted"].as_u64(),
            Some(u64::try_from(BATCH).unwrap())
        );
    }
    d.wait_decided(u64::try_from(EVENTS).unwrap(), Duration::from_secs(300))
        .await;

    // Through the API: every event applied once, and each owner's balance is its credit less its usage.
    let applied = d
        .get("/v1/usage_outcomes?state=applied&limit=1", Who::Operator)
        .await
        .ok();
    assert_eq!(
        applied.body["totalCount"].as_u64(),
        Some(u64::try_from(EVENTS).unwrap())
    );
    for (who, spent) in &usage {
        assert_eq!(d.net(who).await, CREDIT - spent, "{who}");
    }

    // Straight from the ledger's tables, read-only: what no API answers.
    let pool = read_only_pool(&d.database_url).await;
    let unbalanced: i64 = sqlx::query_scalar(
        "SELECT count(*) FROM (SELECT currency FROM ob_entry GROUP BY currency HAVING sum(amount) <> 0) x",
    )
    .fetch_one(&pool)
    .await
    .unwrap();
    assert_eq!(unbalanced, 0, "the ledger sums to zero per currency");
    let (txns, distinct): (i64, i64) = sqlx::query_as(
        "SELECT count(*), count(DISTINCT event_id) FROM ob_txn WHERE kind = 'usage'",
    )
    .fetch_one(&pool)
    .await
    .unwrap();
    assert_eq!(
        (txns, distinct),
        (
            i64::try_from(EVENTS).unwrap(),
            i64::try_from(EVENTS).unwrap()
        )
    );
    let revenue: i64 = sqlx::query_scalar(
        "SELECT sum(e.amount)::bigint FROM ob_entry e JOIN ob_txn x ON x.txn_id = e.txn_id \
         WHERE x.kind = 'usage' AND e.book = 'revenue'",
    )
    .fetch_one(&pool)
    .await
    .unwrap();
    assert_eq!(
        revenue,
        usage.values().sum::<i64>(),
        "revenue is every event's amount, once"
    );
    let waiting: i64 =
        sqlx::query_scalar("SELECT count(*) FROM ob_usage_event WHERE state <> 'applied'")
            .fetch_one(&pool)
            .await
            .unwrap();
    assert_eq!(waiting, 0);
}
