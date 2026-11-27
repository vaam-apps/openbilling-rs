# openbilling-pg

The PostgreSQL store of openbilling-rs, on [sqlx](https://crates.io/crates/sqlx) (features `runtime-tokio`,
`tls-rustls`, `postgres`, `migrate`; no `macros`, so no query is checked against a database at build time), and the
price book's YAML parser, [serde-saphyr](https://crates.io/crates/serde-saphyr) (deserializing only).

**PostgreSQL 19 is the minimum.** The store uses its features rather than workarounds for older versions (ADR 0001,
amended 2026-11-27): `INSERT … ON CONFLICT … DO SELECT … RETURNING old…` for every once-only write, `uuidv7()` for the
ids the service makes. `connect` refuses an older server with `Error::UnsupportedServer`.

**Every rule of the ledger is SQL** in `migrations/` (agent guide, rule 12; [ADR 0002](../../docs/adr/0002-an-owner-holds-several-accounts.md)).
The Rust code calls one function per write or internal operation and maps the answer; a read that carries no rule
is a plain parameterized query (the owner's decision of 2026-11-27). It implements no rule.

| Migration | What |
|---|---|
| `…001_configuration_and_accounts.sql` | Helpers (`ob_is_id`, `ob_ts`, `ob_micros`, …), currencies, labels and their draw rank, the lot order (seeded with the example), accounts (`(owner, currency, label)` unique), the balance row per owner and currency (the lock, the debt, the debt limit), member caps |
| `…002_ledger.sql` | Ledger transactions, credits (once-only by source and id), lots, reversals, entries; append-only triggers, a deferred trigger that refuses a transaction that does not balance, the reversal cap as a trigger, and the views `ob_lot_drift` and `ob_debt_drift`, which must stay empty |
| `…003_usage_queue.sql` | `ob_usage_event`, partitioned by occurrence day with no default partition; the outcome per event id with its pending-to-final guard; `ob_prepare_partitions` |
| `…004_ledger_functions.sql` | The payload hash, `ob_configure`, `ob_open_account`, `ob_set_debt_limit`, `ob_set_member_cap`, `ob_draw`, `ob_add_debt`, `ob_repay_from_lot`, `ob_credit`, `ob_reverse`, `ob_grant_allowance`, `ob_intake`, `ob_drain`, and the reads `ob_balance_of`, `ob_allowed`, `ob_lots_of`, `ob_outcome_of` (dropped by `…006`), `ob_daily_usage` |
| `…005_rated_events.sql` | Raw events ([ADR 0004](../../docs/adr/0004-a-versioned-price-book-in-sql.md)): `meter`, `quantity`, `dimensions` and `price_version` on the event and its outcome, the partial index of rated events that occurred after they were received, the guards again, and the raw payload hash (`ob_raw_payload_canonical`, `ob_raw_payload_hash`) |
| `…006_price_book.sql` | The price book: `ob_price_version`, `ob_price_meter`, `ob_price_dimension`, `ob_price`; the content hash, the seal and append-only triggers, `ob_price_back_dated`; `ob_load_price_list`; `ob_round_half_even`, `ob_price_of`, `ob_rate`; `ob_intake` and `ob_drain` again, with raw events |

Each function says why it is `LANGUAGE sql` with a SQL-standard body or `plpgsql`. The functions raise SQLSTATEs of
class `OB`; `src/sqlstate.rs` maps each one, in one table, to a refusal or an error class.

## API

- `PgStore::connect(url)` and `connect_with(options, max_connections)` open a pool and check the server version;
  `migrate()` applies `migrator()`; `server_version_num()`, `ping()`, `pool()`, `close()`.
- `migrator()`: the four migrations, embedded. sqlx sends each file whole as one simple-protocol query, so no
  `BEGIN ATOMIC` body is split on `;`.
- `PgLedger::new(&store, LedgerSettings)` implements `openbilling_core::LedgerStore` and `openbilling_core::PriceBook`.
  `load_price_list` parses the YAML (`src/price_list.rs`) and calls `ob_load_price_list`; `rate` calls `ob_rate`;
  `intake` and `intake_raw` call `ob_intake`; `price_versions` and `outcome` are plain queries. `LedgerSettings` holds the window
  (default seven days and five minutes), the partitions made ahead (default 2 days) and the `NOTIFY` interval (default
  100 ms). `prepare_partitions()` makes the partitions of every day intake accepts and of the days ahead.
- `PgLedger::wakeup()` returns a `Wakeup` listening on `NOTIFY_CHANNEL`; `wait(poll)` answers `Woken::Notified` or
  `Woken::Polled`. Intake notifies after it commits, at most once per interval; a notification carries no data. The
  listener needs a direct session, not a pooler in transaction mode.
- `sqlstate::{LEDGER_SQLSTATES, RULES_DECIDED_IN_RUST, meaning_of, Meaning, Names}`: the map, and what a DETAIL names (events or a position in a price list).

Errors carry a class (`openbilling_core::Classify`): a network failure, a pool timeout or a SQLSTATE of class `08`,
`53` or `57P01`–`57P03` is `unavailable`, and so are a serialization failure, a deadlock and an event for a day with no
partition; a refusal has its rule's class; the rest is `internal`. An error's message never holds the URL, which can
carry a password.

**Not built:** detaching and dropping old partitions, the worker loop, an external read API for the price book (it
will be CrateStack's, over these tables).

## sqlx version

sqlx is pinned `=0.9.0`, not the newer 0.9.1: CrateStack 0.20.6, the stack ADR 0001 names, pins `=0.9.0`, and Cargo
cannot build two semver-compatible versions of one crate. Move them together.

## Tests

| Test | Needs |
|---|---|
| Unit tests in `src/` (error classes, every SQLSTATE's meaning, the table and the migrations naming the same codes, DETAIL parsing of events and of positions, a malformed URL, the YAML parser's lines and refusals, a refusal that quotes nothing of the list) | nothing |
| `tests/connect.rs` (connect, the server version, ping, close) | `OPENBILLING_TEST_DATABASE_URL`, a PostgreSQL 19 or later |
| `tests/ledger.rs`: the whole conformance suite of `openbilling-testkit`, 26 cases, each on a fresh database | the same, and a user that may create databases |
| `tests/price_book.rs`: the price book's conformance suite, 9 cases, each on a fresh database | the same |
| `tests/sql.rs`: migrations apply once and every function answers (and none is untested), the payload hash test vector, append-only tables, an unbalanced transaction cannot commit, the cap whoever writes, a drain that dies books nothing, a replay is not blocked by a held row, a missing partition is unavailable, a closed pool is unavailable, intake wakes a listener, refusals reach the caller with their class; the raw payload hash test vector, half-to-even rounding in SQL, a version cannot be modified (append-only, sealed, its hash, back-dated even when written directly), a load waits for an intake that is rating, the price book's refusals reach the caller with their rule, class and position | the same |

The database tests skip (passing, with `skipped:` on stderr) when `OPENBILLING_TEST_DATABASE_URL` is unset. Under
`OPENBILLING_TEST_REQUIRE_DB=1` an unset variable is a failure instead; CI sets it. Each test creates its own database,
`ob_test_<pid>_<nanos>_<n>`, and leaves it behind: CI's server is thrown away with its job.

```sh
docker run --rm -d --name openbilling-pg -p 127.0.0.1:5432:5432 -e POSTGRES_HOST_AUTH_METHOD=trust postgres:19.0
OPENBILLING_TEST_DATABASE_URL=postgres://postgres@127.0.0.1:5432/postgres cargo test -p openbilling-pg
```
