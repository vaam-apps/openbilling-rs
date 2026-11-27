# The openbilling image: a static musl binary, `bin/openbilling`, on distroless/static, non-root (ADR 0005, ADR 0006).
#
# Built from the repository root (the context is `.`):
#
#   docker build -f docker/openbilling/Dockerfile --target smoke .     # build, and run --version and --help in it
#   docker build -f docker/openbilling/Dockerfile .                    # the image (the last stage, `runtime`)
#
# Stages: `chef` (the toolchain and cargo-chef), `planner` (the dependency recipe), `builder` (dependencies in a
# layer of their own, then the binary, `--locked`, for the musl target, and a check that it is static), `smoke` (the
# binary run on the runtime base) and `runtime`.
#
# One build per architecture, on a runner of that architecture (release.yml): the builder is Alpine, whose Rust
# toolchain is musl-native, so `x86_64-unknown-linux-musl` is built on amd64 and `aarch64-unknown-linux-musl` on
# arm64. `--target` is passed even though it is the host's triple: with it, `RUSTFLAGS` (`+crt-static`) does not
# reach build scripts and proc-macros, which must stay dynamic to be loaded by the compiler.
#
# Pins, by tag and digest (the digest wins; the tag says what it is). Move both together; dependabot does not see
# these lines.
#   - rust:1.98.1-alpine3.24, the multi-arch index digest, read from Docker Hub on 2026-11-27 (*verified*:
#     hub.docker.com/v2/repositories/library/rust/tags/1.98.1-alpine3.24). The toolchain CI builds with (ci.yml,
#     RUST_TOOLCHAIN); the MSRV is 1.98 (Cargo.toml).
#   - gcr.io/distroless/static-debian12:nonroot, the multi-arch index digest, read from gcr.io on 2026-11-27
#     (*verified*: the registry's manifest answer, anonymously). No libc, no shell: CA certificates, /etc/passwd with
#     uid 65532, tzdata. Debian 12 because that is the static image's release the owner named (ANR-1173).
#   - cargo-chef 0.1.78, the newest on crates.io on 2026-11-27 (*verified*, published 2026-08-12).
#
# The migrations are compiled into the binary (`openbilling-pg`, `include_str!`), and `openbilling serve` applies
# them at start, under sqlx's advisory lock: the image carries no SQL file.

ARG RUST_IMAGE=rust:1.98.1-alpine3.24@sha256:7cc1c22d77d9432f7fe012a70e6d3e555af54c2a6832700ed7d553f1769ae89f
ARG RUNTIME_IMAGE=gcr.io/distroless/static-debian12:nonroot@sha256:afa5c872c891853ca7fcf1f12c3edb23f7eeef36189728842dd51042ff57f7ab

FROM ${RUST_IMAGE} AS chef
ARG CARGO_CHEF_VERSION=0.1.78
ARG TARGETARCH
# The image already has gcc and musl-dev, which `ring` needs to compile its C and assembly. `file`: the static check.
RUN apk add --no-cache file \
    && cargo install cargo-chef --locked --version "${CARGO_CHEF_VERSION}" \
    && case "${TARGETARCH}" in \
         amd64) echo x86_64-unknown-linux-musl ;; \
         arm64) echo aarch64-unknown-linux-musl ;; \
         *) echo "unsupported architecture: ${TARGETARCH}" >&2; exit 1 ;; \
       esac > /rust-target \
    && rustup target add "$(cat /rust-target)"
ENV RUSTFLAGS="-C target-feature=+crt-static"
WORKDIR /src

# The recipe is the workspace's manifests and lockfile with the sources stripped, so the next stage's dependency
# layer survives a change to the code (and, since cargo-chef masks the workspace's own versions, a release bump).
FROM chef AS planner
COPY . .
RUN cargo chef prepare --recipe-path recipe.json

FROM chef AS builder
COPY --from=planner /src/recipe.json recipe.json
RUN cargo chef cook --release --locked --target "$(cat /rust-target)" --package openbilling --recipe-path recipe.json
COPY . .
RUN cargo build --release --locked --target "$(cat /rust-target)" --package openbilling --bin openbilling \
    && install -D -m 0755 "target/$(cat /rust-target)/release/openbilling" /out/openbilling
# A static executable has no interpreter and no shared library: `file` says "statically linked" (or "static-pie
# linked") and never "dynamically linked". A build step, so a dependency that starts to need libc fails here.
RUN out="$(file -b /out/openbilling)"; echo "${out}" \
    && case "${out}" in *"dynamically linked"*) echo "not static" >&2; exit 1 ;; *"static"*) ;; *) echo "cannot tell if it is static" >&2; exit 1 ;; esac

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
