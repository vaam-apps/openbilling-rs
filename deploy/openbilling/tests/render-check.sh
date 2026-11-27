#!/bin/sh
# render-check.sh RENDER [--require-digest]
#
# What a render of the chart (`helm template` output, a file) must hold, whatever the values:
#   - no Secret object: the chart holds no secret, it names one;
#   - no credential in a URL (`scheme://user:password@`) anywhere in the render;
#   - OPENBILLING_DATABASE_URL comes from a Secret (`valueFrom.secretKeyRef`), never a literal `value`;
#   - every image is pinned: a fixed tag that is not `latest` or `edge`, or a digest; with --require-digest, every
#     image carries `@sha256:` and 64 hex digits (the chart release.yml publishes, and tests/values/full.yaml).
#
# POSIX sh, grep and awk only, so it runs the same on a laptop and in CI. Called by check.sh and release.yml.

set -eu

render=${1:?usage: render-check.sh RENDER [--require-digest]}
require_digest=no
[ "${2:-}" = "--require-digest" ] && require_digest=yes

failed=0
fail() {
    printf 'render-check: %s: %s\n' "$render" "$1" >&2
    failed=1
}

[ -s "$render" ] || { fail "the render is empty"; exit 1; }

if grep -Eq '^kind:[[:space:]]*Secret[[:space:]]*$' "$render"; then
    fail "renders a Secret; the chart must only name an existing one"
fi

# A URL with a password in it. Matches `postgres://u:p@h`, `https://u:p@h`, quoted or not.
if grep -Eq '[a-z][a-z0-9+.-]*://[^/@[:space:]"'"'"']*:[^/@[:space:]"'"'"']*@' "$render"; then
    fail "holds a URL with a credential in it"
fi

# The database URL: the line after `- name: OPENBILLING_DATABASE_URL` must be `valueFrom:`, and the env entry must
# not carry a `value:`.
db_entries=$(grep -Ec -- '-[[:space:]]+name:[[:space:]]+OPENBILLING_DATABASE_URL[[:space:]]*$' "$render" || true)
if [ "$db_entries" -eq 0 ]; then
    fail "sets no OPENBILLING_DATABASE_URL"
fi
if ! awk '
    /-[[:space:]]+name:[[:space:]]+OPENBILLING_DATABASE_URL[[:space:]]*$/ { want = 1; next }
    want == 1 {
        if ($0 !~ /^[[:space:]]+valueFrom:[[:space:]]*$/) { bad = 1 }
        want = 2; next
    }
    want == 2 {
        if ($0 ~ /^[[:space:]]+secretKeyRef:[[:space:]]*$/) { want = 0; next }
        bad = 1; want = 0
    }
    END { exit bad }
' "$render"; then
    fail "OPENBILLING_DATABASE_URL is not read from a Secret (valueFrom.secretKeyRef)"
fi

# Images.
images=$(grep -E '^[[:space:]]*(-[[:space:]]+)?image:[[:space:]]' "$render" | sed -E 's/^[[:space:]]*(-[[:space:]]+)?image:[[:space:]]*//; s/^"//; s/"[[:space:]]*$//' || true)
[ -n "$images" ] || fail "renders no image"
for image in $images; do
    case $image in
        *@sha256:*)
            digest=${image##*@sha256:}
            printf '%s' "$digest" | grep -Eq '^[0-9a-f]{64}$' || fail "image $image: the digest is not 64 hex digits"
            ref=${image%@sha256:*}
            ;;
        *)
            [ "$require_digest" = yes ] && fail "image $image carries no digest"
            ref=$image
            ;;
    esac
    # The tag is what follows the last `:` after the last `/` (a registry port is not a tag).
    name=${ref##*/}
    case $name in
        *:*) tag=${name##*:} ;;
        *) tag="" ;;
    esac
    case $tag in
        "") fail "image $image has no tag" ;;
        latest) fail "image $image is tagged latest" ;;
        edge)
            case $image in
                *@sha256:*) ;;
                *) fail "image $image is tagged edge with no digest" ;;
            esac
            ;;
    esac
done

if [ "$failed" -ne 0 ]; then
    exit 1
fi
printf 'render-check: %s: ok (%s)\n' "$render" "$(printf '%s\n' "$images" | sort -u | tr '\n' ' ')"
