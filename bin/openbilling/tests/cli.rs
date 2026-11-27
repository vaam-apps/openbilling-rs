//! The built binary, run as a process.

// Tests may unwrap and expect. clippy.toml allows it in `#[test]` bodies; this covers the helpers too.
#![allow(clippy::unwrap_used, clippy::expect_used)]

use std::process::{Command, Output};

/// The binary with `args`, its environment cleared of colour settings unless `env` sets them. Its output is a
/// pipe, not a terminal.
fn run_with(args: &[&str], env: &[(&str, &str)]) -> Output {
    let mut command = Command::new(env!("CARGO_BIN_EXE_openbilling"));
    command
        .args(args)
        .env_remove("NO_COLOR")
        .env_remove("CLICOLOR_FORCE")
        .env_remove("CLICOLOR");
    for (name, value) in env {
        command.env(name, value);
    }
    command.output().expect("the binary runs")
}

fn run(args: &[&str]) -> Output {
    run_with(args, &[])
}

const ESC: char = '\u{1b}';

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
fn help_prints_the_usage_and_the_environment() {
    let output = run(&["--help"]);
    assert!(output.status.success());
    let stdout = String::from_utf8(output.stdout).unwrap();
    assert!(stdout.contains("Usage: openbilling"), "{stdout}");
    assert!(stdout.contains("serve"), "{stdout}");
    assert!(stdout.contains("--version"), "{stdout}");
    assert!(stdout.contains("OPENBILLING_DATABASE_URL"), "{stdout}");
}

#[test]
fn colour_follows_the_terminal_no_color_and_clicolor_force() {
    let piped = String::from_utf8(run(&["--help"]).stdout).unwrap();
    assert!(!piped.contains(ESC), "a pipe gets no colour");
    let forced =
        String::from_utf8(run_with(&["--help"], &[("CLICOLOR_FORCE", "1")]).stdout).unwrap();
    assert!(forced.contains(ESC), "CLICOLOR_FORCE colours a pipe");
    let refused = run_with(&["serve"], &[("CLICOLOR_FORCE", "1")]);
    assert!(String::from_utf8(refused.stderr).unwrap().contains(ESC));
    let off = run_with(&["--help"], &[("CLICOLOR_FORCE", "1"), ("NO_COLOR", "1")]);
    assert!(
        !String::from_utf8(off.stdout).unwrap().contains(ESC),
        "NO_COLOR wins"
    );
}

#[test]
fn no_argument_and_an_unknown_one_are_usage_errors() {
    for args in [&[][..], &["--serve"], &["serve", "now"]] {
        let output = run(args);
        assert_eq!(output.status.code(), Some(2), "{args:?}");
        assert!(output.stdout.is_empty());
    }
}

#[test]
fn serve_without_a_database_says_what_is_missing() {
    let output = run_with(&["serve"], &[("OPENBILLING_DATABASE_URL", "")]);
    assert_eq!(output.status.code(), Some(1));
    let stderr = String::from_utf8(output.stderr).unwrap();
    assert!(stderr.contains("OPENBILLING_DATABASE_URL"), "{stderr}");
}
