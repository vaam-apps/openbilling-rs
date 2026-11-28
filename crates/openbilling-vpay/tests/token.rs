//! Minting the `vpay:v1` token inside the adapter, against the stand-in's token endpoint, which verifies the
//! client assertion as vpay's OP does.

#![allow(clippy::unwrap_used, clippy::expect_used)]

use std::time::Duration;

use openbilling_core::{
    Classify, CreditId, ErrorClass, PaymentError, PaymentProvider, PaymentRefusal,
};
use openbilling_vpay::stand_in::{
    CLIENT_ID, CLIENT_KEY_PEM, OTHER_KEY_PEM, SCOPE, StandIn, client_credentials, config, start,
};
use openbilling_vpay::{ClientCredentials, ConfigError, Credential, KeySource, VpayProvider};

fn provider(stand_in: &StandIn, tune: impl FnOnce(&mut ClientCredentials)) -> VpayProvider {
    let mut settings = client_credentials();
    tune(&mut settings);
    let mut config = config(&stand_in.base_url);
    config.credential = Credential::ClientCredentials(settings);
    VpayProvider::new(config).unwrap()
}

fn id() -> CreditId {
    CreditId::new("pi_any").unwrap()
}

/// A read that reaches the API: the stand-in has no such payment, which is an answer, not a failure.
async fn read(provider: &VpayProvider) -> Result<(), PaymentError> {
    match provider.read_payment(&id()).await {
        Err(error) if error.refusal() == Some(PaymentRefusal::NotFound) => Ok(()),
        other => other.map(|_| ()),
    }
}

/// The error and everything it wraps, as a log line would show it.
fn chain(error: &PaymentError) -> String {
    let mut text = format!("{error} / {error:?}");
    let mut source = std::error::Error::source(error);
    while let Some(inner) = source {
        text.push_str(&format!(" / {inner} / {inner:?}"));
        source = inner.source();
    }
    text
}

fn mints(stand_in: &StandIn) -> u32 {
    stand_in.state.lock().unwrap().mints
}

fn token_requests(stand_in: &StandIn) -> u32 {
    stand_in.state.lock().unwrap().token_requests
}

#[tokio::test]
async fn first_use_mints_one_token_with_vpays_request_and_reuses_it() {
    let stand_in = start().await;
    let provider = provider(&stand_in, |_| {});
    assert_eq!(
        token_requests(&stand_in),
        0,
        "nothing is minted before first use"
    );

    read(&provider).await.unwrap();
    read(&provider).await.unwrap();
    assert_eq!(mints(&stand_in), 1);

    let state = stand_in.state.lock().unwrap();
    let form = &state.token_forms[0];
    assert_eq!(form["grant_type"], "client_credentials");
    assert_eq!(form["client_id"], CLIENT_ID);
    assert_eq!(
        form["client_assertion_type"],
        "urn:ietf:params:oauth:client-assertion-type:jwt-bearer"
    );
    assert_eq!(form["audience"], "vpay:v1");
    assert!(!form.contains_key("scope"), "no scope unless configured");
    let claims = &state.assertions[0];
    assert_eq!(claims["iss"], CLIENT_ID);
    assert_eq!(claims["sub"], CLIENT_ID);
    assert_eq!(
        claims["aud"],
        format!("{}/v1/oauth/token", stand_in.base_url),
        "the assertion names the token endpoint by default"
    );
    assert_eq!(
        claims["exp"].as_u64().unwrap() - claims["iat"].as_u64().unwrap(),
        60
    );
    assert!(claims["jti"].as_str().unwrap().len() >= 32);
    let bearer: Vec<_> = state
        .seen
        .iter()
        .filter_map(|s| s.authorization.clone())
        .collect();
    assert_eq!(bearer.len(), 2);
    assert_eq!(bearer[0], bearer[1], "the cached token is reused");
}

