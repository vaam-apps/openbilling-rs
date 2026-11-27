//! The `tracing` layer: it turns the spans of this repository and of CrateStack into OTLP spans, our `INFO`-and-above
//! events into OTLP logs, and the database spans into the database histogram.
//!
//! **An attribute leaves the process only if its field name is in [`FIELDS`].** Every other field of a span or an
//! event is dropped where it is read, whatever its value. A text value is also held to a plain alphabet and a
//! length ([`plain`]), so a field that is allowed for its name cannot carry a sentence. `Debug` values are never
//! read, so a struct (an error, a request) cannot be exported by accident. Only spans and events whose target is
//! this repository's or CrateStack's are looked at, and a log's body is the message of an event of this repository:
//! static text, never an id (the rule is in the README and held by a test).

use std::collections::hash_map::RandomState;
use std::hash::BuildHasher;
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::{Instant, SystemTime};

use tokio::sync::mpsc::Sender;
use tracing::field::{Field, Visit};
use tracing::span::{Attributes, Id, Record};
use tracing::{Event, Level, Subscriber};
use tracing_subscriber::Layer;
use tracing_subscriber::layer::Context;
use tracing_subscriber::registry::{LookupSpan, SpanRef};

use crate::metrics::{Histogram, Metrics, db_operation};
use crate::otlp::{LogRecord, SpanRecord, Value, unix_nanos};

/// The fields that may be exported, and the attribute each becomes. Everything else is dropped.
pub const FIELDS: [(&str, &str); 17] = [
    ("cratestack_route", "http.route"),
    ("cratestack_procedure", "cratestack.procedure"),
    ("cratestack_model", "cratestack.model"),
    ("cratestack_operation", "cratestack.operation"),
    ("cratestack_paged", "cratestack.paged"),
    ("cratestack_authenticated", "cratestack.authenticated"),
    ("cratestack_error", "error.type"),
    ("http.request.method", "http.request.method"),
    ("http.route", "http.route"),
    ("http.response.status_code", "http.response.status_code"),
    ("db.system", "db.system.name"),
    ("db.operation", "db.operation.name"),
    ("openbilling.batch.limit", "openbilling.batch.limit"),
    ("openbilling.events.decided", "openbilling.events.decided"),
    ("openbilling.events.applied", "openbilling.events.applied"),
    ("openbilling.events.refused", "openbilling.events.refused"),
    ("openbilling.error.class", "openbilling.error.class"),
];

const MAX_TEXT: usize = 64;

/// A text value held to a plain alphabet and [`MAX_TEXT`] bytes; anything else is `redacted`.
#[must_use]
pub fn plain(text: &str) -> String {
    let fits = text.len() <= MAX_TEXT
        && text
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || b"_./$:-{}".contains(&b));
    if fits {
        text.to_owned()
    } else {
        "redacted".to_owned()
    }
}

fn ours(target: &str) -> bool {
    target.starts_with("openbilling") || target.starts_with("cratestack")
}

