//! The verifier against a real issuer on a loopback port (`test_issuer`): each check refuses what it should, with
//! its own reason, and each refusal has a control, the same token with only that one thing right, that is accepted.

#![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]

use std::time::Duration;

use base64::Engine;
use base64::engine::general_purpose::URL_SAFE_NO_PAD;
use jsonwebtoken::{Algorithm, DecodingKey};
use openbilling_http::auth::{TokenRefusal, TokenVerifier};
use openbilling_jwt::test_issuer::{
    AUDIENCE, KID_ES256, KID_RS256, Signer, TestIssuer, now_secs, rfc7515_a2, rfc8037_a4,
};
use openbilling_jwt::{JwtConfig, JwtRefusal, JwtVerifier, RoleMap, provider};
use serde_json::{Value, json};

fn config(issuer: &TestIssuer) -> JwtConfig {
    JwtConfig::new(issuer.issuer.clone(), AUDIENCE)
}

async fn start() -> (TestIssuer, JwtVerifier) {
    let issuer = TestIssuer::start().await;
    let verifier = JwtVerifier::new(config(&issuer)).unwrap();
    (issuer, verifier)
}

fn with(mut claims: Value, key: &str, value: Value) -> Value {
    claims[key] = value;
    claims
}

fn without(mut claims: Value, key: &str) -> Value {
    claims.as_object_mut().unwrap().remove(key);
    claims
}

#[tokio::test]
async fn the_three_algorithms_are_accepted_and_the_roles_mapped() {
    let (issuer, verifier) = start().await;
    for signer in [Signer::Es256, Signer::EdDsa, Signer::Rs256] {
        let claims = issuer.claims("owner-1", &["operator", "someone-else", "operator"]);
        let token = issuer.sign(signer, &TestIssuer::header(signer), &claims);
        let principal = verifier.check(&token).await.unwrap();
        assert_eq!(principal.sub, "owner-1", "{signer:?}");
        assert_eq!(principal.roles, ["operator"], "{signer:?}");
    }
    assert_eq!(
        issuer.jwks_fetches(),
        1,
        "one fetch serves every key while fresh"
    );
    let owner = verifier.check(&issuer.token("owner-2", &[])).await.unwrap();
    assert!(owner.roles.is_empty(), "an owner needs no role");
}

#[tokio::test]
async fn a_refused_algorithm_is_refused_whatever_it_is_signed_with() {
    let (issuer, verifier) = start().await;
    let claims = issuer.claims("owner-1", &["operator"]);
    let encode = |v: &Value| URL_SAFE_NO_PAD.encode(serde_json::to_vec(v).unwrap());

    // `none`: no signature at all.
    let none = format!(
        "{}.{}.",
        encode(&json!({"alg": "none", "kid": KID_ES256})),
        encode(&claims)
    );
    assert_eq!(verifier.check(&none).await, Err(JwtRefusal::Algorithm));

    // HS256 keyed with the RSA key's public modulus: the classic confusion of a public key as an HMAC secret.
    let input = format!(
        "{}.{}",
        encode(&json!({"alg": "HS256", "kid": KID_RS256})),
        encode(&claims)
    );
    let key = ring::hmac::Key::new(
        ring::hmac::HMAC_SHA256,
        &URL_SAFE_NO_PAD.decode(rfc7515_a2::N).unwrap(),
    );
    let tag = ring::hmac::sign(&key, input.as_bytes());
    let hs256 = format!("{input}.{}", URL_SAFE_NO_PAD.encode(tag.as_ref()));
    assert_eq!(verifier.check(&hs256).await, Err(JwtRefusal::Algorithm));

    // An accepted algorithm, but not the one of the key its `kid` names.
    let mismatch = issuer.sign(
        Signer::Es256,
        &json!({"alg": "ES256", "kid": KID_RS256}),
        &claims,
    );
    assert_eq!(verifier.check(&mismatch).await, Err(JwtRefusal::Algorithm));
    for other in ["RS512", "PS256", "ES384"] {
        let token = issuer.sign(
            Signer::Es256,
            &json!({"alg": other, "kid": KID_ES256}),
            &claims,
        );
        assert_eq!(
            verifier.check(&token).await,
            Err(JwtRefusal::Algorithm),
            "{other}"
        );
    }
    // The control: the same claims, signed as the key says.
    assert!(
        verifier
            .check(&issuer.sign(Signer::Es256, &TestIssuer::header(Signer::Es256), &claims))
            .await
            .is_ok()
    );
}

