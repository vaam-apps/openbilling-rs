//! The external API of openbilling-rs ([ADR 0003]).
//!
//! It is a CrateStack schema, `schema/openbilling.cstack`, served by the router CrateStack generates from it
//! under `/v1`: models over the ledger's own tables, queried on demand; `query` blocks for the reads that are not
//! one table, each behind a read procedure; and mutation procedures that call the ledger's PostgreSQL functions
//! through [`PgLedger`](openbilling_pg::PgLedger). Beside it, plain axum routes: the payment provider's webhook
//! (it needs the raw body), `/healthz` and `/readyz`.
//!
//! Every call to `/v1` needs a bearer token a [`TokenVerifier`](auth::TokenVerifier) accepts, or it is a `401`
//! ([`auth`]). Every mutation needs an `Idempotency-Key`: a replay answers the first answer, and the operation's
//! own once-only id (the account id, the credit id, each event's id, the top-up key) is in its arguments too.
//!
//! [ADR 0003]: https://github.com/vaam-apps/openbilling-rs/blob/main/docs/adr/0003-credits-from-a-payment-provider.md

pub mod auth;
pub mod errors;
pub mod intake;
mod procedures;
pub mod time;
mod webhook;

/// The code CrateStack generates from `schema/openbilling.cstack`.
// Generated code, not ours: its lints are CrateStack's to fix.
#[allow(
    clippy::all,
    clippy::pedantic,
    missing_docs,
    non_camel_case_types,
    non_snake_case
)]
pub mod generated {
    cratestack::include_server_schema!("schema/openbilling.cstack", db = Postgres);
}

use std::sync::Arc;
use std::time::Duration;

use cratestack::axum::Router;
use cratestack::axum::extract::{Request, State};
use cratestack::axum::http::{Method, StatusCode};
use cratestack::axum::middleware::{self, Next};
use cratestack::axum::response::{IntoResponse, Response};
use cratestack::axum::routing::{get, post};
use cratestack::idempotency::IdempotencyLayer;
use cratestack::{CratestackError, JsonCodec, SqlxIdempotencyStore};
use openbilling_core::{LedgerStore, PaymentProvider, PriceBook};

pub use procedures::{Procedures, Services};

use crate::generated::cratestack_schema;

/// Where the CrateStack API is mounted.
pub const API_PREFIX: &str = "/v1";

/// The provider's webhook route.
pub const WEBHOOK_PATH: &str = "/webhooks/payments";

/// The mutation procedures: each needs an `Idempotency-Key` header. Held equal to the schema by a test.
pub const MUTATIONS: [&str; 11] = [
    "configure",
    "openAccount",
    "grantCredit",
    "recordUsage",
    "startTopUp",
    "reconcilePayment",
    "reconcileRefund",
    "recordRawUsage",
    "setDebtLimit",
    "setMemberCap",
    "loadPriceList",
];

/// The read procedures, which need none.
pub const READS: [&str; 5] = [
    "getBalance",
    "getAllowed",
    "listCredits",
    "getStatement",
    "getUsageSeries",
];

/// How long an `Idempotency-Key` answers its first answer.
pub const IDEMPOTENCY_TTL: Duration = Duration::from_secs(24 * 3600);

/// Creates CrateStack's own idempotency table, `cratestack_idempotency`, if it is missing. It is CrateStack's
/// bookkeeping, not a table of the ledger.
///
/// # Errors
///
/// CrateStack's error when the database refuses.
pub async fn prepare(pool: &cratestack::sqlx::PgPool) -> Result<(), CratestackError> {
    SqlxIdempotencyStore::new(pool.clone())
        .ensure_schema()
        .await
}

/// Refuses a mutation with no `Idempotency-Key`: `400`, before it is authenticated or run.
async fn require_idempotency_key(request: Request, next: Next) -> Response {
    let path = request.uri().path();
    let mutation = request.method() == Method::POST
        && path
            .strip_prefix("/$procs/")
            .is_some_and(|name| MUTATIONS.contains(&name));
    let keyed = request
        .headers()
        .get("idempotency-key")
        .and_then(|v| v.to_str().ok())
        .is_some_and(|v| !v.trim().is_empty());
    if mutation && !keyed {
        return webhook::answer(CratestackError::BadRequest(
            "idempotency_key: an Idempotency-Key header is required on every write".to_owned(),
        ));
    }
    next.run(request).await
}

