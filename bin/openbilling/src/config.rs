//! The service's configuration, read from the environment through a lookup function (so it can be tested
//! without `std::env::set_var`, which is `unsafe` in edition 2024 and this workspace forbids `unsafe_code`).
//!
//! The database URL can carry a password: it is read from `OPENBILLING_DATABASE_URL` or from the file
//! `OPENBILLING_DATABASE_URL_FILE` names, and no message, `Debug` or error ever holds it.

use std::fmt;
use std::net::SocketAddr;
use std::path::PathBuf;
use std::time::Duration;

use anyhow::{Context, Result, bail};
use openbilling_core::CurrencyCode;
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
        Ok(Self {
            database_url: DatabaseUrl(database_url),
            listen,
            vpay,
            auth: auth(&get)?,
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
}
