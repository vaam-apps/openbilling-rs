//! Verifying a vpay webhook, exactly as vpay specifies, and naming what it is about.
//!
//! *Verified 2026-11-27*, vpay `8b56831`: `backends/crates/vpay-worker/src/signing.rs` (the sender) and
//! `sdks/rust/src/webhooks.rs` (vpay's own verifier, which this port follows rule for rule):
//!
//! - the header is `Vpay-Signature: t=<unix seconds>,v1=<hex>[,v1=<hex>…]`, one `v1=` per secret vpay holds for
//!   the endpoint (so a rotation sends two); parts are split on `,` and on the first `=`, keys and values trimmed,
//!   unknown keys ignored and an empty `v1=` dropped;
//! - `t` must be ASCII digits only, and there must be at least one `v1`; otherwise the header is malformed;
//! - the signed bytes are the literal text of `t`, a `.`, and the raw body: HMAC-SHA256, lower-case hex;
//! - a signature is accepted when `|now − t|` is at most the tolerance, 300 seconds by default, either way;
//! - candidates are compared in constant time, and the body is parsed only after one matched.
//!
//! The body is then read only for what to fetch again: the event's `type` and `data.object.id`. A callback is a
//! hint (ADR 0003; vpay's own rule, the `vpay` skill, "Callbacks are hints").

use hmac::{Hmac, KeyInit, Mac};
use openbilling_core::{CreditId, PaymentRefusal, Timestamp, WebhookHeaders, WebhookHint};
use serde::Deserialize;
use sha2::Sha256;
use subtle::ConstantTimeEq;

use crate::WebhookSecret;

type HmacSha256 = Hmac<Sha256>;

/// The header vpay signs with. It sends the same value as `Stripe-Signature`, which is not read.
pub const SIGNATURE_HEADER: &str = "vpay-signature";

/// vpay's default tolerance, in seconds (`DEFAULT_TOLERANCE` of its SDK).
pub const DEFAULT_TOLERANCE_SECS: u32 = 300;

struct Parsed<'a> {
    timestamp_text: &'a str,
    timestamp: i64,
    signatures: Vec<&'a str>,
}

fn parse_header(header: &str) -> Option<Parsed<'_>> {
    let mut timestamp_text = None;
    let mut signatures = Vec::new();
    for part in header.split(',') {
        let Some((key, value)) = part.split_once('=') else {
            continue;
        };
        match key.trim() {
            "t" => timestamp_text = Some(value.trim()),
            "v1" => {
                let value = value.trim();
                if !value.is_empty() {
                    signatures.push(value);
                }
            }
            _ => {}
        }
    }
    let timestamp_text = timestamp_text?;
    if timestamp_text.is_empty()
        || !timestamp_text.bytes().all(|b| b.is_ascii_digit())
        || signatures.is_empty()
    {
        return None;
    }
    // All digits and too large for an i64 is outside any tolerance, as vpay's verifier says.
    let timestamp = timestamp_text.parse::<i64>().unwrap_or(i64::MAX);
    Some(Parsed {
        timestamp_text,
        timestamp,
        signatures,
    })
}

fn expected(secret: &WebhookSecret, timestamp_text: &str, body: &[u8]) -> Option<Vec<u8>> {
    let mut mac = HmacSha256::new_from_slice(secret.expose().as_bytes()).ok()?;
    mac.update(timestamp_text.as_bytes());
    mac.update(b".");
    mac.update(body);
    Some(mac.finalize().into_bytes().to_vec())
}

fn matches(candidate: &str, expected: &[u8]) -> bool {
    match hex::decode(candidate) {
        Ok(bytes) if bytes.len() == expected.len() => bytes.ct_eq(expected).into(),
        _ => false,
    }
}

/// Verifies `body` against the `Vpay-Signature` header with any of `secrets`, at `now`.
pub(crate) fn verify(
    headers: &WebhookHeaders,
    body: &[u8],
    secrets: &[WebhookSecret],
    tolerance_secs: u32,
    now: Timestamp,
) -> Result<(), PaymentRefusal> {
    // One header only: two would let a sender pick which one a proxy forwards.
    let mut values = headers.all(SIGNATURE_HEADER);
    let (Some(header), None) = (values.next(), values.next()) else {
        return Err(PaymentRefusal::WebhookSignature);
    };
    let parsed = parse_header(header).ok_or(PaymentRefusal::WebhookSignature)?;
    let now_secs = now.unix_micros().div_euclid(1_000_000);
    let delta = now_secs
        .checked_sub(parsed.timestamp)
        .and_then(i64::checked_abs)
        .ok_or(PaymentRefusal::WebhookTimestamp)?;
    if delta > i64::from(tolerance_secs) {
        return Err(PaymentRefusal::WebhookTimestamp);
    }
    let matched = secrets.iter().any(|secret| {
        expected(secret, parsed.timestamp_text, body).is_some_and(|expected| {
            parsed
                .signatures
                .iter()
                .any(|candidate| matches(candidate, &expected))
        })
    });
    if matched {
        Ok(())
    } else {
        Err(PaymentRefusal::WebhookSignature)
    }
}

