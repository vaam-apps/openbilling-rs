//! The facade's own tests: no service, no database.

#![allow(clippy::unwrap_used, clippy::expect_used)]

use openbilling_sdk::{Classify, Client, Error, ErrorClass, IdempotencyKey};

#[test]
fn a_key_is_visible_ascii_of_one_to_255_characters() {
    assert!(IdempotencyKey::new("open-acct-1").is_ok());
    assert!(IdempotencyKey::new("x".repeat(255)).is_ok());
    for bad in [
        String::new(),
        "x".repeat(256),
        "has space".to_owned(),
        "é".to_owned(),
    ] {
        assert!(matches!(
            IdempotencyKey::new(bad),
            Err(Error::IdempotencyKey(_))
        ));
    }
}

#[test]
fn a_base_url_is_http_or_https_with_no_credentials() {
    assert!(Client::new("https://billing.example.com").is_ok());
    assert!(Client::new("http://127.0.0.1:8080/").is_ok());
    for bad in [
        "not a url",
        "ftp://x.example",
        "mailto:a@b.example",
        "https://u:p@x.example",
    ] {
        assert!(matches!(Client::new(bad), Err(Error::BaseUrl(_))), "{bad}");
    }
}

#[test]
fn the_token_is_in_no_debug_output_and_no_error() {
    let client = Client::new("https://billing.example.com").unwrap();
    client.set_token("SECRET-TOKEN-VALUE");
    assert!(!format!("{client:?}").contains("SECRET"));
    let error = Error::Remote {
        status: 401,
        message: "unauthorized".to_owned(),
    };
    assert!(!format!("{error} {error:?}").contains("SECRET"));
}

#[test]
fn every_error_has_the_class_a_caller_decides_from() {
    let remote = |status| Error::Remote {
        status,
        message: String::new(),
    };
    assert_eq!(remote(400).class(), ErrorClass::Invalid);
    assert_eq!(remote(401).class(), ErrorClass::Invalid);
    assert_eq!(remote(403).class(), ErrorClass::Invalid);
    assert_eq!(remote(404).class(), ErrorClass::Invalid);
    assert_eq!(remote(422).class(), ErrorClass::Invalid);
    assert_eq!(remote(409).class(), ErrorClass::Conflict);
    assert_eq!(remote(503).class(), ErrorClass::Unavailable);
    assert_eq!(remote(500).class(), ErrorClass::Internal);
    assert_eq!(
        Error::Transport(String::new()).class(),
        ErrorClass::Unavailable
    );
    assert_eq!(Error::Decode(String::new()).class(), ErrorClass::Internal);
}

/// A connection nothing answers is an `Unavailable` error, and the message names no token.
#[tokio::test]
async fn a_service_that_is_not_there_is_unavailable() {
    use openbilling_sdk::generated::cratestack_schema::{procedures, types};
    let client = Client::new("http://127.0.0.1:1").unwrap();
    client.set_token("SECRET-TOKEN-VALUE");
    let error = client
        .get_balance(&procedures::get_balance::Args {
            args: types::OwnerCurrency {
                owner: "o".to_owned(),
                currency: "EUR".to_owned(),
            },
        })
        .await
        .unwrap_err();
    assert_eq!(error.class(), ErrorClass::Unavailable, "{error}");
    assert!(!error.to_string().contains("SECRET"));
}

/// The schema this crate generates from is the server's, byte for byte. Skips in a packaged copy, where the server's
/// schema is not beside it; CI's `sdk` job runs the same comparison with `cmp`.
#[test]
fn the_schema_is_the_servers() {
    let here = std::path::Path::new(env!("CARGO_MANIFEST_DIR"));
    let server = here.join("../openbilling-http/schema/openbilling.cstack");
    let Ok(server) = std::fs::read(&server) else {
        eprintln!("skipped: not in the repository");
        return;
    };
    let ours = std::fs::read(here.join("schema/openbilling.cstack")).unwrap();
    assert!(
        ours == server,
        "crates/openbilling-sdk/schema/openbilling.cstack differs from the server's: \
         cp crates/openbilling-http/schema/openbilling.cstack crates/openbilling-sdk/schema/"
    );
}

fn snake(camel: &str) -> String {
    let mut out = String::new();
    for c in camel.chars() {
        if c.is_ascii_uppercase() {
            out.push('_');
            out.push(c.to_ascii_lowercase());
        } else {
            out.push(c);
        }
    }
    out
}

/// Every operation of the API has a call on the facade, and no call is for an operation that is gone: the
/// procedures of the schema, and the routes of `docs/api/ops.json` (when in the repository).
#[test]
fn every_operation_has_a_call() {
    let here = std::path::Path::new(env!("CARGO_MANIFEST_DIR"));
    let schema = std::fs::read_to_string(here.join("schema/openbilling.cstack")).unwrap();
    let mut procedures: Vec<(String, bool)> = schema
        .lines()
        .filter_map(|line| {
            let (write, rest) = match line.strip_prefix("mutation procedure ") {
                Some(rest) => (true, rest),
                None => (false, line.strip_prefix("procedure ")?),
            };
            Some((snake(rest.split('(').next()?), write))
        })
        .collect();
    procedures.sort();
    let source = include_str!("../src/client.rs");
    let mut calls: Vec<(String, bool)> = Vec::new();
    let mut write = false;
    for line in source.lines() {
        match line.trim() {
            "reads! {" => write = false,
            "writes! {" => write = true,
            name if name.ends_with(',')
                && name.len() > 1
                && name[..name.len() - 1]
                    .chars()
                    .all(|c| c.is_ascii_lowercase() || c == '_')
                && line.starts_with("        ")
                && !line.starts_with("         ") =>
            {
                calls.push((name[..name.len() - 1].to_owned(), write));
            }
            _ => {}
        }
    }
    calls.sort();
    assert_eq!(calls, procedures);
    assert_eq!(procedures.len(), 17);

    if let Ok(ops) = std::fs::read_to_string(here.join("../../docs/api/ops.json")) {
        let ops: serde_json::Value = serde_json::from_str(&ops).unwrap();
        let mut from_ops: Vec<String> = ops["ops"]
            .as_array()
            .unwrap()
            .iter()
            .filter_map(|op| {
                let key = op["key"].as_str()?;
                Some(snake(key.strip_prefix("POST /$procs/")?))
            })
            .collect();
        from_ops.sort();
        let names: Vec<String> = procedures.into_iter().map(|(n, _)| n).collect();
        assert_eq!(from_ops, names);
    }
}
