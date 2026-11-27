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
}
