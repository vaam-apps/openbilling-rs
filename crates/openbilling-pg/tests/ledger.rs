//! The conformance suite of `openbilling-testkit` against the PostgreSQL store: every case on a fresh database.
//!
//! Needs `OPENBILLING_TEST_DATABASE_URL` (a PostgreSQL 19 server whose user may create databases); skips without it,
//! and fails instead under `OPENBILLING_TEST_REQUIRE_DB=1`.

mod support;

openbilling_testkit::ledger_conformance!(support::fresh_ledger);
