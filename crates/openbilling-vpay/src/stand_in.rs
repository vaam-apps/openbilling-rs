//! A stand-in for vpay's `/v1`, for tests: in process, on a loopback port, the parts of the merchant API the adapter
//! calls, answering as vpay answers (*verified 2026-11-27*, vpay `8b56831`, the files the adapter cites), and a
//! fixture that drives it for `openbilling-testkit`'s payment conformance suite.
//!
//! **Test code**, behind the feature `stand-in`, which no shipping binary enables. It is a test double of vpay
//! reached over HTTP exactly as vpay is: the adapter talks to it through its ordinary client.

#![allow(
    missing_docs,
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::panic,
    clippy::result_large_err,
    clippy::missing_panics_doc
)]

use std::collections::{BTreeMap, BTreeSet};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use crate::token::parse_key;
use crate::{
    ClientCredentials, Credential, CurrencyUnit, KeySource, VpayConfig, VpayProvider,
    WebhookSecret, webhook,
};
use axum::Router;
use axum::body::Bytes;
use axum::extract::{Path, State};
use axum::http::{HeaderMap, StatusCode};
use axum::response::{IntoResponse, Response};
use axum::routing::{get, post};
use base64::Engine as _;
use base64::engine::general_purpose::URL_SAFE_NO_PAD;
use openbilling_core::{Amount, CreditId, CurrencyCode, RefundStatus, Timestamp, WebhookHeaders};
use openbilling_testkit::payments::{ProviderFixture, SignedWebhook};
use ring::signature::{RSA_PKCS1_2048_8192_SHA256, UnparsedPublicKey};
use serde_json::{Value, json};

/// A token the stand-in accepts without minting: one an operator provisioned elsewhere (`Credential::TokenFile`).
pub const TOKEN: &str = "test-token-not-a-secret";
/// The `client_id` registered at the stand-in.
pub const CLIENT_ID: &str = "test-merchant";
/// The private key whose public half is registered at the stand-in. A test key: it signs nothing real.
pub const CLIENT_KEY_PEM: &str = include_str!("../tests/vectors/test-client-key.pem");
/// A key the stand-in does not know, for an assertion it must refuse.
pub const OTHER_KEY_PEM: &str = include_str!("../tests/vectors/test-other-key.pem");
/// The scope the registered client may ask for.
pub const SCOPE: &str = "payments:write";
/// The webhook secret the stand-in signs with.
pub const SECRET: &str = "whsec_test_generated_in_tests";

#[derive(Debug, Clone)]
pub struct Intent {
    pub id: String,
    pub amount: i64,
    pub currency: String,
    pub status: &'static str,
    pub metadata: BTreeMap<String, String>,
    pub payment_method_types: Vec<String>,
}

#[derive(Debug, Clone)]
pub struct RefundRow {
    pub id: String,
    pub payment_intent: String,
    pub amount: i64,
    pub currency: String,
    pub status: &'static str,
}

/// One request the stand-in received.
#[derive(Debug, Clone)]
pub struct Seen {
    pub method: String,
    pub path: String,
    pub authorization: Option<String>,
    pub idempotency_key: Option<String>,
    pub body: String,
}

#[derive(Debug, Default)]
pub struct Vpay {
    pub intents: BTreeMap<String, Intent>,
    pub sessions: BTreeMap<String, String>,
    pub refunds: BTreeMap<String, RefundRow>,
    /// Idempotency: key to (method and path and body, status, answer).
    pub keys: BTreeMap<String, (String, u16, Value)>,
    pub seen: Vec<Seen>,
    next: u32,
    /// When set, every `/v1` call answers this status with vpay's envelope.
    pub fail_with: Option<u16>,
    /// The stand-in's own base URL: the audience an assertion may name is `{base_url}/v1/oauth/token` or
    /// `{base_url}/v1/oauth`.
    pub base_url: String,
    /// The `kid` the registered key has; an assertion must carry it when set.
    pub expected_kid: Option<String>,
    /// How long a token lives, in seconds; 900 when unset, as at vpay.
    pub token_ttl_secs: Option<u64>,
    /// Makes the token endpoint answer this status, this many times (then it answers as usual).
    pub token_fail: Option<(u16, u32)>,
    /// Tokens issued, with their expiry.
    pub tokens: BTreeMap<String, Instant>,
    /// Assertion ids already spent: a replay is refused, as at vpay.
    pub spent_jtis: BTreeSet<String>,
    /// How long the token endpoint takes to answer, in milliseconds.
    pub token_delay_ms: u64,
    /// How many tokens were issued.
    pub mints: u32,
    /// How many requests reached the token endpoint, refused or not.
    pub token_requests: u32,
    /// The decoded claims of every assertion that verified.
    pub assertions: Vec<Value>,
    /// The form of every token request.
    pub token_forms: Vec<BTreeMap<String, String>>,
}

