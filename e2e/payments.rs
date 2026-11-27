//! Scenario 8: a payment round trip through the provider, the vpay stand-in (`openbilling-vpay`, feature
//! `stand-in`), which the service reaches over HTTP as it reaches vpay. The owner starts a top-up; the payer pays at
//! the provider; a webhook signed as vpay signs it credits once, and a replay credits nothing more; a refund that
//! reads back as succeeded reverses part of it.

use openbilling_core::{Amount, CreditId, RefundStatus};
use openbilling_testkit::payments::{ProviderFixture, SignedWebhook};
use openbilling_vpay::stand_in;
use serde_json::json;

use crate::support::{Deployment, TempDir, Who, micros, now};

const OWNER: &str = "tenant-ex-8";

async fn deliver(d: &Deployment, hook: &SignedWebhook) -> crate::support::Answer {
    let signature = hook.headers.all("vpay-signature").next().map(str::to_owned);
    d.webhook(hook.body.clone(), signature.as_deref()).await
}

#[tokio::test(flavor = "multi_thread")]
async fn a_signed_payment_webhook_credits_once_and_a_succeeded_refund_reverses_it() {
    if crate::support::database_url().is_none() {
        return;
    }
    let fixture = stand_in::fixture().await;
    let files = TempDir::new();
    let token = files.path().join("vpay-token");
    std::fs::write(&token, stand_in::TOKEN).unwrap();
    let secrets = files.path().join("vpay-webhook-secrets");
    std::fs::write(&secrets, format!("{}\n", stand_in::SECRET)).unwrap();
    let extra = vec![
        (
            "OPENBILLING_VPAY_BASE_URL".to_owned(),
            fixture.stand_in.base_url.clone(),
        ),
        (
            "OPENBILLING_VPAY_TOKEN_FILE".to_owned(),
            token.display().to_string(),
        ),
        (
            "OPENBILLING_VPAY_WEBHOOK_SECRETS_FILE".to_owned(),
            secrets.display().to_string(),
        ),
        ("OPENBILLING_VPAY_CURRENCIES".to_owned(), "EUR:2".to_owned()),
        (
            "OPENBILLING_VPAY_PAYMENT_METHOD_TYPES".to_owned(),
            "test_rail".to_owned(),
        ),
    ];
    let Some(deployment) = Deployment::start(1, &extra).await else {
        return;
    };
    let d = &deployment;
    d.configure().await;
    d.open("acct-ex-8", OWNER).await;

    // The owner asks for a top-up of 12.34; the payment is pending until the payer pays.
    let started = d
        .write(
            "startTopUp",
            Who::Owner(OWNER),
            json!({"key": "topup-ex-8", "owner": OWNER, "currency": "EUR", "label": "main",
                   "amountMicros": "12340000", "successUrl": "https://service.example/paid",
                   "cancelUrl": "https://service.example/cancel"}),
        )
        .await
        .ok();
    assert_eq!(started.body["status"], "pending");
    let payment = CreditId::new(started.body["payment"].as_str().unwrap()).unwrap();

    // A webhook before the payer paid: the read-back says pending, and nothing is booked.
    let early = deliver(d, &fixture.payment_webhook(&payment, now()))
        .await
        .ok();
    assert_ne!(early.body["outcome"], "credited", "{}", early.text);
    assert_eq!(d.net(OWNER).await, 0);

    // Paid. A forged signature is refused; the provider's credits once; its replay credits nothing more.
    fixture.pay(&payment).await;
    let forged = deliver(d, &fixture.forged_webhook(&payment, now())).await;
    assert_eq!(forged.status, 401, "{}", forged.text);
    assert_eq!(d.net(OWNER).await, 0);
    let first = deliver(d, &fixture.payment_webhook(&payment, now()))
        .await
        .ok();
    assert_eq!(
        (
            first.body["outcome"].as_str(),
            first.body["replayed"].as_bool()
        ),
        (Some("credited"), Some(false))
    );
    let replay = deliver(d, &fixture.payment_webhook(&payment, now()))
        .await
        .ok();
    assert_eq!(
        (
            replay.body["outcome"].as_str(),
            replay.body["replayed"].as_bool()
        ),
        (Some("credited"), Some(true))
    );
    assert_eq!(d.net(OWNER).await, 12_340_000, "credited once");

    // A refund of 2: pending books nothing; succeeded reverses it, once.
    let part = Amount::from_micros(2_000_000).unwrap();
    let pending = fixture.refund(&payment, part, RefundStatus::Pending).await;
    let answer = deliver(d, &fixture.refund_webhook(&pending, now()))
        .await
        .ok();
    assert_eq!(answer.body["outcome"], "refund_not_settled");
    assert_eq!(d.net(OWNER).await, 12_340_000);
    let refund = fixture
        .refund(&payment, part, RefundStatus::Succeeded)
        .await;
    let reversed = deliver(d, &fixture.refund_webhook(&refund, now()))
        .await
        .ok();
    assert_eq!(
        (
            reversed.body["outcome"].as_str(),
            reversed.body["replayed"].as_bool()
        ),
        (Some("reversed"), Some(false))
    );
    let again = deliver(d, &fixture.refund_webhook(&refund, now()))
        .await
        .ok();
    assert_eq!(again.body["replayed"], true);
    assert_eq!(d.net(OWNER).await, 10_340_000);

    // The statement shows the payment and the refund; the credits list names the provider's payment.
    let (year, month, _) = now().utc_day().ymd();
    let lines = d.statement(OWNER, year, month).await;
    let line = |kind: &str| {
        lines
            .iter()
            .find(|l| l["line"] == "line" && l["kind"] == kind)
            .map(|l| {
                (
                    l["reference"].as_str().unwrap().to_owned(),
                    micros(&l["amountMicros"]),
                )
            })
    };
    assert_eq!(
        line("payment"),
        Some((payment.as_str().to_owned(), 12_340_000))
    );
    assert_eq!(
        line("refund"),
        Some((refund.as_str().to_owned(), -2_000_000))
    );
    let credits = d
        .read(
            "listCredits",
            Who::Owner(OWNER),
            json!({"owner": OWNER, "currency": "EUR"}),
        )
        .await
        .ok();
    assert_eq!(credits.body[0]["source"], "payment");
    assert_eq!(credits.body[0]["creditId"], payment.as_str());
}
