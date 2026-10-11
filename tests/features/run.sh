#!/usr/bin/env bash
# Feature tests that need other services: load balancing with failover,
# OpenTelemetry tracing, the built-in WAF, external authorization (SSO and
# the full OWASP CRS through the official ModSecurity image) and ACME DNS-01
# wildcard certificates (Pebble + its test DNS server).
#
#   tests/features/run.sh            # needs the nova:dev image (scripts/check.sh builds it)
#   KEEP=1 tests/features/run.sh     # leave the stack running
set -uo pipefail
cd "$(dirname "$0")"

PROJECT=nova-features
PORT=${NOVA_FEAT_PORT:-18188}
TLS=${NOVA_FEAT_TLS:-18543}
CTL=${NOVA_FEAT_CTL:-18190}
PEBBLE=${NOVA_FEAT_PEBBLE:-18500}
BASE="http://127.0.0.1:$PORT"
dc() { docker compose -p "$PROJECT" "$@"; }

PASS=0 FAIL=0
ok()   { PASS=$((PASS + 1)); printf '  \033[32m✓\033[0m %s\n' "$1"; }
bad()  { FAIL=$((FAIL + 1)); printf '  \033[31m✗\033[0m %s\n' "$1"; [ -n "${2:-}" ] && printf '      %s\n' "$2"; }
check() { local name=$1; shift; if "$@"; then ok "$name"; else bad "$name"; fi; }
eq()   { [ "$2" = "$3" ] && ok "$1" || bad "$1" "expected [$3], got [$2]"; }
has()  { [[ "$2" == *"$3"* ]] && ok "$1" || bad "$1" "missing [$3] in: ${2:0:300}"; }
hasnt(){ [[ "$2" != *"$3"* ]] && ok "$1" || bad "$1" "unexpected [$3]"; }
code() { curl -s -o /dev/null -w '%{http_code}' "$@"; }
section() { printf '\n\033[1m%s\033[0m\n' "$1"; }
jget() { python3 -c "import json,sys; d=json.load(sys.stdin); print(eval(sys.argv[1]))" "$1"; }

cleanup() { [ -z "${KEEP:-}" ] && dc down -v --remove-orphans >/dev/null 2>&1; }
trap cleanup EXIT

section "Start"
dc down -v --remove-orphans >/dev/null 2>&1
dc up -d >/dev/null 2>&1 || { bad "stack started"; dc logs --tail 40; exit 1; }
up() { for _ in $(seq 120); do [ "$(code "$BASE/_nova/health/ready")" = 200 ] && return 0; sleep 0.5; done; return 1; }
check "NOVA ready" up
# ModSecurity needs a moment to load the Core Rule Set.
for _ in $(seq 60); do [ "$(code -H 'Host: crs.test' "$BASE/")" = 200 ] && break; sleep 1; done

section "Load balancing"
L=(-H "Host: lb.test")
SEEN=$(for _ in $(seq 10); do curl -s "${L[@]}" "$BASE/x" | jget 'd["backend"]'; done | sort | uniq -c | tr -s ' ' | tr '\n' ' ' | tr -s ' ')
eq "round robin over both upstreams" "$SEEN" " 5 a 5 b "
R=$(curl -s "${L[@]}" "$BASE/x")
eq "upstream gets the client address" "$(echo "$R" | jget 'd["xff"]')" "$(echo "$R" | jget 'd["xff"]' | grep -E '^[0-9.]+$')"
eq "upstream gets the original Host" "$(echo "$R" | jget 'd["host"]')" "lb.test"
eq "static files stay with NOVA" "$(curl -s "${L[@]}" "$BASE/static.txt")" "lb static"
curl -s -X POST "http://127.0.0.1:$CTL/down/b" >/dev/null
CODES=$(for _ in $(seq 20); do code "${L[@]}" "$BASE/x"; echo; done | sort | uniq -c | tr -s ' ' | tr '\n' ' ')
eq "upstream down: no failed request" "$CODES" " 20 200 "
ONLY=$(for _ in $(seq 6); do curl -s "${L[@]}" "$BASE/x" | jget 'd["backend"]'; done | sort -u | tr '\n' ' ')
eq "traffic moved to the healthy upstream" "$ONLY" "a "
curl -s -X POST "http://127.0.0.1:$CTL/up/b" >/dev/null
back() { for _ in $(seq 20); do for _ in 1 2 3 4; do curl -s "${L[@]}" "$BASE/x"; done | grep -q '"backend": "b"' && return 0; sleep 0.5; done; return 1; }
check "recovered upstream gets traffic again (health check)" back
ADM=$(curl -s "$BASE/_nova/admin/status" -H 'Host: lb.test')
has "admin API shows upstream health" "$ADM" '"up":true'

