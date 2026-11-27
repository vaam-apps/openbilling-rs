//! Conformance tests for the storage port of openbilling-rs.
//!
//! The agent guide (rule 4) makes storage a trait with a conformance testkit: one set of tests, written once, that
//! every implementation must pass. This crate will hold them as macros, so an implementation's own test file runs
//! the whole suite against its store with one line.
//!
//! It holds nothing yet, because the trait does not exist yet. When it does, the suite starts with what
//! [ADR 0001](https://github.com/vaam-apps/openbilling-rs/blob/main/docs/adr/0001-postgres-is-the-queue.md) promises:
//! an event accepted once by its id, a replay answered as the first acceptance, the same id with another payload
//! answered as a conflict that echoes nothing, an event outside the window refused, two workers never taking the
//! same event, and a ledger transaction that does not balance never committing.
