# 0008. A Rust SDK, generated from the schema and published to crates.io

Status: **Proposed** (2026-11-28). The owner's decisions quoted under *Context* stand; *Still open* is for the owner.
Nothing is published yet: the crate is built and tested, `cargo publish --dry-run` passes, and the first release is
the owner's, by hand (*Publishing*).

## Summary

The repository ships a Rust client, the crate **`openbilling-sdk`**, and a release publishes it to crates.io. Its typed
calls are not written by hand: CrateStack generates them from the API's schema at build time, and a thin facade adds
the base URL, the bearer token, a required `Idempotency-Key` on every write and an error with a class. It **supersedes**
the decision of 2026-11-27 that integrators generate their own client from the `.cstack`.

## Context

The owner, 2026-11-27: integrators generate their own client from the `.cstack` schema (recorded in
`docs/api/README.md`: "None is generated in this repository yet"). The owner, 2026-11-28: *"We should add a sdk for
openbilling, so something like openbilling-sdk, in rust, that we'll push to crates.io via CI."* And, the same day:
*"We'll do the .cstack -> rust generation in CI."*

## Decision

1. **The SDK is a crate of this workspace**, `crates/openbilling-sdk`, `Apache-2.0`, published as `openbilling-sdk`
   (the name was free on crates.io on 2026-11-28: `GET /api/v1/crates/openbilling-sdk` answered 404).
