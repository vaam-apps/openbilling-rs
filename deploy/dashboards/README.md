# Dashboards

Two Grafana dashboards for the operators of openbilling-rs, as code: the JSON in this folder is the dashboard, and **it is generated** by the Python scripts in [`tools/dashboards/`](../../tools/dashboards/) (`sh tools/dashboards/generate.sh`; standard library only). Edit the script, never the JSON: CI regenerates the files and fails on any difference. A panel is one call of `timeseries` or `stat` in `service.py` or `queue_dashboard.py`.

They read the metrics the service exports over OpenTelemetry ([`openbilling-telemetry`](../../crates/openbilling-telemetry/README.md),
[ADR 0007](../../docs/adr/0007-opentelemetry-through-a-layer-over-cratestacks-spans.md)) once a collector has put
them in a Prometheus-compatible store (Grafana's Prometheus data source, an OpenObserve instance's PromQL endpoint,
Mimir, VictoriaMetrics, and so on).

| File | Dashboard | For |
|---|---|---|
| [`openbilling-service.json`](openbilling-service.json) | openbilling: service | The API: traffic, errors, latency, `getAllowed` verdicts |
| [`openbilling-queue.json`](openbilling-queue.json) | openbilling: queue, drain and database | The usage queue, the drain that decides it, PostgreSQL |

## Importing

Grafana 10 or later. In Grafana: *Dashboards, New, Import*, upload each file, and pick the Prometheus data source.
Or from a shell, with a service account token in `GRAFANA_TOKEN` (never in a file of this repository):

```sh
for f in deploy/dashboards/*.json; do
  curl -sS -X POST "$GRAFANA_URL/api/dashboards/db" \
    -H "Authorization: Bearer $GRAFANA_TOKEN" -H 'Content-Type: application/json' \
    -d "{\"dashboard\": $(cat "$f"), \"overwrite\": true}"
done
```

The Helm chart can also ship them as a ConfigMap for Grafana's dashboard sidecar
([`deploy/openbilling/README.md`](../openbilling/README.md), *Dashboards*). The chart's copy in
`deploy/openbilling/dashboards/` is held equal to these files by `tests/check.sh`.

