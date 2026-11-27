# openbilling-rs

Open-source usage billing in Rust, on PostgreSQL alone.

openbilling-rs is an open alternative to hosted metering-and-credits services. It takes usage events from your
services, rates them against a price book you define, and records the result in a double-entry ledger. PostgreSQL is
the queue, the once-only record and the ledger, so a write is one insert and every step is a transaction. There is no
broker to run.

**Status: the ledger and the price book.** The design is [ADR 0001](docs/adr/0001-postgres-is-the-queue.md),
[ADR 0002](docs/adr/0002-an-owner-holds-several-accounts.md) and
[ADR 0004](docs/adr/0004-a-versioned-price-book-in-sql.md). The PostgreSQL store loads a versioned price book from
YAML, rates raw usage events at intake by the version in force when they happened, takes them (or pre-priced ones)
into a partitioned queue once, drains them into a double-entry ledger across an owner's accounts, and books payments,
promotions, adjustments, refunds and chargebacks. Every rule is SQL; Rust holds the types, the ports and the tests.
Credits from a payment provider (vpay first) and an external API in CrateStack are built
([ADR 0003](docs/adr/0003-credits-from-a-payment-provider.md)), with callers refused until a real token verifier is
wired. The database tests have not yet run against a PostgreSQL 19 outside CI.

## Features

Legend: ✅ built and tested in CI · ⚠️ built, but partial or not yet proven end to end · 🚧 being built ·
📋 planned · ⏸️ on hold. Updated with every change that moves a row.

| Area | Feature | Status | Notes |
|---|---|---|---|
| Storage | PostgreSQL 19 or later, the only infrastructure | ✅ | Older servers are refused at connect |
| Intake | Usage events accepted once by id; a replay changes nothing | ✅ | One `ON CONFLICT DO SELECT` statement; a differing payload is a conflict ([ADR 0001](docs/adr/0001-postgres-is-the-queue.md)) |
| Intake | Partitioned queue by occurrence day, made ahead of time | ✅ | `ob_prepare_partitions` |
| Intake | Dropping old partitions after the window | 📋 | |
| Intake | Pre-priced events (each carries its amount) | ✅ | |
| Intake | Raw events rated at intake | ✅ | A meter, a quantity and dimensions; the event and its outcome keep their price version ([ADR 0004](docs/adr/0004-a-versioned-price-book-in-sql.md)) |
| Queue | Drain with `FOR UPDATE SKIP LOCKED`, once-only per event | ✅ | Concurrency tested on the conformance suite |
| Queue | Workers woken by `LISTEN/NOTIFY`, the table as the truth | ✅ | |
| Queue | A worker loop in the service binary | ⚠️ | `openbilling serve` drains on `LISTEN/NOTIFY` with a 5 s poll and prepares partitions hourly; not yet run against a PostgreSQL 19 |
| Rating | A versioned price book, loaded from YAML, rated in SQL | ✅ | Loaded once per content, never changed or back-dated; each event priced by the version in force when it happened, half to even once per event; unpriced meters and dimensions refused ([ADR 0004](docs/adr/0004-a-versioned-price-book-in-sql.md)) |
| Accounts | An owner holds several accounts (one per currency and label) | ✅ | [ADR 0002](docs/adr/0002-an-owner-holds-several-accounts.md) |
| Ledger | Double-entry, append-only, amounts in integer micro-units | ✅ | Every transaction balances, enforced by triggers |
| Ledger | Configurable draw order across accounts and lots | ✅ | |
| Ledger | Debt, a debt limit, and repayment from new credit | ✅ | |
| Ledger | Monthly allowance, granted once per account and month | ✅ | |
| Credits | Sources: payment, promotion, adjustment, each once-only by id | ✅ | |
| Credits | Refunds and chargebacks, capped at the payment | ✅ | Enforced by a trigger, also under concurrency |
| Budget | `allowed`: may this owner spend, with per-member monthly caps | ✅ | |
| Reads | Balance, lots, event outcomes, usage per day | ✅ | SQL functions today; an outcome is a plain query since slice 3; through the API as CrateStack models and `query` blocks (below) |
| Reads | Monthly statements | ⚠️ | A CrateStack `query` block: usage per day, credits, reversals, allowances, a total per kind. Its database test runs only in CI |
| Payments | A payment-provider port with vpay as the first adapter | ⚠️ | Signed webhooks become once-only credits; top-ups through the provider; each payment read back before it is credited ([ADR 0003](docs/adr/0003-credits-from-a-payment-provider.md)). The port's suite runs against a vpay stand-in; never against a real vpay; the vpay token is minted outside |
| API | External API through CrateStack: on-demand queries, custom selects, policies | ⚠️ | Models over the ledger's tables, `query` blocks behind read procedures, mutation procedures that call the SQL functions, an `Idempotency-Key` on every write; the contract lock and `docs/api/ops.json` checked in CI. The database half of its tests runs only in CI |
| API | Authentication of API callers, fail-closed | ⚠️ | A bearer `AuthProvider` over a `TokenVerifier` port: no token is a `401`. Only `RefuseAll` ships, so every call is refused until a JWT verifier against an issuer is built; the service binds to `127.0.0.1` |
| Testing | A conformance suite for every `LedgerStore` | ✅ | |
| Testing | A conformance suite for every `PriceBook` | ✅ | 9 cases, run in CI on PostgreSQL 19 |
| Testing | End-to-end scenarios against the real service and PostgreSQL | 📋 | Metered usage, prepaid balance, organisation caps, once-only, concurrency, price versions, statements, payment round-trip |
| Command line | `openbilling serve`, `--help`, `--version`, with clap and colour | ⚠️ | Colour off when not a terminal, `NO_COLOR` and `CLICOLOR_FORCE` honoured (tested); `serve` not yet run against a PostgreSQL 19 |
| Release | A signed container image and an OCI Helm chart (bjw-s app-template v5) | 📋 | |
| CI | fmt, clippy, tests on PostgreSQL 19, cargo deny, MSRV | ✅ | |
| CI | The CrateStack contract: `cratestack check`, `contract check`, `ops.json` current, no operation open to an anonymous caller | ⚠️ | New; CI has not run it yet |

