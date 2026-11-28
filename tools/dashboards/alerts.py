#!/usr/bin/env python3
"""Generates the alert rules (deploy/alerts/openbilling.rules.yaml, and the chart's copy) and their promtool unit tests
(tools/dashboards/tests/alerts.test.yaml). Standard library only.

  python3 tools/dashboards/alerts.py                          the defaults of thresholds.toml
  python3 tools/dashboards/alerts.py --thresholds mine.toml   a deployer's partial file over the defaults
  python3 tools/dashboards/alerts.py --out DIR                write the rules (and the tests) to DIR alone

Every signal has a warning and a critical alert over the same query as the stat panel beside it. The warning alert
fires in the band from `warning` up to `critical`; the critical one from `critical`. Every metric is the Prometheus
name of an instrument of the service; crates/openbilling-telemetry/tests/alerts.rs holds each expression to that list.
"""

import argparse
import json
import sys
from pathlib import Path

import thresholds

ROOT = Path(__file__).resolve().parents[2]
RULES_NAME = "openbilling.rules.yaml"
HTTP = "http_server_request_duration_seconds_count"
JOB = "openbilling"
INCREASE_WINDOW = "1h"  # the range of the `increase` signals; their summaries say "the last hour"
INCREASE_TEST_MINUTES = 20  # a test evaluates an `increase` signal this long after its counter starts

# name -> what the signal is. `ratio` signals are `part / total`, `gauge` signals are one metric's max.
# `numerator` and `rest` are the series a unit test feeds: the part, and the remainder of the total.
SIGNALS = {
    "http_5xx_ratio": {
        "alert": "OpenbillingHttp5xx",
        "kind": "ratio",
        "numerator": (HTTP, 'status="5xx"'),
        "rest": (HTTP, 'status="2xx"'),
        "total": HTTP,
        "summary": "{} of requests answer 5xx",
        "meaning": "The service answers 5xx to more than {pct} of its requests over the window: a bug, PostgreSQL unavailable, or a deploy gone wrong.",
        "unit": "percent",
    },
    "http_4xx_ratio": {
        "alert": "OpenbillingHttp4xx",
        "kind": "ratio",
        "numerator": (HTTP, 'status="4xx"'),
        "rest": (HTTP, 'status="2xx"'),
        "total": HTTP,
        "summary": "{} of requests are refused with 4xx",
        "meaning": "More than {pct} of requests are refused with 4xx over the window: a producer sending bad requests, or a credential or issuer problem.",
        "unit": "percent",
    },
    "allowed_refused_ratio": {
        "alert": "OpenbillingAllowedRefused",
        "kind": "ratio",
        "numerator": ("openbilling_allowed_checks_total", 'verdict="refused"'),
        "rest": ("openbilling_allowed_checks_total", 'verdict="allowed"'),
        "total": "openbilling_allowed_checks_total",
        "summary": "{} of getAllowed answers refuse",
        "meaning": "More than {pct} of getAllowed answers refuse over the window: usage is being stopped (debt limit, balance or member cap).",
        "unit": "percent",
    },
    "queue_backlog": {
        "alert": "OpenbillingQueueBacklog",
        "kind": "gauge",
        "metric": "openbilling_queue_backlog",
        "summary": "the usage queue holds {} undecided events",
        "meaning": "More than {value} usage events are received and not decided: the drain is behind or stopped.",
        "unit": "events",
    },
    "queue_oldest_age_seconds": {
        "alert": "OpenbillingQueueOldestAge",
        "kind": "gauge",
        "metric": "openbilling_queue_oldest_age_seconds",
        "summary": "the oldest undecided event has waited {}",
        "meaning": "The oldest undecided usage event has waited more than {value} seconds: what a producer feels as lag.",
        "unit": "seconds",
    },
    "drain_lag_seconds": {
        "alert": "OpenbillingDrainLag",
        "kind": "gauge",
        "metric": "openbilling_drain_lag_seconds",
        "summary": "no drain pass has succeeded for {}",
        "meaning": "No drain pass has succeeded for more than {value} seconds, even with an empty queue: the drain fails or has stopped while the service still reports. A service that reports nothing is OpenbillingNotReportingCritical.",
        "unit": "seconds",
    },
    "drain_error_ratio": {
        "alert": "OpenbillingDrainErrors",
        "kind": "ratio",
        "numerator": ("openbilling_drain_runs_total", 'result="error"'),
        "rest": ("openbilling_drain_runs_total", 'result="ok"'),
        "total": "openbilling_drain_runs_total",
        "summary": "{} of drain passes fail",
        "meaning": "More than {pct} of drain passes fail over the window.",
        "unit": "percent",
    },
    "payment_mismatches": {
        "alert": "OpenbillingPaymentMismatches",
        "kind": "increase",
        "metric": "openbilling_payments_mismatches_total",
        "summary": "{} or more payments read back differ from their top-up in the last hour",
        "meaning": "At least {value} payments read back from the payment provider differ from the top-up recorded for them over the last hour (owner, label, currency or amount): read the `field` label on the payments panel, and settle each by hand.",
        "unit": "payments",
    },
    "topups_expired": {
        "alert": "OpenbillingTopUpsExpired",
        "kind": "increase",
        "metric": "openbilling_topups_expired_total",
        "summary": "{} or more pending top-ups expired in the last hour",
        "meaning": "At least {value} pending top-ups were older than the reconciler's stop_after over the last hour and are no longer read back: webhooks may be lost. A late webhook or reconcilePayment still settles them.",
        "unit": "top-ups",
    },
    "pool_used_ratio": {
        "alert": "OpenbillingPoolSaturated",
        "kind": "pool",
        "summary": "{} of the connection pool is in use",
        "meaning": "More than {pct} of the connection pool is in use: requests wait for a connection.",
        "unit": "percent",
    },
}


