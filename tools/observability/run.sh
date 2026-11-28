#!/bin/sh
# Runs the whole observability stack on this machine and the gated end-to-end test against it: the real
# `openbilling serve` -> OpenTelemetry Collector (contrib) -> Prometheus -> Grafana, all bound to 127.0.0.1, no
# container runtime. CI's `observability.yml` runs this same script.
#
#   sh tools/observability/run.sh                  # OBS_LEG=scrape (default)
#   OBS_LEG=otlp sh tools/observability/run.sh
#
# Two legs, one test. `scrape`: Collector `prometheus` exporter on OBS_EXPORTER_PORT, scraped by Prometheus.
# `otlp`: no exporter and no scrape job; the Collector's `otlphttp` exporter pushes to Prometheus's own OTLP receiver
# (`--web.enable-otlp-receiver`, /api/v1/otlp/v1/metrics), configured by prometheus-otlp.yaml (ADR 0007).
#
# Needs: python3, curl, tar, cargo, and OPENBILLING_TEST_DATABASE_URL (a PostgreSQL 19 whose user may create
# databases). Binaries are fetched by tools/observability/fetch.py at the pins of versions.env into
# OBS_CACHE (default ~/.cache/openbilling-observability) and checked by digest. Ports (change them if taken):
# OBS_OTLP_PORT 4318, OBS_EXPORTER_PORT 8889 (scrape leg only), OBS_PROMETHEUS_PORT 9090, OBS_GRAFANA_PORT 3000.
set -eu

here=$(cd "$(dirname "$0")" && pwd)
root=$(cd "$here/../.." && pwd)
. "$here/versions.env"

leg=${OBS_LEG:-scrape}
case "$leg" in scrape | otlp) ;; *) echo "OBS_LEG must be scrape or otlp, not '$leg'" >&2; exit 1 ;; esac

: "${OPENBILLING_TEST_DATABASE_URL:?set OPENBILLING_TEST_DATABASE_URL to a PostgreSQL 19 server}"
cache=${OBS_CACHE:-${HOME:-/tmp}/.cache/openbilling-observability}
otlp_port=${OBS_OTLP_PORT:-4318}
exporter_port=${OBS_EXPORTER_PORT:-8889}
prom_port=${OBS_PROMETHEUS_PORT:-9090}
grafana_port=${OBS_GRAFANA_PORT:-3000}

mkdir -p "$cache"
otelcol_dir=$cache/otelcol-$OTELCOL_TAG
prom_dir=$cache/prometheus-$PROMETHEUS_TAG
grafana_tar=$cache/grafana-$GRAFANA_VERSION.tar.gz
grafana_dir=$cache/grafana-$GRAFANA_VERSION

python3 -I "$here/fetch.py" image "$OTELCOL_REPO" "$OTELCOL_DIGEST" "$otelcol_dir" otelcol-contrib
python3 -I "$here/fetch.py" image "$PROMETHEUS_REPO" "$PROMETHEUS_DIGEST" "$prom_dir" bin/prometheus bin/promtool
python3 -I "$here/fetch.py" url "https://dl.grafana.com/oss/release/grafana-$GRAFANA_VERSION.linux-amd64.tar.gz" \
  "$GRAFANA_SHA256" "$grafana_tar"
# The extracted tree carries the sha256 of the tarball it came from; any other stamp (or none) extracts it again.
if [ ! -x "$grafana_dir/bin/grafana" ] || [ "$(cat "$grafana_dir/.tarball-sha256" 2>/dev/null)" != "$GRAFANA_SHA256" ]; then
  rm -rf "$grafana_dir.part" "$grafana_dir"
  mkdir -p "$grafana_dir.part"
  tar -xzf "$grafana_tar" -C "$grafana_dir.part" --strip-components=1
  printf '%s\n' "$GRAFANA_SHA256" >"$grafana_dir.part/.tarball-sha256"
  mv "$grafana_dir.part" "$grafana_dir"
fi

# Build the test binary now, so that no server idles (or times out) while cargo compiles.
cd "$root"
cargo test -p openbilling --locked --test e2e --no-run

work=$(mktemp -d)
pids=""
cleanup() {
  status=$?
  trap - EXIT INT TERM
  for pid in $pids; do kill "$pid" 2>/dev/null || true; done
  # Bounded: give each server 10 s to stop on TERM, then KILL it.
  for pid in $pids; do
    n=0
    while kill -0 "$pid" 2>/dev/null && [ "$n" -lt 20 ]; do n=$((n + 1)); sleep 0.5; done
    kill -9 "$pid" 2>/dev/null || true
    wait "$pid" 2>/dev/null || true
  done
  for log in otelcol prometheus grafana; do
    if [ "$status" -ne 0 ] && [ -f "$work/$log.log" ]; then
      echo "----- $log.log (last 60 lines) -----"
      tail -n 60 "$work/$log.log"
    fi
  done
  rm -rf "$work"
  exit "$status"
}
trap cleanup EXIT INT TERM

# Refuses to start a server on a port where something already answers: a foreign server would be tested instead.
port_free() { # name port
  if curl -s --noproxy '*' --max-time 2 -o /dev/null "http://127.0.0.1:$2/"; then
    echo "port $2 ($1) already answers: stop what runs there or set OBS_*_PORT" >&2
    exit 1
  fi
}

wait_for() { # name url pid
  i=0
  until curl -sf --noproxy '*' -o /dev/null "$2"; do
    if ! kill -0 "$3" 2>/dev/null; then echo "$1: the process $3 exited before it was ready" >&2; exit 1; fi
    i=$((i + 1))
    if [ "$i" -gt 120 ]; then echo "$1 was not ready at $2 in 60 s" >&2; exit 1; fi
    sleep 0.5
  done
}

