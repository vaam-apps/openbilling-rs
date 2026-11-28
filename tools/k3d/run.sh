#!/bin/sh
# Tests a PUBLISHED release of openbilling in a k3d cluster on your own machine (see README.md).
# What it does, in order: resolve the image and the chart anonymously, cosign-verify them (when cosign is installed),
# create the cluster `openbilling-smoke`, run PostgreSQL 19.0 in it, `helm install` the published chart, and ask
# /healthz, /readyz and an unauthenticated /v1 call through a port-forward. It prints PASS or FAIL per step.
set -eu

CLUSTER=openbilling-smoke
CONTEXT="k3d-${CLUSTER}"
NS=billing
RELEASE=billing
IMAGE_REPO=vaam-apps/openbilling-rs
CHART_REPO=vaam-apps/charts/openbilling
COSIGN_OIDC_ISSUER=https://token.actions.githubusercontent.com
COSIGN_IDENTITY_PREFIX=https://github.com/vaam-apps/openbilling-rs/.github/workflows/release.yml@refs/tags/v
# The same PostgreSQL 19.0 release-check.yml and CI's Test job use, by tag and digest.
POSTGRES_IMAGE='postgres:19.0@sha256:4ea94de574c17f0372722806c949757050fbb1cd7648410fe45d6bba275cc00e'
# k3s v1.37.2 (Kubernetes 1.37, the chart's tested minor): the tag is given to k3d, the index digest is checked after
# the cluster is up. Both read from Docker Hub's API for rancher/k3s on 2026-11-28 (verified 2026-11-28).
K3S_TAG=v1.37.2-k3s1
K3S_DIGEST=sha256:21ea6e0fac4e3930c2ce8aa40ddd41a8b18cb3e9a0222c030dc005e7e88c4332
K3S_IMAGE="rancher/k3s:${K3S_TAG}"
LOCAL_PORT=18080

usage() {
  cat <<USAGE
Usage: sh tools/k3d/run.sh [VERSION] [--reuse] [--keep] [--port N]

  VERSION   the published release to test, X.Y.Z (default: .release-please-manifest.json)
  --reuse   use the cluster ${CLUSTER} if it exists (otherwise a cluster that exists is refused)
  --keep    do not delete the cluster at the end
  --port N  the local port of the port-forward (default ${LOCAL_PORT})
USAGE
}

VERSION=
REUSE=0
KEEP=0
while [ $# -gt 0 ]; do
  case "$1" in
    --reuse) REUSE=1 ;;
    --keep) KEEP=1 ;;
    --port) shift; LOCAL_PORT="${1:?--port needs a number}" ;;
    -h|--help) usage; exit 0 ;;
    -*) echo "unknown option: $1" >&2; usage >&2; exit 2 ;;
    *) [ -z "$VERSION" ] || { echo "one version only" >&2; exit 2; }; VERSION="$1" ;;
  esac
  shift
done

ROOT=$(cd "$(dirname "$0")/../.." && pwd)
if [ -z "$VERSION" ]; then
  VERSION=$(sed -n 's/.*"\.": *"\([0-9][0-9.]*\)".*/\1/p' "$ROOT/.release-please-manifest.json" | head -1)
fi
case "$VERSION" in
  [0-9]*.[0-9]*.[0-9]*) : ;;
  *) echo "'$VERSION' is not X.Y.Z" >&2; exit 2 ;;
esac
case "$VERSION" in *[!0-9.]*) echo "'$VERSION' is not X.Y.Z" >&2; exit 2 ;; esac

# --- tools ------------------------------------------------------------------------------------------------------
missing=0
need() { command -v "$1" >/dev/null 2>&1 || { echo "missing: $1 ($2)" >&2; missing=1; }; }
need k3d "https://k3d.io, and a running Docker"
need kubectl "https://kubernetes.io/docs/tasks/tools/"
need helm "v4: https://helm.sh/docs/intro/install/"
need docker "k3d runs its nodes in Docker"
need curl "to resolve the published digests"
need jq "to read the registry's answers"
[ "$missing" = 0 ] || exit 2
if ! helm version --template '{{.Version}}' 2>/dev/null | grep -q '^v4\.'; then
  echo "helm v4 is required (found: $(helm version --short 2>/dev/null || echo unknown))" >&2
  exit 2
fi
docker info >/dev/null 2>&1 || { echo "the Docker daemon is not reachable" >&2; exit 2; }
HAVE_COSIGN=0
if command -v cosign >/dev/null 2>&1; then
  HAVE_COSIGN=1
else
  echo "warning: cosign is not installed; the image and chart signatures will NOT be verified" >&2
fi

WORK=$(mktemp -d)
RESULTS=""
FAILED=0
PF_PID=""
CREATED=0

record() { # PASS|FAIL|SKIP name
  RESULTS="${RESULTS}$1  $2
"
  [ "$1" != FAIL ] || FAILED=1
}