impl Vpay {
    fn mint(&mut self, prefix: &str) -> String {
        self.next += 1;
        format!("{prefix}{:08}", self.next)
    }
}

pub type Shared = Arc<Mutex<Vpay>>;

fn envelope(status: u16, kind: &str, code: &str) -> Response {
    let body = json!({"error": {"type": kind, "code": code, "message": "stand-in"}});
    (StatusCode::from_u16(status).unwrap(), axum::Json(body)).into_response()
}

fn percent_decode(input: &str) -> String {
    let bytes = input.as_bytes();
    let mut out = Vec::new();
    let mut i = 0;
    while i < bytes.len() {
        if bytes[i] == b'%' && i + 3 <= bytes.len() {
            let hex = std::str::from_utf8(&bytes[i + 1..i + 3]).unwrap();
            out.push(u8::from_str_radix(hex, 16).unwrap());
            i += 3;
        } else {
            out.push(bytes[i]);
            i += 1;
        }
    }
    String::from_utf8(out).unwrap()
}

/// vpay's decoder: brackets are structural and literal; each segment and value percent-decoded; `+` is a plus.
fn decode(body: &str) -> BTreeMap<String, String> {
    body.split('&')
        .filter(|pair| !pair.is_empty())
        .map(|pair| {
            let (k, v) = pair.split_once('=').unwrap_or((pair, ""));
            let key = match k.find('[') {
                Some(open) => {
                    let head = percent_decode(&k[..open]);
                    let rest = &k[open..];
                    let inner = rest.trim_start_matches('[').trim_end_matches(']');
                    format!("{head}[{}]", percent_decode(inner))
                }
                None => percent_decode(k),
            };
            (key, percent_decode(v))
        })
        .collect()
}

fn authorized(vpay: &Vpay, headers: &HeaderMap) -> bool {
    let Some(token) = headers
        .get("authorization")
        .and_then(|v| v.to_str().ok())
        .and_then(|v| v.strip_prefix("Bearer "))
    else {
        return false;
    };
    token == TOKEN
        || vpay
            .tokens
            .get(token)
            .is_some_and(|expires| Instant::now() < *expires)
}

fn token_error(status: u16, code: &str) -> Response {
    (
        StatusCode::from_u16(status).unwrap(),
        axum::Json(json!({"error": code, "error_description": "stand-in"})),
    )
        .into_response()
}

/// Verifies a client assertion as vpay does (`docs/flows/merchant-auth.md`, and `authkestra-op` 0.13.1's
/// `verify_client_assertion`): RS256 under the registered key, `iss` and `sub` the client, `aud` the OP's token
/// endpoint or issuer, `exp` not more than 60 s past (the OP's clock leeway) and not more than 300 s ahead (no
/// leeway on that bound), a `jti` spent once.
fn verify_assertion(
    vpay: &mut Vpay,
    assertion: &str,
    client_id: &str,
) -> Result<Value, &'static str> {
    let parts: Vec<&str> = assertion.split('.').collect();
    let [header, claims, signature] = parts[..] else {
        return Err("invalid_client");
    };
    let decode_json = |part: &str| -> Option<Value> {
        serde_json::from_slice(&URL_SAFE_NO_PAD.decode(part).ok()?).ok()
    };
    let header = decode_json(header).ok_or("invalid_client")?;
    if header["alg"] != "RS256" {
        return Err("invalid_client");
    }
    if let Some(kid) = &vpay.expected_kid
        && header["kid"] != json!(kid)
    {
        return Err("invalid_client");
    }
    let registered = parse_key(CLIENT_KEY_PEM).expect("the test key");
    let signature = URL_SAFE_NO_PAD
        .decode(signature)
        .map_err(|_| "invalid_client")?;
    UnparsedPublicKey::new(&RSA_PKCS1_2048_8192_SHA256, registered.public().as_ref())
        .verify(format!("{}.{}", parts[0], parts[1]).as_bytes(), &signature)
        .map_err(|_| "invalid_client")?;
    let claims = decode_json(claims).ok_or("invalid_client")?;
    let base = &vpay.base_url;
    let audiences = [format!("{base}/v1/oauth/token"), format!("{base}/v1/oauth")];
    let now = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap()
        .as_secs();
    let exp = claims["exp"].as_u64().ok_or("invalid_client")?;
    let jti = claims["jti"]
        .as_str()
        .filter(|j| !j.is_empty())
        .ok_or("invalid_client")?;
    if claims["iss"] != client_id
        || claims["sub"] != client_id
        || !audiences.iter().any(|a| claims["aud"] == json!(a))
        || exp + 60 < now
        || exp > now + 300
    {
        return Err("invalid_client");
    }
    if !vpay.spent_jtis.insert(jti.to_owned()) {
        return Err("invalid_client");
    }
    Ok(claims)
}

