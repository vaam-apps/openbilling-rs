# openbilling-rs

Open-source usage billing in Rust, on PostgreSQL alone.

openbilling-rs is an open alternative to hosted metering-and-credits services. It takes usage events from your
services, rates them against a price book you define, and records the result in a double-entry ledger. PostgreSQL is
the queue, the once-only record and the ledger, so a write is one insert and every step is a transaction. There is no
broker to run.

**Status: the ledger and the price book.** The design is [ADR 0001](docs/adr/0001-postgres-is-the-queue.md),
[ADR 0002](docs/adr/0002-an-owner-holds-several-accounts.md) and
[ADR 0004](docs/adr/0004-a-versioned-price-book-in-sql.md). The PostgreSQL store loads a versioned price book from
YAML, rates raw usage events at intake by the version in force when they happened, takes them (or pre-priced ones)
into a partitioned queue once, drains them into a double-entry ledger across an owner's accounts, and books payments,
promotions, adjustments, refunds and chargebacks. Every rule is SQL; Rust holds the types, the ports and the tests.
Credits from a payment provider (vpay first) and an external API in CrateStack are built
([ADR 0003](docs/adr/0003-credits-from-a-payment-provider.md)), with callers authenticated by a JWT verifier against
an issuer's keys, and refused when none is configured. Every suite, the end-to-end one included, has run against
PostgreSQL 19.0 outside CI (2026-11-27). The release pipeline (a signed image and a Helm chart,
[ADR 0005](docs/adr/0005-release-fixed-versions-image-and-chart.md)) has run: v0.1.1, v0.2.0, v0.3.0 and v0.4.0 are
published (*verified 2026-11-27*, GitHub releases), and the 0.3.0 image and chart pulled anonymously. 0.4.0, the first
static image ([ADR 0006](docs/adr/0006-a-static-musl-binary-on-distroless-static.md)), was installed from the registry
in kind and its signatures verified by `release-check.yml` (run 46076030298, 2026-11-27).
Traces, metrics and logs go out over OpenTelemetry when an endpoint is set
([ADR 0007](docs/adr/0007-opentelemetry-through-a-layer-over-cratestacks-spans.md)). The chart has been installed in kind, not
in a real cluster.

## Features

Legend: ✅ built and tested in CI · ⚠️ built, but partial or not yet proven end to end · 🚧 being built ·
📋 planned · ⏸️ on hold. Updated with every change that moves a row.

