# openbilling-metrics

The registry every crate records its metrics into, and nothing else: **no dependency**, so `openbilling-pg` and
`openbilling-http` record without depending on an HTTP client. [`openbilling-telemetry`](../openbilling-telemetry/README.md)
exports it over OTLP and lists every instrument, label and meaning. The registry has one seam, `Metrics::attach`
of a `Recorder`, which receives each counter increment and histogram observation: that is how the telemetry crate
feeds the OpenTelemetry SDK without this crate, or its callers, depending on it.

**No instrument can carry an identity or an amount.** A counter adds a count, a histogram records a duration, a
gauge holds a count or an age, and a label is a pair of `&'static str` (a literal, or a closed enum's `as_str`).
`global()` is the process-wide registry; `Metrics::new()` is for tests. `exposed_names()` is the list of names a
Prometheus-compatible store has them under, which the dashboards' and the alert rules' tests hold every panel and every expression to. That a real Collector and Prometheus give exactly these names was *verified locally 2026-11-28* by `tools/observability/run.sh` (ADR 0007).

`cargo test -p openbilling-metrics`: counters and histograms, the drain's lag, the closed database operations, the
exposed names, and a recorder that sees each increment once.
