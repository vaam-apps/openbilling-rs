# openbilling-rs

Open-source usage billing in Rust, on PostgreSQL alone.

openbilling-rs is an open alternative to hosted metering-and-credits services. It takes usage events from your
services, rates them against a price book you define, and records the result in a double-entry ledger. PostgreSQL is
the queue, the once-only record and the ledger, so a write is one insert and every step is a transaction. There is no
broker to run.

**Status (2026-11-28): built and tested, not yet proven on anything real.** Intake, rating, the double-entry ledger, the
price book, the budget check, the external API with its JWT verifier, payments through a provider port, the release
pipeline (a signed image and a Helm chart) and the OpenTelemetry telemetry, with its dashboards and alert rules, are
built. All of it is tested in CI on PostgreSQL 19.0; the telemetry ran against a real OpenTelemetry Collector,
Prometheus, Alertmanager and Grafana on a CI runner; the published chart installs in a kind cluster. **Not proven
anywhere real:** a cluster that carries traffic, a real vpay (the payments are proven against a stand-in), and a real
identity provider (a test issuer only). The latest release is v0.6.0. The design is
[ADR 0001](docs/adr/0001-postgres-is-the-queue.md), [ADR 0002](docs/adr/0002-an-owner-holds-several-accounts.md),
[ADR 0003](docs/adr/0003-credits-from-a-payment-provider.md) and [ADR 0004](docs/adr/0004-a-versioned-price-book-in-sql.md);
the releases are [ADR 0005](docs/adr/0005-release-fixed-versions-image-and-chart.md) and
[ADR 0006](docs/adr/0006-a-static-musl-binary-on-distroless-static.md), the telemetry
[ADR 0007](docs/adr/0007-opentelemetry-through-a-layer-over-cratestacks-spans.md), the Rust SDK
[ADR 0008](docs/adr/0008-a-rust-sdk-on-crates-io.md).

## What is ready

Legend: ✅ built and tested in CI · ⚠️ built, but partial or not yet proven end to end · 🚧 being built ·
📋 planned · ⏸️ on hold · ❌ not built or not done. Full table with notes and evidence:
[docs/features.md](docs/features.md).

**Money and usage**

| Feature | Status |
|---|---|
| Usage events accepted once by id, pre-priced or raw | ✅ |
| Versioned price book from YAML, rated at intake | ✅ |
| Partitioned queue, drained once per event | ✅ |
| Dropping old partitions after the retention window | ⚠️ not on a real deployment |
| Double-entry ledger, micro-unit integers, draw order, debt | ✅ |
| Owners with several accounts, monthly allowance, refunds and chargebacks | ✅ |
| `allowed` check with debt limit, balance and member caps | ✅ |
| Balances, lots, outcomes, usage per day, monthly statements | ✅ |

**Service and API**

| Feature | Status |
|---|---|
| External API (CrateStack) with contract checks | ✅ |
| Rust SDK (`openbilling-sdk`), generated from the schema | ⚠️ not yet published to crates.io |
| Fail-closed bearer authentication (JWT verifier) | ⚠️ test issuer only |
| `openbilling serve`, the drain loop, clean stop on `SIGTERM` | ⚠️ never in a real cluster |
| End-to-end suite against the real service and PostgreSQL 19 | ✅ |

**Payments**

| Feature | Status |
|---|---|
| Payment-provider port, vpay adapter, signed webhooks, top-ups | ⚠️ stand-in only |
| Changed payments refused, pending top-ups read back | ⚠️ stand-in only |
| An operator resolves a mismatched top-up | ⚠️ stand-in only, no runbook |

**Release and operations**

| Feature | Status |
|---|---|
| Versions from commits (release-please), tags, changelog | ✅ |
| Signed static image (amd64, arm64) and signed Helm chart | ✅ |
| Chart installed from the registry in kind | ✅ |
| OpenTelemetry traces, metrics and logs | ✅ |
| Grafana dashboards as code | ✅ |
| Prometheus alert rules as code, fire and resolve through Alertmanager | ✅ |
| Alert routing and paging | ❌ |
| Database backup and restore | ❌ |

**Ready for production?**

| Question | Status |
|---|---|
| Deployed on a real cluster | ❌ not yet |
| A real vpay | ❌ the gated `vpay-sandbox.yml` has never run |
| A real identity provider | ❌ test issuer only |
| A runbook for payment mismatches | ⚠️ the procedure exists, no written runbook |

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
| `tools/observability/` | The real Collector, Prometheus, Alertmanager and Grafana run against `serve`, and the gated test that asks them |
| `deploy/alerts/` | The generated Prometheus alert rules |
| `deploy/openbilling/` | The Helm chart, on the bjw-s common library 5.3.0, and its checks (`tests/`) |
| `release-please-config.json`, `.release-please-manifest.json` | The versions release-please writes |
| `.github/workflows/` | `ci.yml` (Rust, the end-to-end suite, the API contract, the image's smoke test, the chart), `observability.yml`, `release-please.yml`, `release.yml`, `release-check.yml`, `vpay-sandbox.yml` |
| `docs/features.md` | Every feature, its status, its notes and its evidence; and what is not ready for production |
| `docs/adr/` | The decisions |
| `docs/api/` | The API's contract, as CrateStack states it |

Each crate has a `README.md` that says what it holds and how it is tested.

## Contributing and security

See [CONTRIBUTING.md](CONTRIBUTING.md). Report a vulnerability privately as [SECURITY.md](SECURITY.md) says, never
in a public issue.

## Licence

Apache-2.0. See [LICENSE](LICENSE) and [NOTICE](NOTICE).
