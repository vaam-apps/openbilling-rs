{{- /*
The refusals values cannot say. Each names the value to change. Run before the common library renders anything.
*/ -}}
{{- define "openbilling.validate" -}}
  {{- $database := dig "database" dict (.Values.openbilling | default dict) -}}
  {{- if not (dig "existingSecret" "" $database) -}}
    {{- fail "openbilling.database.existingSecret: name the existing Secret that holds the database URL; the chart never makes one" -}}
  {{- end -}}
  {{- if not (dig "key" "" $database) -}}
    {{- fail "openbilling.database.key: name the key of that Secret that holds the URL" -}}
  {{- end -}}

  {{- $otel := dig "otel" dict (.Values.openbilling | default dict) -}}
  {{- $endpoint := toString (dig "endpoint" "" $otel) -}}
  {{- if and $endpoint (not (regexMatch "^https?://[^/@ ]+(/[^ ]*)?$" $endpoint)) -}}
    {{- fail "openbilling.otel.endpoint: an http(s) URL with no user or password (a credential is a header, from a file), or empty for no OpenTelemetry" -}}
  {{- end -}}

  {{- /* Production runs fixed versions only (ADR 0005). */ -}}
  {{- $image := dig "main" "containers" "main" "image" dict (.Values.controllers | default dict) -}}
  {{- $tag := tpl (toString (dig "tag" "" $image)) . -}}
  {{- $digest := toString (dig "digest" "" $image) -}}
  {{- if not (dig "repository" "" $image) -}}
    {{- fail "controllers.main.containers.main.image.repository: name the image" -}}
  {{- end -}}
  {{- if eq $tag "latest" -}}
    {{- fail "controllers.main.containers.main.image.tag: never `latest`; name a release (X.Y.Z), as appVersion does by default" -}}
  {{- end -}}
  {{- if and $digest (not (regexMatch "^sha256:[0-9a-f]{64}$" $digest)) -}}
    {{- fail "controllers.main.containers.main.image.digest: a digest is sha256: and 64 lower-case hex digits" -}}
  {{- end -}}
  {{- if and (not $digest) (not (regexMatch "^(v?[0-9]+\\.[0-9]+\\.[0-9]+(-[0-9A-Za-z.-]+)?|sha-[0-9a-f]{7,40})$" $tag)) -}}
    {{- fail (printf "controllers.main.containers.main.image.tag: %q is not a fixed version; name a release (X.Y.Z) or a sha-<7> build, or set image.digest" $tag) -}}
  {{- end -}}
{{- end -}}
