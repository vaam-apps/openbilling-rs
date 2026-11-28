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

  {{- /* The token verifier: the issuer and the audience together, or neither (as the binary refuses at start). */ -}}
  {{- $auth := dig "auth" dict (.Values.openbilling | default dict) -}}
  {{- $issuer := trim (toString (dig "issuer" "" $auth)) -}}
  {{- $audience := trim (toString (dig "audience" "" $auth)) -}}
  {{- if and $issuer (not $audience) -}}
    {{- fail "openbilling.auth.audience: set it with openbilling.auth.issuer, or set neither" -}}
  {{- end -}}
  {{- if and $audience (not $issuer) -}}
    {{- fail "openbilling.auth.issuer: set it with openbilling.auth.audience, or set neither" -}}
  {{- end -}}
  {{- if not $issuer -}}
    {{- range $name := list "jwksUrl" "rolesClaim" "roleMap" -}}
      {{- if trim (toString (dig $name "" $auth)) -}}
        {{- fail (printf "openbilling.auth.%s: needs openbilling.auth.issuer and openbilling.auth.audience" $name) -}}
      {{- end -}}
    {{- end -}}
  {{- end -}}
  {{- if and $issuer (not (regexMatch "^https://|^http://(127\\.[0-9.]+|localhost|\\[::1\\])([:/]|$)" $issuer)) -}}
    {{- fail "openbilling.auth.issuer: https, or http to a loopback address" -}}
  {{- end -}}

  {{- /* The configuration file (OPENBILLING_CONFIG): what the binary takes, refused here as it refuses it at start.
  The file holds policy only; secrets and endpoints are environment variables. */ -}}
  {{- $config := dig "config" dict (.Values.openbilling | default dict) -}}
  {{- if not (kindIs "map" $config) -}}
    {{- fail "openbilling.config: a map; it holds `allowed` and `vpay`" -}}
  {{- end -}}
  {{- range $key, $_ := $config -}}
    {{- if not (has $key (list "allowed" "vpay")) -}}
      {{- fail (printf "openbilling.config.%s: not a key of the configuration file, which holds `allowed` and `vpay`; secrets and endpoints are environment variables" $key) -}}
    {{- end -}}
  {{- end -}}
  {{- $allowed := dig "allowed" dict $config -}}
  {{- if not (kindIs "map" $allowed) -}}
    {{- fail "openbilling.config.allowed: a map of debt_limit, balance and member_cap" -}}
  {{- end -}}
  {{- $checks := list "debt_limit" "balance" "member_cap" -}}
  {{- range $key, $mode := $allowed -}}
    {{- if not (has $key $checks) -}}
      {{- fail (printf "openbilling.config.allowed.%s: not a check; the checks are debt_limit, balance and member_cap" $key) -}}
    {{- end -}}
    {{- if not (has (toString $mode) (list "at_or_above" "above" "never")) -}}
      {{- fail (printf "openbilling.config.allowed.%s: at_or_above, above or never" $key) -}}
    {{- end -}}
  {{- end -}}

  {{- /* vpay.token: the tunables of a minted vpay token (the binary also refuses them without a client key). */ -}}
  {{- $vpay := dig "vpay" dict $config -}}
  {{- if not (kindIs "map" $vpay) -}}
    {{- fail "openbilling.config.vpay: a map holding `token`" -}}
  {{- end -}}
  {{- range $key, $_ := $vpay -}}
    {{- if ne $key "token" -}}
      {{- fail (printf "openbilling.config.vpay.%s: not a key; vpay holds only `token` (the adapter's endpoints and secrets are environment variables)" $key) -}}
    {{- end -}}
  {{- end -}}
  {{- $token := dig "token" dict $vpay -}}
  {{- if not (kindIs "map" $token) -}}
    {{- fail "openbilling.config.vpay.token: a map of audience, scope, refresh_skew_secs, retries and retry_backoff_ms" -}}
  {{- end -}}
  {{- $bounds := dict "refresh_skew_secs" 600 "retries" 10 "retry_backoff_ms" 5000 -}}
  {{- range $key, $value := $token -}}
    {{- if has $key (list "audience" "scope") -}}
      {{- if not (and (kindIs "string" $value) $value) -}}
        {{- fail (printf "openbilling.config.vpay.token.%s: a non-empty string" $key) -}}
      {{- end -}}
    {{- else if hasKey $bounds $key -}}
      {{- if not (or (kindIs "float64" $value) (kindIs "int64" $value) (kindIs "int" $value)) -}}
        {{- fail (printf "openbilling.config.vpay.token.%s: a whole number" $key) -}}
      {{- end -}}
      {{- if or (lt (float64 $value) 0.0) (gt (float64 $value) (float64 (get $bounds $key))) (ne (float64 $value) (floor (float64 $value))) -}}
        {{- fail (printf "openbilling.config.vpay.token.%s: a whole number from 0 to %v" $key (get $bounds $key)) -}}
      {{- end -}}
    {{- else -}}
      {{- fail (printf "openbilling.config.vpay.token.%s: not a key; the keys are audience, scope, refresh_skew_secs, retries and retry_backoff_ms" $key) -}}
    {{- end -}}
  {{- end -}}

  {{- $otel := dig "otel" dict (.Values.openbilling | default dict) -}}
  {{- $endpoint := toString (dig "endpoint" "" $otel) -}}
  {{- if and $endpoint (not (regexMatch "^https?://[^/@ ]+(/[^ ]*)?$" $endpoint)) -}}
    {{- fail "openbilling.otel.endpoint: an http(s) URL with no user or password (a credential is a header, from a file), or empty for no OpenTelemetry" -}}
  {{- end -}}
  {{- /* With the NetworkPolicy on, the collector's port must be one its egress rules allow, or the pod would silently lose its telemetry. */ -}}
  {{- $policy := dig "main" dict (.Values.networkpolicies | default dict) -}}
  {{- if and $endpoint (dig "enabled" false $policy) -}}
    {{- $authority := regexFind "^[^/]*" (regexReplaceAll "^https?://" $endpoint "") -}}
    {{- $port := ternary "443" "80" (hasPrefix "https://" $endpoint) -}}
    {{- $given := trimPrefix ":" (regexFind ":[0-9]+$" $authority) -}}
    {{- if $given -}}
      {{- $port = $given -}}
    {{- end -}}
    {{- $open := false -}}
    {{- range $rule := (dig "rules" "egress" (list) $policy) -}}
      {{- range $p := (dig "ports" (list) $rule) -}}
        {{- if eq (toString (dig "port" "" $p)) $port -}}
          {{- $open = true -}}
        {{- end -}}
      {{- end -}}
    {{- end -}}
    {{- if not $open -}}
      {{- fail (printf "openbilling.otel.endpoint: the NetworkPolicy is on and networkpolicies.main.rules.egress allows no port %s, the collector's; add a rule for it" $port) -}}
    {{- end -}}
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
