//! What the dashboards' and the alerts' tests share: a tiny reader of the PromQL they hold (names and labels only,
//! not a parser), and the labels the code records.

use std::collections::BTreeSet;

/// The labels the code records, and the ones a Prometheus backend adds.
pub const LABELS: [&str; 19] = [
    "job",
    "le",
    "route",
    "method",
    "status",
    "procedure",
    "class",
    "source",
    "verdict",
    "check",
    "result",
    "reason",
    "outcome",
    "kind",
    "state",
    "operation",
    "mode",
    "signal",
    "__rate_interval",
];

/// The words of an expression.
pub fn tokens(expr: &str) -> BTreeSet<String> {
    expr.split(|c: char| !(c.is_ascii_alphanumeric() || c == '_'))
        .filter(|t| !t.is_empty())
        .map(str::to_owned)
        .collect()
}

/// The metrics of the service an expression names.
pub fn metrics(expr: &str) -> Vec<String> {
    tokens(expr)
        .into_iter()
        .filter(|t| {
            ["openbilling_", "http_server_", "db_client_"]
                .iter()
                .any(|p| t.starts_with(p))
        })
        .collect()
}

/// The labels an expression groups by or matches on: the words inside `by (…)` and before a matcher operator.
pub fn labels(expr: &str) -> Vec<String> {
    let mut found = Vec::new();
    for group in expr.split("by (").skip(1) {
        let inside = group.split(')').next().unwrap();
        found.extend(inside.split(',').map(|l| l.trim().to_owned()));
    }
    for matcher in expr.split('{').skip(1) {
        let inside = matcher.split('}').next().unwrap();
        for part in inside.split(',') {
            found.push(
                part.trim()
                    .chars()
                    .take_while(|c| c.is_ascii_alphanumeric() || *c == '_')
                    .collect(),
            );
        }
    }
    found
}