#[derive(Default)]
struct Fields {
    attributes: Vec<(&'static str, Value)>,
    message: Option<String>,
}

impl Fields {
    fn put(&mut self, field: &Field, value: Value) {
        let Some((_, name)) = FIELDS.iter().find(|(raw, _)| *raw == field.name()) else {
            return;
        };
        match self.attributes.iter_mut().find(|(key, _)| key == name) {
            Some(slot) => slot.1 = value,
            None => self.attributes.push((name, value)),
        }
    }
}

impl Visit for Fields {
    fn record_str(&mut self, field: &Field, value: &str) {
        if field.name() == "message" {
            self.message = Some(value.chars().take(256).collect());
        } else {
            self.put(field, Value::Str(plain(value)));
        }
    }
    fn record_i64(&mut self, field: &Field, value: i64) {
        self.put(field, Value::Int(value));
    }
    fn record_u64(&mut self, field: &Field, value: u64) {
        self.put(field, Value::Int(i64::try_from(value).unwrap_or(i64::MAX)));
    }
    fn record_bool(&mut self, field: &Field, value: bool) {
        self.put(field, Value::Bool(value));
    }
    fn record_f64(&mut self, field: &Field, value: f64) {
        self.put(field, Value::Double(value));
    }
    fn record_debug(&mut self, field: &Field, value: &dyn std::fmt::Debug) {
        // `Debug` is read for the message only (`format_args!` of a literal): never for another field.
        if field.name() == "message" {
            let text = format!("{value:?}");
            self.message = Some(text.chars().take(256).collect());
        }
    }
}

struct SpanData {
    trace_id: [u8; 16],
    span_id: [u8; 8],
    parent: Option<[u8; 8]>,
    name: &'static str,
    kind: u8,
    start: SystemTime,
    began: Instant,
    attributes: Vec<(&'static str, Value)>,
    error: bool,
    sampled: bool,
}

fn random_u64() -> u64 {
    static COUNTER: AtomicU64 = AtomicU64::new(1);
    // `RandomState` is seeded randomly per process; the counter makes every call differ.
    RandomState::new().hash_one((
        COUNTER.fetch_add(1, Ordering::Relaxed),
        unix_nanos(SystemTime::now()),
    ))
}

fn new_span_id() -> [u8; 8] {
    let id = random_u64().max(1);
    id.to_be_bytes()
}

fn new_trace_id() -> [u8; 16] {
    let mut id = [0_u8; 16];
    id[..8].copy_from_slice(&random_u64().to_be_bytes());
    id[8..].copy_from_slice(&random_u64().max(1).to_be_bytes());
    id
}

fn kept(trace_id: &[u8; 16], ratio: f64) -> bool {
    if ratio >= 1.0 {
        return true;
    }
    let mut low = [0_u8; 8];
    low.copy_from_slice(&trace_id[8..]);
    #[allow(clippy::cast_precision_loss)]
    let fraction = (u64::from_be_bytes(low) >> 11) as f64 / (1_u64 << 53) as f64;
    fraction < ratio
}

fn kind_of(name: &str) -> u8 {
    if name.starts_with("http.server") {
        2
    } else if name.starts_with("db.") {
        3
    } else {
        1
    }
}

/// The layer. Cheap to clone: senders and a reference.
#[derive(Clone)]
pub struct TelemetryLayer {
    spans: Option<Sender<SpanRecord>>,
    logs: Option<Sender<LogRecord>>,
    ratio: f64,
    metrics: &'static Metrics,
    dropped: std::sync::Arc<AtomicU64>,
}

impl TelemetryLayer {
    /// A layer that sends finished spans to `spans` and logs to `logs` (either may be `None`), keeps `ratio` of
    /// the traces, and feeds the database histogram of `metrics`.
    #[must_use]
    pub fn new(
        spans: Option<Sender<SpanRecord>>,
        logs: Option<Sender<LogRecord>>,
        ratio: f64,
        metrics: &'static Metrics,
    ) -> Self {
        Self {
            spans,
            logs,
            ratio,
            metrics,
            dropped: std::sync::Arc::default(),
        }
    }

