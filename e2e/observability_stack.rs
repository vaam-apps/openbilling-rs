//! Scenario: the observability stack. The real `openbilling serve` exports over OTLP/HTTP to a real OpenTelemetry
//! Collector, whose Prometheus exporter a real Prometheus scrapes, and a real Grafana reads Prometheus with the
//! dashboards of `deploy/dashboards` provisioned unmodified. The test asks Prometheus and Grafana, over their HTTP
//! APIs, what the dashboards and the shipped alert rules ask them.
//!
//! Gated: it needs the stack `tools/observability/run.sh` starts (which sets `OPENBILLING_OBS_OTLP_URL`,
//! `OPENBILLING_OBS_PROMETHEUS_URL`, `OPENBILLING_OBS_GRAFANA_URL` and `OPENBILLING_OBS_GRAFANA_PASSWORD`) and skips
//! without it; under `OPENBILLING_OBS_REQUIRE=1` it fails instead. A database is also needed, as for every scenario.

use std::collections::BTreeSet;
use std::path::PathBuf;
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use openbilling_telemetry::exposed_names;
use serde_json::{Value, json};

use crate::support::{Deployment, now, priced};

const OWNER: &str = "tenant-obs";
const MEMBER: &str = "member-obs";

/// Panels whose query is a ratio or a rate over a counter that this workload never records, so that Prometheus has
/// no series for them and the panel shows "No data" by design (a series that has never counted does not exist,
/// ADR 0007). Every other panel must answer with a value.
const EXPECTED_EMPTY: [&str; 4] = [
    "5xx ratio",
    "Drain error ratio",
    "Telemetry dropped",
    "Refused share of getAllowed",
];

/// Metrics the dashboards query that have no series after this workload, for the same reason.
const EXPECTED_ABSENT: [&str; 1] = ["openbilling_telemetry_dropped_total"];

/// What the files of `deploy/dashboards` hold today (two dashboards): the panels that have targets, and the targets.
/// A floor, so that a test which found nothing to check cannot pass; raise it with the dashboards.
const MIN_DASHBOARDS: usize = 2;
const MIN_PANELS: usize = 26;
const MIN_TARGETS: usize = 31;

struct Stack {
    http: reqwest::Client,
    prometheus: String,
    grafana: String,
    password: String,
}

fn setting(name: &str) -> Option<String> {
    std::env::var(name).ok().filter(|v| !v.is_empty())
}

impl Stack {
    async fn query(&self, expr: &str) -> Vec<Value> {
        let answer = self
            .http
            .get(format!("{}/api/v1/query", self.prometheus))
            .query(&[("query", expr)])
            .send()
            .await
            .unwrap();
        let body: Value = answer.json_value().await;
        assert_eq!(body["status"], "success", "{expr}: {body}");
        body["data"]["result"]
            .as_array()
            .cloned()
            .unwrap_or_default()
    }

    async fn value(&self, expr: &str) -> Option<f64> {
        self.query(expr)
            .await
            .first()
            .and_then(|r| r["value"][1].as_str())
            .and_then(|v| v.parse().ok())
    }

    async fn names(&self) -> BTreeSet<String> {
        let answer = self
            .http
            .get(format!("{}/api/v1/label/__name__/values", self.prometheus))
            .send()
            .await
            .unwrap();
        let body: Value = answer.json_value().await;
        body["data"]
            .as_array()
            .unwrap()
            .iter()
            .map(|n| n.as_str().unwrap().to_owned())
            .collect()
    }

    async fn grafana_get(&self, path: &str) -> (u16, Value) {
        let answer = self
            .http
            .get(format!("{}{path}", self.grafana))
            .basic_auth("admin", Some(&self.password))
            .send()
            .await
            .unwrap();
        let status = answer.status().as_u16();
        (status, answer.json_value().await)
    }
}

trait JsonValue {
    async fn json_value(self) -> Value;
}

impl JsonValue for reqwest::Response {
    async fn json_value(self) -> Value {
        let text = self.text().await.unwrap();
        serde_json::from_str(&text).unwrap_or_else(|_| panic!("not JSON: {text}"))
    }
}

