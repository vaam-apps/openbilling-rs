//! Scenario 1: an example AI service reports token usage per model and kind (input, output, cached) and compute
//! seconds. The operator loads the price list through the API (`loadPriceList`); the producer sends raw events
//! (`recordRawUsage`); each is rated by the version in force when it happened, and the statement shows each line.

use std::collections::BTreeMap;
use std::time::Duration;

use serde_json::json;

use crate::support::{Deployment, SECOND, Who, micros, now, plus, rated, raw, text};

/// The price list of the example: per 1 000 tokens of each kind for each model, and per compute second.
const PRICES: [(&str, &str, i64, i64); 7] = [
    ("tokens.input", "example-small", 250, 1_000),
    ("tokens.output", "example-small", 1_000, 1_000),
    ("tokens.cached", "example-small", 25, 1_000),
    ("tokens.input", "example-large", 1_500, 1_000),
    ("tokens.output", "example-large", 6_000, 1_000),
    ("tokens.cached", "example-large", 150, 1_000),
    ("compute.seconds", "", 40, 1),
];

fn price_list(effective_from: &str) -> String {
    let mut yaml = format!(
        "# An example metered service: tokens per model and kind, and compute time.\n\
         effective_from: \"{effective_from}\"\n\
         meters:\n\
         \x20 - name: tokens.input\n    unit: token\n    dimensions: [model]\n\
         \x20 - name: tokens.output\n    unit: token\n    dimensions: [model]\n\
         \x20 - name: tokens.cached\n    unit: token\n    dimensions: [model]\n\
         \x20 - name: compute.seconds\n    unit: second\n\
         prices:\n"
    );
    for (meter, model, price, per) in PRICES {
        yaml.push_str(&format!("  - meter: {meter}\n    currency: EUR\n"));
        if !model.is_empty() {
            yaml.push_str(&format!("    dimensions: {{ model: {model} }}\n"));
        }
        yaml.push_str(&format!("    price_micros: {price}\n    per: {per}\n"));
    }
    yaml
}

fn price_of(meter: &str, model: &str) -> (i64, i64) {
    PRICES
        .iter()
        .find(|(m, d, _, _)| *m == meter && *d == model)
        .map(|(_, _, price, per)| (*price, *per))
        .unwrap()
}

