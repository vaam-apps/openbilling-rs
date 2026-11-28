//! The payment conformance cases that need no payer, against a real vpay (a sandbox), through a minted token.
//!
//! **Skipped unless** `VPAY_SANDBOX_URL`, `VPAY_SANDBOX_CLIENT_ID`, `VPAY_SANDBOX_CLIENT_KEY` (the PEM text of the
//! client's RSA private key) and `VPAY_SANDBOX_PAYMENT_METHOD_TYPE` are all set; fails instead when
//! `VPAY_SANDBOX_REQUIRE=1`. Optional: `VPAY_SANDBOX_CURRENCY` (`CODE:EXPONENT`, default `EUR:2`),
//! `VPAY_SANDBOX_WEBHOOK_SECRET` (without it the webhook case is skipped), `VPAY_SANDBOX_KEY_ID`,
//! `VPAY_SANDBOX_TOKEN_URL`, `VPAY_SANDBOX_ASSERTION_AUDIENCE`.
//!
//! Three of the suite's nine cases run: a payment created once per key and read back pending, objects vpay does
//! not know and an amount it cannot hold, and forged, stale and tampered webhooks (when the secret is given). The
//! other four need the payer or the rail to act (`pay`, `cancel`, `refund`, a payment openbilling did not ask for),
//! which no client of vpay can do, so they stay on the stand-in. This file is run by the gated workflow
//! `.github/workflows/vpay-sandbox.yml`; until it has run green once, nothing here has been proven against vpay.

#![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]

use std::time::Duration;

use openbilling_core::{Amount, CreditId, CurrencyCode, RefundStatus, Timestamp};
use openbilling_testkit::payments::{self, ProviderFixture, SignedWebhook};
use openbilling_vpay::stand_in::{event, signed};
use openbilling_vpay::{
    ClientCredentials, Credential, CurrencyUnit, KeySource, VpayConfig, VpayProvider,
    WebhookSecret, webhook,
};
use serde_json::json;

struct Sandbox {
    provider: VpayProvider,
    currency: CurrencyUnit,
    secret: Option<String>,
}

fn env(name: &str) -> Option<String> {
    std::env::var(name).ok().filter(|v| !v.is_empty())
}

async fn sandbox() -> Option<Sandbox> {
    let required = [
        "VPAY_SANDBOX_URL",
        "VPAY_SANDBOX_CLIENT_ID",
        "VPAY_SANDBOX_CLIENT_KEY",
        "VPAY_SANDBOX_PAYMENT_METHOD_TYPE",
    ];
    let [Some(url), Some(client_id), Some(key), Some(method)] = required.map(env) else {
        assert!(
            env("VPAY_SANDBOX_REQUIRE").as_deref() != Some("1"),
            "VPAY_SANDBOX_REQUIRE=1 and a VPAY_SANDBOX_* setting is missing"
        );
        eprintln!(
            "skipped: VPAY_SANDBOX_URL, _CLIENT_ID, _CLIENT_KEY and _PAYMENT_METHOD_TYPE are not all set"
        );
        return None;
    };
    let currency = env("VPAY_SANDBOX_CURRENCY").unwrap_or_else(|| "EUR:2".to_owned());
    let (code, exponent) = currency.split_once(':').expect("CODE:EXPONENT");
    let currency = CurrencyUnit {
        currency: CurrencyCode::new(code).expect("a currency"),
        exponent: exponent.parse().expect("an exponent"),
    };
    let mut credentials = ClientCredentials::new(client_id, KeySource::Pem(key));
    credentials.key_id = env("VPAY_SANDBOX_KEY_ID");
    credentials.token_url = env("VPAY_SANDBOX_TOKEN_URL");
    credentials.assertion_audience = env("VPAY_SANDBOX_ASSERTION_AUDIENCE");
    let secret = env("VPAY_SANDBOX_WEBHOOK_SECRET");
    let config = VpayConfig {
        base_url: url,
        credential: Credential::ClientCredentials(credentials),
        webhook_secrets: vec![
            WebhookSecret::new(
                secret
                    .clone()
                    .unwrap_or_else(|| "whsec_not_given".to_owned()),
            )
            .unwrap(),
        ],
        webhook_tolerance_secs: webhook::DEFAULT_TOLERANCE_SECS,
        currencies: vec![currency.clone()],
        payment_method_types: vec![method],
        timeout: Duration::from_secs(30),
    };
    Some(Sandbox {
        provider: VpayProvider::new(config).expect("a usable configuration"),
        currency,
        secret,
    })
}

