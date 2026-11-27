//! Before the scenarios: the service verifies its callers. Without a token, or with one the issuer did not sign for
//! this audience, nothing runs; a service started with no issuer refuses even a good token.

use openbilling_jwt::test_issuer::{Signer, TestIssuer};
use serde_json::json;

use crate::support::{Deployment, Service, Who};

#[tokio::test(flavor = "multi_thread")]
async fn the_service_refuses_every_call_without_a_token_its_issuer_signed_for_it() {
    let Some(deployment) = Deployment::start(1, &Vec::new()).await else {
        return;
    };
    let d = &deployment;
    let args = json!({"owner": "tenant-ex-0", "currency": "EUR"});
    let none = d.read("getBalance", Who::Nobody, args.clone()).await;
    assert_eq!(none.status, 401, "{}", none.text);
    assert_eq!(
        none.headers
            .get("www-authenticate")
            .and_then(|v| v.to_str().ok()),
        Some("Bearer")
    );
    let mut claims = d.issuer.claims("tenant-ex-0", &["operator"]);
    claims["aud"] = json!("other-service");
    let elsewhere = d
        .issuer
        .sign(Signer::Es256, &TestIssuer::header(Signer::Es256), &claims);
    let refused = d
        .read("getBalance", Who::Token(&elsewhere), args.clone())
        .await;
    assert_eq!(refused.status, 401, "{}", refused.text);
    let stranger = TestIssuer::start().await;
    let foreign = stranger.token("tenant-ex-0", &["operator"]);
    let refused = d
        .read("getBalance", Who::Token(&foreign), args.clone())
        .await;
    assert_eq!(
        refused.status, 401,
        "another issuer's token: {}",
        refused.text
    );
    // The issuer's own token for the owner is accepted, and reads an owner with no account as no rows.
    let mine = d
        .read("getBalance", Who::Owner("tenant-ex-0"), args.clone())
        .await
        .ok();
    assert_eq!(mine.body, json!([]));
    // A role the token does not carry is a 403, not a 401.
    let write = d
        .write(
            "setDebtLimit",
            Who::Owner("tenant-ex-0"),
            json!({"owner": "tenant-ex-0", "currency": "EUR"}),
        )
        .await;
    assert_eq!(write.status, 403, "{}", write.text);

    // The same binary with no issuer configured: every call is a 401, whatever the token.
    let url = d.database_url.clone();
    // Its own services stop first; the database stays.
    drop(deployment);
    let issuer = TestIssuer::start().await;
    let unconfigured = Service::start_without_issuer(&url).await;
    let token = issuer.token("tenant-ex-0", &["operator"]);
    let answer = reqwest_get_balance(&unconfigured.base, &token).await;
    assert_eq!(answer, 401);
    assert!(
        unconfigured.log().contains("no token issuer configured"),
        "{}",
        unconfigured.log()
    );
}

async fn reqwest_get_balance(base: &str, token: &str) -> u16 {
    reqwest::Client::new()
        .post(format!("{base}/v1/$procs/getBalance"))
        .bearer_auth(token)
        .header("content-type", "application/json")
        .body(r#"{"args":{"owner":"tenant-ex-0","currency":"EUR"}}"#)
        .send()
        .await
        .unwrap()
        .status()
        .as_u16()
}
