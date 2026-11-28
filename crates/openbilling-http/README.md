# openbilling-http

The external API of openbilling-rs ([ADR 0003](../../docs/adr/0003-credits-from-a-payment-provider.md)): a
CrateStack 0.20.6 schema, [`schema/openbilling.cstack`](schema/openbilling.cstack), served by the router CrateStack
generates from it, under `/v1`, over REST and JSON. Beside it, plain axum routes for the payment provider's webhook
and the probes.

## Routes

| Route | Who | What |
|---|---|---|
| `GET /v1/accounts`, `/v1/accounts/{id}` | the owner, `operator` | The `Account` model over `ob_account`: filters, `fields`, `sort`, paging (`@@paged`: `items`, `totalCount`, `pageInfo`) |
| `GET /v1/lots`, `/v1/lots/{id}` | the owner (through the account), `operator` | The `Lot` model over `ob_lot` |
| `GET /v1/usage_outcomes`, `/v1/usage_outcomes/{id}` | the owner, `operator` | The `UsageOutcome` model over `ob_event_outcome`, with a raw event's `meter`, `quantity`, `dimensions` and `priceVersion` |
| `POST /v1/$procs/getBalance` | the owner, `operator` | Each account's drawable credit now, the owner's total, debt and net, its debt limit and whether its debt is past it (`pastLimit`, the view `ob_past_debt_limit`) (`query balanceRows`, over the ledger's read `ob_balance_of`) |
| `POST /v1/$procs/getAllowed` | the owner, `operator`, `producer` | May a member spend now: refused `debt_limit`, `balance` or `member_cap`, compared as the service's configuration file says (`Services::allowed`; a caller cannot choose it), and the numbers whatever the verdict: `balanceMicros`, `availableMicros`, `debtMicros`, `debtLimitMicros`, `pastLimit`, `memberSpentMicros`, `memberCapMicros`, `memberRemainingMicros`. A caller with a rule of its own reads them and ignores `allowed` and `refused` (`query allowedRow`, over `ob_allowed`) |
| `POST /v1/$procs/listCredits` | the owner, `operator` | Every credit on the owner's accounts in a currency (`query creditRows`, plain SQL) |
| `POST /v1/$procs/getStatement` | the owner, `operator` | A UTC month: usage per day and meter (the meter is the line's `reference`; null for pre-priced events), credits, reversals, allowances, signed, and a total per kind (`query statementLines`, plain SQL) |
| `POST /v1/$procs/getUsageSeries` | the owner, `operator` | Applied usage per UTC day between two instants (`query usageDays`, plain SQL) |
| `POST /v1/$procs/configure` | `operator` | `ob_configure` |
| `POST /v1/$procs/openAccount` | `operator` | `ob_open_account`, once by account id |
| `POST /v1/$procs/grantCredit` | `operator` | `ob_credit`, a `promotion` or an `adjustment` only; once by source and id |
| `POST /v1/$procs/recordUsage` | `producer`, `operator` | `ob_intake`, pre-priced events, once by id and day (the body is `intake.rs`'s) |
| `POST /v1/$procs/recordRawUsage` | `producer`, `operator` | `ob_intake` through `PriceBook::intake_raw`: raw events (a meter, a quantity, dimensions as `{key, value}` pairs, each key once), rated at intake by the version in force at their time |
| `POST /v1/$procs/setDebtLimit`, `setMemberCap` | `operator` | `ob_set_debt_limit`, `ob_set_member_cap`; no amount removes it |
| `POST /v1/$procs/loadPriceList` | `operator` | `ob_load_price_list`: a YAML price list (ADR 0004) as a new version, or the one already loaded with that content (`replayed`) |
| `POST /v1/$procs/startTopUp` | the owner, `operator` | Checks the account exists first (`unknown_account`, nothing is asked of the provider), then asks the payment provider for a payment and its checkout, and records the top-up (owner, label, currency, amount) before answering: what the provider reports back is compared with it. A failure to record is the ledger's error and the caller retries with the same key |
| `POST /v1/$procs/reconcilePayment`, `reconcileRefund` | `operator` | Reads a payment or a refund back and books it if it succeeded; a payment that differs from its top-up is `409` `read_back_mismatch`, naming the field and no value |
| `POST /webhooks/payments` | the provider, by its signature | Verifies, reads back, books (`ob_credit` or `ob_reverse`) |
| `GET /healthz`, `GET /readyz` | anyone | The process answers; the database answers |

A procedure takes `{"args": {…}}`. Amounts are `BigInt` micro-units, decimal strings in JSON, beside their currency
code. Times are RFC 3339.

**Status codes.** `200` (a model read, a procedure), `400` (a body CrateStack cannot decode; a write with no
`Idempotency-Key`), `401` (no bearer token, or one the verifier refuses: with `WWW-Authenticate: Bearer`), `403` (a
procedure's policy), `404` (a model row the caller may not see, or that does not exist: the same answer), `405` (a
write to a model), `409` (a ledger or provider conflict; an idempotency key still in flight), `422` (a ledger or
provider refusal, a validator, a key reused with another body), `503` (the database or the provider unavailable, or
no provider configured), `500` (a fault; the message is fixed). A ledger refusal's message starts with the rule's
name (`unknown_account: …`) and names the events it refused. The webhook answers `200` with the outcome (`mismatch` for a payment that differs from its top-up: the refusal is final, so the provider is not asked to retry), `401` for a
signature that does not verify, `400` for a body that is not an event, and the class's status otherwise, so the
provider retries.

## Authentication

`auth::BearerAuth<V: TokenVerifier>` is CrateStack's `AuthProvider`: no bearer token, or one `V` refuses, is a
`401` before anything runs (a `503` when `V` cannot decide now). A `Principal` is the token's `sub` (an owner) and
`roles`. The service's verifier is [`openbilling-jwt`](../openbilling-jwt/README.md)'s `JwtVerifier`, a JWT against
one issuer's keys, when an issuer and an audience are configured; without them it is `auth::RefuseAll`, which
refuses every token, so every `/v1` call is a `401`. `auth::StaticTokens` is a fixed table for tests only.

## The ledger's tables are not CrateStack's

The models are `@@map`ped onto tables `openbilling-pg`'s migrations create. `cratestack migrate` is never run on
this schema, no snapshot is kept, and every model is read-only: `@@internal` removes its write routes, and no policy
allows a write. `prepare(pool)` creates CrateStack's own idempotency table, `cratestack_idempotency`, which is
CrateStack's bookkeeping.

## API

`router(services, pool, auth)` builds the whole surface; `Services { ledger, provider, allowed }` holds a
`LedgerStore` (`PgLedger` in the service), an optional `PaymentProvider`, and the `AllowedPolicy` `getAllowed` passes
to `ob_allowed` (the service's configuration file; a new field since 2026-11-27). `MUTATIONS` and `READS` name the procedures, held equal
to the schema by a test. `generated::cratestack_schema` is what CrateStack generates.

## Telemetry

`router` wraps the whole surface in one layer: an `http.server.request` span per request (not for `/healthz` and
`/readyz`, which are metrics only) and the request duration by method, route, procedure and status class. The route
and the procedure are fixed names (`/v1/$procs/{name}`, `recordUsage`, never a path with an id). CrateStack's own
procedure and list spans are its children. `getAllowed` counts its verdict by the check that decided; every error
the API answers is counted by class. Nothing a caller sent is a label or an attribute. The list is in
[`openbilling-telemetry`](../openbilling-telemetry/README.md).

## The contract

CrateStack 0.20.6 produces no OpenAPI document. The contract is CrateStack's: `schema/openbilling.contracts.lock`
(`cratestack contract lock`) and [`docs/api/ops.json`](../../docs/api/ops.json) (`cratestack contract ops --json`:
every operation, its rules, the roles they name and whether it needs a caller). CI's `API contract` job checks the
schema, the lock and that `ops.json` is current, and refuses an operation a caller with no principal could reach.

```sh
export CRATESTACK_REQUIRE_VERSION=0.20.6     # npm install --global @cratestack/cli@0.20.6
cratestack check --schema crates/openbilling-http/schema/openbilling.cstack
cratestack contract check --schema crates/openbilling-http/schema/openbilling.cstack \
  --lock crates/openbilling-http/schema/openbilling.contracts.lock
cratestack contract ops --schema crates/openbilling-http/schema/openbilling.cstack --json > docs/api/ops.json
```

A change to the contract that breaks a locked one fails `contract check`; `contract lock` records a new generation.

## Tests

| Test | Needs |
|---|---|
| Unit tests in `src/` (bearer parsing, the default verifier, error classes) | nothing |
| `tests/api.rs`: the procedure lists are the schema's; health, and readiness following the database; nothing served without an accepted token, on every model and read; every write needs a key; another owner and a producer refused by the policies; the webhook verifies, reads back (a "succeeded" event for a pending payment books nothing, and what the read-back means for the top-up is the ledger's to say) and answers `503` when the ledger is down | nothing: a ledger that is down and a pool to nowhere |
| `tests/otel.rs`: one scripted flow (configure, open, credit, intake, a refused batch, the drain, `getAllowed` twice, a bad token, partitions) exported to a collector in the process: the spans of the routes, procedures and database calls, the metrics by value, a log, and a scan of every byte received for the tokens, owner, account, member, event and credit ids, idempotency keys and amounts the flow sent | `OPENBILLING_TEST_DATABASE_URL` as below |
| `tests/flow.rs`: configure, open, credit (replayed by the key and by the ledger), a payment credit refused, usage refused to an owner, intake and a refused batch naming its event, the drain, then balance, `allowed`, credits, the statement and the usage series as the owner and the operator; the models owner-scoped, filtered and sorted; a top-up through the vpay stand-in, its webhook crediting once, reconciliation, a pending and a succeeded refund; the operator's settings (a debt limit and a member cap, refused to a producer and for an unknown owner), `getAllowed` under the deployer's policy (each check alone, a lenient policy, the numbers the same, a mode in the request ignored), a price list loaded and replayed and a malformed one refused, raw usage refused before the version, a dimension given twice refused, a raw event rated half to even and its outcome carrying its rating | `OPENBILLING_TEST_DATABASE_URL` (skips without it; fails under `OPENBILLING_TEST_REQUIRE_DB=1`) |
