# Contributing

Thank you for helping. This page is the short version; [CLAUDE.md](CLAUDE.md) holds the rules every change is checked
against, for people and coding agents alike.

## 1. Claim an issue

Pick an open issue and comment that you are taking it, so two people do not do the same work. For anything not
covered by an issue, open one first and say what you want to change and why. A change to a decision needs an ADR in
`docs/adr/` (CLAUDE.md, rule 5).

## 2. Build

You need Rust 1.94 or later and, for the database tests, Docker (or any PostgreSQL 19).

```sh
cargo build --workspace
```

## 3. Test

Run what CI runs:

```sh
cargo fmt --all --check
cargo clippy --workspace --all-targets --locked -- -D warnings
cargo test --workspace --locked
cargo deny check            # if cargo-deny is installed
```

The database test skips unless `OPENBILLING_TEST_DATABASE_URL` is set. To run it:

```sh
docker run --rm -d --name openbilling-pg -p 127.0.0.1:5432:5432 -e POSTGRES_HOST_AUTH_METHOD=trust postgres:19.0
OPENBILLING_TEST_DATABASE_URL=postgres://postgres@127.0.0.1:5432/postgres cargo test --workspace --locked
```

A change to a crate's public API, environment variables or tests updates that crate's `README.md` in the same pull
request.

## 4. Commit

[Conventional Commits](https://www.conventionalcommits.org/): `<type>[(scope)][!]: <description>`, with the types
`feat fix docs style refactor perf test build ci chore revert`. One focused change per commit.

## 5. Open a pull request

Work on a branch and open a pull request against `main`. Say what it changes, why, and how you checked it. CI must be
green before it is merged.

By contributing you agree that your contribution is licensed under Apache-2.0, the licence of this repository.
