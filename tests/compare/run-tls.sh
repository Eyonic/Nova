#!/bin/bash
# HTTPS head-to-head: HTTP/2 and HTTP/3. Same servers as run.sh (setup there
# with NGINXCONF=nginx-tls.conf CADDYFILE=Caddyfile-tls), same cores.
#   run-tls.sh h2|h3 scenarios...
set -u
NET=nova-exp_default; LOADCPUS=8-11,20-23; OHA=/mnt/user/appdata/nova-images/oha-h3
AE="Accept-Encoding: gzip, deflate, br, zstd"
endpoint() { case $1 in nova) echo nova-exp-nova-1:8443;; nginx) echo bench-nginx:443;; fp) echo bench-fp:443;; esac; }
scen() { case $1 in html) echo "static.test /index.html";; jpg) echo "static.test /images/hero.jpg";; wp) echo "wp.nova.test /";; up) echo "laravel.nova.test /up";; esac; }
oha() { # proto host path endpoint c p -> rps p99 bad
  local v=2; [ $1 = h3 ] && v=3
  docker run --rm --network $NET --cpuset-cpus $LOADCPUS -v $OHA:/oha:ro --entrypoint /oha localhost/nova:dnd3 \
    --no-tui --insecure --http-version $v -z ${D:-10s} -c $5 -p $6 -H "$AE" --connect-to "$2:443:$4" "https://$2$3" 2>/dev/null | awk "
    /Requests\/sec:/ {rps=int(\$2)}
    /99.00% in/ {p99=\$3; u=\$4; if (u ~ /^sec/) p99*=1000; if (u ~ /^us/ || u ~ /^µs/) p99/=1000}
    /^ *\[[0-9]+\]/ {code=\$1; gsub(/[\[\]]/,\"\",code); if (code!=\"200\") bad+=\$2}
    END {printf \"%d/%.1f/%d\n\", rps, p99, bad+0}"
}
proto=$1; shift
for s in "$@"; do
  read -r host path <<<"$(scen $s)"
  c=8; p=8; case $s in wp|up) c=4; p=8;; esac
  declare -A R=()
  for r in 1 2 3; do
    order="nova nginx fp"; [ $r = 2 ] && order="fp nova nginx"; [ $r = 3 ] && order="nginx fp nova"
    for srv in $order; do
      D=3s oha $proto $host $path $(endpoint $srv) $c $p >/dev/null
      R[$srv]="${R[$srv]:-} $(oha $proto $host $path $(endpoint $srv) $c $p)"
    done
  done
  for srv in nova nginx fp; do
    med=$(echo ${R[$srv]} | tr " " "\n" | cut -d/ -f1 | sort -n | sed -n 2p)
    printf "%-3s %-5s %-6s median %7s req/s   runs:%s\n" $proto $s $srv "$med" "${R[$srv]}"
  done
  unset R
done
