# openbilling-telemetry

OpenTelemetry for openbilling-rs: **traces, metrics and logs through the OpenTelemetry SDK over OTLP/HTTP with JSON bodies, off unless an endpoint
is set.** [ADR 0007](../../docs/adr/0007-opentelemetry-through-a-layer-over-cratestacks-spans.md) says why it is built
this way. The dashboards that read what it exports are in [`deploy/dashboards`](../../deploy/dashboards/README.md).

## What CrateStack gives, and what it does not

*Verified 2026-11-27 by reading the `=0.20.6` sources in the cargo registry (`cratestack-macros`,
`cratestack-service`) and a search of crates.io for `cratestack`.*

- CrateStack **emits `tracing` spans** from the code its macros generate: `cratestack_procedure_route` (fields
  `cratestack_route`, `cratestack_procedure`), `cratestack_procedure_invoke_with_db` (`cratestack_procedure`,
  `cratestack_authenticated`), `cratestack_model_list_route` (`cratestack_route`, `cratestack_model`,
  `cratestack_paged`), and events on `target: "cratestack"` that carry `cratestack_error` when a procedure fails.
- CrateStack has **no OpenTelemetry exporter**. `cratestack-service` offers `telemetry::init`, a `tracing_subscriber`
  text or JSON formatter; the crates.io listing of the `cratestack-*` crates holds no OTel crate.

So the exporter is this crate: `tracing-opentelemetry` turns those spans, and ours, into OpenTelemetry spans, and the
SDK exports them over OTLP. It reads the spans CrateStack already emits, adds nothing to CrateStack, and is the place
where the allowlist below is held.

## Configuration

The standard `OTEL_*` variables, a strict subset (`bin/openbilling --help` lists them). **No endpoint, no telemetry.**

| Variable | Meaning |
|---|---|
| `OTEL_EXPORTER_OTLP_ENDPOINT` | The collector's base URL; `/v1/traces`, `/v1/metrics`, `/v1/logs` are appended. `OTEL_EXPORTER_OTLP_{TRACES,METRICS,LOGS}_ENDPOINT` give a full URL for one signal |
| `OPENBILLING_OTEL_HEADERS_FILE` | A file of `Name: value` lines sent with every export. https, or this machine only |
| `OTEL_SERVICE_NAME`, `OTEL_RESOURCE_ATTRIBUTES` | `service.name` (default `openbilling`); `deployment.environment.name` (or `deployment.environment`) and `service.instance.id` are read, other keys ignored. `service.version` is the release |
| `OTEL_TRACES_SAMPLER_ARG` | The share of traces kept, 0 to 1 (default 1), decided from the trace id at the root |
| `OTEL_METRIC_EXPORT_INTERVAL`, `OTEL_BSP_SCHEDULE_DELAY`, `OTEL_EXPORTER_OTLP_TIMEOUT` | Milliseconds: 15000, 5000 and 10000 by default |
| `OTEL_TRACES_EXPORTER`, `OTEL_METRICS_EXPORTER`, `OTEL_LOGS_EXPORTER` | `otlp` or `none` |
| `OTEL_SDK_DISABLED` | `true` turns everything off |
| `OTEL_EXPORTER_OTLP_CERTIFICATE` | A PEM CA bundle; the only trust for an `https` collector |
| `OTEL_TRACES_SAMPLER` | `always_on`, `always_off`, `traceidratio` (with `OTEL_TRACES_SAMPLER_ARG`), or the `parentbased_` forms |

A signal is on when it has a URL (a base, or its own endpoint) and is not `none`: with only `OTEL_EXPORTER_OTLP_TRACES_ENDPOINT`, only traces go out.

**Refused at start, with a message that names the variable and never its value:** every variable of the specification this exporter does not honour (per-signal `*_HEADERS`, `*_TIMEOUT` and `*_CERTIFICATE`, `OTEL_EXPORTER_OTLP_CLIENT_KEY` and `_CLIENT_CERTIFICATE`, `_COMPRESSION`, `OTEL_BLRP_*` and the other `OTEL_BSP_*`, `OTEL_PROPAGATORS`, the attribute limits), a sampler other than `always_on`, `always_off`, `traceidratio` and their `parentbased_` forms, a per-signal protocol other than `http/json`; `OTEL_EXPORTER_OTLP_PROTOCOL` other than
`http/json`; `OTEL_EXPORTER_OTLP_HEADERS` (a credential in an environment variable shows in a process listing: use the
file); an endpoint with a user, a password or a query; headers over plain `http` to another host. The service does have a YAML file (`OPENBILLING_CONFIG`), but it holds the deployer's policy (`getAllowed`); the environment holds endpoints and secrets, and no setting is read from both, so OpenTelemetry is configured by the variables.

## What leaves the process

