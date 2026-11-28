//! Scenario: the observability stack. The real `openbilling serve` exports over OTLP/HTTP to a real OpenTelemetry
//! Collector, whose Prometheus exporter a real Prometheus scrapes (leg `scrape`) or whose `otlphttp` exporter pushes to
//! Prometheus's OTLP receiver (leg `otlp`, `OBS_LEG` of run.sh; the assertions are the same), and a real Grafana reads
//! Prometheus with the dashboards of `deploy/dashboards` provisioned unmodified. The test asks Prometheus and Grafana, over their HTTP
//! APIs, what the dashboards and the shipped alert rules ask them.
//!
//! Gated: it needs the stack `tools/observability/run.sh` starts (which sets `OPENBILLING_OBS_OTLP_URL`,
//! `OPENBILLING_OBS_PROMETHEUS_URL`, `OPENBILLING_OBS_GRAFANA_URL` and `OPENBILLING_OBS_GRAFANA_PASSWORD`) and skips
//! without it; under `OPENBILLING_OBS_REQUIRE=1` it fails instead. A database is also needed, as for every scenario.

use std::collections::BTreeSet;
use std::io::{BufRead, BufReader, Read, Write};
use std::net::{TcpListener, TcpStream};
use std::path::PathBuf;
use std::sync::{Arc, Mutex, MutexGuard, PoisonError};
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use openbilling_telemetry::exposed_names;
use serde_json::{Value, json};

use crate::support::{Deployment, now, priced};

const OWNER: &str = "tenant-obs";
const MEMBER: &str = "member-obs";
const ACCOUNT: &str = "acct-obs";
const CREDIT: &str = "credit-obs";
/// The workload's identities and amounts: what the test sends is built from these, and the alert-delivery proof checks
/// that none of them reached the webhook.
const EVENTS: i64 = 50;
const SEED_MICROS: i64 = 1_000_000;
const CONFLICT_MICROS: i64 = 7_000_000;
const CREDIT_MICROS: i64 = 900_000_000;

fn event_id(i: i64) -> String {
    format!("evt-obs-{i}")
}

/// Every identity and amount the test sends to the service.
fn sent_identities() -> Vec<String> {
    let mut sent = vec![
        OWNER.to_owned(),
        MEMBER.to_owned(),
        ACCOUNT.to_owned(),
        CREDIT.to_owned(),
        CONFLICT_MICROS.to_string(),
        CREDIT_MICROS.to_string(),
    ];
    for i in 0..EVENTS {
        sent.push(event_id(i));
        sent.push((SEED_MICROS + i).to_string());
    }
    sent
}

/// Panels whose query is a ratio or a rate over a counter that this workload never records, so that Prometheus has
/// no series for them and the panel shows "No data" by design (a series that has never counted does not exist,
/// ADR 0007). Every other panel must answer with a value.
const EXPECTED_EMPTY: [&str; 9] = [
    "5xx ratio",
    "Drain error ratio",
    "Telemetry dropped",
    "Refused share of getAllowed",
    // The workload makes no payment, so the payment counters never count (reconciler, mismatch, expiry).
    "Payment mismatches (1 h)",
    "Payment mismatches by field",
    "Top-ups reconciled by outcome",
    "Expired top-ups (1 h)",
    "Expired top-ups over time",
];

/// Metrics the dashboards query that have no series after this workload, for the same reason.
const EXPECTED_ABSENT: [&str; 4] = [
    "openbilling_telemetry_dropped_total",
    "openbilling_payments_mismatches_total",
    "openbilling_topups_reconciled_total",
    "openbilling_topups_expired_total",
];

/// What the sink has seen: the parsed bodies, every request's raw bytes (head and body, as received) and what went wrong.
/// A failure is recorded here and the test asserts on it, so that a thread's panic or a poisoned lock cannot hide it.
#[derive(Default)]
struct SinkState {
    bodies: Vec<Value>,
    raw: Vec<Vec<u8>>,
    errors: Vec<String>,
}

