# The API contract

The external API is a CrateStack schema, [`crates/openbilling-http/schema/openbilling.cstack`](../../crates/openbilling-http/schema/openbilling.cstack)
([ADR 0003](../adr/0003-credits-from-a-payment-provider.md)). CrateStack 0.20.6 generates no OpenAPI document; what
it offers instead is checked in:

| File | What | Made by |
|---|---|---|
| [`ops.json`](ops.json) | Every operation: its route, kind, `@allow` and `@deny` rules, the roles they name, whether it needs a caller, and its contract digest | `cratestack contract ops --schema … --json` |
| [`openbilling.contracts.lock`](../../crates/openbilling-http/schema/openbilling.contracts.lock) | The locked contract generations; a change that breaks one fails `cratestack contract check` | `cratestack contract lock` |

CI's `API contract` job runs `cratestack check`, `cratestack contract check`, regenerates `ops.json` and fails on a
difference, and fails on any operation a caller with no principal could reach. The routes, status codes and
authentication are in [`crates/openbilling-http/README.md`](../../crates/openbilling-http/README.md).

**`getAllowed` reports, the deployer judges.** Its answer carries the numbers the verdict was made from, whatever the
verdict: `balanceMicros` (drawable credit less debt), `availableMicros`, `debtMicros`, `debtLimitMicros`, `pastLimit`
(debt above the limit), `memberSpentMicros` (the member's applied usage this UTC month), `memberCapMicros` and
`memberRemainingMicros`. `allowed` and `refused` are the verdict of the comparisons the deployment's configuration
file chooses (`OPENBILLING_CONFIG`; `at_or_above` for every check by default), never the caller's. A caller with a
rule of its own, an external policy engine for example, reads the numbers and ignores the verdict (the owner's
decision of 2026-11-27, [ADR 0001](../adr/0001-postgres-is-the-queue.md), amended, F1 to F4).

Typed clients come from the same schema: `include_client_schema!` for Rust, `cratestack generate-typescript` and
`generate-dart` for the others. None is generated in this repository yet.
