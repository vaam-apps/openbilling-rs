//! A fresh database per test, migrated, on the server `OPENBILLING_TEST_DATABASE_URL` names.
//!
//! Skips (returns `None`) when the variable is unset, and fails instead under `OPENBILLING_TEST_REQUIRE_DB=1` (CI sets
//! it), so a missing database can never pass CI by skipping (agent guide, rule 10). The databases are left behind:
//! CI's server is thrown away with its job.

#![allow(dead_code, clippy::unwrap_used, clippy::expect_used)]

use std::sync::atomic::{AtomicU32, Ordering};
use std::time::{SystemTime, UNIX_EPOCH};

use openbilling_pg::{LedgerSettings, PgLedger, PgStore};
use sqlx::postgres::PgConnectOptions;
use sqlx::{AssertSqlSafe, Connection, Executor, PgConnection};

const URL_VAR: &str = "OPENBILLING_TEST_DATABASE_URL";
const REQUIRE_VAR: &str = "OPENBILLING_TEST_REQUIRE_DB";

/// Connections per test's pool: tests run in parallel, and the server's default limit is 100.
const POOL: u32 = 12;

static NEXT: AtomicU32 = AtomicU32::new(0);

/// The server's URL, or `None` when the test should skip. Panics when skipping is not allowed.
pub fn database_url() -> Option<String> {
    match std::env::var(URL_VAR) {
        Ok(url) if !url.trim().is_empty() => Some(url),
        _ => {
            let required = std::env::var(REQUIRE_VAR).is_ok_and(|v| v == "1");
            assert!(!required, "{REQUIRE_VAR}=1 but {URL_VAR} is unset or empty");
            eprintln!("skipped: {URL_VAR} is unset");
            None
        }
    }
}

/// A new, empty database, migrated.
pub async fn fresh_store() -> Option<PgStore> {
    let url = database_url()?;
    let admin: PgConnectOptions = url.parse().expect("a PostgreSQL URL");
    let nanos = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .expect("after the epoch")
        .subsec_nanos();
    let name = format!(
        "ob_test_{}_{}_{}",
        std::process::id(),
        nanos,
        NEXT.fetch_add(1, Ordering::Relaxed)
    );
    let mut conn = PgConnection::connect_with(&admin).await.expect("connect");
    conn.execute(AssertSqlSafe(format!("CREATE DATABASE {name}")))
        .await
        .expect("create a database");
    conn.close().await.expect("close");
    let store = PgStore::connect_with(admin.database(&name), POOL)
        .await
        .expect("connect to the new database");
    store.migrate().await.expect("migrate");
    Some(store)
}

/// A fresh ledger with the default settings and its partitions made.
pub async fn fresh_ledger() -> Option<PgLedger> {
    let store = fresh_store().await?;
    let ledger = PgLedger::new(&store, LedgerSettings::default());
    ledger.prepare_partitions().await.expect("partitions");
    Some(ledger)
}
