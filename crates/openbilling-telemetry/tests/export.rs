//! The exporter against a collector in the process: what is recorded arrives as OTLP/JSON, with the headers from
//! the file, the resource, and nothing the allowlist drops. One test: it installs the process's subscriber.

#![allow(clippy::unwrap_used, clippy::expect_used)]

use std::collections::BTreeMap;
use std::time::Duration;

use openbilling_telemetry::testkit::TestCollector;
use openbilling_telemetry::{Counter, Settings, global, start};
use tracing::{info, info_span};

#[tokio::test]
async fn spans_metrics_and_logs_reach_the_collector() {
    let collector = TestCollector::start().await;
    let vars: BTreeMap<&str, String> = BTreeMap::from([
        ("OTEL_EXPORTER_OTLP_ENDPOINT", collector.endpoint()),
        ("OTEL_BSP_SCHEDULE_DELAY", "50".to_owned()),
        ("OTEL_METRIC_EXPORT_INTERVAL", "50".to_owned()),
        (
            "OTEL_RESOURCE_ATTRIBUTES",
            "deployment.environment.name=ci,service.instance.id=pod-7".to_owned(),
        ),
        ("OPENBILLING_OTEL_HEADERS_FILE", "headers".to_owned()),
    ]);
    let settings = Settings::from_lookup(
        |name| vars.get(name).cloned(),
        "9.9.9",
        |_| Some("X-Collector-Key: k-12345\n".to_owned()),
    )
    .unwrap()
    .unwrap();
    let guard = start(&settings, global()).expect("starts");
    // A second start in one process is refused, not a panic.
    assert!(start(&settings, global()).is_err());

    global().add(Counter::DrainRuns, &[("result", "ok")], 4);
    {
        let span = info_span!(
            target: "openbilling_test",
            "http.server.request",
            http.request.method = "GET",
            owner = "acct-must-not-leave",
        );
        let _in = span.enter();
        info!(target: "openbilling_test", "a log line");
    }
    assert!(
        collector
            .wait_for(Duration::from_secs(5), |c| {
                !c.spans().is_empty() && !c.metrics().is_empty() && !c.logs().is_empty()
            })
            .await
    );
    guard.shutdown(Duration::from_secs(5)).await;

    let span = &collector.spans()[0];
    assert_eq!(span["name"], "http.server.request");
    assert_eq!(span["kind"], 2);
    assert_eq!(collector.logs()[0]["body"]["stringValue"], "a log line");
    assert!(
        collector
            .metrics()
            .iter()
            .any(|m| m["name"] == "openbilling.drain.runs")
    );
    for resource in collector.resources() {
        let text = resource.to_string();
        for want in ["openbilling", "9.9.9", "ci", "pod-7"] {
            assert!(text.contains(want), "{want} in {text}");
        }
    }
    assert!(
        collector
            .headers()
            .iter()
            .all(|h| h.get("x-collector-key").is_some_and(|v| v == "k-12345"))
    );
    assert!(!collector.bodies().join("").contains("acct-must-not-leave"));
}
