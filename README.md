# openbilling-rs

Open-source usage billing in Rust, on PostgreSQL alone.

openbilling-rs takes usage events from your services, rates them against a price book you define, and records the
result in a double-entry ledger. PostgreSQL is the queue, the once-only record and the ledger, so a write is one
insert and every step is a transaction. There is no broker to run.

**Status: design.** Nothing is built yet. The design is [ADR 0001](docs/adr/0001-postgres-is-the-queue.md).

## What it will do

| Part | What |
|---|---|
| Intake | Accept a usage event once by its id; a replay is a no-op |
| Rating | Turn events into amounts with a versioned price book |
| Ledger | Debit and credit accounts in one transaction, with balances and limits |
| Reads | Balances, statements and usage series for an account |

## Licence

Apache-2.0. See [LICENSE](LICENSE).