/// `POST /v1/oauth/token`: `client_credentials` with a `private_key_jwt`, as vpay's OP serves it.
async fn token_endpoint(State(state): State<Shared>, headers: HeaderMap, body: Bytes) -> Response {
    // RFC 6749 §4.4.2 fixes the encoding: vpay's `RawForm` answers any other content type with a 415.
    let form_encoded = headers
        .get("content-type")
        .and_then(|v| v.to_str().ok())
        .is_some_and(|v| v.starts_with("application/x-www-form-urlencoded"));
    if !form_encoded {
        return StatusCode::UNSUPPORTED_MEDIA_TYPE.into_response();
    }
    let delay = state.lock().unwrap().token_delay_ms;
    if delay > 0 {
        tokio::time::sleep(Duration::from_millis(delay)).await;
    }
    let mut vpay = state.lock().unwrap();
    vpay.token_requests += 1;
    let form = decode(std::str::from_utf8(&body).unwrap());
    vpay.token_forms.push(form.clone());
    if let Some((status, left)) = vpay.token_fail
        && left > 0
    {
        vpay.token_fail = Some((status, left - 1));
        return token_error(status, "server_error");
    }
    let field = |name: &str| form.get(name).map(String::as_str);
    if field("grant_type") != Some("client_credentials") {
        return token_error(400, "unsupported_grant_type");
    }
    if field("client_assertion_type")
        != Some("urn:ietf:params:oauth:client-assertion-type:jwt-bearer")
        || field("client_id") != Some(CLIENT_ID)
    {
        return token_error(401, "invalid_client");
    }
    let claims = match verify_assertion(
        &mut vpay,
        field("client_assertion").unwrap_or(""),
        CLIENT_ID,
    ) {
        Ok(claims) => claims,
        Err(code) => return token_error(401, code),
    };
    if field("audience") != Some("vpay:v1") {
        return token_error(400, "invalid_target");
    }
    if field("scope").is_some_and(|scope| scope != SCOPE) {
        return token_error(400, "invalid_scope");
    }
    vpay.assertions.push(claims);
    vpay.mints += 1;
    let ttl = vpay.token_ttl_secs.unwrap_or(900);
    let token = format!("vt_{:08}_{}", vpay.mints, vpay.spent_jtis.len());
    vpay.tokens
        .insert(token.clone(), Instant::now() + Duration::from_secs(ttl));
    axum::Json(json!({
        "access_token": token, "token_type": "Bearer", "expires_in": ttl,
        "scope": field("scope").unwrap_or(SCOPE),
    }))
    .into_response()
}

fn record(state: &Shared, method: &str, path: &str, headers: &HeaderMap, body: &str) {
    state.lock().unwrap().seen.push(Seen {
        method: method.to_owned(),
        path: path.to_owned(),
        authorization: headers
            .get("authorization")
            .map(|v| v.to_str().unwrap().to_owned()),
        idempotency_key: headers
            .get("idempotency-key")
            .map(|v| v.to_str().unwrap().to_owned()),
        body: body.to_owned(),
    });
}

fn intent_json(intent: &Intent) -> Value {
    json!({
        "id": intent.id, "object": "payment_intent", "amount": intent.amount, "currency": intent.currency,
        "status": intent.status, "payment_method_types": intent.payment_method_types,
        "payment_method_details": null, "next_action": null, "last_payment_error": null,
        "metadata": intent.metadata, "description": null, "customer": null, "created": 1_795_737_600,
        "livemode": false, "client_secret": format!("{}_secret_x", intent.id),
    })
}

