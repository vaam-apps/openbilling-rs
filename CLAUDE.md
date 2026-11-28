# Agent guide — openbilling-rs

`AGENTS.md` is a symlink to this file. Edit `CLAUDE.md` only.

## What this is

An open-source usage billing service in Rust on PostgreSQL alone: intake of usage events, rating against a price book,
and a double-entry ledger. **Status: the ledger (slice 2), the price book (slice 3), the API, the release pipeline and OpenTelemetry.** [ADR 0001](docs/adr/0001-postgres-is-the-queue.md),
[ADR 0002](docs/adr/0002-an-owner-holds-several-accounts.md) and
[ADR 0004](docs/adr/0004-a-versioned-price-book-in-sql.md) are the design. Built: owners with several accounts
(one currency and label each), credits by source (payment, promotion, adjustment), refunds and chargebacks capped at
the payment, the monthly allowance, the partitioned usage queue with once-only intake, the drain into the ledger with
the configured draw order, debt and debt limits (usage is always booked as debt; the limit refuses new calls
through `allowed` and flags the owner: the owner's decision of 2026-11-27), member caps, and the reads (`allowed`
reports its numbers and compares them as the deployer's YAML configuration file says, `OPENBILLING_CONFIG`, with no
special case: the owner's decision of 2026-11-27, ADR 0001 amended F1 to F4); a versioned price book loaded from YAML
(once per content, never changed or back-dated), raw events rated at intake by the version in force at their time
(half to even, once per event; unpriced meters and dimensions refused) and kept with their version. Every rule is SQL
in `crates/openbilling-pg/migrations/`; `PgLedger` calls it through the `LedgerStore` and `PriceBook` ports, and
`openbilling-testkit` holds the conformance suites. Every suite ran against PostgreSQL 19.0 (the image CI pins) outside CI on 2026-11-27. **Slice ANR-1143** ([ADR 0003](docs/adr/0003-credits-from-a-payment-provider.md)): a `PaymentProvider` port with its
conformance suite and the vpay adapter; the external API in CrateStack (`openbilling-http`), with policies and
bearer authentication; `openbilling serve` with the drain. **ANR-1142**: a JWT verifier against one issuer's keys
(`openbilling-jwt`, ADR 0003 amended), raw usage, the price list, debt limits and member caps through the API, and
the end-to-end suite (`e2e/`). Old usage partitions are detached and dropped after the window plus a margin (the owner's decision of 2026-11-28: `retention.margin`, default 90 days; ADR 0001 amended, G1 to G5).
**ANR-1332** (ADR 0003 amended 2026-11-28): the vpay adapter mints its own `vpay:v1` token (`client_credentials` with an
RS256 `private_key_jwt`, signed by ring; refreshed before expiry, one mint at a time, bounded retries; the client's
identity, key and endpoints are `OPENBILLING_VPAY_CLIENT_*`, `_TOKEN_URL` and `_ASSERTION_AUDIENCE`; its tunables are the
YAML file's `vpay.token`), **proven against the stand-in only**; the gated workflow
`vpay-sandbox.yml` (manual, needs a sandbox) has never run, so the Payments row is not ✅.
**ANR-1405** (ADR 0003 amended 2026-11-28): `startTopUp` records the top-up (`ob_top_up`); a payment read back is compared in
SQL with it and credited from the recorded terms, and one that differs (owner, label, currency or amount) credits nothing, stays
refused and is counted in `openbilling.payments.mismatches`; a succeeded payment with no recorded top-up follows
`payments.unrecorded` (default `refuse`); `serve` reads the pending top-ups back on a timer (`reconcile:` in the configuration
file, `openbilling.topups.reconciled`); a late record upgrades a payment a webhook refused as `unrecorded` when its terms agree, and a pending top-up that reaches `stop_after` is read back one last time (ANR-1473): paid, it is credited; only one still pending at the provider (or one it says does not exist) becomes `expired`; any other failed read leaves it pending (still settled by a webhook; `openbilling.topups.expired`; the backstop at `stop_after + last_read_for`). **Proven against the stand-in only**, which can change a payment after it was asked
for; vpay has not been asked. **ANR-1474** (ADR 0003 amended 2026-11-28): the operator's `resolveTopUp` closes a mismatched top-up once, by
`credit_recorded` (the recorded terms through `ob_credit`) or `dismiss`, recorded with who decided (`ob_top_up.resolved_by`, never logged); a mismatch no top-up was
recorded for can only be dismissed; counted in `openbilling.topups.resolved`.
**Slice ANR-1144** ([ADR 0005](docs/adr/0005-release-fixed-versions-image-and-chart.md)): the release pipeline:
release-please (versions from the commits, a GitHub App token), the image (`docker/openbilling`: since 0.4.0 a static
musl binary on `distroless/static`, [ADR 0006](docs/adr/0006-a-static-musl-binary-on-distroless-static.md); a smoke
stage), the Helm chart (`deploy/openbilling`, the bjw-s common library 5.3.0) and `release.yml` (`X.Y.Z` and `X.Y` on
a tag, `sha-<7>` and `edge` on `main`, never `latest`; signed; the chart pinned to its release's digest). It has run:
v0.1.1, v0.2.0, v0.3.0 and v0.4.0 are published GitHub releases, and the 0.3.0 image and chart resolved anonymously
(*verified 2026-11-27*). v0.4.0, the first static image, was releasing on 2026-11-27 and its packages did not resolve
yet; the chart has not been installed in a cluster, and no signature has been verified by hand. `serve` stops on
`SIGTERM`. **Observability** ([ADR 0007](docs/adr/0007-opentelemetry-through-a-layer-over-cratestacks-spans.md)):
traces, metrics and logs over OTLP/HTTP through the OpenTelemetry SDK, off unless an endpoint is set, and Grafana
dashboards and alert rules generated by Python, from one thresholds file. **Run against a real OpenTelemetry Collector,
Prometheus and Grafana on 2026-11-28**, locally and in CI (`sh tools/observability/run.sh`, ANR-1333; observability.yml run 46097502499, job 140832142178, at 14fd8e1); the names, the dashboards' queries and the rules'
evaluation held; an alert *firing* is proven by `promtool test rules`, and, with Alertmanager in the stack, end to end for one alert at test thresholds (ANR-1477: firing and resolved delivered to a webhook, locally 2026-11-28; CI run id to be added).

## Layout

| Path | What |
|---|---|
| `crates/openbilling-core` | Types, no I/O, no async runtime, no rule: the ids (`EventId`, `AccountId`, `OwnerId`, `MemberId`, `CreditId`, `CurrencyCode`, `AccountLabel`, `MeterId`, `DimensionKey`, `DimensionValue`), `Amount` (`i64` micro-units), `Quantity`, `Timestamp`/`UtcDay`/`UtcMonth`, the configuration, credits, priced and raw events, `EventOutcome`, the ports `LedgerStore` and `PriceBook` with `LedgerError`, `Refusal`, `ListPosition` and `Rule`, and `Error` with `ErrorClass` through `Classify` |
| `crates/openbilling-pg` | The PostgreSQL store on sqlx `=0.9.0` (held to CrateStack 0.20.6's pin); refuses a server older than PostgreSQL 19. `migrations/` holds every rule (hand-written SQL, applied by `migrator()`); `src/ledger.rs` is `PgLedger`, a thin caller; `src/price_list.rs` parses the YAML price list (serde-saphyr) and nothing more; `src/sqlstate.rs` the one map of SQLSTATEs. `tests/` need `OPENBILLING_TEST_DATABASE_URL` and make a fresh database per test |
| `crates/openbilling-testkit` | The conformance suites: `ledger_conformance!(factory)` of `LedgerStore` (`src/cases.rs`) and `price_book_conformance!(factory)` of `PriceBook` (`src/price_cases.rs`), and fixtures; of `PaymentProvider`, `payment_conformance!(factory)` over a `ProviderFixture` (`src/payments.rs`) |
| `crates/openbilling-vpay` | The vpay adapter of `PaymentProvider` (reqwest 0.13, rustls on ring); its webhook verifier is held to vpay's scheme and a vector; `src/token.rs` mints the `vpay:v1` token (RS256 assertion signed with ring directly: jsonwebtoken's provider is process-wide and `openbilling-jwt` owns it); the feature `stand-in` is a stand-in for vpay's API and its token endpoint, test code only. Every vpay fact is in ADR 0003's *Agent notes* |
| `crates/openbilling-metrics` | The dependency-free metrics registry every crate records into, with one seam (`Metrics::attach` of a `Recorder`); no instrument can carry an identity or an amount |
| `crates/openbilling-telemetry` | OpenTelemetry on the SDK (`opentelemetry`, `opentelemetry-otlp`, `tracing-opentelemetry`) over OTLP/HTTP JSON, off unless `OTEL_EXPORTER_OTLP_ENDPOINT` is set: the allowlist that cleans every span, the log layer, the bridge from the closed metrics, a collector for tests; a strict subset of the `OTEL_*` variables (ADR 0007) |
| `crates/openbilling-http` | The external API: `schema/openbilling.cstack` (CrateStack 0.20.6, REST, under `/v1`), its procedure bodies (writes call `PgLedger`), `BearerAuth` over the `TokenVerifier` port (`RefuseAll` ships), the webhook route, `schema/openbilling.contracts.lock`. Its models map onto the ledger's tables: never run `cratestack migrate` on this schema |
| `crates/openbilling-jwt` | The `TokenVerifier` the service uses when an issuer is configured: a JWT against the issuer's JWKS (`RS256`, `ES256`, `EdDSA` only), claims by jsonwebtoken 11 with no backend of its own, signatures by a jsonwebtoken `CryptoProvider` on ring (`src/provider.rs`); never aws-lc-rs or `rsa`. The feature `test-issuer` is a test issuer on a loopback port, test code only |
| `bin/openbilling` | The service binary (`anyhow`, clap with `color` and `wrap_help`, `anstream`): `serve`, which migrates at start and stops on `SIGINT` or `SIGTERM`. `src/config.rs` reads the environment (secrets, endpoints) and the optional YAML file `OPENBILLING_CONFIG` names (policy: `allowed.debt_limit`, `balance`, `member_cap`, each `at_or_above`, `above` or `never`), refusing an unknown key or value at start; no setting is read from both |
| `e2e/` | The end-to-end suite: the `openbilling` package's `e2e` test target (it needs `CARGO_BIN_EXE_openbilling`), which starts `openbilling serve` as a process against a fresh database and drives only its HTTP API and webhook, with tokens of `openbilling-jwt`'s test issuer and the vpay stand-in; `compose.yaml` is a local PostgreSQL 19. A scenario never writes to the tables; it may read invariants from them, read-only |
| `tools/dashboards/` | The Python (standard library) that writes the Grafana dashboards and the alert rules (`generate.sh`), from `thresholds.toml` (the stat panels' steps and the alerts' thresholds; `alerts.py --thresholds FILE --out DIR` takes a deployer's partial file); `tests/alerts.test.yaml` is the generated promtool unit test. CI's dashboards job regenerates everything, fails on any difference and runs `promtool check rules` and `test rules` |
| `tools/observability/` | `run.sh`: fetches the pinned Collector (contrib), Prometheus, Alertmanager and Grafana without a container runtime (`fetch.py`, pins and digests in `versions.env`, all *verified 2026-11-28*), runs them on 127.0.0.1 and the gated test `e2e/observability_stack.rs` (with test-only rules from `test-thresholds.toml` and a webhook sink the test serves), in two legs (`OBS_LEG=scrape`, the default: the Collector's `prometheus` exporter scraped; `otlp`: the `otlphttp` exporter into Prometheus's OTLP receiver, ANR-1476); the `observability.yml` workflow runs the same script and gates nothing |
| `deploy/dashboards/` | The generated dashboards, with a README line per panel; the chart can ship them as a ConfigMap (`configMaps.dashboards.enabled`) |
| `deploy/alerts/` | The generated Prometheus alert rules, with a README line per alert; the chart can render them as a `PrometheusRule` (`prometheusRule.enabled`, off by default) |
| `docker/openbilling/Dockerfile` | The image (ADR 0005, ADR 0006): a static musl binary on `distroless/static` (the build fails if the binary is not static): `chef`, `planner`, `builder` (`--locked`), `smoke` (`--version`, `--help` on the runtime base), `runtime` (the last, the default target). Bases pinned by tag and digest in its header: move both together, read the digest from the registry |
| `deploy/openbilling/` | The Helm chart (ADR 0005): values for the bjw-s common library 5.3.0 (`Chart.yaml`, `Chart.lock`; `charts/` is built, not committed), `templates/common.yaml` (the loader) and `templates/_validate.tpl` (the refusals, the configuration file's among them; `openbilling.config` is that file, rendered into a ConfigMap and mounted), `tests/check.sh` (everything CI checks), `tests/render-check.sh`, `tests/values/` (a `refused-*.yaml` says its message on `# expect:`), `tests/schemas/` (vendored; re-vendor, never edit). The digest line of `values.yaml` is rewritten by `release.yml`: keep its comment |
| `release-please-config.json`, `.release-please-manifest.json` | release-please's configuration and the last released version. The version lines it writes carry `# x-release-please-version`: never edit them by hand |
| `docs/api/` | The API's contract as CrateStack states it: `ops.json` (`cratestack contract ops --json`), checked in CI |
| `docs/adr/` | The decisions, with an index in `docs/adr/README.md` |
| `Cargo.toml`, `clippy.toml`, `deny.toml` | Workspace (edition 2024, resolver 3, MSRV `rust-version`), lints (`unsafe_code` forbidden, `unwrap`/`expect` warn outside tests), dependency policy |
| `.github/workflows/`, `.github/dependabot.yml` | `ci.yml` (format, clippy, tests with a PostgreSQL service, the end-to-end suite as a step of Test, cargo deny, MSRV, the API contract, the dashboards' drift check, the image's smoke test on pull requests, the chart), `vpay-sandbox.yml` (manual only: three conformance cases against a real vpay, skipped without its variables and secrets), `release-please.yml` (the release pull request and the tag, App token), `release.yml` (the image and the chart, signed), `cancel-stale-runs.yml` (cancels a branch's running workflows when its pull request closes or the branch is deleted), `observability.yml` (the Collector, Prometheus and Grafana on the runner and the gated end-to-end test, by `tools/observability/run.sh`; gates nothing), `release-check.yml` (installs the published chart in kind and runs `cosign verify`; on dispatch and after a release, gates nothing), and weekly updates of crates and actions |
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
| The payment provider (vpay): its API, webhooks, statuses | `vpay` first; it routes to `vpay-merchant-api`, `vpay-webhooks`, `vpay-payments` and the rest |
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
cargo test -p openbilling --test e2e   # the end-to-end suite alone (e2e/README.md; a database as below)
cargo deny check                       # deny.toml: advisories, licences, bans, sources
cargo +1.98 check --workspace --all-targets --locked   # MSRV: rust-version of Cargo.toml (CrateStack 0.20.6's)
CRATESTACK_REQUIRE_VERSION=0.20.6 cratestack check --schema crates/openbilling-http/schema/openbilling.cstack
CRATESTACK_REQUIRE_VERSION=0.20.6 cratestack contract check --schema crates/openbilling-http/schema/openbilling.cstack --lock crates/openbilling-http/schema/openbilling.contracts.lock
docker build -f docker/openbilling/Dockerfile --target smoke .   # pull requests: the image builds, --version and --help run
sh deploy/openbilling/tests/check.sh                            # the chart (helm v4.3.2 and kubeconform v0.8.0 on PATH)
actionlint                                                      # not in CI; run it on any workflow change
sh tools/dashboards/generate.sh && git diff --exit-code -- deploy tools/dashboards/tests   # the dashboards' and alerts' drift check
sh tools/observability/run.sh          # the real Collector, Prometheus and Grafana against `serve` (a database as below; fetches ~700 MB once); OBS_LEG=otlp for Prometheus's native OTLP receiver
```

A database for the tests (PostgreSQL 19; the store refuses an older one). Its user must be allowed to create
databases: each test makes its own.

```sh
docker run --rm -d --name openbilling-pg -p 127.0.0.1:5432:5432 -e POSTGRES_HOST_AUTH_METHOD=trust postgres:19.0
export OPENBILLING_TEST_DATABASE_URL=postgres://postgres@127.0.0.1:5432/postgres
```

## Commits

Conventional Commits: `<type>[(scope)][!]: <description>`. They are the release notes: release-please turns `feat`,
`fix` and the other types into the changelog and the next version (ADR 0005), so a commit's type and description are
what a reader of the release sees.

## Releases

Never edit a version line, tag by hand or push `latest`. Merging release-please's pull request tags `vX.Y.Z`;
`release.yml` refuses a tag whose `Cargo.toml` or `Chart.yaml` version differs. A change to the image or the chart
runs `sh deploy/openbilling/tests/check.sh` and, where a daemon exists, the smoke build; a new base image or chart
dependency is pinned by version and digest, read from its registry and marked *verified* with the date.
