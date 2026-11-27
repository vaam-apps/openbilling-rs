//! The span, log and database layers against in-memory exporters: what the allowlist lets out, how a failure
//! marks a span, what the sampler keeps, and that a full queue drops and counts.

use std::sync::Arc;
use std::sync::atomic::AtomicUsize;

use opentelemetry::Value;
use opentelemetry::logs::AnyValue;
use opentelemetry::logs::LoggerProvider;
use opentelemetry::trace::{SpanKind, Status, TracerProvider};
use opentelemetry_sdk::logs::{InMemoryLogExporter, SdkLoggerProvider};
use opentelemetry_sdk::trace::{
    BatchConfigBuilder, BatchSpanProcessor, InMemorySpanExporter, Sampler, SdkTracerProvider,
    SpanData,
};
use tracing::{Metadata, info, info_span, warn};
use tracing_subscriber::Layer;
use tracing_subscriber::filter::filter_fn;
use tracing_subscriber::layer::SubscriberExt;

use crate::filter::{BoundedSpans, CAP, GuardedSpans, Stats, Tally, ours};
use crate::layer::{DbTimingLayer, LogLayer};
use openbilling_metrics::{Counter, Histogram, Metrics};

fn metrics() -> &'static Metrics {
    Box::leak(Box::new(Metrics::new()))
}

fn spans_filter(meta: &Metadata<'_>) -> bool {
    ours(meta.target()) && *meta.level() <= CAP
}

struct Run {
    spans: Vec<SpanData>,
    logs: Vec<opentelemetry_sdk::logs::in_memory_exporter::LogDataWithResource>,
    metrics: &'static Metrics,
}

fn run(sampler: Sampler, f: impl FnOnce()) -> Run {
    let metrics = metrics();
    let stats = Arc::new(Stats::default());
    let tally = Tally::new(stats, metrics);
    let span_exporter = InMemorySpanExporter::default();
    let guarded = GuardedSpans::new(span_exporter.clone(), Arc::new(AtomicUsize::new(0)), tally);
    let tracer_provider = SdkTracerProvider::builder()
        .with_simple_exporter(guarded)
        .with_sampler(sampler)
        .build();
    let log_exporter = InMemoryLogExporter::default();
    let logger_provider = SdkLoggerProvider::builder()
        .with_simple_exporter(log_exporter.clone())
        .build();
    let otel = tracing_opentelemetry::layer()
        .with_tracer(tracer_provider.tracer("test"))
        .with_location(false)
        .with_threads(false)
        .with_tracked_inactivity(false)
        .with_filter(filter_fn(spans_filter));
    let logs = LogLayer::new(logger_provider.logger("test")).with_filter(filter_fn(
        |meta: &Metadata<'_>| meta.is_event() && meta.target().starts_with("openbilling"),
    ));
    let db = DbTimingLayer::new(metrics).with_filter(filter_fn(|meta: &Metadata<'_>| {
        meta.is_span() && spans_filter(meta)
    }));
    tracing::subscriber::with_default(
        tracing_subscriber::registry()
            .with(otel)
            .with(logs)
            .with(db),
        f,
    );
    let _ = tracer_provider.force_flush();
    let _ = logger_provider.force_flush();
    Run {
        spans: span_exporter.get_finished_spans().unwrap_or_default(),
        logs: log_exporter.get_emitted_logs().unwrap_or_default(),
        metrics,
    }
}

fn keys(span: &SpanData) -> Vec<&str> {
    span.attributes.iter().map(|kv| kv.key.as_str()).collect()
}