#[tokio::test]
async fn a_token_is_replaced_before_it_expires() {
    let stand_in = start().await;
    // A 4 s token and a 2 s margin: replaced from 2 s on.
    stand_in.state.lock().unwrap().token_ttl_secs = Some(4);
    let provider = provider(&stand_in, |s| s.refresh_skew = Duration::from_secs(2));
    read(&provider).await.unwrap();
    read(&provider).await.unwrap();
    assert_eq!(mints(&stand_in), 1, "still fresh");

    tokio::time::sleep(Duration::from_millis(2100)).await;
    read(&provider).await.unwrap();
    assert_eq!(
        mints(&stand_in),
        2,
        "refreshed before the first token's 4 s were up"
    );
    let state = stand_in.state.lock().unwrap();
    let bearer: Vec<_> = state
        .seen
        .iter()
        .filter_map(|s| s.authorization.clone())
        .collect();
    assert_ne!(bearer[0], bearer[2]);
    assert_eq!(state.spent_jtis.len(), 2, "each mint is a new assertion");
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn concurrent_callers_mint_once() {
    let stand_in = start().await;
    stand_in.state.lock().unwrap().token_delay_ms = 300;
    let provider = provider(&stand_in, |_| {});
    let calls: Vec<_> = (0..12)
        .map(|_| {
            let provider = provider.clone();
            tokio::spawn(async move { read(&provider).await })
        })
        .collect();
    for call in calls {
        call.await.unwrap().unwrap();
    }
    assert_eq!(
        token_requests(&stand_in),
        1,
        "one request reached the token endpoint"
    );
    assert_eq!(mints(&stand_in), 1);
}

#[tokio::test]
async fn a_refusal_is_classed_names_vpays_code_and_is_not_retried() {
    let stand_in = start().await;
    let wrong_key = provider(&stand_in, |s| {
        s.key = KeySource::Pem(OTHER_KEY_PEM.to_owned())
    });
    let error = read(&wrong_key).await.unwrap_err();
    assert_eq!(error.class(), ErrorClass::Internal);
    assert!(chain(&error).contains("invalid_client"), "{error}");
    assert_eq!(token_requests(&stand_in), 1, "a refusal is not retried");
    assert_eq!(mints(&stand_in), 0);
    let text = chain(&error);
    assert!(
        !text.contains("stand-in"),
        "vpay's description is never kept: {text}"
    );
    assert!(!text.contains("BEGIN") && !text.contains("MIIE"), "{text}");

    let other_audience = provider(&stand_in, |s| s.audience = "vpay:other".to_owned());
    let error = read(&other_audience).await.unwrap_err();
    assert!(chain(&error).contains("invalid_target"), "{error}");

    let bad_scope = provider(&stand_in, |s| s.scope = Some("refunds:write".to_owned()));
    let error = read(&bad_scope).await.unwrap_err();
    assert!(chain(&error).contains("invalid_scope"), "{error}");

    let wrong_audience = provider(&stand_in, |s| {
        s.assertion_audience = Some("https://elsewhere.example/v1/oauth".to_owned());
    });
    let error = read(&wrong_audience).await.unwrap_err();
    assert!(chain(&error).contains("invalid_client"), "{error}");
}

#[tokio::test]
async fn a_scope_the_client_may_ask_for_is_sent() {
    let stand_in = start().await;
    let provider = provider(&stand_in, |s| s.scope = Some(SCOPE.to_owned()));
    read(&provider).await.unwrap();
    assert_eq!(
        stand_in.state.lock().unwrap().token_forms[0]["scope"],
        SCOPE
    );
}

#[tokio::test]
async fn a_transient_failure_is_retried_then_succeeds() {
    let stand_in = start().await;
    stand_in.state.lock().unwrap().token_fail = Some((503, 2));
    let provider = provider(&stand_in, |s| {
        s.retries = 2;
        s.retry_backoff = Duration::from_millis(10);
    });
    read(&provider).await.unwrap();
    assert_eq!(token_requests(&stand_in), 3);
    assert_eq!(mints(&stand_in), 1);
}

#[tokio::test]
async fn retries_are_bounded_and_the_error_is_unavailable() {
    let stand_in = start().await;
    stand_in.state.lock().unwrap().token_fail = Some((503, 100));
    let provider = provider(&stand_in, |s| {
        s.retries = 1;
        s.retry_backoff = Duration::from_millis(10);
    });
    let error = read(&provider).await.unwrap_err();
    assert_eq!(error.class(), ErrorClass::Unavailable);
    assert_eq!(token_requests(&stand_in), 2, "one attempt and one retry");

    stand_in.state.lock().unwrap().token_fail = None;
    read(&provider).await.unwrap();
}

#[tokio::test]
async fn a_replayed_or_tampered_assertion_is_refused_by_the_stand_in() {
    let stand_in = start().await;
    let provider = provider(&stand_in, |_| {});
    read(&provider).await.unwrap();
    let form = stand_in.state.lock().unwrap().token_forms[0].clone();

    let post = |form: &std::collections::BTreeMap<String, String>| {
        let body = form
            .iter()
            .map(|(k, v)| format!("{k}={}", v.replace(':', "%3A")))
            .collect::<Vec<_>>()
            .join("&");
        let url = format!("{}/v1/oauth/token", stand_in.base_url);
        async move {
            reqwest::Client::new()
                .post(url)
                .header("content-type", "application/x-www-form-urlencoded")
                .body(body)
                .send()
                .await
                .unwrap()
                .status()
                .as_u16()
        }
    };

    assert_eq!(post(&form).await, 401, "the assertion was spent once");

    // A fresh assertion, then one character of its claims changed: the signature no longer holds.
    let stand_in2 = start().await;
    let provider2 = self::provider(&stand_in2, |_| {});
    read(&provider2).await.unwrap();
    let mut forged = stand_in2.state.lock().unwrap().token_forms[0].clone();
    stand_in2.state.lock().unwrap().spent_jtis.clear();
    let assertion = forged["client_assertion"].clone();
    let mut parts: Vec<String> = assertion.split('.').map(str::to_owned).collect();
    let claims = &mut parts[1];
    let last = claims.pop().unwrap();
    claims.push(if last == 'A' { 'B' } else { 'A' });
    forged.insert("client_assertion".to_owned(), parts.join("."));
    let body = forged
        .iter()
        .map(|(k, v)| format!("{k}={}", v.replace(':', "%3A")))
        .collect::<Vec<_>>()
        .join("&");
    let status = reqwest::Client::new()
        .post(format!("{}/v1/oauth/token", stand_in2.base_url))
        .header("content-type", "application/x-www-form-urlencoded")
        .body(body)
        .send()
        .await
        .unwrap()
        .status()
        .as_u16();
    assert_eq!(status, 401, "a tampered assertion is refused");
    assert_eq!(mints(&stand_in2), 1, "and mints nothing more");
}

#[tokio::test]
async fn the_key_id_is_sent_when_configured_and_required_when_the_client_has_several_keys() {
    let stand_in = start().await;
    stand_in.state.lock().unwrap().expected_kid = Some("key-2026".to_owned());
    let without = provider(&stand_in, |_| {});
    assert!(read(&without).await.is_err());

    let with = provider(&stand_in, |s| s.key_id = Some("key-2026".to_owned()));
    read(&with).await.unwrap();
}

#[tokio::test]
async fn a_token_vpay_refuses_is_dropped_and_the_next_call_mints_another() {
    let stand_in = start().await;
    let provider = provider(&stand_in, |_| {});
    read(&provider).await.unwrap();
    stand_in.state.lock().unwrap().tokens.clear(); // revoked at vpay
    let error = read(&provider).await.unwrap_err();
    assert_eq!(
        error.class(),
        ErrorClass::Internal,
        "vpay refused the credential"
    );
    read(&provider).await.unwrap();
    assert_eq!(mints(&stand_in), 2);
}

#[tokio::test]
async fn the_key_file_is_read_at_each_mint_so_a_rotation_needs_no_restart() {
    let stand_in = start().await;
    let dir = std::env::temp_dir().join(format!("ob-vpay-key-{}", std::process::id()));
    std::fs::create_dir_all(&dir).unwrap();
    let path = dir.join("client.pem");
    std::fs::write(&path, CLIENT_KEY_PEM).unwrap();
    let provider = provider(&stand_in, |s| s.key = KeySource::File(path.clone()));
    read(&provider).await.unwrap();

    // The key is rotated to one vpay does not know, and the token has to be replaced.
    std::fs::write(&path, OTHER_KEY_PEM).unwrap();
    stand_in.state.lock().unwrap().tokens.clear();
    let _ = read(&provider).await; // the 401 drops the cached token
    let error = read(&provider).await.unwrap_err();
    assert!(chain(&error).contains("invalid_client"), "{error}");

    std::fs::write(&path, CLIENT_KEY_PEM).unwrap();
    read(&provider).await.unwrap();

    std::fs::remove_file(&path).unwrap();
    stand_in.state.lock().unwrap().tokens.clear();
    let _ = read(&provider).await;
    let error = read(&provider).await.unwrap_err();
    assert_eq!(
        error.class(),
        ErrorClass::Unavailable,
        "a missing key file is not the adapter's fault"
    );
    assert!(!chain(&error).contains(path.to_str().unwrap()));
    let _ = std::fs::remove_dir(&dir);
}

#[tokio::test]
async fn unusable_settings_are_refused_at_start() {
    let stand_in = start().await;
    let refused = |tune: &dyn Fn(&mut ClientCredentials)| {
        let mut settings = client_credentials();
        tune(&mut settings);
        let mut config = config(&stand_in.base_url);
        config.credential = Credential::ClientCredentials(settings);
        VpayProvider::new(config).unwrap_err()
    };
    assert_eq!(
        refused(&|s| s.client_id = String::new()),
        ConfigError::ClientCredentials
    );
    assert_eq!(
        refused(&|s| s.client_id = "a b".into()),
        ConfigError::ClientCredentials
    );
    assert_eq!(
        refused(&|s| s.audience = String::new()),
        ConfigError::ClientCredentials
    );
    assert_eq!(
        refused(&|s| s.scope = Some(String::new())),
        ConfigError::ClientCredentials
    );
    assert_eq!(
        refused(&|s| s.key_id = Some("k\n".into())),
        ConfigError::ClientCredentials
    );
    assert_eq!(
        refused(&|s| s.refresh_skew = Duration::from_secs(601)),
        ConfigError::ClientCredentials
    );
    assert_eq!(refused(&|s| s.retries = 11), ConfigError::ClientCredentials);
    assert_eq!(
        refused(&|s| s.retry_backoff = Duration::from_secs(6)),
        ConfigError::ClientCredentials
    );
    assert_eq!(
        refused(&|s| s.token_url = Some("http://vpay.example/t".into())),
        ConfigError::TokenUrl
    );
    assert_eq!(
        refused(&|s| s.key = KeySource::Pem("nope".into())),
        ConfigError::ClientKey
    );
    assert_eq!(
        refused(&|s| s.key = KeySource::File("/nonexistent/key.pem".into())),
        ConfigError::ClientKey
    );
}

#[tokio::test]
async fn no_error_or_debug_output_holds_the_key_or_a_token() {
    let stand_in = start().await;
    let provider = provider(&stand_in, |_| {});
    read(&provider).await.unwrap();
    let token = stand_in
        .state
        .lock()
        .unwrap()
        .tokens
        .keys()
        .next()
        .cloned()
        .unwrap();
    let key_body = CLIENT_KEY_PEM.lines().nth(2).unwrap();
    let mut shown = vec![
        format!("{provider:?}"),
        format!("{:?}", config(&stand_in.base_url)),
    ];
    stand_in.state.lock().unwrap().fail_with = Some(403);
    shown.push(chain(&read(&provider).await.unwrap_err()));
    for text in shown {
        assert!(!text.contains(&token), "{text}");
        assert!(!text.contains(key_body), "{text}");
        assert!(!text.contains("BEGIN"), "{text}");
    }
}

fn now() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap()
        .as_secs()
}

