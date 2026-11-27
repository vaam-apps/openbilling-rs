# openbilling-testkit

The conformance tests of the storage port, as macros. An implementation of the port (the PostgreSQL store first)
will run the whole suite from its own test file with one line, so every implementation is held to the same rules.

**Now:** empty. The storage trait does not exist yet, and a suite with nothing to test would only look finished.
`src/lib.rs` says what the suite will check first: the promises of
[ADR 0001](../../docs/adr/0001-postgres-is-the-queue.md) (once-only intake, replays, conflicts that echo nothing,
the window, the drain, a ledger that balances).

## Tests

None yet.