section "OpenTelemetry"
TRACE=4bf92f3577b34da6a3ce929d0e0e4736
TP=$(curl -s "${L[@]}" -H "traceparent: 00-$TRACE-00f067aa0ba902b7-01" "$BASE/traced" | jget 'd["traceparent"]')
has "upstream joins the client's trace" "$TP" "00-$TRACE-"
curl -s -H 'Host: sso.test' -b 'session=ok' "$BASE/" >/dev/null
spans() { curl -s "http://127.0.0.1:$CTL/spans"; }
got() { spans | python3 -c "
import json,sys
s=json.load(sys.stdin)
srv=[x for x in s if x['trace']=='$TRACE' and x['attrs'].get('url.path')=='/traced']
child=[x for x in s if x['trace']=='$TRACE' and x['name']=='proxy']
php=[x for x in s if x['name'] in ('php-fpm','php worker')]
sys.exit(0 if srv and child and child[0]['parent']==srv[0]['span'] and php else 1)"; }
wait_spans() { for _ in $(seq 20); do got && return 0; sleep 0.5; done; return 1; }
check "server span, proxy child span and PHP span exported (OTLP)" wait_spans
has "span carries the site and status" "$(spans)" '"nova.site": "lb"'

section "WAF (block)"
W=(-H "Host: waf.test")
eq "normal page" "$(code "${W[@]}" "$BASE/")" 200
eq "quotes in normal input pass" "$(curl -s "${W[@]}" --get --data-urlencode "q=O'Brien's -- best; deal" "$BASE/search.php")" "q=O'Brien's -- best; deal"
eq "SQL injection blocked" "$(code "${W[@]}" --get --data-urlencode "q=1' OR '1'='1" "$BASE/search.php")" 403
eq "XSS blocked" "$(code "${W[@]}" --get --data-urlencode "q=<script>alert(1)</script>" "$BASE/search.php")" 403
eq "path traversal blocked" "$(code "${W[@]}" --get --data-urlencode "q=../../../../etc/passwd" "$BASE/search.php")" 403
eq "secret probe blocked" "$(code "${W[@]}" "$BASE/.env")" 403
eq "scanner blocked" "$(code "${W[@]}" -A 'sqlmap/1.8' "$BASE/")" 403
has "matches counted" "$(curl -s "$BASE/_nova/metrics")" 'nova_waf_matches_total'

section "OWASP CRS via auth_request (official ModSecurity image)"
C=(-H "Host: crs.test")
eq "normal page" "$(code "${C[@]}" "$BASE/")" 200
eq "form post passes and reaches PHP" "$(curl -s "${C[@]}" -d 'name=Ann&comment=Nice+place' "$BASE/" | jget 'd["post"]["comment"]')" "Nice place"
eq "SQL injection in the query: 403" "$(code "${C[@]}" --get --data-urlencode "id=1 UNION SELECT password FROM users" "$BASE/")" 403
eq "SQL injection in a form: 403" "$(code "${C[@]}" --data-urlencode "id=1' OR 1=1 -- " "$BASE/")" 403
eq "XSS: 403" "$(code "${C[@]}" --get --data-urlencode "q=<script>alert(document.cookie)</script>" "$BASE/")" 403
eq "command injection: 403" "$(code "${C[@]}" --get --data-urlencode "f=;cat /etc/passwd" "$BASE/")" 403

section "Single sign-on via auth_request"
S=(-H "Host: sso.test")
eq "no session: redirected to the login" "$(curl -s -o /dev/null -w '%{http_code} %{redirect_url}' "${S[@]}" "$BASE/")" "302 https://login.example/?rd=/"
eq "session: PHP sees the user" "$(curl -s "${S[@]}" -b 'session=ok' "$BASE/")" "user=alice"
eq "forged identity header dropped" "$(curl -s "${S[@]}" -b 'session=ok' -H 'Remote-User: mallory' "$BASE/")" "user=alice"
eq "forged header without a session: still the login" "$(code "${S[@]}" -H 'Remote-User: mallory' "$BASE/")" 302

section "ACME DNS-01 (Pebble): wildcard certificate"
ROOT=$(mktemp)
issued() {
  curl -sk "https://127.0.0.1:$PEBBLE/roots/0" > "$ROOT" 2>/dev/null || return 1
  [ "$(curl -s -o /dev/null -w '%{http_code}' --cacert "$ROOT" --resolve "shop.nova-e2e.net:$TLS:127.0.0.1" "https://shop.nova-e2e.net:$TLS/")" = 200 ]
}
wait_cert() { for _ in $(seq 90); do issued && return 0; sleep 1; done; return 1; }
check "certificate issued and served" wait_cert
eq "wildcard covers any subdomain" "$(curl -s -o /dev/null -w '%{http_code}' --cacert "$ROOT" --resolve "anything.shop.nova-e2e.net:$TLS:127.0.0.1" "https://anything.shop.nova-e2e.net:$TLS/")" 200
has "names in the certificate" "$(echo | openssl s_client -connect 127.0.0.1:$TLS -servername shop.nova-e2e.net 2>/dev/null | openssl x509 -noout -ext subjectAltName 2>/dev/null)" "*.shop.nova-e2e.net"
rm -f "$ROOT"

printf '\n%d passed, %d failed\n' "$PASS" "$FAIL"
[ "$FAIL" -eq 0 ] || { dc logs nova --tail 30 2>&1 | tail -30; exit 1; }