def expression(signal, window):
    """The PromQL value of a signal, per `job`."""
    spec = SIGNALS[signal]
    if spec["kind"] == "ratio":
        name, matcher = spec["numerator"]
        return (
            f"sum by (job) (rate({name}{{{matcher}}}[{window}])) / sum by (job) (rate({spec['total']}[{window}]))"
        )
    if spec["kind"] == "gauge":
        return f"max by (job) ({spec['metric']})"
    if spec["kind"] == "increase":
        # A counter has no series until its first add, so `increase` alone reads a series that starts at N as 0. The
        # left branch is the series with no sample an hour ago (a new one): its value is what it has counted so far.
        m = spec["metric"]
        return f"sum by (job) (({m} unless {m} offset {INCREASE_WINDOW}) or increase({m}[{INCREASE_WINDOW}]))"
    return 'sum by (job) (db_client_connection_count{state="used"}) / sum by (job) (db_client_connection_max)'


def number(value):
    return format(value, "g") if isinstance(value, float) else str(value)


def percent(value):
    return f"{number(round(value * 100, 6))}%"


def texts(signal, level, value):
    spec = SIGNALS[signal]
    shown = number(value) if spec["kind"] in ("gauge", "increase") else percent(value)
    summary = f"{level}: " + spec["summary"].format(shown)
    description = spec["meaning"].format(pct=shown, value=number(value)) + " See deploy/alerts/README.md."
    return summary, description


def rules(config):
    window = config["window"]
    out = []
    for signal, spec in SIGNALS.items():
        keys = config["signals"][signal]
        value = expression(signal, window)
        for level in ("warning", "critical"):
            if level == "warning":
                expr = f"({value}) >= {number(keys['warning'])} and ({value}) < {number(keys['critical'])}"
            else:
                expr = f"({value}) >= {number(keys['critical'])}"
            summary, description = texts(signal, level, keys[level])
            out.append(
                {
                    "alert": spec["alert"] + level.capitalize(),
                    "expr": expr,
                    "for": keys["for"],
                    "labels": {"severity": level},
                    "annotations": {"summary": summary, "description": description},
                }
            )
    return out


ABSENT_METRIC = "openbilling_drain_lag_seconds"
ABSENT_ALERT = "OpenbillingNotReportingCritical"


def absent_rule(config):
    """The alert no signal can be: the service reports nothing, so no expression above has a series to compare."""
    window, hold = config["absence"]["window"], config["absence"]["for"]
    return {
        "alert": ABSENT_ALERT,
        "expr": f"absent_over_time({ABSENT_METRIC}[{window}])",
        "for": hold,
        "labels": {"severity": "critical"},
        "annotations": {
            "summary": f"critical: the service has reported no metrics for {window}",
            "description": f"{ABSENT_METRIC}, which a running service always reports, has had no sample for {window}: the service is down, its export or the Collector is broken, or the scrape is. Assumes one job per Prometheus. See deploy/alerts/README.md.",
        },
    }


def all_rules(config):
    return rules(config) + [absent_rule(config)]