/// The webhook receiver Alertmanager posts to: a std `TcpListener` on a thread of this test, which keeps every request
/// it is sent. It is in the test (not a script of run.sh) so that the test owns what it asserts on and needs no file
/// handoff or extra interpreter; run.sh only fixes its port and writes it into Alertmanager's configuration. It starts
/// before any traffic, so that nothing Alertmanager sends is lost to a retry that could satisfy a later assertion.
/// One request per connection, `Connection: close`, a body with `Content-Length` or chunked; a request it cannot read
/// or parse is recorded as an error, which fails the test.
struct Sink {
    state: Arc<Mutex<SinkState>>,
}

fn lock(state: &Mutex<SinkState>) -> MutexGuard<'_, SinkState> {
    state.lock().unwrap_or_else(PoisonError::into_inner)
}

/// Reads one request: the raw bytes (head and body) and the body alone.
fn read_request(stream: TcpStream) -> Result<(Vec<u8>, Vec<u8>), String> {
    stream
        .set_read_timeout(Some(Duration::from_secs(10)))
        .map_err(|e| e.to_string())?;
    stream
        .set_write_timeout(Some(Duration::from_secs(10)))
        .map_err(|e| e.to_string())?;
    let mut reader = BufReader::new(stream);
    let mut raw = Vec::new();
    let mut length = None;
    let mut chunked = false;
    loop {
        let mut line = String::new();
        let n = reader
            .read_line(&mut line)
            .map_err(|e| format!("reading the head: {e}"))?;
        if n == 0 {
            return Err("the connection closed inside the head".to_owned());
        }
        raw.extend_from_slice(line.as_bytes());
        if line.trim().is_empty() {
            break;
        }
        if let Some((name, value)) = line.split_once(':') {
            if name.eq_ignore_ascii_case("content-length") {
                length = Some(
                    value
                        .trim()
                        .parse::<usize>()
                        .map_err(|e| format!("bad Content-Length: {e}"))?,
                );
            } else if name.eq_ignore_ascii_case("transfer-encoding")
                && value.to_ascii_lowercase().contains("chunked")
            {
                chunked = true;
            }
        }
    }
    let mut body = Vec::new();
    if chunked {
        loop {
            let mut size_line = String::new();
            reader
                .read_line(&mut size_line)
                .map_err(|e| format!("reading a chunk size: {e}"))?;
            raw.extend_from_slice(size_line.as_bytes());
            let size = usize::from_str_radix(size_line.split(';').next().unwrap_or("").trim(), 16)
                .map_err(|e| format!("bad chunk size {size_line:?}: {e}"))?;
            if size == 0 {
                // The trailer: lines up to the empty one.
                loop {
                    let mut trailer = String::new();
                    let n = reader
                        .read_line(&mut trailer)
                        .map_err(|e| format!("reading the trailer: {e}"))?;
                    raw.extend_from_slice(trailer.as_bytes());
                    if n == 0 || trailer.trim().is_empty() {
                        break;
                    }
                }
                break;
            }
            let mut chunk = vec![0u8; size + 2]; // the chunk and its CRLF
            reader
                .read_exact(&mut chunk)
                .map_err(|e| format!("reading a chunk: {e}"))?;
            raw.extend_from_slice(&chunk);
            body.extend_from_slice(&chunk[..size]);
        }
    } else if let Some(length) = length {
        body = vec![0u8; length];
        reader
            .read_exact(&mut body)
            .map_err(|e| format!("reading the body: {e}"))?;
        raw.extend_from_slice(&body);
    } else {
        return Err("a request with neither Content-Length nor chunked encoding".to_owned());
    }
    // Answered on the same stream, after the request has been read.
    let _ = reader
        .get_mut()
        .write_all(b"HTTP/1.1 200 OK\r\nContent-Length: 0\r\nConnection: close\r\n\r\n");
    Ok((raw, body))
}

impl Sink {
    fn start(port: u16) -> Self {
        let listener = TcpListener::bind(("127.0.0.1", port))
            .unwrap_or_else(|e| panic!("the alert sink cannot bind 127.0.0.1:{port}: {e}"));
        let state = Arc::new(Mutex::new(SinkState::default()));
        let kept = state.clone();
        std::thread::spawn(move || {
            for stream in listener.incoming() {
                let kept = kept.clone();
                match stream {
                    Ok(stream) => {
                        std::thread::spawn(move || match read_request(stream) {
                            Ok((raw, body)) => {
                                let mut state = lock(&kept);
                                match serde_json::from_slice::<Value>(&body) {
                                    Ok(json) => state.bodies.push(json),
                                    Err(e) => state.errors.push(format!(
                                        "a body that is not JSON ({e}): {}",
                                        String::from_utf8_lossy(&body)
                                    )),
                                }
                                state.raw.push(raw);
                            }
                            Err(e) => lock(&kept).errors.push(e),
                        });
                    }
                    Err(e) => lock(&kept).errors.push(format!("accept: {e}")),
                }
            }
        });
        Self { state }
    }

