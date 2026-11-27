# openbilling-core

The pure types and rules of openbilling-rs. No I/O, no async, one dependency (`thiserror`). Every other crate shares
this vocabulary, taken from [ADR 0001](../../docs/adr/0001-postgres-is-the-queue.md) and nothing beyond it.

| Type | What |
|---|---|
| `EventId`, `AccountId` | Caller-given ids: 1 to 128 bytes of printable ASCII (`!` to `~`). No whitespace, control character or non-ASCII text, so ids that look alike are the same bytes and log safely |
| `Amount` | Money as an integer number of minor units. Never a float. Checked arithmetic |
| `EventOutcome`, `RefusalReason` | What a worker decided: `applied`, or `refused` with `limit`, `no_price`, `unknown_account` or `id_reused` |
| `Error`, `ErrorClass`, `Classify` | Errors with a class (`invalid`, `conflict`, `unavailable`, `internal`). Callers decide from the class, never from the variant. An error never holds the value it refused |

## Why `Amount` is an `i64`

- It is PostgreSQL's `bigint`, which the driver maps directly. An `i128` would need `numeric` in the database, which
  is slower and needs a decimal type in the driver.
- It is what CrateStack's `BigInt` carries across the API (ADR 0001, *How the design sits on CrateStack*).
- `2^63 - 1` minor units is enough for any one balance: with two decimal places it is about 9.2 × 10^16 major units.
- Overflow is never silent: `checked_add` and `checked_sub` return `None`.

The range is symmetric: `i64::MIN` is refused, so every amount has a negation and every entry has its balancing
entry. An `Amount` carries no currency; the ledger keeps currencies apart and brings the type that pairs them.

`EventOutcome` is the decision only. The outcome record (price book version, amount, ledger transaction) comes with
the ledger. Intake refusals (`too_old`, `too_far_ahead`, invalid) are answers to a producer, not outcomes.

## Tests

`cargo test -p openbilling-core`: unit tests next to each module. No database, no environment variable.
