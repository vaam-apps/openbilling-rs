#!/bin/sh
# check.sh: every check of the chart, run by CI (ci.yml, job `chart`) and runnable as is on a laptop.
#
#   sh deploy/openbilling/tests/check.sh
#
# Needs helm (v4; CI pins the version) and kubeconform on PATH, and the network once: the common library is pulled
# from ghcr.io, and kubeconform reads the Kubernetes schemas from GitHub. The ServiceMonitor schema is vendored.
#
#   1. the versions agree: Cargo.toml's workspace version, Chart.yaml's version and appVersion;
#   2. the common library is the archive Chart.yaml pins, by sha256;
#   3. helm lint, defaults and every accepted values file;
#   4. helm template of each, then render-check.sh and kubeconform (strict) on the render;
#   5. the defaults: every /v1 call refused, and the configuration file with every check at_or_above and a retention
#      margin of 90d and timeout of 5m, mounted where
#      OPENBILLING_CONFIG says; full.yaml's file is its own;
#   6. every refused values file is refused, with its message.
#
# With OPENBILLING_BIN naming a built `openbilling`, the configuration file of each render is also handed to
# `openbilling serve` (with a database that is never reached), which must accept it: the chart's refusals and the
# binary's agree.

set -eu

here=$(cd "$(dirname "$0")" && pwd)
chart=$(cd "$here/.." && pwd)
root=$(cd "$chart/../.." && pwd)
work=$(mktemp -d)
trap 'rm -rf "$work"' EXIT INT TERM

KUBERNETES_VERSION=${KUBERNETES_VERSION:-1.35.0}
# The sha256 of common-5.3.0.tgz: the layer of oci://ghcr.io/bjw-s-labs/helm/common:5.3.0 and the digest in the
# bjw-s index.yaml (verified 2026-11-27). Move it with Chart.yaml's dependency.
COMMON_ARCHIVE=common-5.3.0.tgz
COMMON_SHA256=d373034b8f8d2d7c25d4028f8280a2112486a8b1a3deb1f4e690cd132ac20f80

step() { printf '\n== %s\n' "$1"; }
die() { printf 'check.sh: %s\n' "$1" >&2; exit 1; }

for tool in helm kubeconform; do
    command -v "$tool" > /dev/null 2>&1 || die "$tool is not on PATH"
done

sha256() {
    if command -v sha256sum > /dev/null 2>&1; then sha256sum "$1" | cut -d' ' -f1
    else shasum -a 256 "$1" | cut -d' ' -f1
    fi
}

step "versions agree"
# `[^ #"]*`, never `.*$`: each line ends in the release-please marker, and a pattern anchored to the end of the line
# would match nothing and pass.
cargo_version=$(sed -n 's/^version = "\([^"]*\)" # x-release-please-version$/\1/p' "$root/Cargo.toml" | head -1)
chart_version=$(sed -n 's/^version: *\([^ #"]*\) # x-release-please-version$/\1/p' "$chart/Chart.yaml" | head -1)
app_version=$(sed -n 's/^appVersion: *"\([^"]*\)" # x-release-please-version$/\1/p' "$chart/Chart.yaml" | head -1)
printf 'Cargo.toml %s, Chart.yaml version %s, appVersion %s\n' "$cargo_version" "$chart_version" "$app_version"
[ -n "$cargo_version" ] || die "Cargo.toml has no workspace version line with its x-release-please-version marker"
[ -n "$chart_version" ] || die "Chart.yaml has no version line with its x-release-please-version marker"
[ -n "$app_version" ] || die "Chart.yaml has no appVersion line with its x-release-please-version marker"
[ "$cargo_version" = "$chart_version" ] || die "Chart.yaml version $chart_version is not Cargo.toml's $cargo_version"
[ "$cargo_version" = "$app_version" ] || die "Chart.yaml appVersion $app_version is not Cargo.toml's $cargo_version"

step "the common library"
if [ ! -f "$chart/charts/$COMMON_ARCHIVE" ]; then
    helm dependency build "$chart"
fi
got=$(sha256 "$chart/charts/$COMMON_ARCHIVE")
[ "$got" = "$COMMON_SHA256" ] || die "charts/$COMMON_ARCHIVE is $got, not the pinned $COMMON_SHA256"
printf '%s sha256:%s\n' "$COMMON_ARCHIVE" "$got"

# name|values file (empty: the defaults)|render-check flags
accepted="defaults||
full|$here/values/full.yaml|--require-digest
edge-with-digest|$here/values/edge-with-digest.yaml|--require-digest
otel|$here/values/otel.yaml|"

printf '%s\n' "$accepted" | while IFS='|' read -r name values flags; do
    set --
    [ -n "$values" ] && set -- -f "$values"

    step "lint: $name"
    helm lint --strict "$chart" "$@"

    step "template: $name"
    helm template openbilling "$chart" --namespace billing "$@" > "$work/$name.yaml"
    # shellcheck disable=SC2086 # $flags is empty or one flag.
    sh "$here/render-check.sh" "$work/$name.yaml" $flags
    kubeconform -strict -summary \
        -kubernetes-version "$KUBERNETES_VERSION" \
        -schema-location default \
        -schema-location "$here/schemas/{{.Group}}/{{.ResourceKind}}_{{.ResourceAPIVersion}}.json" \
        "$work/$name.yaml"
