//! The harness: a fresh PostgreSQL database, the real `openbilling serve` started as a process against it, a test
//! issuer whose tokens the service verifies, and an HTTP client that speaks only the service's public API and its
//! webhook route.
//!
//! The service is killed when its [`Service`] is dropped: a scenario never waits for a clean stop.

use std::fs::File;
use std::path::{Path, PathBuf};
use std::process::{Child, Command, Stdio};
use std::sync::atomic::{AtomicU32, Ordering};
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use openbilling_core::Timestamp;
use openbilling_jwt::test_issuer::{AUDIENCE, TestIssuer};
use serde_json::{Value, json};
use sqlx::postgres::PgConnectOptions;
use sqlx::{AssertSqlSafe, Connection, Executor, PgConnection, PgPool};
use tokio::sync::{Semaphore, SemaphorePermit};

/// The currency every scenario uses.
pub const EUR: &str = "EUR";

/// How many services may run at once across the suite: each holds up to 11 connections, and a default PostgreSQL
/// allows 100. The concurrency scenario takes three.
static SLOTS: Semaphore = Semaphore::const_new(4);

static NEXT: AtomicU32 = AtomicU32::new(0);

/// The database server, or `None` to skip: agent guide, rule 10.
pub fn database_url() -> Option<String> {
    match std::env::var("OPENBILLING_TEST_DATABASE_URL") {
        Ok(url) if !url.trim().is_empty() => Some(url),
        _ => {
            let required = std::env::var("OPENBILLING_TEST_REQUIRE_DB").is_ok_and(|v| v == "1");
            assert!(
                !required,
                "OPENBILLING_TEST_REQUIRE_DB=1 but OPENBILLING_TEST_DATABASE_URL is unset"
            );
            eprintln!("skipped: OPENBILLING_TEST_DATABASE_URL is unset");
            None
        }
    }
}

fn unique(prefix: &str) -> String {
    let nanos = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap()
        .subsec_nanos();
    format!(
        "{prefix}_{}_{}_{}",
        std::process::id(),
        nanos,
        NEXT.fetch_add(1, Ordering::Relaxed)
    )
}

/// A new, empty database on the server `admin_url` names; its URL.
pub async fn fresh_database(admin_url: &str) -> String {
    let admin: PgConnectOptions = admin_url.parse().unwrap();
    let name = unique("ob_e2e");
    let mut conn = PgConnection::connect_with(&admin).await.unwrap();
    conn.execute(AssertSqlSafe(format!("CREATE DATABASE {name}")))
        .await
        .unwrap();
    conn.close().await.unwrap();
    let mut url = url::Url::parse(admin_url).unwrap();
    url.set_path(&format!("/{name}"));
    url.to_string()
}

/// A pool for reading the ledger's invariants straight from its tables, read-only, after the API has done its work.
pub async fn read_only_pool(url: &str) -> PgPool {
    let options: PgConnectOptions = url.parse().unwrap();
    let options = options.options([("default_transaction_read_only", "on")]);
    sqlx::postgres::PgPoolOptions::new()
        .max_connections(2)
        .connect_with(options)
        .await
        .unwrap()
}

/// A directory of its own under the system's temporary directory, removed on drop.
pub struct TempDir(PathBuf);

impl TempDir {
    pub fn new() -> Self {
        let path = std::env::temp_dir().join(unique("ob-e2e"));
        std::fs::create_dir_all(&path).unwrap();
        Self(path)
    }

    pub fn path(&self) -> &Path {
        &self.0
    }
}

impl Drop for TempDir {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.0);
    }
}

/// Environment for `serve`, besides the database, the address and the issuer.
pub type Extra = Vec<(String, String)>;

/// A running `openbilling serve`.
pub struct Service {
    child: Child,
    pub base: String,
    log: PathBuf,
    _dir: TempDir,
}

