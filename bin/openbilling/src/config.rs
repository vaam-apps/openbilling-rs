//! The service's configuration, read from the environment through a lookup function (so it can be tested
//! without `std::env::set_var`, which is `unsafe` in edition 2024 and this workspace forbids `unsafe_code`), and
//! from one optional YAML file that `OPENBILLING_CONFIG` names.
//!
//! **Where a setting lives.** The file holds the deployer's policy; the environment holds the secrets and the
//! endpoints (the database, the address, the token issuer, the payment provider). No setting is read from both, so
//! neither overrides the other: a key the file does not know, such as a database URL, is refused, and the policy has
//! no variable. With no file, the policy is its default.
//!
//! The database URL can carry a password: it is read from `OPENBILLING_DATABASE_URL` or from the file
//! `OPENBILLING_DATABASE_URL_FILE` names, and no message, `Debug` or error ever holds it.
//!
//! The file, all of it optional (the owner's decision of 2026-11-27: the service reports, and the deployer decides
//! how `getAllowed` judges):
//!
//! ```yaml
//! allowed:
//!   debt_limit: at_or_above   # refuse when debt >= limit; or above (debt > limit); or never (report only)
//!   balance: at_or_above      # refuse when debt >= the drawable credit (net <= 0); or above (net < 0); or never
//!   member_cap: at_or_above   # refuse when the member's usage this month >= cap; or above; or never
//! retention:
//!   margin: 90d               # drop a day's usage partition this long after the intake window; 1d to 3650d
//! ```
//!
//! The retention job (ADR 0001, point 10, amended 2026-11-28) runs hourly with the partition job, so it has no
//! interval of its own.

use std::fmt;
use std::net::SocketAddr;
use std::path::PathBuf;
use std::time::Duration;

use anyhow::{Context, Result, bail};
use openbilling_core::{AllowedPolicy, CurrencyCode, RefuseWhen, RetentionMargin};
use openbilling_jwt::{JwtConfig, RoleMap};
use openbilling_vpay::{Credential, CurrencyUnit, VpayConfig, WebhookSecret, webhook};

/// The default address: this machine only. With an issuer configured, set `OPENBILLING_LISTEN` to serve beyond it.
pub const DEFAULT_LISTEN: &str = "127.0.0.1:8080";

/// A database URL, never printed.
pub struct DatabaseUrl(String);

impl DatabaseUrl {
    pub fn expose(&self) -> &str {
        &self.0
    }
}

impl fmt::Debug for DatabaseUrl {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str("DatabaseUrl(<hidden>)")
    }
}

/// Everything `serve` needs.
#[derive(Debug)]
pub struct Settings {
    pub database_url: DatabaseUrl,
    pub listen: SocketAddr,
    pub vpay: Option<VpayConfig>,
    /// The issuer whose tokens the API accepts; `None`: every `/v1` call is refused (`RefuseAll`).
    pub auth: Option<JwtConfig>,
    /// How `getAllowed` compares its numbers: the configuration file's `allowed`, or the default.
    pub allowed: AllowedPolicy,
    /// How long after the window a day's usage partition is dropped: the configuration file's `retention.margin`, or
    /// 90 days.
    pub retention_margin: RetentionMargin,
    /// OpenTelemetry, from the standard `OTEL_*` variables; `None` unless an endpoint is set.
    pub otel: Option<openbilling_telemetry::Settings>,
}

/// The largest configuration file read: it holds a few lines of policy.
pub const MAX_CONFIG_BYTES: u64 = 64 * 1024;

/// The configuration file. Every level refuses a key it does not know.
#[derive(Debug, Default, serde::Deserialize)]
#[serde(deny_unknown_fields)]
struct File {
    #[serde(default)]
    allowed: AllowedFile,
    #[serde(default)]
    retention: RetentionFile,
}

