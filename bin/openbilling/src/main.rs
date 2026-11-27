//! The openbilling-rs service. `serve` runs the API, the provider's webhook and the drain. The binary only
//! composes the crates (agent guide, rule 4).
//!
//! The command line is clap's. Its help, and the binary's own status lines and refusals, are coloured through
//! `anstream`, which writes plain text when the stream is not a terminal, and honours `NO_COLOR` and
//! `CLICOLOR_FORCE`.

use std::process::ExitCode;

use anstyle::{AnsiColor, Effects, Style};
use clap::builder::Styles;
use clap::{Parser, Subcommand};

mod config;
mod serve;

/// Headers bold, literals in the accent colour, placeholders dimmed.
const STYLES: Styles = Styles::styled()
    .header(Style::new().effects(Effects::BOLD))
    .usage(Style::new().effects(Effects::BOLD))
    .literal(AnsiColor::Cyan.on_default().effects(Effects::BOLD))
    .placeholder(Style::new().effects(Effects::DIMMED))
    .error(AnsiColor::Red.on_default().effects(Effects::BOLD))
    .valid(AnsiColor::Green.on_default())
    .invalid(AnsiColor::Yellow.on_default());

/// The accent of the binary's own status lines.
pub const ACCENT: Style = AnsiColor::Cyan.on_default().effects(Effects::BOLD);
/// A warning.
pub const WARN: Style = AnsiColor::Yellow.on_default().effects(Effects::BOLD);
/// A refusal.
pub const ERROR: Style = AnsiColor::Red.on_default().effects(Effects::BOLD);

const ENVIRONMENT: &str = "\
serve reads its policy from an optional YAML file, and its secrets and endpoints from the
environment. No setting is read from both:
  OPENBILLING_CONFIG
      The YAML file of policy, checked at start: an unknown key or value is refused.
      Unset: every check at_or_above. A check is at_or_above (refuse when used >= bound),
      above (used > bound) or never (report only):
        allowed:
          debt_limit: at_or_above   # the debt against its limit
          balance: at_or_above      # the debt against the drawable credit
          member_cap: at_or_above   # a member's usage this month against the cap
  OPENBILLING_DATABASE_URL, or OPENBILLING_DATABASE_URL_FILE (a file holding it)
      The PostgreSQL 19 database. Never printed.
  OPENBILLING_LISTEN
      The address to listen on; default 127.0.0.1:8080, this machine only.
  OPENBILLING_AUTH_ISSUER, OPENBILLING_AUTH_AUDIENCE
      The issuer whose JWT bearer tokens /v1 accepts (its iss, exactly; its keys from its discovery
      document) and the audience they must name. Both or neither; unset: every /v1 call answers 401.
  OPENBILLING_AUTH_JWKS_URL
      The issuer's JWKS, instead of the one its discovery document names.
  OPENBILLING_AUTH_ROLES_CLAIM
      The claim that holds the roles, a dotted path; default roles.
  OPENBILLING_AUTH_ROLE_MAP
      How the claim's values map to roles, such as admin=operator,meter=producer;
      default operator=operator,producer=producer.
  OPENBILLING_VPAY_BASE_URL
      vpay's base URL. Unset: no payment provider, and top-ups and webhooks answer 503.
  OPENBILLING_VPAY_TOKEN_FILE
      A file holding the bearer token for vpay's /v1, read on every call.
  OPENBILLING_VPAY_WEBHOOK_SECRETS_FILE
      A file holding vpay's webhook secrets, one per line.
  OPENBILLING_VPAY_CURRENCIES
      Currencies and their minor-unit exponents, such as EUR:2,USD:2.
  OPENBILLING_VPAY_PAYMENT_METHOD_TYPES
      The payment method types every intent offers, comma-separated.
  OPENBILLING_VPAY_WEBHOOK_TOLERANCE_SECS
      How old a webhook signature may be; default 300.";

/// Usage billing on PostgreSQL alone.
#[derive(Debug, Parser)]
#[command(name = "openbilling", version, styles = STYLES, after_long_help = ENVIRONMENT)]
struct Cli {
    #[command(subcommand)]
    command: Command,
}

#[derive(Debug, PartialEq, Eq, Subcommand)]
enum Command {
    /// Apply the migrations, then serve the API, the payment provider's webhook and the drain
    Serve,
}

fn refuse(error: &anyhow::Error) -> ExitCode {
    anstream::eprintln!("{ERROR}error:{ERROR:#} {error:#}");
    ExitCode::FAILURE
}

fn main() -> ExitCode {
    let cli = Cli::parse();
    match cli.command {
        Command::Serve => {
            let settings = match config::Settings::from_lookup(|name| std::env::var(name).ok()) {
                Ok(settings) => settings,
                Err(error) => return refuse(&error),
            };
            let runtime = match tokio::runtime::Builder::new_multi_thread()
                .enable_all()
                .build()
            {
                Ok(runtime) => runtime,
                Err(error) => return refuse(&error.into()),
            };
            match runtime.block_on(serve::serve(settings)) {
                Ok(()) => ExitCode::SUCCESS,
                Err(error) => refuse(&error),
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use clap::CommandFactory;

    use super::*;

    fn parse(args: &[&str]) -> Result<Cli, clap::Error> {
        Cli::try_parse_from(std::iter::once("openbilling").chain(args.iter().copied()))
    }

    #[test]
    fn the_command_line_is_well_formed() {
        Cli::command().debug_assert();
    }

    #[test]
    fn serve_and_nothing_else() {
        assert_eq!(parse(&["serve"]).unwrap().command, Command::Serve);
        for bad in [&[][..], &["--serve"], &["serve", "now"], &["unknown"]] {
            let error = parse(bad).unwrap_err();
            assert_eq!(error.exit_code(), 2, "{bad:?}");
        }
        assert_eq!(parse(&["--help"]).unwrap_err().exit_code(), 0);
        assert_eq!(parse(&["--version"]).unwrap_err().exit_code(), 0);
    }
}