#[test]
fn only_allowlisted_fields_leave_and_their_text_is_plain() {
    let r = run(Sampler::AlwaysOn, || {
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
    assert_eq!(r.spans.len(), 1);
    let span = &r.spans[0];
    assert_eq!(
        keys(span),
        ["http.request.method", "http.route", "cratestack.procedure"]
    );
    assert_eq!(
        span.attributes[2].value,
        Value::String("redacted".into()),
        "an allowed name with a sentence in it is redacted"
    );
    assert_eq!(span.span_kind, SpanKind::Server);
    assert!(span.events.events.is_empty() && span.links.links.is_empty());
    assert_eq!(r.logs.len(), 1);
    let log = &r.logs[0].record;
    assert_eq!(log.body(), Some(&AnyValue::String("drain finished".into())));
    assert_eq!(log.attributes_iter().count(), 0);
}

#[test]
fn a_log_in_a_recorded_span_carries_its_trace_and_span_ids() {
    let r = run(Sampler::AlwaysOn, || {
        let span = info_span!(target: "openbilling_http", "http.server.request");
        let _in = span.enter();
        info!(target: "openbilling_pg", "drain finished");
    });
    let ids = r.logs[0].record.trace_context().expect("trace context");
    assert_eq!(ids.span_id, r.spans[0].span_context.span_id());
    assert_eq!(ids.trace_id, r.spans[0].span_context.trace_id());
}

#[test]
fn children_join_their_parents_trace_and_a_failure_marks_the_span() {
    let r = run(Sampler::AlwaysOn, || {
        let root = info_span!(target: "openbilling_http", "http.server.request");
        let _root = root.enter();
        let route = info_span!(target: "cratestack_x", "cratestack_procedure_route", cratestack_procedure = "getBalance");
        let _route = route.enter();
        warn!(target: "cratestack", cratestack_error = "FORBIDDEN", cratestack_detail = "acct-9", "cratestack procedure failed");
    });
    assert_eq!(r.spans.len(), 2);
    let (child, root) = (&r.spans[0], &r.spans[1]);
    assert_eq!(child.span_context.trace_id(), root.span_context.trace_id());
    assert_eq!(child.parent_span_id, root.span_context.span_id());
    assert!(matches!(child.status, Status::Error { .. }));
    assert_eq!(root.status, Status::Unset);
    assert_eq!(keys(child), ["cratestack.procedure", "error.type"]);
    assert!(child.events.events.is_empty());
    let text = format!("{child:?}");
    assert!(!text.contains("acct-9"), "{text}");
}

#[test]
fn a_5xx_status_marks_the_span_as_an_error() {
    let r = run(Sampler::AlwaysOn, || {
        let span = info_span!(target: "openbilling_http", "http.server.request", http.response.status_code = tracing::field::Empty);
        span.record("http.response.status_code", 503_i64);
    });
    assert!(matches!(r.spans[0].status, Status::Error { .. }));
}

#[test]
fn a_foreign_target_is_not_looked_at_and_the_sample_ratio_holds() {
    let r = run(Sampler::AlwaysOn, || {
        let span = info_span!(target: "sqlx::query", "sqlx", db.statement = "SELECT 1");
        let _in = span.enter();
        info!(target: "other", "hello");
    });
    assert!(r.spans.is_empty() && r.logs.is_empty());
    let none = run(Sampler::TraceIdRatioBased(0.0), || {
        let span = info_span!(target: "openbilling_http", "http.server.request");
        let _in = span.enter();
    });
    assert!(none.spans.is_empty());
}

#[test]
fn verbose_levels_are_not_dispatched() {
    let r = run(Sampler::AlwaysOn, || {
        let span = tracing::debug_span!(target: "openbilling_pg", "db.query");
        drop(span.enter());
    });
    assert!(r.spans.is_empty());
}

#[test]
fn a_database_span_feeds_the_database_histogram_even_unsampled() {
    let r = run(Sampler::TraceIdRatioBased(0.0), || {
        let span = info_span!(target: "openbilling_pg", "db.query", db.system = "postgresql", db.operation = "intake");
        let _in = span.enter();
    });
    assert!(r.spans.is_empty());
    assert_eq!(r.metrics.snapshot().observations(Histogram::Db), 1);
}

#[test]
fn a_full_queue_drops_and_counts() {
    let metrics = metrics();
    let stats = Arc::new(Stats::default());
    let tally = Tally::new(Arc::clone(&stats), metrics);
    let exporter = InMemorySpanExporter::default();
    // A queue that is already at its limit: every span finds it full.
    let pending = Arc::new(AtomicUsize::new(4));
    let guarded = GuardedSpans::new(exporter.clone(), Arc::clone(&pending), tally.clone());
    let batch = BatchSpanProcessor::builder(guarded)
        .with_batch_config(BatchConfigBuilder::default().build())
        .build();
    let provider = SdkTracerProvider::builder()
        .with_span_processor(BoundedSpans::new(batch, pending, 4, tally))
        .build();
    let layer = tracing_opentelemetry::layer()
        .with_tracer(provider.tracer("test"))
        .with_filter(filter_fn(spans_filter));
    tracing::subscriber::with_default(tracing_subscriber::registry().with(layer), || {
        for _ in 0..5 {
            let span = info_span!(target: "openbilling_http", "http.server.request");
            drop(span.enter());
        }
    });
    let _ = provider.force_flush();
    assert!(exporter.get_finished_spans().unwrap_or_default().is_empty());
    assert_eq!(stats.dropped.load(std::sync::atomic::Ordering::Relaxed), 5);
    assert_eq!(
        metrics
            .snapshot()
            .counter(Counter::TelemetryDropped, Some(&[("signal", "traces")])),
        5
    );
}

#[test]
fn a_full_log_queue_drops_and_counts() {
    use crate::filter::{BoundedLogs, GuardedLogs};
    use opentelemetry_sdk::logs::BatchLogProcessor;
    let metrics = metrics();
    let stats = Arc::new(Stats::default());
    let tally = Tally::new(Arc::clone(&stats), metrics);
    let exporter = InMemoryLogExporter::default();
    let pending = Arc::new(AtomicUsize::new(4));
    let guarded = GuardedLogs::new(exporter.clone(), Arc::clone(&pending), tally.clone());
    let batch = BatchLogProcessor::builder(guarded).build();
    let provider = SdkLoggerProvider::builder()
        .with_log_processor(BoundedLogs::new(batch, pending, 4, tally))
        .build();
    let layer =
        LogLayer::new(provider.logger("test")).with_filter(filter_fn(|meta: &Metadata<'_>| {
            meta.is_event() && meta.target().starts_with("openbilling")
        }));
    tracing::subscriber::with_default(tracing_subscriber::registry().with(layer), || {
        for _ in 0..5 {
            info!(target: "openbilling_pg", "drain finished");
        }
    });
    let _ = provider.force_flush();
    assert!(exporter.get_emitted_logs().unwrap_or_default().is_empty());
    assert_eq!(stats.dropped.load(std::sync::atomic::Ordering::Relaxed), 5);
    assert_eq!(
        metrics
            .snapshot()
            .counter(Counter::TelemetryDropped, Some(&[("signal", "logs")])),
        5
    );
}

/// Records what the subscriber answers when a callsite registers.
struct Probe<S> {
    inner: S,
    seen: Arc<std::sync::Mutex<Vec<(String, tracing::Level, bool)>>>,
}

impl<S: tracing::Subscriber> tracing::Subscriber for Probe<S> {
    fn register_callsite(&self, meta: &'static Metadata<'static>) -> tracing::subscriber::Interest {
        let interest = self.inner.register_callsite(meta);
        self.seen.lock().unwrap().push((
            meta.target().to_owned(),
            *meta.level(),
            interest.is_never(),
        ));
        interest
    }
    fn enabled(&self, meta: &Metadata<'_>) -> bool {
        self.inner.enabled(meta)
    }
    fn max_level_hint(&self) -> Option<tracing::metadata::LevelFilter> {
        self.inner.max_level_hint()
    }
    fn new_span(&self, attrs: &tracing::span::Attributes<'_>) -> tracing::span::Id {
        self.inner.new_span(attrs)
    }
    fn record(&self, span: &tracing::span::Id, values: &tracing::span::Record<'_>) {
        self.inner.record(span, values);
    }
    fn record_follows_from(&self, span: &tracing::span::Id, follows: &tracing::span::Id) {
        self.inner.record_follows_from(span, follows);
    }
    fn event(&self, event: &tracing::Event<'_>) {
        self.inner.event(event);
    }
    fn enter(&self, span: &tracing::span::Id) {
        self.inner.enter(span);
    }
    fn exit(&self, span: &tracing::span::Id) {
        self.inner.exit(span);
    }
    fn clone_span(&self, id: &tracing::span::Id) -> tracing::span::Id {
        self.inner.clone_span(id)
    }
    fn try_close(&self, id: tracing::span::Id) -> bool {
        self.inner.try_close(id)
    }
}

fn interests(providers: &crate::export::Providers) -> Vec<(String, tracing::Level, bool)> {
    let seen = Arc::new(std::sync::Mutex::default());
    let dispatch = tracing::Dispatch::new(Probe {
        inner: crate::export::subscriber(providers, metrics()),
        seen: Arc::clone(&seen),
    });
    tracing::dispatcher::with_default(&dispatch, || {
        tracing::trace!(target: "hyper", "foreign trace");
        tracing::info!(target: "sqlx::query", "foreign info");
        tracing::debug!(target: "openbilling_pg", "our debug");
        tracing::trace!(target: "cratestack_x", "our trace");
        let _ = tracing::debug_span!(target: "openbilling_pg", "debug span");
        tracing::info!(target: "openbilling_pg", "our info");
    });
    let seen = seen.lock().unwrap();
    seen.iter().map(|(t, l, i)| (t.clone(), *l, *i)).collect()
}

fn assert_only_ours_at_info_are_dispatched(providers: &crate::export::Providers) {
    let seen = interests(providers);
    assert!(seen.len() >= 5, "{seen:?}");
    for (target, level, never) in &seen {
        let ours = (target.starts_with("openbilling") || target.starts_with("cratestack"))
            && *level <= tracing::Level::INFO;
        // With every signal off, even ours is dispatched to nobody: `never` too.
        let any = providers.tracer.is_some() || providers.logger.is_some();
        if !ours {
            assert!(*never, "{target} {level} must be Interest::never: {seen:?}");
        } else if any {
            assert!(!*never, "{target} {level} must be looked at: {seen:?}");
        }
    }
}

#[test]
fn foreign_and_verbose_callsites_are_never_dispatched_with_traces_on() {
    let tracer = SdkTracerProvider::builder()
        .with_simple_exporter(InMemorySpanExporter::default())
        .build();
    let logger = SdkLoggerProvider::builder()
        .with_simple_exporter(InMemoryLogExporter::default())
        .build();
    let on = crate::export::Providers {
        tracer: Some(tracer),
        meter: None,
        logger: Some(logger),
    };
    assert_only_ours_at_info_are_dispatched(&on);
}

#[test]
fn foreign_and_verbose_callsites_are_never_dispatched_with_traces_off() {
    let off = crate::export::Providers {
        tracer: None,
        meter: None,
        logger: None,
    };
    assert_only_ours_at_info_are_dispatched(&off);
}
