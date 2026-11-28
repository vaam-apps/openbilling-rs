# openbilling

The openbilling-rs service. A binary only composes the crates (agent guide, rule 4); it uses `anyhow` for its
errors and clap for its command line.

```sh
cargo run -p openbilling -- --help
OPENBILLING_DATABASE_URL=postgres://… cargo run -p openbilling -- serve
```

| Command | What |
|---|---|
| `serve` | Connects to PostgreSQL 19 or later, applies the migrations, prepares partitions and CrateStack's idempotency table, then serves the API and the payment provider's webhook ([`openbilling-http`](../../crates/openbilling-http/README.md)) and runs the drain (on `LISTEN/NOTIFY`, polling every 5 s) and the partition job (hourly: it makes partitions ahead, then drops the expired ones, see below) until Ctrl-C (`SIGINT`) or `SIGTERM`, the signal Kubernetes stops a pod with; then it stops accepting connections and finishes the requests in flight |
| `--help`, `-h`; `--version`, `-V` | Answer and exit 0 |

No command, or an unknown one, is a usage error: exit 2. A refusal of `serve` (a missing variable, a database that
does not answer) is exit 1, printed as `error: …`.

**Configuration** is one optional YAML file for policy and the environment for secrets and endpoints, both listed in
`--help`. No setting is read from both, so neither overrides the other.

`OPENBILLING_CONFIG` names the file (unset or empty: the defaults). It is checked at start, before any connection:
an unknown key or value, an empty file or one over 64 KiB stops `serve` with exit 1, naming the variable and the
line, never what the file holds. It holds the policy of `getAllowed` (the owner's decision of 2026-11-27,
ADR 0001, amended, F1 to F4: the service reports the numbers and the deployer decides how they are judged), the
retention margin and timeout, and the tunables of a minted vpay token (below):

```yaml
allowed:
  debt_limit: at_or_above   # refuse when debt >= limit; above: debt > limit; never: report only
  balance: at_or_above      # refuse when debt >= the drawable credit (net <= 0); above: net < 0; never
  member_cap: at_or_above   # refuse when the member's usage this month >= cap; above; never
retention:
  margin: 90d               # drop a day's usage partition this long after the intake window (7 days); 1d to 3650d
  timeout: 5m               # the longest a retention run waits for a lock or runs a statement; 1s to 1h
vpay:
  token:                    # only with a client key (OPENBILLING_VPAY_CLIENT_*); each key optional
    audience: vpay:v1
    scope: payments:write   # default none: vpay grants the client's registered scopes
    refresh_skew_secs: 30   # at most half the token's life, at most 600
    retries: 2              # at most 10
    retry_backoff_ms: 200   # doubling, at most 5000
```

`retention.margin` is how long a day's usage partition outlives the intake window before `serve` detaches and drops it
(the owner's decision of 2026-11-28, ADR 0001, amended, G1 to G5: "drop after W plus a margin, default 90 days,
configurable"); it is written as digits and a `d`, and anything else, `0d`, `3651d`, `90` or `1.5d` included, stops
`serve` at start (no leading zero: `010d` is refused). `timeout` is the `lock_timeout` and `statement_timeout` of the job's connection, because a detach waits for every transaction on the table; a run that times out is logged and retried at the next hour. The job has no interval of its own: it runs in the background, at once and then hourly (so the service never waits for it to listen), with the job that makes
partitions ahead, and two instances take turns through an advisory lock. It logs how many partitions it dropped, never
their names, and counts them in `openbilling.partitions.dropped`. A failure of the job is logged by class and counted
in `openbilling.errors` (`source=retention`); it does not stop the service.

Each key may be left out (`at_or_above`); every comparison is plain, so under `at_or_above` a limit or cap of zero
refuses even with nothing used. `serve` prints the policy it runs with. The environment: `OPENBILLING_DATABASE_URL` or
`OPENBILLING_DATABASE_URL_FILE` (never printed), `OPENBILLING_LISTEN` (default `127.0.0.1:8080`, this machine
only), the token verifier's `OPENBILLING_AUTH_ISSUER` and `OPENBILLING_AUTH_AUDIENCE` (both or neither; neither:
every `/v1` call is a `401`, and `serve` says so on start; an empty value is unset) with the optional `OPENBILLING_AUTH_JWKS_URL`,
`OPENBILLING_AUTH_ROLES_CLAIM` and `OPENBILLING_AUTH_ROLE_MAP` ([`openbilling-jwt`](../../crates/openbilling-jwt/README.md)),
and the vpay adapter's `OPENBILLING_VPAY_*` (all of them, or none: without them top-ups and webhooks answer `503`).
The vpay token is either a file somebody else keeps fresh (`OPENBILLING_VPAY_TOKEN_FILE`) or minted by the service:
`OPENBILLING_VPAY_CLIENT_ID` and `OPENBILLING_VPAY_CLIENT_KEY_FILE` (a PEM file, read at each mint), with the optional
`OPENBILLING_VPAY_CLIENT_KEY_ID`, `_TOKEN_URL` (default `<base URL>/v1/oauth/token`) and `_ASSERTION_AUDIENCE` (default the
token URL): the client's identity, its key and the places it talks to, which are the environment's. How the token is
asked for and kept fresh is the configuration file's `vpay.token` (`audience` `vpay:v1`, `scope` none, `refresh_skew_secs`
30, at most 600, `retries` 2, at most 10, and `retry_backoff_ms` 200, at most 5000), and it has no variable. Both ways at
once, minting settings without a client id and key, and `vpay.token` with a token file or without vpay are refused at
start; no value is printed.

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
