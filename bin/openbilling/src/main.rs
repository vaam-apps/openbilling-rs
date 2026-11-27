//! The openbilling-rs service. For now it answers `--help` and `--version` and exits: there is nothing to serve yet.

use std::ffi::OsString;
use std::io::{self, Write};

use anyhow::{Result, bail};

const VERSION: &str = env!("CARGO_PKG_VERSION");

const HELP: &str = "\
openbilling: usage billing on PostgreSQL alone

Usage: openbilling [OPTION]

Options:
  -h, --help     Print this help and exit
  -V, --version  Print the version and exit

Status: skeleton. The service does not run yet.
";

/// What the command line asks for.
#[derive(Debug, PartialEq, Eq)]
enum Command {
    Help,
    Version,
}

/// Reads the arguments after the program name.
fn parse(mut args: impl Iterator<Item = OsString>) -> Result<Command> {
    let Some(first) = args.next() else {
        bail!("nothing to run: the service is not built yet (see --help)");
    };
    if let Some(extra) = args.next() {
        bail!("unexpected argument {extra:?} (see --help)");
    }
    match first.to_str() {
        Some("-h" | "--help") => Ok(Command::Help),
        Some("-V" | "--version") => Ok(Command::Version),
        _ => bail!("unknown argument {first:?} (see --help)"),
    }
}

fn main() -> Result<()> {
    let command = parse(std::env::args_os().skip(1))?;
    let mut stdout = io::stdout().lock();
    match command {
        Command::Help => stdout.write_all(HELP.as_bytes())?,
        Command::Version => writeln!(stdout, "openbilling {VERSION}")?,
    }
    stdout.flush()?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn parse_strs(args: &[&str]) -> Result<Command> {
        parse(args.iter().map(OsString::from))
    }

    #[test]
    fn help_and_version_in_both_spellings() {
        assert_eq!(parse_strs(&["--help"]).unwrap(), Command::Help);
        assert_eq!(parse_strs(&["-h"]).unwrap(), Command::Help);
        assert_eq!(parse_strs(&["--version"]).unwrap(), Command::Version);
        assert_eq!(parse_strs(&["-V"]).unwrap(), Command::Version);
    }

    #[test]
    fn anything_else_is_refused() {
        assert!(parse_strs(&[]).is_err());
        assert!(parse_strs(&["--serve"]).is_err());
        assert!(parse_strs(&["--help", "--version"]).is_err());
    }

    #[test]
    fn an_unknown_argument_is_quoted_with_escapes() {
        let error = parse_strs(&["a\u{1b}[31mb"]).unwrap_err().to_string();
        assert!(!error.contains('\u{1b}'), "{error}");
    }
}