/// The route and the procedure of a request, from fixed names: a path never becomes a label or an attribute as it
/// is, so an id in a path cannot leave the process.
fn classify(path: &str) -> (&'static str, &'static str) {
    if let Some(name) = path.strip_prefix("/v1/$procs/") {
        let known = MUTATIONS.iter().chain(READS.iter()).find(|n| **n == name);
        return ("/v1/$procs/{name}", known.copied().unwrap_or("unknown"));
    }
    if let Some(rest) = path.strip_prefix("/v1/") {
        let model = rest.split('/').next().unwrap_or_default();
        let route = match model {
            "accounts" => "/v1/accounts",
            "lots" => "/v1/lots",
            "usage_outcomes" => "/v1/usage_outcomes",
            _ => "/v1/other",
        };
        return (route, "");
    }
    match path {
        WEBHOOK_PATH => (WEBHOOK_PATH, ""),
        "/healthz" => ("/healthz", ""),
        "/readyz" => ("/readyz", ""),
        _ => ("other", ""),
    }
}

/// The method as a closed label.
fn method_label(method: &Method) -> &'static str {
    match *method {
        Method::GET => "GET",
        Method::POST => "POST",
        Method::PUT => "PUT",
        Method::PATCH => "PATCH",
        Method::DELETE => "DELETE",
        Method::HEAD => "HEAD",
        Method::OPTIONS => "OPTIONS",
        _ => "OTHER",
    }
}

/// The outermost layer: one `http.server.request` span per request (not for the probes) and the request duration
/// by method, route and status class. CrateStack's own spans for the procedure and the list route are its children.
async fn observe(request: Request, next: Next) -> Response {
    let method = method_label(request.method());
    let (route, procedure) = classify(request.uri().path());
    let probe = matches!(route, "/healthz" | "/readyz");
    let began = std::time::Instant::now();
    let response = if probe {
        next.run(request).await
    } else {
        let span = tracing::info_span!(
            "http.server.request",
            http.request.method = method,
            http.route = route,
            http.response.status_code = tracing::field::Empty,
        );
        let response = tracing::Instrument::instrument(next.run(request), span.clone()).await;
        span.record(
            "http.response.status_code",
            i64::from(response.status().as_u16()),
        );
        response
    };
    let status = match response.status().as_u16() {
        100..=199 => "1xx",
        200..=299 => "2xx",
        300..=399 => "3xx",
        400..=499 => "4xx",
        _ => "5xx",
    };
    openbilling_telemetry::global().observe(
        openbilling_telemetry::Histogram::HttpServer,
        &[
            ("method", method),
            ("route", route),
            ("procedure", procedure),
            ("status", status),
        ],
        began.elapsed().as_secs_f64(),
    );
    response
}

async fn healthz() -> &'static str {
    "ok"
}

async fn readyz<L: LedgerStore + 'static, P: PaymentProvider + 'static>(
    State(services): State<Arc<Services<L, P>>>,
) -> Response {
    match services.ledger.now().await {
        Ok(_) => (StatusCode::OK, "ready").into_response(),
        Err(_) => webhook::answer(CratestackError::Unavailable(
            "the database does not answer".to_owned(),
        )),
    }
}

/// The whole HTTP surface:
///
/// - `/v1/…`: the CrateStack API (models at `/v1/accounts`, `/v1/lots`, `/v1/usage_outcomes`; procedures at
///   `POST /v1/$procs/<name>`), JSON, bearer-authenticated by `auth`;
/// - `POST /webhooks/payments`: the payment provider's signed webhook;
/// - `GET /healthz` (the process answers) and `GET /readyz` (the database answers).
///
/// `pool` is the database the ledger is in: CrateStack reads its tables through it. Call [`prepare`] once
/// before serving.
pub fn router<L, P, A>(
    services: Arc<Services<L, P>>,
    pool: cratestack::sqlx::PgPool,
    auth: A,
) -> Router
where
    L: PriceBook + 'static,
    P: PaymentProvider + 'static,
    A: cratestack::AuthProvider,
{
    let db = cratestack_schema::Cratestack::builder(pool.clone()).build();
    let api = cratestack_schema::axum::router(
        db,
        Procedures(Arc::clone(&services)),
        (),
        JsonCodec,
        auth,
        cratestack::DEFAULT_BODY_LIMIT_BYTES,
    )
    .layer(IdempotencyLayer::new(
        Arc::new(SqlxIdempotencyStore::new(pool)),
        IDEMPOTENCY_TTL,
    ))
    .layer(middleware::from_fn(require_idempotency_key));
    Router::new()
        .route("/healthz", get(healthz))
        .route("/readyz", get(readyz::<L, P>))
        .route(
            WEBHOOK_PATH,
            post(webhook::receive::<L, P>).layer(cratestack::axum::extract::DefaultBodyLimit::max(
                webhook::WEBHOOK_BODY_LIMIT,
            )),
        )
        .with_state(services)
        .nest(API_PREFIX, api)
        .layer(middleware::from_fn(observe))
}