fn refund_json(refund: &RefundRow) -> Value {
    json!({
        "id": refund.id, "object": "refund", "amount": refund.amount, "currency": refund.currency,
        "payment_intent": refund.payment_intent, "status": refund.status, "reason": null, "metadata": {},
        "created": 1_795_737_600, "fee": null,
    })
}

/// The idempotent `POST` shape: a key is required; a replay answers the first answer; the same key with another
/// body is a `400` of type `idempotency_error`.
fn idempotent(
    state: &Shared,
    path: &str,
    headers: &HeaderMap,
    body: &str,
    create: impl FnOnce(&mut Vpay, &BTreeMap<String, String>) -> Result<Value, Response>,
) -> Response {
    record(state, "POST", path, headers, body);
    let mut vpay = state.lock().unwrap();
    if let Some(status) = vpay.fail_with {
        return envelope(status, "api_error", "stand_in");
    }
    if !authorized(&vpay, headers) {
        return envelope(401, "authentication_error", "invalid_token");
    }
    let Some(key) = headers.get("idempotency-key").and_then(|v| v.to_str().ok()) else {
        return envelope(400, "invalid_request_error", "invalid_request");
    };
    let fingerprint = format!("POST {path} {body}");
    if let Some((seen, status, answer)) = vpay.keys.get(key) {
        if *seen != fingerprint {
            return envelope(400, "idempotency_error", "idempotency_key_in_use");
        }
        return (
            StatusCode::from_u16(*status).unwrap(),
            axum::Json(answer.clone()),
        )
            .into_response();
    }
    let form = decode(body);
    match create(&mut vpay, &form) {
        Ok(answer) => {
            vpay.keys
                .insert(key.to_owned(), (fingerprint, 200, answer.clone()));
            axum::Json(answer).into_response()
        }
        Err(refused) => refused,
    }
}

async fn create_intent(State(state): State<Shared>, headers: HeaderMap, body: Bytes) -> Response {
    let body = String::from_utf8(body.to_vec()).unwrap();
    idempotent(
        &state,
        "/v1/payment_intents",
        &headers,
        &body,
        |vpay, form| {
            let amount = form
                .get("amount")
                .and_then(|a| a.parse::<i64>().ok())
                .filter(|a| *a > 0)
                .ok_or_else(|| envelope(400, "invalid_request_error", "invalid_request"))?;
            let currency = form
                .get("currency")
                .filter(|c| c.as_str() == "eur")
                .ok_or_else(|| envelope(400, "invalid_request_error", "invalid_request"))?
                .clone();
            let types: Vec<String> = (0..)
                .map_while(|i| form.get(&format!("payment_method_types[{i}]")).cloned())
                .collect();
            if types.is_empty() {
                return Err(envelope(400, "invalid_request_error", "invalid_request"));
            }
            let metadata = form
                .iter()
                .filter_map(|(k, v)| {
                    k.strip_prefix("metadata[")
                        .and_then(|k| k.strip_suffix(']'))
                        .map(|k| (k.to_owned(), v.clone()))
                })
                .collect();
            let id = vpay.mint("pi_");
            let intent = Intent {
                id: id.clone(),
                amount,
                currency,
                status: "requires_payment_method",
                metadata,
                payment_method_types: types,
            };
            let answer = intent_json(&intent);
            vpay.intents.insert(id, intent);
            Ok(answer)
        },
    )
}

async fn create_session(State(state): State<Shared>, headers: HeaderMap, body: Bytes) -> Response {
    let body = String::from_utf8(body.to_vec()).unwrap();
    idempotent(
        &state,
        "/v1/checkout/sessions",
        &headers,
        &body,
        |vpay, form| {
            let refuse = || envelope(400, "invalid_request_error", "invalid_request");
            let intent = form.get("payment_intent").ok_or_else(refuse)?.clone();
            match vpay.intents.get(&intent) {
                Some(i) if i.status == "requires_payment_method" => {}
                Some(_) => return Err(envelope(409, "invalid_request_error", "invalid_state")),
                None => return Err(refuse()),
            }
            if form.get("ui_mode").map(String::as_str) != Some("hosted")
                || !form.contains_key("success_url")
                || !form.contains_key("cancel_url")
            {
                return Err(refuse());
            }
            let id = vpay.mint("cs_");
            vpay.sessions.insert(id.clone(), intent.clone());
            Ok(json!({
                "id": id, "object": "checkout.session", "livemode": false, "payment_intent": intent,
                "ui_mode": "hosted", "status": "open", "payment_status": "unpaid",
                "success_url": form["success_url"], "cancel_url": form["cancel_url"], "return_url": null,
                "url": format!("http://checkout.invalid/c/{id}#cs_secret"), "customer": null,
                "expires_at": 1_795_824_000, "created": 1_795_737_600,
            }))
        },
    )
}