#[tokio::test]
async fn a_wrong_audience_or_issuer_is_refused() {
    let (issuer, verifier) = start().await;
    let header = TestIssuer::header(Signer::Es256);
    let good = issuer.claims("owner-1", &[]);
    let sign = |claims: &Value| issuer.sign(Signer::Es256, &header, claims);
    assert!(verifier.check(&sign(&good)).await.is_ok());
    for aud in [
        json!("someone-else"),
        json!([]),
        json!(["openbilling-test-2"]),
        json!("OPENBILLING-TEST"),
    ] {
        let token = sign(&with(good.clone(), "aud", aud.clone()));
        assert_eq!(
            verifier.check(&token).await,
            Err(JwtRefusal::Audience),
            "{aud}"
        );
    }
    let among = sign(&with(good.clone(), "aud", json!(["other", AUDIENCE])));
    assert!(
        verifier.check(&among).await.is_ok(),
        "an audience among others is ours"
    );
    let no_aud = sign(&without(good.clone(), "aud"));
    assert_eq!(verifier.check(&no_aud).await, Err(JwtRefusal::MissingClaim));
    let iss = sign(&with(
        good.clone(),
        "iss",
        json!(format!("{}/", issuer.issuer)),
    ));
    assert_eq!(verifier.check(&iss).await, Err(JwtRefusal::Issuer));
    let no_iss = sign(&without(good, "iss"));
    assert_eq!(verifier.check(&no_iss).await, Err(JwtRefusal::MissingClaim));
}

#[tokio::test]
async fn an_expired_or_premature_token_is_refused_past_the_leeway() {
    let (issuer, verifier) = start().await;
    let header = TestIssuer::header(Signer::EdDsa);
    let good = issuer.claims("owner-1", &[]);
    let sign = |claims: &Value| issuer.sign(Signer::EdDsa, &header, claims);
    let now = now_secs();
    let expired = sign(&with(good.clone(), "exp", json!(now - 120)));
    assert_eq!(verifier.check(&expired).await, Err(JwtRefusal::Expired));
    let within_leeway = sign(&with(good.clone(), "exp", json!(now - 10)));
    assert!(verifier.check(&within_leeway).await.is_ok());
    let no_exp = sign(&without(good.clone(), "exp"));
    assert_eq!(verifier.check(&no_exp).await, Err(JwtRefusal::MissingClaim));
    let premature = sign(&with(good.clone(), "nbf", json!(now + 120)));
    assert_eq!(
        verifier.check(&premature).await,
        Err(JwtRefusal::NotYetValid)
    );
    let no_nbf = sign(&without(good, "nbf"));
    assert!(verifier.check(&no_nbf).await.is_ok(), "nbf is optional");
}

#[tokio::test]
async fn an_unknown_kid_is_refused_and_fetched_again_at_most_once_per_interval() {
    let issuer = TestIssuer::start().await;
    let mut throttled = config(&issuer);
    throttled.min_refetch = Duration::from_secs(3600);
    let verifier = JwtVerifier::new(throttled).unwrap();
    let claims = issuer.claims("owner-1", &[]);
    let unknown = issuer.sign(
        Signer::Unpublished,
        &TestIssuer::header(Signer::Unpublished),
        &claims,
    );
    assert_eq!(verifier.check(&unknown).await, Err(JwtRefusal::UnknownKey));
    assert_eq!(issuer.jwks_fetches(), 1);
    issuer.publish_unpublished_key();
    for _ in 0..5 {
        assert_eq!(verifier.check(&unknown).await, Err(JwtRefusal::UnknownKey));
    }
    assert_eq!(
        issuer.jwks_fetches(),
        1,
        "an unknown kid does not make it hammer the issuer"
    );
    let no_kid = issuer.sign(Signer::Es256, &json!({"alg": "ES256"}), &claims);
    assert_eq!(verifier.check(&no_kid).await, Err(JwtRefusal::UnknownKey));

    // Without the throttle, the new key is fetched on the first token that names it.
    let mut eager = config(&issuer);
    eager.min_refetch = Duration::ZERO;
    let verifier = JwtVerifier::new(eager).unwrap();
    assert!(verifier.check(&issuer.token("owner-1", &[])).await.is_ok());
    assert!(verifier.check(&unknown).await.is_ok());
}

#[tokio::test]
async fn a_tampered_token_a_bad_subject_and_crit_are_refused() {
    let (issuer, verifier) = start().await;
    let header = TestIssuer::header(Signer::Es256);
    let good = issuer.claims("owner-1", &["producer"]);
    let token = issuer.sign(Signer::Es256, &header, &good);
    let (head, rest) = token.split_once('.').unwrap();
    let (_, signature) = rest.split_once('.').unwrap();
    let forged_claims = URL_SAFE_NO_PAD
        .encode(serde_json::to_vec(&with(good.clone(), "roles", json!(["operator"]))).unwrap());
    let forged = format!("{head}.{forged_claims}.{signature}");
    assert_eq!(verifier.check(&forged).await, Err(JwtRefusal::Signature));

    let sign = |claims: &Value| issuer.sign(Signer::Es256, &header, claims);
    let no_sub = sign(&without(good.clone(), "sub"));
    assert_eq!(verifier.check(&no_sub).await, Err(JwtRefusal::MissingClaim));
    for sub in [json!("has space"), json!(""), json!("x".repeat(256))] {
        let token = sign(&with(good.clone(), "sub", sub.clone()));
        assert_eq!(
            verifier.check(&token).await,
            Err(JwtRefusal::Subject),
            "{sub}"
        );
    }
    let crit = issuer.sign(
        Signer::Es256,
        &json!({"alg": "ES256", "kid": KID_ES256, "crit": ["exp"]}),
        &good,
    );
    assert_eq!(verifier.check(&crit).await, Err(JwtRefusal::Malformed));
    for junk in ["", "a.b", "a.b.c", "....", &"a".repeat(20_000)] {
        assert_eq!(
            verifier.check(junk).await,
            Err(JwtRefusal::Malformed),
            "{junk:.10}"
        );
    }
}