/// `retention:`: how long a usage partition outlives the window, as a number of days with a `d`: `90d`.
#[derive(Debug, Default, serde::Deserialize)]
#[serde(deny_unknown_fields)]
struct RetentionFile {
    margin: Option<String>,
}

/// A margin as the file writes it: digits and a `d`, one day to ten years.
fn parse_margin(text: &str) -> Option<RetentionMargin> {
    let digits = text.strip_suffix('d')?;
    if digits.is_empty() || !digits.bytes().all(|b| b.is_ascii_digit()) || digits.len() > 5 {
        return None;
    }
    RetentionMargin::new(digits.parse().ok()?).ok()
}

/// What the configuration file holds; the defaults when it names none.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct FileSettings {
    pub allowed: AllowedPolicy,
    pub retention_margin: RetentionMargin,
}

/// `allowed:`: one comparison per check of `getAllowed`, each `at_or_above` when left out.
#[derive(Debug, Default, serde::Deserialize)]
#[serde(deny_unknown_fields)]
struct AllowedFile {
    #[serde(default)]
    debt_limit: Mode,
    #[serde(default)]
    balance: Mode,
    #[serde(default)]
    member_cap: Mode,
}

/// A comparison as the file names it; [`RefuseWhen`] in the core.
#[derive(Clone, Copy, Debug, Default, serde::Deserialize)]
#[serde(rename_all = "snake_case")]
enum Mode {
    #[default]
    AtOrAbove,
    Above,
    Never,
}

impl From<Mode> for RefuseWhen {
    fn from(mode: Mode) -> Self {
        match mode {
            Mode::AtOrAbove => Self::AtOrAbove,
            Mode::Above => Self::Above,
            Mode::Never => Self::Never,
        }
    }
}

/// What the file says, refused whole when anything in it is not the configuration. The message names the line and
/// column and the keys there are, never a value from the file (agent guide, rule 9).
pub fn parse_config(yaml: &str) -> Result<FileSettings> {
    if yaml.trim().is_empty() {
        bail!(
            "OPENBILLING_CONFIG names an empty file; write `allowed: {{}}` for the defaults, or unset it"
        );
    }
    let file: File = serde_saphyr::from_str(yaml).map_err(|error| {
        let at = error.location().map_or_else(String::new, |at| {
            format!(" at line {}, column {}", at.line(), at.column())
        });
        anyhow::anyhow!(
            "OPENBILLING_CONFIG is not a valid configuration{at}: it holds only `allowed`, whose keys are \
             debt_limit, balance and member_cap, each at_or_above, above or never, and `retention`, whose key is \
             margin, a number of days written like 90d"
        )
    })?;
    let retention_margin = match file.retention.margin {
        None => RetentionMargin::default(),
        Some(text) => parse_margin(&text).context(
            "OPENBILLING_CONFIG: retention.margin is a number of days from 1 to 3650 written like 90d",
        )?,
    };
    Ok(FileSettings {
        allowed: AllowedPolicy {
            debt_limit: file.allowed.debt_limit.into(),
            balance: file.allowed.balance.into(),
            member_cap: file.allowed.member_cap.into(),
        },
        retention_margin,
    })
}

/// What the file `OPENBILLING_CONFIG` names holds, or the defaults when it names none.
fn file_settings(get: &impl Fn(&str) -> Option<String>) -> Result<FileSettings> {
    let Some(path) = get("OPENBILLING_CONFIG") else {
        return Ok(FileSettings::default());
    };
    let path = path.trim();
    let size = std::fs::metadata(path)
        .context("OPENBILLING_CONFIG could not be read")?
        .len();
    if size > MAX_CONFIG_BYTES {
        bail!("OPENBILLING_CONFIG names a file larger than {MAX_CONFIG_BYTES} bytes");
    }
    parse_config(&read_file(path, "OPENBILLING_CONFIG")?)
}

fn read_file(path: &str, what: &str) -> Result<String> {
    // The error names the variable, never the file's contents.
    std::fs::read_to_string(path).with_context(|| format!("{what} could not be read"))
}