async fn wait_until<F, Fut>(what: &str, within: Duration, mut check: F)
where
    F: FnMut() -> Fut,
    Fut: Future<Output = bool>,
{
    let deadline = Instant::now() + within;
    while !check().await {
        assert!(
            Instant::now() < deadline,
            "{what} did not hold in {within:?}"
        );
        tokio::time::sleep(Duration::from_millis(500)).await;
    }
}

fn dashboards() -> Vec<(String, Value)> {
    let dir = PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../../deploy/dashboards");
    let mut found: Vec<_> = std::fs::read_dir(dir)
        .unwrap()
        .filter_map(Result::ok)
        .filter(|e| e.path().extension().is_some_and(|x| x == "json"))
        .map(|e| {
            let text = std::fs::read_to_string(e.path()).unwrap();
            (
                e.file_name().to_string_lossy().into_owned(),
                serde_json::from_str(&text).unwrap(),
            )
        })
        .collect();
    found.sort_by(|a, b| a.0.cmp(&b.0));
    found
}

fn metric_tokens(expr: &str) -> BTreeSet<String> {
    expr.split(|c: char| !(c.is_ascii_alphanumeric() || c == '_'))
        .filter(|t| {
            ["openbilling_", "http_server_", "db_client_"]
                .iter()
                .any(|p| t.starts_with(p))
        })
        .map(str::to_owned)
        .collect()
}

/// Every panel's targets as Grafana's query API takes them, with the dashboard's variables filled in.
fn panel_queries(dashboard: &Value) -> Vec<(String, Vec<Value>)> {
    dashboard["panels"]
        .as_array()
        .unwrap()
        .iter()
        .filter(|p| p["type"] != "row")
        .filter_map(|panel| {
            let targets = panel["targets"].as_array().filter(|t| !t.is_empty())?;
            let queries = targets
                .iter()
                .map(|t| {
                    json!({
                        "refId": t["refId"],
                        "datasource": {"type": "prometheus", "uid": "prometheus"},
                        "expr": t["expr"].as_str().unwrap().replace("$job", "openbilling"),
                        "range": true,
                        "instant": false,
                        "intervalMs": 2000,
                        "maxDataPoints": 300
                    })
                })
                .collect();
            Some((panel["title"].as_str().unwrap().to_owned(), queries))
        })
        .collect()
}

/// Whether any frame of a Grafana query answer holds a number.
fn has_value(answer: &Value) -> bool {
    answer["results"]
        .as_object()
        .into_iter()
        .flat_map(|results| results.values())
        .flat_map(|r| r["frames"].as_array().cloned().unwrap_or_default())
        .any(|frame| {
            frame["data"]["values"]
                .as_array()
                .and_then(|columns| columns.get(1))
                .and_then(Value::as_array)
                .is_some_and(|values| values.iter().any(Value::is_number))
        })
}