    /// How many spans and logs were dropped because the queue to the exporter was full.
    #[must_use]
    pub fn dropped(&self) -> u64 {
        self.dropped.load(Ordering::Relaxed)
    }
}

fn nearest<'a, S: LookupSpan<'a>>(span: &SpanRef<'a, S>) -> Option<([u8; 16], [u8; 8], bool)> {
    span.scope().find_map(|ancestor| {
        ancestor
            .extensions()
            .get::<SpanData>()
            .map(|data| (data.trace_id, data.span_id, data.sampled))
    })
}

impl<S> Layer<S> for TelemetryLayer
where
    S: Subscriber + for<'a> LookupSpan<'a>,
{
    fn on_new_span(&self, attrs: &Attributes<'_>, id: &Id, ctx: Context<'_, S>) {
        if !ours(attrs.metadata().target()) {
            return;
        }
        let Some(span) = ctx.span(id) else { return };
        let mut fields = Fields::default();
        attrs.record(&mut fields);
        // The nearest recorded ancestor, found from the span's own parent chain.
        let parent = span.parent().and_then(|parent| nearest(&parent));
        let (trace_id, parent_id, sampled) = match parent {
            Some((trace, span_id, sampled)) => (trace, Some(span_id), sampled),
            None => {
                let trace = new_trace_id();
                (trace, None, kept(&trace, self.ratio))
            }
        };
        span.extensions_mut().insert(SpanData {
            trace_id,
            span_id: new_span_id(),
            parent: parent_id,
            name: attrs.metadata().name(),
            kind: kind_of(attrs.metadata().name()),
            start: SystemTime::now(),
            began: Instant::now(),
            attributes: fields.attributes,
            error: false,
            sampled,
        });
    }

    fn on_record(&self, id: &Id, values: &Record<'_>, ctx: Context<'_, S>) {
        let Some(span) = ctx.span(id) else { return };
        let mut extensions = span.extensions_mut();
        let Some(data) = extensions.get_mut::<SpanData>() else {
            return;
        };
        let mut fields = Fields::default();
        values.record(&mut fields);
        for (key, value) in fields.attributes {
            if key == "http.response.status_code" {
                data.error |= matches!(value, Value::Int(code) if code >= 500);
            }
            match data.attributes.iter_mut().find(|(k, _)| *k == key) {
                Some(slot) => slot.1 = value,
                None => data.attributes.push((key, value)),
            }
        }
    }

    fn on_event(&self, event: &Event<'_>, ctx: Context<'_, S>) {
        let meta = event.metadata();
        if !ours(meta.target()) {
            return;
        }
        let mut fields = Fields::default();
        event.record(&mut fields);
        let current = ctx.event_span(event).or_else(|| ctx.lookup_current());
        // CrateStack says a procedure failed with an event carrying the error's code: the span takes it.
        if *meta.level() <= Level::WARN
            && let Some(code) = fields
                .attributes
                .iter()
                .find(|(key, _)| *key == "error.type")
                .map(|(_, value)| value.clone())
            && let Some(span) = &current
            && let Some(data) = span.extensions_mut().get_mut::<SpanData>()
        {
            data.error = true;
            match data.attributes.iter_mut().find(|(k, _)| *k == "error.type") {
                Some(slot) => slot.1 = code,
                None => data.attributes.push(("error.type", code)),
            }
        }
        let Some(logs) = &self.logs else { return };
        if !meta.target().starts_with("openbilling") || *meta.level() > Level::INFO {
            return;
        }
        let Some(body) = fields.message else { return };
        let trace = current
            .as_ref()
            .and_then(nearest)
            .and_then(|(trace, span, sampled)| sampled.then_some((trace, span)));
        let severity = match *meta.level() {
            Level::ERROR => 17,
            Level::WARN => 13,
            _ => 9,
        };
        let record = LogRecord {
            time: unix_nanos(SystemTime::now()),
            severity,
            body,
            attributes: fields
                .attributes
                .into_iter()
                .filter(|(key, _)| *key != "error.type")
                .collect(),
            trace,
        };
        if logs.try_send(record).is_err() {
            self.dropped.fetch_add(1, Ordering::Relaxed);
        }
    }

    fn on_close(&self, id: Id, ctx: Context<'_, S>) {
        let Some(span) = ctx.span(&id) else { return };
        let Some(data) = span.extensions_mut().remove::<SpanData>() else {
            return;
        };
        let elapsed = data.began.elapsed();
        if let Some((_, Value::Str(operation))) = data
            .attributes
            .iter()
            .find(|(key, _)| *key == "db.operation.name")
        {
            self.metrics.observe(
                Histogram::Db,
                &[("operation", db_operation(operation))],
                elapsed.as_secs_f64(),
            );
        }
        let Some(spans) = &self.spans else { return };
        if !data.sampled {
            return;
        }
        let start = unix_nanos(data.start);
        let record = SpanRecord {
            trace_id: data.trace_id,
            span_id: data.span_id,
            parent: data.parent,
            name: data.name,
            kind: data.kind,
            start,
            end: start.saturating_add(u64::try_from(elapsed.as_nanos()).unwrap_or(u64::MAX)),
            attributes: data.attributes,
            error: data.error,
        };
        if spans.try_send(record).is_err() {
            self.dropped.fetch_add(1, Ordering::Relaxed);
        }
    }
}

#[cfg(test)]
mod tests {
    use tokio::sync::mpsc;
    use tracing::{info, info_span, warn};
    use tracing_subscriber::layer::SubscriberExt;

