//! The end-to-end suite: the real `openbilling serve`, started as a process against a fresh PostgreSQL database,
//! driven only through its public HTTP API (`/v1`) and its payment webhook, with bearer tokens a test issuer signs
//! and the service verifies against the issuer's JWKS.
//!
//! Each scenario is one test with a plain-language name, a deployment of its own (a database, an issuer, one or more
//! services) and an example metered service as the producer. Reads of the ledger's tables, read-only, check
//! invariants no API answers (the ledger sums to zero, one transaction per event); nothing is written but through
//! the API.
//!
//! Needs `OPENBILLING_TEST_DATABASE_URL` (a PostgreSQL 19 server whose user may create databases); skips without it,
//! and fails instead under `OPENBILLING_TEST_REQUIRE_DB=1` (agent guide, rule 10). `e2e/README.md` says how to run
//! it.

#![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]

mod support;

mod authentication;
mod concurrency;
mod metered_usage;
mod observability_stack;
mod once_only;
mod organisation;
mod payments;
mod prepaid_balance;
mod price_versions;
mod sdk;
mod statements;
mod telemetry;
