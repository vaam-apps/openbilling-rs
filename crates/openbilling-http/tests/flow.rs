//! The whole API on a real ledger: configure, open, credit, take usage in, drain, and read it back through the
//! models and the read procedures as the owner, another owner and the operator; a top-up through the vpay
//! stand-in, its webhook, a refund. Needs `OPENBILLING_TEST_DATABASE_URL`; skips without it.

#![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]

mod support;

use cratestack::axum::http::{Method, StatusCode};
use openbilling_core::{Amount, CreditId, LedgerStore, RefundStatus};
use openbilling_testkit::payments::{ProviderFixture, SignedWebhook};
use openbilling_vpay::stand_in;
use serde_json::{Value, json};
use support::{OPERATOR, OWNER_1, OWNER_2, PRODUCER, app, call, fresh, procedure, raw};

fn ok(answer: &support::Answer) {
    assert!(
        answer.status.is_success(),
        "{}: {}",
        answer.status,
        answer.text
    );
}

fn now_text() -> String {
    let micros = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap()
        .as_micros();
    openbilling_core::Timestamp::from_unix_micros(i64::try_from(micros).unwrap())
        .unwrap()
        .to_rfc3339_micros()
}

async fn setup(app: &cratestack::axum::Router) {
    let config = json!({
        "currencies": ["EUR", "USD"],
        "labels": ["promo", "main"],
        "lotOrder": [
            {"kind": "allowance", "soonestExpiryFirst": true},
            {"kind": "promotion", "soonestExpiryFirst": true},
            {"kind": "adjustment", "soonestExpiryFirst": false},
            {"kind": "payment", "soonestExpiryFirst": false}
        ]
    });
    ok(&procedure(app, "configure", Some(OPERATOR), Some("cfg-1"), config).await);
    for (id, owner, label) in [
        ("a1", "o1", "main"),
        ("b1", "o1", "promo"),
        ("x1", "o2", "main"),
    ] {
        let account = json!({"accountId": id, "owner": owner, "currency": "EUR", "label": label});
        let answer = procedure(
            app,
            "openAccount",
            Some(OPERATOR),
            Some(&format!("open-{id}")),
            account,
        )
        .await;
        ok(&answer);
        assert_eq!(answer.body["replayed"], false);
    }
}

