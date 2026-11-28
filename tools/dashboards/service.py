#!/usr/bin/env python3
"""Generates deploy/dashboards/openbilling-service.json (and the chart's copy). Standard library only.

Run `python3 tools/dashboards/service.py`; CI runs `tools/dashboards/generate.sh` and fails on any difference.
Every metric is the Prometheus name of an instrument of the service (crates/openbilling-metrics); a test
(crates/openbilling-telemetry/tests/dashboards.rs) holds each query to that list.
"""

import thresholds
from lib import dashboard, row, stat, timeseries, write

PANELS = [
    row('Traffic and errors', y=0),
    timeseries(
        'Request rate by route',
        'Requests per second by route (probes included).',
        (0, 1, 12, 8),
        ['sum by (route) (rate(http_server_request_duration_seconds_count{job=~"$job"}[$__rate_interval]))'],
        unit='reqps', fill=10, stacking='none',
    ),
    timeseries(
        'Requests by status class',
        'Requests per second by status class: 2xx, 4xx, 5xx.',
        (12, 1, 12, 8),
        ['sum by (status) (rate(http_server_request_duration_seconds_count{job=~"$job"}[$__rate_interval]))'],
        unit='reqps', fill=40, stacking='normal',
    ),
    stat(
        '5xx ratio',
        'The share of requests that answered 5xx, over the range.',
        (0, 9, 6, 5),
        ['sum(rate(http_server_request_duration_seconds_count{job=~"$job",status="5xx"}[$__rate_interval])) / sum(rate(http_server_request_duration_seconds_count{job=~"$job"}[$__rate_interval]))'],
        unit='percentunit',
        steps=thresholds.steps('http_5xx_ratio'),
    ),
    stat(
        '4xx ratio',
        'The share of requests refused with 4xx: a jump is a producer sending bad requests or a credential problem.',
        (6, 9, 6, 5),
        ['sum(rate(http_server_request_duration_seconds_count{job=~"$job",status="4xx"}[$__rate_interval])) / sum(rate(http_server_request_duration_seconds_count{job=~"$job"}[$__rate_interval]))'],
        unit='percentunit',
        steps=thresholds.steps('http_4xx_ratio'),
    ),
    timeseries(
        'Errors by class and source',
        'Errors the service answered or hit, by class (invalid, conflict, unavailable, internal) and where.',
        (12, 9, 12, 8),
        ['sum by (class, source) (rate(openbilling_errors_total{job=~"$job"}[$__rate_interval]))'],
        unit='ops', fill=10, stacking='none',
    ),
    row('Latency', y=17),
    timeseries(
        'Latency by route (p50, p95, p99)',
        'Request duration quantiles, over every route.',
        (0, 18, 12, 8),
        [('histogram_quantile(0.50, sum by (le, route) (rate(http_server_request_duration_seconds_bucket{job=~"$job",route!~"/healthz|/readyz"}[$__rate_interval])))', 'p50'), ('histogram_quantile(0.95, sum by (le, route) (rate(http_server_request_duration_seconds_bucket{job=~"$job",route!~"/healthz|/readyz"}[$__rate_interval])))', 'p95'), ('histogram_quantile(0.99, sum by (le, route) (rate(http_server_request_duration_seconds_bucket{job=~"$job",route!~"/healthz|/readyz"}[$__rate_interval])))', 'p99')],
        unit='s', fill=10, stacking='none',
    ),
    timeseries(
        'p95 latency by procedure',
        'The 95th percentile of each procedure of the API: where a slow call is.',
        (12, 18, 12, 8),
        ['histogram_quantile(0.95, sum by (le, procedure) (rate(http_server_request_duration_seconds_bucket{job=~"$job",procedure!=""}[$__rate_interval])))'],
        unit='s', fill=10, stacking='none',
    ),
    row('Credit checks', y=26),
    timeseries(
        'getAllowed verdicts',
        'getAllowed answers per second by verdict, the deciding check (debt_limit, balance, member_cap) and the mode it ran under: a rise of refusals is usage being stopped.',
        (0, 27, 12, 8),
        ['sum by (verdict, check, mode) (rate(openbilling_allowed_checks_total{job=~"$job"}[$__rate_interval]))'],
        unit='ops', fill=40, stacking='normal',
    ),
    stat(
        'Refused share of getAllowed',
        'The share of getAllowed answers that refused, over the range.',
        (12, 27, 6, 8),
        ['sum(rate(openbilling_allowed_checks_total{job=~"$job",verdict="refused"}[$__rate_interval])) / sum(rate(openbilling_allowed_checks_total{job=~"$job"}[$__rate_interval]))'],
        unit='percentunit',
        steps=thresholds.steps('allowed_refused_ratio'),
    ),
    timeseries(
        'Ledger postings by kind',
        'Transactions booked per second: usage debits, credits, allowances, reversals. Replays are not counted.',
        (18, 27, 6, 8),
        ['sum by (kind) (rate(openbilling_ledger_postings_total{job=~"$job"}[$__rate_interval]))'],
        unit='ops', fill=10, stacking='none',
    ),
    row('The exporter', y=35),
    timeseries(
        'Telemetry dropped',
        "Spans and logs dropped per second because the exporter's queue was full: the collector is slow or down, or the service is busier than the queue.",
        (0, 36, 12, 8),
        ['sum by (signal) (rate(openbilling_telemetry_dropped_total{job=~"$job"}[$__rate_interval]))'],
        unit='ops', fill=10, stacking='none',
    ),
    timeseries(
        'Telemetry exports',
        "Export requests per second to the collector by signal and result. A rising failed band means this dashboard's own data is incomplete.",
        (12, 36, 12, 8),
        ['sum by (signal, result) (rate(openbilling_telemetry_exports_total{job=~"$job"}[$__rate_interval]))'],
        unit='ops', fill=10, stacking='none',
    ),
]

if __name__ == "__main__":
    write(
        "openbilling-service",
        dashboard(
            uid="openbilling-service",
            title="openbilling: service",
            description="The API: traffic, errors and latency, getAllowed verdicts and errors by class. Panels are described in deploy/dashboards/README.md.",
            panels=PANELS,
        ),
    )