impl Settings {
    /// The settings `lookup` gives, `lookup(name)` answering an environment variable.
    pub fn from_lookup(lookup: impl Fn(&str) -> Option<String>) -> Result<Self> {
        let get = |name: &str| lookup(name).filter(|v| !v.trim().is_empty());
        let database_url = match (
            get("OPENBILLING_DATABASE_URL"),
            get("OPENBILLING_DATABASE_URL_FILE"),
        ) {
            (Some(url), None) => url,
            (None, Some(path)) => read_file(&path, "OPENBILLING_DATABASE_URL_FILE")?
                .trim()
                .to_owned(),
            (Some(_), Some(_)) => {
                bail!("set OPENBILLING_DATABASE_URL or OPENBILLING_DATABASE_URL_FILE, not both")
            }
            (None, None) => {
                bail!("set OPENBILLING_DATABASE_URL or OPENBILLING_DATABASE_URL_FILE")
            }
        };
        let listen = get("OPENBILLING_LISTEN")
            .unwrap_or_else(|| DEFAULT_LISTEN.to_owned())
            .parse()
            .context("OPENBILLING_LISTEN is not an address such as 127.0.0.1:8080")?;
        let vpay = match get("OPENBILLING_VPAY_BASE_URL") {
            None => None,
            Some(base_url) => Some(vpay(base_url, &get)?),
        };
        // The error names the variable and never its value; a headers file is read here and nowhere else.
        let otel = openbilling_telemetry::Settings::from_lookup(
            &lookup,
            env!("CARGO_PKG_VERSION"),
            |path| std::fs::read_to_string(path).ok(),
        )
        .context("the OpenTelemetry configuration")?;
        let file = file_settings(&get)?;
        Ok(Self {
            database_url: DatabaseUrl(database_url),
            listen,
            vpay,
            auth: auth(&get)?,
            allowed: file.allowed,
            retention_margin: file.retention_margin,
            otel,
        })
    }
}

/// The token verifier's settings: an issuer and an audience together, or neither.
fn auth(get: &impl Fn(&str) -> Option<String>) -> Result<Option<JwtConfig>> {
    let (issuer, audience) = match (
        get("OPENBILLING_AUTH_ISSUER"),
        get("OPENBILLING_AUTH_AUDIENCE"),
    ) {
        (Some(issuer), Some(audience)) => (issuer, audience),
        (None, None) => {
            for name in [
                "OPENBILLING_AUTH_JWKS_URL",
                "OPENBILLING_AUTH_ROLES_CLAIM",
                "OPENBILLING_AUTH_ROLE_MAP",
            ] {
                if get(name).is_some() {
                    bail!("{name} needs OPENBILLING_AUTH_ISSUER and OPENBILLING_AUTH_AUDIENCE");
                }
            }
            return Ok(None);
        }
        _ => {
            bail!("set OPENBILLING_AUTH_ISSUER and OPENBILLING_AUTH_AUDIENCE together, or neither")
        }
    };
    let mut config = JwtConfig::new(issuer.trim(), audience.trim());
    config.jwks_url = get("OPENBILLING_AUTH_JWKS_URL").map(|url| url.trim().to_owned());
    if let Some(claim) = get("OPENBILLING_AUTH_ROLES_CLAIM") {
        claim.trim().clone_into(&mut config.roles_claim);
    }
    if let Some(map) = get("OPENBILLING_AUTH_ROLE_MAP") {
        config.roles = RoleMap::parse(&map).context(
            "OPENBILLING_AUTH_ROLE_MAP is not a list of value=operator or value=producer",
        )?;
    }
    Ok(Some(config))
}

