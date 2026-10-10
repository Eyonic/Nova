#!/usr/bin/env bash
# One command for every NOVA check. Prints a PASS/FAIL table, exits non-zero
# when anything failed. Works with Docker or Podman (via a `docker` shim).
#
#   scripts/check.sh            # quick: fmt, clippy (warnings fail), unit tests
#   scripts/check.sh full       # quick + integration suite + NOVA Live browser
#                               #   tests + performance regression vs baseline
#   scripts/check.sh integration|browser|perf   # one stage
#
# Logs: target/check-logs/<stage>.log (also the place to look after a FAIL).
set -uo pipefail
cd "$(dirname "$0")/.."
MODE=${1:-quick}
LOGS=target/check-logs
mkdir -p "$LOGS"
declare -a NAMES RESULTS TIMES

strip() { sed 's/\x1b\[[0-9;]*m//g'; }

stage() { # name command...
  local name=$1; shift
  local t0=$SECONDS log="$LOGS/$name.log"
  printf '%-12s ... ' "$name"
  if "$@" >"$log" 2>&1; then
    NAMES+=("$name"); RESULTS+=("PASS"); TIMES+=($((SECONDS - t0)))
    printf 'PASS (%ss)\n' $((SECONDS - t0))
  else
    NAMES+=("$name"); RESULTS+=("FAIL"); TIMES+=($((SECONDS - t0)))
    printf 'FAIL (%ss) — %s\n' $((SECONDS - t0)) "$log"
    strip <"$log" | grep -E "^(error|warning)|FAILED|panicked|✗" | head -8 | sed 's/^/    /'
  fi
}

fmt() {
  # Local rustfmt when available (no container write access needed).
  if command -v cargo >/dev/null && cargo +1.99 fmt --version >/dev/null 2>&1; then
    cargo +1.99 fmt --all --check
  else
    scripts/cargo.sh fmt --all --check
  fi
}
clippy() {
  scripts/cargo.sh clippy --workspace --all-targets --locked -- -D warnings 2>&1 | strip
  return "${PIPESTATUS[0]}"
}
unit() {
  local out; out=$(scripts/cargo.sh test --workspace --locked 2>&1 | strip); local rc=$?
  echo "$out"
  [ $rc -eq 0 ] && ! grep -q "FAILED" <<<"$out"
}
integration() {
  teardown   # never reuse an earlier run's database (its passwords differ)
  # Leaves the stack running for the browser stage (KEEP=1); torn down below.
  KEEP=1 tests/integration/run.sh 2>&1 | strip
  local rc=${PIPESTATUS[0]}
  return "$rc"
}
browser() {
  local port=${NOVA_TEST_PORT:-18088}
  if ! curl -sf -o /dev/null "http://127.0.0.1:$port/_nova/health/live"; then
    echo "no stack on :$port (run the integration stage first, or set NOVA_URL)"; return 1
  fi
  NOVA_URL=http://localhost:$port tests/browser/run.sh 2>&1 | strip
  return "${PIPESTATUS[0]}"
}
teardown() {
  # compose.yaml requires these variables even to tear down; any value works.
  NOVA_DB_ROOT_PASSWORD=x NOVA_DB_EXAMPLE_PASSWORD=x NOVA_DB_SECOND_PASSWORD=x \
    docker compose -f compose.yaml -p nova-test down -v --remove-orphans >/dev/null 2>&1
  if docker volume ls --format '{{.Name}}' | grep -q '^nova-test_'; then
    echo "warning: nova-test volumes still present after teardown" >&2
  fi
}
perf() {
  command -v "${OHA:-oha}" >/dev/null || [ -x "$HOME/.local/bin/oha" ] || { echo "oha not installed (cargo install oha)"; return 1; }
  OHA=${OHA:-$(command -v oha || echo "$HOME/.local/bin/oha")} tests/performance/run.sh 2>&1 | strip
  return "${PIPESTATUS[0]}"
}

case $MODE in
  quick)
    stage fmt fmt; stage clippy clippy; stage unit unit ;;
  full)
    stage fmt fmt; stage clippy clippy; stage unit unit
    stage integration integration
    stage browser browser
    teardown
    stage perf perf ;;
  integration) stage integration integration; teardown ;;
  browser) stage browser browser ;;
  perf) stage perf perf ;;
  *) echo "usage: $0 [quick|full|integration|browser|perf]"; exit 2 ;;
esac

echo
printf '%-12s %-5s %s\n' STAGE RESULT SECONDS
fail=0
for i in "${!NAMES[@]}"; do
  printf '%-12s %-5s %s\n' "${NAMES[$i]}" "${RESULTS[$i]}" "${TIMES[$i]}"
  [ "${RESULTS[$i]}" = FAIL ] && fail=1
done
# Integration count, when that stage ran.
grep -hE "[0-9]+ passed, [0-9]+ failed" "$LOGS/integration.log" 2>/dev/null | tail -1 | strip | sed 's/^/integration: /'
exit $fail