#[tokio::test(flavor = "multi_thread")]
async fn metered_model_usage_is_rated_by_the_version_in_force_and_each_line_shows_on_the_statement()
{
    let Some(deployment) = Deployment::start(1, &Vec::new()).await else {
        return;
    };
    let d = &deployment;
    d.configure().await;
    d.open("acct-ex-1", "tenant-ex-1").await;
    d.grant("adjustment", "prepaid-ex-1", "tenant-ex-1", 1_000_000)
        .await;

    // The list takes effect a little ahead: a version is never back-dated, and events may occur up to the skew
    // ahead of the clock (5 minutes), so they can be sent at once.
    let from = plus(now(), 5 * SECOND);
    let loaded = d
        .write(
            "loadPriceList",
            Who::Operator,
            json!({"yaml": price_list(&text(from))}),
        )
        .await
        .ok();
    assert_eq!(loaded.body["replayed"], false);
    let version = loaded.body["version"].as_str().unwrap().to_owned();
    assert_eq!(
        loaded.body["effectiveFrom"].as_str().unwrap()[..19],
        text(from)[..19]
    );

    // One request of the service, then another: each kind of token per model, and the compute it took.
    let at = |s: i64| plus(from, s * SECOND);
    let usage = [
        ("req-0001-in", "tokens.input", "example-small", 1_234, at(1)),
        ("req-0001-out", "tokens.output", "example-small", 777, at(1)),
        (
            "req-0001-cached",
            "tokens.cached",
            "example-small",
            10,
            at(1),
        ),
        ("req-0001-compute", "compute.seconds", "", 3, at(1)),
        ("req-0002-in", "tokens.input", "example-large", 2_001, at(2)),
        ("req-0002-out", "tokens.output", "example-large", 777, at(2)),
        (
            "req-0002-cached",
            "tokens.cached",
            "example-large",
            4_000,
            at(2),
        ),
        ("req-0002-compute", "compute.seconds", "", 11, at(2)),
    ];
    let events = usage
        .iter()
        .map(|(id, meter, model, quantity, at)| {
            let dims: Vec<(&str, &str)> = if model.is_empty() {
                vec![]
            } else {
                vec![("model", *model)]
            };
            raw(
                id,
                "tenant-ex-1",
                "member-ex-1",
                meter,
                *quantity,
                &dims,
                *at,
            )
        })
        .collect();
    let taken = d.raw_usage(events).await.ok();
    assert_eq!(taken.body["accepted"], 8);

    // A model the list does not price is refused at intake, naming the event, and nothing of its batch is taken.
    let unpriced = d
        .raw_usage(vec![
            raw(
                "req-0003-in",
                "tenant-ex-1",
                "member-ex-1",
                "tokens.input",
                10,
                &[("model", "example-medium")],
                at(3),
            ),
            raw(
                "req-0003-out",
                "tenant-ex-1",
                "member-ex-1",
                "tokens.output",
                10,
                &[("model", "example-small")],
                at(3),
            ),
        ])
        .await;
    assert_eq!(unpriced.status, 422, "{}", unpriced.text);
    assert!(
        unpriced.text.contains("unpriced_dimension") && unpriced.text.contains("req-0003-in"),
        "{}",
        unpriced.text
    );
    d.wait_decided(8, Duration::from_secs(60)).await;

    // Each outcome: the amount worked out here, the version that rated it, and what it measured.
    let outcomes = d.outcomes("tenant-ex-1").await;
    assert_eq!(outcomes.len(), 8, "the refused batch left no outcome");
    let mut expected_total = 0;
    let mut per_meter: BTreeMap<(String, String), i64> = BTreeMap::new();
    for (id, meter, model, quantity, at) in usage {
        let (price, per) = price_of(meter, model);
        let amount = rated(quantity, price, per);
        expected_total += amount;
        *per_meter
            .entry((text(at)[..10].to_owned(), meter.to_owned()))
            .or_default() += amount;
        let outcome = outcomes.iter().find(|o| o["eventId"] == id).unwrap();
        assert_eq!(outcome["state"], "applied", "{id}");
        assert_eq!(micros(&outcome["amount"]), amount, "{id}");
        assert_eq!(outcome["priceVersion"], version.as_str(), "{id}");
        assert_eq!(outcome["meter"], meter, "{id}");
        assert_eq!(micros(&outcome["quantity"]), quantity, "{id}");
        let dims = if model.is_empty() {
            json!({})
        } else {
            json!({ "model": model })
        };
        assert_eq!(outcome["dimensions"], dims, "{id}");
    }
    // The half-to-even cases this list was chosen for: 308.5 is 308, 0.25 is 0, 600.15 is 600.
    assert_eq!(rated(1_234, 250, 1_000), 308);
    assert_eq!(rated(10, 25, 1_000), 0);
    assert_eq!(rated(4_000, 150, 1_000), 600);

    // The statement: one usage line per day and meter, the prepaid credit, and a total per kind.
    let (year, month, _) = from.utc_day().ymd();
    let lines = d.statement("tenant-ex-1", year, month).await;
    let usage_lines: BTreeMap<(String, String), i64> = lines
        .iter()
        .filter(|l| l["line"] == "line" && l["kind"] == "usage")
        .map(|l| {
            (
                (
                    l["day"].as_str().unwrap().to_owned(),
                    l["reference"].as_str().unwrap().to_owned(),
                ),
                -micros(&l["amountMicros"]),
            )
        })
        .collect();
    assert_eq!(usage_lines, per_meter);
    let total = |kind: &str| {
        lines
            .iter()
            .find(|l| l["line"] == "total" && l["kind"] == kind)
            .map(|l| micros(&l["amountMicros"]))
    };
    assert_eq!(total("usage"), Some(-expected_total));
    assert_eq!(total("adjustment"), Some(1_000_000));
    assert_eq!(d.net("tenant-ex-1").await, 1_000_000 - expected_total);
}
