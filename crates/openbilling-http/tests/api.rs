//! The router in process, where no database is needed: authentication fails closed, every write needs an
//! `Idempotency-Key`, a policy refuses another owner's reads, and the webhook refuses what does not verify. Every
//! request here is refused (or answered) before any query runs.

#![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]

mod support;

use cratestack::axum::body::Body;
use cratestack::axum::http::{Method, Request, StatusCode};
use openbilling_core::{Amount, PaymentProvider};
use openbilling_http::{MUTATIONS, READS, WEBHOOK_PATH};
use openbilling_testkit::payments::{ProviderFixture, request};
use openbilling_vpay::stand_in;
use serde_json::{Value, json};
use support::{
    DownLedger, OPERATOR, OWNER_2, PRODUCER, app, call, procedure, raw, unreachable_pool,
};

const SCHEMA: &str = include_str!("../schema/openbilling.cstack");

fn plain() -> cratestack::axum::Router {
    app(DownLedger, None, unreachable_pool())
}

#[test]
fn the_procedure_lists_are_the_schemas() {
    let names = |prefix: &str| -> Vec<String> {
        let mut names: Vec<String> = SCHEMA
            .lines()
            .filter_map(|line| line.strip_prefix(prefix))
            .map(|rest| rest.split('(').next().unwrap().trim().to_owned())
            .collect();
        names.sort();
        names
    };
    let mut mutations: Vec<String> = MUTATIONS.iter().map(|m| (*m).to_owned()).collect();
    mutations.sort();
    let mut reads: Vec<String> = READS.iter().map(|r| (*r).to_owned()).collect();
    reads.sort();
    assert_eq!(names("mutation procedure "), mutations);
    assert_eq!(names("procedure "), reads);
}

#[tokio::test]
async fn health_answers_and_readiness_follows_the_database() {
    let app = plain();
    let health = call(&app, Method::GET, "/healthz", None, None, None).await;
    assert_eq!(
        (health.status, health.text.as_str()),
        (StatusCode::OK, "ok")
    );
    let ready = call(&app, Method::GET, "/readyz", None, None, None).await;
    assert_eq!(ready.status, StatusCode::SERVICE_UNAVAILABLE);
    assert_eq!(ready.body["code"], "UNAVAILABLE");
}

#[tokio::test]
async fn nothing_is_served_without_an_accepted_bearer_token() {
    let app = plain();
    for path in [
        "/v1/accounts",
        "/v1/lots",
        "/v1/usage_outcomes",
        "/v1/accounts/a1",
    ] {
        for token in [None, Some("not-a-token")] {
            let answer = call(&app, Method::GET, path, token, None, None).await;
            assert_eq!(answer.status, StatusCode::UNAUTHORIZED, "{path} {token:?}");
            assert_eq!(answer.body["code"], "UNAUTHORIZED");
            assert_eq!(answer.headers["www-authenticate"], "Bearer");
        }
    }
    for name in READS {
        let answer = procedure(&app, name, None, None, json!({})).await;
        assert_eq!(answer.status, StatusCode::UNAUTHORIZED, "{name}");
    }
    let basic = Request::builder()
        .uri("/v1/accounts")
        .header("authorization", format!("Basic {OPERATOR}"))
        .body(Body::empty())
        .unwrap();
    assert_eq!(raw(&app, basic).await.status, StatusCode::UNAUTHORIZED);
}

#[tokio::test]
async fn every_write_needs_an_idempotency_key() {
    let app = plain();
    for name in MUTATIONS {
        for token in [None, Some(OPERATOR)] {
            let answer = procedure(&app, name, token, None, json!({})).await;
            assert_eq!(answer.status, StatusCode::BAD_REQUEST, "{name}");
            assert_eq!(answer.body["code"], "BAD_REQUEST");
            assert!(answer.text.contains("idempotency_key"), "{}", answer.text);
        }
        let blank = procedure(&app, name, Some(OPERATOR), Some("  "), json!({})).await;
        assert_eq!(blank.status, StatusCode::BAD_REQUEST, "{name}");
    }
}

