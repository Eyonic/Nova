#!/usr/bin/env bash
# End-to-end acceptance tests for the first NOVA milestone.
#
# Runs a throwaway Compose project (own volumes, own port, random passwords),
# exercises static files, PHP, image optimization, the database, isolation,
# graceful shutdown and restart persistence, then tears everything down.
#
#   tests/integration/run.sh            # build + test + clean up
#   KEEP=1 tests/integration/run.sh     # leave the stack running afterwards
set -uo pipefail
cd "$(dirname "$0")/../.."

PROJECT=nova-test
PORT=${NOVA_TEST_PORT:-18088}
BASE="http://127.0.0.1:$PORT"
WORK=$(mktemp -d)
rnd() { head -c 24 /dev/urandom | base64 | tr -dc 'A-Za-z0-9' | head -c 20; }
cat > "$WORK/env" <<EOF
NOVA_HTTP_PORT=$PORT
NOVA_MODE=production
NOVA_DB_ROOT_PASSWORD=$(rnd)
NOVA_DB_EXAMPLE_PASSWORD=$(rnd)
NOVA_DB_SECOND_PASSWORD=$(rnd)
EOF
EXAMPLE_PW=$(grep EXAMPLE_PASSWORD "$WORK/env" | cut -d= -f2)
# -f pins the committed compose file: local overrides never leak into tests.
dc() { docker compose -f compose.yaml -p "$PROJECT" --env-file "$WORK/env" "$@"; }

PASS=0 FAIL=0
ok()   { PASS=$((PASS + 1)); printf '  \033[32m✓\033[0m %s\n' "$1"; }
bad()  { FAIL=$((FAIL + 1)); printf '  \033[31m✗\033[0m %s\n' "$1"; [ -n "${2:-}" ] && printf '      %s\n' "$2"; }
check() { local name=$1; shift; if "$@"; then ok "$name"; else bad "$name"; fi; }
deny()  { local name=$1; shift; if "$@"; then bad "$name"; else ok "$name"; fi; }
eq()   { [ "$2" = "$3" ] && ok "$1" || bad "$1" "expected [$3], got [$2]"; }
has()  { [[ "$2" == *"$3"* ]] && ok "$1" || bad "$1" "missing [$3] in: ${2:0:300}"; }
hasnt(){ [[ "$2" != *"$3"* ]] && ok "$1" || bad "$1" "unexpected [$3]"; }
code() { curl -s -o /dev/null -w '%{http_code}' "$@"; }
section() { printf '\n\033[1m%s\033[0m\n' "$1"; }

cleanup() {
  if [ -z "${KEEP:-}" ]; then dc down -v --remove-orphans >/dev/null 2>&1; fi
  rm -rf "$WORK"
}
trap cleanup EXIT

section "Build and start"
dc build -q nova >/dev/null 2>&1 || { echo "build failed"; dc build nova; exit 1; }
dc up -d --wait >/dev/null 2>&1 || { dc logs; echo "stack did not become healthy"; exit 1; }
ok "stack healthy"
eq "readiness endpoint" "$(code "$BASE/_nova/health/ready")" 200
eq "docker health check" "$(docker inspect -f '{{.State.Health.Status}}' "$(dc ps -q nova)")" healthy
# Process table: uid and effective capabilities of every process.
PS=$(dc exec -T nova sh -c 'for p in /proc/[0-9]*; do [ -r $p/status ] || continue; printf "%s %s %s %s\n" "${p#/proc/}" "$(awk "/^Uid:/{print \$2}" $p/status)" "$(awk "/^CapEff:/{print \$2}" $p/status)" "$(tr "\0" " " < $p/cmdline | cut -c1-80)"; done')
uid_of() { echo "$PS" | grep -F -- "$1" | head -1 | awk '{print $2}'; }
caps_of() { echo "$PS" | grep -F -- "$1" | head -1 | awk '{print $3}'; }
eq "worker runs as uid 10001" "$(uid_of 'nova --config')" 10001
eq "worker has no capabilities" "$(caps_of 'nova --config')" 0000000000000000
U_EX=$(uid_of 'master process (/run/nova/conf/example.conf)'); U_SE=$(uid_of 'master process (/run/nova/conf/second.conf)')
distinct_uids() { [ -n "$U_EX" ] && [ -n "$U_SE" ] && [ "$U_EX" != "$U_SE" ] && [ "$U_EX" -ge 20000 ] && [ "$U_SE" -ge 20000 ]; }
check "each site's PHP has its own uid ($U_EX, $U_SE)" distinct_uids
eq "PHP workers have no capabilities" "$(echo "$PS" | grep 'php-fpm' | awk '{print $3}' | sort -u)" 0000000000000000
eq "supervisor keeps only its 6 capabilities" "$(caps_of 'nova serve')" 00000000000000eb