const NEEDS_A_PAYER: &str = "needs the payer or the rail to act: not available against a sandbox";

impl ProviderFixture for Sandbox {
    type Provider = VpayProvider;

    fn provider(&self) -> &VpayProvider {
        &self.provider
    }

    fn currency(&self) -> CurrencyCode {
        self.currency.currency.clone()
    }

    fn amount(&self) -> Amount {
        // 12.34 major units, as micro-units; a currency of exponent below 2 holds 12 instead.
        Amount::from_micros(if self.currency.exponent >= 2 {
            12_340_000
        } else {
            12_000_000
        })
        .unwrap()
    }

    fn inexact_amount(&self) -> Option<Amount> {
        (self.currency.exponent < 6).then(|| Amount::from_micros(12_000_001).unwrap())
    }

    fn tolerance_micros(&self) -> i64 {
        i64::from(webhook::DEFAULT_TOLERANCE_SECS) * 1_000_000
    }

    async fn pay(&self, _: &CreditId) {
        panic!("{NEEDS_A_PAYER}");
    }

    async fn cancel(&self, _: &CreditId) {
        panic!("{NEEDS_A_PAYER}");
    }

    async fn alter(&self, _: &CreditId, _: payments::Alteration) {
        panic!("{NEEDS_A_PAYER}");
    }

    async fn refund(&self, _: &CreditId, _: Amount, _: RefundStatus) -> CreditId {
        panic!("{NEEDS_A_PAYER}");
    }

    async fn foreign_payment(&self) -> CreditId {
        panic!("{NEEDS_A_PAYER}");
    }

    fn payment_webhook(&self, payment: &CreditId, at: Timestamp) -> SignedWebhook {
        let body = event(
            "payment_intent.succeeded",
            json!({"id": payment.as_str(), "object": "payment_intent", "status": "succeeded"}),
        );
        signed(body, at, self.secret.as_deref().expect("a webhook secret"))
    }

    fn refund_webhook(&self, refund: &CreditId, at: Timestamp) -> SignedWebhook {
        let body = event(
            "charge.refunded",
            json!({"id": refund.as_str(), "object": "refund", "status": "pending"}),
        );
        signed(body, at, self.secret.as_deref().expect("a webhook secret"))
    }

    fn other_webhook(&self, at: Timestamp) -> SignedWebhook {
        let body = event(
            "customer.created",
            json!({"id": "cus_1", "object": "customer"}),
        );
        signed(body, at, self.secret.as_deref().expect("a webhook secret"))
    }

    fn forged_webhook(&self, payment: &CreditId, at: Timestamp) -> SignedWebhook {
        let body = event(
            "payment_intent.succeeded",
            json!({"id": payment.as_str(), "object": "payment_intent"}),
        );
        signed(body, at, "whsec_someone_else")
    }

    fn sign(&self, body: &[u8], at: Timestamp) -> SignedWebhook {
        signed(
            body.to_vec(),
            at,
            self.secret.as_deref().expect("a webhook secret"),
        )
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_payment_is_created_once_per_key_and_read_back_pending() {
    if let Some(fixture) = sandbox().await {
        payments::a_payment_is_created_once_per_key_and_read_back_pending(fixture).await;
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn unknown_objects_and_inexact_amounts_are_refused() {
    if let Some(fixture) = sandbox().await {
        payments::unknown_objects_and_inexact_amounts_are_refused(fixture).await;
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_forged_stale_or_tampered_webhook_is_refused() {
    match sandbox().await {
        Some(fixture) if fixture.secret.is_some() => {
            payments::a_forged_stale_or_tampered_webhook_is_refused(fixture).await;
        }
        Some(_) => eprintln!("skipped: VPAY_SANDBOX_WEBHOOK_SECRET is not set"),
        None => {}
    }
}
