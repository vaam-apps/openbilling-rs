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
| `20261128000001_partition_retention.sql` | Partition retention (ADR 0001, amended 2026-11-28, G1 to G5): `ob_expired_partitions` (what may go, and the statement that detaches it), `ob_drop_partition` (checks again, then drops; `OB904` when it must not), the helpers `ob_partition_day`, `ob_partition_blocked`, `ob_retention_cutoff`, and the lock `ob_retention_try_lock` / `ob_retention_unlock` |
| `20261128000002_top_ups.sql` | Top-ups (ADR 0003, amended 2026-11-28, ANR-1405): the table `ob_top_up` and its guard (terms never change, no delete, a final state is final: `OB901`; `pending` may become `expired`, an `expired` one is still settled, and a `mismatched`/`unrecorded` row is upgraded to `pending` by a late record with the very terms, nothing else leaves `mismatched`), `ob_record_top_up` (once-only by payment id; `OB011` for other terms, `OB002` for an account that does not exist; a late record whose terms equal a refused `unrecorded` payment upgrades it to pending), `ob_settle_top_up` (the comparison with the read-back, the credit from the recorded terms, a mismatch answered and kept with `o_marked` true only in the call that marked it, the policy for an unrecorded payment as an argument; under `refuse` the payment is kept as a mismatched top-up of the field `unrecorded`) `ob_expire_pending_top_ups` (pending older than `stop_after` to `expired`, bounded, oldest first by `ob_top_up_pending`) and `ob_claim_pending_top_ups` (a SQL-standard body: a materialized `picked` CTE with `FOR UPDATE SKIP LOCKED` under the `LIMIT`, then `UPDATE … RETURNING`; ordered `last_read_at NULLS FIRST, created_at`, served by the partial index `ob_top_up_pending_read`) |
| `20261128000003_top_up_last_read.sql` | One last read at expiry (ANR-1473, ADR 0003 amended 2026-11-28): `ob_claim_expiring_top_ups` (the pending rows at least `stop_after` old and not claimed in the last `read_again_after`, oldest first, the same `FOR UPDATE SKIP LOCKED` claim; disjoint from `ob_claim_pending_top_ups`) and `ob_expire_top_up` (moves one row to `expired` only while it is still pending and old enough, `false` when settled meanwhile, so no `OB901`); both SQL-standard bodies |
| `…004_ledger_functions.sql` | The payload hash, `ob_configure`, `ob_open_account`, `ob_set_debt_limit`, `ob_set_member_cap`, `ob_draw`, `ob_add_debt`, `ob_repay_from_lot`, `ob_credit`, `ob_reverse`, `ob_grant_allowance`, `ob_intake`, `ob_drain`, and the reads `ob_balance_of`, `ob_allowed`, `ob_lots_of`, `ob_outcome_of` (dropped by `…006`), `ob_daily_usage` |
| `…005_rated_events.sql` | Raw events ([ADR 0004](../../docs/adr/0004-a-versioned-price-book-in-sql.md)): `meter`, `quantity`, `dimensions` and `price_version` on the event and its outcome, the partial index of rated events that occurred after they were received, the guards again, and the raw payload hash (`ob_raw_payload_canonical`, `ob_raw_payload_hash`) |
| `…006_price_book.sql` | The price book: `ob_price_version`, `ob_price_meter`, `ob_price_dimension`, `ob_price`; the content hash, the seal and append-only triggers, `ob_price_back_dated`; `ob_load_price_list`; `ob_round_half_even`, `ob_price_of`, `ob_rate`; `ob_intake` and `ob_drain` again, with raw events |
| `…007_always_book_debt.sql` | The owner's decision of 2026-11-27, "Always book as debt": `ob_drain` again, with no `limit` refusal (what finds no credit is debt whatever the limit; a debt past the largest amount is `overflow`); `ob_allowed` again, refusing `debt_limit` first; the view `ob_past_debt_limit`, the owners flagged |
| `…008_allowed_policy.sql` | The owner's decision of 2026-11-27, "Its not the role of this service to judge that" (ADR 0001, amended, F1 to F4): the domain `ob_comparison` (`at_or_above`, `above`, `never`; anything else, or nothing, fails at the call), `ob_reached`, a plain comparison with no special case, and `ob_allowed` again, taking one mode per check (debt limit, balance, member cap) and answering the numbers beside the verdict: net balance, drawable credit, debt, limit, `past_limit`, the member's usage this month, cap and remainder |