/// A token request for `client_assertion`, as vpay's form decoder reads it; the status it answers.
async fn post_assertion(stand_in: &StandIn, assertion: &str, content_type: &str) -> u16 {
    let body = format!(
        "grant_type=client_credentials&client_id={CLIENT_ID}&audience=vpay%3Av1\
         &client_assertion_type=urn%3Aietf%3Aparams%3Aoauth%3Aclient-assertion-type%3Ajwt-bearer\
         &client_assertion={assertion}"
    );
    reqwest::Client::new()
        .post(format!("{}/v1/oauth/token", stand_in.base_url))
        .header("content-type", content_type)
        .body(body)
        .send()
        .await
        .unwrap()
        .status()
        .as_u16()
}

#[tokio::test]
async fn the_stand_in_refuses_an_assertion_as_vpays_op_does() {
    let stand_in = start().await;
    let aud = format!("{}/v1/oauth/token", stand_in.base_url);
    let claims = |tune: &dyn Fn(&mut serde_json::Value)| {
        let t = now();
        let mut claims = serde_json::json!({
            "iss": CLIENT_ID, "sub": CLIENT_ID, "aud": aud,
            "jti": format!("forged-{}", rand_suffix()), "iat": t, "exp": t + 60,
        });
        tune(&mut claims);
        stand_in::assertion(CLIENT_KEY_PEM, None, &claims)
    };
    use openbilling_vpay::stand_in;
    let form = "application/x-www-form-urlencoded";

    // The control: what the adapter itself mints is accepted, so each refusal below is the one change.
    assert_eq!(post_assertion(&stand_in, &claims(&|_| {}), form).await, 200);
    let minted = mints(&stand_in);

    let t = now();
    type Tune = Box<dyn Fn(&mut serde_json::Value)>;
    let cases: [(&str, Tune); 7] = [
        (
            "a life of 340 s (the OP's ceiling is 300, with no leeway)",
            Box::new(move |c| c["exp"] = (t + 340).into()),
        ),
        (
            "an exp past its 60 s leeway",
            Box::new(move |c| c["exp"] = (t - 120).into()),
        ),
        ("another issuer", Box::new(|c| c["iss"] = "someone".into())),
        ("another subject", Box::new(|c| c["sub"] = "someone".into())),
        (
            "no jti",
            Box::new(|c| {
                c.as_object_mut().unwrap().remove("jti");
            }),
        ),
        (
            "no exp",
            Box::new(|c| {
                c.as_object_mut().unwrap().remove("exp");
            }),
        ),
        (
            "an audience that is the host alone",
            Box::new(|c| c["aud"] = "http://127.0.0.1".into()),
        ),
    ];
    for (what, tune) in cases {
        assert_eq!(
            post_assertion(&stand_in, &claims(&*tune), form).await,
            401,
            "{what}"
        );
    }
    assert_eq!(
        mints(&stand_in),
        minted,
        "nothing was issued for any of them"
    );

    // The issuer, as the OP's other accepted name for itself, and a replay of a jti.
    let issuer = format!("{}/v1/oauth", stand_in.base_url);
    assert_eq!(
        post_assertion(
            &stand_in,
            &claims(&|c| c["aud"] = issuer.clone().into()),
            form
        )
        .await,
        200
    );
    let once = claims(&|c| c["jti"] = "spent-once".into());
    assert_eq!(post_assertion(&stand_in, &once, form).await, 200);
    assert_eq!(
        post_assertion(&stand_in, &once, form).await,
        401,
        "a spent jti"
    );

    // RFC 6749 fixes the encoding: vpay's form extractor answers anything else with a 415.
    assert_eq!(
        post_assertion(&stand_in, &claims(&|_| {}), "application/json").await,
        415
    );
}

fn rand_suffix() -> u64 {
    use std::sync::atomic::{AtomicU64, Ordering};
    static NEXT: AtomicU64 = AtomicU64::new(0);
    NEXT.fetch_add(1, Ordering::Relaxed)
}

#[test]
fn a_static_token_is_never_printed_by_the_provider() {
    let mut config = config("http://127.0.0.1:1");
    config.credential = Credential::Token("static-token-value".to_owned());
    let provider = VpayProvider::new(config).unwrap();
    let shown = format!("{provider:?}");
    assert!(!shown.contains("static-token-value"), "{shown}");
}