| Area | Feature | Status | Notes |
|---|---|---|---|
| Storage | PostgreSQL 19 or later, the only infrastructure | ✅ | Older servers are refused at connect |
| Intake | Usage events accepted once by id; a replay changes nothing | ✅ | One `ON CONFLICT DO SELECT` statement; a differing payload is a conflict ([ADR 0001](docs/adr/0001-postgres-is-the-queue.md)) |
| Intake | Partitioned queue by occurrence day, made ahead of time | ✅ | `ob_prepare_partitions` |
| Intake | Dropping old partitions after the window plus a margin | ⚠️ | `retention.margin`, default 90 days (the owner's decision of 2026-11-28); `ob_expired_partitions` and `ob_drop_partition`, the detach run as a top-level `DETACH … CONCURRENTLY` by `openbilling serve` under an advisory lock, once at start and then hourly. Proven on PostgreSQL 19.0, never on a real deployment ([ADR 0001](docs/adr/0001-postgres-is-the-queue.md), G1 to G5) |
| Intake | Pre-priced events (each carries its amount) | ✅ | |
| Intake | Raw events rated at intake | ✅ | A meter, a quantity and dimensions; the event and its outcome keep their price version ([ADR 0004](docs/adr/0004-a-versioned-price-book-in-sql.md)). Through the API: `recordRawUsage`; the price list: `loadPriceList` |
| Queue | Drain with `FOR UPDATE SKIP LOCKED`, once-only per event | ✅ | Concurrency tested on the conformance suite |
| Queue | Workers woken by `LISTEN/NOTIFY`, the table as the truth | ✅ | |
| Queue | A worker loop in the service binary | ⚠️ | `openbilling serve` drains on `LISTEN/NOTIFY` with a 5 s poll and prepares partitions hourly; three at once on one database in the end-to-end suite, run by CI's Test job on PostgreSQL 19.0; never on a real deployment |
| Rating | A versioned price book, loaded from YAML, rated in SQL | ✅ | Loaded once per content, never changed or back-dated; each event priced by the version in force when it happened, half to even once per event; unpriced meters and dimensions refused ([ADR 0004](docs/adr/0004-a-versioned-price-book-in-sql.md)) |
| Accounts | An owner holds several accounts (one per currency and label) | ✅ | [ADR 0002](docs/adr/0002-an-owner-holds-several-accounts.md) |
| Ledger | Double-entry, append-only, amounts in integer micro-units | ✅ | Every transaction balances, enforced by triggers |
| Ledger | Configurable draw order across accounts and lots | ✅ | |
| Ledger | Debt, a debt limit, and repayment from new credit | ✅ | Usage is always booked, as debt past the limit (the owner's decision of 2026-11-27, "Always book as debt"); the limit refuses new calls through `allowed` (`debt_limit`, compared as the deployer's configuration says) and flags the owner (`pastLimit`, debt above the limit, on the balance read and on `allowed`) |
| Ledger | Monthly allowance, granted once per account and month | ✅ | |
| Credits | Sources: payment, promotion, adjustment, each once-only by id | ✅ | |
| Credits | Refunds and chargebacks, capped at the payment | ✅ | Enforced by a trigger, also under concurrency |
| Budget | `allowed`: may this owner spend, with per-member monthly caps | ✅ | Three checks in order, the debt limit, the balance (the debt against the drawable credit) and the member's cap, each a plain comparison with no special case for zero (the owner's decision of 2026-11-27: "Its not the role of this service to judge that") |
| Budget | The deployer chooses how `allowed` compares, in YAML | ✅ | `OPENBILLING_CONFIG` names one optional file: `allowed.debt_limit`, `balance` and `member_cap`, each `at_or_above` (the default), `above` or `never` (report only). An unknown key or value is refused at start; the rule stays in SQL, the mode passed in ([ADR 0001](docs/adr/0001-postgres-is-the-queue.md), amended, F1 to F4). The chart's `openbilling.config` renders it |
| Budget | `allowed` reports its numbers, whatever the verdict | ✅ | The net balance, the drawable credit, the debt, the limit, `pastLimit`, the member's usage this month, their cap and what is left: a caller with its own rule reads them and ignores the verdict |
| Reads | Balance, lots, event outcomes, usage per day | ✅ | SQL functions today; an outcome is a plain query since slice 3; through the API as CrateStack models and `query` blocks (below) |
| Reads | Monthly statements | ✅ | A CrateStack `query` block: usage per day and meter, credits, reversals, allowances, a total per kind. Run in CI on PostgreSQL 19 |
| Payments | A payment-provider port with vpay as the first adapter | ⚠️ | Signed webhooks become once-only credits; top-ups through the provider; each payment read back before it is credited ([ADR 0003](docs/adr/0003-credits-from-a-payment-provider.md)). The port's suite runs against a vpay stand-in; never yet against a real vpay. The adapter mints its own `vpay:v1` token (`client_credentials` with an RS256 `private_key_jwt`, ADR 0003 amended 2026-11-28), proven against the stand-in only: not ✅ until `vpay-sandbox.yml` has run green against a real vpay |
| API | External API through CrateStack: on-demand queries, custom selects, policies | ✅ | Models over the ledger's tables, `query` blocks behind read procedures, mutation procedures that call the SQL functions, an `Idempotency-Key` on every write; the contract lock and `docs/api/ops.json` checked in CI. Its flow tests run in CI on PostgreSQL 19 |
| API | Authentication of API callers, fail-closed | ✅ | A bearer `AuthProvider` over a `TokenVerifier` port: no token, or one refused, is a `401`. `openbilling-jwt` verifies a JWT against one issuer's JWKS (`RS256`, `ES256`, `EdDSA` only; `iss` and `aud` exact; `exp` required; roles from a configured claim), signatures on ring; with no issuer configured every call is refused. Never tried against a real identity provider, only a test issuer |
| Testing | A conformance suite for every `LedgerStore` | ✅ | |
| Testing | A conformance suite for every `PriceBook` | ✅ | 9 cases, run in CI on PostgreSQL 19 |
| Testing | End-to-end scenarios against the real service and PostgreSQL | ✅ | [`e2e/`](e2e/README.md): `openbilling serve` as a process, driven only through its API and webhook, with tokens of a test issuer. Metered model usage, prepaid balance, organisation caps, once-only, 10 000 events over three services, price versions, statements, payment round-trip, authentication. Run by CI's Test job, a step of its own, against the PostgreSQL 19.0 service CI pins; the real service is never tried against a real identity provider or vpay |
| Command line | `openbilling serve`, `--help`, `--version`, with clap and colour | ⚠️ | Colour off when not a terminal, `NO_COLOR` and `CLICOLOR_FORCE` honoured (tested); the help and the colour are tested; `serve` is what the end-to-end suite runs |
| Release | Versions from the commits: release-please's release pull request, a `vX.Y.Z` tag, a changelog | ✅ | `release-please.yml`, through a GitHub App token; it has opened the release pull requests and the tags v0.1.1 to v0.4.0 (*verified 2026-11-27*) ([ADR 0005](docs/adr/0005-release-fixed-versions-image-and-chart.md)) |
| Release | A container image, amd64 and arm64: a static musl binary on `distroless/static`, non-root, signed (cosign keyless), with provenance and an SBOM | ✅ | `docker/openbilling/Dockerfile` ([ADR 0006](docs/adr/0006-a-static-musl-binary-on-distroless-static.md)); the build fails if the binary is not static; `X.Y.Z` and `X.Y` on a tag, `sha-<7>` and `edge` on `main`, never `latest`. Built in CI on every pull request (merged as #20) and published since 0.1.1; 0.4.0 is the first static one. `release-check.yml` run [46076030298](https://github.com/vaam-apps/openbilling-rs/actions/runs/46076030298) (2026-11-27, 0.4.0): pulled anonymously as `sha256:4e7da01d…`, `cosign verify` accepted it for `release.yml@refs/tags/v0.4.0` (GitHub's OIDC issuer), the index holds linux/amd64 and linux/arm64 and attestation manifests with an SBOM and with SLSA provenance, and the service answered in kind |
| Release | A smoke test of the image (`--version`, `--help` on the runtime base, and `serve` against PostgreSQL), on both architectures, on every pull request | ✅ | CI's `image` job, which pushes nothing |
| Release | An OCI Helm chart on the bjw-s common library 5.3.0, signed, pinning its release's image by tag and digest | ✅ | `deploy/openbilling`, published at `oci://ghcr.io/vaam-apps/charts/openbilling` on a tag; linted, rendered and schema-checked by `tests/check.sh`, which CI's `chart` job runs; `release-check.yml` run [46076030298](https://github.com/vaam-apps/openbilling-rs/actions/runs/46076030298) (2026-11-27): the 0.4.0 chart pulled anonymously as `sha256:001ae382…`, `cosign verify` accepted it for `release.yml@refs/tags/v0.4.0`, and `helm install` of it in kind (v1.37.0) against PostgreSQL 19.0 ran the verified image by digest, `/healthz` and `/readyz` answered 200 and an unauthenticated `/v1` call 401. The token verifier's settings are optional values, unset by default |
| Command line | `serve` stops cleanly on `SIGTERM` (how Kubernetes stops a pod) | ⚠️ | `tests/cli.rs` runs in CI's Test job against PostgreSQL 19.0, and the `image` job (`docker stop` must exit 0); never in a Kubernetes pod |
| Observability | Traces, metrics and logs over OTLP/HTTP, off unless `OTEL_EXPORTER_OTLP_ENDPOINT` is set | ⚠️ | CrateStack `=0.20.6` emits `tracing` spans and has no exporter, so the OpenTelemetry SDK (`opentelemetry`, `opentelemetry-otlp`, `tracing-opentelemetry`) exports them with ours: routes, procedures, database calls, the drain and partition loops; metrics for events, the queue, `getAllowed`, errors by class and the pool; no id or amount can be an attribute ([ADR 0007](docs/adr/0007-opentelemetry-through-a-layer-over-cratestacks-spans.md)). Proven against a collector in the process on PostgreSQL 19; never against a real collector |
| Observability | Grafana dashboards as code, one README line per panel | ⚠️ | [`deploy/dashboards`](deploy/dashboards/README.md): the service, and the queue, the drain and PostgreSQL; generated by Python scripts in [`tools/dashboards`](tools/dashboards/generate.sh), with CI failing on any difference; a test holds every query to what the service exports. Never loaded into a Grafana |
| CI | fmt, clippy, tests on PostgreSQL 19, cargo deny, MSRV | ✅ | |
| CI | The chart: lint, a values matrix, kubeconform, no secret and every image pinned in each render | ✅ | CI's `chart` job runs `deploy/openbilling/tests/check.sh` |
| CI | The CrateStack contract: `cratestack check`, `contract check`, `ops.json` current, no operation open to an anonymous caller | ✅ | Run by the `API contract` job |

## Build and test

You need Rust 1.98 or later and, for the database tests, PostgreSQL 19 or later (the minimum; older versions are
not supported). 1.98 is the MSRV because CrateStack 0.20.6 declares `rust-version = "1.98.0"` (sqlx 0.9 alone would
allow 1.94); CI's MSRV job holds it. The CrateStack CLI (`npm install --global @cratestack/cli@0.20.6`) checks the
API's schema and contract; see [`crates/openbilling-http`](crates/openbilling-http/README.md).

```sh
cargo build --workspace
cargo test --workspace     # the database tests skip: OPENBILLING_TEST_DATABASE_URL is unset

docker run --rm -d --name openbilling-pg -p 127.0.0.1:5432:5432 -e POSTGRES_HOST_AUTH_METHOD=trust postgres:19.0
OPENBILLING_TEST_DATABASE_URL=postgres://postgres@127.0.0.1:5432/postgres cargo test --workspace  # a fresh database per test

cargo run -p openbilling -- --help
OPENBILLING_DATABASE_URL=postgres://postgres@127.0.0.1:5432/postgres cargo run -p openbilling -- serve
```

`OPENBILLING_TEST_REQUIRE_DB=1` turns the skip into a failure; CI sets it. `docker stop openbilling-pg` removes the
database.

## Releases

Versions come from the commits ([ADR 0005](docs/adr/0005-release-fixed-versions-image-and-chart.md)): release-please
keeps a release pull request open on `main` with the next version and its changelog, and merging it tags `vX.Y.Z`.
Nobody edits a version line by hand; `Cargo.toml`'s workspace version and the chart's `version` and `appVersion`
carry `x-release-please-version` and move together.

| Ref | Image `ghcr.io/vaam-apps/openbilling-rs` | Chart `oci://ghcr.io/vaam-apps/charts/openbilling` |
|---|---|---|
| tag `vX.Y.Z` | `X.Y.Z`, `X.Y` | `X.Y.Z`, whose default image is `X.Y.Z` by tag and digest |
| push to `main` | `sha-<7>`, `edge` (development only) | none |

There is no `latest`: production runs fixed versions only, and the chart refuses `latest` and any floating tag without
a digest. Images are built for amd64 and arm64 and carry their provenance and an SBOM; images and charts are signed
with cosign keyless, by `release.yml`'s identity ([the chart's README](deploy/openbilling/README.md) has the
`cosign verify` lines).

