//! The alert rules in `deploy/alerts` are held to the service as the dashboards are: every metric an expression
//! queries is one the service exports (as a Prometheus backend names it), every label is one a recording uses,
//! every alert has a severity, a `for`, a summary and a description, each has a warning and a critical pair, each is
//! described in the README, and the chart's copy equals them. `promtool test rules` holds their thresholds
//! (`tools/dashboards/tests/alerts.test.yaml`, run in CI).

#![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]

use std::collections::BTreeSet;
use std::path::PathBuf;

use openbilling_telemetry::exposed_names;
use serde_json::Value;

#[path = "common/promql.rs"]
mod promql;

fn deploy() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../../deploy")
}

fn rules() -> Vec<Value> {
    let text = std::fs::read_to_string(deploy().join("alerts/openbilling.rules.yaml")).unwrap();
    let document: Value = serde_saphyr::from_str(&text).unwrap();
    document["groups"]
        .as_array()
        .unwrap()
        .iter()
        .flat_map(|g| g["rules"].as_array().unwrap().clone())
        .collect()
}

#[test]
fn every_alert_queries_what_the_service_exports_and_is_described() {
    let exposed: BTreeSet<String> = exposed_names().into_iter().collect();
    let readme = std::fs::read_to_string(deploy().join("alerts/README.md")).unwrap();
    let rules = rules();
    assert!(rules.len() >= 16, "the alerts: {}", rules.len());
    let mut names = BTreeSet::new();
    for rule in &rules {
        let name = rule["alert"].as_str().unwrap();
        assert!(names.insert(name), "{name} is unique");
        assert!(
            readme.contains(&format!("`{name}`")),
            "{name} is not in the README"
        );
        let expr = rule["expr"].as_str().unwrap();
        let metrics = promql::metrics(expr);
        assert!(
            !metrics.is_empty(),
            "{name}: {expr} queries no metric of the service"
        );
        for metric in metrics {
            assert!(
                exposed.contains(&metric),
                "{name} queries {metric}, which the service does not export"
            );
        }
        for label in promql::labels(expr) {
            assert!(
                promql::LABELS.contains(&label.as_str()),
                "{name}: label {label:?} is not recorded"
            );
        }
        let severity = rule["labels"]["severity"].as_str().unwrap();
        assert!(
            ["warning", "critical"].contains(&severity),
            "{name}: severity {severity}"
        );
        assert!(
            name.ends_with(&capitalised(severity)),
            "{name} says its severity"
        );
        assert!(rule["for"].as_str().is_some(), "{name} has a `for`");
        for key in ["summary", "description"] {
            assert!(
                rule["annotations"][key]
                    .as_str()
                    .is_some_and(|t| t.len() > 20),
                "{name}: {key} says what is wrong"
            );
        }
    }
    for name in &names {
        if let Some(base) = name.strip_suffix("Warning") {
            assert!(
                names.contains(format!("{base}Critical").as_str()),
                "{name} has no critical pair"
            );
        }
    }
}

fn capitalised(severity: &str) -> String {
    let mut chars = severity.chars();
    chars
        .next()
        .map(|c| c.to_uppercase().chain(chars).collect())
        .unwrap_or_default()
}

#[test]
fn the_charts_copy_equals_the_rules() {
    let canonical = std::fs::read(deploy().join("alerts/openbilling.rules.yaml")).unwrap();
    let copy = std::fs::read(deploy().join("openbilling/alerts/openbilling.rules.yaml"))
        .unwrap_or_else(|_| {
            panic!("deploy/openbilling/alerts/openbilling.rules.yaml is missing: copy it")
        });
    assert!(
        canonical == copy,
        "deploy/openbilling/alerts/openbilling.rules.yaml differs from deploy/alerts/openbilling.rules.yaml"
    );
}
