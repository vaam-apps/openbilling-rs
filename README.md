# openbilling-rs

Open-source usage billing in Rust, on PostgreSQL alone.

openbilling-rs is an open alternative to hosted metering-and-credits services. It takes usage events from your
services, rates them against a price book you define, and records the result in a double-entry ledger. PostgreSQL is
the queue, the once-only record and the ledger, so a write is one insert and every step is a transaction. There is no
broker to run.

**Status: design and skeleton.** The design is [ADR 0001](docs/adr/0001-postgres-is-the-queue.md). The workspace
builds and is tested; it holds the core types and a store that connects. Nothing bills yet.

## What it will do

| Part | What |
|---|---|
| Intake | Accept a usage event once by its id; a replay is a no-op |
| Rating | Turn events into amounts with a versioned price book |
| Ledger | Debit and credit accounts in one transaction, with balances and limits |
| Reads | Balances, statements and usage series for an account |

## Build and test

You need Rust 1.94 or later and, for the database tests, a PostgreSQL. 1.94 is the MSRV because sqlx 0.9 declares
`rust-version = "1.94.0"` (edition 2024 alone would allow 1.85); CI's MSRV job holds it.

```sh
cargo build --workspace
cargo test --workspace     # the database test skips: OPENBILLING_TEST_DATABASE_URL is unset

docker run --rm -d --name openbilling-pg -p 127.0.0.1:5432:5432 -e POSTGRES_HOST_AUTH_METHOD=trust postgres:19.0
OPENBILLING_TEST_DATABASE_URL=postgres://postgres@127.0.0.1:5432/postgres cargo test --workspace

cargo run -p openbilling -- --help
```

`OPENBILLING_TEST_REQUIRE_DB=1` turns the skip into a failure; CI sets it. `docker stop openbilling-pg` removes the
database.

## Layout

| Path | What |
|---|---|
| `crates/openbilling-core` | Pure types and rules: ids, amounts, event outcomes, errors with a class. No I/O |
| `crates/openbilling-pg` | The PostgreSQL store on sqlx. Connects and pings for now |
| `crates/openbilling-testkit` | The storage conformance suite, as macros. Empty until the storage trait exists |
| `bin/openbilling` | The service binary. Answers `--help` and `--version` for now |
| `docs/adr/` | The decisions |

Each crate has a `README.md` that says what it holds and how it is tested.

## Contributing and security

See [CONTRIBUTING.md](CONTRIBUTING.md). Report a vulnerability privately as [SECURITY.md](SECURITY.md) says, never
in a public issue.

## Licence

Apache-2.0. See [LICENSE](LICENSE) and [NOTICE](NOTICE).
