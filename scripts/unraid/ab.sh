#!/usr/bin/env bash
# ab.sh [rounds] [scenarios...]: A/B stable (:38080/:38443) vs experimental (:39080/:39443)
# on this server. Rounds alternate the order so background load hits both alike.
OHA=/mnt/user/appdata/nova-images/oha
ROUNDS=${1:-3}; shift || true
SCEN=${*:-small img jsbr php h2}
D=${D:-8s}
run() { # stack scenario -> req/s
  local http=$1 https=$2 s=$3 args url
  case $s in
    small) url=http://127.0.0.1:$http/index.html; args="-c 128 --disable-compression" ;;
    img)   url=http://127.0.0.1:$http/images/hero.jpg; args="-c 64 --disable-compression" ;;
    jsbr)  url=http://127.0.0.1:$http/js/cart.js; args="-c 128 --disable-compression -H Accept-Encoding:br" ;;
    php)   url=http://127.0.0.1:$http/info.php; args="-c 64 --disable-compression" ;;
    phpbr) url=http://127.0.0.1:$http/info.php; args="-c 64 --disable-compression -H Accept-Encoding:br" ;;
    h2)    url=https://localhost:$https/index.html; args="--http2 -c 16 -p 8 --insecure --disable-compression" ;;
  esac
  taskset -c 16-23 $OHA --no-tui -z 2s $args "$url" >/dev/null 2>&1
  # -> "<req/s> <p99 ms> <non-200 count>"
  taskset -c 16-23 $OHA --no-tui -z $D $args "$url" 2>/dev/null | awk '
    /Requests\/sec:/ {rps=int($2)}
    /99.00% in/ {p99=$3; if ($4=="secs") p99*=1000; if ($4=="us") p99/=1000}
    /^ *\[[0-9]+\]/ {code=$1; gsub(/[\[\]]/,"",code); if (code!="200") bad+=$2}
    END {printf "%d %.2f %d\n", rps, p99, bad+0}'
}
for s in $SCEN; do
  A=(); B=()
  for r in $(seq $ROUNDS); do
    if [ $((r % 2)) = 1 ]; then a=$(run 38080 38443 $s); b=$(run 39080 39443 $s); else b=$(run 39080 39443 $s); a=$(run 38080 38443 $s); fi
    A+=("$a"); B+=("$b")
  done
  med() { printf '%s\n' "$@" | awk '{print $1}' | sort -n | awk '{v[NR]=$1} END {print (NR%2 ? v[(NR+1)/2] : (v[NR/2]+v[NR/2+1])/2)}'; }
  ma=$(med "${A[@]}"); mb=$(med "${B[@]}")
  list() { printf '%s\n' "$@" | awk '{printf "%s%s", (NR>1?",":""), $1}'; }
  bad=$(printf '%s\n' "${A[@]}" "${B[@]}" | awk '{s+=$3} END {print s+0}')
  awk -v s="$s" -v ma="$ma" -v mb="$mb" -v la="$(list "${A[@]}")" -v lb="$(list "${B[@]}")" -v pa="${A[0]}" -v pb="${B[0]}" -v bad="$bad" 'BEGIN {
    split(pa,x," "); split(pb,y," ");
    printf "%-6s stable %8d [%s] | exp %8d [%s] | exp/stable %6.1f%% | p99 %s/%s ms | non-200 %d\n", s, ma, la, mb, lb, 100*mb/ma, x[2], y[2], bad }'
done