    /// Fails the test on anything the sink could not read or parse.
    fn assert_clean(&self) {
        let errors = lock(&self.state).errors.clone();
        assert!(errors.is_empty(), "the alert sink failed: {errors:?}");
    }

    /// Every alert received so far (a body holds a group of alerts), as `(status, alert)`, in arrival order.
    fn alerts(&self) -> Vec<(String, Value)> {
        lock(&self.state)
            .bodies
            .iter()
            .flat_map(|body| body["alerts"].as_array().cloned().unwrap_or_default())
            .map(|a| (a["status"].as_str().unwrap_or_default().to_owned(), a))
            .collect()
    }

    /// A position in the arrival order: what came after it is what came after this call.
    fn marker(&self) -> usize {
        self.alerts().len()
    }

    /// The alerts named `alertname` that arrived after `marker`, as `(status, alert)`.
    fn named_since(&self, marker: usize, alertname: &str) -> Vec<(String, Value)> {
        self.alerts()
            .into_iter()
            .skip(marker)
            .filter(|(_, a)| a["labels"]["alertname"] == alertname)
            .collect()
    }

    fn has_since(&self, marker: usize, status: &str, alertname: &str) -> bool {
        self.named_since(marker, alertname)
            .iter()
            .any(|(s, _)| s == status)
    }

    /// The status of the last notification for `alertname`, if there was any.
    fn last_status(&self, alertname: &str) -> Option<String> {
        self.named_since(0, alertname).pop().map(|(s, _)| s)
    }

    /// Every request as received, head and body, as text.
    fn raw_text(&self) -> String {
        lock(&self.state)
            .raw
            .iter()
            .map(|r| String::from_utf8_lossy(r).into_owned())
            .collect::<Vec<_>>()
            .join("\n")
    }
}

/// The alert the test-only rules (`tools/observability/test-thresholds.toml`: a 10 s window, `for: 2s`) fire when the
/// 4xx ratio is at its critical threshold.
const PROOF_ALERT: &str = "OpenbillingHttp4xxCritical";

