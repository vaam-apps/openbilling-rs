# The end-to-end suite

The real `openbilling serve`, started as a process against a fresh PostgreSQL 19 database, and driven only through
its public HTTP API (`/v1`) and its payment webhook (`POST /webhooks/payments`). Callers carry bearer tokens that a
test issuer on a loopback port signs (`openbilling-jwt`, feature `test-issuer`) and that the service verifies
against the issuer's JWKS, as it would a real identity provider's. The payment provider is the vpay stand-in
(`openbilling-vpay`, feature `stand-in`), which the service reaches over HTTP as it reaches vpay.

It is the `e2e` test target of the `openbilling` package (`bin/openbilling/Cargo.toml`), because only a package's own
tests are handed the path of its built binary (`CARGO_BIN_EXE_openbilling`). The files live here.

## Run it

```sh
docker compose -f e2e/compose.yaml up -d --wait          # PostgreSQL 19.0, the image CI pins
export OPENBILLING_TEST_DATABASE_URL=postgres://postgres@127.0.0.1:5432/postgres
cargo test -p openbilling --test e2e                     # about 10 seconds once built
docker compose -f e2e/compose.yaml down
```

Without `OPENBILLING_TEST_DATABASE_URL` every scenario skips; under `OPENBILLING_TEST_REQUIRE_DB=1` it fails instead
(agent guide, rule 10). CI's Test job runs it with `cargo test --workspace`, against the job's PostgreSQL 19 service.

## The scenarios

Each is one test with a deployment of its own: a database, an issuer, and one service (two for the prepaid balance,
with two configurations, and three for the concurrency scenario). The producer is an example metered service; every id is made up and the currency is EUR.

| File | Test | What it shows |
|---|---|---|
| `authentication.rs` | `the_service_refuses_every_call_without_a_token_its_issuer_signed_for_it` | No token, another audience and another issuer are `401`; a missing role is `403`; the same binary with no issuer configured refuses a good token |
| `metered_usage.rs` | `metered_model_usage_is_rated_by_the_version_in_force_and_each_line_shows_on_the_statement` | A price list loaded through `loadPriceList`: input, output and cached tokens per model, and compute seconds. Raw events through `recordRawUsage`, each rated half to even by the version in force (amounts worked out by the test, apart from the SQL); each outcome keeps its meter, quantity, dimensions and version; the statement has one usage line per day and meter; an unpriced model refuses its batch |
| `prepaid_balance.rs` | `a_prepaid_balance_is_drawn_and_usage_past_the_debt_limit_is_booked_as_debt_and_flagged` | A credit drawn by usage; `getAllowed` refuses `balance` once it is used up and `debt_limit` once the debt passes the limit; the usage past the limit is applied and booked as debt (the owner's decision of 2026-11-27, "Always book as debt"), the owner flagged `pastLimit`; new credit repays the debt and lifts both. A second service on the same database reads a configuration file (`debt_limit: above`, `balance: never`): at every step both answer the same numbers, and their verdicts differ where the comparisons do (a limit of zero with nothing owed, debt under the limit with no credit, debt equal to the limit) |
| `organisation.rs` | `an_organisation_pays_and_a_members_cap_refuses_that_member_alone` | One owner pays for its members; a member at its cap is refused while the others and the organisation's balance go on; usage past the cap is still booked; caps are the operator's; another owner reads nothing |
| `once_only.rs` | `the_same_event_sent_three_times_debits_once_and_a_stale_event_is_refused_at_intake` | One event sent three times debits once (the API's idempotency replay, then the ledger's); the same id with another amount is a `409` that echoes nothing; an event older than the window or past the skew is refused; the same id on another day is refused `id_reused` and moves nothing |
| `concurrency.rs` | `three_services_drain_ten_thousand_events_and_each_is_applied_exactly_once` | Three services on one database, ten batches of 1 000 events sent to all three at once while their drains run: 10 000 outcomes, all applied, one ledger transaction per event, revenue equal to the usage, every owner's balance exact, the ledger summing to zero |
| `price_versions.rs` | `a_new_price_version_applies_from_its_start_and_older_events_keep_their_price` | Two versions: an event a microsecond before the second keeps the first's price, one at its start takes the second's, a late one from the first's time keeps the first's; before any version is `unpriced_meter`; a replayed list is the same version; the same instant is `version_conflict`; a back-dated version is refused |
| `statements.rs` | `a_months_statement_and_the_daily_series_equal_the_sum_of_the_applied_entries` | Usage over five days (one or two UTC months) and credits of each kind: each month's statement and the daily series equal the ledger's revenue entries per day, the outcomes agree, and a refused event is on none of them |
| `payments.rs` | `a_signed_payment_webhook_credits_once_and_a_succeeded_refund_reverses_it` | A top-up; a webhook before payment books nothing; a forged signature is `401`; the signed webhook credits once and its replay nothing more; a pending refund books nothing, a succeeded one reverses once; the statement and the credits list show both |
| `telemetry.rs` | `a_service_with_an_endpoint_exports_its_spans_and_metrics_and_names_no_one` | The service with `OTEL_EXPORTER_OTLP_ENDPOINT` set to a collector in the test: usage taken in and drained; the collector gets the request and `drain.cycle` spans, the metrics the dashboards read (the sampler's queue and pool gauges among them) with the resource's service name and release, and no byte that names the token, the owner, the member, an event or credit id, or an amount |

**Reads straight from the tables.** Nothing is written but through the API. A few invariants no API answers are read
from the ledger's tables, on a read-only connection: the ledger sums to zero per currency and per transaction, one
usage transaction per event, revenue per day, and the refusal of a reused id (which has no outcome of its own).

**Time.** Prices take effect a few seconds ahead of the clock (a version is never back-dated) and the events occur
within the 5-minute skew the window allows, so no scenario waits for a version to come into force. The database and
the test share one clock in these runs.

**Stopping.** A service is killed when its scenario ends, never asked to stop: a clean stop on `SIGTERM` is what
`bin/openbilling/tests/cli.rs` tests, not these scenarios.

**Connections.** At most four services run at once (the concurrency scenario takes three slots), so the suite stays
under PostgreSQL's default of 100 connections: each service holds up to 11.
