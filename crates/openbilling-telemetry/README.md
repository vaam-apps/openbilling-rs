# openbilling-telemetry

OpenTelemetry for openbilling-rs: **traces, metrics and logs over OTLP/HTTP with JSON bodies, off unless an endpoint
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

So the exporter is this crate: a `tracing` layer that turns those spans, and ours, into OTLP. It reads the spans
CrateStack already emits, adds nothing to CrateStack, and is the place where the allowlist below is held.

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

**Refused, with a message that names the variable and never its value:** `OTEL_EXPORTER_OTLP_PROTOCOL` other than
`http/json`; `OTEL_EXPORTER_OTLP_HEADERS` (a credential in an environment variable shows in a process listing: use the
file); an endpoint with a user, a password or a query; headers over plain `http` to another host. There is no YAML
config file in the service today (`OPENBILLING_CONFIG` does not exist), so the variables are the configuration.

## What leaves the process

**An attribute is exported only if its field name is in the allowlist** (`layer::FIELDS`: the CrateStack fields above,
`http.request.method`, `http.route`, `http.response.status_code`, `db.system`, `db.operation`, and four counts and a
class on the loop spans). Every other field of a span or an event is dropped where it is read. A text value is also
held to a plain alphabet and 64 bytes (`layer::plain`). `Debug` values are never read. Only spans and events whose
target starts with `openbilling` or `cratestack` are looked at; a log is an `INFO`-or-above event of `openbilling*`
whose message is static text.

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
| `openbilling.errors` | counter | `class` (`invalid`, `conflict`, `unavailable`, `internal`), `source` (`api`, `drain`, `partitions`) | Errors by class |
| `http.server.request.duration` | histogram, s | `method`, `route`, `procedure`, `status` (`2xx` …) | Requests |
| `db.client.operation.duration` | histogram, s | `operation` (a closed list, else `other`) | Ledger database operations |
| `db.client.connection.count` | gauge | `state` (`idle`, `used`) | The pool |
| `db.client.connection.max` | gauge | none | The pool's maximum |

The queue and pool gauges are read every 15 s by `PgLedger::sample_metrics`.

### Logs

`INFO` and above from `openbilling*` targets: `drain failed`, `preparing partitions failed`, with the error class, in
the trace of the span they happened in. The same lines still go to stderr, as before.

## Layout

| File | What |
|---|---|
| `src/settings.rs` | `Settings::from_lookup`: the variables, their refusals, the resource |
| `src/metrics.rs` | The registry: `Counter`, `Histogram`, `Gauge`, `global()`, `exposed_names()` |
| `src/layer.rs` | The `tracing` layer and the allowlist |
| `src/otlp.rs` | The three OTLP/JSON request documents |
| `src/export.rs` | `start`: the tasks that batch and post, `Guard::shutdown` to flush |
| `src/testkit.rs` | Feature `testkit`: a collector in the process (`TestCollector`), in no image |

## Dependencies

No OpenTelemetry SDK, no `tonic`, no protobuf. New packages: `tracing-subscriber` (registry only: no formatter, no
env filter, no regex) and, with it, `sharded-slab`, `thread_local` and `lazy_static`. The HTTP client is the
workspace's `reqwest` on rustls with ring, so the static binary stays free of OpenSSL and of any C code
([ADR 0007](../../docs/adr/0007-opentelemetry-through-a-layer-over-cratestacks-spans.md#dependencies), after ADR 0006).

## Tests

`cargo test -p openbilling-telemetry`: settings and their refusals, the metrics registry, the layer (the allowlist,
parents, sampling, the database histogram), the OTLP documents, the exporter against a collector in the process
(`tests/export.rs`), and the dashboards against the service (`tests/dashboards.rs`: every metric and label a panel
queries exists, every panel is described, the chart's copy equals the files). The flow test with a database is
`openbilling-http/tests/otel.rs`.
