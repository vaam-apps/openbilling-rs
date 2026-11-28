//! Scenario 8: a payment round trip through the provider, the vpay stand-in (`openbilling-vpay`, feature
//! `stand-in`), which the service reaches over HTTP as it reaches vpay. The owner starts a top-up; the payer pays at
//! the provider; a webhook signed as vpay signs it credits once, and a replay credits nothing more; a refund that
//! reads back as succeeded reverses part of it.

use openbilling_core::{Amount, CreditId, RefundStatus};
use openbilling_testkit::payments::{Alteration, ProviderFixture, SignedWebhook};
use openbilling_vpay::stand_in;
use serde_json::json;

use crate::support::{Deployment, TempDir, Who, micros, now};

const OWNER: &str = "tenant-ex-8";

async fn deliver(d: &Deployment, hook: &SignedWebhook) -> crate::support::Answer {
    let signature = hook.headers.all("vpay-signature").next().map(str::to_owned);
    d.webhook(hook.body.clone(), signature.as_deref()).await
}

/// The environment that points a service at the stand-in `fixture`, with its token and secrets written under `files`.
fn vpay_env(fixture: &stand_in::Fixture, files: &TempDir) -> Vec<(String, String)> {
    let token = files.path().join("vpay-token");
    std::fs::write(&token, stand_in::TOKEN).unwrap();
    let secrets = files.path().join("vpay-webhook-secrets");
    std::fs::write(&secrets, format!("{}\n", stand_in::SECRET)).unwrap();
    vec![
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
    ]
}

#[tokio::test(flavor = "multi_thread")]
async fn a_signed_payment_webhook_credits_once_and_a_succeeded_refund_reverses_it() {
    if crate::support::database_url().is_none() {
        return;
    }
    let fixture = stand_in::fixture().await;
    let files = TempDir::new();
    let extra = vpay_env(&fixture, &files);
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

/// A paid top-up whose webhook never came is found by the reconciler, which reads it back and credits it once; one a
/// payer changed after it was asked for (here its amount) is never credited, the webhook and the operator's
/// `reconcilePayment` answer it as the contract says, and a top-up still unpaid is left alone. The service runs with
/// short `reconcile` values (the file's, documented).
#[tokio::test(flavor = "multi_thread")]
async fn a_paid_top_up_with_no_webhook_is_credited_once_by_the_reconciler_and_an_altered_one_never()
{
    if crate::support::database_url().is_none() {
        return;
    }
    let fixture = stand_in::fixture().await;
    let files = TempDir::new();
    let config = files.path().join("config.yaml");
    std::fs::write(
        &config,
        "reconcile:\n  pending_after: 1s\n  read_again_after: 1s\n  every: 1s\n  batch: 10\n",
    )
    .unwrap();
    let mut extra = vpay_env(&fixture, &files);
    extra.push((
        "OPENBILLING_CONFIG".to_owned(),
        config.to_str().unwrap().to_owned(),
    ));
    let Some(deployment) = Deployment::start(1, &extra).await else {
        return;
    };
    let d = &deployment;
    d.configure().await;
    d.open("acct-ex-9", "tenant-ex-9").await;
    d.open("acct-ex-9b", "tenant-ex-9b").await;

    let start = |owner: &'static str, key: &'static str| async move {
        let started = d
            .write(
                "startTopUp",
                Who::Owner(owner),
                json!({"key": key, "owner": owner, "currency": "EUR", "label": "main",
                       "amountMicros": "12340000", "successUrl": "https://service.example/paid",
                       "cancelUrl": "https://service.example/cancel"}),
            )
            .await
            .ok();
        CreditId::new(started.body["payment"].as_str().unwrap()).unwrap()
    };
    let paid = start("tenant-ex-9", "topup-ex-9-paid").await;
    let altered = start("tenant-ex-9b", "topup-ex-9-altered").await;
    let waiting = start("tenant-ex-9", "topup-ex-9-waiting").await;
    // Starting the same top-up again is the same payment, recorded again as a replay.
    assert_eq!(start("tenant-ex-9", "topup-ex-9-paid").await, paid);

    // The payer of `altered` changes its amount before paying; `paid` is paid as asked; no webhook is delivered.
    fixture.alter(&altered, Alteration::Amount).await;
    fixture.pay(&altered).await;
    fixture.pay(&paid).await;
    let mut credited = false;
    for _ in 0..40 {
        if d.net("tenant-ex-9").await == 12_340_000 {
            credited = true;
            break;
        }
        tokio::time::sleep(std::time::Duration::from_millis(250)).await;
    }
    assert!(credited, "the reconciler credited the paid top-up");
    // Read again and again, it stays credited once; the unpaid one and the altered one are not credited.
    tokio::time::sleep(std::time::Duration::from_secs(3)).await;
    assert_eq!(d.net("tenant-ex-9").await, 12_340_000, "once");
    assert_eq!(d.net("tenant-ex-9b").await, 0, "never the altered one");

    // The webhook for the altered payment is acknowledged as a mismatch (the provider need not retry it); the
    // operator's reconcile of it is a conflict that names the field and no value.
    let hook = deliver(d, &fixture.payment_webhook(&altered, now()))
        .await
        .ok();
    assert_eq!(hook.body["outcome"], "mismatch", "{}", hook.text);
    let operator = d
        .write(
            "reconcilePayment",
            Who::Operator,
            json!({"id": altered.as_str()}),
        )
        .await;
    assert_eq!(operator.status, 409, "{}", operator.text);
    assert!(operator.text.contains("amount"), "{}", operator.text);
    assert_eq!(d.net("tenant-ex-9b").await, 0);

    // Paid later, the waiting one is credited by a later pass.
    fixture.pay(&waiting).await;
    let mut both = false;
    for _ in 0..40 {
        if d.net("tenant-ex-9").await == 2 * 12_340_000 {
            both = true;
            break;
        }
        tokio::time::sleep(std::time::Duration::from_millis(250)).await;
    }
    assert!(both, "the top-up paid later was credited by a later pass");
}