#[derive(Deserialize)]
struct Event {
    #[serde(rename = "type")]
    kind: String,
    data: EventData,
}

#[derive(Deserialize)]
struct EventData {
    object: EventObject,
}

#[derive(Deserialize)]
struct EventObject {
    id: String,
}

/// What a verified body names. `payment_intent.*` names a payment, `charge.refunded` and `charge.refund.updated`
/// a refund (*verified 2026-11-27*, vpay `8b56831`, migration `0039`'s `type_is_a_documented_event` and the
/// `vpay-webhooks` skill's writer table); every other type moves no credit.
pub(crate) fn hint(body: &[u8]) -> Result<WebhookHint, PaymentRefusal> {
    let event: Event = serde_json::from_slice(body).map_err(|_| PaymentRefusal::WebhookBody)?;
    let id =
        || CreditId::new(event.data.object.id.clone()).map_err(|_| PaymentRefusal::WebhookBody);
    if event.kind.starts_with("payment_intent.") {
        Ok(WebhookHint::Payment(id()?))
    } else if matches!(
        event.kind.as_str(),
        "charge.refunded" | "charge.refund.updated"
    ) {
        Ok(WebhookHint::Refund(id()?))
    } else {
        Ok(WebhookHint::Ignored)
    }
}

/// The `Vpay-Signature` value vpay would send for `body` at `at` with `secrets`, for tests and stand-ins: the same
/// construction as vpay's `signature_header`.
#[must_use]
pub fn signature_header(body: &[u8], at: Timestamp, secrets: &[WebhookSecret]) -> String {
    let timestamp_text = at.unix_micros().div_euclid(1_000_000).to_string();
    let mut header = format!("t={timestamp_text}");
    for secret in secrets {
        if let Some(mac) = expected(secret, &timestamp_text, body) {
            header.push_str(",v1=");
            header.push_str(&hex::encode(mac));
        }
    }
    header
}

#[cfg(test)]
mod tests {
    use super::*;

    const BODY: &[u8] = br#"{"id":"evt_1","object":"event","type":"payment_intent.succeeded","created":1753401600,"livemode":false,"data":{"object":{"id":"pi_1"}}}"#;
    const T: i64 = 1_753_401_600;

    /// The vector: HMAC-SHA256 of `1753401600.` and `BODY` under `whsec_test_secret`, computed outside this crate
    /// with `openssl dgst -sha256 -hmac whsec_test_secret` on 2026-11-27. The body and the secret are those of vpay's
    /// own verifier tests (`sdks/rust/src/webhooks.rs`).
    const VECTOR: &str = include_str!("../tests/vectors/vpay-signature.hex");

    fn secret(value: &str) -> WebhookSecret {
        WebhookSecret::new(value).expect("a secret")
    }

    fn at(secs: i64) -> Timestamp {
        Timestamp::from_unix_micros(secs * 1_000_000).expect("an instant")
    }

    fn headers(value: &str) -> WebhookHeaders {
        WebhookHeaders::new([("Vpay-Signature".to_owned(), value.to_owned())])
    }

    #[test]
    fn the_signature_is_the_vector() {
        let header = signature_header(BODY, at(T), &[secret("whsec_test_secret")]);
        assert_eq!(header, format!("t={T},v1={}", VECTOR.trim()));
        let secrets = [secret("whsec_test_secret")];
        assert_eq!(
            verify(&headers(&header), BODY, &secrets, 300, at(T + 10)),
            Ok(())
        );
    }

    #[test]
    fn a_rotation_header_verifies_with_either_secret() {
        let both = [secret("old"), secret("new")];
        let header = signature_header(BODY, at(T), &both);
        assert_eq!(header.matches("v1=").count(), 2);
        for one in ["old", "new"] {
            assert_eq!(
                verify(&headers(&header), BODY, &[secret(one)], 300, at(T)),
                Ok(())
            );
        }
        assert_eq!(
            verify(&headers(&header), BODY, &[secret("other")], 300, at(T)),
            Err(PaymentRefusal::WebhookSignature)
        );
    }

