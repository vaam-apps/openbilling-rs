//! The built binary, run as a process.

// Tests may unwrap and expect. clippy.toml allows it in `#[test]` bodies; this covers the helpers too.
#![allow(clippy::unwrap_used, clippy::expect_used)]

use std::process::{Command, Output};

fn run(args: &[&str]) -> Output {
    Command::new(env!("CARGO_BIN_EXE_openbilling"))
        .args(args)
        .output()
        .expect("the binary runs")
}

#[test]
fn version_prints_the_package_version() {
    let output = run(&["--version"]);
    assert!(output.status.success());
    assert_eq!(
        String::from_utf8(output.stdout).unwrap(),
        format!("openbilling {}\n", env!("CARGO_PKG_VERSION"))
    );
    assert!(output.stderr.is_empty());
}

#[test]
fn help_prints_the_usage() {
    let output = run(&["--help"]);
    assert!(output.status.success());
    let stdout = String::from_utf8(output.stdout).unwrap();
    assert!(stdout.contains("Usage: openbilling"), "{stdout}");
    assert!(stdout.contains("--version"), "{stdout}");
}

#[test]
fn no_argument_fails_and_says_why() {
    let output = run(&[]);
    assert!(!output.status.success());
    assert!(output.stdout.is_empty());
    let stderr = String::from_utf8(output.stderr).unwrap();
    assert!(stderr.contains("not built yet"), "{stderr}");
}

#[test]
fn an_unknown_argument_fails() {
    let output = run(&["--serve"]);
    assert!(!output.status.success());
    assert!(
        String::from_utf8(output.stderr)
            .unwrap()
            .contains("unknown argument")
    );
}