2. **The client is generated, not written.** The crate holds `schema/openbilling.cstack`, a copy of the server's
   `crates/openbilling-http/schema/openbilling.cstack`, and `include_client_schema!` of CrateStack 0.20.6 expands it
   into the models, the types, every procedure's `Args` and `Output` and a raw `Client`.
   - **Why a copy, and why the macro.** CrateStack's CLI has `generate-dart`, `generate-typescript` and
     `generate-wiremock` and **no Rust generator** (`cratestack --help`, 0.20.6, *verified 2026-11-28*; the
     `cratestack-cli` skill says so too), so Rust output can only come from the proc-macro. The macro resolves its path
     against the crate's own `CARGO_MANIFEST_DIR`, so a path like `../openbilling-http/schema/…` would not exist in the
     package `cargo publish` builds and verifies. The schema therefore lives inside the crate, and what is published is
     what was reviewed: the copy, the facade, and the macro that is a pinned dependency.
   - **Design (a) or (b), as the owner offered.** (a) would commit the generated Rust and fail CI on a diff; it is not
     possible here, because there is no generator to produce the Rust (the only way to get it is to expand the macro
     with `cargo expand`, which is not a CrateStack command, would commit a large expansion nobody reads,
     and would still need the macro's runtime types). (b) is generation at build time. So the design is **(b) in its
     one available form: the generation is the macro, run by `cargo build`**, and it runs in every CI build, in the
     publish dry run on every pull request, and in the release job before `cargo publish`. What CI adds to hold it to
     the contract is a **drift check on the input**, not on generated output: the `sdk` job fails when
     `crates/openbilling-sdk/schema/openbilling.cstack` differs from the server's (`cmp`), and so does a test
     (`the_schema_is_the_servers`); the `sdk` job also runs `cratestack check` on the copy, pinned with
     `CRATESTACK_REQUIRE_VERSION=0.20.6`. The same job's `cargo publish --dry-run` builds the package on its own, so
     an input the package cannot reach fails the pull request, not the release.
   - If CrateStack ever ships `generate-rust`, move to (a): commit its output, regenerate in CI and fail on any
     diff, like the dashboards (ADR 0007).
3. **The facade is small and hand-written** (`src/client.rs`, `src/error.rs`, about 250 lines):
   - `Client::new(base_url)`, `set_token`, `clear_token` (the token rides a `RequestAuthorizer`, and its `Debug` says
     nothing), `raw()` for the generated client and the models;
   - one method per procedure, made by a macro from its name: the five reads take their `Args`, the twelve writes
     take their `Args` and an `IdempotencyKey`, so a write cannot be sent without one (the API answers `400` to a
     write with none, ADR 0003). A test holds the list equal to the schema's procedures and to `docs/api/ops.json`;
   - `Error` with an `ErrorClass` (`Invalid`, `Conflict`, `Unavailable`, `Internal`) and `Classify`, the same four
     classes as the service's (`openbilling_core`, rule 9). The crate does not depend on `openbilling-core`, which is
     `publish = false` and carries the service's types and no client; publishing it for four enum variants would
     publish a crate of no use to an integrator. An error never holds the token (a test checks it), and a base URL
     with credentials in it is refused.
4. **Dependencies are all on crates.io at the pinned version** (*verified 2026-11-28*): `cratestack-client` 0.20.6 (the
   client-only facade, no axum, no sqlx; it brings `cratestack-client-rust`, `-macros`, `-core`, `-parser`, `-policy`,
   `-sql`, `-codec-json` and `-codec-cbor`, all 0.20.6), `serde`, `thiserror`, `url`, `async-trait`. The version is
   pinned `=0.20.6`, as the server's is (they move together).
5. **One version, the workspace's.** The SDK is not a release-please component of its own: it takes
   `version.workspace = true`, so release-please's one release pull request, one tag and one changelog cover the
   service, the image, the chart and the SDK, and `release.yml` already checks that the tag is the version in
   `Cargo.toml`. The alternative, a component with its own tag and version, would let the SDK's version drift from the
   API it speaks to and would need a second tag, a second changelog and a second pull request per release. The cost is
   an SDK release when only the service changed; it publishes the same code under a new number, which is harmless.
   A user picks the SDK whose minor matches the service's. While the version is below 1.0 a `feat` bumps the minor
   (`bump-minor-pre-major`), and an API-breaking schema change must be a `feat!` or carry `BREAKING CHANGE:` (the
   contract check already refuses an unlocked break).
6. **Tests.** Unit tests (`tests/facade.rs`) need nothing; the end-to-end scenario `e2e/sdk.rs` configures, opens an
   account, books a credit, records usage and reads the balance through the SDK against the real `openbilling serve`
   with the test issuer, and checks a `403` and a `401` as classes. It is gated like the other database tests.

## Publishing

`release.yml` has a job, `publish-sdk`, that runs on a `v*` tag after the image, the chart and their checks:

- It uses **crates.io trusted publishing**: `rust-lang/crates-io-auth-action` (pinned by commit to v1.0.6,
  `3268eb41…`, the tag read from GitHub on 2026-11-28) exchanges the job's GitHub OIDC token for a short-lived
  publish token and revokes it at the end of the job. No secret is stored in the repository, and the job has
  `id-token: write` and `contents: read` only.
- **Trusted publishing needs the crate to exist.** A trusted publisher can only be configured on crates.io after a
  first, manual publish (RFC 3691: "can only be created after an initial manual publishing of a crate"; the crates.io
  development update of 2025-07-11: "you'll need to publish your first release manually"; *verified 2026-11-28*).
  So the first release is the owner's.
- **It skips cleanly.** The job runs only when the repository variable `CRATES_IO_PUBLISH` is `true`; and it stops
  with a notice, not a failure, when the crate is not on crates.io yet or this version already is (a re-run). Nothing
  else depends on it, so a crates.io outage or a missing configuration never fails the image or the chart.
- `cargo publish --dry-run -p openbilling-sdk --locked` runs on every pull request (`ci.yml`, job `sdk`) and again in
  the release job. It passed on 2026-11-28 (packaged 9 files, built the package on its own).
- A `CARGO_REGISTRY_TOKEN` secret is the fallback only if trusted publishing is refused to this repository; it is not
  used and none is added.

### The first release, by the owner

1. Log in to crates.io (GitHub) and create an API token scoped to `publish-new` for `openbilling-sdk`.
2. From a checkout of the release tag `vX.Y.Z`: `cargo publish -p openbilling-sdk --locked` with
   `CARGO_REGISTRY_TOKEN` set in that shell only. Revoke the token afterwards.
3. On crates.io, `openbilling-sdk` > Settings > Trusted Publishing > add a GitHub publisher: owner `vaam-apps`,
   repository `openbilling-rs`, workflow `release.yml`, environment empty.
4. In GitHub, repository Settings > Secrets and variables > Actions > Variables: `CRATES_IO_PUBLISH` = `true`.
5. The next release tag publishes the SDK by itself. Then mark the SDK ✅ in `docs/features.md` and the README.

## Consequences

- The SDK's public types include CrateStack's (`BigInt`, the generated structs with camelCase fields); a CrateStack
  major bump is an SDK bump.
- The server's schema and the SDK's copy move together: `cp` after every schema change; CI says so when one is
  forgotten.
- The SDK speaks JSON, as the server's `codec-json` does.
- A schema change that adds a procedure fails the SDK's test until the facade names it; that is the point.

## Still open

- Whether the SDK should have its own version line (item 5). The workspace's is chosen for now.
- A TypeScript or Dart client is not shipped; `cratestack generate-typescript` and `generate-dart` can make them.
- Not proven: the crate has never been published, so the install path (`cargo add openbilling-sdk`) and the trusted
  publishing job have not run.