**An attribute is exported only if its field name is in the allowlist** (`filter::FIELDS`: the CrateStack fields above,
`http.request.method`, `http.route`, `http.response.status_code`, `db.system`, `db.operation`, and four counts and a
class on the loop spans). `tracing-opentelemetry` records every field of a span, so **each finished span is cleaned at the exporter** (`filter::clean`, in the exporter that wraps the OTLP one): every other field is dropped, whatever its value; span events and links never leave (CrateStack's `cratestack_error` event becomes the span's `error.type` and error status); the span's status carries no message. A text value is also
held to a plain alphabet and 64 bytes (`filter::plain`). Logs are filtered where they are made (`layer::LogLayer`) and `Debug` values are never read. Only spans and events whose
target starts with `openbilling` or `cratestack`, at `INFO` or above, are looked at (a per-layer filter, so the `TRACE` callsites of sqlx, hyper and rustls are never dispatched); a log is an `INFO`-or-above event of `openbilling*`
whose message is **held to words of letters only** (`filter::plain_message`: `drain failed (unavailable).` passes; `account acct-42 refused` or anything over 128 bytes is exported as `redacted`), so an id with a digit in it cannot leave through a log. A message of letters alone is the code's own to keep static.

**Metrics cannot carry an identity or an amount**: a counter adds a count, a histogram records a duration, a gauge
holds a count or an age, and a label is a pair of `&'static str` taken from a closed enum or a literal.

So **no secret, token, owner, account, member, event id, credit id, idempotency key or amount is an attribute**, and
the route is a fixed name (`/v1/$procs/{name}`, never a path with an id). `openbilling-http/tests/otel.rs` runs a
flow with sentinel values and scans every byte the collector received for each of them.

### Spans