#[tokio::test]
async fn roles_come_from_the_configured_claim_through_the_map() {
    let issuer = TestIssuer::start().await;
    let mut nested = config(&issuer);
    nested.roles_claim = "realm_access.roles".to_owned();
    nested.roles = RoleMap::parse("billing-admin=operator,meter=producer").unwrap();
    let verifier = JwtVerifier::new(nested).unwrap();
    let header = TestIssuer::header(Signer::Es256);
    let base = without(issuer.claims("svc", &[]), "roles");
    let claims = with(
        base.clone(),
        "realm_access",
        json!({"roles": ["meter", "operator", "billing-admin"]}),
    );
    let principal = verifier
        .check(&issuer.sign(Signer::Es256, &header, &claims))
        .await
        .unwrap();
    assert_eq!(
        principal.roles,
        ["operator", "producer"],
        "`operator` is not a value the map names"
    );
    let spaced = with(base.clone(), "realm_access", json!({"roles": "meter x"}));
    let principal = verifier
        .check(&issuer.sign(Signer::Es256, &header, &spaced))
        .await
        .unwrap();
    assert_eq!(principal.roles, ["producer"]);
    let flat = with(base, "roles", json!(["billing-admin"]));
    let principal = verifier
        .check(&issuer.sign(Signer::Es256, &header, &flat))
        .await
        .unwrap();
    assert!(
        principal.roles.is_empty(),
        "only the configured claim is read"
    );
}

#[tokio::test]
async fn an_issuer_that_is_down_is_unavailable_never_accepted() {
    let issuer = TestIssuer::start().await;
    issuer.set_down(true);
    let mut eager = config(&issuer);
    eager.min_refetch = Duration::ZERO;
    eager.refresh = Duration::ZERO;
    let verifier = JwtVerifier::new(eager.clone()).unwrap();
    let token = issuer.token("owner-1", &[]);
    assert_eq!(verifier.check(&token).await, Err(JwtRefusal::Unavailable));
    assert_eq!(
        verifier.verify(&token).await,
        Err(TokenRefusal::Unavailable)
    );

    // Keys once read serve while the issuer is down, for `stale_for`.
    issuer.set_down(false);
    assert!(verifier.check(&token).await.is_ok());
    issuer.set_down(true);
    assert!(verifier.check(&token).await.is_ok());
    let mut strict = eager;
    strict.stale_for = Duration::ZERO;
    let verifier = JwtVerifier::new(strict).unwrap();
    issuer.set_down(false);
    assert!(verifier.check(&token).await.is_ok());
    issuer.set_down(true);
    assert_eq!(verifier.check(&token).await, Err(JwtRefusal::Unavailable));
    // Every other refusal is a 401.
    assert_eq!(verifier.verify("junk").await, Err(TokenRefusal::Invalid));
}

#[test]
fn the_provider_verifies_the_rfc_examples_and_refuses_hmac() {
    assert!(provider::install());
    let verify = |jws: &str, key: &DecodingKey, alg| {
        let (message, signature) = jws.rsplit_once('.').unwrap();
        jsonwebtoken::crypto::verify(signature, message.as_bytes(), key, alg)
    };
    let rsa = DecodingKey::from_rsa_components(rfc7515_a2::N, rfc7515_a2::E).unwrap();
    assert!(verify(rfc7515_a2::JWS, &rsa, Algorithm::RS256).unwrap());
    let ed = DecodingKey::from_ed_components(rfc8037_a4::X).unwrap();
    assert!(verify(rfc8037_a4::JWS, &ed, Algorithm::EdDSA).unwrap());
    // One character of the signature changed, well inside it.
    let mut flipped: Vec<char> = rfc8037_a4::JWS.chars().collect();
    let at = flipped.len() - 10;
    flipped[at] = if flipped[at] == 'A' { 'B' } else { 'A' };
    let flipped: String = flipped.into_iter().collect();
    assert!(!verify(&flipped, &ed, Algorithm::EdDSA).unwrap());
    // The right key with another algorithm, and HMAC with anything, are refused before any signature is checked.
    assert!(verify(rfc7515_a2::JWS, &rsa, Algorithm::RS512).is_err());
    assert!(verify(rfc7515_a2::JWS, &rsa, Algorithm::ES256).is_err());
    let secret = DecodingKey::from_secret(b"secret");
    assert!(verify(rfc7515_a2::JWS, &secret, Algorithm::HS256).is_err());
}
