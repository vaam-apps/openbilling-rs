//! A fresh, migrated ledger on the server `OPENBILLING_TEST_DATABASE_URL` names, made as `openbilling-pg`'s own
//! tests make one. Skips (returns `None`) when the variable is unset, and fails instead under
//! `OPENBILLING_TEST_REQUIRE_DB=1` (agent guide, rule 10). The databases are left behind: CI's server is thrown away.

#![allow(dead_code, clippy::unwrap_used, clippy::expect_used)]

use std::sync::atomic::{AtomicU32, Ordering};
use std::time::{SystemTime, UNIX_EPOCH};

use openbilling_pg::{LedgerSettings, PgLedger, PgStore};
use sqlx::postgres::PgConnectOptions;
use sqlx::{AssertSqlSafe, Connection, Executor, PgConnection};

static NEXT: AtomicU32 = AtomicU32::new(0);

pub async fn fresh_ledger() -> Option<PgLedger> {
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
    let admin: PgConnectOptions = url.parse().expect("a PostgreSQL URL");
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
    let mut conn = PgConnection::connect_with(&admin).await.expect("connect");
    conn.execute(AssertSqlSafe(format!("CREATE DATABASE {name}")))
        .await
        .expect("create a database");
    conn.close().await.expect("close");
    let store = PgStore::connect_with(admin.database(&name), 8)
        .await
        .expect("connect to the new database");
    store.migrate().await.expect("migrate");
    let ledger = PgLedger::new(&store, LedgerSettings::default());
    ledger.prepare_partitions().await.expect("partitions");
    Some(ledger)
}
