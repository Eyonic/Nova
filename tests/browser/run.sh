#!/usr/bin/env bash
# Browser tests for NOVA Live (headless Chromium via the official Playwright image).
# Expects a running stack; defaults to the dev stack from compose.yaml.
#   tests/browser/run.sh                       # http://localhost:8088
#   NOVA_URL=http://localhost:18088 tests/browser/run.sh
set -euo pipefail
cd "$(dirname "$0")"
PORT=$(echo "${NOVA_URL:-http://localhost:8088}" | sed -E 's#.*:([0-9]+).*#\1#')
exec docker run --rm --network host --init \
  -e NOVA_PORT="$PORT" -e npm_config_update_notifier=false \
  -v "$PWD:/tests" -v nova-browser-npm:/tests/node_modules -w /tests \
  mcr.microsoft.com/playwright:v1.52.0-noble \
  sh -c 'npm install --silent --no-audit --no-fund >/dev/null && node live.test.mjs'
