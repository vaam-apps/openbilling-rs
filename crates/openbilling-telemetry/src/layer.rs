//! The two small `tracing` layers that sit beside `tracing-opentelemetry`'s: [`LogLayer`], which turns the `INFO`-and-
//! above events of this repository into OpenTelemetry logs, and [`DbTimingLayer`], which feeds the database
//! histogram from the database spans.
//!
//! The spans themselves go through `tracing-opentelemetry`'s layer and are cleaned at the exporter
//! ([`crate::filter::clean`]). Logs are filtered **where they are made**: **an attribute is read only if its field name is
//! in [`FIELDS`]**, every other field of an event is dropped where it is read, whatever its value; a text value is
//! held to a plain alphabet and a length; a log's body is the message of an event of this repository, held to words of
//! letters ([`plain_message`]); `Debug` values are never read, so a struct (an error, a request) cannot be exported
//! by accident.

use std::time::{Instant, SystemTime};

use opentelemetry::logs::{AnyValue, LogRecord, Logger, Severity};
use opentelemetry::{Key, trace::TraceContextExt};
use opentelemetry_sdk::logs::SdkLogger;
use tracing::field::{Field, Visit};
use tracing::span::{Attributes, Id, Record};
use tracing::{Event, Level, Subscriber};
use tracing_opentelemetry::OpenTelemetrySpanExt;
use tracing_subscriber::Layer;
use tracing_subscriber::layer::Context;
use tracing_subscriber::registry::LookupSpan;

use crate::filter::{FIELDS, plain, plain_message};
use openbilling_metrics::{Histogram, Metrics, db_operation};

/// The allowlisted fields an event carried, by the attribute they export as, and its message.
#[derive(Default)]
struct Fields {
    attributes: Vec<(&'static str, AnyValue)>,
    message: Option<String>,
}

impl Fields {
    fn put(&mut self, field: &Field, value: AnyValue) {
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
            self.message = Some(plain_message(value));
        } else {
            self.put(field, AnyValue::String(plain(value).into()));
        }
    }
    fn record_i64(&mut self, field: &Field, value: i64) {
        self.put(field, AnyValue::Int(value));
    }
    fn record_u64(&mut self, field: &Field, value: u64) {
        self.put(
            field,
            AnyValue::Int(i64::try_from(value).unwrap_or(i64::MAX)),
        );
    }
    fn record_bool(&mut self, field: &Field, value: bool) {
        self.put(field, AnyValue::Boolean(value));
    }
    fn record_f64(&mut self, field: &Field, value: f64) {
        self.put(field, AnyValue::Double(value));
    }
    fn record_debug(&mut self, field: &Field, value: &dyn std::fmt::Debug) {
        // `Debug` is read for the message only (`format_args!` of a literal): never for another field.
        if field.name() == "message" {
            self.message = Some(plain_message(&format!("{value:?}")));
        }
    }
}

/// Exports the events of this repository as OpenTelemetry logs. Give it a per-layer filter that lets through
/// events at `INFO` and above whose target begins `openbilling`.
pub struct LogLayer {
    logger: SdkLogger,
}

impl LogLayer {
    /// A layer that emits into `logger`.
    #[must_use]
    pub fn new(logger: SdkLogger) -> Self {
        Self { logger }
    }
}

impl<S> Layer<S> for LogLayer
where
    S: Subscriber + for<'a> LookupSpan<'a>,
{
    fn on_event(&self, event: &Event<'_>, _ctx: Context<'_, S>) {
        let meta = event.metadata();
        if !meta.target().starts_with("openbilling") || *meta.level() > Level::INFO {
            return;
        }
        let mut fields = Fields::default();
        event.record(&mut fields);
        let Some(body) = fields.message else { return };
        let (severity, text) = match *meta.level() {
            Level::ERROR => (Severity::Error, "ERROR"),
            Level::WARN => (Severity::Warn, "WARN"),
            _ => (Severity::Info, "INFO"),
        };
        let mut record = self.logger.create_log_record();
        let now = SystemTime::now();
        record.set_timestamp(now);
        record.set_observed_timestamp(now);
        record.set_severity_number(severity);
        record.set_severity_text(text);
        record.set_body(AnyValue::String(body.into()));
        for (key, value) in fields.attributes {
            if key != "error.type" {
                record.add_attribute(Key::new(key), value);
            }
        }
        // The log belongs to the span it was written in, when that span is recorded.
        let context = tracing::Span::current().context();
        let span = context.span();
        let span_context = span.span_context();
        if span_context.is_valid() && span_context.is_sampled() {
            record.set_trace_context(
                span_context.trace_id(),
                span_context.span_id(),
                Some(span_context.trace_flags()),
            );
        }
        self.logger.emit(record);
    }
}

struct Timing {
    operation: Option<&'static str>,
    began: Instant,
}

struct Operation(Option<&'static str>);

impl Visit for Operation {
    fn record_str(&mut self, field: &Field, value: &str) {
        if field.name() == "db.operation" {
            self.0 = Some(db_operation(value));
        }
    }
    fn record_debug(&mut self, _: &Field, _: &dyn std::fmt::Debug) {}
}

/// Records the duration of each database span into the database histogram, sampled or not.
pub struct DbTimingLayer {
    metrics: &'static Metrics,
}

impl DbTimingLayer {
    /// A layer that observes into `metrics`.
    #[must_use]
    pub const fn new(metrics: &'static Metrics) -> Self {
        Self { metrics }
    }
}

impl<S> Layer<S> for DbTimingLayer
where
    S: Subscriber + for<'a> LookupSpan<'a>,
{
    fn on_new_span(&self, attrs: &Attributes<'_>, id: &Id, ctx: Context<'_, S>) {
        let Some(span) = ctx.span(id) else { return };
        let mut operation = Operation(None);
        attrs.record(&mut operation);
        span.extensions_mut().insert(Timing {
            operation: operation.0,
            began: Instant::now(),
        });
    }

    fn on_record(&self, id: &Id, values: &Record<'_>, ctx: Context<'_, S>) {
        let Some(span) = ctx.span(id) else { return };
        let mut extensions = span.extensions_mut();
        let Some(timing) = extensions.get_mut::<Timing>() else {
            return;
        };
        let mut operation = Operation(None);
        values.record(&mut operation);
        if operation.0.is_some() {
            timing.operation = operation.0;
        }
    }

    fn on_close(&self, id: Id, ctx: Context<'_, S>) {
        let Some(span) = ctx.span(&id) else { return };
        let Some(timing) = span.extensions_mut().remove::<Timing>() else {
            return;
        };
        if let Some(operation) = timing.operation {
            self.metrics.observe(
                Histogram::Db,
                &[("operation", operation)],
                timing.began.elapsed().as_secs_f64(),
            );
        }
    }
}
