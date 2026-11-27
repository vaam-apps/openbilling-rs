//! Scenario 2: a prepaid balance. A credit arrives, usage draws it, and `getAllowed` refuses once the balance is used
//! up, and then once the debt reaches the limit. Usage that already happened is always booked, as debt, even past
//! the limit: the owner's decision of 2026-11-27, "Always book as debt" (ADR 0001, amended, E1 to E3). The owner
//! past its limit is flagged, and credit lifts it.

use std::time::Duration;

use serde_json::json;

use crate::support::{Deployment, Who, micros, now, priced};

const OWNER: &str = "tenant-ex-2";
const EUR_1: i64 = 1_000_000;

#[tokio::test(flavor = "multi_thread")]
async fn a_prepaid_balance_is_drawn_and_usage_past_the_debt_limit_is_booked_as_debt_and_flagged() {
    let Some(deployment) = Deployment::start(1, &Vec::new()).await else {
        return;
    };
    let d = &deployment;
    d.configure().await;
    d.open("acct-ex-2", OWNER).await;
    d.write(
        "setDebtLimit",
        Who::Operator,
        json!({"owner": OWNER, "currency": "EUR", "limitMicros": (5 * EUR_1).to_string()}),
    )
    .await
    .ok();
    d.grant("adjustment", "prepaid-ex-2", OWNER, 10 * EUR_1)
        .await;
    assert_eq!(d.allowed(OWNER, "member-ex-1").await["allowed"], true);

    let t = now();
    let mut decided = 0;
    let mut spend = async |id: &str, amount: i64| {
        d.usage(vec![priced(id, OWNER, "member-ex-1", amount, t)])
            .await
            .ok();
        decided += 1;
        d.wait_decided(decided, Duration::from_secs(30)).await;
    };

    // 6 of 10 drawn: allowed.
    spend("use-ex-1", 6 * EUR_1).await;
    let allowed = d.allowed(OWNER, "member-ex-1").await;
    assert_eq!(
        (
            allowed["allowed"].as_bool(),
            micros(&allowed["balanceMicros"])
        ),
        (Some(true), 4 * EUR_1)
    );

    // 6 more: 4 from the credit and 2 as debt, under the limit of 5. The balance is used up: refused `balance`.
    spend("use-ex-2", 6 * EUR_1).await;
    let allowed = d.allowed(OWNER, "member-ex-1").await;
    assert_eq!(allowed["allowed"], false);
    assert_eq!(allowed["refused"], "balance");
    assert_eq!(micros(&allowed["balanceMicros"]), -2 * EUR_1);
    let row = &d.balance(OWNER).await[0];
    assert_eq!(
        (micros(&row["debtMicros"]), row["pastLimit"].as_bool()),
        (2 * EUR_1, Some(false))
    );

    // 4 more takes the debt to 6, past the limit of 5. It already happened, so it is booked, not refused; the owner
    // is flagged and new calls are refused for the limit.
    spend("use-ex-3", 4 * EUR_1).await;
    let outcome = d
        .get("/v1/usage_outcomes/use-ex-3", Who::Owner(OWNER))
        .await
        .ok();
    assert_eq!(outcome.body["state"], "applied");
    assert_eq!(outcome.body["reason"], serde_json::Value::Null);
    let row = &d.balance(OWNER).await[0];
    assert_eq!(micros(&row["debtMicros"]), 6 * EUR_1);
    assert_eq!(micros(&row["netMicros"]), -6 * EUR_1);
    assert_eq!(micros(&row["debtLimitMicros"]), 5 * EUR_1);
    assert_eq!(row["pastLimit"], true);
    let allowed = d.allowed(OWNER, "member-ex-1").await;
    assert_eq!(
        (allowed["allowed"].as_bool(), allowed["refused"].as_str()),
        (Some(false), Some("debt_limit"))
    );

    // A credit repays the debt first; the flag and the refusal go.
    let repaid = d
        .grant("adjustment", "prepaid-ex-2b", OWNER, 10 * EUR_1)
        .await;
    assert_eq!(micros(&repaid.body["repaidMicros"]), 6 * EUR_1);
    let row = &d.balance(OWNER).await[0];
    assert_eq!(
        (micros(&row["netMicros"]), row["pastLimit"].as_bool()),
        (4 * EUR_1, Some(false))
    );
    assert_eq!(d.allowed(OWNER, "member-ex-1").await["allowed"], true);

    // Every debit is on the statement: 16 of usage against 20 of credit.
    let (year, month, _) = t.utc_day().ymd();
    let lines = d.statement(OWNER, year, month).await;
    let total = |kind: &str| {
        lines
            .iter()
            .find(|l| l["line"] == "total" && l["kind"] == kind)
            .map(|l| micros(&l["amountMicros"]))
    };
    assert_eq!(total("usage"), Some(-16 * EUR_1));
    assert_eq!(total("adjustment"), Some(20 * EUR_1));
}
