# Agent guide — openbilling-rs

`AGENTS.md` is a symlink to this file. Edit `CLAUDE.md` only.

## What this is

An open-source usage billing service in Rust on PostgreSQL alone: intake of usage events, rating against a price book,
and a double-entry ledger. **Status: the ledger (slice 2) and the price book (slice 3).** [ADR 0001](docs/adr/0001-postgres-is-the-queue.md),
[ADR 0002](docs/adr/0002-an-owner-holds-several-accounts.md) and
[ADR 0004](docs/adr/0004-a-versioned-price-book-in-sql.md) are the design. Built: owners with several accounts
(one currency and label each), credits by source (payment, promotion, adjustment), refunds and chargebacks capped at
the payment, the monthly allowance, the partitioned usage queue with once-only intake, the drain into the ledger with
the configured draw order, debt and debt limits, member caps, and the reads; a versioned price book loaded from YAML
(once per content, never changed or back-dated), raw events rated at intake by the version in force at their time
(half to even, once per event; unpriced meters and dimensions refused) and kept with their version. Every rule is SQL
in `crates/openbilling-pg/migrations/`; `PgLedger` calls it through the `LedgerStore` and `PriceBook` ports, and
`openbilling-testkit` holds the conformance suites. **The suites have not run against a PostgreSQL 19 outside CI**
(none was available where they were written). **Slice ANR-1143** ([ADR 0003](docs/adr/0003-credits-from-a-payment-provider.md)): a `PaymentProvider` port with its
conformance suite and the vpay adapter; the external API in CrateStack (`openbilling-http`), with policies and
bearer authentication that refuses everyone until a real token verifier is wired; `openbilling serve` with the drain.
Not built: detaching and dropping old partitions, a JWT verifier, minting the vpay token, raw events through the
API.

## Layout

| Path | What |
|---|---|
| `crates/openbilling-core` | Types, no I/O, no async runtime, no rule: the ids (`EventId`, `AccountId`, `OwnerId`, `MemberId`, `CreditId`, `CurrencyCode`, `AccountLabel`, `MeterId`, `DimensionKey`, `DimensionValue`), `Amount` (`i64` micro-units), `Quantity`, `Timestamp`/`UtcDay`/`UtcMonth`, the configuration, credits, priced and raw events, `EventOutcome`, the ports `LedgerStore` and `PriceBook` with `LedgerError`, `Refusal`, `ListPosition` and `Rule`, and `Error` with `ErrorClass` through `Classify` |
| `crates/openbilling-pg` | The PostgreSQL store on sqlx `=0.9.0` (held to CrateStack 0.20.6's pin); refuses a server older than PostgreSQL 19. `migrations/` holds every rule (hand-written SQL, applied by `migrator()`); `src/ledger.rs` is `PgLedger`, a thin caller; `src/price_list.rs` parses the YAML price list (serde-saphyr) and nothing more; `src/sqlstate.rs` the one map of SQLSTATEs. `tests/` need `OPENBILLING_TEST_DATABASE_URL` and make a fresh database per test |
| `crates/openbilling-testkit` | The conformance suites: `ledger_conformance!(factory)` of `LedgerStore` (`src/cases.rs`) and `price_book_conformance!(factory)` of `PriceBook` (`src/price_cases.rs`), and fixtures; of `PaymentProvider`, `payment_conformance!(factory)` over a `ProviderFixture` (`src/payments.rs`) |
| `crates/openbilling-vpay` | The vpay adapter of `PaymentProvider` (reqwest 0.13, rustls on ring); its webhook verifier is held to vpay's scheme and a vector; the feature `stand-in` is a stand-in for vpay's API, test code only. Every vpay fact is in ADR 0003's *Agent notes* |
| `crates/openbilling-http` | The external API: `schema/openbilling.cstack` (CrateStack 0.20.6, REST, under `/v1`), its procedure bodies (writes call `PgLedger`), `BearerAuth` over the `TokenVerifier` port (`RefuseAll` ships), the webhook route, `schema/openbilling.contracts.lock`. Its models map onto the ledger's tables: never run `cratestack migrate` on this schema |
| `bin/openbilling` | The service binary (`anyhow`, clap with `color` and `wrap_help`, `anstream`): `serve` |
| `docs/api/` | The API's contract as CrateStack states it: `ops.json` (`cratestack contract ops --json`), checked in CI |
| `docs/adr/` | The decisions, with an index in `docs/adr/README.md` |
| `Cargo.toml`, `clippy.toml`, `deny.toml` | Workspace (edition 2024, resolver 3, MSRV `rust-version`), lints (`unsafe_code` forbidden, `unwrap`/`expect` warn outside tests), dependency policy |
| `.github/workflows/ci.yml`, `.github/dependabot.yml` | CI (format, clippy, tests with a PostgreSQL service, cargo deny, MSRV) and weekly updates of crates and actions |
| `.agents/`, `.claude/`, `.goose/`, `.kiro/`, `skills-lock.json` | The pinned skills; never edited by hand |

## Rules

1. **PostgreSQL is the only infrastructure.** No broker, no cache in the write path. A notification wakes a worker; the
   table is the truth.
2. **Every write is once-only.** An event, a credit and a debit each have a caller-given id; a replay changes nothing.
3. **Money is integers** in micro-units of the account's currency (10^-6 of its major unit; ADR 0001, amended
   2026-11-27), never a float, never converted between currencies. Time is UTC.