cleanup() {
  [ -z "$PF_PID" ] || kill "$PF_PID" 2>/dev/null || true
  if [ "$CREATED" = 1 ] && [ "$KEEP" = 0 ]; then
    echo "deleting the cluster ${CLUSTER}" >&2
    k3d cluster delete "$CLUSTER" >/dev/null 2>&1 || true
  elif [ "$CREATED" = 1 ]; then
    echo "kept the cluster ${CLUSTER} (context ${CONTEXT}); delete it with: k3d cluster delete ${CLUSTER}" >&2
  fi
  rm -rf "$WORK"
}
finish() {
  printf '\nSummary for openbilling %s\n%s' "$VERSION" "$RESULTS"
  if [ "$FAILED" = 0 ]; then echo "RESULT: PASS"; else echo "RESULT: FAIL"; fi
}
trap 'cleanup' EXIT

# Every kubectl and helm call names the cluster's context: the current context is never trusted.
kc() { kubectl --context "$CONTEXT" "$@"; }
hm() { helm --kube-context "$CONTEXT" "$@"; }

# step NAME CMD...: runs CMD (a shell function), records PASS or FAIL, stops at the first failure.
step() {
  name="$1"; shift
  echo "== ${name}" >&2
  if ( "$@" ); then record PASS "$name"; else record FAIL "$name"; finish; exit 1; fi
}

# --- 1. what was published ---------------------------------------------------------------------------------------
resolve() { # repository tag -> digest, anonymously
  accept='application/vnd.oci.image.index.v1+json, application/vnd.oci.image.manifest.v1+json, application/vnd.docker.distribution.manifest.v2+json, application/vnd.docker.distribution.manifest.list.v2+json'
  token=$(curl -fsS "https://ghcr.io/token?scope=repository:$1:pull" | jq -er .token) || return 1
  d=$(curl -fsSI -H "Authorization: Bearer ${token}" -H "Accept: ${accept}" "https://ghcr.io/v2/$1/manifests/$2" \
    | tr -d '\r' | sed -n 's/^[Dd]ocker-[Cc]ontent-[Dd]igest: *//p' | head -1)
  case "$d" in sha256:????????????????????????????????????????????????????????????????) echo "$d" ;; *) echo "$1:$2 did not resolve anonymously to a digest" >&2; return 1 ;; esac
}
do_resolve() {
  resolve "$IMAGE_REPO" "$VERSION" > "$WORK/image.digest"
  resolve "$CHART_REPO" "$VERSION" > "$WORK/chart.digest"
  echo "image ghcr.io/${IMAGE_REPO}@$(cat "$WORK/image.digest")" >&2
  echo "chart oci://ghcr.io/${CHART_REPO}@$(cat "$WORK/chart.digest")" >&2
}
step "resolve the image and the chart anonymously" do_resolve
IMAGE_D=$(cat "$WORK/image.digest")
CHART_D=$(cat "$WORK/chart.digest")

if [ "$HAVE_COSIGN" = 1 ]; then
  do_cosign() {
    for ref in "ghcr.io/${IMAGE_REPO}@${IMAGE_D}" "ghcr.io/${CHART_REPO}@${CHART_D}"; do
      cosign verify "$ref" \
        --certificate-identity "${COSIGN_IDENTITY_PREFIX}${VERSION}" \
        --certificate-oidc-issuer "$COSIGN_OIDC_ISSUER" >/dev/null
    done
  }
  step "cosign verify the image and the chart" do_cosign
else
  record SKIP "cosign verify the image and the chart (cosign not installed)"
fi

# --- 2. the cluster ----------------------------------------------------------------------------------------------
exists=0
k3d cluster list -o json 2>/dev/null | jq -e --arg n "$CLUSTER" 'any(.[]; .name == $n)' >/dev/null 2>&1 && exists=1
do_cluster() {
  if [ "$exists" = 1 ]; then
    [ "$REUSE" = 1 ] || { echo "the cluster ${CLUSTER} exists; use --reuse, or delete it: k3d cluster delete ${CLUSTER}" >&2; return 1; }
    k3d cluster start "$CLUSTER" >/dev/null 2>&1 || true
    k3d kubeconfig merge "$CLUSTER" --kubeconfig-merge-default >/dev/null
  else
    CREATED=1
    k3d cluster create "$CLUSTER" --image "$K3S_IMAGE" --servers 1 --agents 0 --no-lb \
      --k3s-arg '--disable=traefik@server:*' --k3s-arg '--disable=servicelb@server:*' \
      --kubeconfig-update-default=true --kubeconfig-switch-context=false --wait --timeout 240s
  fi
  kc get nodes >/dev/null
}
echo "== create or reuse the k3d cluster ${CLUSTER}" >&2
if do_cluster; then record PASS "k3d cluster ${CLUSTER}"; else
  record FAIL "k3d cluster ${CLUSTER}"; finish; exit 1