fn vpay(base_url: String, get: &impl Fn(&str) -> Option<String>) -> Result<VpayConfig> {
    let token = get("OPENBILLING_VPAY_TOKEN_FILE")
        .context("OPENBILLING_VPAY_TOKEN_FILE is needed with OPENBILLING_VPAY_BASE_URL")?;
    let secrets_file = get("OPENBILLING_VPAY_WEBHOOK_SECRETS_FILE").context(
        "OPENBILLING_VPAY_WEBHOOK_SECRETS_FILE is needed with OPENBILLING_VPAY_BASE_URL",
    )?;
    let webhook_secrets: Vec<WebhookSecret> =
        read_file(&secrets_file, "OPENBILLING_VPAY_WEBHOOK_SECRETS_FILE")?
            .lines()
            .filter_map(|line| WebhookSecret::new(line.trim()))
            .collect();
    let currencies = get("OPENBILLING_VPAY_CURRENCIES")
        .context("OPENBILLING_VPAY_CURRENCIES is needed with OPENBILLING_VPAY_BASE_URL")?
        .split(',')
        .map(|entry| {
            let (code, exponent) = entry
                .trim()
                .split_once(':')
                .context("OPENBILLING_VPAY_CURRENCIES entries are CODE:EXPONENT")?;
            Ok(CurrencyUnit {
                currency: CurrencyCode::new(code)
                    .context("OPENBILLING_VPAY_CURRENCIES names a code that is not a currency")?,
                exponent: exponent
                    .parse()
                    .context("OPENBILLING_VPAY_CURRENCIES has an exponent that is not a number")?,
            })
        })
        .collect::<Result<Vec<_>>>()?;
    let payment_method_types = get("OPENBILLING_VPAY_PAYMENT_METHOD_TYPES")
        .context("OPENBILLING_VPAY_PAYMENT_METHOD_TYPES is needed with OPENBILLING_VPAY_BASE_URL")?
        .split(',')
        .map(|t| t.trim().to_owned())
        .collect();
    let webhook_tolerance_secs = match get("OPENBILLING_VPAY_WEBHOOK_TOLERANCE_SECS") {
        None => webhook::DEFAULT_TOLERANCE_SECS,
        Some(secs) => secs
            .parse()
            .context("OPENBILLING_VPAY_WEBHOOK_TOLERANCE_SECS is not a number of seconds")?,
    };
    Ok(VpayConfig {
        base_url,
        credential: Credential::TokenFile(PathBuf::from(token)),
        webhook_secrets,
        webhook_tolerance_secs,
        currencies,
        payment_method_types,
        timeout: Duration::from_secs(10),
    })
}

#[cfg(test)]
mod tests {
    use std::collections::BTreeMap;

    use super::*;

    fn settings(vars: &[(&str, &str)]) -> Result<Settings> {
        let vars: BTreeMap<String, String> = vars
            .iter()
            .map(|(k, v)| ((*k).to_owned(), (*v).to_owned()))
            .collect();
        Settings::from_lookup(|name| vars.get(name).cloned())
    }

    #[test]
    fn the_database_url_is_needed_once_and_never_shown() {
        let url = "postgres://user:hunter2@db/x";
        let ok = settings(&[("OPENBILLING_DATABASE_URL", url)]).unwrap();
        assert_eq!(ok.database_url.expose(), url);
        assert_eq!(ok.listen.to_string(), DEFAULT_LISTEN);
        assert!(ok.vpay.is_none());
        assert!(ok.auth.is_none());
        assert_eq!(ok.allowed, AllowedPolicy::default());
        assert_eq!(ok.retention_margin.days(), 90);
        assert!(
            ok.otel.is_none(),
            "telemetry is off unless an endpoint is set"
        );
        assert!(!format!("{ok:?}").contains("hunter2"));
        assert!(settings(&[]).is_err());
        let both = settings(&[
            ("OPENBILLING_DATABASE_URL", url),
            ("OPENBILLING_DATABASE_URL_FILE", "/nowhere"),
        ])
        .unwrap_err();
        assert!(!format!("{both:#}").contains("hunter2"));
        let missing =
            settings(&[("OPENBILLING_DATABASE_URL_FILE", "/nowhere/at/all")]).unwrap_err();
        assert!(format!("{missing:#}").contains("OPENBILLING_DATABASE_URL_FILE"));
    }