## Build and test

You need Rust 1.98 or later and, for the database tests, PostgreSQL 19 or later (the minimum; older versions are
not supported). 1.98 is the MSRV because CrateStack 0.20.6 declares `rust-version = "1.98.0"` (sqlx 0.9 alone would
allow 1.94); CI's MSRV job holds it. The CrateStack CLI (`npm install --global @cratestack/cli@0.20.6`) checks the
API's schema and contract; see [`crates/openbilling-http`](crates/openbilling-http/README.md).

```sh
cargo build --workspace
cargo test --workspace     # the database tests skip: OPENBILLING_TEST_DATABASE_URL is unset

docker run --rm -d --name openbilling-pg -p 127.0.0.1:5432:5432 -e POSTGRES_HOST_AUTH_METHOD=trust postgres:19.0
OPENBILLING_TEST_DATABASE_URL=postgres://postgres@127.0.0.1:5432/postgres cargo test --workspace  # a fresh database per test

cargo run -p openbilling -- --help
OPENBILLING_DATABASE_URL=postgres://postgres@127.0.0.1:5432/postgres cargo run -p openbilling -- serve
```

`OPENBILLING_TEST_REQUIRE_DB=1` turns the skip into a failure; CI sets it. `docker stop openbilling-pg` removes the
database.

## Layout

| Path | What |
|---|---|
| `crates/openbilling-core` | The types and the storage ports, `LedgerStore` and `PriceBook`: ids, micro-unit amounts, quantities, time, credits, priced and raw events, refusals with a class. No I/O, no rule |
| `crates/openbilling-pg` | The PostgreSQL store: the migrations, which hold every rule, the YAML price list's parser, and `PgLedger`, which calls them. Needs PostgreSQL 19 or later |
| `crates/openbilling-testkit` | The conformance suites of `LedgerStore` and `PriceBook`, one macro each, and of `PaymentProvider`, and their fixtures |
| `crates/openbilling-vpay` | The vpay adapter of `PaymentProvider`, and a stand-in for vpay's API for tests |
| `crates/openbilling-http` | The external API: a CrateStack schema over the ledger's tables, its procedures, bearer authentication, and the payment provider's webhook |
| `bin/openbilling` | The service binary: `openbilling serve` |
| `docs/adr/` | The decisions |
| `docs/api/` | The API's contract, as CrateStack states it |

Each crate has a `README.md` that says what it holds and how it is tested.

## Contributing and security

See [CONTRIBUTING.md](CONTRIBUTING.md). Report a vulnerability privately as [SECURITY.md](SECURITY.md) says, never
in a public issue.

## Licence

Apache-2.0. See [LICENSE](LICENSE) and [NOTICE](NOTICE).