def render_rules(config):
    lines = [
        "# Generated by tools/dashboards/alerts.py from tools/dashboards/thresholds.toml: edit those, never this file.",
        "# Each signal has a warning alert (from `warning` up to `critical`) and a critical alert. deploy/alerts/README.md",
        "# says what each means.",
        "groups:",
        "  - name: openbilling",
        "    rules:",
    ]
    for rule in all_rules(config):
        lines.append(f"      - alert: {rule['alert']}")
        lines.append(f"        expr: {json.dumps(rule['expr'])}")
        lines.append(f"        for: {rule['for']}")
        lines.append("        labels:")
        lines.append(f"          severity: {rule['labels']['severity']}")
        lines.append("        annotations:")
        for key, text in rule["annotations"].items():
            lines.append(f"          {key}: {json.dumps(text)}")
    return "\n".join(lines) + "\n"


def series(spec, ratio):
    """Input series (promtool syntax) for one job whose signal sits at `ratio` of its unit (a plain value for gauges)."""
    job = lambda extra: f'job="{{job}}"' + (f",{extra}" if extra else "")  # noqa: E731
    if spec["kind"] == "ratio":
        num_name, num_matcher = spec["numerator"]
        rest_name, rest_matcher = spec["rest"]
        return [
            (f"{num_name}{{{job(num_matcher)}}}", f"0+{number(round(ratio * 100, 9))}x40"),
            (f"{rest_name}{{{job(rest_matcher)}}}", f"0+{number(round((1 - ratio) * 100, 9))}x40"),
        ]
    if spec["kind"] == "gauge":
        return [(f"{spec['metric']}{{{job('')}}}", f"{number(ratio)}x40")]
    if spec["kind"] == "increase":
        # A real counter never exports a 0 sample: it appears, at its first add, with a value (here after 5 minutes).
        return [(f"{spec['metric']}{{{job('')}}}", f"_x5 {number(ratio)}x35")]
    return [
        (f'db_client_connection_count{{{job("state=" + chr(34) + "used" + chr(34))}}}', f"{number(round(ratio * 100, 9))}x40"),
        (f"db_client_connection_max{{{job('')}}}", "100x40"),
    ]


