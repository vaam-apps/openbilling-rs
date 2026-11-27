//! Scenario 4: once-only. The same event sent three times debits once; the same id with another payload is a
//! conflict; an event older than the window, or too far ahead, is refused at intake; the same id on another day is
//! taken in and refused when drained, so it moves no money either.

use std::time::Duration;

use crate::support::{DAY, Deployment, SECOND, Who, micros, now, plus, priced, read_only_pool};

const OWNER: &str = "tenant-ex-4";

#[tokio::test(flavor = "multi_thread")]
async fn the_same_event_sent_three_times_debits_once_and_a_stale_event_is_refused_at_intake() {
    let Some(deployment) = Deployment::start(1, &Vec::new()).await else {
        return;
    };
    let d = &deployment;
    d.configure().await;
    d.open("acct-ex-4", OWNER).await;
    d.grant("adjustment", "prepaid-ex-4", OWNER, 10_000_000)
        .await;

    let t = now();
    let event = priced("call-ex-1", OWNER, "member-ex-1", 1_500_000, t);
    let body = serde_json::json!({"events": [event.clone()]});
    // Twice under one Idempotency-Key with the token the producer holds: the second is the first answer, replayed by
    // the API. (CrateStack namespaces a key by the exact Authorization header, so a retry with a refreshed token is
    // not this replay: it runs again, and the ledger's once-only record answers it, as the third send shows.)
    let token = d.producer_token();
    let first = d
        .call_at(
            0,
            "recordUsage",
            Who::Token(&token),
            Some("retry-1"),
            body.clone(),
        )
        .await
        .ok();
    assert_eq!(
        (
            first.body["accepted"].as_i64(),
            first.body["replayed"].as_i64()
        ),
        (Some(1), Some(0))
    );
    let again = d
        .call_at(
            0,
            "recordUsage",
            Who::Token(&token),
            Some("retry-1"),
            body.clone(),
        )
        .await
        .ok();
    assert_eq!(
        again
            .headers
            .get("idempotency-replayed")
            .and_then(|v| v.to_str().ok()),
        Some("true")
    );
    assert_eq!(again.body, first.body);
    let refreshed = d
        .call_at(
            0,
            "recordUsage",
            Who::Producer,
            Some("retry-1"),
            body.clone(),
        )
        .await
        .ok();
    assert_eq!(refreshed.headers.get("idempotency-replayed"), None);
    assert_eq!(
        refreshed.body["replayed"], 1,
        "the ledger replays what the API ran again"
    );
    // Under another key, as a producer does after losing its own record: the ledger's replay again.
    let third = d.usage(vec![event]).await.ok();
    assert_eq!(
        (
            third.body["accepted"].as_i64(),
            third.body["replayed"].as_i64()
        ),
        (Some(0), Some(1))
    );
    assert_eq!(third.body["answers"][0]["answer"], "replayed");

    // The same id and day with another amount: a conflict, naming the event and nothing of its payload.
    let conflict = d
        .usage(vec![priced(
            "call-ex-1",
            OWNER,
            "member-ex-1",
            9_999_999,
            t,
        )])
        .await;
    assert_eq!(conflict.status, 409, "{}", conflict.text);
    assert!(
        conflict.text.contains("payload_conflict") && conflict.text.contains("call-ex-1"),
        "{}",
        conflict.text
    );
    assert!(!conflict.text.contains("9999999"), "{}", conflict.text);

    // Older than the window (7 days by default) and too far ahead (past the 5-minute skew): refused, nothing taken.
    let stale = d
        .usage(vec![priced(
            "call-ex-old",
            OWNER,
            "member-ex-1",
            1,
            plus(t, -8 * DAY),
        )])
        .await;
    assert_eq!(stale.status, 422, "{}", stale.text);
    assert!(
        stale.text.contains("too_old") && stale.text.contains("call-ex-old"),
        "{}",
        stale.text
    );
    let ahead = d
        .usage(vec![priced(
            "call-ex-new",
            OWNER,
            "member-ex-1",
            1,
            plus(t, 600 * SECOND),
        )])
        .await;
    assert_eq!(ahead.status, 422, "{}", ahead.text);
    assert!(ahead.text.contains("too_far_ahead"), "{}", ahead.text);

    d.wait_decided(1, Duration::from_secs(30)).await;
    assert_eq!(d.net(OWNER).await, 10_000_000 - 1_500_000, "debited once");

    // The same id on another day lands in another partition, so intake takes it; the drain refuses it (`id_reused`)
    // and the first one stays the only debit.
    let yesterday = plus(t, -DAY);
    let reused = d
        .usage(vec![priced(
            "call-ex-1",
            OWNER,
            "member-ex-1",
            1_500_000,
            yesterday,
        )])
        .await
        .ok();
    assert_eq!(reused.body["accepted"], 1);
    let pool = read_only_pool(&d.database_url).await;
    let deadline = std::time::Instant::now() + Duration::from_secs(30);
    loop {
        let refusal: Option<String> = sqlx::query_scalar(
            "SELECT refusal FROM ob_usage_event WHERE event_id = 'call-ex-1' AND occurred_on = $1::date",
        )
        .bind(&crate::support::text(yesterday)[..10])
        .fetch_one(&pool)
        .await
        .unwrap();
        if let Some(refusal) = refusal {
            assert_eq!(refusal, "id_reused");
            break;
        }
        assert!(
            std::time::Instant::now() < deadline,
            "the reused id was not drained"
        );
        tokio::time::sleep(Duration::from_millis(200)).await;
    }
    let outcome = d
        .get("/v1/usage_outcomes/call-ex-1", Who::Owner(OWNER))
        .await
        .ok();
    assert_eq!(micros(&outcome.body["amount"]), 1_500_000);
    assert_eq!(
        &outcome.body["occurredAt"].as_str().unwrap()[..10],
        &crate::support::text(t)[..10]
    );
    assert_eq!(
        d.net(OWNER).await,
        10_000_000 - 1_500_000,
        "still debited once"
    );
    let txns: i64 = sqlx::query_scalar("SELECT count(*) FROM ob_txn WHERE event_id = 'call-ex-1'")
        .fetch_one(&pool)
        .await
        .unwrap();
    assert_eq!(txns, 1, "one ledger transaction for the event");
}