#[tokio::test]
async fn credit_usage_and_the_reads_through_the_api() {
    let Some((store, ledger)) = fresh().await else {
        return;
    };
    let app = app(ledger.clone(), None, store.pool().clone());
    setup(&app).await;

    let grant = json!({
        "creditId": "g1", "source": "promotion", "owner": "o1", "currency": "EUR", "label": "promo",
        "amountMicros": "50000000"
    });
    let first = procedure(
        &app,
        "grantCredit",
        Some(OPERATOR),
        Some("grant-g1"),
        grant.clone(),
    )
    .await;
    ok(&first);
    assert_eq!(first.body["accountId"], "b1");
    // The same key: the first answer, replayed by CrateStack's idempotency layer.
    let again = procedure(
        &app,
        "grantCredit",
        Some(OPERATOR),
        Some("grant-g1"),
        grant.clone(),
    )
    .await;
    ok(&again);
    assert_eq!(
        again
            .headers
            .get("idempotency-replayed")
            .map(|v| v.to_str().unwrap()),
        Some("true")
    );
    // Another key, the same credit id: the ledger's once-only record answers a replay.
    let other_key = procedure(
        &app,
        "grantCredit",
        Some(OPERATOR),
        Some("grant-g1-retry"),
        grant,
    )
    .await;
    ok(&other_key);
    assert_eq!(other_key.body["replayed"], true);

    // A payment is not on offer to the operator: it comes only from the provider.
    let payment = json!({
        "creditId": "p1", "source": "payment", "owner": "o1", "currency": "EUR", "label": "main",
        "amountMicros": "1"
    });
    let refused = procedure(&app, "grantCredit", Some(OPERATOR), Some("pay-p1"), payment).await;
    assert!(refused.status.is_client_error(), "{}", refused.text);

    // Only an operator writes credit; only a producer or an operator sends usage.
    let as_owner = procedure(
        &app,
        "recordUsage",
        Some(OWNER_1),
        Some("u-owner"),
        json!({"events": []}),
    )
    .await;
    assert_eq!(as_owner.status, StatusCode::FORBIDDEN);

    let at = now_text();
    let events = json!({"events": [
        {"eventId": "e1", "owner": "o1", "member": "m1", "currency": "EUR", "amountMicros": "15000000", "occurredAt": at},
        {"eventId": "e2", "owner": "o1", "member": "m2", "currency": "EUR", "amountMicros": "5000000", "occurredAt": at},
        {"eventId": "e3", "owner": "o2", "member": "m1", "currency": "EUR", "amountMicros": "1000000", "occurredAt": at}
    ]});
    let taken = procedure(&app, "recordUsage", Some(PRODUCER), Some("batch-1"), events).await;
    ok(&taken);
    assert_eq!(taken.body["accepted"], 3);
    let bad = json!({"events": [
        {"eventId": "e4", "owner": "o1", "member": "m1", "currency": "GBP", "amountMicros": "1", "occurredAt": at}
    ]});
    let refused = procedure(&app, "recordUsage", Some(PRODUCER), Some("batch-2"), bad).await;
    assert_eq!(
        refused.status,
        StatusCode::UNPROCESSABLE_ENTITY,
        "{}",
        refused.text
    );
    assert!(
        refused.text.contains("unknown_currency") && refused.text.contains("e4"),
        "{}",
        refused.text
    );
    while !ledger.drain(100).await.unwrap().decided.is_empty() {}

    // The owner's balance: 50 promotional, 20 drawn.
    let balance = procedure(
        &app,
        "getBalance",
        Some(OWNER_1),
        None,
        json!({"owner": "o1", "currency": "EUR"}),
    )
    .await;
    ok(&balance);
    let rows = balance.body.as_array().unwrap();
    assert_eq!(rows.len(), 2);
    assert_eq!(rows[0]["accountId"], "b1");
    assert_eq!(rows[0]["availableMicros"], "30000000");
    assert_eq!(rows[0]["netMicros"], "30000000");
    assert_eq!(rows[0]["debtLimitMicros"], Value::Null);
    assert_eq!(rows[0]["pastLimit"], false);
    let operator = procedure(
        &app,
        "getBalance",
        Some(OPERATOR),
        None,
        json!({"owner": "o1", "currency": "EUR"}),
    )
    .await;
    assert_eq!(operator.body, balance.body);

    let allowed = procedure(
        &app,
        "getAllowed",
        Some(PRODUCER),
        None,
        json!({"owner": "o1", "currency": "EUR", "member": "m1"}),
    )
    .await;
    ok(&allowed);
    assert_eq!(allowed.body["allowed"], true);
    let broke = procedure(
        &app,
        "getAllowed",
        Some(PRODUCER),
        None,
        json!({"owner": "o2", "currency": "EUR", "member": "m1"}),
    )
    .await;
    assert_eq!(broke.body["allowed"], false);
    assert_eq!(broke.body["refused"], "balance");

    let credits = procedure(
        &app,
        "listCredits",
        Some(OWNER_1),
        None,
        json!({"owner": "o1", "currency": "EUR"}),
    )
    .await;
    ok(&credits);
    assert_eq!(credits.body.as_array().unwrap().len(), 1);
    assert_eq!(credits.body[0]["creditId"], "g1");

    let today = &at[..10];
    let (year, month) = (
        at[..4].parse::<i64>().unwrap(),
        at[5..7].parse::<i64>().unwrap(),
    );
    let statement = procedure(
        &app,
        "getStatement",
        Some(OWNER_1),
        None,
        json!({"owner": "o1", "currency": "EUR", "year": year, "month": month}),
    )
    .await;
    ok(&statement);
    let lines: Vec<(String, String, Value, String)> = statement
        .body
        .as_array()
        .unwrap()
        .iter()
        .map(|l| {
            (
                l["line"].as_str().unwrap().to_owned(),
                l["kind"].as_str().unwrap().to_owned(),
                l["day"].clone(),
                l["amountMicros"].as_str().unwrap().to_owned(),
            )
        })
        .collect();
    assert_eq!(
        lines,
        [
            (
                "line".into(),
                "promotion".into(),
                json!(today),
                "50000000".into()
            ),
            (
                "line".into(),
                "usage".into(),
                json!(today),
                "-20000000".into()
            ),
            (
                "total".into(),
                "promotion".into(),
                Value::Null,
                "50000000".into()
            ),
            (
                "total".into(),
                "usage".into(),
                Value::Null,
                "-20000000".into()
            ),
        ]
    );

    let series = procedure(
        &app,
        "getUsageSeries",
        Some(OWNER_1),
        None,
        json!({"owner": "o1", "currency": "EUR", "fromTime": at, "toTime": at}),
    )
    .await;
    ok(&series);
    assert_eq!(
        series.body,
        json!([{"day": today, "amountMicros": "20000000"}])
    );

    // The models: each caller sees their own rows, the operator every row, and filters work on demand.
    let mine = call(
        &app,
        Method::GET,
        "/v1/accounts?sort=accountId",
        Some(OWNER_1),
        None,
        None,
    )
    .await;
    ok(&mine);
    let ids: Vec<&str> = mine.body["items"]
        .as_array()
        .unwrap()
        .iter()
        .map(|a| a["accountId"].as_str().unwrap())
        .collect();
    assert_eq!(ids, ["a1", "b1"]);
    let theirs = call(
        &app,
        Method::GET,
        "/v1/accounts/a1",
        Some(OWNER_2),
        None,
        None,
    )
    .await;
    assert_eq!(theirs.status, StatusCode::NOT_FOUND);
    let all = call(
        &app,
        Method::GET,
        "/v1/accounts",
        Some(OPERATOR),
        None,
        None,
    )
    .await;
    assert_eq!(all.body["totalCount"], 3);
    let outcomes = call(
        &app,
        Method::GET,
        "/v1/usage_outcomes?state=applied&sort=eventId",
        Some(OWNER_1),
        None,
        None,
    )
    .await;
    ok(&outcomes);
    let events: Vec<&str> = outcomes.body["items"]
        .as_array()
        .unwrap()
        .iter()
        .map(|o| o["eventId"].as_str().unwrap())
        .collect();
    assert_eq!(events, ["e1", "e2"]);
    let lots = call(
        &app,
        Method::GET,
        "/v1/lots?kind=promotion",
        Some(OWNER_1),
        None,
        None,
    )
    .await;
    ok(&lots);
    assert_eq!(lots.body["items"][0]["remaining"], "30000000");
    let none = call(&app, Method::GET, "/v1/lots", Some(OWNER_2), None, None).await;
    assert_eq!(none.body["totalCount"], 0);
    // No write route is mounted on a model.
    let post = call(
        &app,
        Method::POST,
        "/v1/accounts",
        Some(OPERATOR),
        Some("w"),
        Some(json!({})),
    )
    .await;
    assert!(post.status.is_client_error(), "{}", post.status);
}

