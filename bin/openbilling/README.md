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

**Configuration** is one optional YAML file for policy and the environment for secrets and endpoints, both listed in
`--help`. No setting is read from both, so neither overrides the other.

`OPENBILLING_CONFIG` names the file (unset or empty: the defaults). It is checked at start, before any connection:
an unknown key or value, an empty file or one over 64 KiB stops `serve` with exit 1, naming the variable and the
line, never what the file holds. Today it holds the policy of `getAllowed` (the owner's decision of 2026-11-27,
ADR 0001, amended, F1 to F4: the service reports the numbers and the deployer decides how they are judged):

```yaml
allowed:
  debt_limit: at_or_above   # refuse when debt >= limit; above: debt > limit; never: report only
  balance: at_or_above      # refuse when debt >= the drawable credit (net <= 0); above: net < 0; never
  member_cap: at_or_above   # refuse when the member's usage this month >= cap; above; never
```

Each key may be left out (`at_or_above`); every comparison is plain, so under `at_or_above` a limit or cap of zero
refuses even with nothing used. `serve` prints the policy it runs with. The environment: `OPENBILLING_DATABASE_URL` or
`OPENBILLING_DATABASE_URL_FILE` (never printed), `OPENBILLING_LISTEN` (default `127.0.0.1:8080`, this machine
only), the token verifier's `OPENBILLING_AUTH_ISSUER` and `OPENBILLING_AUTH_AUDIENCE` (both or neither; neither:
every `/v1` call is a `401`, and `serve` says so on start; an empty value is unset) with the optional `OPENBILLING_AUTH_JWKS_URL`,
`OPENBILLING_AUTH_ROLES_CLAIM` and `OPENBILLING_AUTH_ROLE_MAP` ([`openbilling-jwt`](../../crates/openbilling-jwt/README.md)),
and the vpay adapter's `OPENBILLING_VPAY_*` (all of them, or none: without them top-ups and webhooks answer `503`).

OpenTelemetry is configured by the standard `OTEL_*` variables (**off unless `OTEL_EXPORTER_OTLP_ENDPOINT` is set**; the
[crate's README](../../crates/openbilling-telemetry/README.md) lists them and what is refused). `serve` starts the
exporter first, so the start-up work is traced, reads the queue and pool gauges every 15 s, wraps each drain pass and
partition run in a span, counts the loops' errors by class, and flushes the exporter for up to 5 s when it stops.

**Colour.** The help is coloured by clap's `Styles` (headers bold, literals cyan, placeholders dimmed), and the
status lines and refusals by `anstream` and `anstyle`. Both write plain text when the stream is not a terminal,
honour `NO_COLOR`, and colour a pipe under `CLICOLOR_FORCE=1`; `tests/cli.rs` holds all three.

## Tests

`cargo test -p openbilling`: the command line (clap's own check, `serve`, usage errors, exit codes), the
configuration from a lookup (the database URL needed once and never shown, the issuer and the audience together or
not at all, vpay whole or not at all, the configuration file's comparisons and its refusals, none quoting the file),
and the built binary run as a process (`tests/cli.rs`: `--version`, `--help`, colour, usage errors, `serve` without a
database, `serve` refusing a configuration file it does not understand, and `serve` stopping with exit 0 on `SIGTERM`, which needs `OPENBILLING_TEST_DATABASE_URL` and makes its own
database), and the end-to-end suite, this package's `e2e` target ([`e2e/`](../../e2e/README.md)): `serve` against a
fresh PostgreSQL 19 database, driven through its API. It needs `OPENBILLING_TEST_DATABASE_URL` and skips without it.
