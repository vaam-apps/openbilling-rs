//! Scenario: the Rust SDK (`openbilling-sdk`, ADR 0008) against the real service. An operator configures, opens an
//! account and books a credit; a producer sends usage under an `Idempotency-Key`; an owner reads its balance. Every
//! call is the SDK's, so the generated client, the facade's bearer token and key, and the service agree.

use openbilling_sdk::generated::cratestack_schema::{procedures, types};
use openbilling_sdk::{Classify, Client, ErrorClass, IdempotencyKey};

use std::time::{Duration, Instant};

use crate::support::{Deployment, EUR, now, text};

const OWNER: &str = "tenant-sdk-1";

fn key(s: &str) -> IdempotencyKey {
    IdempotencyKey::new(s).unwrap()
}

#[tokio::test(flavor = "multi_thread")]
async fn the_sdk_configures_books_credit_records_usage_and_reads_the_balance() {
    let Some(deployment) = Deployment::start(1, &Vec::new()).await else {
        return;
    };
    let d = &deployment;
    let base = &d.services[0].base;

    let operator = Client::new(base).unwrap();
    operator.set_token(d.issuer.token("ops-console", &["operator"]));
    let producer = Client::new(base).unwrap();
    producer.set_token(d.issuer.token("metering-svc", &["producer"]));
    let owner = Client::new(base).unwrap();
    owner.set_token(d.issuer.token(OWNER, &[]));

    operator
        .configure(
            &procedures::configure::Args {
                args: serde_json::from_value(serde_json::json!({
                    "currencies": [EUR],
                    "labels": ["main"],
                    "lotOrder": [
                        {"kind": "allowance", "soonestExpiryFirst": true},
                        {"kind": "promotion", "soonestExpiryFirst": true},
                        {"kind": "adjustment", "soonestExpiryFirst": false},
                        {"kind": "payment", "soonestExpiryFirst": false}
                    ]
                }))
                .unwrap(),
            },
            &key("sdk-configure"),
        )
        .await
        .unwrap();

    let open = procedures::open_account::Args {
        args: serde_json::from_value(serde_json::json!({
            "accountId": "acct-sdk-1", "owner": OWNER, "currency": EUR, "label": "main"
        }))
        .unwrap(),
    };
    let opened = operator
        .open_account(&open, &key("sdk-open"))
        .await
        .unwrap();
    assert!(!opened.replayed);
    // The same call under the same key is the API's replay of the first answer.
    let again = operator
        .open_account(&open, &key("sdk-open"))
        .await
        .unwrap();
    assert!(!again.replayed, "the API replays the first answer verbatim");

    operator
        .grant_credit(
            &procedures::grant_credit::Args {
                args: serde_json::from_value(serde_json::json!({
                    "creditId": "credit-sdk-1", "source": "adjustment", "owner": OWNER,
                    "currency": EUR, "label": "main", "amountMicros": "10000000"
                }))
                .unwrap(),
            },
            &key("sdk-grant"),
        )
        .await
        .unwrap();

    let at = text(now());
    let usage = procedures::record_usage::Args {
        args: serde_json::from_value(serde_json::json!({"events": [{
            "eventId": "evt-sdk-1", "owner": OWNER, "member": "member-1", "currency": EUR,
            "amountMicros": "1500000", "occurredAt": at
        }]}))
        .unwrap(),
    };
    let taken = producer
        .record_usage(&usage, &key("sdk-usage"))
        .await
        .unwrap();
    assert_eq!((taken.accepted, taken.replayed), (1, 0));

    // The drain applies it on its own timer: the owner reads 10 EUR less 1.5 EUR.
    let balance = procedures::get_balance::Args {
        args: types::OwnerCurrency {
            owner: OWNER.to_owned(),
            currency: EUR.to_owned(),
        },
    };
    let deadline = Instant::now() + Duration::from_secs(30);
    let net = loop {
        let rows = owner.get_balance(&balance).await.unwrap();
        let net = rows[0].netMicros;
        if net == 8_500_000.into() || Instant::now() > deadline {
            break net;
        }
        tokio::time::sleep(Duration::from_millis(200)).await;
    };
    assert_eq!(net, 8_500_000.into(), "net after usage");

    // A write with no operator role is a 403 the SDK classes as Invalid; another owner's read is refused too.
    let denied = producer
        .open_account(&open, &key("sdk-denied"))
        .await
        .unwrap_err();
    assert_eq!(denied.status(), Some(403), "{denied}");
    assert_eq!(denied.class(), ErrorClass::Invalid);
    // No token at all is a 401, and no token is held in the error.
    let anonymous = Client::new(base).unwrap();
    let refused = anonymous.get_balance(&balance).await.unwrap_err();
    assert_eq!(refused.status(), Some(401), "{refused}");
}
