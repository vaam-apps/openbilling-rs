# openbilling

The openbilling-rs service. A binary only composes the crates (agent guide, rule 4); it uses `anyhow` for its
errors and clap for its command line.

```sh
cargo run -p openbilling -- --help
OPENBILLING_DATABASE_URL=postgres://… cargo run -p openbilling -- serve
```

| Command | What |
|---|---|
| `serve` | Connects to PostgreSQL 19 or later, applies the migrations, prepares partitions and CrateStack's idempotency table, then serves the API and the payment provider's webhook ([`openbilling-http`](../../crates/openbilling-http/README.md)) and runs the drain (on `LISTEN/NOTIFY`, polling every 5 s) and the partition job (hourly) until Ctrl-C (`SIGINT`) or `SIGTERM`, the signal Kubernetes stops a pod with; then it stops accepting connections and finishes the requests in flight |
| `--help`, `-h`; `--version`, `-V` | Answer and exit 0 |

No command, or an unknown one, is a usage error: exit 2. A refusal of `serve` (a missing variable, a database that
does not answer) is exit 1, printed as `error: …`.

**Configuration** is the environment, listed in `--help`: `OPENBILLING_DATABASE_URL` or
`OPENBILLING_DATABASE_URL_FILE` (never printed), `OPENBILLING_LISTEN` (default `127.0.0.1:8080`, this machine
only), the token verifier's `OPENBILLING_AUTH_ISSUER` and `OPENBILLING_AUTH_AUDIENCE` (both or neither; neither:
every `/v1` call is a `401`, and `serve` says so on start) with the optional `OPENBILLING_AUTH_JWKS_URL`,
`OPENBILLING_AUTH_ROLES_CLAIM` and `OPENBILLING_AUTH_ROLE_MAP` ([`openbilling-jwt`](../../crates/openbilling-jwt/README.md)),
and the vpay adapter's `OPENBILLING_VPAY_*` (all of them, or none: without them top-ups and webhooks answer `503`).

**Colour.** The help is coloured by clap's `Styles` (headers bold, literals cyan, placeholders dimmed), and the
status lines and refusals by `anstream` and `anstyle`. Both write plain text when the stream is not a terminal,
honour `NO_COLOR`, and colour a pipe under `CLICOLOR_FORCE=1`; `tests/cli.rs` holds all three.

## Tests

`cargo test -p openbilling`: the command line (clap's own check, `serve`, usage errors, exit codes), the
configuration from a lookup (the database URL needed once and never shown, the issuer and the audience together or
not at all, vpay whole or not at all), and the
built binary run as a process (`tests/cli.rs`: `--version`, `--help`, colour, usage errors, `serve` without a
database, and `serve` stopping with exit 0 on `SIGTERM`, which needs `OPENBILLING_TEST_DATABASE_URL` and makes its own
database).