    #[test]
    fn malformed_headers_are_refused_as_vpay_refuses_them() {
        let secrets = [secret("whsec_test_secret")];
        for header in [
            "",
            "t=notanumber,v1=abc",
            "v1=abc",
            "t=123",
            "t=-5,v1=ab",
            "t=,v1=ab",
        ] {
            assert_eq!(
                verify(&headers(header), BODY, &secrets, 300, at(T)),
                Err(PaymentRefusal::WebhookSignature),
                "{header:?}"
            );
        }
        assert_eq!(
            verify(&WebhookHeaders::default(), BODY, &secrets, 300, at(T)),
            Err(PaymentRefusal::WebhookSignature)
        );
    }

    #[test]
    fn the_literal_t_text_is_signed() {
        let secrets = [secret("whsec_test_secret")];
        let mut mac = HmacSha256::new_from_slice(b"whsec_test_secret").expect("a key");
        mac.update(b"01753401600.");
        mac.update(BODY);
        let padded = format!(
            "t=01753401600,v1={}",
            hex::encode(mac.finalize().into_bytes())
        );
        assert_eq!(
            verify(&headers(&padded), BODY, &secrets, 300, at(T)),
            Ok(())
        );
        let rerendered = format!("t={T},v1={}", padded.split("v1=").nth(1).expect("a v1"));
        assert_eq!(
            verify(&headers(&rerendered), BODY, &secrets, 300, at(T)),
            Err(PaymentRefusal::WebhookSignature)
        );
    }

    #[test]
    fn the_tolerance_holds_both_ways() {
        let secrets = [secret("s")];
        let header = signature_header(BODY, at(T), &secrets);
        assert_eq!(
            verify(&headers(&header), BODY, &secrets, 300, at(T + 300)),
            Ok(())
        );
        assert_eq!(
            verify(&headers(&header), BODY, &secrets, 300, at(T - 300)),
            Ok(())
        );
        assert_eq!(
            verify(&headers(&header), BODY, &secrets, 300, at(T + 301)),
            Err(PaymentRefusal::WebhookTimestamp)
        );
        assert_eq!(
            verify(&headers(&header), BODY, &secrets, 300, at(T - 301)),
            Err(PaymentRefusal::WebhookTimestamp)
        );
        let huge = format!(
            "t=99999999999999999999,{}",
            header.split(',').nth(1).expect("v1")
        );
        assert_eq!(
            verify(&headers(&huge), BODY, &secrets, 300, at(T)),
            Err(PaymentRefusal::WebhookTimestamp)
        );
    }

    #[test]
    fn two_signature_headers_are_refused() {
        let secrets = [secret("s")];
        let header = signature_header(BODY, at(T), &secrets);
        let twice = WebhookHeaders::new([
            ("vpay-signature".to_owned(), header.clone()),
            ("Vpay-Signature".to_owned(), header),
        ]);
        assert_eq!(
            verify(&twice, BODY, &secrets, 300, at(T)),
            Err(PaymentRefusal::WebhookSignature)
        );
    }

    #[test]
    fn events_name_payments_refunds_or_nothing() {
        let body = |kind: &str, id: &str| {
            format!(
                r#"{{"id":"evt_1","object":"event","type":"{kind}","data":{{"object":{{"id":"{id}"}}}}}}"#
            )
        };
        let id = |v: &str| CreditId::new(v).expect("an id");
        assert_eq!(
            hint(body("payment_intent.succeeded", "pi_1").as_bytes()),
            Ok(WebhookHint::Payment(id("pi_1")))
        );
        assert_eq!(
            hint(body("payment_intent.canceled", "pi_2").as_bytes()),
            Ok(WebhookHint::Payment(id("pi_2")))
        );
        assert_eq!(
            hint(body("charge.refunded", "re_1").as_bytes()),
            Ok(WebhookHint::Refund(id("re_1")))
        );
        assert_eq!(
            hint(body("charge.refund.updated", "re_2").as_bytes()),
            Ok(WebhookHint::Refund(id("re_2")))
        );
        assert_eq!(
            hint(body("invoice.paid", "in_1").as_bytes()),
            Ok(WebhookHint::Ignored)
        );
        assert_eq!(hint(b"[]"), Err(PaymentRefusal::WebhookBody));
        assert_eq!(
            hint(body("payment_intent.succeeded", "a b").as_bytes()),
            Err(PaymentRefusal::WebhookBody)
        );
    }
}