#[tokio::test(flavor = "multi_thread")]
async fn the_real_collector_prometheus_and_grafana_answer_what_the_dashboards_and_alerts_ask() {
    let (Some(prometheus), Some(grafana)) = (
        setting("OPENBILLING_OBS_PROMETHEUS_URL"),
        setting("OPENBILLING_OBS_GRAFANA_URL"),
    ) else {
        assert!(
            setting("OPENBILLING_OBS_REQUIRE").is_none(),
            "OPENBILLING_OBS_REQUIRE is set but the stack's URLs are not: run tools/observability/run.sh"
        );
        eprintln!(
            "skipped: OPENBILLING_OBS_PROMETHEUS_URL and OPENBILLING_OBS_GRAFANA_URL are not set"
        );
        return;
    };
    let otlp =
        setting("OPENBILLING_OBS_OTLP_URL").unwrap_or_else(|| "http://127.0.0.1:4318".to_owned());
    let extra = vec![
        ("OTEL_EXPORTER_OTLP_ENDPOINT".to_owned(), otlp),
        ("OTEL_SERVICE_NAME".to_owned(), "openbilling".to_owned()),
        ("OTEL_METRIC_EXPORT_INTERVAL".to_owned(), "1000".to_owned()),
    ];
    let Some(deployment) = Deployment::start(1, &extra).await else {
        return;
    };
    let d = &deployment;
    let stack = Stack {
        http: reqwest::Client::builder()
            .timeout(Duration::from_secs(60))
            .build()
            .unwrap(),
        prometheus,
        grafana,
        password: setting("OPENBILLING_OBS_GRAFANA_PASSWORD").unwrap_or_default(),
    };

    // Phase A: a workload that exercises intake, the drain, the ledger and getAllowed.
    d.configure().await;
    d.open("acct-obs", OWNER).await;
    d.grant("adjustment", "credit-obs", OWNER, 900_000_000)
        .await;
    let t = now();
    let events: Vec<Value> = (0..50)
        .map(|i| priced(&format!("evt-obs-{i}"), OWNER, MEMBER, 1_000_000 + i, t))
        .collect();
    d.usage(events).await.ok();
    d.wait_decided(50, Duration::from_secs(60)).await;
    for _ in 0..5 {
        d.allowed(OWNER, MEMBER).await;
    }

    // The gauges are sampled every 15 s: wait for them to reach Prometheus through the Collector.
    wait_until(
        "the gauges reach Prometheus",
        Duration::from_secs(90),
        || async {
            let names = stack.names().await;
            [
                "openbilling_queue_backlog",
                "openbilling_drain_lag_seconds",
                "db_client_connection_count",
                "openbilling_intake_events_total",
                "openbilling_telemetry_exports_total",
                "http_server_request_duration_seconds_count",
            ]
            .iter()
            .all(|n| names.contains(*n))
        },
    )
    .await;

    // Names: every series the service made is a name the code lists, with the `job` the dashboards select by.
    let exposed: BTreeSet<String> = exposed_names().into_iter().collect();
    for name in stack.names().await {
        if ["openbilling_", "http_server_", "db_client_"]
            .iter()
            .any(|p| name.starts_with(p))
        {
            assert!(
                exposed.contains(&name),
                "Prometheus has {name}, which exposed_names() does not list"
            );
        }
    }
    let lag = stack.query("openbilling_drain_lag_seconds").await;
    assert!(!lag.is_empty(), "no drain lag series");
    assert!(
        lag.iter().all(|r| r["metric"]["job"] == "openbilling"),
        "the job label is not the service name: {lag:?}"
    );

    // Every metric a dashboard queries has a series, bar the ones that cannot have one yet.
    let present = stack.names().await;
    let all = dashboards();
    assert!(
        all.len() >= MIN_DASHBOARDS,
        "{} dashboards found in deploy/dashboards, expected at least {MIN_DASHBOARDS}",
        all.len()
    );
    let mut targets_seen = 0;
    let mut absent_used = BTreeSet::new();
    for (file, dashboard) in &all {
        for panel in dashboard["panels"].as_array().unwrap() {
            for target in panel["targets"].as_array().into_iter().flatten() {
                targets_seen += 1;
                for name in metric_tokens(target["expr"].as_str().unwrap()) {
                    if name == "openbilling_errors_total" {
                        continue; // phase B
                    }
                    if EXPECTED_ABSENT.contains(&name.as_str()) {
                        assert!(
                            !present.contains(&name),
                            "{name} is no longer absent: remove it from EXPECTED_ABSENT"
                        );
                        absent_used.insert(name);
                        continue;
                    }
                    assert!(
                        present.contains(&name),
                        "{file}: panel {:?} queries {name}, which Prometheus does not have",
                        panel["title"]
                    );
                }
            }
        }
    }
    assert!(
        targets_seen >= MIN_TARGETS,
        "{targets_seen} targets checked, expected at least {MIN_TARGETS}"
    );
    for name in EXPECTED_ABSENT {
        assert!(
            absent_used.contains(name),
            "EXPECTED_ABSENT lists {name}, which no dashboard queries"
        );
    }

    // Counters that must not exist after a clean run, and exports that must have worked.
    // A name the code does not list would also be "empty": each must be a real instrument first.
    for name in [
        "openbilling_partitions_dropped_total",
        "openbilling_telemetry_exports_total",
        "openbilling_errors_total",
    ] {
        assert!(
            exposed.contains(name),
            "{name} is not in exposed_names(): the assertion below would pass on a misspelling"
        );
    }
    assert!(
        stack
            .query(r#"openbilling_partitions_dropped_total"#)
            .await
            .is_empty()
    );
    assert!(
        stack
            .query(r#"openbilling_telemetry_exports_total{result="failed"}"#)
            .await
            .is_empty(),
        "an export failed"
    );
    assert!(
        stack
            .value(r#"sum(openbilling_telemetry_exports_total{signal="metrics",result="ok"})"#)
            .await
            .is_some_and(|v| v > 0.0),
        "no metrics export is counted ok"
    );
    assert!(
        stack
            .query(r#"openbilling_errors_total{class="conflict"}"#)
            .await
            .is_empty()
    );

    // Phase B: the same event id with another amount is a conflict, and the counter appears.
    let again = d
        .usage(vec![priced("evt-obs-0", OWNER, MEMBER, 7_000_000, t)])
        .await;
    assert_eq!(again.status, 409, "{}", again.text);
    wait_until(
        "the conflict is counted",
        Duration::from_secs(20),
        || async {
            stack
                .value(r#"sum(openbilling_errors_total{class="conflict"})"#)
                .await
                .is_some_and(|v| v >= 1.0)
        },
    )
    .await;
    // A ratio of two rates is 0/0 (no value) once traffic stops: keep a little going, refused and answered, so the
    // last points of the range have one.
    for _ in 0..12 {
        d.allowed(OWNER, MEMBER).await;
        let _ = d
            .usage(vec![priced("evt-obs-0", OWNER, MEMBER, 7_000_000, t)])
            .await;
        tokio::time::sleep(Duration::from_secs(1)).await;
    }

    // The shipped alert rules load and evaluate.
    let rules: Value = stack
        .http
        .get(format!("{}/api/v1/rules", stack.prometheus))
        .send()
        .await
        .unwrap()
        .json_value()
        .await;
    let mut seen = 0;
    for group in rules["data"]["groups"].as_array().unwrap() {
        for rule in group["rules"].as_array().unwrap() {
            seen += 1;
            assert_eq!(rule["health"], "ok", "{}: {rule}", rule["name"]);
            assert!(
                rule["lastError"].as_str().is_none_or(str::is_empty),
                "{}: {rule}",
                rule["name"]
            );
        }
    }
    assert!(seen >= 16, "{seen} alert rules loaded");

    // Grafana: each dashboard is provisioned with its panels, and every target answers.
    let ms = |at: SystemTime| at.duration_since(UNIX_EPOCH).unwrap().as_millis();
    let to = SystemTime::now();
    let from = to - Duration::from_secs(300);
    let mut panels_checked = 0;
    let mut empty_used = BTreeSet::new();
    for (file, dashboard) in &all {
        let uid = dashboard["uid"].as_str().unwrap();
        let (status, found) = stack
            .grafana_get(&format!("/api/dashboards/uid/{uid}"))
            .await;
        assert_eq!(status, 200, "{file}: {found}");
        assert_eq!(
            found["dashboard"]["panels"].as_array().unwrap().len(),
            dashboard["panels"].as_array().unwrap().len(),
            "{file}: panels in Grafana and in the file"
        );
        for (title, queries) in panel_queries(dashboard) {
            panels_checked += 1;
            let body = json!({
                "queries": queries,
                "from": ms(from).to_string(),
                "to": ms(to).to_string()
            });
            let answer = stack
                .http
                .post(format!("{}/api/ds/query", stack.grafana))
                .basic_auth("admin", Some(&stack.password))
                .header("content-type", "application/json")
                .body(body.to_string())
                .send()
                .await
                .unwrap();
            let status = answer.status().as_u16();
            let answer = answer.json_value().await;
            assert_eq!(status, 200, "{file}: {title:?}: {answer}");
            for (id, result) in answer["results"].as_object().unwrap() {
                assert!(
                    result["error"].is_null(),
                    "{file}: {title:?} {id}: {result}"
                );
            }
            if EXPECTED_EMPTY.contains(&title.as_str()) {
                // The list stays honest: a panel that now has a value leaves it.
                assert!(
                    !has_value(&answer),
                    "{file}: panel {title:?} is no longer empty: {answer}"
                );
                empty_used.insert(title);
                continue;
            }
            assert!(
                has_value(&answer),
                "{file}: panel {title:?} has no value:\n{}",
                json!({"queries": queries, "answer": answer})
            );
        }
    }
    assert!(
        panels_checked >= MIN_PANELS,
        "{panels_checked} panels checked, expected at least {MIN_PANELS}"
    );
    for title in EXPECTED_EMPTY {
        assert!(
            empty_used.contains(title),
            "EXPECTED_EMPTY lists {title:?}, which no panel has"
        );
    }
}
