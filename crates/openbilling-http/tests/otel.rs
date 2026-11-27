//! OpenTelemetry end to end: one scripted flow (configure, open, credit, intake, drain, reads, a refusal, a bad
//! token) on a real ledger, exported to a collector in the process. The collector must receive the spans of the
//! routes, the procedures and the database calls, the metrics the dashboards read, and a log; and **nothing it
//! receives may hold a token, an owner, an account, a member, an event or credit id, an idempotency key, or an
//! amount**: the test scans every request body for the values it sent. One test: it installs the subscriber.
//! Needs `OPENBILLING_TEST_DATABASE_URL`; skips without it.

#![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]

mod support;

use std::collections::BTreeMap;
use std::time::Duration;

use cratestack::axum::http::{Method, StatusCode};
use openbilling_core::{LedgerStore, Timestamp};
use openbilling_telemetry::testkit::TestCollector;
use openbilling_telemetry::{Settings, global, start};
use serde_json::{Value, json};
use support::{OPERATOR, OWNER_1, PRODUCER, app, call, fresh, procedure};

const OWNER: &str = "owner-sentinel-1";
const ACCOUNT: &str = "acct-sentinel-1";
const MEMBER: &str = "member-sentinel";
const AMOUNT: &str = "987654321";

fn now_text() -> String {
    let micros = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap()
        .as_micros();
    Timestamp::from_unix_micros(i64::try_from(micros).unwrap())
        .unwrap()
        .to_rfc3339_micros()
}

/// The data points of the latest export of the metric `name`: (attributes, point).
fn points(collector: &TestCollector, name: &str) -> Vec<(BTreeMap<String, String>, Value)> {
    let Some(metric) = collector.metrics().into_iter().rfind(|m| m["name"] == name) else {
        return Vec::new();
    };
    let body = metric
        .get("sum")
        .or_else(|| metric.get("histogram"))
        .or_else(|| metric.get("gauge"))
        .unwrap();
    body["dataPoints"]
        .as_array()
        .unwrap()
        .iter()
        .map(|point| {
            let labels = point["attributes"]
                .as_array()
                .unwrap()
                .iter()
                .map(|a| {
                    (
                        a["key"].as_str().unwrap().to_owned(),
                        a["value"]["stringValue"].as_str().unwrap().to_owned(),
                    )
                })
                .collect();
            (labels, point.clone())
        })
        .collect()
}

/// A counter's value for the labels given (a subset of the point's labels).
fn counter(collector: &TestCollector, name: &str, want: &[(&str, &str)]) -> u64 {
    points(collector, name)
        .iter()
        .filter(|(labels, _)| {
            want.iter()
                .all(|(k, v)| labels.get(*k).is_some_and(|got| got == v))
        })
        .map(|(_, point)| point["asInt"].as_str().unwrap().parse::<u64>().unwrap())
        .sum()
}

fn attr<'a>(span: &'a Value, key: &str) -> Option<&'a str> {
    span["attributes"]
        .as_array()?
        .iter()
        .find(|a| a["key"] == key)
        .and_then(|a| a["value"]["stringValue"].as_str())
}

