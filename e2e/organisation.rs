//! Scenario 3: an organisation and its members. One owner, the organisation, pays for everyone; each member has a
//! monthly cap. The cap refuses a member through `getAllowed` while the organisation keeps its balance and the other
//! members go on. The cap is a read, not a refusal of the debit: usage that already happened is booked.

use std::time::Duration;

use serde_json::json;

use crate::support::{Deployment, Who, micros, now, priced};

const ORG: &str = "org-ex-1";
const EUR_1: i64 = 1_000_000;

#[tokio::test(flavor = "multi_thread")]
async fn an_organisation_pays_and_a_members_cap_refuses_that_member_alone() {
    let Some(deployment) = Deployment::start(1, &Vec::new()).await else {
        return;
    };
    let d = &deployment;
    d.configure().await;
    d.open("acct-org-ex-1", ORG).await;
    d.grant("adjustment", "prepaid-org-ex-1", ORG, 100 * EUR_1)
        .await;
    for (member, cap) in [("member-a", 10 * EUR_1), ("member-b", 30 * EUR_1)] {
        d.write(
            "setMemberCap",
            Who::Operator,
            json!({"owner": ORG, "currency": "EUR", "member": member, "capMicros": cap.to_string()}),
        )
        .await
        .ok();
    }
    // Only the operator sets a cap; the organisation itself may not raise its members'.
    let own = d
        .write(
            "setMemberCap",
            Who::Owner(ORG),
            json!({"owner": ORG, "currency": "EUR", "member": "member-a", "capMicros": null}),
        )
        .await;
    assert_eq!(own.status, 403, "{}", own.text);

    let t = now();
    d.usage(vec![
        priced("org-use-1", ORG, "member-a", 10 * EUR_1, t),
        priced("org-use-2", ORG, "member-b", 12 * EUR_1, t),
        priced("org-use-3", ORG, "member-c", 3 * EUR_1, t),
    ])
    .await
    .ok();
    d.wait_decided(3, Duration::from_secs(30)).await;

    // member-a has spent its cap: refused, while the organisation still has 75.
    let a = d.allowed(ORG, "member-a").await;
    assert_eq!(a["allowed"], false);
    assert_eq!(a["refused"], "member_cap");
    assert_eq!(micros(&a["memberRemainingMicros"]), 0);
    assert_eq!(micros(&a["balanceMicros"]), 75 * EUR_1);
    let b = d.allowed(ORG, "member-b").await;
    assert_eq!(
        (b["allowed"].as_bool(), micros(&b["memberRemainingMicros"])),
        (Some(true), 18 * EUR_1)
    );
    let c = d.allowed(ORG, "member-c").await;
    assert_eq!(c["allowed"], true);
    assert_eq!(
        c["memberRemainingMicros"],
        serde_json::Value::Null,
        "no cap, no limit of its own"
    );

    // Usage member-a already caused is still booked: the cap is a read, not a refusal.
    d.usage(vec![priced("org-use-4", ORG, "member-a", 2 * EUR_1, t)])
        .await
        .ok();
    d.wait_decided(4, Duration::from_secs(30)).await;
    let a = d.allowed(ORG, "member-a").await;
    assert_eq!(
        (a["refused"].as_str(), micros(&a["memberRemainingMicros"])),
        (Some("member_cap"), -2 * EUR_1)
    );
    assert_eq!(d.net(ORG).await, 73 * EUR_1);
    assert_eq!(d.allowed(ORG, "member-b").await["allowed"], true);

    // The organisation reads its members' usage; another owner reads none of it.
    let mine = d
        .get(
            "/v1/usage_outcomes?member=member-a&sort=eventId",
            Who::Owner(ORG),
        )
        .await
        .ok();
    let ids: Vec<&str> = mine.body["items"]
        .as_array()
        .unwrap()
        .iter()
        .map(|o| o["eventId"].as_str().unwrap())
        .collect();
    assert_eq!(ids, ["org-use-1", "org-use-4"]);
    let other = d
        .get("/v1/usage_outcomes", Who::Owner("org-ex-2"))
        .await
        .ok();
    assert_eq!(other.body["totalCount"], 0);
    let balance = d
        .read(
            "getBalance",
            Who::Owner("org-ex-2"),
            json!({"owner": ORG, "currency": "EUR"}),
        )
        .await;
    assert_eq!(balance.status, 403, "{}", balance.text);

    // Raising member-a's cap lets it spend again.
    d.write(
        "setMemberCap",
        Who::Operator,
        json!({"owner": ORG, "currency": "EUR", "member": "member-a", "capMicros": (20 * EUR_1).to_string()}),
    )
    .await
    .ok();
    let a = d.allowed(ORG, "member-a").await;
    assert_eq!(
        (a["allowed"].as_bool(), micros(&a["memberRemainingMicros"])),
        (Some(true), 8 * EUR_1)
    );
}