port_free "the Collector's OTLP receiver" "$otlp_port"
[ "$leg" = scrape ] && port_free "the Collector's exporter" "$exporter_port"
port_free Prometheus "$prom_port"
port_free Grafana "$grafana_port"

start_prometheus() {
  # Prometheus, with the shipped alert rules.
  if [ "$leg" = scrape ]; then
    sed "s#@RULES@#$root/deploy/alerts/openbilling.rules.yaml#; s/127.0.0.1:8889/127.0.0.1:$exporter_port/" \
      "$here/prometheus.yaml" >"$work/prometheus.yaml"
    flags=""
  else
    sed "s#@RULES@#$root/deploy/alerts/openbilling.rules.yaml#" "$here/prometheus-otlp.yaml" >"$work/prometheus.yaml"
    flags="--web.enable-otlp-receiver"
  fi
  "$prom_dir/promtool" check config "$work/prometheus.yaml"
  # shellcheck disable=SC2086
  "$prom_dir/prometheus" --config.file="$work/prometheus.yaml" --storage.tsdb.path="$work/tsdb" \
    --web.listen-address="127.0.0.1:$prom_port" $flags >"$work/prometheus.log" 2>&1 &
  pid=$!
  pids="$pids $pid"
  wait_for Prometheus "http://127.0.0.1:$prom_port/-/ready" "$pid"
}

start_collector() {
  if [ "$leg" = scrape ]; then
    sed "s/127.0.0.1:4318/127.0.0.1:$otlp_port/; s/127.0.0.1:8889/127.0.0.1:$exporter_port/" \
      "$here/otelcol.yaml" >"$work/otelcol.yaml"
  else
    sed "s/127.0.0.1:4318/127.0.0.1:$otlp_port/; s/127.0.0.1:9090/127.0.0.1:$prom_port/" \
      "$here/otelcol-otlp.yaml" >"$work/otelcol.yaml"
  fi
  "$otelcol_dir/otelcol-contrib" --config "$work/otelcol.yaml" >"$work/otelcol.log" 2>&1 &
  pid=$!
  pids="$pids $pid"
  if [ "$leg" = scrape ]; then
    wait_for "the Collector's exporter" "http://127.0.0.1:$exporter_port/metrics" "$pid"
  else
    # Any HTTP answer from the OTLP receiver (a GET is refused with 405) means it listens.
    i=0
    until curl -s --noproxy '*' -o /dev/null "http://127.0.0.1:$otlp_port/v1/metrics"; do
      if ! kill -0 "$pid" 2>/dev/null; then echo "the Collector exited before it was ready" >&2; exit 1; fi
      i=$((i + 1))
      if [ "$i" -gt 120 ]; then echo "the Collector's OTLP receiver was not ready in 60 s" >&2; exit 1; fi
      sleep 0.5
    done
  fi
}

# The otlp leg starts Prometheus first: the Collector pushes to it.
if [ "$leg" = scrape ]; then
  start_collector
  start_prometheus
else
  start_prometheus
  start_collector
fi

# Grafana, provisioned with the datasource and the dashboards exactly as deploy/dashboards/ holds them.
mkdir -p "$work/provisioning/datasources" "$work/provisioning/dashboards" "$work/grafana/data" "$work/grafana/logs"
sed "s#@PROMETHEUS_URL@#http://127.0.0.1:$prom_port#" \
  "$here/grafana/provisioning/datasources/prometheus.yaml" >"$work/provisioning/datasources/prometheus.yaml"
sed "s#@DASHBOARDS@#$root/deploy/dashboards#" \
  "$here/grafana/provisioning/dashboards/openbilling.yaml" >"$work/provisioning/dashboards/openbilling.yaml"
password=$(od -An -N16 -tx1 /dev/urandom | tr -d ' \n')
GF_PATHS_DATA="$work/grafana/data" GF_PATHS_LOGS="$work/grafana/logs" GF_PATHS_PLUGINS="$work/grafana/plugins" \
GF_PATHS_PROVISIONING="$work/provisioning" GF_SERVER_HTTP_ADDR=127.0.0.1 GF_SERVER_HTTP_PORT="$grafana_port" \
GF_SECURITY_ADMIN_USER=admin GF_SECURITY_ADMIN_PASSWORD="$password" GF_ANALYTICS_REPORTING_ENABLED=false \
GF_ANALYTICS_CHECK_FOR_UPDATES=false GF_ANALYTICS_CHECK_FOR_PLUGIN_UPDATES=false \
GF_PLUGINS_PREINSTALL_DISABLED=true GF_NEWS_NEWS_FEED_ENABLED=false GF_LOG_MODE=console \
  "$grafana_dir/bin/grafana" server --homepath "$grafana_dir" >"$work/grafana.log" 2>&1 &
pid=$!
pids="$pids $pid"
wait_for Grafana "http://127.0.0.1:$grafana_port/api/health" "$pid"

# The gated test: it skips without these two, and fails instead under OPENBILLING_OBS_REQUIRE.
cd "$root"
OPENBILLING_OBS_LEG="$leg" \
OPENBILLING_TEST_REQUIRE_DB=${OPENBILLING_TEST_REQUIRE_DB:-1} \
OPENBILLING_OBS_REQUIRE=1 \
OPENBILLING_OBS_OTLP_URL="http://127.0.0.1:$otlp_port" \
OPENBILLING_OBS_PROMETHEUS_URL="http://127.0.0.1:$prom_port" \
OPENBILLING_OBS_GRAFANA_URL="http://127.0.0.1:$grafana_port" \
OPENBILLING_OBS_GRAFANA_PASSWORD="$password" \
  cargo test -p openbilling --locked --test e2e observability_stack -- --nocapture