fn webhook(hook: &SignedWebhook) -> cratestack::axum::http::Request<cratestack::axum::body::Body> {
    let mut request = cratestack::axum::http::Request::builder()
        .method(Method::POST)
        .uri(openbilling_http::WEBHOOK_PATH);
    if let Some(value) = hook.headers.all("vpay-signature").next() {
        request = request.header("vpay-signature", value);
    }
    request
        .body(cratestack::axum::body::Body::from(hook.body.clone()))
        .unwrap()
}

fn now() -> openbilling_core::Timestamp {
    let micros = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap()
        .as_micros();
    openbilling_core::Timestamp::from_unix_micros(i64::try_from(micros).unwrap()).unwrap()
}

#[tokio::test]
async fn a_top_up_through_vpay_credits_once_and_a_refund_reverses_it() {
    let Some((store, ledger)) = fresh().await else {
        return;
    };
    let fixture = stand_in::fixture().await;
    let app = app(
        ledger.clone(),
        Some(fixture.provider.clone()),
        store.pool().clone(),
    );
    setup(&app).await;

    let top_up = json!({
        "key": "top-1", "owner": "o1", "currency": "EUR", "label": "main", "amountMicros": "12340000",
        "successUrl": "https://shop.example/paid", "cancelUrl": "https://shop.example/cancel"
    });
    let other = procedure(
        &app,
        "startTopUp",
        Some(OWNER_2),
        Some("t1"),
        top_up.clone(),
    )
    .await;
    assert_eq!(
        other.status,
        StatusCode::FORBIDDEN,
        "o2 tops up only their own accounts"
    );
    let started = procedure(&app, "startTopUp", Some(OWNER_1), Some("t1"), top_up).await;
    ok(&started);
    assert_eq!(started.body["status"], "pending");
    assert!(
        started.body["checkoutUrl"]
            .as_str()
            .unwrap()
            .starts_with("http://checkout.invalid/c/cs_")
    );
    let payment = CreditId::new(started.body["payment"].as_str().unwrap()).unwrap();

    // Paid at vpay; the webhook credits once, and a second delivery replays.
    fixture.pay(&payment).await;
    let first = raw(&app, webhook(&fixture.payment_webhook(&payment, now()))).await;
    ok(&first);
    assert_eq!(
        (
            first.body["outcome"].as_str(),
            first.body["replayed"].as_bool()
        ),
        (Some("credited"), Some(false))
    );
    let second = raw(&app, webhook(&fixture.payment_webhook(&payment, now()))).await;
    assert_eq!(
        (
            second.body["outcome"].as_str(),
            second.body["replayed"].as_bool()
        ),
        (Some("credited"), Some(true))
    );
    // The operator's reconciliation reads it back and books nothing more.
    let reconciled = procedure(
        &app,
        "reconcilePayment",
        Some(OPERATOR),
        Some("r-1"),
        json!({"id": payment.as_str()}),
    )
    .await;
    assert_eq!(reconciled.body["replayed"], true);

    let at = ledger.now().await.unwrap();
    let held = |b: openbilling_core::Balance| b.net().micros();
    assert_eq!(
        held(
            ledger
                .balance(
                    &openbilling_testkit::fixtures::owner("o1"),
                    &openbilling_testkit::fixtures::eur(),
                    at
                )
                .await
                .unwrap()
        ),
        12_340_000
    );

    // A pending refund books nothing; a succeeded one reverses, once.
    let part = Amount::from_micros(2_000_000).unwrap();
    let pending = fixture.refund(&payment, part, RefundStatus::Pending).await;
    let answer = raw(&app, webhook(&fixture.refund_webhook(&pending, now()))).await;
    assert_eq!(answer.body["outcome"], "refund_not_settled");
    let done = fixture
        .refund(&payment, part, RefundStatus::Succeeded)
        .await;
    let answer = raw(&app, webhook(&fixture.refund_webhook(&done, now()))).await;
    assert_eq!(
        (
            answer.body["outcome"].as_str(),
            answer.body["replayed"].as_bool()
        ),
        (Some("reversed"), Some(false))
    );
    let again = procedure(
        &app,
        "reconcileRefund",
        Some(OPERATOR),
        Some("r-2"),
        json!({"id": done.as_str()}),
    )
    .await;
    assert_eq!(again.body["replayed"], true);
    let at = ledger.now().await.unwrap();
    assert_eq!(
        held(
            ledger
                .balance(
                    &openbilling_testkit::fixtures::owner("o1"),
                    &openbilling_testkit::fixtures::eur(),
                    at
                )
                .await
                .unwrap()
        ),
        10_340_000
    );

    let credits = procedure(
        &app,
        "listCredits",
        Some(OWNER_1),
        None,
        json!({"owner": "o1", "currency": "EUR"}),
    )
    .await;
    assert_eq!(credits.body[0]["source"], "payment");
    assert_eq!(credits.body[0]["creditId"], payment.as_str());
}

