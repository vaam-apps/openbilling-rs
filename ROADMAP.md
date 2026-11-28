# Roadmap

What is built, what is missing, what is half done and what will take long. Written 2026-11-28 from the code, the
ADRs and CI of `main` at `2461e82`. Card ids (`ANR-NNN`) are the project board's; "no card yet" means nobody has filed
one.

## Where it stands (2026-11-28)

- The ledger, the price book, intake and the external API are built, tested in CI on PostgreSQL 19.0, and released
  as a signed image and chart (v0.4.0).
- Every external system is proven only against a stand-in or a test double: the payment provider, the identity
  provider, the OpenTelemetry collector, Grafana.
- No cluster outside kind has run it, and two pieces of work (#29, #30) are in review.

## Built and proven

Details and the per-row status are in the [README features table](README.md#features). In short:

| Area | What | Proof |
|---|---|---|
| Ledger | Double-entry, owners with several accounts, credits by source, refunds and chargebacks capped, debt and limits, allowance, draw order | `ledger_conformance!` on PostgreSQL 19.0 in CI; [ADR 0001](docs/adr/0001-postgres-is-the-queue.md), [0002](docs/adr/0002-an-owner-holds-several-accounts.md) |
| Queue | Once-only intake, day partitions made ahead, drain with `SKIP LOCKED`, `LISTEN/NOTIFY` wake | CI Test job, the end-to-end suite |
| Price book | Versioned YAML, rated in SQL at intake, half to even | `price_book_conformance!`; [ADR 0004](docs/adr/0004-a-versioned-price-book-in-sql.md) |
| API | CrateStack `/v1`, bearer verification (JWT against one issuer's keys), fail closed | the `API contract` job; the end-to-end suite with a test issuer |
| Release | release-please, signed multi-arch static image, signed OCI chart, install in kind | v0.1.1 to v0.4.0 published; `release-check.yml` run [46076030298](https://github.com/vaam-apps/openbilling-rs/actions/runs/46076030298) (2026-11-27, kind v1.37.0) |

## Missing: not started

| What | Why it matters | Card |
|---|---|---|
| Refuse a payment read-back whose provider-side `metadata` was changed | Today the top-up's target lives only in the provider's metadata (ADR 0003, *Still open*) | no card yet |
| Reconcile pending top-ups (list pending payments, read each back) | A lost webhook leaves a paid top-up uncredited until someone reads it back (ADR 0003, *Still open*) | no card yet |
| `Idempotency-Key` keyed by the verified `sub`, not the raw `Authorization` header | A retry with a refreshed token is not replayed by the layer; the ledger's once-only ids still hold (ADR 0003) | ANR-1162 (upstream, in CrateStack) |
| Alert rules generated beside the dashboards | The dashboards' thresholds are starting points only (ADR 0007, *Still open*) | ANR-1333 |
| Several issuers, DPoP- or mTLS-bound tokens, token introspection | One issuer and plain bearer tokens only (ADR 0003, *Not built*) | no card yet |
| A conformance suite for `TokenVerifier` | The verifier's own tests and the end-to-end suite hold it today | no card yet |
| A provider prefix on credit ids | Needed when a second payment provider arrives (ADR 0003) | no card yet |
| Provider chargebacks | The first provider reports none (ADR 0003) | no card yet |
| Opening and closing balances on statements | Needs the expiry entries ADR 0002 leaves open | no card yet |
| An external read API for the price book | Only `loadPriceList` exists (`crates/openbilling-pg/README.md`) | no card yet |
| `traceparent` forwarded to the payment provider | The adapter sends no trace context (ADR 0007) | no card yet |
| An OpenObserve-native dashboard format | Grafana JSON only (ADR 0007) | no card yet |
| A load or soak test | CI runs 10 000 events over three services; no sustained or contended run exists | no card yet |

## Incomplete: started but not done, or built but unproven

| What | State | What would prove it | Card |
|---|---|---|---|
| Partition retention: detach and drop old usage partitions | Built in [#30](https://github.com/vaam-apps/openbilling-rs/pull/30), in review, not merged. Until it lands partitions pile up (ADR 0001, B8). Its PR says `helm` and `kubeconform` were not run locally | CI green on the PR, then a run on a real deployment with more than one replica | ANR-1331 |
| Minting the `vpay:v1` token in the adapter | Built in [#29](https://github.com/vaam-apps/openbilling-rs/pull/29), in review, conflicts with `main`. Proven only against the stand-in. Its gated sandbox job (`vpay-sandbox.yml`, 3 of the suite's 7 cases) has never run | A green run of the sandbox job against a real vpay; the README's Payments row stays at ⚠️ until then | ANR-1332 |
| The vpay adapter as a whole | Webhooks, read-back, top-ups, refunds: run against the stand-in; never against a real vpay | The `payment_conformance!` suite against a real vpay, including the four cases that need a payer | ANR-1332 |
| Bearer authentication | `openbilling-jwt` verifies against one issuer's keys; only a test issuer has been used | An end-to-end run against a real identity provider | no card yet |
| Telemetry | Traces, metrics and logs proven against an in-process collector | A pinned collector and Prometheus in CI; the name translation the dashboards assume (ADR 0007, *Still open*) | ANR-1333 |
| Grafana dashboards | Generated, drift-checked, with a test holding each query to the exported names | Loaded into a real Grafana; a counter with no data point shows no data, not zero (ADR 0007, *unverified*) | ANR-1333 |
| `serve` as a worker | Drain, poll and partition loops run in the end-to-end suite | A run on a deployment under real load | no card yet |
| `SIGTERM` stop | Tested by `tests/cli.rs` and `docker stop` in CI | A rollout in a real Kubernetes pod | no card yet |
| The Helm chart | Linted, rendered over a values matrix, schema-checked, and installed in kind from the registry (v0.4.0) | An install in a real cluster with a real database operator and a scrape of the `ServiceMonitor` by a real Prometheus | ANR-1145 (the deployment from the published chart) |
| Adoption by a downstream service | The crates are built for it; no service depends on them yet | A downstream service wired to the published crates | ANR-1111 |
| `CrateStack` pin and `sqlx` pin | Held at `sqlx =0.9.0` by CrateStack 0.20.6 | CrateStack widens its pin | ANR-1146 (upstream) |

## Needs more time

| What | Why it is big |
|---|---|
| A real vpay run (ANR-1332) | A third party. It needs a sandbox deployment and a client key that the owner provides, and four of the seven suite cases need a payer or a rail to act. |
| A real identity provider run | A third party and an operator's configuration. It also decides whether several issuers or bound tokens are needed. |
| A real cluster install | Needs a cluster, a PostgreSQL 19 operator (the minimum is 19, which not every operator image offers yet: *unverified*), secrets, backups and a scrape. The repo can prove the chart; it cannot prove the platform around it. |
| Telemetry against a real stack (ANR-1333) | Needs a pinned collector, a Prometheus and a Grafana in CI, and their name translation may force edits to every dashboard query. |
| Retention on a live database | `DETACH PARTITION ... CONCURRENTLY` cannot run in a function on PostgreSQL 19.0, so the store drives three steps on one connection. Cutting it short is safe by design, but only a long run on real data shows lock behaviour. |
| Credit expiry and statement balances | An open design question (ADR 0002); it touches the ledger, the statement and the API contract. |

## Next

1. Review and merge [#30](https://github.com/vaam-apps/openbilling-rs/pull/30) (partition retention), with the chart job green.
2. Resolve the conflict on [#29](https://github.com/vaam-apps/openbilling-rs/pull/29), review it, and merge it.
3. Run the gated vpay sandbox job once a sandbox and key exist, and tick the Payments row only when it passes.
4. Add the two ADR 0003 robustness items: refuse a changed read-back, and reconcile pending top-ups.
5. ANR-1333: a pinned collector and Prometheus in CI, then generated alert rules.

## How this page is kept

It is updated in the same pull request as any change that moves a row: a merged card, a new proof, a new gap. The
[README features table](README.md#features) and the ADRs' *Still open* sections stay the detail; this page stays the
summary. A claim about a third party is *verified* with its date and source, or *unverified*.
