//! Scenario 7: statements and series. Usage over the last five days (one or two UTC months), credits of each kind
//! the operator books, and a refused event: each month's statement and the daily usage series equal the sum of the
//! applied events' entries in the ledger, day by day.

use std::collections::BTreeMap;
use std::time::Duration;

use serde_json::json;

use crate::support::{Deployment, SECOND, Who, micros, now, plus, priced, read_only_pool, text};

const OWNER: &str = "tenant-ex-7";

#[tokio::test(flavor = "multi_thread")]
async fn a_months_statement_and_the_daily_series_equal_the_sum_of_the_applied_entries() {
    let Some(deployment) = Deployment::start(1, &Vec::new()).await else {
        return;
    };
    let d = &deployment;
    d.configure().await;
    d.open("acct-ex-7", OWNER).await;
    d.grant("promotion", "welcome-ex-7", OWNER, 5_000_000).await;
    d.grant("adjustment", "prepaid-ex-7", OWNER, 20_000_000)
        .await;
    d.grant("adjustment", "correction-ex-7", OWNER, -1_000_000)
        .await;

    // Four events a day for the four days before today (inside the 7-day window), at noon UTC, and two today.
    let t = now();
    let today = t.utc_day();
    let mut events = Vec::new();
    for days_ago in 1..=4 {
        let noon = plus(
            today.checked_add_days(-days_ago).unwrap().start(),
            12 * 3_600 * SECOND,
        );
        for k in 0..4 {
            let amount = 10_000 * (days_ago + 1) + 1_111 * k;
            events.push(priced(
                &format!("day{days_ago}-ev{k}"),
                OWNER,
                "member-ex-1",
                amount,
                noon,
            ));
        }
    }
    events.push(priced(
        "today-ev0",
        OWNER,
        "member-ex-1",
        70_001,
        plus(t, -60 * SECOND),
    ));
    events.push(priced(
        "today-ev1",
        OWNER,
        "member-ex-2",
        29_999,
        plus(t, -30 * SECOND),
    ));
    // An owner the ledger has no account for: taken in, then refused when drained; it is on no statement.
    events.push(priced(
        "stray-ev0",
        "tenant-ex-none",
        "member-ex-1",
        5_000,
        t,
    ));
    let sent = events.len() as u64;
    d.usage(events).await.ok();
    d.wait_decided(sent, Duration::from_secs(30)).await;
    let stray = d
        .get("/v1/usage_outcomes/stray-ev0", Who::Operator)
        .await
        .ok();
    assert_eq!(
        (stray.body["state"].as_str(), stray.body["reason"].as_str()),
        (Some("refused"), Some("unknown_account"))
    );

    // The ledger's own entries: revenue per day of occurrence of the applied usage (read-only).
    let pool = read_only_pool(&d.database_url).await;
    let rows: Vec<(String, i64)> = sqlx::query_as(
        "SELECT to_char(o.occurred_on, 'YYYY-MM-DD'), sum(e.amount)::bigint \
         FROM ob_entry e JOIN ob_txn x ON x.txn_id = e.txn_id JOIN ob_event_outcome o ON o.event_id = x.event_id \
         WHERE x.kind = 'usage' AND e.book = 'revenue' AND o.owner = $1 \
         GROUP BY o.occurred_on ORDER BY o.occurred_on",
    )
    .bind(OWNER)
    .fetch_all(&pool)
    .await
    .unwrap();
    let ledger: BTreeMap<String, i64> = rows.into_iter().collect();
    assert_eq!(ledger.len(), 5);
    // And the outcomes the API serves agree with it.
    let mut from_outcomes: BTreeMap<String, i64> = BTreeMap::new();
    for o in d.outcomes(OWNER).await {
        assert_eq!(o["state"], "applied");
        *from_outcomes
            .entry(o["occurredAt"].as_str().unwrap()[..10].to_owned())
            .or_default() += micros(&o["amount"]);
    }
    assert_eq!(from_outcomes, ledger);

    // The series over the five days.
    let first = today.checked_add_days(-4).unwrap().start();
    let series = d
        .read(
            "getUsageSeries",
            Who::Owner(OWNER),
            json!({"owner": OWNER, "currency": "EUR", "fromTime": text(first), "toTime": text(t)}),
        )
        .await
        .ok();
    let series: BTreeMap<String, i64> = series
        .body
        .as_array()
        .unwrap()
        .iter()
        .map(|r| {
            (
                r["day"].as_str().unwrap().to_owned(),
                micros(&r["amountMicros"]),
            )
        })
        .collect();
    assert_eq!(series, ledger);

    // Each month the five days touch: its usage lines are the ledger's days of that month, its total their sum, and
    // the credits are those booked this month.
    let mut months: Vec<(i32, u8)> = ledger
        .keys()
        .map(|day| (day[..4].parse().unwrap(), day[5..7].parse().unwrap()))
        .collect();
    months.dedup();
    let mut usage_total = 0;
    for (year, month) in months {
        let lines = d.statement(OWNER, year, month).await;
        let prefix = format!("{year:04}-{month:02}");
        let days: BTreeMap<String, i64> = lines
            .iter()
            .filter(|l| l["line"] == "line" && l["kind"] == "usage")
            .map(|l| {
                (
                    l["day"].as_str().unwrap().to_owned(),
                    -micros(&l["amountMicros"]),
                )
            })
            .collect();
        let expected: BTreeMap<String, i64> = ledger
            .iter()
            .filter(|(day, _)| day.starts_with(&prefix))
            .map(|(d, a)| (d.clone(), *a))
            .collect();
        assert_eq!(days, expected, "{prefix}");
        let total = lines
            .iter()
            .find(|l| l["line"] == "total" && l["kind"] == "usage")
            .map(|l| -micros(&l["amountMicros"]));
        assert_eq!(total, Some(expected.values().sum::<i64>()), "{prefix}");
        usage_total += total.unwrap();
        if prefix == text(t)[..7] {
            let credit = |kind: &str| {
                lines
                    .iter()
                    .find(|l| l["line"] == "total" && l["kind"] == kind)
                    .map(|l| micros(&l["amountMicros"]))
            };
            assert_eq!(credit("promotion"), Some(5_000_000));
            assert_eq!(
                credit("adjustment"),
                Some(19_000_000),
                "20 less the correction of 1"
            );
        }
    }
    assert_eq!(usage_total, ledger.values().sum::<i64>());
    assert_eq!(d.net(OWNER).await, 24_000_000 - usage_total);
    let unbalanced: i64 = sqlx::query_scalar(
        "SELECT count(*) FROM (SELECT txn_id FROM ob_entry GROUP BY txn_id HAVING sum(amount) <> 0) x",
    )
    .fetch_one(&pool)
    .await
    .unwrap();
    assert_eq!(unbalanced, 0, "every transaction balances");
}