    #[test]
    fn the_issuer_and_the_audience_come_together_or_not_at_all() {
        let db = ("OPENBILLING_DATABASE_URL", "postgres://db/x");
        let issuer = ("OPENBILLING_AUTH_ISSUER", "https://idp.example/realms/a");
        let audience = ("OPENBILLING_AUTH_AUDIENCE", "billing");
        let auth = settings(&[db, issuer, audience]).unwrap().auth.unwrap();
        assert_eq!(auth.issuer, "https://idp.example/realms/a");
        assert_eq!(auth.audience, "billing");
        assert_eq!(auth.roles_claim, "roles");
        assert!(auth.jwks_url.is_none());
        assert!(settings(&[db, issuer]).is_err());
        // The chart renders the five variables empty when unset: empty is unset, so RefuseAll stays.
        let empty = settings(&[
            db,
            ("OPENBILLING_AUTH_ISSUER", ""),
            ("OPENBILLING_AUTH_AUDIENCE", " "),
            ("OPENBILLING_AUTH_JWKS_URL", ""),
            ("OPENBILLING_AUTH_ROLES_CLAIM", ""),
            ("OPENBILLING_AUTH_ROLE_MAP", ""),
        ])
        .unwrap();
        assert!(empty.auth.is_none());
        assert!(settings(&[db, audience]).is_err());
        assert!(settings(&[db, ("OPENBILLING_AUTH_ROLE_MAP", "a=operator")]).is_err());
        let full = settings(&[
            db,
            issuer,
            audience,
            ("OPENBILLING_AUTH_JWKS_URL", "https://idp.example/keys"),
            ("OPENBILLING_AUTH_ROLES_CLAIM", "realm_access.roles"),
            (
                "OPENBILLING_AUTH_ROLE_MAP",
                "billing-admin=operator,meter=producer",
            ),
        ])
        .unwrap()
        .auth
        .unwrap();
        assert_eq!(full.jwks_url.as_deref(), Some("https://idp.example/keys"));
        assert_eq!(full.roles_claim, "realm_access.roles");
        let bad_map = settings(&[
            db,
            issuer,
            audience,
            ("OPENBILLING_AUTH_ROLE_MAP", "x=owner"),
        ]);
        assert!(bad_map.is_err());
    }

    #[test]
    fn telemetry_is_on_with_an_endpoint_and_a_bad_setting_is_refused_by_name() {
        let base = ("OPENBILLING_DATABASE_URL", "postgres://db/x");
        let on = settings(&[
            base,
            ("OTEL_EXPORTER_OTLP_ENDPOINT", "http://localhost:4318"),
        ])
        .unwrap()
        .otel
        .unwrap();
        assert_eq!(on.resource.service_version, env!("CARGO_PKG_VERSION"));
        let refused = settings(&[
            base,
            ("OTEL_EXPORTER_OTLP_ENDPOINT", "http://localhost:4318"),
            ("OTEL_EXPORTER_OTLP_HEADERS", "authorization=Bearer hunter2"),
        ])
        .unwrap_err();
        let text = format!("{refused:#}");
        assert!(text.contains("OTEL_EXPORTER_OTLP_HEADERS"), "{text}");
        assert!(!text.contains("hunter2"), "{text}");
    }

