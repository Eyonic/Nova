#!/usr/bin/env bash
# Repeatable load tests: NOVA (and optionally stock nginx + PHP-FPM as a
# reference) serving the same files on this machine.
#
#   tests/performance/run.sh                    # NOVA only, image nova:dev
#   REF=1 tests/performance/run.sh              # also nginx 1.29 + php-fpm
#   NOVA_IMAGE=nova:old tests/performance/run.sh
#   SAVE_BASELINE=1 tests/performance/run.sh    # store results as the baseline
#   NOVA_BIN=target/release/nova tests/performance/run.sh   # test a binary in the image
#   SCENARIOS="static-1k static-1k-br" tests/performance/run.sh
#   LABEL=after tests/performance/run.sh        # name the run (results/<stamp>-<label>)
#   NOVA_TOML=my.toml tests/performance/run.sh   # another NOVA configuration
#
# With tests/performance/baseline.json present, every NOVA scenario must
# reach at least (1 - TOLERANCE) of its baseline requests/s (default 15%),
# otherwise the script exits non-zero. Compare on the same machine only.
#
# Needs: docker or podman, oha (cargo install oha), python3, openssl.
# Servers run with host networking; the server gets cores 0..N/2-1 and the
# load generator the other half (taskset), so they do not compete.
set -uo pipefail
cd "$(dirname "$0")"
HERE=$PWD
NOVA_IMAGE=${NOVA_IMAGE:-nova:dev}
DURATION=${DURATION:-15s}
TOLERANCE=${TOLERANCE:-0.15}
OHA=${OHA:-oha}
command -v "$OHA" >/dev/null || { echo "oha not found (cargo install oha)"; exit 2; }
docker info >/dev/null 2>&1 || { echo "docker/podman not available"; exit 2; }

CPUS=$(nproc); HALF=$((CPUS / 2))
SERVER_CPUS="0-$((HALF - 1))"; LOAD_CPUS="$HALF-$((CPUS - 1))"
WORK=$(mktemp -d); trap 'docker rm -f nova-perf-srv nova-perf-fpm >/dev/null 2>&1; rm -rf "$WORK"' EXIT
STAMP=$(date +%Y%m%d-%H%M%S)
OUT=$HERE/results/$STAMP${LABEL:+-$LABEL}; mkdir -p "$OUT"

# Identical document root for every server.
mkdir -p "$WORK/site/public" "$WORK/conf"
cp ../../sites/example/public/index.html "$WORK/site/public/"
cp ../../sites/example/public/images/hero.jpg "$WORK/site/public/hero.jpg"
head -c 100000 /dev/zero | tr '\0' 'a' | fold -w 120 > "$WORK/site/public/100k.txt"
printf '<?php\necho "hello from php ", PHP_VERSION;\n' > "$WORK/site/public/hello.php"
openssl req -x509 -newkey ec -pkeyopt ec_paramgen_curve:prime256v1 -nodes \
  -keyout "$WORK/conf/key.pem" -out "$WORK/conf/cert.pem" -days 2 -subj /CN=localhost 2>/dev/null
cp nginx.conf fpm.conf "$WORK/conf/"
chmod -R a+rX "$WORK"