impl Service {
    /// Starts `openbilling serve` on a free loopback port against `database_url`, trusting `issuer`, and waits until
    /// `/readyz` answers `200`. Panics with the service's log if it does not.
    pub async fn start(database_url: &str, issuer: &TestIssuer, extra: &Extra) -> Self {
        let trust = vec![
            ("OPENBILLING_AUTH_ISSUER".to_owned(), issuer.issuer.clone()),
            ("OPENBILLING_AUTH_AUDIENCE".to_owned(), AUDIENCE.to_owned()),
        ];
        Self::spawn(database_url, &[trust, extra.clone()].concat()).await
    }

    /// Starts `openbilling serve` with no issuer configured.
    pub async fn start_without_issuer(database_url: &str) -> Self {
        Self::spawn(database_url, &Vec::new()).await
    }

    async fn spawn(database_url: &str, extra: &Extra) -> Self {
        let dir = TempDir::new();
        let log = dir.path().join("serve.log");
        let port = {
            let probe = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
            probe.local_addr().unwrap().port()
        };
        let mut command = Command::new(env!("CARGO_BIN_EXE_openbilling"));
        command
            .arg("serve")
            .env_clear()
            .env("NO_COLOR", "1")
            .env("OPENBILLING_DATABASE_URL", database_url)
            .env("OPENBILLING_LISTEN", format!("127.0.0.1:{port}"))
            .stdin(Stdio::null())
            .stdout(Stdio::from(File::create(&log).unwrap()))
            .stderr(Stdio::from(
                File::options().append(true).open(&log).unwrap(),
            ));
        for (name, value) in extra {
            command.env(name, value);
        }
        let mut child = command.spawn().unwrap();
        let base = format!("http://127.0.0.1:{port}");
        let http = client();
        let deadline = Instant::now() + Duration::from_secs(60);
        loop {
            if let Some(status) = child.try_wait().unwrap() {
                panic!(
                    "openbilling serve exited with {status}:\n{}",
                    std::fs::read_to_string(&log).unwrap_or_default()
                );
            }
            let ready = http
                .get(format!("{base}/readyz"))
                .send()
                .await
                .is_ok_and(|r| r.status() == 200);
            if ready {
                break;
            }
            if Instant::now() > deadline {
                let _ = child.kill();
                panic!(
                    "openbilling serve was not ready in 60 s:\n{}",
                    std::fs::read_to_string(&log).unwrap_or_default()
                );
            }
            tokio::time::sleep(Duration::from_millis(100)).await;
        }
        Self {
            child,
            base,
            log,
            _dir: dir,
        }
    }

    /// What the service printed so far.
    pub fn log(&self) -> String {
        std::fs::read_to_string(&self.log).unwrap_or_default()
    }
}

impl Drop for Service {
    fn drop(&mut self) {
        // Killed, not stopped: a clean stop on a signal is not what these scenarios test.
        let _ = self.child.kill();
        let _ = self.child.wait();
    }
}

fn client() -> reqwest::Client {
    // reqwest is built without a crypto provider of its own; rustls uses ring.
    let _ = rustls::crypto::ring::default_provider().install_default();
    reqwest::Client::builder()
        .timeout(Duration::from_secs(60))
        .build()
        .unwrap()
}

/// An answer: its status, its JSON body (`Null` when it is not JSON), its text and headers.
#[derive(Debug)]
pub struct Answer {
    pub status: u16,
    pub body: Value,
    pub text: String,
    pub headers: reqwest::header::HeaderMap,
}

impl Answer {
    /// Panics with the body unless the status is a success.
    #[track_caller]
    pub fn ok(self) -> Self {
        assert!(
            (200..300).contains(&self.status),
            "{}: {}",
            self.status,
            self.text
        );
        self
    }
}