section "Static files"
eq "index served" "$(code "$BASE/")" 200
has "html content type" "$(curl -sI "$BASE/" | tr -d '\r')" "content-type: text/html; charset=utf-8"
ETAG=$(curl -sI "$BASE/" | tr -d '\r' | awk -F': ' 'tolower($1)=="etag"{print $2}')
eq "conditional GET → 304" "$(code -H "If-None-Match: $ETAG" "$BASE/")" 304
eq "HEAD" "$(curl -s -o /dev/null -w '%{http_code}' -I "$BASE/")" 200
eq "range request → 206" "$(code -H 'Range: bytes=0-9' "$BASE/")" 206
eq "range body length" "$(curl -s -H 'Range: bytes=0-9' "$BASE/" | wc -c | tr -d ' ')" 10
eq "directory without slash redirects" "$(code "$BASE/images")" 301
eq "path traversal rejected" "$(code --path-as-is "$BASE/../../etc/passwd")" 400
eq "encoded traversal rejected" "$(code --path-as-is "$BASE/images/%2e%2e/%2e%2e/config/secret.php")" 400
eq "dotfiles hidden" "$(code "$BASE/.env")" 404
eq "POST to static file → 405" "$(code -X POST "$BASE/index.html")" 405
eq "unknown host falls back to default site" "$(code -H 'Host: nowhere.test' "$BASE/")" 200

section "PHP runtime"
INFO=$(curl -s "$BASE/info.php?x=1")
has "PHP executes (fpm-fcgi)" "$INFO" '"sapi": "fpm-fcgi"'
has "query string" "$INFO" '"x": "1"'
has "site env injected" "$INFO" '"app_name": "NOVA Example"'
has "request id passed to PHP" "$INFO" '"request_id": "'
has "pdo_mysql loaded" "$INFO" 'pdo_mysql'
hasnt "PHP source never exposed" "$(curl -s "$BASE/info.php")" '<?php'
has "PATH_INFO" "$(curl -s "$BASE/info.php/extra/path")" '"path_info": "/extra/path"'
has "front controller route" "$(curl -s "$BASE/hello/nova")" '"name":"nova"'
eq "front controller 404" "$(code "$BASE/no/such/route")" 404
has "form POST" "$(curl -s -d 'a=1&b=two' "$BASE/info.php")" '"b": "two"'
has "chunked POST body" "$(printf 'hello' | curl -s -H 'Transfer-Encoding: chunked' --data-binary @- "$BASE/info.php")" '"raw_body_bytes": 5'
SHA=$(sha1sum sites/example/public/images/hero.jpg | cut -d' ' -f1)
has "multipart upload" "$(curl -s -F "upload=@sites/example/public/images/hero.jpg" "$BASE/info.php")" "\"sha1\": \"$SHA\""
head -c $((33 * 1024 * 1024)) /dev/zero > "$WORK/big"
eq "oversized body → 413" "$(code --data-binary @"$WORK/big" "$BASE/info.php")" 413
eq "config outside docroot unreachable" "$(curl -s "$BASE/config/secret.php" | grep -c PRIVATE-MARKER)" 0
has "second site routed by Host" "$(curl -s -H 'Host: second.localhost' "$BASE/")" "second site (second)"

section "Image optimization"
for _ in $(seq 1 90); do
  STATUS=$(curl -s "$BASE/_nova/optimize/status")
  [[ "$STATUS" == *'"assets":2'* ]] && break
  sleep 2
done
has "both demo images optimized" "$STATUS" '"assets":2'
ORIG=$(stat -c %s sites/example/public/images/hero.jpg)
read -r CT SIZE < <(curl -s -o /dev/null -H 'Accept: image/avif,image/webp,*/*' -w '%{content_type} %{size_download}' "$BASE/images/hero.jpg")
eq "AVIF negotiated" "$CT" image/avif
check "AVIF smaller than original ($SIZE < $ORIG)" [ "$SIZE" -lt "$ORIG" ]
eq "WebP negotiated" "$(curl -s -o /dev/null -H 'Accept: image/webp' -w '%{content_type}' "$BASE/images/hero.jpg")" image/webp
eq "legacy client gets original" "$(curl -s -o /dev/null -H 'Accept: */*' -w '%{size_download}' "$BASE/images/hero.jpg")" "$ORIG"
has "Vary: Accept" "$(curl -sI -H 'Accept: image/avif' "$BASE/images/hero.jpg" | tr -d '\r')" "vary: Accept"
read -r CT640 S640 < <(curl -s -o /dev/null -H 'Accept: image/webp' -w '%{content_type} %{size_download}' "$BASE/images/hero.jpg?w=600")
eq "responsive variant format" "$CT640" image/webp
curl -s -H 'Accept: image/webp' -o "$WORK/v.webp" "$BASE/images/hero.jpg?w=600"
# VP8 bitstream: width in bytes 26-27 (14 bits, little endian).
W=$(od -An -t u2 -j 26 -N 2 "$WORK/v.webp" | tr -d ' '); W=$((W & 0x3fff))
eq "?w=600 rounds up to 640px variant" "$W" 640
eq "transparent PNG to AVIF" "$(curl -s -o /dev/null -H 'Accept: image/avif' -w '%{content_type}' "$BASE/images/logo.png")" image/avif