fn read(
    state: &Shared,
    path: &str,
    headers: &HeaderMap,
    found: impl FnOnce(&Vpay) -> Option<Value>,
) -> Response {
    record(state, "GET", path, headers, "");
    let vpay = state.lock().unwrap();
    if let Some(status) = vpay.fail_with {
        return envelope(status, "api_error", "stand_in");
    }
    if !authorized(&vpay, headers) {
        return envelope(401, "authentication_error", "invalid_token");
    }
    match found(&vpay) {
        Some(answer) => axum::Json(answer).into_response(),
        None => envelope(404, "invalid_request_error", "resource_missing"),
    }
}

async fn get_intent(
    State(state): State<Shared>,
    headers: HeaderMap,
    Path(id): Path<String>,
) -> Response {
    read(
        &state,
        &format!("/v1/payment_intents/{id}"),
        &headers,
        |v| v.intents.get(&id).map(intent_json),
    )
}

async fn get_refund(
    State(state): State<Shared>,
    headers: HeaderMap,
    Path(id): Path<String>,
) -> Response {
    read(&state, &format!("/v1/refunds/{id}"), &headers, |v| {
        v.refunds.get(&id).map(refund_json)
    })
}

/// A running stand-in.
pub struct StandIn {
    pub state: Shared,
    pub base_url: String,
}

/// Starts a stand-in on a free loopback port.
pub async fn start() -> StandIn {
    let state: Shared = Arc::default();
    let app = Router::new()
        .route("/v1/oauth/token", post(token_endpoint))
        .route("/v1/payment_intents", post(create_intent))
        .route("/v1/payment_intents/{id}", get(get_intent))
        .route("/v1/checkout/sessions", post(create_session))
        .route("/v1/refunds/{id}", get(get_refund))
        .with_state(state.clone());
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let address = listener.local_addr().unwrap();
    state.lock().unwrap().base_url = format!("http://{address}");
    tokio::spawn(async move { axum::serve(listener, app).await.unwrap() });
    StandIn {
        state,
        base_url: format!("http://{address}"),
    }
}

pub fn eur() -> CurrencyCode {
    CurrencyCode::new("EUR").unwrap()
}

/// A compact RS256 assertion over `claims`, signed with `key_pem`: for a test that forges one the adapter would not
/// mint (a long life, a wrong claim) to see the stand-in refuse it as vpay's OP does.
pub fn assertion(key_pem: &str, key_id: Option<&str>, claims: &Value) -> String {
    let key = parse_key(key_pem).expect("a test key");
    crate::token::sign_jwt(&key, key_id, claims).expect("a signature")
}

/// The client credentials for a stand-in: the registered client and its key, every other setting at its default.
pub fn client_credentials() -> ClientCredentials {
    ClientCredentials::new(CLIENT_ID, KeySource::Pem(CLIENT_KEY_PEM.to_owned()))
}

/// The adapter's configuration for a stand-in at `base_url`: it mints its own token.
pub fn config(base_url: &str) -> VpayConfig {
    VpayConfig {
        base_url: base_url.to_owned(),
        credential: Credential::ClientCredentials(client_credentials()),
        webhook_secrets: vec![WebhookSecret::new(SECRET).unwrap()],
        webhook_tolerance_secs: webhook::DEFAULT_TOLERANCE_SECS,
        currencies: vec![CurrencyUnit {
            currency: eur(),
            exponent: 2,
        }],
        payment_method_types: vec!["test_rail".to_owned()],
        timeout: Duration::from_secs(5),
    }
}

/// An event body as vpay renders one (`EventObject`, *verified* in `backends/crates/vpay-api/src/model.rs`).
pub fn event(kind: &str, object: Value) -> Vec<u8> {
    serde_json::to_vec(&json!({
        "id": "evt_00000001", "object": "event", "type": kind, "created": 1_795_737_600, "livemode": false,
        "data": {"object": object},
    }))
    .unwrap()
}

