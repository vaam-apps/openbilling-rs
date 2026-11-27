//! What the adapter sends to vpay, and how it reads vpay's refusals and faults.

#![allow(clippy::unwrap_used, clippy::expect_used)]

use openbilling_vpay::stand_in as support;

use std::time::Duration;

use openbilling_core::{Classify, CreditId, ErrorClass, PaymentProvider, PaymentRefusal};
use openbilling_testkit::payments::request;
use openbilling_vpay::{Credential, VpayProvider};
use support::{TOKEN, fixture, start};

#[tokio::test]
async fn a_top_up_is_an_intent_with_metadata_then_a_hosted_session() {
    let f = fixture().await;
    let created = f
        .provider
        .create_payment(&request(
            &f,
            "top-1",
            openbilling_core::Amount::from_micros(12_340_000).unwrap(),
        ))
        .await
        .unwrap();
    assert_eq!(created.checkout.id, "cs_00000002");
    assert_eq!(
        created.checkout.url.as_deref(),
        Some("http://checkout.invalid/c/cs_00000002#cs_secret")
    );
    let seen = f.stand_in.state.lock().unwrap().seen.clone();
    assert_eq!(seen.len(), 2);
    assert_eq!(seen[0].path, "/v1/payment_intents");
    assert_eq!(
        seen[0].body,
        "amount=1234&currency=eur&payment_method_types[0]=test_rail\
         &metadata[openbilling_owner]=o1&metadata[openbilling_label]=main"
    );
    assert_eq!(
        seen[0].idempotency_key.as_deref(),
        Some("openbilling:top-1:payment_intent")
    );
    assert_eq!(seen[0].authorization, Some(format!("Bearer {TOKEN}")));
    assert_eq!(seen[1].path, "/v1/checkout/sessions");
    assert_eq!(
        seen[1].body,
        "payment_intent=pi_00000001&ui_mode=hosted\
         &success_url=https%3A%2F%2Fshop.example%2Fpaid&cancel_url=https%3A%2F%2Fshop.example%2Fcancel"
    );
    assert_eq!(
        seen[1].idempotency_key.as_deref(),
        Some("openbilling:top-1:checkout_session")
    );
}

#[tokio::test]
async fn the_same_key_for_another_top_up_is_a_conflict() {
    let f = fixture().await;
    let micros = |v| openbilling_core::Amount::from_micros(v).unwrap();
    f.provider
        .create_payment(&request(&f, "k", micros(10_000)))
        .await
        .unwrap();
    let error = f
        .provider
        .create_payment(&request(&f, "k", micros(20_000)))
        .await
        .unwrap_err();
    assert_eq!(error.refusal(), Some(PaymentRefusal::Conflict));
}

#[tokio::test]
async fn a_currency_not_configured_is_refused_before_vpay_is_asked() {
    let f = fixture().await;
    let mut asked = request(
        &f,
        "k",
        openbilling_core::Amount::from_micros(10_000).unwrap(),
    );
    asked.currency = openbilling_core::CurrencyCode::new("USD").unwrap();
    let error = f.provider.create_payment(&asked).await.unwrap_err();
    assert_eq!(error.refusal(), Some(PaymentRefusal::UnsupportedCurrency));
    assert!(f.stand_in.state.lock().unwrap().seen.is_empty());
}

#[tokio::test]
async fn the_token_file_is_read_on_every_call() {
    let stand_in = start().await;
    let dir = std::env::temp_dir().join(format!("ob-vpay-token-{}", std::process::id()));
    std::fs::create_dir_all(&dir).unwrap();
    let path = dir.join("token");
    std::fs::write(&path, "wrong\n").unwrap();
    let mut config = support::config(&stand_in.base_url);
    config.credential = Credential::TokenFile(path.clone());
    let provider = VpayProvider::new(config).unwrap();
    let id = CreditId::new("pi_missing").unwrap();
    let error = provider.read_payment(&id).await.unwrap_err();
    assert_eq!(
        error.class(),
        ErrorClass::Internal,
        "vpay refused the credential"
    );
    assert!(!format!("{error:?}").contains("wrong"), "{error:?}");

    std::fs::write(&path, format!("  {TOKEN}\n")).unwrap();
    let error = provider.read_payment(&id).await.unwrap_err();
    assert_eq!(
        error.refusal(),
        Some(PaymentRefusal::NotFound),
        "the rotated token is used"
    );

    std::fs::remove_file(&path).unwrap();
    let error = provider.read_payment(&id).await.unwrap_err();
    assert_eq!(error.class(), ErrorClass::Unavailable);
    let _ = std::fs::remove_dir(&dir);
}

#[tokio::test]
async fn vpay_faults_and_an_unreachable_vpay_are_classed() {
    let f = fixture().await;
    let id = CreditId::new("pi_any").unwrap();
    for (status, class) in [
        (503, ErrorClass::Unavailable),
        (429, ErrorClass::Unavailable),
        (500, ErrorClass::Internal),
        (403, ErrorClass::Internal),
    ] {
        f.stand_in.state.lock().unwrap().fail_with = Some(status);
        let error = f.provider.read_payment(&id).await.unwrap_err();
        assert_eq!(error.class(), class, "{status}");
        assert!(!error.to_string().contains(TOKEN));
    }

    // A port nobody listens on.
    let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
    let address = listener.local_addr().unwrap();
    drop(listener);
    let mut config = support::config(&format!("http://{address}"));
    config.timeout = Duration::from_secs(2);
    let provider = VpayProvider::new(config).unwrap();
    let error = provider.read_payment(&id).await.unwrap_err();
    assert_eq!(error.class(), ErrorClass::Unavailable);
}

#[tokio::test]
async fn an_id_is_escaped_as_a_path_segment() {
    let f = fixture().await;
    let id = CreditId::new("pi_1/../../v1/refunds/x?y#z").unwrap();
    let error = f.provider.read_payment(&id).await.unwrap_err();
    assert_eq!(error.refusal(), Some(PaymentRefusal::NotFound));
    let seen = f.stand_in.state.lock().unwrap().seen.clone();
    assert_eq!(seen.len(), 1);
    assert!(
        seen[0].path.starts_with("/v1/payment_intents/pi_1"),
        "{}",
        seen[0].path
    );
}
