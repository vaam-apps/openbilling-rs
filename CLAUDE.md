# Agent guide — openbilling-rs

`AGENTS.md` is a symlink to this file. Edit `CLAUDE.md` only.

## What this is

An open-source usage billing service in Rust on PostgreSQL alone: intake of usage events, rating against a price book,
and a double-entry ledger. **Status: design and skeleton.** [ADR 0001](docs/adr/0001-postgres-is-the-queue.md) is the
design. The Cargo workspace builds and is tested: the core types, a PostgreSQL store that connects and pings, an empty
conformance testkit and a binary that answers `--help` and `--version`. No table, migration or billing logic exists
yet.

## Layout

| Path | What |
|---|---|
| `crates/openbilling-core` | Pure types and rules, no I/O, no async: `EventId`, `AccountId`, `Amount` (`i64` minor units), `EventOutcome` and `RefusalReason`, `Error` with `ErrorClass` through `Classify` |
| `crates/openbilling-pg` | The PostgreSQL store on sqlx `=0.9.0` (held to CrateStack 0.20.6's pin). No `migrations/` yet: the tables come in slice 2. `tests/connect.rs` needs `OPENBILLING_TEST_DATABASE_URL` |
| `crates/openbilling-testkit` | The storage conformance suite, as macros. Empty until the storage trait exists |
| `bin/openbilling` | The service binary (`anyhow`). Answers `--help` and `--version` for now |
| `docs/adr/` | The decisions, with an index in `docs/adr/README.md` |
| `Cargo.toml`, `clippy.toml`, `deny.toml` | Workspace (edition 2024, resolver 3, MSRV `rust-version`), lints (`unsafe_code` forbidden, `unwrap`/`expect` warn outside tests), dependency policy |
| `.github/workflows/ci.yml`, `.github/dependabot.yml` | CI (format, clippy, tests with a PostgreSQL service, cargo deny, MSRV) and weekly updates of crates and actions |
| `.agents/`, `.claude/`, `.goose/`, `.kiro/`, `skills-lock.json` | The pinned skills; never edited by hand |

## Rules

1. **PostgreSQL is the only infrastructure.** No broker, no cache in the write path. A notification wakes a worker; the
   table is the truth.
2. **Every write is once-only.** An event, a credit and a debit each have a caller-given id; a replay changes nothing.
3. **Money is integers** in the currency's minor unit, never a float. Time is UTC.
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

## Skills

Skills live in `.agents/skills/` (symlinked into `.claude/skills/`, `.goose/skills/` and `.kiro/skills/`), installed
with the skills CLI and pinned in `skills-lock.json`. Update them with `npx skills update -p -y`, never by hand-editing
their files.

| When you are… | Use |
|---|---|
| Writing a schema, a server, procedures, migrations or clients | `cratestack` first; it routes to the `cratestack-*` skills |
| Authentication, tokens, the identity provider | `authkestra-quickstart`, then `authkestra-features`, `authkestra-op`, `authkestra-store` |
| Money, ledgers, idempotency | `domain-fintech`, `cratestack-data-integrity` |
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
cargo +1.94 check --workspace --all-targets --locked   # MSRV: rust-version of Cargo.toml
```

A database for the tests:

```sh
docker run --rm -d --name openbilling-pg -p 127.0.0.1:5432:5432 -e POSTGRES_HOST_AUTH_METHOD=trust postgres:19.0
export OPENBILLING_TEST_DATABASE_URL=postgres://postgres@127.0.0.1:5432/postgres
```

## Commits

Conventional Commits: `<type>[(scope)][!]: <description>`.