```sh
docker build -f docker/openbilling/Dockerfile --target smoke .   # the static musl build and the image's smoke test, as CI runs it
sh deploy/openbilling/tests/check.sh                            # the chart's checks (helm v4, kubeconform)
```

**State on 2026-11-27.** The GitHub App and its two secrets (`RELEASE_PLEASE_APP_CLIENT_ID`,
`RELEASE_PLEASE_APP_PRIVATE_KEY`) are set: release-please opened the release pull requests and v0.1.1, v0.2.0, v0.3.0
and v0.4.0 are published. The image and chart packages are public: the 0.3.0 image and chart manifests were fetched
with an anonymous token (*verified 2026-11-27*). v0.4.0's image and chart resolved later that day and passed
`release-check.yml` (run 46076030298): anonymous pull, `cosign verify`, install in kind, `/readyz`. Run it for any published
version with `workflow_dispatch`, or after a release.

## Layout

| Path | What |
|---|---|
| `crates/openbilling-core` | The types and the storage ports, `LedgerStore` and `PriceBook`: ids, micro-unit amounts, quantities, time, credits, priced and raw events, refusals with a class. No I/O, no rule |
| `crates/openbilling-pg` | The PostgreSQL store: the migrations, which hold every rule, the YAML price list's parser, and `PgLedger`, which calls them. Needs PostgreSQL 19 or later |
| `crates/openbilling-testkit` | The conformance suites of `LedgerStore` and `PriceBook`, one macro each, and of `PaymentProvider`, and their fixtures |
| `crates/openbilling-vpay` | The vpay adapter of `PaymentProvider`, and a stand-in for vpay's API for tests |
| `crates/openbilling-telemetry` | OpenTelemetry: the SDK pipeline over OTLP/HTTP JSON, the allowlist that cleans every span, the log layer, the bridge from the closed metrics, and a collector for tests |
| `crates/openbilling-http` | The external API: a CrateStack schema over the ledger's tables, its procedures, bearer authentication, and the payment provider's webhook |
| `crates/openbilling-jwt` | The API's token verifier: a JWT against an issuer's keys, on ring |
| `bin/openbilling` | The service binary: `openbilling serve` |
| `e2e/` | The end-to-end suite: the real service against PostgreSQL, through its API |
| `docker/openbilling/` | The image: cargo-chef, a static musl build, distroless/static, non-root, a smoke stage |
| `deploy/dashboards/` | The Grafana dashboards, with a README for each panel; generated by `tools/dashboards/` |
| `tools/dashboards/` | The Python (standard library) that writes the dashboards: `lib.py`, `service.py`, `queue_dashboard.py`, `generate.sh` |
| `deploy/openbilling/` | The Helm chart, on the bjw-s common library 5.3.0, and its checks (`tests/`) |
| `release-please-config.json`, `.release-please-manifest.json` | The versions release-please writes |
| `.github/workflows/` | `ci.yml` (Rust, the end-to-end suite, the API contract, the image's smoke test, the chart), `release-please.yml`, `release.yml`, `release-check.yml` |
| `docs/adr/` | The decisions |
| `docs/api/` | The API's contract, as CrateStack states it |

Each crate has a `README.md` that says what it holds and how it is tested.

## Contributing and security

See [CONTRIBUTING.md](CONTRIBUTING.md). Report a vulnerability privately as [SECURITY.md](SECURITY.md) says, never
in a public issue.

## Licence

Apache-2.0. See [LICENSE](LICENSE) and [NOTICE](NOTICE).