Both dashboards have a **Data source** variable and a **Service** variable (the `job` label, which a collector sets
from `service.name`, `openbilling` by default). Names below are the Prometheus names: a collector turns
`openbilling.drain.runs` into `openbilling_drain_runs_total`, and a unit of `s` into `_seconds`. A test
(`crates/openbilling-telemetry/tests/dashboards.rs`) holds every metric and label a panel queries to the
service's own list, and every panel to this page. **Verified 2026-11-28** by `tools/observability/run.sh`, locally and in CI (observability.yml run 46097502499, job 140832142178, at 14fd8e1, which predates the five payment panels: they were added after it, and the stack run of this pull request covers them, locally; its CI run id is not yet known): a real OpenTelemetry Collector (contrib 0.142.0, `prometheus` exporter), Prometheus v3.16.0 (`honor_labels: true`) and Grafana 13.3.1 give exactly these names, and every panel answers through Grafana's query API with these files provisioned unmodified, bar the panels that show no data until their counter first counts (`5xx ratio`, `Drain error ratio`, `Telemetry dropped`, `Refused share of getAllowed`, and the five payment panels, which the scenario's workload never drives). **Prometheus's native OTLP receiver (ANR-1476) gives the same names and the same `job`, verified 2026-11-28** by `OBS_LEG=otlp sh tools/observability/run.sh`, locally and in CI (observability.yml run 46136084384, job 140951588621, at a31ea78): the Collector's `otlphttp` exporter pushes to `/api/v1/otlp/v1/metrics` of Prometheus v3.16.0 started with `--web.enable-otlp-receiver`, and Prometheus's `otlp.translation_strategy` must be `UnderscoreEscapingWithSuffixes` (its default; any other value, such as `NoTranslation`, keeps dots and drops the suffixes, and these queries then find nothing). The receiver itself maps `service.name` to `job`, so no `promote_resource_attributes` is needed; the SDK's cumulative metrics need no delta feature flag. **Unverified:** any other collector or store (OpenObserve, Mimir, VictoriaMetrics); one with another naming scheme needs the queries adjusted. The stat panels' colours and the [alert rules](../alerts/README.md) share one thresholds file, `tools/dashboards/thresholds.toml`.

## openbilling: service

| Panel | What it watches | Why it matters, and what to do |
|---|---|---|
| `Request rate by route` | Requests per second by route: `/v1/$procs/{name}`, `/v1/accounts`, `/v1/lots`, `/v1/usage_outcomes`, the webhook, the probes | Traffic is the first golden signal. A producer that goes quiet shows here before the queue does |
| `Requests by status class` | Requests per second as 2xx, 4xx, 5xx, stacked | The mix of answers. A growing 4xx band is callers refused (a credential, a bad batch); 5xx is the service or its database |
| `5xx ratio` | Share of requests that answered 5xx | The error budget of the API. Orange at 0.1%, red at 1%. Look at `Errors by class and source`, then the database panels |
| `4xx ratio` | Share of requests refused with 4xx | Orange at 5%, red at 20%. A jump after a deployment is usually a producer on an old contract or a token that expired |
| `Errors by class and source` | Errors per second by class (`invalid`, `conflict`, `unavailable`, `internal`) and source (`api`, `drain`, `partitions`) | Callers decide from the class, so does the operator: `unavailable` is a dependency (PostgreSQL), `internal` is a fault of the service, `invalid` and `conflict` are callers |
| `Latency by route (p50, p95, p99)` | Request duration quantiles of each route, probes left out | The latency signal. p99 climbing alone points at a slow query or a lock; all three together at the pool or the database |
| `p95 latency by procedure` | The 95th percentile of each procedure of the API | Which call is slow: `recordUsage` takes a batch in one transaction, so its latency follows the batch size |
| `getAllowed verdicts` | `getAllowed` answers per second by verdict, the check that decided (`debt_limit`, `balance`, `member_cap`, or `none` when allowed) and the mode it ran under (`at_or_above`, `above`, `never`) | What a serving layer is told. A rise of `balance` refusals is owners running out of credit; `debt_limit` is owners at their debt limit; `member_cap` is a member at their cap. The mode says which comparison the deployer chose, so a change of policy shows as a change of mix |
| `Refused share of getAllowed` | Share of `getAllowed` answers that refused | Orange at 20%, red at 50%. A sudden jump with no change of usage means credit stopped arriving (a payment provider outage) |
| `Ledger postings by kind` | Transactions booked per second: `usage_debit`, `credit`, `allowance`, `reversal`; replays are not counted | The ledger's own throughput. Debits that stop while events are accepted say the drain is stuck; reversals that rise say refunds or chargebacks |
| `Telemetry dropped` | Spans and logs dropped per second because the exporter's queue was full | The exporter never blocks a request, it drops. A band here says the collector is slow or down and the other panels are incomplete |
| `Telemetry exports` | Export requests per second to the collector by signal and result | `failed` is a collector that refuses or does not answer after the SDK's retries; look at its address, its credential header and its network policy |

## openbilling: queue, drain and database

| Panel | What it watches | Why it matters, and what to do |
|---|---|---|
| `Backlog` | Usage events received and not yet decided (the largest value any replica reads: they read one table) | The queue is a table: this is its depth. Orange at 10,000, red at 100,000. Steadily rising means the drain is slower than intake |
| `Oldest event age` | How long the oldest undecided event has waited | What a producer would feel as delay. Orange at 30 s, red at 5 minutes |
| `Drain lag` | Seconds since a drain pass last succeeded | Unlike the backlog it grows when the drain **fails or does not run** even with an empty queue. Orange at 30 s, red at 2 minutes. The drain polls every 5 s, so a healthy value stays under 10 s |
| `Drain error ratio` | Share of drain passes that failed | A failing drain is retried after the poll and the table stays the truth, but a ratio above 1% is a database problem to read in `Errors by class and source` |
| `Backlog and oldest age` | The two queue numbers over time | Read with `Drain lag over time` to tell a burst (both rise and fall) from a stall (both rise, lag rises) |
| `Drain lag over time` | Seconds since the last successful drain pass | A sawtooth under 10 s is normal; a ramp is a stall |
| `Events at intake by result and reason` | Events per second at intake: `accepted`, `replayed`, `refused` with the rule that refused (`too_old`, `too_far_ahead`, `unpriced_meter`, ...) | The producers' health. `replayed` is retries; a `too_old` band is a producer replaying old events past the window; `unpriced_meter` is a meter missing from the price list |
| `Events decided by the drain` | Events per second the drain `applied` or `refused` (`limit`, `unknown_account`, `id_reused`, `no_price`) | Business outcomes. `unknown_account` is usage for an owner nobody opened; `limit` is an owner at their debt limit |
| `Drain duration (p50, p95)` | How long one drain pass takes | A pass is one transaction of up to 500 events. A p95 near the poll interval means the drain cannot keep up and the pool may need room |
| `Drain passes by result` | Drain passes per second, `ok` and `error` | The numerator of `Drain error ratio`, as a rate |
| `Connection pool` | Connections idle and in use, against the pool's maximum | A pool at its maximum makes requests wait for a connection. The pool also holds the drain's `LISTEN` connection |
| `Pool in use` | Share of the pool in use now | Orange at 70%, red at 90%: raise the maximum or add a replica before requests queue |
| `Database operations per second` | Calls per second by ledger operation (`intake`, `drain`, `credit`, `allowed`, ...) | The shape of the load on PostgreSQL, by what causes it |
| `Database p95 latency by operation` | The 95th percentile of each ledger operation | PostgreSQL's health as the service sees it. One operation slow is its function or a lock; all slow is the server |
| `Payment mismatches (1 h)` | Payments read back from the payment provider that differ from the top-up recorded for them, over the last hour | None is expected. Orange from 1 (at least one), red at 10: the same thresholds as `OpenbillingPaymentMismatches*`. Read the next panel for which field differed. No data until the first mismatch is counted; the first one reads as 1 |
| `Payment mismatches by field` | Increase per interval of mismatching payments by the field that differed (`owner`, `label`, `currency`, `amount`, `unrecorded`) | `unrecorded` is a payment with no top-up behind it; the others are a payment that does not match its top-up: settle it by hand |
| `Top-ups reconciled by outcome` | Pending top-ups the reconciler read back per second, by outcome (`credited`, `replayed`, `pending`, `not_ours`, `canceled`, `mismatch`, `error`) | Mostly `pending` and `credited` in a healthy service. A steady `error` is the payment provider or its token; `mismatch` also counts on the mismatch panels |
| `Expired top-ups (1 h)` | Pending top-ups moved to `expired` in the last hour because they outlived the reconciler's `stop_after` | Orange from 5, red at 50: the same thresholds as `OpenbillingTopUpsExpired*`. Customers who never paid expire too; many at once suggest webhooks are lost. A late webhook or `reconcilePayment` still settles an expired top-up. No data until the first expiry is counted |
| `Expired top-ups over time` | The increase per interval of the panel (one bar width, set by the dashboard's range and the panel's width) | Tells a burst (an outage of the webhook path) from a steady trickle |

## What to alert on

Alert rules are the operator's, and none is shipped. Starting points, from the thresholds above: `Drain lag` over 120
s for 5 minutes (the drain is stuck); `Oldest event age` over 300 s; `5xx ratio` over 1% for 10 minutes; `Pool in use`
over 90% for 10 minutes; `Errors by class and source` with `class="internal"` above zero for 5 minutes.