    #[test]
    fn vpay_is_configured_whole_or_not_at_all() {
        let dir = std::env::temp_dir().join(format!("ob-config-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let secrets = dir.join("secrets");
        std::fs::write(&secrets, "whsec_a\n\nwhsec_b\n").unwrap();
        let secrets = secrets.to_str().unwrap();
        let base = [
            ("OPENBILLING_DATABASE_URL", "postgres://db/x"),
            ("OPENBILLING_VPAY_BASE_URL", "https://vpay.example"),
            ("OPENBILLING_VPAY_TOKEN_FILE", "/run/token"),
            ("OPENBILLING_VPAY_WEBHOOK_SECRETS_FILE", secrets),
            ("OPENBILLING_VPAY_CURRENCIES", "EUR:2, USD:2"),
            ("OPENBILLING_VPAY_PAYMENT_METHOD_TYPES", "rail_a,rail_b"),
        ];
        let vpay = settings(&base).unwrap().vpay.unwrap();
        assert_eq!(vpay.webhook_secrets.len(), 2);
        assert_eq!(vpay.currencies.len(), 2);
        assert_eq!(vpay.currencies[0].exponent, 2);
        assert_eq!(vpay.payment_method_types, ["rail_a", "rail_b"]);
        assert_eq!(vpay.webhook_tolerance_secs, 300);
        assert!(!format!("{vpay:?}").contains("whsec_a"));
        for missing in 2..base.len() {
            let mut partial = base.to_vec();
            partial.remove(missing);
            assert!(settings(&partial).is_err(), "without {}", base[missing].0);
        }
        let mut bad = base.to_vec();
        bad[4] = ("OPENBILLING_VPAY_CURRENCIES", "EUR");
        assert!(settings(&bad).is_err());
        let _ = std::fs::remove_dir_all(&dir);
    }

    fn policy(
        debt_limit: RefuseWhen,
        balance: RefuseWhen,
        member_cap: RefuseWhen,
    ) -> AllowedPolicy {
        AllowedPolicy {
            debt_limit,
            balance,
            member_cap,
        }
    }

    #[test]
    fn the_file_sets_each_comparison_and_leaves_the_rest_at_or_above() {
        use RefuseWhen::{Above, AtOrAbove, Never};
        for (yaml, expected) in [
            ("allowed: {}", AllowedPolicy::default()),
            ("{}", AllowedPolicy::default()),
            ("# only a comment\nallowed: {}\n", AllowedPolicy::default()),
            (
                "allowed:\n  debt_limit: at_or_above\n  balance: at_or_above\n  member_cap: at_or_above\n",
                AllowedPolicy::default(),
            ),
            (
                "allowed:\n  debt_limit: above\n",
                policy(Above, AtOrAbove, AtOrAbove),
            ),
            (
                "allowed:\n  balance: never\n  member_cap: above\n",
                policy(AtOrAbove, Never, Above),
            ),
            (
                "allowed: {debt_limit: never, balance: never, member_cap: never}",
                policy(Never, Never, Never),
            ),
        ] {
            assert_eq!(parse_config(yaml).unwrap().allowed, expected, "{yaml:?}");
        }
    }

    #[test]
    fn the_retention_margin_defaults_to_ninety_days_and_takes_days_from_one_to_ten_years() {
        for (yaml, days) in [
            ("{}", 90),
            ("allowed: {}", 90),
            ("retention: {}", 90),
            ("retention:\n  margin: 90d\n", 90),
            ("retention:\n  margin: 1d\n", 1),
            ("retention:\n  margin: 3650d\n", 3650),
            (
                "retention:\n  margin: \"30d\"\nallowed: {balance: never}\n",
                30,
            ),
        ] {
            assert_eq!(
                parse_config(yaml).unwrap().retention_margin.days(),
                days,
                "{yaml:?}"
            );
        }
        for bad in [
            "retention:\n  margin: 0d\n",
            "retention:\n  margin: 3651d\n",
            "retention:\n  margin: 99999d\n",
            "retention:\n  margin: 090000d\n",
            "retention:\n  margin: 90\n",
            "retention:\n  margin: d\n",
            "retention:\n  margin: 90D\n",
            "retention:\n  margin: 90 d\n",
            "retention:\n  margin: -1d\n",
            "retention:\n  margin: 1.5d\n",
            "retention:\n  margin: 90h\n",
            "retention:\n  margin: [90d]\n",
            "retention:\n  margin: \"\"\n",
            "retention:\n  every: 1h\n",
            "retention: 90d\n",
            "retention:\n  margin: 1d\n  margin: 2d\n",
        ] {
            let error = parse_config(bad).expect_err(bad);
            assert!(
                format!("{error:#}").contains("OPENBILLING_CONFIG"),
                "{bad:?}"
            );
        }
    }

    #[test]
    fn the_file_refuses_what_it_does_not_know_without_quoting_it() {
        for yaml in [
            "",
            "  \n",
            "allowed:\n  debt_limit: below\n",
            "allowed:\n  debt_limit: At_Or_Above\n",
            "allowed:\n  debt_limit: \"\"\n",
            "allowed:\n  debt_limit: 0\n",
            "allowed:\n  debt_limit: [above]\n",
            "allowed:\n  debt_limt: above\n",
            "allowed:\n  debt_limit: above\n  debt_limit: never\n",
            "allowed: never\n",
            "database_url: postgres://user:hunter2@db/x\n",
            "allowed: {}\nlisten: 0.0.0.0:8080\n",
            "- allowed\n",
            "allowed: {debt_limit: above",
        ] {
            let error = parse_config(yaml).expect_err(yaml);
            let message = format!("{error:#}");
            assert!(message.contains("OPENBILLING_CONFIG"), "{message}");
            for quoted in ["hunter2", "below", "At_Or_Above", "debt_limt", "listen"] {
                assert!(
                    !yaml.contains(quoted) || !message.contains(quoted),
                    "{yaml:?} quoted in {message}"
                );
            }
        }
        let located = format!(
            "{:#}",
            parse_config("allowed:\n  balance: none\n").unwrap_err()
        );
        assert!(located.contains("line 2"), "{located}");
    }

    #[test]
    fn the_file_is_read_when_named_and_refused_when_it_cannot_be() {
        let dir = std::env::temp_dir().join(format!("ob-config-file-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let db = ("OPENBILLING_DATABASE_URL", "postgres://db/x");
        let file = dir.join("config.yaml");
        std::fs::write(
            &file,
            "allowed:\n  debt_limit: above\n  member_cap: never\n",
        )
        .unwrap();
        let named = settings(&[db, ("OPENBILLING_CONFIG", file.to_str().unwrap())]).unwrap();
        assert_eq!(
            named.allowed,
            policy(RefuseWhen::Above, RefuseWhen::AtOrAbove, RefuseWhen::Never)
        );
        // Empty is unset, as for every variable the chart renders.
        let unset = settings(&[db, ("OPENBILLING_CONFIG", " ")]).unwrap();
        assert_eq!(unset.allowed, AllowedPolicy::default());
        let missing = settings(&[db, ("OPENBILLING_CONFIG", "/nowhere/config.yaml")]).unwrap_err();
        assert!(format!("{missing:#}").contains("OPENBILLING_CONFIG"));
        let bad = dir.join("bad.yaml");
        std::fs::write(&bad, "allowed:\n  debt_limit: hunter2\n").unwrap();
        let refused = settings(&[db, ("OPENBILLING_CONFIG", bad.to_str().unwrap())]).unwrap_err();
        let message = format!("{refused:#}");
        assert!(
            message.contains("OPENBILLING_CONFIG") && !message.contains("hunter2"),
            "{message}"
        );
        let big = dir.join("big.yaml");
        let padding = "#".repeat(usize::try_from(MAX_CONFIG_BYTES).unwrap());
        std::fs::write(&big, format!("allowed: {{}}\n{padding}\n")).unwrap();
        assert!(settings(&[db, ("OPENBILLING_CONFIG", big.to_str().unwrap())]).is_err());
        let _ = std::fs::remove_dir_all(&dir);
    }
}
