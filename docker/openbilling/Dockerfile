# The openbilling image: `bin/openbilling` on a distroless runtime, non-root (ADR 0005).
#
# Built from the repository root (the context is `.`):
#
#   docker build -f docker/openbilling/Dockerfile --target smoke .     # build, and run --version and --help in it
#   docker build -f docker/openbilling/Dockerfile .                    # the image (the last stage, `runtime`)
#
# Stages: `chef` (the toolchain and cargo-chef), `planner` (the dependency recipe), `builder` (dependencies in a
# layer of their own, then the binary, `--locked`), `smoke` (the binary run on the runtime base) and `runtime`.
#
# Pins, by tag and digest (the digest wins; the tag says what it is). Move both together; dependabot does not see
# these lines.
#   - rust:1.98.1-slim-trixie, the multi-arch index digest, read from Docker Hub on 2026-11-27 (*verified*:
#     hub.docker.com/v2/repositories/library/rust/tags/1.98.1-slim-trixie). The toolchain CI builds with
#     (ci.yml, RUST_TOOLCHAIN); the MSRV is 1.98 (Cargo.toml).
#   - gcr.io/distroless/cc-debian13:nonroot, the multi-arch index digest, read from gcr.io on 2026-11-27 (*verified*:
#     the registry's manifest answer). glibc, libgcc and CA certificates, no shell, uid 65532. Debian 13 (trixie),
#     the release the builder links against.
#   - cargo-chef 0.1.78, the newest on crates.io on 2026-11-27 (*verified*, published 2026-08-12).
#
# The migrations are compiled into the binary (`openbilling-pg`, `include_str!`), and `openbilling serve` applies
# them at start, under sqlx's advisory lock: the image carries no SQL file.

ARG RUST_IMAGE=rust:1.98.1-slim-trixie@sha256:4cd829461bd5c4d511c32e269da9cb8929223b666519d8004e35fc8d1d771ab7
ARG RUNTIME_IMAGE=gcr.io/distroless/cc-debian13:nonroot@sha256:29ee70762214fe8190cea04ef1c0a84a8365879e047ce8f911ae5a5ab2355fa2

FROM ${RUST_IMAGE} AS chef
ARG CARGO_CHEF_VERSION=0.1.78
RUN cargo install cargo-chef --locked --version "${CARGO_CHEF_VERSION}"
WORKDIR /src

# The recipe is the workspace's manifests and lockfile with the sources stripped, so the next stage's dependency
# layer survives a change to the code (and, since cargo-chef masks the workspace's own versions, a release bump).
FROM chef AS planner
COPY . .
RUN cargo chef prepare --recipe-path recipe.json

FROM chef AS builder
COPY --from=planner /src/recipe.json recipe.json
RUN cargo chef cook --release --locked --package openbilling --recipe-path recipe.json
COPY . .
RUN cargo build --release --locked --package openbilling --bin openbilling \
    && install -D -m 0755 target/release/openbilling /out/openbilling

# The binary, run where it will run: the runtime base, as the runtime user. Exec form, since distroless has no
# shell. A binary that cannot start (a missing library, the wrong architecture) fails the build here.
FROM ${RUNTIME_IMAGE} AS smoke
COPY --from=builder /out/openbilling /usr/local/bin/openbilling
USER 65532:65532
RUN ["/usr/local/bin/openbilling", "--version"]
RUN ["/usr/local/bin/openbilling", "--help"]

# The published image. Last, so it is the default target.
FROM ${RUNTIME_IMAGE} AS runtime
COPY --from=builder /out/openbilling /usr/local/bin/openbilling
USER 65532:65532
# OPENBILLING_LISTEN defaults to 127.0.0.1:8080 (this machine only, until callers are authenticated by a real
# issuer: bin/openbilling/README.md). The image keeps that default; a deployment that must be reached sets it, as
# the chart does (deploy/openbilling).
EXPOSE 8080
ENTRYPOINT ["/usr/local/bin/openbilling"]
CMD ["serve"]
