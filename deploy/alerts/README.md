# Alert rules

Prometheus alert rules for the operators of openbilling-rs, as code: [`openbilling.rules.yaml`](openbilling.rules.yaml)
is **generated** by [`tools/dashboards/alerts.py`](../../tools/dashboards/alerts.py) from
[`tools/dashboards/thresholds.toml`](../../tools/dashboards/thresholds.toml), the same file that sets the colours of
the stat panels of the [dashboards](../dashboards/README.md), so a panel and its alert never disagree. Edit the
thresholds, never the YAML: CI regenerates the files, fails on any difference, and runs `promtool check rules` and
`promtool test rules` (the unit tests are [`tools/dashboards/tests/alerts.test.yaml`](../../tools/dashboards/tests/alerts.test.yaml),
generated too).

Each signal has two alerts over the same query as its panel, per `job` (the service): the **warning** one fires from
the `warning` threshold up to the `critical` one (inclusive at the bottom), the **critical** one from `critical` up.
Both wait for the signal to hold for `for` before they fire. Rates are taken over a 5 minute window. A ratio over a
counter that has never counted has no series, so its alert cannot fire until the first event (ADR 0007, *Still
open*).

## Changing a threshold

Without editing the defaults: write a partial file in the shape of `thresholds.toml`, naming only what changes, and
generate the rules for it:

```sh
cat > mine.toml <<'EOF'
[signals.queue_backlog]
warning = 50000
critical = 500000
EOF
python3 tools/dashboards/alerts.py --thresholds mine.toml --out ./my-rules
```

An unknown signal or key, a value of the wrong type, or a warning at or above its critical is refused with the
file's path in the message. `--out` writes the rules file alone, and not the tests or the chart's copy.

## The alerts

| Alert | Fires when | What to do |
|---|---|---|
| `OpenbillingHttp5xxWarning`, `OpenbillingHttp5xxCritical` | The share of requests answered 5xx is at least 0.1% (warning) or 1% (critical) for 5 minutes | Look at the errors by class and source on the service dashboard, then PostgreSQL and the latest deployment |
| `OpenbillingHttp4xxWarning`, `OpenbillingHttp4xxCritical` | The share refused with 4xx is at least 5% or 20% for 10 minutes | A producer on an old contract or an expired token; the issuer's keys |
| `OpenbillingAllowedRefusedWarning`, `OpenbillingAllowedRefusedCritical` | The share of `getAllowed` answers that refuse is at least 20% or 50% for 10 minutes | Usage is being stopped: read the `check` label of the verdicts panel (`debt_limit`, `balance`, `member_cap`) |
| `OpenbillingQueueBacklogWarning`, `OpenbillingQueueBacklogCritical` | Undecided usage events number at least 10 000 or 100 000 for 10 minutes | The drain is behind or stopped: its lag, its error ratio, the pool |
| `OpenbillingQueueOldestAgeWarning`, `OpenbillingQueueOldestAgeCritical` | The oldest undecided event has waited at least 30 s or 300 s for 5 minutes | What producers feel as lag; the same causes as the backlog |
| `OpenbillingDrainLagWarning`, `OpenbillingDrainLagCritical` | No drain pass has succeeded for at least 30 s or 120 s, for 5 minutes | The drain fails or does not run, even with an empty queue: the service's log, PostgreSQL |
| `OpenbillingDrainErrorsWarning`, `OpenbillingDrainErrorsCritical` | At least 1% or 10% of drain passes fail for 5 minutes | The service's log for the failing pass; PostgreSQL |
| `OpenbillingPoolSaturatedWarning`, `OpenbillingPoolSaturatedCritical` | At least 70% or 90% of the connection pool is in use for 10 minutes | Requests wait for a connection: slow queries, a lock, or a pool smaller than the load |

The numbers above are the defaults. Each alert's summary and description state the threshold it was generated with.

## Using them

- **Prometheus, Mimir, VictoriaMetrics:** list the file under `rule_files`, or load it with the vmalert/ruler of your
  store.
- **The Helm chart** ([`deploy/openbilling/README.md`](../openbilling/README.md), *Alert rules*) can render them as a
  `PrometheusRule` for the Prometheus Operator (`prometheusRule.enabled`, off by default). The chart's copy in
  `deploy/openbilling/alerts/` is held equal to these files by a test and by `tests/check.sh`.

**Verified locally 2026-11-28** by `tools/observability/run.sh`: the file loads in Prometheus v3.16.0 and every rule
evaluates without error against series made by a real Collector (see ADR 0007, *Amended 2026-11-28*). That the alerts
*fire* on a live service is covered by `promtool test rules` only; no end-to-end run drives a threshold.
