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
    assert!(stdout.contains("OPENBILLING_CONFIG"), "{stdout}");
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

/// The configuration file is checked at start, before any connection: a bad one stops `serve` with exit 1, saying
/// which variable and where, never what the file holds.
#[test]
fn serve_refuses_a_configuration_file_it_does_not_understand() {
    let dir = std::env::temp_dir().join(format!("ob-cli-config-{}", std::process::id()));
    std::fs::create_dir_all(&dir).unwrap();
    let bad = dir.join("config.yaml");
    std::fs::write(&bad, "allowed:\n  debt_limit: sometimes\n").unwrap();
    // A database that is never reached: the file is refused first.
    let db = (
        "OPENBILLING_DATABASE_URL",
        "postgres://nobody@127.0.0.1:1/none",
    );
    for config in [bad.to_str().unwrap(), "/nowhere/config.yaml"] {
        let output = run_with(&["serve"], &[db, ("OPENBILLING_CONFIG", config)]);
        assert_eq!(output.status.code(), Some(1), "{config}");
        let stderr = String::from_utf8(output.stderr).unwrap();
        assert!(stderr.contains("OPENBILLING_CONFIG"), "{stderr}");
        assert!(!stderr.contains("sometimes"), "{stderr}");
        assert!(!stderr.contains("connecting"), "{stderr}");
    }
    let _ = std::fs::remove_dir_all(&dir);
}

/// A fresh, empty database on the server `OPENBILLING_TEST_DATABASE_URL` names, as a URL, or `None` to skip
/// (agent guide, rule 10: a failure instead under `OPENBILLING_TEST_REQUIRE_DB=1`). Left behind, as the other
/// crates' test databases are: CI's server is thrown away.
fn fresh_database() -> Option<String> {
    use sqlx::postgres::PgConnectOptions;
    use sqlx::{AssertSqlSafe, ConnectOptions, Connection, Executor};

    let url = match std::env::var("OPENBILLING_TEST_DATABASE_URL") {
        Ok(url) if !url.trim().is_empty() => url,
        _ => {
            let required = std::env::var("OPENBILLING_TEST_REQUIRE_DB").is_ok_and(|v| v == "1");
            assert!(
                !required,
                "OPENBILLING_TEST_REQUIRE_DB=1 but OPENBILLING_TEST_DATABASE_URL is unset"
            );
            eprintln!("skipped: OPENBILLING_TEST_DATABASE_URL is unset");
            return None;
        }
    };
    let admin: PgConnectOptions = url.parse().unwrap();
    let name = format!("ob_test_cli_{}", std::process::id());
    tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .unwrap()
        .block_on(async {
            let mut conn = sqlx::PgConnection::connect_with(&admin).await.unwrap();
            conn.execute(AssertSqlSafe(format!("DROP DATABASE IF EXISTS {name}")))
                .await
                .unwrap();
            conn.execute(AssertSqlSafe(format!("CREATE DATABASE {name}")))
                .await
                .unwrap();
            conn.close().await.unwrap();
        });
    Some(admin.database(&name).to_url_lossy().to_string())
}

/// `serve` stops on `SIGTERM`, which is how Kubernetes stops a pod. As PID 1 in a container, a process without a
/// handler would ignore it and be killed at the end of its grace period.
#[cfg(unix)]
#[test]
fn serve_stops_cleanly_on_sigterm() {
    use std::io::{BufRead, BufReader};
    use std::process::Stdio;
    use std::sync::mpsc;
    use std::time::{Duration, Instant};

    let Some(url) = fresh_database() else {
        return;
    };
    let mut child = Command::new(env!("CARGO_BIN_EXE_openbilling"))
        .arg("serve")
        .env("OPENBILLING_DATABASE_URL", url)
        .env("OPENBILLING_LISTEN", "127.0.0.1:0")
        .env_remove("OPENBILLING_DATABASE_URL_FILE")
        .env_remove("OPENBILLING_VPAY_BASE_URL")
        .stdout(Stdio::null())
        .stderr(Stdio::piped())
        .spawn()
        .unwrap();
    let stderr = child.stderr.take().unwrap();
    let (lines, received) = mpsc::channel();
    std::thread::spawn(move || {
        for line in BufReader::new(stderr).lines().map_while(Result::ok) {
            if lines.send(line).is_err() {
                break;
            }
        }
    });
    let deadline = Instant::now() + Duration::from_secs(60);
    loop {
        let line = received
            .recv_timeout(deadline.saturating_duration_since(Instant::now()))
            .unwrap_or_else(|_| {
                let _ = child.kill();
                panic!("serve did not say it was listening");
            });
        if line.contains("listening on") {
            break;
        }
    }
    let killed = Command::new("kill")
        .args(["-TERM", &child.id().to_string()])
        .status()
        .unwrap();
    assert!(killed.success());
    let deadline = Instant::now() + Duration::from_secs(10);
    let status = loop {
        if let Some(status) = child.try_wait().unwrap() {
            break status;
        }
        if Instant::now() > deadline {
            let _ = child.kill();
            panic!("serve did not stop within 10 s of SIGTERM");
        }
        std::thread::sleep(Duration::from_millis(50));
    };
    assert!(status.success(), "serve exits 0 on SIGTERM: {status:?}");
}
