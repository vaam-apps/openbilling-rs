# 0007. OpenTelemetry through a layer over CrateStack's spans, and dashboards as code

Status: **Proposed** (2026-11-27). The owner's decision quoted under *Context* stands; the judgement calls under
*Still open* are for the owner to confirm. Verified by tests (a collector in the process, a PostgreSQL 19 in CI);
not yet run against a real collector or a real Grafana.

## Summary

The service exports traces, metrics and logs over OTLP/HTTP with JSON bodies, **off unless an endpoint is set**.
CrateStack `=0.20.6` has **no OpenTelemetry exporter**: it emits `tracing` spans, and this decision exports them. A
small crate, `openbilling-telemetry`, holds a `tracing` layer that turns CrateStack's spans and ours into OTLP spans,
a registry of closed metrics, and the exporter. **Nothing that identifies a person, an account or an amount can be
an attribute or a label**; that is held by construction and by a test. Two Grafana dashboards ship as JSON in
`deploy/dashboards/`, each panel explained, and the chart can ship them as a ConfigMap.

## Context

The owner, 2026-11-27 (decision row 219): *"Cratestack support OTel exporters. We need to use that. More generally,
all backends should export OTel and we need to build OTel dashboards, important information to watch."*
Dashboards for a Grafana or an OpenObserve instance for operators, separate from any instance for users.

**What CrateStack gives** (*verified 2026-11-27*, the `=0.20.6` sources in the cargo registry and crates.io):

| Claim | Source |
|---|---|
| CrateStack emits `tracing` spans from generated code: `cratestack_procedure_route`, `cratestack_procedure_invoke_with_db`, `cratestack_model_list_route`, with `cratestack_*` fields | `cratestack-macros-0.20.6/src/axum/procedure.rs`, `procedure/instrument.rs`, `axum/model/handlers_list.rs` |
| It emits events on `target: "cratestack"` with `cratestack_error` when a procedure fails | same files |
| `cratestack-service` has `telemetry::init`, a `tracing_subscriber` fmt (text or JSON) bootstrap, and no OpenTelemetry | `cratestack-service-0.20.6/src/telemetry.rs`, its `Cargo.toml` (no opentelemetry dependency) |
| No `cratestack-*` crate on crates.io is an OpenTelemetry exporter | crates.io search for `cratestack`, 2026-11-27, 48 crates, none |

So the owner's premise (CrateStack supports OTel exporters) holds at this version for **spans**, which are the
thing an exporter reads, and not for an exporter. The decision is to export what CrateStack emits, rather than wait
for CrateStack to ship one.