# Rootless containers cannot use --cpuset-cpus: pin the threads of the
# container's own cgroup (and nothing else) instead.
pin() {
  local c pid rel dir
  for c in nova-perf-srv nova-perf-fpm; do
    pid=$(docker inspect -f '{{.State.Pid}}' "$c" 2>/dev/null) || continue
    [ "${pid:-0}" -gt 0 ] 2>/dev/null || continue
    rel=$(sed -n 's/^0:://p' "/proc/$pid/cgroup"); dir=/sys/fs/cgroup${rel%/container}
    case "$dir" in */libpod-*.scope | */docker-*.scope | */docker/*) ;; *) continue ;; esac
    find "$dir" -name cgroup.threads -exec cat {} + 2>/dev/null |
      while read -r t; do taskset -p -c "$SERVER_CPUS" "$t" >/dev/null 2>&1; done
  done
}

load() { # server label url oha-args...
  local server=$1 label=$2 url=$3; shift 3
  "$OHA" --no-tui --disable-compression -z 3s -c 64 "$@" "$url" >/dev/null 2>&1 # warm-up
  pin
  taskset -c "$LOAD_CPUS" "$OHA" --no-tui --disable-compression --output-format json \
    -z "$DURATION" "$@" "$url" > "$OUT/$server-$label.json" 2>/dev/null
}

ALL="static-1k static-100k image-470k h2-static-1k php-hello static-1k-br static-100k-br"
want() { [[ " ${SCENARIOS:-$ALL} " == *" $1 "* ]]; }
scenarios() { # server http-port https-port
  want static-1k && load "$1" static-1k "http://127.0.0.1:$2/index.html" -c 256
  want static-100k && load "$1" static-100k "http://127.0.0.1:$2/100k.txt" -c 64
  want image-470k && load "$1" image-470k "http://127.0.0.1:$2/hero.jpg" -c 64
  want h2-static-1k && load "$1" h2-static-1k "https://localhost:$3/index.html" --http2 -c 32 -p 8 --insecure
  want php-hello && load "$1" php-hello "http://127.0.0.1:$2/hello.php" -c 64
  # What a browser gets: Accept-Encoding sent (the response stays encoded).
  want static-1k-br && load "$1" static-1k-br "http://127.0.0.1:$2/index.html" -c 256 -H 'Accept-Encoding: br, gzip'
  want static-100k-br && load "$1" static-100k-br "http://127.0.0.1:$2/100k.txt" -c 64 -H 'Accept-Encoding: br, gzip'
  return 0
}

wait_up() { for _ in $(seq 60); do curl -sf -o /dev/null "$1" && return 0; sleep 1; done; return 1; }

echo "NOVA ($NOVA_IMAGE)"
docker run -d --name nova-perf-srv --network host \
  --cap-drop ALL --cap-add CHOWN --cap-add DAC_OVERRIDE --cap-add FOWNER \
  --cap-add SETUID --cap-add SETGID --cap-add KILL --security-opt no-new-privileges \
  --tmpfs /run/nova --tmpfs /var/lib/nova --tmpfs /tmp -e NOVA_MODE=production \
  -v "$(realpath "${NOVA_TOML:-$HERE/nova.toml}"):/etc/nova/nova.toml:ro" -v "$WORK/site:/srv/sites/bench:ro" \
  ${NOVA_BIN:+-v "$(realpath "$NOVA_BIN"):/usr/local/bin/nova:ro"} \
  "$NOVA_IMAGE" >/dev/null || exit 1
wait_up http://127.0.0.1:9005/index.html || { docker logs nova-perf-srv | tail; exit 1; }
# Let the Script Optimizer precompress the text files first.
for _ in $(seq 60); do
  curl -s -D - -o /dev/null -H 'Accept-Encoding: br' http://127.0.0.1:9005/100k.txt |
    grep -qi 'content-encoding: br' && break
  sleep 1
done
scenarios nova 9005 9445
docker rm -f nova-perf-srv >/dev/null

if [ -n "${REF:-}" ]; then
  echo "nginx + php-fpm (reference)"
  docker run -d --name nova-perf-fpm --network host -v "$WORK/site:/site:ro" \
    -v "$WORK/conf/fpm.conf:/usr/local/etc/php-fpm.d/zz-bench.conf:ro" php:8.5-fpm >/dev/null
  docker run -d --name nova-perf-srv --network host -v "$WORK/site:/site:ro" -v "$WORK/conf:/conf:ro" \
    -v "$WORK/conf/nginx.conf:/etc/nginx/nginx.conf:ro" nginx:1.29 >/dev/null
  wait_up http://127.0.0.1:9001/index.html || { docker logs nova-perf-srv | tail; exit 1; }
  scenarios nginx 9001 9441
fi

python3 -I - "$OUT" "$HERE/baseline.json" "$TOLERANCE" "${SAVE_BASELINE:-}" <<'PY'
import json, os, sys
out, baseline_path, tol, save = sys.argv[1], sys.argv[2], float(sys.argv[3]), sys.argv[4]
rows = {}
for f in sorted(os.listdir(out)):
    d = json.load(open(os.path.join(out, f)))
    s, p = d["summary"], d["latencyPercentiles"]
    ok = d.get("statusCodeDistribution", {}).get("200", 0)
    total = sum(d.get("statusCodeDistribution", {}).values()) or 1
    rows[f[:-5]] = {"rps": round(s["requestsPerSec"]), "p50_ms": round(p["p50"] * 1e3, 2),
                    "p99_ms": round(p["p99"] * 1e3, 2), "ok_ratio": round(ok / total, 4)}
base = json.load(open(baseline_path)) if os.path.exists(baseline_path) else {}
lines = ["| scenario | req/s | p50 ms | p99 ms | 200s | vs baseline |", "|---|---:|---:|---:|---:|---:|"]
failed = []
for k, r in rows.items():
    vs = ""
    if k in base and k.startswith("nova-"):
        ratio = r["rps"] / base[k]["rps"]
        vs = f"{ratio:.0%}"
        if ratio < 1 - tol or r["ok_ratio"] < 0.999:
            failed.append(k)
    lines.append(f'| {k} | {r["rps"]} | {r["p50_ms"]} | {r["p99_ms"]} | {r["ok_ratio"]:.1%} | {vs} |')
report = "\n".join(lines)
print(report)
open(os.path.join(out, "summary.md"), "w").write(report + "\n")
json.dump(rows, open(os.path.join(out, "summary.json"), "w"), indent=2)
if save:
    json.dump({k: v for k, v in rows.items() if k.startswith("nova-")}, open(baseline_path, "w"), indent=2)
    print(f"baseline saved to {baseline_path}")
if failed:
    print("REGRESSION (more than {:.0%} below baseline or non-200 responses): {}".format(tol, ", ".join(failed)))
    sys.exit(1)
PY
