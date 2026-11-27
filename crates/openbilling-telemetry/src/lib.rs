//! OpenTelemetry for openbilling-rs: traces, metrics and logs over OTLP/HTTP with JSON bodies, **off unless an
//! endpoint is set** (`OTEL_EXPORTER_OTLP_ENDPOINT`, [`Settings`]).
//!
//! - [`TelemetryLayer`](layer::TelemetryLayer) exports the `tracing` spans of this repository and of CrateStack
//!   (which emits spans, and no exporter: README), through an allowlist of field names ([`layer::FIELDS`]).
//! - [`metrics`] is the registry of closed instruments every crate records into ([`metrics::global`]); none can
//!   take an id or an amount.
//! - [`otlp`] writes the three OTLP/JSON request documents; [`export::start`] ships them.
//!
//! The crate holds no secret: headers come from a file, and no error or `Debug` shows one.

pub mod export;
pub mod layer;
pub mod otlp;
pub mod settings;
#[cfg(feature = "testkit")]
pub mod testkit;

pub use export::{Guard, StartError, Stats, start};
pub use openbilling_metrics as metrics;
pub use openbilling_metrics::{Counter, Gauge, Histogram, Metrics, exposed_names, global};
pub use settings::{Resource, Settings, SettingsError};
