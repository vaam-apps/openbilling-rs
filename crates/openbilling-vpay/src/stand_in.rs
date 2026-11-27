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

use std::collections::BTreeMap;
use std::sync::{Arc, Mutex};
use std::time::Duration;

use crate::{Credential, CurrencyUnit, VpayConfig, VpayProvider, WebhookSecret, webhook};
use axum::Router;
use axum::body::Bytes;
use axum::extract::{Path, State};
use axum::http::{HeaderMap, StatusCode};
use axum::response::{IntoResponse, Response};
use axum::routing::{get, post};
use openbilling_core::{Amount, CreditId, CurrencyCode, RefundStatus, Timestamp, WebhookHeaders};
use openbilling_testkit::payments::{ProviderFixture, SignedWebhook};
use serde_json::{Value, json};

/// The token the stand-in accepts.
pub const TOKEN: &str = "test-token-not-a-secret";
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

fn authorized(headers: &HeaderMap) -> bool {
    headers
        .get("authorization")
        .and_then(|v| v.to_str().ok())
        .is_some_and(|v| v == format!("Bearer {TOKEN}"))
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
    if !authorized(headers) {
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
    if !authorized(headers) {
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
        .route("/v1/payment_intents", post(create_intent))
        .route("/v1/payment_intents/{id}", get(get_intent))
        .route("/v1/checkout/sessions", post(create_session))
        .route("/v1/refunds/{id}", get(get_refund))
        .with_state(state.clone());
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let address = listener.local_addr().unwrap();
    tokio::spawn(async move { axum::serve(listener, app).await.unwrap() });
    StandIn {
        state,
        base_url: format!("http://{address}"),
    }
}

pub fn eur() -> CurrencyCode {
    CurrencyCode::new("EUR").unwrap()
}

/// The adapter's configuration for a stand-in at `base_url`.
pub fn config(base_url: &str) -> VpayConfig {
    VpayConfig {
        base_url: base_url.to_owned(),
        credential: Credential::Token(TOKEN.to_owned()),
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
