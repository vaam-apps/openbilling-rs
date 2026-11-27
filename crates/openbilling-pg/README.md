# openbilling-pg

The PostgreSQL store of openbilling-rs, on [sqlx](https://crates.io/crates/sqlx) (features `runtime-tokio`,
`tls-rustls`, `postgres`, `migrate`; no `macros`, so no query is checked against a database at build time).

**PostgreSQL 19 is the minimum.** The store uses its features rather than workarounds for older versions (ADR 0001,
amended 2026-11-27: intake as one `INSERT … ON CONFLICT … DO SELECT … RETURNING OLD…` statement, `uuidv7()` for the
ids the service makes). `connect` refuses an older server with `Error::UnsupportedServer`.

**Now:** `PgStore::connect(url)` opens a pool and one connection and checks the server version (`server_version_num`
at least `MIN_SERVER_VERSION_NUM`, 190000), `server_version_num()` reads it, `ping()` runs `SELECT 1`, `close()`
closes the pool.
Errors carry a class (`openbilling_core::Classify`): a network failure, a pool timeout or a SQLSTATE of class `08`,
`53` or `57P01`–`57P03` is `unavailable`; an old server and the rest are `internal`. An error's message never holds the URL, which can
carry a password.

**Not yet:** the tables, the migrations and the hand-written SQL of [ADR 0001](../../docs/adr/0001-postgres-is-the-queue.md)
(intake, the drain, the partition job). There is no `migrations/` folder: the ADR names the tables but not all
their columns (the outcome record, the ledger, the price book), and a migration written now would invent them.
They come in slice 2.

## sqlx version

sqlx is pinned `=0.9.0`, not the newer 0.9.1: CrateStack 0.20.6, the stack ADR 0001 names, pins `=0.9.0`, and Cargo
cannot build two semver-compatible versions of one crate. Move them together.

## Tests

| Test | Needs |
|---|---|
| Unit tests in `src/lib.rs` (error classes, messages, a malformed URL) | nothing |
| `tests/connect.rs` (connect, the server version, ping, close) | `OPENBILLING_TEST_DATABASE_URL`, a PostgreSQL 19 or later |

`tests/connect.rs` connects when `OPENBILLING_TEST_DATABASE_URL` is set and skips (passing, with `skipped:` on
stderr) when it is unset. Under `OPENBILLING_TEST_REQUIRE_DB=1` an unset variable is a failure instead; CI sets it. Against a server older than
PostgreSQL 19 the test fails, because `connect` refuses it.

```sh
docker run --rm -d --name openbilling-pg -p 127.0.0.1:5432:5432 -e POSTGRES_HOST_AUTH_METHOD=trust postgres:19.0
OPENBILLING_TEST_DATABASE_URL=postgres://postgres@127.0.0.1:5432/postgres cargo test -p openbilling-pg
```
