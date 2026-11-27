#!/usr/bin/env python3
"""Generates deploy/dashboards/openbilling-queue.json (and the chart's copy). Standard library only.

Run `python3 tools/dashboards/queue_dashboard.py`; CI runs `tools/dashboards/generate.sh` and fails on any difference.
Every metric is the Prometheus name of an instrument of the service (crates/openbilling-metrics); a test
(crates/openbilling-telemetry/tests/dashboards.rs) holds each query to that list.
"""

from lib import dashboard, row, stat, timeseries, write

PANELS = [
    row('The queue', y=0),
    stat(
        'Backlog',
        'Usage events received and not yet decided. Climbs when the drain is behind or stopped.',
        (0, 1, 6, 5),
        ['max(openbilling_queue_backlog{job=~"$job"})'],
        unit='short',
        steps=[(None, 'green'), (10000, 'orange'), (100000, 'red')],
    ),
    stat(
        'Oldest event age',
        'How long the oldest undecided event has waited: what a producer would feel as lag.',
        (6, 1, 6, 5),
        ['max(openbilling_queue_oldest_age_seconds{job=~"$job"})'],
        unit='s',
        steps=[(None, 'green'), (30, 'orange'), (300, 'red')],
    ),
    stat(
        'Drain lag',
        'Seconds since a drain pass last succeeded. It grows when the drain fails or does not run, even with an empty queue.',
        (12, 1, 6, 5),
        ['max(openbilling_drain_lag_seconds{job=~"$job"})'],
        unit='s',
        steps=[(None, 'green'), (30, 'orange'), (120, 'red')],
    ),
    stat(
        'Drain error ratio',
        'The share of drain passes that failed, over the range.',
        (18, 1, 6, 5),
        ['sum(rate(openbilling_drain_runs_total{job=~"$job",result="error"}[$__rate_interval])) / sum(rate(openbilling_drain_runs_total{job=~"$job"}[$__rate_interval]))'],
        unit='percentunit',
        steps=[(None, 'green'), (0.01, 'orange'), (0.1, 'red')],
    ),
    timeseries(
        'Backlog and oldest age',
        "The queue's size and the age of its oldest event, over time.",
        (0, 6, 12, 8),
        [('max(openbilling_queue_backlog{job=~"$job"})', 'backlog'), ('max(openbilling_queue_oldest_age_seconds{job=~"$job"})', 'oldest age (s)')],
        unit='short', fill=10, stacking='none',
    ),
    timeseries(
        'Drain lag over time',
        'Seconds since the last successful drain pass.',
        (12, 6, 12, 8),
        [('max(openbilling_drain_lag_seconds{job=~"$job"})', 'lag')],
        unit='s', fill=10, stacking='none',
    ),
    row('Events', y=14),
    timeseries(
        'Events at intake by result and reason',
        'Events per second at intake: accepted, replayed, or refused by the rule (too_old, too_far_ahead, unpriced_meter, ...).',
        (0, 15, 12, 8),
        ['sum by (result, reason) (rate(openbilling_intake_events_total{job=~"$job"}[$__rate_interval]))'],
        unit='ops', fill=40, stacking='normal',
    ),
    timeseries(
        'Events decided by the drain',
        'Events per second the drain applied or refused (limit, unknown_account, id_reused, no_price).',
        (12, 15, 12, 8),
        ['sum by (outcome, reason) (rate(openbilling_drain_events_total{job=~"$job"}[$__rate_interval]))'],
        unit='ops', fill=40, stacking='normal',
    ),
    timeseries(
        'Drain duration (p50, p95)',
        'How long one drain pass takes.',
        (0, 23, 12, 8),
        [('histogram_quantile(0.50, sum by (le) (rate(openbilling_drain_duration_seconds_bucket{job=~"$job"}[$__rate_interval])))', 'p50'), ('histogram_quantile(0.95, sum by (le) (rate(openbilling_drain_duration_seconds_bucket{job=~"$job"}[$__rate_interval])))', 'p95')],
        unit='s', fill=10, stacking='none',
    ),
    timeseries(
        'Drain passes by result',
        'Drain passes per second, ok and error.',
        (12, 23, 12, 8),
        ['sum by (result) (rate(openbilling_drain_runs_total{job=~"$job"}[$__rate_interval]))'],
        unit='ops', fill=10, stacking='none',
    ),
    row('PostgreSQL', y=31),
    timeseries(
        'Connection pool',
        "Connections idle and in use, against the pool's maximum: a pool at its maximum makes requests wait.",
        (0, 32, 12, 8),
        ['sum by (state) (db_client_connection_count{job=~"$job"})', ('sum(db_client_connection_max{job=~"$job"})', 'max')],
        unit='short', fill=10, stacking='none',
    ),
    stat(
        'Pool in use',
        "The share of the pool's connections in use now.",
        (12, 32, 6, 8),
        ['sum(db_client_connection_count{job=~"$job",state="used"}) / sum(db_client_connection_max{job=~"$job"})'],
        unit='percentunit',
        steps=[(None, 'green'), (0.7, 'orange'), (0.9, 'red')], minimum=0,
    ),
    timeseries(
        'Database operations per second',
        "Calls per second to the ledger's database operations.",
        (18, 32, 6, 8),
        ['sum by (operation) (rate(db_client_operation_duration_seconds_count{job=~"$job"}[$__rate_interval]))'],
        unit='ops', fill=10, stacking='none',
    ),
    timeseries(
        'Database p95 latency by operation',
        'The 95th percentile of each database operation: a slow PostgreSQL shows here first.',
        (0, 40, 12, 8),
        ['histogram_quantile(0.95, sum by (le, operation) (rate(db_client_operation_duration_seconds_bucket{job=~"$job"}[$__rate_interval])))'],
        unit='s', fill=10, stacking='none',
    ),
]

if __name__ == "__main__":
    write(
        "openbilling-queue",
        dashboard(
            uid="openbilling-queue",
            title="openbilling: queue, drain and database",
            description="The usage queue, the drain that decides it, the ledger postings and PostgreSQL. Panels are described in deploy/dashboards/README.md.",
            panels=PANELS,
        ),
    )
