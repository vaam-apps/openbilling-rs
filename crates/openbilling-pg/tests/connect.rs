//! The store against a real PostgreSQL.
//!
//! Runs when `OPENBILLING_TEST_DATABASE_URL` is set, skips when it is unset, and fails instead of skipping under
//! `OPENBILLING_TEST_REQUIRE_DB=1` (CI sets it), so a missing database can never pass CI by skipping.

// Tests may unwrap and expect. clippy.toml allows it in `#[test]` bodies; this covers the helpers too.
#![allow(clippy::unwrap_used, clippy::expect_used)]

use openbilling_pg::PgStore;

const URL_VAR: &str = "OPENBILLING_TEST_DATABASE_URL";
const REQUIRE_VAR: &str = "OPENBILLING_TEST_REQUIRE_DB";

/// The database URL, or `None` when the test should skip. Panics when skipping is not allowed.
fn database_url() -> Option<String> {
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

#[tokio::test]
async fn connects_pings_and_closes() {
    let Some(url) = database_url() else { return };
    let store = PgStore::connect(&url).await.expect("connect");
    store.ping().await.expect("ping");
    store.close().await;
}
