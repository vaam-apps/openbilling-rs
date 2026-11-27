# openbilling-rs

Open-source usage billing in Rust, on PostgreSQL alone.

openbilling-rs is an open alternative to hosted metering-and-credits services. It takes usage events from your
services, rates them against a price book you define, and records the result in a double-entry ledger. PostgreSQL is
the queue, the once-only record and the ledger, so a write is one insert and every step is a transaction. There is no
broker to run.

**Status: the ledger, on pre-priced events.** The design is [ADR 0001](docs/adr/0001-postgres-is-the-queue.md) and
[ADR 0002](docs/adr/0002-an-owner-holds-several-accounts.md). The PostgreSQL store takes priced usage events into a
partitioned queue once, drains them into a double-entry ledger across an owner's accounts, and books payments,
promotions, adjustments, refunds and chargebacks. Every rule is SQL; Rust holds the types, the port and the tests.
Rating against a price book is next. The database tests have not yet run against a PostgreSQL 19 outside CI.

## What it will do

| Part | What |
|---|---|
| Intake | Accept a usage event once by its id; a replay is a no-op |
| Rating | Turn events into amounts with a versioned price book (not built yet: events carry their amount) |
| Ledger | Debit an owner's accounts across currencies and labels in one transaction, with lots, debt and limits |
| Reads | Balances, `allowed` with member caps, lots, outcomes and usage per day |

## Build and test

You need Rust 1.94 or later and, for the database tests, PostgreSQL 19 or later (the minimum; older versions are
not supported). 1.94 is the MSRV because sqlx 0.9 declares
`rust-version = "1.94.0"` (edition 2024 alone would allow 1.85); CI's MSRV job holds it.

```sh
cargo build --workspace
cargo test --workspace     # the database tests skip: OPENBILLING_TEST_DATABASE_URL is unset

docker run --rm -d --name openbilling-pg -p 127.0.0.1:5432:5432 -e POSTGRES_HOST_AUTH_METHOD=trust postgres:19.0
OPENBILLING_TEST_DATABASE_URL=postgres://postgres@127.0.0.1:5432/postgres cargo test --workspace  # a fresh database per test

cargo run -p openbilling -- --help
```

`OPENBILLING_TEST_REQUIRE_DB=1` turns the skip into a failure; CI sets it. `docker stop openbilling-pg` removes the
database.

## Layout

| Path | What |
|---|---|
| `crates/openbilling-core` | The types and the storage port, `LedgerStore`: ids, micro-unit amounts, time, credits, events, refusals with a class. No I/O, no rule |
| `crates/openbilling-pg` | The PostgreSQL store: the migrations, which hold every rule, and `PgLedger`, which calls them. Needs PostgreSQL 19 or later |
| `crates/openbilling-testkit` | The conformance suite of `LedgerStore`, one macro, and its fixtures |
| `bin/openbilling` | The service binary. Answers `--help` and `--version` for now |
| `docs/adr/` | The decisions |

Each crate has a `README.md` that says what it holds and how it is tested.

## Contributing and security

See [CONTRIBUTING.md](CONTRIBUTING.md). Report a vulnerability privately as [SECURITY.md](SECURITY.md) says, never
in a public issue.

## Licence

Apache-2.0. See [LICENSE](LICENSE) and [NOTICE](NOTICE).
