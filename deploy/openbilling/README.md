# openbilling Helm chart

The chart of openbilling-rs: one Deployment running `openbilling serve`, its Service, and, off by default, a
NetworkPolicy and a ServiceMonitor. It is values for the [bjw-s common library](https://bjw-s-labs.github.io/helm-charts/docs/app-template/)
**5.3.0**, the library app-template is, rendered with app-template's loader. The decision is
[ADR 0005](../../docs/adr/0005-release-fixed-versions-image-and-chart.md).

**Status: not yet run in a cluster.** The chart is linted, rendered over a values matrix and checked against the
Kubernetes schemas in CI; no release has been published yet, and nothing here has been installed anywhere.

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
| Deployment | `openbilling serve` as uid 65532, read-only root, no capabilities, `RuntimeDefault` seccomp, no service account token. It applies the migrations at start (they are in the binary, under sqlx's advisory lock), then listens on `0.0.0.0:8080` |
| Probes | startup and liveness on `/healthz` (the process answers; the startup probe allows 5 minutes for the migrations), readiness on `/readyz` (the database answers) |
| Service | `ClusterIP`, port `8080`, named `http` |
| ServiceAccount | Its own, with no token mounted |
| NetworkPolicy | Off. When on: ingress from the namespace on 8080; egress to DNS, PostgreSQL (5432) and HTTPS (443, the payment provider). Narrow it to your cluster |
| ServiceMonitor | Off, and of no use yet: openbilling serves no `/metrics`, so it would scrape a 404. Needs the Prometheus Operator's CRDs |

There is no migration Job: the binary has no command that only migrates, and `serve` does it safely at start with any
number of replicas. Every `/v1` call answers `401` until a token verifier ships
([ADR 0003](../../docs/adr/0003-credits-from-a-payment-provider.md)); `/healthz`, `/readyz` and the payment
provider's webhook answer.

## Values

Everything is the common library's own values (`controllers`, `service`, `networkpolicies`, `serviceMonitor`, …;
see [values.yaml](values.yaml)), plus:

| Value | Default | What |
|---|---|---|
| `openbilling.database.existingSecret` | `openbilling-database` | The existing Secret with the database URL. Required |
| `openbilling.database.key` | `uri` | Its key |
| `controllers.main.containers.main.image.tag` | `{{ .Chart.AppVersion }}` | A fixed version |
| `controllers.main.containers.main.image.digest` | empty in git; the release's digest in a published chart | `sha256:…` |

The vpay adapter's settings (`OPENBILLING_VPAY_*`, `bin/openbilling/README.md`) go under
`controllers.main.containers.main.env`, the files they name mounted with `persistence` from Secrets of your own.

**Refused** (the render fails, naming the value): an empty `existingSecret` or `key`; an image tagged `latest`; a tag
that is not a fixed version (`X.Y.Z`, a pre-release of one, or `sha-<7>`) without a digest; a digest that is not
`sha256:` and 64 hex digits.

## Checks

```sh
sh deploy/openbilling/tests/check.sh    # helm (v4) and kubeconform on PATH; the network once
```

| File | What |
|---|---|
| `tests/check.sh` | The versions of `Cargo.toml` and `Chart.yaml` agree; the common library's archive is the pinned sha256; `helm lint --strict`; `helm template` of each accepted values file, then `render-check.sh` and kubeconform (strict, Kubernetes 1.35.0) on it; each refused file refused with its message. CI's `chart` job runs it |
| `tests/render-check.sh` | On a render: no Secret, no credential in a URL, `OPENBILLING_DATABASE_URL` from a Secret, every image a fixed version or a digest, `--require-digest` for a digest on every image |
| `tests/values/` | `full.yaml` (everything on), `edge-with-digest.yaml`, and `refused-*.yaml`, each with the message it must be refused with on its `# expect:` line |
| `tests/schemas/` | The ServiceMonitor JSON schema, vendored from datreeio/CRDs-catalog at `b50f3fb` (ADR 0005, *Agent notes*) |

`Chart.yaml`'s `version` and `appVersion` are written by release-please. Never edit them by hand.