section "Database"
V1=$(curl -s -X POST "$BASE/db.php" | sed -E 's/.*"visits":([0-9]+).*/\1/')
V2=$(curl -s -X POST "$BASE/db.php" | sed -E 's/.*"visits":([0-9]+).*/\1/')
eq "writes persist across requests" "$V2" $((V1 + 1))

section "Isolation (site 'second' attacking site 'example')"
PROBE=$(curl -s -H 'Host: second.localhost' "$BASE/probe.php")
for k in read_other_site_file include_other_site_file list_other_site_dir read_proc_environ read_fpm_master_environ \
         read_etc_passwd read_nova_state_of_other_site shell_exec proc_open other_db_with_own_credentials; do
  has "blocked: $k" "$PROBE" "\"$k\": false"
done
for k in connect_other_site_fpm_socket list_php_socket_dirs connect_nova_http_port connect_internet_https \
         listen_tcp_port write_tmp write_own_project read_worker_cmdline posix_kill_available; do
  has "blocked: $k" "$PROBE" "\"$k\": false"
done
has "own state dir writable" "$PROBE" '"own_state_writable": true'
has "own database still works" "$PROBE" '"own_db_works": true'
hasnt "other site's DB password not visible" "$PROBE" "$EXAMPLE_PW"
hasnt "other site's private file not leaked" "$PROBE" "PRIVATE-MARKER"

section "Kernel isolation (no PHP restrictions involved)"
# Run plain shell commands as site "second" inside its Landlock policy.
SB="nova sandbox --read /usr --read /lib --read /lib64 --read /bin --read /etc/ld.so.cache --read /etc/resolv.conf --read /etc/hosts --read /etc/nsswitch.conf --read /srv/sites/second \
  --write /var/lib/nova/sites/second --write /dev/null --connect 3306 --require --"
EX_PID=$(echo "$PS" | grep -F 'master process (/run/nova/conf/example.conf)' | awk '{print $1}')
kern() { dc exec -T -u "$U_SE" nova sh -c "$SB sh -c '$1' >/dev/null 2>&1"; }
deny "kernel denies reading other site's code" kern 'cat /srv/sites/example/config/secret.php'
deny "kernel denies other site's socket dir" kern 'ls /run/nova/php/example'
deny "kernel denies other site's state" kern 'ls /var/lib/nova/sites/example'
deny "kernel denies other site's /proc environ" kern "cat /proc/$EX_PID/environ"
deny "kernel denies signalling other site" kern "kill -0 $EX_PID"
tcp() { dc exec -T -u "$U_SE" nova $SB php -n -r 'exit(@fsockopen($argv[1], (int) $argv[2], $e, $s, 2) ? 0 : 1);' "$1" "$2" >/dev/null 2>&1; }
deny "kernel denies outbound TCP to non-allowed port (1.1.1.1:443)" tcp 1.1.1.1 443
deny "kernel denies TCP to NOVA's own HTTP port" tcp 127.0.0.1 8080
check "kernel allows the site's database port (db:3306)" tcp db 3306
check "kernel allows own state dir" kern 'touch /var/lib/nova/sites/second/tmp/kernel-ok'
deny "kernel denies reading /etc/passwd" kern 'cat /etc/passwd'