fi
if [ "$exists" = 1 ]; then CREATED=1; fi  # a reused cluster is deleted at the end too, unless --keep
do_k3s_digest() {
  img=$(docker inspect --format '{{.Config.Image}}' "k3d-${CLUSTER}-server-0" 2>/dev/null || true)
  [ "$img" = "$K3S_IMAGE" ] || { echo "the cluster runs '${img}', not ${K3S_IMAGE}" >&2; return 1; }
  docker image inspect --format '{{json .RepoDigests}}' "$K3S_IMAGE" | grep -q "$K3S_DIGEST" \
    || { echo "${K3S_IMAGE} is not the pinned digest ${K3S_DIGEST}" >&2; return 1; }
}
step "the node image is the pinned k3s" do_k3s_digest

# --- 3. PostgreSQL 19.0 and the database Secret ------------------------------------------------------------------
do_postgres() {
  if hm -n "$NS" status "$RELEASE" >/dev/null 2>&1; then hm -n "$NS" uninstall "$RELEASE" --wait >/dev/null; fi
  kc delete namespace "$NS" --ignore-not-found --wait >/dev/null
  kc create namespace "$NS" >/dev/null
  kc -n "$NS" create secret generic openbilling-database \
    --from-literal=uri='postgres://postgres:check@postgres.billing.svc:5432/postgres' >/dev/null
  kc -n "$NS" apply -f - >/dev/null <<MANIFEST
apiVersion: apps/v1
kind: Deployment
metadata: {name: postgres}
spec:
  replicas: 1
  selector: {matchLabels: {app: postgres}}
  template:
    metadata: {labels: {app: postgres}}
    spec:
      containers:
        - name: postgres
          image: ${POSTGRES_IMAGE}
          env:
            - {name: POSTGRES_PASSWORD, value: check}
          ports: [{containerPort: 5432}]
          readinessProbe:
            exec: {command: [pg_isready, -U, postgres]}
            periodSeconds: 2
---
apiVersion: v1
kind: Service
metadata: {name: postgres}
spec:
  selector: {app: postgres}
  ports: [{port: 5432}]
MANIFEST
  kc -n "$NS" rollout status deployment/postgres --timeout=240s
}
step "PostgreSQL 19.0 in the cluster" do_postgres

# --- 4. the published chart --------------------------------------------------------------------------------------
do_install() {
  hm pull "oci://ghcr.io/${CHART_REPO}" --version "$VERSION" --destination "$WORK" >"$WORK/pull.log" 2>&1 || { cat "$WORK/pull.log" >&2; return 1; }
  pulled=$(sed -n 's/^Digest: \(sha256:[0-9a-f]\{64\}\)$/\1/p' "$WORK/pull.log" | head -1)
  [ "$pulled" = "$CHART_D" ] || { echo "helm pulled '${pulled:-no digest}', the resolved chart is ${CHART_D}" >&2; return 1; }
  # The chart's own database Secret name and key are its defaults (`openbilling-database`, `uri`).
  hm install "$RELEASE" "$WORK/openbilling-${VERSION}.tgz" --namespace "$NS" --wait --timeout 5m
  img=$(kc -n "$NS" get deploy -l "app.kubernetes.io/instance=${RELEASE}" -o jsonpath='{.items[0].spec.template.spec.containers[0].image}')
  echo "the chart runs ${img}" >&2
  [ "$img" = "ghcr.io/${IMAGE_REPO}:${VERSION}@${IMAGE_D}" ] || { echo "not the resolved image ghcr.io/${IMAGE_REPO}:${VERSION}@${IMAGE_D}" >&2; return 1; }
}
step "helm install of the published chart, running the resolved image" do_install

# --- 5. the service, through a port-forward ----------------------------------------------------------------------
svc=$(kc -n "$NS" get svc -l "app.kubernetes.io/instance=${RELEASE}" -o jsonpath='{.items[0].metadata.name}')
kc -n "$NS" port-forward "svc/${svc}" "${LOCAL_PORT}:8080" >"$WORK/pf.log" 2>&1 &
PF_PID=$!
i=0
while [ "$i" -lt 30 ]; do
  curl -fsS --max-time 5 "http://127.0.0.1:${LOCAL_PORT}/healthz" >/dev/null 2>&1 && break
  i=$((i + 1)); sleep 1
done
code() { curl -s --max-time 10 -o /dev/null -w '%{http_code}' "$@"; }
expect() { # name url want
  got=$(code "$2")
  echo "$2 -> ${got}" >&2
  [ "$got" = "$3" ]
}
step "/healthz is 200" expect healthz "http://127.0.0.1:${LOCAL_PORT}/healthz" 200
step "/readyz is 200" expect readyz "http://127.0.0.1:${LOCAL_PORT}/readyz" 200
# No issuer is configured: every /v1 call is refused, never open.
step "an unauthenticated /v1 call is 401" expect v1 "http://127.0.0.1:${LOCAL_PORT}/v1/usage_outcomes?limit=1" 401

finish
[ "$FAILED" = 0 ]