/// What the files of `deploy/dashboards` hold today (two dashboards): the panels that have targets, and the targets.
/// A floor, so that a test which found nothing to check cannot pass; raise it with the dashboards.
const MIN_DASHBOARDS: usize = 2;
const MIN_PANELS: usize = 31;
const MIN_TARGETS: usize = 36;

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

    /// The alerts named `alertname` that Prometheus lists as firing. The shipped rule of the same name (window 5 m, `for`
    /// 10 m) cannot be told from the test-only one by its labels, and it stays `pending` for minutes after the traffic
    /// stops, so a pending alert is not looked at: only `firing` is what Alertmanager is sent.
    async fn firing_alerts(&self, alertname: &str) -> Vec<Value> {
        let body: Value = self
            .http
            .get(format!("{}/api/v1/alerts", self.prometheus))
            .send()
            .await
            .unwrap()
            .json_value()
            .await;
        body["data"]["alerts"]
            .as_array()
            .cloned()
            .unwrap_or_default()
            .into_iter()
            .filter(|a| a["labels"]["alertname"] == alertname && a["state"] == "firing")
            .collect()
    }

    /// The alerts named `alertname` that Alertmanager holds active.
    async fn alertmanager_active(&self, alertmanager: &str, alertname: &str) -> Vec<Value> {
        let body: Value = self
            .http
            .get(format!("{alertmanager}/api/v2/alerts"))
            .query(&[
                ("active", "true"),
                ("silenced", "false"),
                ("inhibited", "false"),
            ])
            .send()
            .await
            .unwrap()
            .json_value()
            .await;
        body.as_array()
            .cloned()
            .unwrap_or_default()
            .into_iter()
            .filter(|a| a["labels"]["alertname"] == alertname)
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
    eprintln!(
        "observability stack, leg {}",
        setting("OPENBILLING_OBS_LEG").unwrap_or_else(|| "unknown".to_owned())
    );
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

    // The webhook sink starts before any traffic: whatever Alertmanager sends from here on is kept, and the proof below
    // reads only what arrives after a marker it takes itself.
    let alertmanager = setting("OPENBILLING_OBS_ALERTMANAGER_URL")
        .expect("OPENBILLING_OBS_ALERTMANAGER_URL is set by run.sh");
    let sink_port: u16 = setting("OPENBILLING_OBS_SINK_PORT")
        .and_then(|p| p.parse().ok())
        .expect("OPENBILLING_OBS_SINK_PORT is set by run.sh");
    let sink = Sink::start(sink_port);

    // Phase A: a workload that exercises intake, the drain, the ledger and getAllowed.
    d.configure().await;
    d.open(ACCOUNT, OWNER).await;
    d.grant("adjustment", CREDIT, OWNER, CREDIT_MICROS).await;
    let t = now();
    let events: Vec<Value> = (0..EVENTS)
        .map(|i| priced(&event_id(i), OWNER, MEMBER, SEED_MICROS + i, t))
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
        .usage(vec![priced(
            &event_id(0),
            OWNER,
            MEMBER,
            CONFLICT_MICROS,
            t,
        )])
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
            .usage(vec![priced(
                &event_id(0),
                OWNER,
                MEMBER,
                CONFLICT_MICROS,
                t,
            )])
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
    let groups = rules["data"]["groups"].as_array().unwrap();
    for group in groups {
        for rule in group["rules"].as_array().unwrap() {
            assert_eq!(rule["health"], "ok", "{}: {rule}", rule["name"]);
            assert!(
                rule["lastError"].as_str().is_none_or(str::is_empty),
                "{}: {rule}",
                rule["name"]
            );
        }
    }
    // The shipped file is loaded (by its name), and so is the test-only file that run.sh generated (its group's `file`),
    // with the proof alert at the 2 s `for` of test-thresholds.toml and not the shipped 10 minutes.
    let in_file = |is: fn(&str) -> bool| {
        groups
            .iter()
            .filter(|g| g["file"].as_str().is_some_and(is))
            .collect::<Vec<_>>()
    };
    let shipped = in_file(|f| f.ends_with("/deploy/alerts/openbilling.rules.yaml"));
    assert!(
        !shipped.is_empty(),
        "the shipped rules file is not loaded: {groups:?}"
    );
    assert!(
        shipped
            .iter()
            .flat_map(|g| g["rules"].as_array().unwrap())
            .any(|r| r["name"] == PROOF_ALERT && r["duration"].as_f64() == Some(600.0)),
        "the shipped {PROOF_ALERT} is not loaded with its 10 minute `for`"
    );
    let test_rules = in_file(|f| f.contains("/test-rules/"));
    assert!(
        !test_rules.is_empty(),
        "the test-only rules file is not loaded: {groups:?}"
    );
    let proof_rule = test_rules
        .iter()
        .flat_map(|g| g["rules"].as_array().unwrap())
        .find(|r| r["name"] == PROOF_ALERT)
        .unwrap_or_else(|| panic!("the test-only file has no {PROOF_ALERT}"));
    assert_eq!(
        proof_rule["duration"].as_f64(),
        Some(2.0),
        "{PROOF_ALERT} of the test-only file is not at its 2 s `for`: {proof_rule}"
    );

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

    // Phase C, alert delivery: a threshold crossed on the live service reaches Alertmanager's webhook as `firing`,
    // and as `resolved` once the traffic stops. Nothing here is a unit test of a rule: Prometheus evaluates the test
    // rules, sends to Alertmanager, which notifies the sink.
    let ready = stack
        .http
        .get(format!("{alertmanager}/-/ready"))
        .send()
        .await
        .unwrap();
    assert!(ready.status().is_success(), "Alertmanager is not ready");
    // Phase B already drove conflicts, so the proof alert may have fired and been delivered. Start from a known state:
    // it is not firing in Prometheus for longer than the 10 s window, inactive in
    // Alertmanager, and the last thing the sink heard of it is `resolved` (or nothing).
    let quiet_since: std::cell::Cell<Option<Instant>> = std::cell::Cell::new(None);
    wait_until(
        "the proof alert is inactive and its window empty",
        Duration::from_secs(120),
        || {
            let quiet = &quiet_since;
            let (stack, sink, alertmanager) = (&stack, &sink, &alertmanager);
            async move {
                let idle = stack.firing_alerts(PROOF_ALERT).await.is_empty()
                    && stack
                        .alertmanager_active(alertmanager, PROOF_ALERT)
                        .await
                        .is_empty()
                    && sink
                        .last_status(PROOF_ALERT)
                        .is_none_or(|s| s == "resolved");
                if !idle {
                    quiet.set(None);
                    return false;
                }
                let since = quiet.get().unwrap_or_else(Instant::now);
                quiet.set(Some(since));
                since.elapsed() >= Duration::from_secs(12)
            }
        },
    )
    .await;
    sink.assert_clean();
    // From here, only what the proof itself causes counts: every later notification is after this marker, which is
    // taken before the first proof request.
    let before_proof = sink.marker();
    // Every request is a 409 (a conflict is a 4xx), so the ratio of 4xx stays near 1 over the 10 s window, far above
    // the critical 0.2; the loop ends as soon as the alert has been delivered.
    let started = Instant::now();
    let deadline = started + Duration::from_secs(90);
    loop {
        let refused = d
            .usage(vec![priced(
                &event_id(0),
                OWNER,
                MEMBER,
                CONFLICT_MICROS,
                t,
            )])
            .await;
        assert_eq!(refused.status, 409, "{}", refused.text);
        sink.assert_clean();
        if sink.has_since(before_proof, "firing", PROOF_ALERT) {
            break;
        }
        assert!(
            Instant::now() < deadline,
            "{PROOF_ALERT} was not delivered firing in 90 s; received since the marker: {:?}",
            sink.alerts()
                .into_iter()
                .skip(before_proof)
                .collect::<Vec<_>>()
        );
        tokio::time::sleep(Duration::from_millis(500)).await;
    }
    eprintln!("alert firing delivered after {:?}", started.elapsed());
    assert!(
        !stack.firing_alerts(PROOF_ALERT).await.is_empty(),
        "the sink heard {PROOF_ALERT} firing, but Prometheus does not list it firing"
    );
    // What was delivered for the proof alert: the name, the severity and the job, and nothing that identifies a person
    // or an amount (the other rules' alerts, if any, are not the subject of the label check).
    let proof = sink.named_since(before_proof, PROOF_ALERT);
    for (_, alert) in &proof {
        let keys: BTreeSet<&str> = alert["labels"]
            .as_object()
            .unwrap()
            .keys()
            .map(String::as_str)
            .collect();
        assert_eq!(
            keys,
            BTreeSet::from(["alertname", "job", "severity"]),
            "labels of {alert}"
        );
        assert_eq!(alert["labels"]["severity"], "critical", "{alert}");
        assert_eq!(alert["labels"]["job"], "openbilling", "{alert}");
    }
    // Everything the sink received, parsed and as raw bytes (head included), against everything the test sent.
    let parsed = serde_json::to_string(&lock(&sink.state).bodies).unwrap();
    let everything = format!("{parsed}\n{}", sink.raw_text());
    for secret in sent_identities() {
        assert!(
            !everything.contains(&secret),
            "{secret} is in what the webhook received: {everything}"
        );
    }
    // Traffic has stopped: the 10 s window empties, the ratio has no value, the alert ends and Alertmanager says so.
    // A `resolved` counts only if it arrived after this marker, and Prometheus must no longer list the alert.
    let after_traffic = sink.marker();
    let stopped = Instant::now();
    wait_until(
        "the alert is delivered resolved",
        Duration::from_secs(90),
        || async { sink.has_since(after_traffic, "resolved", PROOF_ALERT) },
    )
    .await;
    eprintln!(
        "alert resolved delivered {:?} after traffic stopped",
        stopped.elapsed()
    );
    assert!(
        stack.firing_alerts(PROOF_ALERT).await.is_empty(),
        "{PROOF_ALERT} was delivered resolved but Prometheus still lists it firing"
    );
    sink.assert_clean();
}