#[tokio::test]
async fn one_flow_gives_spans_metrics_and_a_log_and_leaks_nothing() {
    let Some((store, ledger)) = fresh().await else {
        return;
    };
    let collector = TestCollector::start().await;
    let vars: BTreeMap<&str, String> = BTreeMap::from([
        ("OTEL_EXPORTER_OTLP_ENDPOINT", collector.endpoint()),
        ("OTEL_BSP_SCHEDULE_DELAY", "50".to_owned()),
        ("OTEL_METRIC_EXPORT_INTERVAL", "50".to_owned()),
        ("OTEL_SERVICE_NAME", "openbilling-test".to_owned()),
    ]);
    let settings = Settings::from_lookup(|n| vars.get(n).cloned(), "0.0.0-test", |_| None)
        .unwrap()
        .unwrap();
    let guard = start(&settings, global()).unwrap();

    let app = app(ledger.clone(), None, store.pool().clone());
    tracing::info!(target: "openbilling_http", "the scripted flow starts");

    let config = json!({
        "currencies": ["EUR"], "labels": ["main"],
        "lotOrder": [
            {"kind": "allowance", "soonestExpiryFirst": true},
            {"kind": "promotion", "soonestExpiryFirst": true},
            {"kind": "adjustment", "soonestExpiryFirst": false},
            {"kind": "payment", "soonestExpiryFirst": false}
        ]
    });
    let ok = |answer: support::Answer| {
        assert!(
            answer.status.is_success(),
            "{}: {}",
            answer.status,
            answer.text
        );
    };
    ok(procedure(
        &app,
        "configure",
        Some(OPERATOR),
        Some("key-sentinel-cfg"),
        config,
    )
    .await);
    ok(procedure(
        &app,
        "openAccount",
        Some(OPERATOR),
        Some("key-sentinel-open"),
        json!({"accountId": ACCOUNT, "owner": OWNER, "currency": "EUR", "label": "main"}),
    )
    .await);
    ok(procedure(
        &app,
        "grantCredit",
        Some(OPERATOR),
        Some("key-sentinel-grant"),
        json!({"creditId": "credit-sentinel", "source": "promotion", "owner": OWNER,
               "currency": "EUR", "label": "main", "amountMicros": "2000000011"}),
    )
    .await);

    let at = now_text();
    let event = |id: &str, owner: &str| {
        json!({"eventId": id, "owner": owner, "member": MEMBER, "currency": "EUR",
               "amountMicros": AMOUNT, "occurredAt": at})
    };
    // Two events for the owner, one for an owner with no account (the drain will refuse it).
    ok(procedure(
        &app,
        "recordUsage",
        Some(PRODUCER),
        Some("key-sentinel-usage"),
        json!({"events": [
            event("evt-sentinel-1", OWNER),
            event("evt-sentinel-2", OWNER),
            event("evt-sentinel-3", "owner-sentinel-ghost"),
        ]}),
    )
    .await);
    // A batch the ledger refuses: an event from before the window. The answer is 422 and nothing is written.
    let old = json!({"eventId": "evt-sentinel-old", "owner": OWNER, "member": MEMBER, "currency": "EUR",
                     "amountMicros": AMOUNT, "occurredAt": "2020-01-01T00:00:00.000000Z"});
    let refused = procedure(
        &app,
        "recordUsage",
        Some(PRODUCER),
        Some("key-sentinel-old"),
        json!({"events": [old]}),
    )
    .await;
    assert_eq!(
        refused.status,
        StatusCode::UNPROCESSABLE_ENTITY,
        "{}",
        refused.text
    );

    let report = ledger.drain(500).await.unwrap();
    assert_eq!((report.decided.len(), report.applied()), (3, 2));

    // getAllowed: the owner with credit is allowed; an owner with none is refused on the balance.
    let allowed = |owner: &'static str| {
        procedure(
            &app,
            "getAllowed",
            Some(OPERATOR),
            None,
            json!({"owner": owner, "currency": "EUR", "member": MEMBER}),
        )
    };
    assert_eq!(allowed(OWNER).await.body["allowed"], true);
    assert_eq!(allowed("owner-sentinel-none").await.body["allowed"], false);
    // A bad token: 401 from CrateStack, seen as a 4xx in the request metrics.
    let denied = call(
        &app,
        Method::GET,
        "/v1/accounts",
        Some("token-nobody"),
        None,
        None,
    )
    .await;
    assert_eq!(denied.status, StatusCode::UNAUTHORIZED);
    let as_owner = call(&app, Method::GET, "/v1/accounts", Some(OWNER_1), None, None).await;
    assert!(as_owner.status.is_success() || as_owner.status.is_client_error());
    assert_eq!(
        call(&app, Method::GET, "/healthz", None, None, None)
            .await
            .status,
        StatusCode::OK
    );

    ledger.prepare_partitions().await.unwrap();
    ledger.sample_metrics().await;
    tokio::time::sleep(Duration::from_millis(200)).await;
    let dropped = guard.dropped();
    guard.shutdown(Duration::from_secs(5)).await;
    assert!(
        collector
            .wait_for(Duration::from_secs(2), |c| !c.logs().is_empty())
            .await
    );

    // ---- spans ----
    let spans = collector.spans();
    let named = |name: &str| spans.iter().filter(|s| s["name"] == name).count();
    assert!(
        named("http.server.request") >= 8,
        "{}",
        named("http.server.request")
    );
    assert!(named("cratestack_procedure_route") >= 6);
    assert!(named("cratestack_procedure_invoke_with_db") >= 6);
    let db: Vec<&str> = spans
        .iter()
        .filter(|s| s["name"] == "db.query")
        .filter_map(|s| attr(s, "db.operation.name"))
        .collect();
    for operation in [
        "configure",
        "open_account",
        "credit",
        "intake",
        "drain",
        "prepare_partitions",
        "queue_stats",
    ] {
        assert!(
            db.contains(&operation),
            "no db span for {operation}: {db:?}"
        );
    }
    // A database span is a child, in the trace, of the request that caused it.
    let intake = spans
        .iter()
        .find(|s| s["name"] == "db.query" && attr(s, "db.operation.name") == Some("intake"))
        .unwrap();
    let request = spans
        .iter()
        .find(|s| s["name"] == "http.server.request" && s["traceId"] == intake["traceId"])
        .expect("the intake's trace has its request");
    assert_eq!(request["kind"], 2);
    assert_eq!(attr(request, "http.route"), Some("/v1/$procs/{name}"));
    assert!(
        spans
            .iter()
            .any(|s| attr(s, "cratestack.procedure") == Some("getAllowed"))
    );
    assert!(
        spans.iter().any(|s| s["status"]["code"] == 2),
        "a failed request marks its span"
    );
    assert!(
        spans
            .iter()
            .all(|s| attr(s, "http.route") != Some("/healthz")),
        "probes are metrics only"
    );

    // ---- metrics ----
    assert_eq!(
        counter(
            &collector,
            "openbilling.intake.events",
            &[("result", "accepted")]
        ),
        3
    );
    assert_eq!(
        counter(
            &collector,
            "openbilling.intake.events",
            &[("result", "refused"), ("reason", "too_old")]
        ),
        1
    );
    assert_eq!(
        counter(
            &collector,
            "openbilling.drain.events",
            &[("outcome", "applied")]
        ),
        2
    );
    assert_eq!(
        counter(
            &collector,
            "openbilling.drain.events",
            &[("outcome", "refused"), ("reason", "unknown_account")]
        ),
        1
    );
    assert_eq!(
        counter(
            &collector,
            "openbilling.ledger.postings",
            &[("kind", "credit")]
        ),
        1
    );
    assert_eq!(
        counter(
            &collector,
            "openbilling.ledger.postings",
            &[("kind", "usage_debit")]
        ),
        2
    );
    assert_eq!(
        counter(&collector, "openbilling.drain.runs", &[("result", "ok")]),
        1
    );
    assert_eq!(
        counter(
            &collector,
            "openbilling.allowed.checks",
            &[("verdict", "allowed"), ("check", "none")]
        ),
        1
    );
    assert_eq!(
        counter(
            &collector,
            "openbilling.allowed.checks",
            &[
                ("verdict", "refused"),
                ("check", "balance"),
                ("mode", "at_or_above")
            ]
        ),
        1
    );
    assert!(
        counter(
            &collector,
            "openbilling.errors",
            &[("class", "invalid"), ("source", "api")]
        ) >= 1
    );
    let requests = points(&collector, "http.server.request.duration");
    assert!(
        requests
            .iter()
            .any(|(l, _)| l["status"] == "4xx" && l["route"] == "/v1/accounts")
    );
    assert!(
        requests
            .iter()
            .any(|(l, _)| l["procedure"] == "recordUsage" && l["status"] == "2xx")
    );
    assert!(
        requests.iter().any(|(l, _)| l["route"] == "/healthz"),
        "probes count in the metrics"
    );
    let ops: Vec<String> = points(&collector, "db.client.operation.duration")
        .into_iter()
        .map(|(l, _)| l["operation"].clone())
        .collect();
    assert!(
        ops.contains(&"intake".to_owned()) && ops.contains(&"drain".to_owned()),
        "{ops:?}"
    );
    for gauge in [
        "openbilling.queue.backlog",
        "openbilling.queue.oldest_age",
        "db.client.connection.count",
        "db.client.connection.max",
        "openbilling.drain.lag",
    ] {
        assert!(!points(&collector, gauge).is_empty(), "{gauge}");
    }
    // The queue is drained: nothing is waiting.
    assert_eq!(
        points(&collector, "openbilling.queue.backlog")[0].1["asDouble"],
        0.0
    );

    // ---- logs and the resource ----
    let logs = collector.logs();
    assert!(
        logs.iter()
            .any(|l| l["body"]["stringValue"] == "the scripted flow starts")
    );
    let resources = collector
        .resources()
        .iter()
        .map(Value::to_string)
        .collect::<String>();
    assert!(resources.contains("openbilling-test") && resources.contains("0.0.0-test"));

    // ---- nothing that identifies a person or a secret left the process ----
    let everything = collector.bodies().join("\n");
    for forbidden in [
        OWNER,
        ACCOUNT,
        MEMBER,
        AMOUNT,
        "owner-sentinel",
        "evt-sentinel",
        "credit-sentinel",
        "key-sentinel",
        "token-operator",
        "token-producer",
        "token-owner",
        "token-nobody",
        "Bearer",
        "idempotency",
        "2000000011",
    ] {
        assert!(
            !everything.contains(forbidden),
            "{forbidden} left the process"
        );
    }
    assert_eq!(dropped, 0, "the queue never overflowed");
    store.close().await;
}
