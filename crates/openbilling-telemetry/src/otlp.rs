//! OTLP/HTTP with JSON bodies, written by hand: the three request documents of the OTLP specification (resource
//! spans, resource metrics, resource logs) with the field names OTLP/JSON uses (lowerCamelCase; 64-bit integers
//! as decimal strings; trace and span ids as lowercase hex). No SDK, no protobuf and no tonic come in with it.

use std::time::{SystemTime, UNIX_EPOCH};

use serde_json::{Value as Json, json};

use crate::metrics::{
    BUCKETS, DRAIN_LAG, Pair, Snapshot, all_counters, all_gauges, all_histograms, describe_counter,
    describe_gauge, describe_histogram,
};
use crate::settings::Resource;

/// An attribute value. Only these four shapes exist: no list, no map, no bytes.
#[derive(Clone, Debug, PartialEq)]
pub enum Value {
    /// Text.
    Str(String),
    /// An integer.
    Int(i64),
    /// A flag.
    Bool(bool),
    /// A number.
    Double(f64),
}

fn any_value(value: &Value) -> Json {
    match value {
        Value::Str(text) => json!({ "stringValue": text }),
        Value::Int(n) => json!({ "intValue": n.to_string() }),
        Value::Bool(b) => json!({ "boolValue": b }),
        Value::Double(x) => json!({ "doubleValue": x }),
    }
}

fn attributes(pairs: &[(&str, Value)]) -> Json {
    Json::Array(
        pairs
            .iter()
            .map(|(key, value)| json!({ "key": key, "value": any_value(value) }))
            .collect(),
    )
}

fn label_attributes(pairs: &[Pair]) -> Json {
    Json::Array(
        pairs
            .iter()
            .map(|(key, value)| json!({ "key": key, "value": { "stringValue": value } }))
            .collect(),
    )
}

/// Nanoseconds since the epoch.
#[must_use]
pub fn unix_nanos(time: SystemTime) -> u64 {
    time.duration_since(UNIX_EPOCH)
        .ok()
        .and_then(|d| u64::try_from(d.as_nanos()).ok())
        .unwrap_or(0)
}

fn hex(bytes: &[u8]) -> String {
    use std::fmt::Write as _;
    bytes.iter().fold(String::new(), |mut out, byte| {
        let _ = write!(out, "{byte:02x}");
        out
    })
}

fn resource(resource: &Resource) -> Json {
    let mut pairs = vec![
        ("service.name", Value::Str(resource.service_name.clone())),
        (
            "service.version",
            Value::Str(resource.service_version.clone()),
        ),
    ];
    if let Some(environment) = &resource.environment {
        pairs.push((
            "deployment.environment.name",
            Value::Str(environment.clone()),
        ));
    }
    if let Some(instance) = &resource.instance {
        pairs.push(("service.instance.id", Value::Str(instance.clone())));
    }
    json!({ "attributes": attributes(&pairs) })
}

const SCOPE: &str = "openbilling-telemetry";

fn scope(version: &str) -> Json {
    json!({ "name": SCOPE, "version": version })
}