**Telemetry.** Every `LedgerStore` and `PriceBook` method, `prepare_partitions` and `queue_stats` runs in a
`db.query` span (`db.operation`, never a parameter), and records into `openbilling-telemetry`'s registry: events at
intake by result and refusal rule, the drain's decisions and a debit posting per applied event, its duration and its
last success (the drain's lag), and a posting per credit, allowance or reversal that was not a replay.
`PgLedger::queue_stats` (the backlog and the age of the oldest event) and `sample_metrics` (those and the pool's
gauges) feed the gauges the dashboards read.

Each function says why it is `LANGUAGE sql` with a SQL-standard body or `plpgsql`. The functions raise SQLSTATEs of
class `OB`; `src/sqlstate.rs` maps each one, in one table, to a refusal or an error class.

## API

- `PgStore::connect(url)` and `connect_with(options, max_connections)` open a pool and check the server version;
  `migrate()` applies `migrator()`; `server_version_num()`, `ping()`, `pool()`, `close()`.
- `migrator()`: the ten migrations, embedded. sqlx sends each file whole as one simple-protocol query, so no
  `BEGIN ATOMIC` body is split on `;`.
- `PgLedger::new(&store, LedgerSettings)` implements `openbilling_core::LedgerStore` and `openbilling_core::PriceBook`.
  `load_price_list` parses the YAML (`src/price_list.rs`) and calls `ob_load_price_list`; `rate` calls `ob_rate`;
  `intake` and `intake_raw` call `ob_intake`; `price_versions` and `outcome` are plain queries. `LedgerSettings` holds the window
  (default seven days and five minutes), the partitions made ahead (default 2 days), the `NOTIFY` interval (default
  100 ms) and the retention margin (default 90 days). `prepare_partitions()` makes the partitions of every day intake accepts and of the days ahead.
- `PgLedger` implements `openbilling_core::PartitionRetention`: `drop_expired_partitions(margin)` drops every partition whose
  whole day is older than the window plus the margin (ADR 0001, point 10, amended 2026-11-28) and answers the days it dropped;
  `run_retention()` does so with `LedgerSettings::retention_margin` (default 90 days; the deployer's, from `retention.margin`
  of the configuration file). It never drops a partition inside the window plus the margin, one holding an event still
  `received`, or one whose day has an outcome still `pending`. **It runs four steps on one connection**:
  `ob_retention_try_lock` (a run that does not get it answers `skipped` and does nothing), `ob_expired_partitions` (the plan),
  for each partition the statement the plan returned (`ALTER TABLE … DETACH PARTITION … CONCURRENTLY`, or `FINALIZE` for a
  partition a cut-short detach left pending), run as a top-level command because PostgreSQL 19 refuses it in a transaction
  block, a function or a procedure (*verified 2026-11-28*), then `ob_drop_partition`. A run that stops between the steps is
  finished by the next; PostgreSQL refuses a second detach while another partition is pending-detach (*verified 2026-11-28*), so pending partitions are listed first as `finalize`; one partition failing does not stop the others (each drop is counted as it happens, the first failure is returned); the connection has `lock_timeout` and `statement_timeout` of `LedgerSettings::retention_timeout` (default 5 minutes), and a timeout is `unavailable`; the connection is closed after each run, so the session lock cannot outlive it. Outcomes are kept for
  good, so an id applied in a dropped partition is still refused as `id_reused`. It counts `openbilling.partitions.dropped`.
- `PgLedger` implements `openbilling_core::TopUpLedger` (ADR 0003, amended 2026-11-28): `record_top_up`, `settle_top_up`, `claim_expiring_top_ups`, `expire_top_up`, `expire_stale_top_ups` (the backstop, `ob_expire_pending_top_ups($stop_after, $batch, $last_read_for)`: at `stop_after + last_read_for` and only when `last_read_at >= created_at + stop_after`, a last read attempted) and `claim_pending_top_ups` each call one function of `…top_ups.sql`. `LedgerSettings::unrecorded_top_ups` (default `Refuse`, the deployer's `payments.unrecorded`) is passed to `ob_settle_top_up` as an argument. A mismatch is counted once, here, only when the call marked it (`o_marked`), in `openbilling.payments.mismatches` (the field is the label) and logged as a warning with the field and the class, never a value.
- `PgLedger::wakeup()` returns a `Wakeup` listening on `NOTIFY_CHANNEL`; `wait(poll)` answers `Woken::Notified` or
  `Woken::Polled`. Intake notifies after it commits, at most once per interval; a notification carries no data. The
  listener needs a direct session, not a pooler in transaction mode.
- `sqlstate::{LEDGER_SQLSTATES, RULES_DECIDED_IN_RUST, meaning_of, Meaning, Names}`: the map, and what a DETAIL names (events or a position in a price list).

Errors carry a class (`openbilling_core::Classify`): a network failure, a pool timeout or a SQLSTATE of class `08`,
`53` or `57P01`–`57P03` is `unavailable`, and so are a serialization failure, a deadlock and an event for a day with no
partition; a refusal has its rule's class; the rest is `internal`. An error's message never holds the URL, which can
carry a password.

**Not built:** an external read API for the price book (it will be
CrateStack's, in `openbilling-http`, which today only loads one: `loadPriceList`). The worker loop is
`openbilling serve` (`bin/openbilling`), which drains on `LISTEN/NOTIFY` and prepares partitions.

## sqlx version

sqlx is pinned `=0.9.0`, not the newer 0.9.1: CrateStack 0.20.6, the stack ADR 0001 names, pins `=0.9.0`, and Cargo
cannot build two semver-compatible versions of one crate. Move them together.

## Tests

| Test | Needs |
|---|---|
| Unit tests in `src/` (error classes, every SQLSTATE's meaning, the table and the migrations naming the same codes, DETAIL parsing of events and of positions, a malformed URL, the YAML parser's lines and refusals, a refusal that quotes nothing of the list) | nothing |
| `tests/connect.rs` (connect, the server version, ping, close) | `OPENBILLING_TEST_DATABASE_URL`, a PostgreSQL 19 or later |
| `tests/ledger.rs`: the whole conformance suite of `openbilling-testkit`, 27 cases, each on a fresh database | the same, and a user that may create databases |
| `tests/price_book.rs`: the price book's conformance suite, 9 cases, each on a fresh database | the same |
| `tests/top_ups.rs`: the top-up conformance suite, 6 cases (and 2 of its own, below), each on a fresh database (recorded once and other terms refused; every field that differs credits nothing and stays refused; the unrecorded policy both ways; the recorded terms credited once by eight settles at once; canceled is final; claims respect the three windows and the limit and are disjoint under concurrency), and what only PostgreSQL shows: a claim skips a row another connection holds locked and still fills its limit; an unrecorded payment is kept as a mismatched row; the table refuses a change of terms, a delete, a truncate and a second final state (`OB901`) | the same |
| `tests/retention.rs`: the retention conformance suite, 5 cases, each on a fresh database, and what only PostgreSQL shows: the settings' margin decides what `run_retention` drops (default 90 days), a run cut short after the detach, or in it (a partition left pending-detach), is finished by the next, a run that finds the lock held does nothing and a run releases it, an outcome still `pending` keeps its partition, `ob_drop_partition` refuses (`OB904`) a name that is not a partition's, a day inside the margin, an attached partition and one holding an undecided event, and a table that is not ours is never listed | the same |
| `tests/sql.rs`: migrations apply once and every function answers (and none is untested), the payload hash test vector, append-only tables, an unbalanced transaction cannot commit, the cap whoever writes, a drain that dies books nothing, a replay is not blocked by a held row, a missing partition is unavailable, a closed pool is unavailable, each comparison of `ob_reached` at its boundaries and an unknown or missing mode failing at the call of `ob_allowed`, intake wakes a listener, refusals reach the caller with their class; the raw payload hash test vector, half-to-even rounding in SQL, a version cannot be modified (append-only, sealed, its hash, back-dated even when written directly), a load waits for an intake that is rating, the price book's refusals reach the caller with their rule, class and position | the same |

The database tests skip (passing, with `skipped:` on stderr) when `OPENBILLING_TEST_DATABASE_URL` is unset. Under
`OPENBILLING_TEST_REQUIRE_DB=1` an unset variable is a failure instead; CI sets it. Each test creates its own database,
`ob_test_<pid>_<nanos>_<n>`, and leaves it behind: CI's server is thrown away with its job.

```sh
docker run --rm -d --name openbilling-pg -p 127.0.0.1:5432:5432 -e POSTGRES_HOST_AUTH_METHOD=trust postgres:19.0
OPENBILLING_TEST_DATABASE_URL=postgres://postgres@127.0.0.1:5432/postgres cargo test -p openbilling-pg
```
