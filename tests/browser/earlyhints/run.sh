#!/usr/bin/env bash
# Early Hints A/B in Chromium (see README.md).
set -euo pipefail
HERE=$(cd "$(dirname "$0")" && pwd)
WORK=$(mktemp -d); trap 'docker rm -f nova-eh >/dev/null 2>&1; rm -rf "$WORK"' EXIT
mkdir -p "$WORK/site/public/eh"
cp "$HERE/page.php" "$WORK/site/public/page.php"
head -c 120000 /dev/urandom > "$WORK/rnd"
base64 -w 76 "$WORK/rnd" | sed 's/^/.x{content:"/; s/$/"}/' > "$WORK/site/public/eh/app.css"
printf 'h1.hero{color:#123}\n' >> "$WORK/site/public/eh/app.css"
base64 -w 76 "$WORK/rnd" | sed 's/^/\/\/ /' > "$WORK/site/public/eh/app.js"
chmod -R a+rX "$WORK"
for hints in false true; do
  cat > "$WORK/nova.toml" <<TOML
version = 1
mode = "production"
[server]
listen = "0.0.0.0:9007"
admin_allow = ["127.0.0.0/8"]
[server.tls]
enabled = true
listen = "0.0.0.0:9447"
public_port = 9447
self_signed = true
http3 = false
[paths]
state_dir = "/var/lib/nova"
run_dir = "/run/nova"
[optimize]
enabled = false
[[site]]
name = "eh"
hosts = ["localhost", "127.0.0.1"]
default = true
path = "/srv/sites/eh"
early_hints = $hints
[site.php]
max_children = 4
TOML
  docker rm -f nova-eh >/dev/null 2>&1 || true
  docker run -d --name nova-eh --network host --cap-drop ALL \
    --cap-add CHOWN,DAC_OVERRIDE,FOWNER,SETUID,SETGID,KILL \
    --tmpfs /run/nova --tmpfs /var/lib/nova --tmpfs /tmp \
    -v "$WORK/nova.toml:/etc/nova/nova.toml:ro" -v "$WORK/site:/srv/sites/eh:ro" \
    ${NOVA_BIN:+-v "$(realpath "$NOVA_BIN"):/usr/local/bin/nova:ro"} nova:dev >/dev/null
  until curl -sf -o /dev/null http://127.0.0.1:9007/_nova/health/ready; do sleep 0.5; done
  # One visit so NOVA learns what the page loads.
  curl -sk --http2 -o /dev/null -H 'Sec-Fetch-Mode: navigate' -H 'Accept: text/html' https://localhost:9447/page.php
  SPKI=$(openssl s_client -connect localhost:9447 -servername localhost </dev/null 2>/dev/null |
    openssl x509 -pubkey -noout | openssl pkey -pubin -outform der | openssl dgst -sha256 -binary | base64)
  printf 'early_hints=%-5s ' "$hints"
  docker run --rm --network host --init -e URL=https://localhost:9447/page.php -e RUNS="${RUNS:-5}" -e DEBUG="${DEBUG:-}" -e SPKI="$SPKI" -e LATENCY="${LATENCY:-80}" \
    -v "$HERE:/pw" -v nova-browser-npm:/pw/node_modules -w /pw mcr.microsoft.com/playwright:v1.52.0-noble \
    sh -c 'npm i --silent --no-audit --no-fund playwright@1.52.0 >/dev/null 2>&1; node measure.mjs'
done
