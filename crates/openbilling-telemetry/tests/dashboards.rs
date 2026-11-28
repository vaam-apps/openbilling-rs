//! The dashboards in `deploy/dashboards` are held to the service: every metric a panel queries is one the service
//! exports (as a Prometheus backend names it), every label is one a recording uses, every panel says what it
//! watches and is described in the README, and the chart's copy equals them.

#![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]

use std::collections::BTreeSet;
use std::path::PathBuf;

#[path = "common/promql.rs"]
mod promql;

use openbilling_telemetry::exposed_names;
use serde_json::Value;

fn root() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../../deploy")
}

fn dashboards() -> Vec<(String, Value)> {
    let mut found: Vec<_> = std::fs::read_dir(root().join("dashboards"))
        .unwrap()
        .filter_map(Result::ok)
        .filter(|e| e.path().extension().is_some_and(|x| x == "json"))
        .map(|e| {
            let text = std::fs::read_to_string(e.path()).unwrap();
            (
                e.file_name().to_string_lossy().into_owned(),
                serde_json::from_str(&text).unwrap(),
            )
        })
        .collect();
    found.sort_by(|a, b| a.0.cmp(&b.0));
    found
}

#[test]
fn every_panel_queries_what_the_service_exports_and_is_described() {
    let exposed: BTreeSet<String> = exposed_names().into_iter().collect();
    let readme = std::fs::read_to_string(root().join("dashboards/README.md")).unwrap();
    let found = dashboards();
    assert!(found.len() >= 2, "the two dashboards");
    let mut uids = BTreeSet::new();
    let mut seen = BTreeSet::new();
    for (file, dashboard) in &found {
        assert!(
            uids.insert(dashboard["uid"].as_str().unwrap().to_owned()),
            "{file}: uid is unique"
        );
        assert!(readme.contains(file.as_str()), "{file} is in the README");
        let panels = dashboard["panels"].as_array().unwrap();
        assert!(
            panels.iter().any(|p| p["type"] != "row"),
            "{file} has panels"
        );
        for panel in panels.iter().filter(|p| p["type"] != "row") {
            let title = panel["title"].as_str().unwrap();
            assert!(
                seen.insert(title.to_owned()),
                "{file}: panel title {title:?} is unique"
            );
            assert!(
                readme.contains(&format!("`{title}`")),
                "{file}: panel {title:?} is not in the README"
            );
            assert!(
                panel["description"].as_str().is_some_and(|d| d.len() > 20),
                "{file}: {title:?} says what it watches"
            );
            let targets = panel["targets"].as_array().unwrap();
            assert!(!targets.is_empty(), "{title:?} has a query");
            for target in targets {
                let expr = target["expr"].as_str().unwrap();
                let names = promql::metrics(expr);
                assert!(
                    !names.is_empty(),
                    "{title:?}: {expr} queries no metric of the service"
                );
                for name in names {
                    assert!(
                        exposed.contains(&name),
                        "{title:?} queries {name}, which the service does not export"
                    );
                }
                // Labels: the words inside `by (…)` and before a matcher operator.
                for label in promql::labels(expr) {
                    assert!(
                        promql::LABELS.contains(&label.as_str()),
                        "{title:?}: label {label:?} is not recorded"
                    );
                }
            }
        }
    }
}

#[test]
fn the_charts_copy_equals_the_dashboards() {
    let chart = root().join("openbilling/dashboards");
    for (file, _) in dashboards() {
        let canonical = std::fs::read(root().join("dashboards").join(&file)).unwrap();
        let copy = std::fs::read(chart.join(&file))
            .unwrap_or_else(|_| panic!("deploy/openbilling/dashboards/{file} is missing: copy it"));
        assert!(
            canonical == copy,
            "deploy/openbilling/dashboards/{file} differs from deploy/dashboards/{file}"
        );
    }
}