Constraints: the binary stays static with no OpenSSL and no C code (ADR 0006, in review in vaam-apps/openbilling-rs#20);
the repository is public; `cargo deny` stays green; **no secret, token or person's id in a signal**. Account, owner
and member ids are opaque to the service, but they are the customers' and an operations instance is not where they
belong.

## Decision

1. **A layer, not an SDK.** `openbilling-telemetry` implements a `tracing_subscriber::Layer` that records spans
   whose target is this repository's or CrateStack's, and an exporter that posts OTLP/HTTP JSON with the workspace's
   `reqwest` on rustls with ring. No OpenTelemetry SDK, no tonic, no protobuf.
2. **An allowlist of field names** (`layer::FIELDS`) decides what leaves; every other field is dropped where it is
   read. Text values are held to a plain alphabet and 64 bytes, `Debug` values are never read, and an inbound
   `traceparent` is ignored.
3. **Metrics are closed.** Counters add counts, histograms record durations, gauges hold counts and ages; a label is
   two `&'static str`. No amount and no id can be recorded. The list is in the crate README; the dashboards are held
   to it by a test.
4. **Off unless an endpoint is set**, configured by the standard `OTEL_*` variables (a subset, strictly read). Headers
   come from a file, never an environment variable; `OTEL_EXPORTER_OTLP_PROTOCOL` other than `http/json` is refused.
   The service has no YAML config file, so there is no `otel` block to add.
5. **Spans:** one `http.server.request` per request (the probes are metrics only), CrateStack's three route and
   procedure spans as its children, one `db.query` span per ledger operation, `drain.cycle` and `partitions.run`
   for the loops. **Metrics:** events at intake and decided by the drain by result and reason, ledger postings by
   kind, `getAllowed` verdicts by check and mode, errors by class and source, request and database durations, the drain's
   duration and lag, the queue's backlog and oldest age, and the pool. **Logs:** `INFO`-and-above events of this
   repository, whose messages are static text.
6. **Dashboards are Grafana JSON** in `deploy/dashboards/` (the format ANR-1165 left to the first card; Grafana reads
   a Prometheus-compatible store, which an OpenObserve instance also offers over PromQL). A test holds each
   panel's metrics and labels to the service's own list. The chart carries a copy as an optional ConfigMap for
   Grafana's sidecar, held equal by a test.

## Dependencies

New packages: `tracing-subscriber 0.3.23` with only `registry` and `std`, and its `sharded-slab`, `thread_local`
and `lazy_static` (all MIT or MIT/Apache-2.0, pure Rust). `tracing` was already in the lockfile through CrateStack.
`reqwest`, `rustls` (ring), `serde_json`, `url` and `tokio` are the workspace's. `axum` is used by the crate's test
collector only, behind the feature `testkit`. No OpenSSL, `native-tls`, or C build step comes in; `deny.toml` is
unchanged.

## Alternatives

| Alternative | Why not |
|---|---|
| `opentelemetry` + `opentelemetry-otlp` + `tracing-opentelemetry` | Three crates with their own versions that move together, `tonic` and `prost` for `gen-tonic-messages` even with HTTP, an async-runtime story for the batch processor, and a TLS choice (`reqwest-rustls` selects aws-lc-rs) that would add a C dependency to a binary that is to be static. Hand-written OTLP/JSON is about 250 lines and no new package beyond the subscriber |
| Wait for CrateStack to ship an exporter | None is announced; the owner wants dashboards now. The layer reads the spans CrateStack emits, so an exporter of CrateStack's would replace only `export.rs` |
| `cratestack_service::telemetry::init` | It formats to stdout; it does not export, and it pulls `env-filter` (regex) |
| Prometheus scrape (`/metrics`) | A pull endpoint on the public listener, or a second one to firewall. Push over OTLP matches the owner's decision and needs no Service or ServiceMonitor |
| Account ids as attributes "for debugging" | A support need is met by the ledger, not by a trace store of the operator's. The cost of one leaked id in a vendor's store is not reversible |

## Consequences

- A deployment sets one variable (`openbilling.otel.endpoint` in the chart) to turn everything on, and gets golden
  signals, the queue and the database.
- The exporter drops, never blocks: queues are bounded (4096), a failed export is retried three times and dropped
  with a rate-limited line on stderr. A collector that is down costs telemetry, not requests.
- Metrics are cumulative and in the process: a restart resets counters (a Prometheus-compatible store handles it).
  With several replicas each exports its own series and the dashboards aggregate them: counters and pool gauges
  with `sum`, and the queue's backlog, age and the drain's lag with `max`, because every replica reads the same
  table.
- A new span field or metric label is a code change in two places (the field and `FIELDS`, or the instrument
  enum), and the dashboards test fails if a panel queries a name that is gone.

## Still open

| Question | Notes |
|---|---|
| Does the deployment's collector and store apply the OpenTelemetry to Prometheus name translation the dashboards assume? | *Unverified.* The names follow the specification; a different scheme needs the queries adjusted |
| An OpenObserve-native dashboard format | Grafana JSON was chosen once for the family. OpenObserve imports its own format; a conversion is a follow-up if the operators' instance is OpenObserve alone |
| Alert rules | None shipped: the thresholds in the dashboards' README are starting points for the operator |
| `traceparent` propagation to the payment provider | Not done; the vpay adapter does not forward a trace context |