| Span | From | Attributes |
|---|---|---|
| `http.server.request` (server) | `openbilling-http`, every request but the probes | `http.request.method`, `http.route`, `http.response.status_code`; error on 5xx |
| `cratestack_procedure_route`, `cratestack_procedure_invoke_with_db`, `cratestack_model_list_route` | CrateStack | `http.route`, `cratestack.procedure`, `cratestack.model`, `cratestack.paged`, `cratestack.authenticated`; `error.type` (CrateStack's error code) and an error status when a procedure fails |
| `db.query` (client) | `openbilling-pg`, one per ledger operation | `db.system.name` = `postgresql`, `db.operation.name`, and for the drain `openbilling.events.decided` |
| `drain.cycle` | `openbilling serve`, one per drain pass | `openbilling.batch.limit`, `openbilling.events.decided`, `.applied`, `.refused` |
| `partitions.run` | `openbilling serve`, hourly | none |
| `retention.run` | `openbilling serve`, hourly, after `partitions.run` | none |
| `reconcile.run` | `openbilling serve`, every `reconcile.every`, only with a payment provider | none |

An inbound `traceparent` is **not** read: this is a public API, and a caller must not choose the trace of its request.

### Metrics

Names below are OTLP names; a Prometheus-compatible store has them as `openbilling_intake_events_total` and so on
(`metrics::exposed_names`, which the dashboards test uses).

| Metric | Type | Labels | What |
|---|---|---|---|
| `openbilling.intake.events` | counter | `result` (`accepted`, `replayed`, `refused`), `reason` (the rule, on a refusal) | Events at intake |
| `openbilling.drain.events` | counter | `outcome` (`applied`, `refused`), `reason` | Events the drain decided |
| `openbilling.drain.runs` | counter | `result` (`ok`, `error`) | Drain passes |
| `openbilling.drain.duration` | histogram, s | none | One drain pass |
| `openbilling.drain.lag` | gauge, s | none | Seconds since a drain pass last succeeded |
| `openbilling.queue.backlog` | gauge | none | Events received and not yet decided |
| `openbilling.queue.oldest_age` | gauge, s | none | Age of the oldest of them |
| `openbilling.ledger.postings` | counter | `kind` (`usage_debit`, `credit`, `allowance`, `reversal`) | Transactions booked; replays not counted |
| `openbilling.allowed.checks` | counter | `verdict` (`allowed`, `refused`), `check` (`debt_limit`, `balance`, `member_cap`, `none`, `other`), `mode` (`at_or_above`, `above`, `never`: the deployer's comparison for that check; `none` when allowed) | `getAllowed` verdicts |
| `openbilling.telemetry.dropped` | counter | `signal` (`traces`, `logs`) | Spans or logs dropped because the exporter's queue (4096) was full |
| `openbilling.telemetry.exports` | counter | `signal` (`traces`, `metrics`, `logs`), `result` (`ok`, `failed`) | Export requests to the collector; `failed` is after the SDK's retries |
| `openbilling.partitions.dropped` | counter | none | Usage partitions the retention job detached and dropped |
| `openbilling.payments.mismatches` | counter | `field` (`owner`, `label`, `currency`, `amount`, `unrecorded`) | Payments read back that differ from the top-up recorded for them: nothing is credited. Counted once, where the ledger decides; also a warning with `openbilling.mismatch.field` and the class |
| `openbilling.topups.reconciled` | counter | `outcome` (`credited`, `pending`, `canceled`, `mismatch`, `error`) | Pending top-ups the reconciler read back |
| `openbilling.topups.expired` | counter | none | Pending top-ups moved to `expired` after a last read at `reconcile.stop_after` found them still pending (or, unread, because the provider said no such payment or, past `last_read_for`, a failing last read was given up); a webhook or `reconcilePayment` still settles them |
| `openbilling.errors` | counter | `class` (`invalid`, `conflict`, `unavailable`, `internal`), `source` (`api`, `drain`, `partitions`, `retention`, `reconcile`) | Errors by class |
| `http.server.request.duration` | histogram, s | `method`, `route`, `procedure`, `status` (`2xx` …) | Requests |
| `db.client.operation.duration` | histogram, s | `operation` (a closed list, else `other`) | Ledger database operations |
| `db.client.connection.count` | gauge | `state` (`idle`, `used`) | The pool |
| `db.client.connection.max` | gauge | none | The pool's maximum |

The queue and pool gauges are read every 15 s by `PgLedger::sample_metrics`.

### Logs

`INFO` and above from `openbilling*` targets: `drain failed`, `preparing partitions failed`, `dropping expired partitions failed`, with the error class, in
the trace of the span they happened in. The same lines still go to stderr, as before.

## Layout

| File | What |
|---|---|
| `src/settings.rs` | `Settings::from_lookup`: the variables, their refusals, the resource |
| (`openbilling-metrics`) | The registry: `Counter`, `Histogram`, `Gauge`, `global()`, `exposed_names()`: a crate of its own with no dependency, re-exported here, so `openbilling-pg` and `openbilling-http` record without an HTTP client |
| `src/filter.rs` | The allowlist (`FIELDS`, `clean`), the text rules, and the wrappers around the SDK's processors and exporters that count, bound and clean |
| `src/layer.rs` | `LogLayer` (logs, filtered where they are made) and `DbTimingLayer` (the database histogram) |
| `src/meter.rs` | The bridge: the registry's counters and histograms into the SDK's instruments, its gauges as observable instruments |
| `src/export.rs` | `start`: builds the tracer, meter and logger providers over one `reqwest` client and installs the subscriber; `Guard::shutdown` flushes with a deadline |
| `src/testkit.rs` | Feature `testkit`: a collector in the process (`TestCollector`), in no image |

## Dependencies

*Verified 2026-11-27 (crates.io and each crate's `Cargo.toml`).* `opentelemetry` 0.33.1, `opentelemetry_sdk` 0.33.1,
`opentelemetry-otlp` 0.33.1 (features `http-json` and `reqwest-blocking-client`: message types and a blocking client,
no `tonic`) and `tracing-opentelemetry` 0.34.0, which move together. The HTTP client is the workspace's `reqwest` 0.13 on
rustls with ring, supplied by this crate (the SDK's own client builder is not used), so the static binary stays free of
OpenSSL, `aws-lc-rs`, `tonic` and any new `-sys` crate. `cargo tree` on both musl targets, before and after: the new
packages are the four crates, `opentelemetry-http`, `opentelemetry-proto`, `prost`, `rand` 0.9 and its parts,
`getrandom` 0.3, `futures-executor`, `const-hex`, `itertools`, `zerocopy` and `ppv-lite86`
([ADR 0007](../../docs/adr/0007-opentelemetry-through-a-layer-over-cratestacks-spans.md#dependencies), after ADR 0006).
`openbilling-metrics` depends on none of them: it offers a `Recorder` seam that this crate attaches.

## Tests

`cargo test -p openbilling-telemetry`: settings and their refusals (the unsupported variables, the sampler, the CA, one signal alone), the metrics registry and its `Recorder` seam, the pipeline against in-memory exporters (`src/pipeline_tests.rs`: the allowlist,
parents, a failure marking its span, sampling, the database histogram, a full queue dropping and counting), the exporter against a collector in the process
(`tests/export.rs`: what arrives, the headers, the resource, the exporter's own metrics, and no byte of a sentinel), a collector that never answers (`tests/hang.rs`: recording is instant and `shutdown` ends by its deadline), a full queue (a unit test), and the dashboards against the service (`tests/dashboards.rs`: every metric and label a panel
queries exists, every panel is described, the chart's copy equals the files), and the alert rules likewise (`tests/alerts.rs`:
every metric and label an expression queries exists, every alert has a severity, a `for` and a description, each warning has its critical pair, every alert is in `deploy/alerts/README.md`, the chart's copy equals the file; the PromQL reader is `tests/common/promql.rs`). The flow test with a database is
`openbilling-http/tests/otel.rs`. Against a real Collector, Prometheus and Grafana: `e2e/observability_stack.rs`, run by `tools/observability/run.sh` (gated; *verified locally 2026-11-28*).
