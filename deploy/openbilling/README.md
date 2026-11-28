# openbilling Helm chart

The chart of openbilling-rs: one Deployment running `openbilling serve`, its Service, and, off by default, a
NetworkPolicy and a ServiceMonitor. It is values for the [bjw-s common library](https://bjw-s-labs.github.io/helm-charts/docs/app-template/)
**5.3.0**, the library app-template is, rendered with app-template's loader. The decision is
[ADR 0005](../../docs/adr/0005-release-fixed-versions-image-and-chart.md).

**Status: published, not yet run in a cluster.** The chart is linted, rendered over a values matrix and checked against
the Kubernetes schemas in CI. Releases v0.1.1 to v0.4.0 are published and the 0.3.0 chart pulled anonymously
(*verified 2026-11-27*); v0.4.0 was releasing that day. Nothing here has been installed in a cluster, and the
signatures were not verified by hand.

## Install

Each release publishes the chart, signed, at `oci://ghcr.io/vaam-apps/charts/openbilling`, at the release's version.
Its default image is that release, **by tag and digest**. There is no `latest`.

```sh
helm install billing oci://ghcr.io/vaam-apps/charts/openbilling --version X.Y.Z \
  --namespace billing --set openbilling.database.existingSecret=billing-db-app
```

The chart holds no secret. It needs an existing Secret with the PostgreSQL 19 database URL under a key
(`postgres://…` or `postgresql://…`), named by `openbilling.database.existingSecret` (default `openbilling-database`)
and `openbilling.database.key` (default `uri`, the key a CloudNativePG cluster's `<cluster>-app` Secret uses).

Verify a release's chart and image before trusting them:

```sh
cosign verify ghcr.io/vaam-apps/charts/openbilling:X.Y.Z \
  --certificate-identity-regexp '^https://github.com/vaam-apps/openbilling-rs/\.github/workflows/release\.yml@refs/tags/v' \
  --certificate-oidc-issuer https://token.actions.githubusercontent.com
cosign verify ghcr.io/vaam-apps/openbilling-rs:X.Y.Z \
  --certificate-identity-regexp '^https://github.com/vaam-apps/openbilling-rs/\.github/workflows/release\.yml@refs/tags/v' \
  --certificate-oidc-issuer https://token.actions.githubusercontent.com
```

From a clone (development), build the dependency first: `helm dependency build deploy/openbilling`.

## What it runs

| Object | What |
|---|---|
| ConfigMap | The configuration file, from `openbilling.config`, mounted into the Deployment |
| Deployment | `openbilling serve` as uid 65532, read-only root, no capabilities, `RuntimeDefault` seccomp, no service account token. It applies the migrations at start (they are in the binary, under sqlx's advisory lock), then listens on `0.0.0.0:8080` |
| Probes | startup and liveness on `/healthz` (the process answers; the startup probe allows 5 minutes for the migrations), readiness on `/readyz` (the database answers) |
| Service | `ClusterIP`, port `8080`, named `http` |
| ServiceAccount | Its own, with no token mounted |
| NetworkPolicy | Off. When on: ingress from the namespace on 8080; egress to DNS, PostgreSQL (5432) and HTTPS (443, the payment provider). Narrow it to your cluster |
| OpenTelemetry | Off. `openbilling.otel.endpoint` turns it on (the collector's base URL); see *OpenTelemetry* below |
| Dashboards | Off. `configMaps.dashboards.enabled: true` ships the two Grafana dashboards as a ConfigMap labelled `grafana_dashboard: "1"`, for Grafana's sidecar |
| ServiceMonitor | Off, and of no use: openbilling serves no `/metrics`, it pushes OTLP (above). Needs the Prometheus Operator's CRDs |

There is no migration Job: the binary has no command that only migrates, and `serve` does it safely at start with any
number of replicas. Without `openbilling.auth` every `/v1` call answers `401` (`RefuseAll`,
[ADR 0003](../../docs/adr/0003-credits-from-a-payment-provider.md)); with an issuer and an audience, `/v1` takes the
bearer JWTs that issuer signs for that audience. `/healthz`, `/readyz` and the payment provider's webhook answer
either way.

## Values

Everything is the common library's own values (`controllers`, `service`, `networkpolicies`, `serviceMonitor`, …;
see [values.yaml](values.yaml)), plus:

| Value | Default | What |
|---|---|---|
| `openbilling.database.existingSecret` | `openbilling-database` | The existing Secret with the database URL. Required |
| `openbilling.database.key` | `uri` | Its key |
| `openbilling.auth.issuer` | empty | The token issuer, exactly as `iss` names it (`https`, or `http` to a loopback address). Empty: every `/v1` call is a `401` |
| `openbilling.auth.audience` | empty | The audience a token's `aud` must hold. Set with `issuer`, or neither |
| `openbilling.auth.jwksUrl` | empty | The issuer's JWKS URL, in place of its discovery document |
| `openbilling.auth.rolesClaim` | empty (`roles`) | The claim with the roles, a dotted path (`realm_access.roles`) |
| `openbilling.auth.roleMap` | empty | `value=operator,value=producer`: the issuer's role values for openbilling's two roles |
| `openbilling.config` | `allowed:` with `debt_limit`, `balance` and `member_cap` all `at_or_above`, and `retention:` with `margin: 90d` and `timeout: 5m` | The service's configuration file, policy only: rendered as YAML into the ConfigMap, mounted read-only at `/etc/openbilling/config.yaml` and named by `OPENBILLING_CONFIG`; its checksum is on the pod, so a change rolls it. Each check `at_or_above`, `above` or `never` (report only); `retention.margin` is how long after the 7-day intake window a day's usage partition is dropped, `1d` to `3650d` (the owner's decision of 2026-11-28), and `retention.timeout` the longest a retention run waits for a lock; `payments.unrecorded` (`refuse`, the default of the binary, or `credit`; **upgrading with top-ups in flight: set `credit` until the ones started before the upgrade have finished, at most `reconcile.stop_after`, then `refuse`**; a top-up marked mismatched that is right at the provider is recovered today by an operator adjustment credit, a procedure for it is a follow-up) and `reconcile` (`pending_after`, `read_again_after`, `stop_after`, `last_read_for` (`1m` to `30d`, at least `read_again_after`), `every`, `batch`; none set by default, so the binary's defaults stand) are accepted too, see [`bin/openbilling`](../../bin/openbilling/README.md) |
| `openbilling.otel.endpoint` | empty (off) | The OTLP/HTTP collector's base URL: `http(s)://host[:port][/path]`, no user or password |
| `openbilling.otel.environment` | empty | `deployment.environment.name` of every signal |
| `controllers.main.containers.main.image.tag` | `{{ .Chart.AppVersion }}` | A fixed version |
| `controllers.main.containers.main.image.digest` | empty in git; the release's digest in a published chart | `sha256:…` |

The vpay adapter's settings (`OPENBILLING_VPAY_*`, `bin/openbilling/README.md`) go under
`controllers.main.containers.main.env`, the files they name mounted with `persistence` from Secrets of your own: the
webhook secrets, and either the token file or, for a token the service mints, the client's private key
(`OPENBILLING_VPAY_CLIENT_KEY_FILE`). The chart renders none of them itself, so it needs no new value; the
template's refusals do not know them.

## OpenTelemetry

`openbilling.otel.endpoint` sets `OTEL_EXPORTER_OTLP_ENDPOINT`; empty leaves OpenTelemetry off. A collector that needs a
credential gets it as a header **from a file**: mount a Secret of your own with `persistence` (`type: secret`) and set
`OPENBILLING_OTEL_HEADERS_FILE` under `controllers.main.containers.main.env`; the chart never holds a credential, and
`OTEL_EXPORTER_OTLP_HEADERS` is refused by the binary. Egress to the collector's port needs a rule of its own in
`networkpolicies.main.rules.egress` when the policy is on: the render is refused when none allows the endpoint's port (`tests/values/otel.yaml` shows one). What is exported, and what never is, is in
[`openbilling-telemetry`](../../crates/openbilling-telemetry/README.md).

## Dashboards

`deploy/dashboards/` is the canonical copy ([its README](../dashboards/README.md) explains each panel); `dashboards/`
in this chart is a copy a chart can read (Helm cannot read outside the chart). Both are **generated** by
`tools/dashboards/generate.sh` (Python, standard library only), which writes the two copies identically; CI runs it and
fails on any difference, and `crates/openbilling-telemetry/tests/dashboards.rs` fails when they differ. Edit
`tools/dashboards/`, never the JSON. The ConfigMap reads them with `.Files.Get`; its first
render is CI's (*unverified*: no Helm was available when it was written).

## Alert rules

`prometheusRule.enabled: true` renders [`deploy/alerts`](../alerts/README.md) as a `PrometheusRule` for the Prometheus Operator (its CRDs are the cluster's); `prometheusRule.labels` are added to the object for a Prometheus whose `ruleSelector` asks for some. Off by default. `alerts/` in this chart is a copy a chart can read, **generated** with the dashboards by `tools/dashboards/generate.sh` from `tools/dashboards/thresholds.toml`; a test and CI hold it equal. A deployer's own thresholds: `python3 tools/dashboards/alerts.py --thresholds mine.toml --out DIR`, then ship that file instead. The render is checked by `tests/check.sh` (`tests/values/alerts.yaml`; kubeconform on the vendored `PrometheusRule` schema).

**Refused** (the render fails, naming the value): an empty `existingSecret` or `key`; an `otel.endpoint` that is not an http(s) URL or holds a credential, or, with the NetworkPolicy on, whose port no egress rule allows; an `auth.issuer` without an
`auth.audience` or the reverse, `jwksUrl`, `rolesClaim` or `roleMap` without both, an issuer that is not `https` or
loopback `http`; in `openbilling.config`, a key other than `allowed`, `retention`, `vpay`, `payments` and `reconcile`, a `payments` key other than `unrecorded` or a value other than `refuse` and `credit`, a `reconcile` key other than its five, a span outside its range or not written like `15m`, `2h` or `7d`, a `batch` outside 1 to 1000, a `stop_after` not longer than `pending_after`, a `retention` key other than `margin`, a margin that is not days written like `90d` from `1d` to `3650d` (no leading zero), a timeout that is not `1s` to `1h` like `5m`, a check other than `debt_limit`, `balance` and `member_cap`, or
a mode other than `at_or_above`, `above` and `never` (the binary refuses the same at start); an image tagged `latest`; a tag
that is not a fixed version (`X.Y.Z`, a pre-release of one, or `sha-<7>`) without a digest; a digest that is not
`sha256:` and 64 hex digits.

## Checks

```sh
sh deploy/openbilling/tests/check.sh    # helm (v4) and kubeconform on PATH; the network once
```

| File | What |
|---|---|
| `tests/check.sh` | The versions of `Cargo.toml` and `Chart.yaml` agree; the common library's archive is the pinned sha256; `helm lint --strict`; `helm template` of each accepted values file, then `render-check.sh` and kubeconform (strict, Kubernetes 1.35.0) on it; the defaults render the issuer and the audience empty (the service refuses every `/v1` call); the configuration file of the defaults is every check `at_or_above` with a retention margin of `90d`, a timeout of `5m` and `full.yaml`'s its own (`payments.yaml` is accepted too, and handed to the binary), each mounted where `OPENBILLING_CONFIG` says with the ConfigMap's checksum on the pod, and, with `OPENBILLING_BIN` naming a built binary, accepted by `openbilling serve`; each refused file
refused with its message. CI's `chart` job runs it |
| `tests/render-check.sh` | On a render: no Secret, no credential in a URL, `OPENBILLING_DATABASE_URL` from a Secret, every image a fixed version or a digest, `--require-digest` for a digest on every image |
| `tests/values/` | `full.yaml` (everything on, the token verifier and a configuration file that is not the defaults included), `edge-with-digest.yaml`, `otel.yaml` (OpenTelemetry and the dashboards' ConfigMap on), `alerts.yaml` (the PrometheusRule on), and `refused-*.yaml`, each with the message it must be refused with on its `# expect:` line |
| `tests/schemas/` | The ServiceMonitor and PrometheusRule JSON schemas, vendored from datreeio/CRDs-catalog at `b50f3fb` (ADR 0005, *Agent notes*) |

`Chart.yaml`'s `version` and `appVersion` are written by release-please. Never edit them by hand.