#[tokio::test]
async fn reads_are_the_owners_own_or_the_operators() {
    let app = plain();
    let reads: [(&str, Value); 5] = [
        ("getBalance", json!({"owner": "o1", "currency": "EUR"})),
        (
            "getAllowed",
            json!({"owner": "o1", "currency": "EUR", "member": "m1"}),
        ),
        ("listCredits", json!({"owner": "o1", "currency": "EUR"})),
        (
            "getStatement",
            json!({"owner": "o1", "currency": "EUR", "year": 2026, "month": 11}),
        ),
        (
            "getUsageSeries",
            json!({"owner": "o1", "currency": "EUR", "fromTime": "2026-11-01T00:00:00Z", "toTime": "2026-11-27T00:00:00Z"}),
        ),
    ];
    for (name, args) in &reads {
        let other = procedure(&app, name, Some(OWNER_2), None, args.clone()).await;
        assert_eq!(
            other.status,
            StatusCode::FORBIDDEN,
            "{name} as another owner"
        );
        let producer = procedure(&app, name, Some(PRODUCER), None, args.clone()).await;
        let expected = if *name == "getAllowed" {
            // A producer may ask whether a member may spend; the ledger is down here.
            StatusCode::SERVICE_UNAVAILABLE
        } else {
            StatusCode::FORBIDDEN
        };
        assert_eq!(
            producer.status, expected,
            "{name} as a producer: {}",
            producer.text
        );
    }
}

#[tokio::test]
async fn the_webhook_without_a_provider_is_unavailable() {
    let app = plain();
    let answer = call(
        &app,
        Method::POST,
        WEBHOOK_PATH,
        None,
        None,
        Some(json!({})),
    )
    .await;
    assert_eq!(answer.status, StatusCode::SERVICE_UNAVAILABLE);
}

fn webhook_request(hook: &openbilling_testkit::payments::SignedWebhook) -> Request<Body> {
    let mut request = Request::builder().method(Method::POST).uri(WEBHOOK_PATH);
    for name in ["vpay-signature", "stripe-signature"] {
        if let Some(value) = hook.headers.all(name).next() {
            request = request.header(name, value);
        }
    }
    request
        .header("content-type", "application/json")
        .body(Body::from(hook.body.clone()))
        .unwrap()
}

fn now() -> openbilling_core::Timestamp {
    let micros = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap()
        .as_micros();
    openbilling_core::Timestamp::from_unix_micros(i64::try_from(micros).unwrap()).unwrap()
}

#[tokio::test]
async fn the_webhook_verifies_reads_back_and_believes_nothing_else() {
    let fixture = stand_in::fixture().await;
    let app = app(
        DownLedger,
        Some(fixture.provider.clone()),
        unreachable_pool(),
    );
    let created = fixture
        .provider
        .create_payment(&request(
            &fixture,
            "k1",
            Amount::from_micros(10_000_000).unwrap(),
        ))
        .await
        .unwrap();

    let forged = fixture.forged_webhook(&created.payment, now());
    let answer = raw(&app, webhook_request(&forged)).await;
    assert_eq!(answer.status, StatusCode::UNAUTHORIZED);

    let mut stale = fixture.payment_webhook(&created.payment, now());
    stale.body.push(b' ');
    assert_eq!(
        raw(&app, webhook_request(&stale)).await.status,
        StatusCode::UNAUTHORIZED
    );

    let garbage = fixture.sign(b"not an event", now());
    assert_eq!(
        raw(&app, webhook_request(&garbage)).await.status,
        StatusCode::BAD_REQUEST
    );

    // The event says "succeeded"; vpay, read back, says the payment is still pending. Nothing is believed from the
    // event, and what the read-back means for the top-up is the ledger's to say (ADR 0003, amended 2026-11-28):
    // it is down here, so vpay is told to retry.
    let pending = fixture.payment_webhook(&created.payment, now());
    let answer = raw(&app, webhook_request(&pending)).await;
    assert_eq!(
        answer.status,
        StatusCode::SERVICE_UNAVAILABLE,
        "{}",
        answer.text
    );

    // Paid: the ledger must book it, and it is down, so vpay is told to retry.
    fixture.pay(&created.payment).await;
    let paid = fixture.payment_webhook(&created.payment, now());
    let answer = raw(&app, webhook_request(&paid)).await;
    assert_eq!(
        answer.status,
        StatusCode::SERVICE_UNAVAILABLE,
        "{}",
        answer.text
    );

    let ignored = fixture.other_webhook(now());
    let answer = raw(&app, webhook_request(&ignored)).await;
    assert_eq!(answer.body["outcome"], "ignored");
}
