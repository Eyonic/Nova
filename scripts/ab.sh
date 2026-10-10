#!/usr/bin/env bash
# A/B two NOVA binaries on this machine with the performance harness.
# Rounds alternate A and B so background noise hits both alike; the
# result is the median per scenario and the B/A ratio.
#
#   scripts/ab.sh old/nova new/nova
#   ROUNDS=3 SCENARIOS="static-1k h2-static-1k php-hello" scripts/ab.sh A B
#   NOVA_TOML=my.toml scripts/ab.sh A B     # same config for both
#
# Binaries must run in the nova:dev image (build them with
# `scripts/cargo.sh build --release -p nova-cli`).
set -uo pipefail
cd "$(dirname "$0")/.."
[ $# -eq 2 ] || { echo "usage: $0 <binary A> <binary B>"; exit 2; }
A=$(realpath "$1"); B=$(realpath "$2")
ROUNDS=${ROUNDS:-2}
export DURATION=${DURATION:-10s}
export SCENARIOS=${SCENARIOS:-static-1k static-100k image-470k h2-static-1k php-hello static-1k-br}
export OHA=${OHA:-$(command -v oha || echo "$HOME/.local/bin/oha")}
STAMP=ab-$(date +%Y%m%d-%H%M%S)
for r in $(seq "$ROUNDS"); do
  order=(A B); [ $((r % 2)) = 0 ] && order=(B A)   # swap the order every other round
  for side in "${order[@]}"; do
    bin=$A; [ "$side" = B ] && bin=$B
    echo "round $r: $side ($(basename "$bin"))"
    NOVA_BIN=$bin LABEL=$STAMP-$side-$r tests/performance/run.sh >/dev/null 2>&1 || true
  done
done
python3 -I - tests/performance/results "$STAMP" "$A" "$B" <<'PY'
import json, os, statistics, sys
root, stamp, a, b = sys.argv[1:5]
runs = {"A": [], "B": []}
for d in sorted(os.listdir(root)):
    if d.split("-", 2)[-1].startswith(stamp):
        side = d.split("-")[-2]
        f = os.path.join(root, d, "summary.json")
        if os.path.exists(f) and side in runs:
            runs[side].append(json.load(open(f)))
if not runs["A"] or not runs["B"]:
    sys.exit("no results (is the nova:dev image built?)")
scen = sorted({k for r in runs["A"] + runs["B"] for k in r if k.startswith("nova-")})
print(f"\nA = {a}\nB = {b}\n")
print(f"{'scenario':<22}{'A req/s':>12}{'B req/s':>12}{'B/A':>9}   p99 A/B ms")
for s in scen:
    ra = [r[s]["rps"] for r in runs["A"] if s in r]; rb = [r[s]["rps"] for r in runs["B"] if s in r]
    pa = [r[s]["p99_ms"] for r in runs["A"] if s in r]; pb = [r[s]["p99_ms"] for r in runs["B"] if s in r]
    ma, mb = statistics.median(ra), statistics.median(rb)
    flag = "  better" if mb / ma > 1.10 else ("  WORSE" if mb / ma < 0.90 else "")
    print(f"{s[5:]:<22}{ma:>12.0f}{mb:>12.0f}{mb/ma:>8.0%}   {statistics.median(pa):.2f}/{statistics.median(pb):.2f}{flag}")
print("\n(>10% marked: about the noise level of single local runs)")
PY