def render_tests(config):
    """A promtool test per signal: three jobs below, in the warning band and above critical."""
    lines = [
        "# Generated by tools/dashboards/alerts.py: edit that, never this file. `promtool test rules` runs it.",
        "# Per signal, three jobs: under the warning (nothing fires), in the warning band, and above critical.",
        "rule_files:",
        f"  - ../../../deploy/alerts/{RULES_NAME}",
        "evaluation_interval: 1m",
        "tests:",
    ]
    for signal, spec in SIGNALS.items():
        keys = config["signals"][signal]
        warning, critical = keys["warning"], keys["critical"]
        high = critical * 1.5 if spec["kind"] in ("gauge", "increase") else min(critical * 1.5, (critical + 1) / 2)
        cases = {"low": warning / 2, "band": (warning + critical) / 2, "high": high}
        lines.append(f"  - name: {signal}")
        lines.append("    interval: 1m")
        lines.append("    input_series:")
        for job, ratio in cases.items():
            for name, values in series(spec, ratio):
                lines.append(f"      - series: {json.dumps(name.replace('{job}', job))}")
                lines.append(f"        values: {json.dumps(values)}")
        lines.append("    alert_rule_test:")
        for_minutes = int(keys["for"][:-1])
        window_minutes = int(config["window"][:-1])
        eval_minutes = for_minutes + window_minutes + 4
        if spec["kind"] == "increase":
            eval_minutes = INCREASE_TEST_MINUTES + for_minutes
        if not (keys["for"].endswith("m") and config["window"].endswith("m")) or eval_minutes > 38:
            sys.exit(f"{signal}: `for` and window must be minutes and short for the generated tests")
        for level, job in (("warning", "band"), ("critical", "high")):
            summary, description = texts(signal, level, keys[level])
            lines.append(f"      - eval_time: {eval_minutes}m")
            lines.append(f"        alertname: {SIGNALS[signal]['alert']}{level.capitalize()}")
            lines.append("        exp_alerts:")
            lines.append("          - exp_labels:")
            lines.append(f"              severity: {level}")
            lines.append(f"              job: {job}")
            lines.append("            exp_annotations:")
            lines.append(f"              summary: {json.dumps(summary)}")
            lines.append(f"              description: {json.dumps(description)}")
    for signal, spec in SIGNALS.items():
        keys = config["signals"][signal]
        for_minutes = int(keys["for"][:-1])
        eval_minutes = INCREASE_TEST_MINUTES + for_minutes if spec["kind"] == "increase" else for_minutes + int(config["window"][:-1]) + 4
        alert = spec["alert"]
        # Inside the warning band the critical alert stays silent.
        band = (keys["warning"] + keys["critical"]) / 2
        lines += [f"  - name: {signal}_critical_silent_in_band", "    interval: 1m", "    input_series:"]
        for name, values in series(spec, band):
            lines += [f"      - series: {json.dumps(name.replace('{job}', 'band'))}", f"        values: {json.dumps(values)}"]
        lines += [
            "    alert_rule_test:",
            f"      - eval_time: {eval_minutes}m",
            f"        alertname: {alert}Critical",
            "        exp_alerts: []",
        ]
        if spec["kind"] in ("gauge", "increase"):
            # The critical value itself is critical (inclusive), and the warning alert is then silent.
            summary, description = texts(signal, "critical", keys["critical"])
            lines += [f"  - name: {signal}_critical_inclusive", "    interval: 1m", "    input_series:"]
            for name, values in series(spec, keys["critical"]):
                lines += [f"      - series: {json.dumps(name.replace('{job}', 'edge'))}", f"        values: {json.dumps(values)}"]
            lines += [
                "    alert_rule_test:",
                f"      - eval_time: {eval_minutes}m",
                f"        alertname: {alert}Critical",
                "        exp_alerts:",
                "          - exp_labels:",
                "              severity: critical",
                "              job: edge",
                "            exp_annotations:",
                f"              summary: {json.dumps(summary)}",
                f"              description: {json.dumps(description)}",
                f"      - eval_time: {eval_minutes}m",
                f"        alertname: {alert}Warning",
                "        exp_alerts: []",
            ]
        if spec["kind"] == "increase":
            # The first event of a counter that did not exist before: it must read as its value, not as 0.
            first = {"payment_mismatches": 1, "topups_expired": 7}.get(signal, keys["warning"])
            summary, description = texts(signal, "warning", keys["warning"])
            lines += [
                f"  - name: {signal}_first_event",
                "    interval: 1m",
                "    input_series:",
                f"      - series: {json.dumps(spec['metric'] + '{job=\"first\"}')}",
                f"        values: {json.dumps('_x10 ' + str(first) + 'x30')}",
                "    alert_rule_test:",
                f"      - eval_time: 20m",
                f"        alertname: {alert}Warning",
                "        exp_alerts:",
                "          - exp_labels:",
                "              severity: warning",
                "              job: first",
                "            exp_annotations:",
                f"              summary: {json.dumps(summary)}",
                f"              description: {json.dumps(description)}",
            ]
    window = int(config["absence"]["window"][:-1])
    hold = int(config["absence"]["for"][:-1])
    if not (config["absence"]["window"].endswith("m") and config["absence"]["for"].endswith("m")):
        sys.exit("absence: `window` and `for` must be minutes for the generated tests")
    rule = absent_rule(config)
    # The metric reports for 5 minutes and then stops; the alert is pending for `for` once a whole window is empty.
    lines += [
        "  - name: not_reporting",
        "    interval: 1m",
        "    input_series:",
        f"      - series: {json.dumps(ABSENT_METRIC + '{job=\"' + JOB + '\"}')}",
        '        values: "1 1 1 1 1 _x60"',
        "    alert_rule_test:",
        "      - eval_time: 4m",
        f"        alertname: {ABSENT_ALERT}",
        "        exp_alerts: []",
        f"      - eval_time: {5 + window + hold + 2}m",
        f"        alertname: {ABSENT_ALERT}",
        "        exp_alerts:",
        "          - exp_labels:",
        "              severity: critical",
        "            exp_annotations:",
        f"              summary: {json.dumps(rule['annotations']['summary'])}",
        f"              description: {json.dumps(rule['annotations']['description'])}",
    ]
    return "\n".join(lines) + "\n"


def main():
    parser = argparse.ArgumentParser(description=__doc__, formatter_class=argparse.RawDescriptionHelpFormatter)
    parser.add_argument("--thresholds", help="a partial thresholds file laid over the defaults")
    parser.add_argument("--out", help="write the rules here alone (and no tests, and no chart copy)")
    args = parser.parse_args()
    try:
        config = thresholds.load(args.thresholds)
    except (thresholds.Refused, OSError) as error:
        sys.exit(f"thresholds: {error}")
    text = render_rules(config)
    if args.out:
        out = Path(args.out)
        out.mkdir(parents=True, exist_ok=True)
        (out / RULES_NAME).write_text(text, encoding="utf-8")
        return
    for directory in (ROOT / "deploy" / "alerts", ROOT / "deploy" / "openbilling" / "alerts"):
        directory.mkdir(parents=True, exist_ok=True)
        (directory / RULES_NAME).write_text(text, encoding="utf-8")
    tests = ROOT / "tools" / "dashboards" / "tests"
    tests.mkdir(parents=True, exist_ok=True)
    (tests / "alerts.test.yaml").write_text(render_tests(config), encoding="utf-8")


if __name__ == "__main__":
    main()