    use super::*;

    fn run(ratio: f64, f: impl FnOnce()) -> (Vec<SpanRecord>, Vec<LogRecord>, &'static Metrics) {
        let metrics: &'static Metrics = Box::leak(Box::new(Metrics::new()));
        let (span_tx, mut span_rx) = mpsc::channel(64);
        let (log_tx, mut log_rx) = mpsc::channel(64);
        let layer = TelemetryLayer::new(Some(span_tx), Some(log_tx), ratio, metrics);
        tracing::subscriber::with_default(tracing_subscriber::registry().with(layer), f);
        let mut spans = Vec::new();
        while let Ok(span) = span_rx.try_recv() {
            spans.push(span);
        }
        let mut logs = Vec::new();
        while let Ok(log) = log_rx.try_recv() {
            logs.push(log);
        }
        (spans, logs, metrics)
    }

    #[test]
    fn only_allowlisted_fields_leave_and_their_text_is_plain() {
        let (spans, logs, _) = run(1.0, || {
            let span = info_span!(
                target: "openbilling_http",
                "http.server.request",
                http.request.method = "POST",
                cratestack_route = "/$procs/getAllowed",
                owner = "acct-secret-7",
                token = "Bearer abc",
                amount = 123_456_789_u64,
                cratestack_procedure = "has a space and an id acct-1",
            );
            let _in = span.enter();
            info!(target: "openbilling_pg", owner = "acct-secret-7", batch = 5_u64, "drain finished");
        });
        assert_eq!(spans.len(), 1);
        let keys: Vec<_> = spans[0].attributes.iter().map(|(k, _)| *k).collect();
        assert_eq!(
            keys,
            ["http.request.method", "http.route", "cratestack.procedure"]
        );
        assert_eq!(spans[0].attributes[2].1, Value::Str("redacted".into()));
        assert_eq!(spans[0].kind, 2);
        assert_eq!(logs.len(), 1);
        assert_eq!(logs[0].body, "drain finished");
        assert!(logs[0].attributes.is_empty());
        assert_eq!(logs[0].trace.map(|(_, s)| s), Some(spans[0].span_id));
    }

    #[test]
    fn children_join_their_parents_trace_and_a_failure_marks_the_span() {
        let (spans, _, _) = run(1.0, || {
            let root = info_span!(target: "openbilling_http", "http.server.request");
            let _root = root.enter();
            let route = info_span!(target: "cratestack_x", "cratestack_procedure_route", cratestack_procedure = "getBalance");
            let _route = route.enter();
            warn!(target: "cratestack", cratestack_error = "FORBIDDEN", cratestack_detail = "acct-9", "cratestack procedure failed");
        });
        assert_eq!(spans.len(), 2);
        let (child, root) = (&spans[0], &spans[1]);
        assert_eq!(child.trace_id, root.trace_id);
        assert_eq!(child.parent, Some(root.span_id));
        assert!(child.error && !root.error);
        let keys: Vec<_> = child.attributes.iter().map(|(k, _)| *k).collect();
        assert_eq!(keys, ["cratestack.procedure", "error.type"]);
    }

    #[test]
    fn a_foreign_target_is_not_looked_at_and_the_sample_ratio_holds() {
        let (spans, logs, _) = run(1.0, || {
            let span = info_span!(target: "sqlx::query", "sqlx", db.statement = "SELECT 1");
            let _in = span.enter();
            info!(target: "other", "hello");
        });
        assert!(spans.is_empty() && logs.is_empty());
        let (none, _, _) = run(0.0, || {
            let span = info_span!(target: "openbilling_http", "http.server.request");
            let _in = span.enter();
        });
        assert!(none.is_empty());
    }

    #[test]
    fn a_database_span_feeds_the_database_histogram_even_unsampled() {
        let (spans, _, metrics) = run(0.0, || {
            let span = info_span!(target: "openbilling_pg", "db.query", db.system = "postgresql", db.operation = "intake");
            let _in = span.enter();
        });
        assert!(spans.is_empty());
        assert_eq!(metrics.snapshot().observations(Histogram::Db), 1);
    }
}