/// One finished span.
#[derive(Clone, Debug)]
pub struct SpanRecord {
    /// The trace.
    pub trace_id: [u8; 16],
    /// This span.
    pub span_id: [u8; 8],
    /// Its parent, if any.
    pub parent: Option<[u8; 8]>,
    /// Its name: a static name from the code that made it.
    pub name: &'static str,
    /// `SpanKind`: 1 internal, 2 server, 3 client.
    pub kind: u8,
    /// Start, nanoseconds since the epoch.
    pub start: u64,
    /// End, nanoseconds since the epoch.
    pub end: u64,
    /// The allowlisted attributes.
    pub attributes: Vec<(&'static str, Value)>,
    /// Whether the span ended in an error.
    pub error: bool,
}

/// One log record.
#[derive(Clone, Debug)]
pub struct LogRecord {
    /// When, nanoseconds since the epoch.
    pub time: u64,
    /// `SeverityNumber`: 5 debug, 9 info, 13 warn, 17 error.
    pub severity: u8,
    /// The message: static text of this repository's own code.
    pub body: String,
    /// The allowlisted attributes.
    pub attributes: Vec<(&'static str, Value)>,
    /// The span it was emitted in, when sampled.
    pub trace: Option<([u8; 16], [u8; 8])>,
}

/// The `ExportTraceServiceRequest` of `spans`.
#[must_use]
pub fn traces_request(resource_: &Resource, spans: &[SpanRecord]) -> Json {
    let spans: Vec<Json> = spans
        .iter()
        .map(|span| {
            let mut doc = json!({
                "traceId": hex(&span.trace_id),
                "spanId": hex(&span.span_id),
                "name": span.name,
                "kind": span.kind,
                "startTimeUnixNano": span.start.to_string(),
                "endTimeUnixNano": span.end.to_string(),
                "attributes": attributes(&span.attributes),
            });
            if let Some(parent) = span.parent {
                doc["parentSpanId"] = Json::String(hex(&parent));
            }
            if span.error {
                doc["status"] = json!({ "code": 2 });
            }
            doc
        })
        .collect();
    json!({ "resourceSpans": [{
        "resource": resource(resource_),
        "scopeSpans": [{ "scope": scope(&resource_.service_version), "spans": spans }],
    }] })
}

/// The `ExportLogsServiceRequest` of `logs`.
#[must_use]
pub fn logs_request(resource_: &Resource, logs: &[LogRecord]) -> Json {
    let records: Vec<Json> = logs
        .iter()
        .map(|log| {
            let mut doc = json!({
                "timeUnixNano": log.time.to_string(),
                "severityNumber": log.severity,
                "severityText": match log.severity { 17.. => "ERROR", 13.. => "WARN", 9.. => "INFO", _ => "DEBUG" },
                "body": { "stringValue": log.body },
                "attributes": attributes(&log.attributes),
            });
            if let Some((trace, span)) = log.trace {
                doc["traceId"] = Json::String(hex(&trace));
                doc["spanId"] = Json::String(hex(&span));
            }
            doc
        })
        .collect();
    json!({ "resourceLogs": [{
        "resource": resource(resource_),
        "scopeLogs": [{ "scope": scope(&resource_.service_version), "logRecords": records }],
    }] })
}

/// The `ExportMetricsServiceRequest` of a snapshot, at `now`.
#[must_use]
pub fn metrics_request(resource_: &Resource, snapshot: &Snapshot, now: SystemTime) -> Json {
    let start = unix_nanos(snapshot.start).to_string();
    let time = unix_nanos(now).to_string();
    let mut metrics = Vec::new();
    for counter in all_counters() {
        let points: Vec<Json> = snapshot
            .counters
            .iter()
            .filter(|(c, _, _)| *c == counter)
            .map(|(_, labels, n)| {
                json!({
                    "attributes": label_attributes(labels),
                    "startTimeUnixNano": start, "timeUnixNano": time, "asInt": n.to_string(),
                })
            })
            .collect();
        if !points.is_empty() {
            let (name, description, unit) = describe_counter(counter);
            metrics.push(json!({ "name": name, "description": description, "unit": unit,
                "sum": { "dataPoints": points, "aggregationTemporality": 2, "isMonotonic": true } }));
        }
    }
    for histogram in all_histograms() {
        let points: Vec<Json> = snapshot
            .histograms
            .iter()
            .filter(|(h, _, _)| *h == histogram)
            .map(|(_, labels, hist)| {
                json!({
                    "attributes": label_attributes(labels),
                    "startTimeUnixNano": start, "timeUnixNano": time,
                    "count": hist.count.to_string(), "sum": hist.sum,
                    "bucketCounts": hist.counts.iter().map(u64::to_string).collect::<Vec<_>>(),
                    "explicitBounds": BUCKETS,
                })
            })
            .collect();
        if !points.is_empty() {
            let (name, description) = describe_histogram(histogram);
            metrics.push(
                json!({ "name": name, "description": description, "unit": "s",
                "histogram": { "dataPoints": points, "aggregationTemporality": 2 } }),
            );
        }
    }
    for gauge in all_gauges() {
        let points: Vec<Json> = snapshot
            .gauges
            .iter()
            .filter(|(g, _, _)| *g == gauge)
            .map(|(_, labels, value)| {
                json!({
                    "attributes": label_attributes(labels),
                    "startTimeUnixNano": start, "timeUnixNano": time, "asDouble": value,
                })
            })
            .collect();
        if !points.is_empty() {
            let (name, description, unit) = describe_gauge(gauge);
            metrics.push(
                json!({ "name": name, "description": description, "unit": unit,
                "gauge": { "dataPoints": points } }),
            );
        }
    }
    metrics.push(json!({
        "name": DRAIN_LAG,
        "description": "Seconds since a drain pass last succeeded",
        "unit": "s",
        "gauge": { "dataPoints": [{
            "attributes": [], "startTimeUnixNano": start, "timeUnixNano": time,
            "asDouble": snapshot.drain_lag(),
        }] },
    }));
    json!({ "resourceMetrics": [{
        "resource": resource(resource_),
        "scopeMetrics": [{ "scope": scope(&resource_.service_version), "metrics": metrics }],
    }] })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::metrics::{Counter, Histogram, Metrics};

    fn resource_() -> Resource {
        Resource {
            service_name: "openbilling".into(),
            service_version: "0.1.1".into(),
            environment: Some("test".into()),
            instance: None,
        }
    }

    #[test]
    fn metrics_follow_the_otlp_json_shape() {
        let m = Metrics::new();
        m.add(Counter::IntakeEvents, &[("result", "accepted")], 3);
        m.observe(Histogram::Db, &[("operation", "intake")], 0.02);
        let doc = metrics_request(&resource_(), &m.snapshot(), SystemTime::now());
        let metrics = &doc["resourceMetrics"][0]["scopeMetrics"][0]["metrics"];
        let sum = &metrics[0]["sum"];
        assert_eq!(metrics[0]["name"], "openbilling.intake.events");
        assert_eq!(sum["isMonotonic"], true);
        assert_eq!(sum["aggregationTemporality"], 2);
        assert_eq!(sum["dataPoints"][0]["asInt"], "3");
        let hist = &metrics[1]["histogram"]["dataPoints"][0];
        assert_eq!(hist["count"], "1");
        assert_eq!(
            hist["bucketCounts"].as_array().map(Vec::len),
            Some(BUCKETS.len() + 1)
        );
        assert_eq!(metrics[2]["name"], DRAIN_LAG);
        assert_eq!(
            doc["resourceMetrics"][0]["resource"]["attributes"][0]["value"]["stringValue"],
            "openbilling"
        );
    }

    #[test]
    fn spans_carry_hex_ids_and_string_integers() {
        let span = SpanRecord {
            trace_id: [0xab; 16],
            span_id: [0x01; 8],
            parent: Some([0x02; 8]),
            name: "db.query",
            kind: 3,
            start: 5,
            end: 9,
            attributes: vec![
                ("db.operation", Value::Str("intake".into())),
                ("n", Value::Int(7)),
            ],
            error: true,
        };
        let doc = traces_request(&resource_(), &[span]);
        let span = &doc["resourceSpans"][0]["scopeSpans"][0]["spans"][0];
        assert_eq!(span["traceId"], "ab".repeat(16));
        assert_eq!(span["parentSpanId"], "0202020202020202");
        assert_eq!(span["startTimeUnixNano"], "5");
        assert_eq!(span["attributes"][1]["value"]["intValue"], "7");
        assert_eq!(span["status"]["code"], 2);
    }
}