pub fn signed(body: Vec<u8>, at: Timestamp, secret: &str) -> SignedWebhook {
    let header = webhook::signature_header(&body, at, &[WebhookSecret::new(secret).unwrap()]);
    SignedWebhook {
        headers: WebhookHeaders::new([
            ("Vpay-Signature".to_owned(), header.clone()),
            ("Stripe-Signature".to_owned(), header),
        ]),
        body,
    }
}

/// The conformance fixture.
pub struct Fixture {
    pub stand_in: StandIn,
    pub provider: VpayProvider,
}

pub async fn fixture() -> Fixture {
    let stand_in = start().await;
    let provider = VpayProvider::new(config(&stand_in.base_url)).unwrap();
    Fixture { stand_in, provider }
}

impl Fixture {
    fn set_status(&self, payment: &CreditId, status: &'static str) {
        let mut vpay = self.stand_in.state.lock().unwrap();
        vpay.intents
            .get_mut(payment.as_str())
            .expect("a known intent")
            .status = status;
    }
}

impl ProviderFixture for Fixture {
    type Provider = VpayProvider;

    fn provider(&self) -> &VpayProvider {
        &self.provider
    }

    fn currency(&self) -> CurrencyCode {
        eur()
    }

    fn amount(&self) -> Amount {
        Amount::from_micros(12_340_000).unwrap()
    }

    fn inexact_amount(&self) -> Option<Amount> {
        Some(Amount::from_micros(12_345).unwrap())
    }

    fn tolerance_micros(&self) -> i64 {
        i64::from(webhook::DEFAULT_TOLERANCE_SECS) * 1_000_000
    }

    async fn pay(&self, payment: &CreditId) {
        self.set_status(payment, "succeeded");
    }

    async fn cancel(&self, payment: &CreditId) {
        self.set_status(payment, "canceled");
    }

    async fn refund(&self, payment: &CreditId, amount: Amount, status: RefundStatus) -> CreditId {
        let mut vpay = self.stand_in.state.lock().unwrap();
        let id = vpay.mint("re_");
        let currency = vpay.intents[payment.as_str()].currency.clone();
        vpay.refunds.insert(
            id.clone(),
            RefundRow {
                id: id.clone(),
                payment_intent: payment.as_str().to_owned(),
                amount: amount.micros() / 10_000,
                currency,
                status: status.as_str(),
            },
        );
        CreditId::new(id).unwrap()
    }

    async fn foreign_payment(&self) -> CreditId {
        let mut vpay = self.stand_in.state.lock().unwrap();
        let id = vpay.mint("pi_");
        vpay.intents.insert(
            id.clone(),
            Intent {
                id: id.clone(),
                amount: 500,
                currency: "eur".to_owned(),
                status: "succeeded",
                metadata: BTreeMap::from([("order".to_owned(), "42".to_owned())]),
                payment_method_types: vec!["test_rail".to_owned()],
            },
        );
        CreditId::new(id).unwrap()
    }

    fn payment_webhook(&self, payment: &CreditId, at: Timestamp) -> SignedWebhook {
        let body = event(
            "payment_intent.succeeded",
            json!({"id": payment.as_str(), "object": "payment_intent", "status": "succeeded"}),
        );
        signed(body, at, SECRET)
    }

    fn refund_webhook(&self, refund: &CreditId, at: Timestamp) -> SignedWebhook {
        let body = event(
            "charge.refunded",
            json!({"id": refund.as_str(), "object": "refund", "status": "pending"}),
        );
        signed(body, at, SECRET)
    }

    fn other_webhook(&self, at: Timestamp) -> SignedWebhook {
        signed(
            event(
                "customer.created",
                json!({"id": "cus_00000001", "object": "customer"}),
            ),
            at,
            SECRET,
        )
    }

    fn forged_webhook(&self, payment: &CreditId, at: Timestamp) -> SignedWebhook {
        let body = event(
            "payment_intent.succeeded",
            json!({"id": payment.as_str(), "object": "payment_intent"}),
        );
        signed(body, at, "whsec_someone_else")
    }

    fn sign(&self, body: &[u8], at: Timestamp) -> SignedWebhook {
        signed(body.to_vec(), at, SECRET)
    }
}
