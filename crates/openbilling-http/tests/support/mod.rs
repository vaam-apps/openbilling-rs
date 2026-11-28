//! What the route tests share: tokens, a request helper over the real router, a ledger that is down, and a fresh
//! database when `OPENBILLING_TEST_DATABASE_URL` names a server.

#![allow(dead_code, clippy::unwrap_used, clippy::expect_used, clippy::panic)]

use std::sync::Arc;
use std::sync::atomic::{AtomicU32, Ordering};
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use cratestack::axum::Router;
use cratestack::axum::body::{Body, to_bytes};
use cratestack::axum::http::{HeaderMap, Method, Request, StatusCode};
use openbilling_core::{
    AccountId, Allowed, AllowedPolicy, Amount, Balance, Credited, CurrencyCode, DrainReport,
    ErrorClass, EventId, IntakeReport, LedgerConfig, LedgerError, LedgerStore, Loaded, LotView,
    MemberId, NewAccount, Opened, OutcomeView, OwnerId, PriceBook, PriceVersionView, Rated,
    RawEvent, ReconcileSettings, Reversal, Timestamp, TopUp, TopUpLedger, TopUpSettled, UsageEvent,
    UtcDay, UtcMonth,
};
use openbilling_http::auth::{BearerAuth, Principal, StaticTokens};
use openbilling_http::{Services, router};
use openbilling_pg::{LedgerSettings, PgLedger, PgStore};
use openbilling_vpay::VpayProvider;
use serde_json::Value;
use sqlx::postgres::{PgConnectOptions, PgPoolOptions};
use sqlx::{AssertSqlSafe, Connection, Executor, PgConnection};
use tower::ServiceExt;

pub const OPERATOR: &str = "token-operator";
pub const PRODUCER: &str = "token-producer";
pub const OWNER_1: &str = "token-owner-1";
pub const OWNER_2: &str = "token-owner-2";

pub fn tokens() -> BearerAuth<StaticTokens> {
    let principal = |sub: &str, roles: &[&str]| Principal {
        sub: sub.to_owned(),
        roles: roles.iter().map(|r| (*r).to_owned()).collect(),
    };
    BearerAuth::new(StaticTokens::new([
        (OPERATOR.to_owned(), principal("ops", &["operator"])),
        (
            PRODUCER.to_owned(),
            principal("svc-producer", &["producer"]),
        ),
        (OWNER_1.to_owned(), principal("o1", &[])),
        (OWNER_2.to_owned(), principal("o2", &[])),
    ]))
}

/// A pool to a server nobody listens on: building the router needs one, using it fails fast.
pub fn unreachable_pool() -> sqlx::PgPool {
    PgPoolOptions::new()
        .acquire_timeout(Duration::from_secs(2))
        .connect_lazy("postgres://nobody@127.0.0.1:1/none")
        .unwrap()
}

/// A ledger whose database is down: every call is `unavailable`.
#[derive(Clone, Copy, Debug)]
pub struct DownLedger;

fn down<T>() -> Result<T, LedgerError> {
    Err(LedgerError::Store {
        class: ErrorClass::Unavailable,
        source: "the database is down".into(),
    })
}

impl LedgerStore for DownLedger {
    async fn now(&self) -> Result<Timestamp, LedgerError> {
        down()
    }
    async fn configure(&self, _: &LedgerConfig) -> Result<(), LedgerError> {
        down()
    }
    async fn open_account(&self, _: &NewAccount) -> Result<Opened, LedgerError> {
        down()
    }
    async fn set_debt_limit(
        &self,
        _: &OwnerId,
        _: &CurrencyCode,
        _: Option<Amount>,
    ) -> Result<(), LedgerError> {
        down()
    }
    async fn set_member_cap(
        &self,
        _: &OwnerId,
        _: &CurrencyCode,
        _: &MemberId,
        _: Option<Amount>,
    ) -> Result<(), LedgerError> {
        down()
    }
    async fn grant_allowance(
        &self,
        _: &AccountId,
        _: UtcMonth,
        _: Amount,
    ) -> Result<Credited, LedgerError> {
        down()
    }
    async fn credit(&self, _: &openbilling_core::Credit) -> Result<Credited, LedgerError> {
        down()
    }
    async fn reverse(&self, _: &Reversal) -> Result<Credited, LedgerError> {
        down()
    }
    async fn intake(&self, _: &[UsageEvent]) -> Result<IntakeReport, LedgerError> {
        down()
    }
    async fn drain(&self, _: u32) -> Result<DrainReport, LedgerError> {
        down()
    }
    async fn balance(
        &self,
        _: &OwnerId,
        _: &CurrencyCode,
        _: Timestamp,
    ) -> Result<Balance, LedgerError> {
        down()
    }
    async fn allowed(
        &self,
        _: &OwnerId,
        _: &CurrencyCode,
        _: &MemberId,
        _: Timestamp,
        _: AllowedPolicy,
    ) -> Result<Allowed, LedgerError> {
        down()
    }
    async fn lots(&self, _: &AccountId) -> Result<Vec<LotView>, LedgerError> {
        down()
    }
    async fn outcome(&self, _: &EventId) -> Result<Option<OutcomeView>, LedgerError> {
        down()
    }
    async fn daily_usage(
        &self,
        _: &OwnerId,
        _: &CurrencyCode,
    ) -> Result<Vec<(UtcDay, Amount)>, LedgerError> {
        down()
    }
}