done

step "the defaults keep every /v1 call refused"
# Without `openbilling.auth` the issuer and the audience render empty, which the binary reads as unset: RefuseAll.
helm template openbilling "$chart" --namespace billing > "$work/defaults-auth.yaml"
for name in OPENBILLING_AUTH_ISSUER OPENBILLING_AUTH_AUDIENCE; do
    awk -v want="$name" '
        $0 ~ ("-[[:space:]]+name:[[:space:]]+" want "[[:space:]]*$") {
            seen = 1
            if ((getline line) <= 0 || line !~ /^[[:space:]]+value:[[:space:]]*""[[:space:]]*$/) bad = 1
        }
        END { exit (seen && !bad) ? 0 : 1 }
    ' "$work/defaults-auth.yaml" || die "the defaults set $name; they must leave it empty (RefuseAll)"
done
printf 'OPENBILLING_AUTH_ISSUER and OPENBILLING_AUTH_AUDIENCE are empty by default\n'

step "the configuration file"
# config_of RENDER: the `config.yaml` of the render's ConfigMap, unindented.
config_of() {
    awk '
        /^kind:[[:space:]]*ConfigMap[[:space:]]*$/ { cm = 1 }
        /^---/ { cm = 0; inside = 0 }
        cm && /^[[:space:]]+config\.yaml:[[:space:]]*\|-?[[:space:]]*$/ { inside = 1; next }
        inside && /^    / { sub(/^    /, ""); print; next }
        inside { inside = 0 }
    ' "$1"
}
expected_defaults='allowed:
  balance: at_or_above
  debt_limit: at_or_above
  member_cap: at_or_above
retention:
  margin: 90d
  timeout: 5m'
[ "$(config_of "$work/defaults.yaml")" = "$expected_defaults" ] || {
    config_of "$work/defaults.yaml" >&2
    die "the defaults' configuration file is not every check at_or_above with a retention margin of 90d and timeout of 5m"
}
expected_full='allowed:
  balance: never
  debt_limit: above
  member_cap: at_or_above
retention:
  margin: 30d
  timeout: 5m'
[ "$(config_of "$work/full.yaml")" = "$expected_full" ] || {
    config_of "$work/full.yaml" >&2
    die "full.yaml's configuration file is not its own"
}
for name in defaults full edge-with-digest; do
    awk '
        /-[[:space:]]+name:[[:space:]]+OPENBILLING_CONFIG[[:space:]]*$/ {
            if ((getline line) > 0 && line ~ /^[[:space:]]+value:[[:space:]]*"?\/etc\/openbilling\/config\.yaml"?[[:space:]]*$/) env = 1
        }
        /-[[:space:]]+mountPath:[[:space:]]+\/etc\/openbilling\/config\.yaml[[:space:]]*$/ { mount = 1 }
        /^[[:space:]]+subPath:[[:space:]]+config\.yaml[[:space:]]*$/ { sub_path = 1 }
        /^[[:space:]]+checksum\/configMaps:[[:space:]]+[0-9a-f]{64}[[:space:]]*$/ { checksum = 1 }
        END { exit (env && mount && sub_path && checksum) ? 0 : 1 }
    ' "$work/$name.yaml" || die "$name: OPENBILLING_CONFIG, its mount and the pod's checksum of the ConfigMap do not agree"
done
if [ -n "${OPENBILLING_BIN:-}" ]; then
    for name in defaults full edge-with-digest; do
        config_of "$work/$name.yaml" > "$work/$name.config.yaml"
        # A database on a closed port: the file is read first, then the connection fails.
        if OPENBILLING_DATABASE_URL=postgres://nobody@127.0.0.1:1/none OPENBILLING_CONFIG="$work/$name.config.yaml" \
            NO_COLOR=1 "$OPENBILLING_BIN" serve > "$work/$name.serve" 2>&1; then
            die "$name: openbilling serve started against a closed port"
        fi
        grep -q 'connecting to PostgreSQL' "$work/$name.serve" || {
            cat "$work/$name.serve" >&2
            die "$name: openbilling serve refused the configuration file the chart rendered"
        }
    done
    printf 'openbilling serve accepts the configuration file of every render\n'
fi
printf 'every render mounts its configuration file where OPENBILLING_CONFIG says, the defaults every check at_or_above\n'

for values in "$here"/values/refused-*.yaml; do
    name=$(basename "$values" .yaml)
    step "refused: $name"
    expect=$(sed -n 's/^# expect: //p' "$values" | head -1)
    [ -n "$expect" ] || die "$values has no '# expect:' line"
    if helm template openbilling "$chart" -f "$values" > "$work/$name.yaml" 2> "$work/$name.err"; then
        die "$name rendered; it must be refused with: $expect"
    fi
    grep -qF -- "$expect" "$work/$name.err" || {
        cat "$work/$name.err" >&2
        die "$name was refused, but not with: $expect"
    }
    printf 'refused, as expected: %s\n' "$expect"
done

step "ok"
