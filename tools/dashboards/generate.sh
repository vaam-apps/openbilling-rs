#!/bin/sh
# Regenerates every dashboard from its script into deploy/dashboards/ and the chart's copy. CI runs this and then
# `git diff --exit-code deploy/`: a dashboard is edited in tools/dashboards/, never in the JSON.
set -eu
cd "$(dirname "$0")"
for script in service.py queue_dashboard.py; do
  python3 -B "$script"
done
python3 -B alerts.py