impl PriceBook for DownLedger {
    async fn load_price_list(&self, _: &str) -> Result<Loaded, LedgerError> {
        down()
    }
    async fn price_versions(&self) -> Result<Vec<PriceVersionView>, LedgerError> {
        down()
    }
    async fn rate(&self, _: &RawEvent) -> Result<Rated, LedgerError> {
        down()
    }
    async fn intake_raw(&self, _: &[RawEvent]) -> Result<IntakeReport, LedgerError> {
        down()
    }
}

impl TopUpLedger for DownLedger {
    async fn record_top_up(&self, _: &TopUp) -> Result<bool, LedgerError> {
        down()
    }
    async fn settle_top_up(
        &self,
        _: &openbilling_core::PaymentView,
    ) -> Result<TopUpSettled, LedgerError> {
        down()
    }
    async fn claim_pending_top_ups(
        &self,
        _: &ReconcileSettings,
    ) -> Result<Vec<openbilling_core::CreditId>, LedgerError> {
        down()
    }
    async fn claim_expiring_top_ups(
        &self,
        _: &ReconcileSettings,
    ) -> Result<Vec<openbilling_core::CreditId>, LedgerError> {
        down()
    }
    async fn expire_top_up(
        &self,
        _: &openbilling_core::CreditId,
        _: &ReconcileSettings,
    ) -> Result<bool, LedgerError> {
        down()
    }
    async fn expire_stale_top_ups(&self, _: &ReconcileSettings) -> Result<u64, LedgerError> {
        down()
    }
}

/// The router over `ledger`, `provider` and `pool`, with the test tokens and the default policy.
pub fn app<L: PriceBook + TopUpLedger + 'static>(
    ledger: L,
    provider: Option<VpayProvider>,
    pool: sqlx::PgPool,
) -> Router {
    app_with(ledger, provider, pool, AllowedPolicy::default())
}

/// The router as [`app`] makes it, with `allowed` as the deployer's policy.
pub fn app_with<L: PriceBook + TopUpLedger + 'static>(
    ledger: L,
    provider: Option<VpayProvider>,
    pool: sqlx::PgPool,
    allowed: AllowedPolicy,
) -> Router {
    router(
        Arc::new(Services {
            ledger,
            provider,
            allowed,
        }),
        pool,
        tokens(),
    )
}

/// An answer: its status, headers and JSON body (`Null` when it is not JSON).
pub struct Answer {
    pub status: StatusCode,
    pub headers: HeaderMap,
    pub body: Value,
    pub text: String,
}

/// Sends one request to `app`.
pub async fn call(
    app: &Router,
    method: Method,
    path: &str,
    token: Option<&str>,
    key: Option<&str>,
    body: Option<Value>,
) -> Answer {
    let mut request = Request::builder().method(method).uri(path);
    if let Some(token) = token {
        request = request.header("authorization", format!("Bearer {token}"));
    }
    if let Some(key) = key {
        request = request.header("idempotency-key", key);
    }
    let body = match body {
        Some(body) => {
            request = request.header("content-type", "application/json");
            Body::from(serde_json::to_vec(&body).unwrap())
        }
        None => Body::empty(),
    };
    raw(app, request.body(body).unwrap()).await
}

/// Sends a request as built.
pub async fn raw(app: &Router, request: Request<Body>) -> Answer {
    let response = app.clone().oneshot(request).await.unwrap();
    let status = response.status();
    let headers = response.headers().clone();
    let bytes = to_bytes(response.into_body(), 1 << 20).await.unwrap();
    let text = String::from_utf8_lossy(&bytes).into_owned();
    Answer {
        status,
        headers,
        body: serde_json::from_slice(&bytes).unwrap_or(Value::Null),
        text,
    }
}

/// `POST /v1/$procs/<name>` with `{"args": args}`.
pub async fn procedure(
    app: &Router,
    name: &str,
    token: Option<&str>,
    key: Option<&str>,
    args: Value,
) -> Answer {
    call(
        app,
        Method::POST,
        &format!("/v1/$procs/{name}"),
        token,
        key,
        Some(serde_json::json!({ "args": args })),
    )
    .await
}

static NEXT: AtomicU32 = AtomicU32::new(0);

/// A fresh, migrated database and its ledger, or `None` to skip: as `openbilling-pg`'s own tests make one.
pub async fn fresh() -> Option<(PgStore, PgLedger)> {
    let url = match std::env::var("OPENBILLING_TEST_DATABASE_URL") {
        Ok(url) if !url.trim().is_empty() => url,
        _ => {
            let required = std::env::var("OPENBILLING_TEST_REQUIRE_DB").is_ok_and(|v| v == "1");
            assert!(
                !required,
                "OPENBILLING_TEST_REQUIRE_DB=1 but OPENBILLING_TEST_DATABASE_URL is unset"
            );
            eprintln!("skipped: OPENBILLING_TEST_DATABASE_URL is unset");
            return None;
        }
    };
    let admin: PgConnectOptions = url.parse().unwrap();
    let nanos = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap()
        .subsec_nanos();
    let name = format!(
        "ob_test_{}_{}_{}",
        std::process::id(),
        nanos,
        NEXT.fetch_add(1, Ordering::Relaxed)
    );
    let mut conn = PgConnection::connect_with(&admin).await.unwrap();
    conn.execute(AssertSqlSafe(format!("CREATE DATABASE {name}")))
        .await
        .unwrap();
    conn.close().await.unwrap();
    let store = PgStore::connect_with(admin.database(&name), 8)
        .await
        .unwrap();
    store.migrate().await.unwrap();
    openbilling_http::prepare(store.pool()).await.unwrap();
    let ledger = PgLedger::new(&store, LedgerSettings::default());
    ledger.prepare_partitions().await.unwrap();
    Some((store, ledger))
}