4. **Ports and adapters.** Storage and identity are traits with a conformance testkit; binaries only compose them.
5. **Decisions are ADRs** in `docs/adr/NNNN-kebab-title.md`, next number, with a row in `docs/adr/README.md`. Change a
   past decision with a dated *Amended* note, never silently.
6. **Mark facts** about third parties *verified* (date and source) or *unverified*.
7. **This repository is public.** No customer, deployment, price list or market of any operator goes in it.
8. **Every crate has a `README.md`** next to its `Cargo.toml` (with `readme = "README.md"` and
   `license = "Apache-2.0"`). Update it in the same change as its public API, environment variables or tests.
9. **Errors carry a class** (`openbilling_core::Classify`). Callers decide from the class, never from the variant, and
   an error never holds a secret or the value it refused. `thiserror` in libraries, `anyhow` in binaries.
10. **Tests that need a database skip when `OPENBILLING_TEST_DATABASE_URL` is unset** and fail instead under
    `OPENBILLING_TEST_REQUIRE_DB=1` (CI sets it).
11. **PostgreSQL 19 is the minimum.** Use its features rather than workarounds for older versions (ADR 0001, amended
    2026-11-27); the store refuses an older server at connect.
12. **SQL first: the ledger's rules live in PostgreSQL** (ADR 0002). Debit, credit by source, reversal, the drain, the
    draw order, intake's checks and the reads are functions, constraints, triggers and views in
    `crates/openbilling-pg/migrations/`, and so are the price book's checks, its content hash, rating and rounding
    (ADR 0004). Rust keeps the port, the types, the wiring, the YAML parsing and the tests, and never implements a
    rule a second time. Writes and internal operations are functions called as `SELECT … FROM fn($1, …)`; a read that
    carries no rule is a plain parameterized query, and reads for external clients are CrateStack's (the owner's
    decisions of 2026-11-27). Plain SQL gets a SQL-standard body (`LANGUAGE sql`, `RETURN` or
    `BEGIN ATOMIC … END`: parsed at creation, dependencies tracked, no polymorphic arguments); `LANGUAGE plpgsql` only
    for control flow or `RAISE` with a SQLSTATE, and the function's comment says which. Functions, not procedures,
    where the caller needs rows back. Atomicity is the caller's transaction, not `BEGIN ATOMIC`. Custom SQLSTATEs are
    class `OB` and are mapped to a refusal or a class in one place, `crates/openbilling-pg/src/sqlstate.rs`, whose
    tests hold the table and the migrations to the same codes. Never edit an applied migration: add one.

## Skills

Skills live in `.agents/skills/` (symlinked into `.claude/skills/`, `.goose/skills/` and `.kiro/skills/`), installed
with the skills CLI and pinned in `skills-lock.json`. Update them with `npx skills update -p -y`, never by hand-editing
their files.

| When you are… | Use |
|---|---|
| Writing a schema, a server, procedures, migrations or clients | `cratestack` first; it routes to the `cratestack-*` skills |
| Authentication, tokens, the identity provider | `authkestra-quickstart`, then `authkestra-features`, `authkestra-op`, `authkestra-store` |
| Money, ledgers, idempotency | `domain-fintech`, `cratestack-data-integrity` |
| The ledger's SQL, migrations | `cratestack-migrations` (the applier's rules), and the PostgreSQL 19 documentation: verify every claim |
| A decision | `documentation-and-adrs`, `doubt-driven-development` |
| Rust | `rust-router`, `rust-skills`, `coding-guidelines` |
| Implementing | `spec-driven-development`, `incremental-implementation`, `test-driven-development` |
| Images and compose | `docker-build-strategies`, `docker-compose-patterns` |

## Commands

What CI runs (`.github/workflows/ci.yml`):

```sh
cargo fmt --all --check
cargo clippy --workspace --all-targets --locked -- -D warnings
cargo test --workspace --locked        # CI sets OPENBILLING_TEST_DATABASE_URL and OPENBILLING_TEST_REQUIRE_DB=1
cargo deny check                       # deny.toml: advisories, licences, bans, sources
cargo +1.98 check --workspace --all-targets --locked   # MSRV: rust-version of Cargo.toml (CrateStack 0.20.6's)
CRATESTACK_REQUIRE_VERSION=0.20.6 cratestack check --schema crates/openbilling-http/schema/openbilling.cstack
CRATESTACK_REQUIRE_VERSION=0.20.6 cratestack contract check --schema crates/openbilling-http/schema/openbilling.cstack --lock crates/openbilling-http/schema/openbilling.contracts.lock
```

A database for the tests (PostgreSQL 19; the store refuses an older one). Its user must be allowed to create
databases: each test makes its own.

```sh
docker run --rm -d --name openbilling-pg -p 127.0.0.1:5432:5432 -e POSTGRES_HOST_AUTH_METHOD=trust postgres:19.0
export OPENBILLING_TEST_DATABASE_URL=postgres://postgres@127.0.0.1:5432/postgres
```

## Commits

Conventional Commits: `<type>[(scope)][!]: <description>`.
