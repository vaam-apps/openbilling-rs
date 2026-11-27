//! OpenTelemetry for openbilling-rs: traces, metrics and logs through the OpenTelemetry SDK over OTLP/HTTP with JSON
//! bodies, **off unless an endpoint is set** (`OTEL_EXPORTER_OTLP_ENDPOINT`, [`Settings`]).
//!
//! - Spans: `tracing-opentelemetry` turns the `tracing` spans of this repository and of CrateStack (which emits
//!   spans, and no exporter: README) into OpenTelemetry spans, and [`filter::clean`] holds each finished span to an
//!   allowlist of field names ([`filter::FIELDS`]) at the exporter.
//! - [`metrics`] is the registry of closed instruments every crate records into ([`metrics::global`]); none can
//!   take an id or an amount. [`meter::install`] feeds the SDK's instruments from it.
//! - Logs: [`layer::LogLayer`] exports the `INFO`-and-above events of this repository through the same allowlist.
//! - [`export::start`] builds the SDK's providers over OTLP/HTTP and installs the subscriber.
//!
//! The crate holds no secret: headers come from a file, and no error or `Debug` shows one.

pub mod export;
pub mod filter;
pub mod layer;
pub mod meter;
#[cfg(test)]
mod pipeline_tests;
pub mod settings;
#[cfg(feature = "testkit")]
pub mod testkit;

pub use export::{Guard, StartError, Stats, start};
pub use openbilling_metrics as metrics;
pub use openbilling_metrics::{Counter, Gauge, Histogram, Metrics, exposed_names, global};
pub use settings::{Resource, Settings, SettingsError};