section "NOVA Live"
SC=(-H 'Host: showcase.localhost')
eq "runtime served as JavaScript" "$(curl -s -o /dev/null -w '%{content_type}' "$BASE/_nova/live.js")" "text/javascript; charset=utf-8"
has "versioned runtime is immutable" "$(curl -sI "$BASE/_nova/live.js?v=0.1.0" | tr -d '\r')" "immutable"
has "runtime carries its version" "$(curl -s "$BASE/_nova/live.js")" "const VERSION = '0.1.0'"
FRAG=$(curl -s "${SC[@]}" -H 'Nova-Live: 1' -H 'Nova-Target: #products' "$BASE/live.php?category=decor")
has "PHP sees Nova-Live headers and returns a fragment" "$FRAG" "Coral vase"
hasnt "fragment is not a full document" "$FRAG" "<html"
has "PHP responses vary on Nova-Live" "$(curl -sI "${SC[@]}" "$BASE/live.php" | tr -d '\r')" "vary: Nova-Live, Nova-Target"
eq "invalid channel rejected" "$(code "${SC[@]}" "$BASE/_nova/live/events?channels=Bad!")" 400
eq "POST to event stream rejected" "$(code -X POST "${SC[@]}" "$BASE/_nova/live/events?channels=notes")" 405
curl -s -N -m 5 "${SC[@]}" "$BASE/_nova/live/events?channels=notes" > "$WORK/sse-showcase" &
curl -s -N -m 5 -H 'Host: second.localhost' "$BASE/_nova/live/events?channels=notes" > "$WORK/sse-second" &
sleep 1
TOKEN=$(curl -s -c "$WORK/jar" "${SC[@]}" "$BASE/live.php" | grep -o 'name="csrf" value="[^"]*"' | cut -d'"' -f4)
eq "forged CSRF token rejected" "$(code -b "$WORK/jar" "${SC[@]}" -d 'csrf=forged&note=x' "$BASE/live.php")" 422
eq "validation error → 422" "$(code -b "$WORK/jar" "${SC[@]}" -d "csrf=$TOKEN&note=" "$BASE/live.php")" 422
POSTED=$(curl -s -D - -o /dev/null -b "$WORK/jar" "${SC[@]}" -d "csrf=$TOKEN&note=integration" "$BASE/live.php" | tr -d '\r')
has "valid post redirects (PRG)" "$POSTED" "HTTP/1.1 303"
hasnt "Nova-Publish header never reaches the client" "$(echo "$POSTED" | tr 'A-Z' 'a-z')" "nova-publish"
wait
has "subscriber on the publishing site is notified" "$(cat "$WORK/sse-showcase")" $'event: invalidate\ndata: notes'
hasnt "same channel on another site is not notified" "$(cat "$WORK/sse-second")" "invalidate"
has "live metrics exported" "$(curl -s "$BASE/_nova/metrics")" "nova_live_published_total"

# State left by another identity (e.g. from shared mode) must be re-owned on start.
dc exec -T nova sh -c 'touch /var/lib/nova/sites/showcase/sessions/sess_legacy && chown 10001:10001 /var/lib/nova/sites/showcase/sessions/sess_legacy'

section "Graceful shutdown"
curl -s -m 20 "$BASE/slow.php?s=3" > "$WORK/slow" &
SLOW=$!
# An open event stream must not hold shutdown for the whole grace period.
curl -s -N -m 60 "${SC[@]}" "$BASE/_nova/live/events?channels=notes" > /dev/null &
SSE=$!
sleep 1
T0=$(date +%s)
dc stop nova >/dev/null 2>&1
T1=$(date +%s)
wait $SLOW
eq "in-flight request completed during shutdown" "$(cat "$WORK/slow")" done
check "open event streams closed promptly ($((T1 - T0))s < 15s)" [ $((T1 - T0)) -lt 15 ]
kill $SSE 2>/dev/null; wait $SSE 2>/dev/null
has "shutdown was clean" "$(dc logs nova 2>&1 | tail -20)" "NOVA stopped"

section "Persistence across restarts"
dc down >/dev/null 2>&1   # containers removed, volumes kept
dc up -d --wait >/dev/null 2>&1 || { bad "stack restarted"; dc logs --tail 50; }
eq "stack back after down/up" "$(code "$BASE/_nova/health/ready")" 200
U_SC=$(dc exec -T nova stat -c %u /var/lib/nova/sites/showcase)
eq "files left by another uid are re-owned to the site" "$(dc exec -T nova stat -c %u /var/lib/nova/sites/showcase/sessions/sess_legacy)" "$U_SC"
has "database rows survived" "$(curl -s "$BASE/db.php")" "\"visits\":$V2"
eq "optimized assets served immediately after restart" \
  "$(curl -s -o /dev/null -H 'Accept: image/avif' -w '%{content_type}' "$BASE/images/hero.jpg")" image/avif

section "Observability"
METRICS=$(curl -s "$BASE/_nova/metrics")
has "metrics exported" "$METRICS" 'nova_requests_total{kind="php"}'
has "PHP pools ready gauge" "$METRICS" 'nova_php_ready 1'
has "structured JSON logs in production" "$(dc logs nova 2>&1 | grep '"target":"nova::access"' | tail -1)" '"status":'
has "x-request-id header" "$(curl -sI "$BASE/" | tr -d '\r')" "x-request-id:"

printf '\n%d passed, %d failed\n' "$PASS" "$FAIL"
[ "$FAIL" -eq 0 ]
