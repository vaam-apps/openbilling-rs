# Try a published release in k3d

`run.sh` installs a **published** openbilling release (the image and the Helm chart from ghcr.io, not your checkout) in
a throwaway [k3d](https://k3d.io) cluster on your Linux machine and checks that it answers. It is the same check as
`release-check.yml`, which runs in kind on GitHub's runners, made runnable by you.

```sh
sh tools/k3d/run.sh                 # the version in .release-please-manifest.json
sh tools/k3d/run.sh 0.5.0 --keep    # a version, and keep the cluster to look at it
```

| Option | Meaning |
|---|---|
| `VERSION` | `X.Y.Z`, a published release (default: the manifest's) |
| `--reuse` | use the cluster `openbilling-smoke` if it exists; without it an existing cluster is refused, never overwritten |
| `--keep` | do not delete the cluster at the end (a reused cluster is deleted too unless you pass this) |
| `--port N` | the local port of the port-forward (default 18080) |

## You need

`docker` (running), `k3d`, `kubectl`, `helm` **v4**, `curl` and `jq`. `cosign` is optional: with it the image and the
chart signatures are verified against the identity `release.yml` signs with; without it the script warns and goes on.
The script installs nothing.

## What it does

1. Resolves the image and the chart for the version anonymously to digests.
2. `cosign verify` of both (skipped without cosign).
3. Creates the k3d cluster `openbilling-smoke` (k3s v1.37.2, traefik and servicelb off), and checks the node runs the
   pinned k3s digest. Every `kubectl` and `helm` call names the context `k3d-openbilling-smoke`; your current context
   is never used or changed.
4. Runs PostgreSQL 19.0 in namespace `billing` (the image and digest of `release-check.yml` and CI) and the database
   Secret.
5. `helm install` of `oci://ghcr.io/vaam-apps/charts/openbilling` at the version: the pulled chart's digest must be the
   resolved one, and the pod must run the resolved image by digest.
6. Through a port-forward: `/healthz` and `/readyz` answer 200, and an unauthenticated `/v1` call answers 401 (no
   issuer is configured, so every call is refused).

It prints `PASS`, `FAIL` or `SKIP` per step and `RESULT: PASS` or `RESULT: FAIL`, and the exit code follows.

## Pins

| What | Pin | Source |
|---|---|---|
| k3s | `rancher/k3s:v1.37.2-k3s1`, index digest `sha256:21ea6e0f…c4332` | Docker Hub's API, *verified 2026-11-28* |
| PostgreSQL | `postgres:19.0@sha256:4ea94de5…c00e` | as `release-check.yml` (*verified 2026-11-27*) |
| k3d, kubectl, helm, cosign | not pinned: they are yours | k3d's release could not be read from where this was written (*unverified*); the script was developed against k3d 5.x flags |

## Not done: `--with-auth`

There is no `--with-auth`. A token written to an authenticated `/v1` call needs an issuer whose address the pod can
reach and whose keys the service trusts, and the repository has none that runs on its own: `openbilling-jwt`'s
`test-issuer` is a library inside the test binaries (feature `test-issuer`, loopback only, no `main` and no example),
and a service configured with `http://127.0.0.1:…` would look inside its own pod. What is missing is a small binary
(or example) that serves the discovery document, the JWKS and a token on a port, plus an in-cluster Deployment for it
and the chart's `openbilling.auth.issuer` and `audience` values pointed at its Service (the chart accepts `http` only
to a loopback address, so that needs `jwksUrl` or an `https` issuer too). That is new code, so it is left out rather
than faked. The authenticated path is proven by `cargo test -p openbilling --test e2e`.

## Status

*Unverified*: the script passed `shellcheck` and `sh -n` and its argument and tool checks ran, but it has not yet been
run against k3d (no Docker daemon where it was written). Its steps are those of `release-check.yml`, which passed.
