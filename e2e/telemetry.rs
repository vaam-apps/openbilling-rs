//! Scenario: telemetry. The real `openbilling serve`, with `OTEL_EXPORTER_OTLP_ENDPOINT` set to a collector in the
//! test process, takes usage in and drains it; the collector receives the spans of the requests and of the drain
//! loop, the metrics the dashboards read (the queue and pool gauges among them, read by the sampler the service
//! runs), and not a byte that names a token, an owner, a member, an event or an amount. Another deployment of the
//! same binary with no endpoint sends nothing and works as before (every other scenario is that).

use std::time::Duration;

use openbilling_telemetry::testkit::TestCollector;
use serde_json::Value;

use crate::support::{Deployment, now, priced};

const OWNER: &str = "tenant-otel-sentinel";
const MEMBER: &str = "member-otel-sentinel";

fn metric_names(collector: &TestCollector) -> Vec<String> {
    collector
        .metrics()
        .iter()
        .filter_map(|m| m["name"].as_str().map(str::to_owned))
        .collect()
}

#[tokio::test(flavor = "multi_thread")]
async fn a_service_with_an_endpoint_exports_its_spans_and_metrics_and_names_no_one() {
    let collector = TestCollector::start().await;
    let extra = vec![
        (
            "OTEL_EXPORTER_OTLP_ENDPOINT".to_owned(),
            collector.endpoint(),
        ),
        ("OTEL_SERVICE_NAME".to_owned(), "openbilling-e2e".to_owned()),
        ("OTEL_BSP_SCHEDULE_DELAY".to_owned(), "100".to_owned()),
        ("OTEL_METRIC_EXPORT_INTERVAL".to_owned(), "200".to_owned()),
    ];
    let Some(deployment) = Deployment::start(1, &extra).await else {
        return;
    };
    let d = &deployment;
    d.configure().await;
    d.open("acct-otel-sentinel", OWNER).await;
    d.grant("adjustment", "credit-otel-sentinel", OWNER, 987_654_321)
        .await;
    let t = now();
    let events = vec![
        priced("evt-otel-sentinel-1", OWNER, MEMBER, 123_456_789, t),
        priced("evt-otel-sentinel-2", OWNER, MEMBER, 223_456_789, t),
    ];
    d.usage(events).await.ok();
    d.wait_decided(2, Duration::from_secs(30)).await;
    let token = d.producer_token();

    let ready = collector
        .wait_for(Duration::from_secs(20), |c| {
            let spans = c.spans();
            let names = metric_names(c);
            spans.iter().any(|s| s["name"] == "drain.cycle")
                && spans.iter().any(|s| s["name"] == "http.server.request")
                && [
                    "openbilling.drain.events",
                    "openbilling.intake.events",
                    "openbilling.queue.backlog",
                    "db.client.connection.count",
                    "openbilling.drain.lag",
                ]
                .iter()
                .all(|want| names.iter().any(|n| n == want))
        })
        .await;
    assert!(
        ready,
        "spans {:?}, metrics {:?}\n{}",
        collector
            .spans()
            .iter()
            .map(|s| s["name"].clone())
            .collect::<Vec<Value>>(),
        metric_names(&collector),
        d.services[0].log()
    );

    // The resource says which service this is, and which release.
    let resources = collector
        .resources()
        .iter()
        .map(Value::to_string)
        .collect::<String>();
    assert!(resources.contains("openbilling-e2e") && resources.contains(env!("CARGO_PKG_VERSION")));
    // The drain pass that took the events has its counts as attributes.
    let pass = collector
        .spans()
        .into_iter()
        .find(|s| {
            s["name"] == "drain.cycle"
                && s["attributes"]
                    .as_array()
                    .is_some_and(|a| a.iter().any(|a| a["key"] == "openbilling.events.applied"))
        })
        .expect("a drain pass that decided events");
    assert_eq!(pass["kind"], 1);

    let everything = collector.bodies().join("\n");
    for forbidden in [
        OWNER,
        MEMBER,
        "acct-otel",
        "evt-otel",
        "credit-otel",
        token.as_str(),
        "Bearer",
        "123456789",
        "223456789",
        "987654321",
    ] {
        assert!(
            !everything.contains(forbidden),
            "{forbidden} left the process"
        );
    }
}