fn at_offset(secs: i64) -> String {
    let micros = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap()
        .as_micros();
    let base = i64::try_from(micros).unwrap() + secs * 1_000_000;
    openbilling_core::Timestamp::from_unix_micros(base)
        .unwrap()
        .to_rfc3339_micros()
}

#[tokio::test]
async fn raw_usage_a_price_list_and_the_operators_settings_through_the_api() {
    let Some((store, ledger)) = fresh().await else {
        return;
    };
    let app = app(ledger.clone(), None, store.pool().clone());
    setup(&app).await;

    // Only the operator sets limits, caps and prices.
    let limit = json!({"owner": "o1", "currency": "EUR", "limitMicros": "1000"});
    let as_producer = procedure(
        &app,
        "setDebtLimit",
        Some(PRODUCER),
        Some("l0"),
        limit.clone(),
    )
    .await;
    assert_eq!(as_producer.status, StatusCode::FORBIDDEN);
    ok(&procedure(&app, "setDebtLimit", Some(OPERATOR), Some("l1"), limit).await);
    let cap = json!({"owner": "o1", "currency": "EUR", "member": "m1", "capMicros": "500"});
    ok(&procedure(&app, "setMemberCap", Some(OPERATOR), Some("c1"), cap).await);
    let unknown = json!({"owner": "nobody", "currency": "EUR", "limitMicros": null});
    let refused = procedure(&app, "setDebtLimit", Some(OPERATOR), Some("l2"), unknown).await;
    assert!(refused.text.contains("unknown_account"), "{}", refused.text);

    let from = at_offset(60);
    let yaml = format!(
        "effective_from: \"{from}\"\nmeters:\n  - name: tokens.input\n    unit: token\n    dimensions: [model]\n\
         prices:\n  - meter: tokens.input\n    currency: EUR\n    dimensions: {{ model: example-small }}\n    \
         price_micros: 3\n    per: 2\n"
    );
    let loaded = procedure(
        &app,
        "loadPriceList",
        Some(OPERATOR),
        Some("p1"),
        json!({"yaml": yaml}),
    )
    .await;
    ok(&loaded);
    assert_eq!(loaded.body["replayed"], false);
    let version = loaded.body["version"].as_str().unwrap().to_owned();
    let again = procedure(
        &app,
        "loadPriceList",
        Some(OPERATOR),
        Some("p2"),
        json!({"yaml": yaml}),
    )
    .await;
    assert_eq!(
        (
            again.body["version"].as_str(),
            again.body["replayed"].as_bool()
        ),
        (Some(version.as_str()), Some(true))
    );
    let malformed = procedure(
        &app,
        "loadPriceList",
        Some(OPERATOR),
        Some("p3"),
        json!({"yaml": "prices: ["}),
    )
    .await;
    assert_eq!(
        malformed.status,
        StatusCode::UNPROCESSABLE_ENTITY,
        "{}",
        malformed.text
    );
    assert!(
        malformed.text.contains("malformed_price_list"),
        "{}",
        malformed.text
    );

    let raw = |id: &str, at: &str, dims: Value| {
        json!({"eventId": id, "owner": "o1", "member": "m1", "currency": "EUR", "meter": "tokens.input",
               "quantity": "7", "dimensions": dims, "occurredAt": at})
    };
    let small = json!([{"key": "model", "value": "example-small"}]);
    let before = procedure(
        &app,
        "recordRawUsage",
        Some(PRODUCER),
        Some("r0"),
        json!({"events": [raw("r-early", &at_offset(0), small.clone())]}),
    )
    .await;
    assert!(
        before.text.contains("unpriced_meter") && before.text.contains("r-early"),
        "{}",
        before.text
    );
    let twice = json!([{"key": "model", "value": "example-small"}, {"key": "model", "value": "x"}]);
    let dup = procedure(
        &app,
        "recordRawUsage",
        Some(PRODUCER),
        Some("r1"),
        json!({"events": [raw("r-dup", &at_offset(90), twice)]}),
    )
    .await;
    assert_eq!(dup.status, StatusCode::UNPROCESSABLE_ENTITY, "{}", dup.text);
    let taken = procedure(
        &app,
        "recordRawUsage",
        Some(PRODUCER),
        Some("r2"),
        json!({"events": [raw("r1", &at_offset(90), small)]}),
    )
    .await;
    ok(&taken);
    assert_eq!(taken.body["accepted"], 1);
    while !ledger.drain(100).await.unwrap().decided.is_empty() {}

    // 7 × 3 ÷ 2 = 10.5, half to even: 10. The outcome keeps the rating.
    let outcome = call(
        &app,
        Method::GET,
        "/v1/usage_outcomes/r1",
        Some(OWNER_1),
        None,
        None,
    )
    .await;
    ok(&outcome);
    assert_eq!(outcome.body["amount"], "10");
    assert_eq!(outcome.body["meter"], "tokens.input");
    assert_eq!(outcome.body["quantity"], "7");
    assert_eq!(outcome.body["priceVersion"], version.as_str());
    assert_eq!(
        outcome.body["dimensions"],
        json!({"model": "example-small"})
    );
    let balance = procedure(
        &app,
        "getBalance",
        Some(OWNER_1),
        None,
        json!({"owner": "o1", "currency": "EUR"}),
    )
    .await;
    assert_eq!(balance.body[0]["debtLimitMicros"], "1000");
    assert_eq!(balance.body[0]["debtMicros"], "10");
    assert_eq!(balance.body[0]["pastLimit"], false);
}
