//! Scenario 6: price book versions. A new version applies from its start only; events that occurred before it keep
//! the price of the version in force when they happened, whenever they arrive. A version is loaded once per content
//! and is never back-dated.

use std::time::Duration;

use serde_json::json;

use crate::support::{Deployment, SECOND, Who, micros, now, plus, raw, text};

const OWNER: &str = "tenant-ex-6";

fn list(effective_from: &str, price: i64) -> String {
    format!(
        "effective_from: \"{effective_from}\"\n\
         meters:\n  - name: compute.seconds\n    unit: second\n\
         prices:\n  - meter: compute.seconds\n    currency: EUR\n    price_micros: {price}\n"
    )
}

fn compute(id: &str, seconds: i64, at: openbilling_core::Timestamp) -> serde_json::Value {
    raw(
        id,
        OWNER,
        "member-ex-1",
        "compute.seconds",
        seconds,
        &[],
        at,
    )
}

#[tokio::test(flavor = "multi_thread")]
async fn a_new_price_version_applies_from_its_start_and_older_events_keep_their_price() {
    let Some(deployment) = Deployment::start(1, &Vec::new()).await else {
        return;
    };
    let d = &deployment;
    d.configure().await;
    d.open("acct-ex-6", OWNER).await;

    // Version 1 from T1 at 100 a second, version 2 from T2 at 150. Both ahead of the clock (a version is never
    // back-dated), and the events inside the 5-minute skew, so nothing has to wait.
    let t = now();
    let (t1, t2) = (plus(t, 30 * SECOND), plus(t, 60 * SECOND));
    let load = async |yaml: String| {
        d.write("loadPriceList", Who::Operator, json!({ "yaml": yaml }))
            .await
    };
    let v1 = load(list(&text(t1), 100)).await.ok();
    let v2 = load(list(&text(t2), 150)).await.ok();
    let (v1, v2) = (
        v1.body["version"].as_str().unwrap().to_owned(),
        v2.body["version"].as_str().unwrap().to_owned(),
    );
    assert_ne!(v1, v2);
    // The same list again is the same version, replayed.
    let again = load(list(&text(t1), 100)).await.ok();
    assert_eq!(
        (
            again.body["version"].as_str(),
            again.body["replayed"].as_bool()
        ),
        (Some(v1.as_str()), Some(true))
    );
    // Other prices at T2 are another version at the same instant: a conflict.
    let same = load(list(&text(t2), 1)).await;
    assert_eq!(same.status, 409, "{}", same.text);
    assert!(same.text.contains("version_conflict"), "{}", same.text);

    // Before T1 nothing is in force: refused at intake.
    let early = d
        .raw_usage(vec![compute("job-early", 1, plus(t1, -SECOND))])
        .await;
    assert_eq!(early.status, 422, "{}", early.text);
    assert!(
        early.text.contains("unpriced_meter") && early.text.contains("job-early"),
        "{}",
        early.text
    );

    // The last microsecond of version 1, the first of version 2, then a late event from version 1's time.
    d.raw_usage(vec![
        compute("job-1", 2, plus(t2, -1)),
        compute("job-2", 2, t2),
    ])
    .await
    .ok();
    d.raw_usage(vec![compute("job-late", 3, plus(t1, 10 * SECOND))])
        .await
        .ok();
    d.wait_decided(3, Duration::from_secs(30)).await;

    let outcomes = d.outcomes(OWNER).await;
    let seen: Vec<(&str, i64, &str)> = outcomes
        .iter()
        .map(|o| {
            (
                o["eventId"].as_str().unwrap(),
                micros(&o["amount"]),
                o["priceVersion"].as_str().unwrap(),
            )
        })
        .collect();
    assert_eq!(
        seen,
        [
            ("job-1", 200, v1.as_str()),
            ("job-2", 300, v2.as_str()),
            ("job-late", 300, v1.as_str()),
        ]
    );

    // No version can take effect where an event is already rated (job-2 at T2, so T2 itself too), nor before the
    // clock.
    for at in [plus(t2, -5 * SECOND), t2, plus(t, -3_600 * SECOND)] {
        let refused = load(list(&text(at), 1)).await;
        assert_eq!(refused.status, 422, "{}", refused.text);
        assert!(refused.text.contains("back_dated"), "{}", refused.text);
    }
}
