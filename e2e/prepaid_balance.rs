//! Scenario 2: a prepaid balance. A credit arrives, usage draws it, and `getAllowed` refuses once the balance is used
//! up, and then once the debt reaches the limit. Usage that already happened is always booked, as debt, even past
//! the limit: the owner's decision of 2026-11-27, "Always book as debt" (ADR 0001, amended, E1 to E3). The owner
//! past its limit is flagged, and credit lifts it.
//!
//! Two services share the database: one with no configuration file (every check `at_or_above`), one whose file says
//! `debt_limit: above` and `balance: never`, a deployer who lets an owner run up debt to its limit. The owner's
//! decision of 2026-11-27 (ADR 0001, amended, F1 to F4): the service reports and the deployer's configuration decides.
//! At every step both answer the same numbers, and their verdicts differ only where the comparisons do.

use std::time::Duration;

use serde_json::{Value, json};

use crate::support::{Deployment, TempDir, Who, micros, now, priced};

const OWNER: &str = "tenant-ex-2";
const MEMBER: &str = "member-ex-1";
const EUR_1: i64 = 1_000_000;

/// The answer without its verdict: the numbers.
fn facts(answer: &Value) -> Value {
    let mut answer = answer.clone();
    let fields = answer.as_object_mut().unwrap();
    fields.remove("allowed");
    fields.remove("refused");
    answer
}

/// The verdict of each service, `None` when allowed, after checking that both answer the same numbers.
async fn verdicts(d: &Deployment) -> (Option<String>, Option<String>) {
    let strict = d.allowed_at(0, OWNER, MEMBER).await;
    let lenient = d.allowed_at(1, OWNER, MEMBER).await;
    assert_eq!(
        facts(&strict),
        facts(&lenient),
        "the numbers do not depend on the policy"
    );
    for answer in [&strict, &lenient] {
        assert_eq!(
            answer["allowed"].as_bool(),
            Some(answer["refused"].is_null())
        );
    }
    let verdict = |answer: &Value| answer["refused"].as_str().map(str::to_owned);
    (verdict(&strict), verdict(&lenient))
}

fn refused(reason: &str) -> Option<String> {
    Some(reason.to_owned())
}

#[tokio::test(flavor = "multi_thread")]
async fn a_prepaid_balance_is_drawn_and_usage_past_the_debt_limit_is_booked_as_debt_and_flagged() {
    let dir = TempDir::new();
    let config = dir.path().join("config.yaml");
    std::fs::write(
        &config,
        "allowed:\n  debt_limit: above\n  balance: never\n  member_cap: at_or_above\n",
    )
    .unwrap();
    let lenient = vec![(
        "OPENBILLING_CONFIG".to_owned(),
        config.to_str().unwrap().to_owned(),
    )];
    let Some(deployment) = Deployment::start_each(&[Vec::new(), lenient]).await else {
        return;
    };
    let d = &deployment;
    assert!(
        d.services[0]
            .log()
            .contains("debt_limit at_or_above, balance at_or_above"),
        "{}",
        d.services[0].log()
    );
    assert!(
        d.services[1]
            .log()
            .contains("debt_limit above, balance never, member_cap at_or_above"),
        "{}",
        d.services[1].log()
    );
    d.configure().await;
    d.open("acct-ex-2", OWNER).await;
    let limit = async |micros: i64| {
        d.write(
            "setDebtLimit",
            Who::Operator,
            json!({"owner": OWNER, "currency": "EUR", "limitMicros": micros.to_string()}),
        )
        .await
        .ok();
    };

    // A limit of zero and nothing owed: 0 >= 0 refuses under `at_or_above`, with no special case; 0 is not above 0.
    limit(0).await;
    assert_eq!(verdicts(d).await, (refused("debt_limit"), None));
    let answer = d.allowed(OWNER, MEMBER).await;
    assert_eq!(
        (
            micros(&answer["debtMicros"]),
            micros(&answer["debtLimitMicros"]),
            answer["pastLimit"].as_bool()
        ),
        (0, 0, Some(false))
    );

    limit(5 * EUR_1).await;
    d.grant("adjustment", "prepaid-ex-2", OWNER, 10 * EUR_1)
        .await;
    assert_eq!(verdicts(d).await, (None, None));

    let t = now();
    let mut decided = 0;
    let mut spend = async |id: &str, amount: i64| {
        d.usage(vec![priced(id, OWNER, MEMBER, amount, t)])
            .await
            .ok();
        decided += 1;
        d.wait_decided(decided, Duration::from_secs(30)).await;
    };

    // 6 of 10 drawn: allowed.
    spend("use-ex-1", 6 * EUR_1).await;
    let allowed = d.allowed(OWNER, MEMBER).await;
    assert_eq!(
        (
            allowed["allowed"].as_bool(),
            micros(&allowed["balanceMicros"])
        ),
        (Some(true), 4 * EUR_1)
    );
    assert_eq!(verdicts(d).await, (None, None));

    // 6 more: 4 from the credit and 2 as debt, under the limit of 5. The balance is used up: refused `balance` where
    // the balance is checked, allowed where the deployer said `never` for it.
    spend("use-ex-2", 6 * EUR_1).await;
    let allowed = d.allowed(OWNER, MEMBER).await;
    assert_eq!(allowed["allowed"], false);
    assert_eq!(allowed["refused"], "balance");
    assert_eq!(micros(&allowed["balanceMicros"]), -2 * EUR_1);
    assert_eq!(
        (
            micros(&allowed["availableMicros"]),
            micros(&allowed["debtMicros"])
        ),
        (0, 2 * EUR_1)
    );
    assert_eq!(verdicts(d).await, (refused("balance"), None));
    let row = &d.balance(OWNER).await[0];
    assert_eq!(
        (micros(&row["debtMicros"]), row["pastLimit"].as_bool()),
        (2 * EUR_1, Some(false))
    );

    // 4 more takes the debt to 6, past the limit of 5. It already happened, so it is booked, not refused; the owner
    // is flagged and new calls are refused for the limit, by both comparisons.
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
    let allowed = d.allowed(OWNER, MEMBER).await;
    assert_eq!(
        (allowed["allowed"].as_bool(), allowed["refused"].as_str()),
        (Some(false), Some("debt_limit"))
    );
    assert_eq!(allowed["pastLimit"], true);
    assert_eq!(
        verdicts(d).await,
        (refused("debt_limit"), refused("debt_limit"))
    );

    // The limit raised to the debt: 6 >= 6 refuses, 6 > 6 does not; at the limit is not past it.
    limit(6 * EUR_1).await;
    assert_eq!(verdicts(d).await, (refused("debt_limit"), None));
    assert_eq!(d.allowed(OWNER, MEMBER).await["pastLimit"], false);
    limit(5 * EUR_1).await;

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
    assert_eq!(d.allowed(OWNER, MEMBER).await["allowed"], true);
    assert_eq!(verdicts(d).await, (None, None));

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
