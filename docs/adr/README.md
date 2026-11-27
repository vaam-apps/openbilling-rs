# Decisions

| # | Decision | Status |
|---|---|---|
| [0001](0001-postgres-is-the-queue.md) | PostgreSQL is the queue, the once-only record and the ledger | Accepted (2026-11-27); amended 2026-11-27: PostgreSQL 19 is the minimum; amended 2026-11-27: micro-units, owners, rules in SQL, pre-priced events (slice 2); amended 2026-11-27: rating at intake, the raw payload hash, exact no-back-dating (slice 3); amended 2026-11-27: always book as debt, the limit refuses new calls; amended 2026-11-27: plain comparisons, the deployer's configuration decides how `allowed` judges, the numbers reported |
| [0002](0002-an-owner-holds-several-accounts.md) | An owner holds several accounts | Accepted (2026-11-27); amended 2026-11-27: always book as debt; amended 2026-11-27: `allowed` compares as the deployer's configuration says |
| [0003](0003-credits-from-a-payment-provider.md) | Credits come from a payment provider through a port, vpay first; the external API is CrateStack | Proposed (2026-11-27); amended 2026-11-27: a JWT token verifier; built and released, awaiting the owner |
| [0004](0004-a-versioned-price-book-in-sql.md) | A versioned price book in SQL | Accepted (2026-11-27) |
| [0005](0005-release-fixed-versions-image-and-chart.md) | Releases: release-please, fixed versions in production, a signed image and a chart on bjw-s common v5 | Proposed (2026-11-27); amended 2026-11-27: has run, v0.1.1–v0.4.0 released |
| [0006](0006-a-static-musl-binary-on-distroless-static.md) | A static musl binary on distroless/static | Proposed (2026-11-27); amends 0005 items 8 and 9; amended 2026-11-27: built in CI (#20), first released in 0.4.0 |
| [0007](0007-opentelemetry-through-a-layer-over-cratestacks-spans.md) | OpenTelemetry through the OpenTelemetry SDK over CrateStack's spans, and dashboards as code | Accepted (2026-11-27) |