/// Who calls.
#[derive(Clone, Copy, Debug)]
pub enum Who<'a> {
    Operator,
    Producer,
    Owner(&'a str),
    /// A token minted beforehand, sent as it is: a caller that retries with the token it already holds.
    Token(&'a str),
    Nobody,
}

/// One deployment: its issuer, its database and its services. Holds its slots of [`SLOTS`] while it lives.
pub struct Deployment {
    pub issuer: TestIssuer,
    pub database_url: String,
    pub services: Vec<Service>,
    http: reqwest::Client,
    next_key: AtomicU32,
    _slots: SemaphorePermit<'static>,
}

impl Deployment {
    /// A fresh database with `instances` services against it (started one after the other), or `None` to skip.
    pub async fn start(instances: u32, extra: &Extra) -> Option<Self> {
        let admin = database_url()?;
        let slots = SLOTS.acquire_many(instances).await.unwrap();
        let issuer = TestIssuer::start().await;
        let database_url = fresh_database(&admin).await;
        let mut services = Vec::new();
        for _ in 0..instances {
            services.push(Service::start(&database_url, &issuer, extra).await);
        }
        Some(Self {
            issuer,
            database_url,
            services,
            http: client(),
            next_key: AtomicU32::new(0),
            _slots: slots,
        })
    }

    fn token(&self, who: Who<'_>) -> Option<String> {
        match who {
            Who::Operator => Some(self.issuer.token("ops-console", &["operator"])),
            Who::Producer => Some(self.issuer.token("metering-svc", &["producer"])),
            Who::Owner(sub) => Some(self.issuer.token(sub, &[])),
            Who::Token(token) => Some(token.to_owned()),
            Who::Nobody => None,
        }
    }

    /// A token of the producer, to send more than once.
    pub fn producer_token(&self) -> String {
        self.issuer.token("metering-svc", &["producer"])
    }

    /// A fresh `Idempotency-Key`.
    pub fn key(&self) -> String {
        format!("k-{}", self.next_key.fetch_add(1, Ordering::Relaxed))
    }

    async fn send(&self, request: reqwest::RequestBuilder, who: Who<'_>) -> Answer {
        let request = match self.token(who) {
            Some(token) => request.bearer_auth(token),
            None => request,
        };
        send(request).await
    }

    fn procedure(
        &self,
        at: usize,
        name: &str,
        who: Who<'_>,
        key: Option<&str>,
        args: &Value,
    ) -> reqwest::RequestBuilder {
        let mut request = self
            .http
            .post(format!("{}/v1/$procs/{name}", self.services[at].base))
            .header("content-type", "application/json")
            .body(serde_json::to_vec(&json!({ "args": args })).unwrap());
        if let Some(key) = key {
            request = request.header("idempotency-key", key);
        }
        match self.token(who) {
            Some(token) => request.bearer_auth(token),
            None => request,
        }
    }

    /// `POST /v1/$procs/<name>` on service `at`, with `{"args": args}` and, for a write, `key`.
    pub async fn call_at(
        &self,
        at: usize,
        name: &str,
        who: Who<'_>,
        key: Option<&str>,
        args: Value,
    ) -> Answer {
        send(self.procedure(at, name, who, key, &args)).await
    }

    /// [`Deployment::call_at`] as a task of its own, so many calls run at once.
    pub fn spawn_call(
        &self,
        at: usize,
        name: &str,
        who: Who<'_>,
        key: Option<&str>,
        args: &Value,
    ) -> tokio::task::JoinHandle<Answer> {
        tokio::spawn(send(self.procedure(at, name, who, key, args)))
    }

    /// A read procedure on the first service.
    pub async fn read(&self, name: &str, who: Who<'_>, args: Value) -> Answer {
        self.call_at(0, name, who, None, args).await
    }

    /// A write procedure on the first service, under a fresh key.
    pub async fn write(&self, name: &str, who: Who<'_>, args: Value) -> Answer {
        let key = self.key();
        self.call_at(0, name, who, Some(&key), args).await
    }

    /// `GET <path>` on the first service.
    pub async fn get(&self, path: &str, who: Who<'_>) -> Answer {
        let request = self.http.get(format!("{}{path}", self.services[0].base));
        self.send(request, who).await
    }

    /// `POST /webhooks/payments` with `body` and its `Vpay-Signature`, as the provider sends it.
    pub async fn webhook(&self, body: Vec<u8>, signature: Option<&str>) -> Answer {
        let mut request = self
            .http
            .post(format!("{}/webhooks/payments", self.services[0].base))
            .header("content-type", "application/json")
            .body(body);
        if let Some(signature) = signature {
            request = request.header("vpay-signature", signature);
        }
        self.send(request, Who::Nobody).await
    }

    /// The configuration every scenario starts from: EUR; labels `promo` then `main`; the example lot order.
    pub async fn configure(&self) {
        self.write(
            "configure",
            Who::Operator,
            json!({
                "currencies": [EUR],
                "labels": ["promo", "main"],
                "lotOrder": [
                    {"kind": "allowance", "soonestExpiryFirst": true},
                    {"kind": "promotion", "soonestExpiryFirst": true},
                    {"kind": "adjustment", "soonestExpiryFirst": false},
                    {"kind": "payment", "soonestExpiryFirst": false}
                ]
            }),
        )
        .await
        .ok();
    }

    /// Opens the `main` account `id` of `owner` in EUR.
    pub async fn open(&self, id: &str, owner: &str) {
        let answer = self
            .write(
                "openAccount",
                Who::Operator,
                json!({"accountId": id, "owner": owner, "currency": EUR, "label": "main"}),
            )
            .await
            .ok();
        assert_eq!(answer.body["replayed"], false);
    }

    /// Books an operator's credit (`promotion` or `adjustment`) on `owner`'s `main` account.
    pub async fn grant(&self, source: &str, id: &str, owner: &str, micros: i64) -> Answer {
        self.write(
            "grantCredit",
            Who::Operator,
            json!({"creditId": id, "source": source, "owner": owner, "currency": EUR, "label": "main",
                   "amountMicros": micros.to_string()}),
        )
        .await
        .ok()
    }

    /// Sends pre-priced usage as the producer.
    pub async fn usage(&self, events: Vec<Value>) -> Answer {
        self.write("recordUsage", Who::Producer, json!({ "events": events }))
            .await
    }

    /// Sends raw usage as the producer.
    pub async fn raw_usage(&self, events: Vec<Value>) -> Answer {
        self.write("recordRawUsage", Who::Producer, json!({ "events": events }))
            .await
    }

    /// How many outcomes the drains have decided, as the operator reads them.
    pub async fn decided(&self) -> u64 {
        let answer = self
            .get("/v1/usage_outcomes?limit=1", Who::Operator)
            .await
            .ok();
        answer.body["totalCount"].as_u64().unwrap()
    }

    /// Waits until the drains (the services' own loops) have decided `n` outcomes.
    pub async fn wait_decided(&self, n: u64, within: Duration) {
        let deadline = Instant::now() + within;
        loop {
            let decided = self.decided().await;
            if decided >= n {
                assert_eq!(decided, n, "more outcomes than events");
                return;
            }
            assert!(
                Instant::now() < deadline,
                "{decided} of {n} outcomes after {within:?}:\n{}",
                self.services[0].log()
            );
            tokio::time::sleep(Duration::from_millis(200)).await;
        }
    }

    /// The owner's balance rows in EUR.
    pub async fn balance(&self, owner: &str) -> Value {
        self.read(
            "getBalance",
            Who::Owner(owner),
            json!({"owner": owner, "currency": EUR}),
        )
        .await
        .ok()
        .body
    }

    /// The owner's net in EUR: what its lots hold less its debt.
    pub async fn net(&self, owner: &str) -> i64 {
        let rows = self.balance(owner).await;
        micros(&rows[0]["netMicros"])
    }

    /// `getAllowed` as the producer.
    pub async fn allowed(&self, owner: &str, member: &str) -> Value {
        self.read(
            "getAllowed",
            Who::Producer,
            json!({"owner": owner, "currency": EUR, "member": member}),
        )
        .await
        .ok()
        .body
    }

    /// Every outcome of `owner`, as the owner reads them, sorted by event id.
    pub async fn outcomes(&self, owner: &str) -> Vec<Value> {
        let answer = self
            .get(
                &format!("/v1/usage_outcomes?owner={owner}&sort=eventId&limit=500"),
                Who::Owner(owner),
            )
            .await
            .ok();
        let items = answer.body["items"].as_array().unwrap().clone();
        assert_eq!(
            answer.body["totalCount"].as_u64().unwrap(),
            items.len() as u64,
            "one page holds them all"
        );
        items
    }

    /// The owner's statement lines for a UTC month.
    pub async fn statement(&self, owner: &str, year: i32, month: u8) -> Vec<Value> {
        self.read(
            "getStatement",
            Who::Owner(owner),
            json!({"owner": owner, "currency": EUR, "year": year, "month": month}),
        )
        .await
        .ok()
        .body
        .as_array()
        .unwrap()
        .clone()
    }
}

async fn send(request: reqwest::RequestBuilder) -> Answer {
    let response = request.send().await.unwrap();
    let status = response.status().as_u16();
    let headers = response.headers().clone();
    let bytes = response.bytes().await.unwrap();
    Answer {
        status,
        body: serde_json::from_slice(&bytes).unwrap_or(Value::Null),
        text: String::from_utf8_lossy(&bytes).into_owned(),
        headers,
    }
}

/// A `BigInt` on the wire (a decimal string) as an `i64`.
#[track_caller]
pub fn micros(value: &Value) -> i64 {
    value
        .as_str()
        .unwrap_or_else(|| panic!("a BigInt string, not {value}"))
        .parse()
        .unwrap()
}

/// Now, by this machine's clock, which is the database's in these runs (both on one host).
pub fn now() -> Timestamp {
    let micros = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap()
        .as_micros();
    Timestamp::from_unix_micros(i64::try_from(micros).unwrap()).unwrap()
}

/// `at` moved by `micros`.
pub fn plus(at: Timestamp, micros: i64) -> Timestamp {
    at.checked_add_micros(micros).unwrap()
}

pub const SECOND: i64 = 1_000_000;
pub const DAY: i64 = 86_400 * SECOND;

/// RFC 3339 text of `at`, as the API takes it.
pub fn text(at: Timestamp) -> String {
    at.to_rfc3339_micros()
}

/// A pre-priced event body.
pub fn priced(id: &str, owner: &str, member: &str, micros: i64, at: Timestamp) -> Value {
    json!({"eventId": id, "owner": owner, "member": member, "currency": EUR,
           "amountMicros": micros.to_string(), "occurredAt": text(at)})
}

/// A raw event body.
pub fn raw(
    id: &str,
    owner: &str,
    member: &str,
    meter: &str,
    quantity: i64,
    dimensions: &[(&str, &str)],
    at: Timestamp,
) -> Value {
    let dimensions: Vec<Value> = dimensions
        .iter()
        .map(|(k, v)| json!({"key": k, "value": v}))
        .collect();
    json!({"eventId": id, "owner": owner, "member": member, "currency": EUR, "meter": meter,
           "quantity": quantity.to_string(), "dimensions": dimensions, "occurredAt": text(at)})
}

/// `quantity × price ÷ per`, rounded once to the micro-unit, half to even: worked out here, apart from the SQL that
/// the service rates with, so the two can disagree.
pub fn rated(quantity: i64, price: i64, per: i64) -> i64 {
    let product = i128::from(quantity) * i128::from(price);
    let per = i128::from(per);
    let (whole, rest) = (product / per, product % per);
    let up = match (2 * rest).cmp(&per) {
        std::cmp::Ordering::Greater => true,
        std::cmp::Ordering::Less => false,
        std::cmp::Ordering::Equal => whole % 2 != 0,
    };
    i64::try_from(whole + i128::from(up)).unwrap()
}

#[test]
fn the_reference_rounding_is_half_to_even() {
    assert_eq!(rated(5, 1, 2), 2, "2.5 goes to 2");
    assert_eq!(rated(7, 1, 2), 4, "3.5 goes to 4");
    assert_eq!(rated(1234, 250, 1000), 308, "308.5 goes to 308");
    assert_eq!(rated(10, 25, 1000), 0);
    assert_eq!(rated(3, 2, 3), 2, "2 exactly");
    assert_eq!(rated(11, 1, 4), 3, "2.75 goes up");
}
