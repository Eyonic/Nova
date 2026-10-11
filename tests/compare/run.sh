#!/bin/bash
# Head-to-head on Unraid: NOVA vs nginx+PHP-FPM vs FrankenPHP (Caddy).
# Every server on the same 4 cores (4-7); load from a container on the same
# Docker network on other cores; browser Accept-Encoding; 3 interleaved rounds.
#   run.sh setup | check | bench [scenarios...] | teardown
set -u
B=/mnt/cache/appdata/nova-bench; SITES=/mnt/cache/appdata/nova-exp/sites; NET=nova-exp_default
OHA=/mnt/user/appdata/nova-images/oha; CPUS=4-7; LOADCPUS=8-11,20-23
eval "$(grep NOVA_DB_WP_PASSWORD /mnt/cache/appdata/nova-exp/.env)"   # the test stack's WordPress DB password
S=/tmp/storage
LENV=(-e APP_ENV=production -e APP_DEBUG=false -e APP_KEY=base64:QyPJje5vr6aGFmKvX7sWIpejADaMxhut+CCgdKZKqwk= -e APP_URL=http://laravel.nova.test
      -e SESSION_DRIVER=file -e CACHE_STORE=file -e LOG_CHANNEL=stderr -e DB_CONNECTION=sqlite -e LARAVEL_STORAGE_PATH=$S
      -e APP_SERVICES_CACHE=$S/bootstrap/services.php -e APP_PACKAGES_CACHE=$S/bootstrap/packages.php -e APP_CONFIG_CACHE=$S/bootstrap/config.php
      -e APP_ROUTES_CACHE=$S/bootstrap/routes-v7.php -e APP_EVENTS_CACHE=$S/bootstrap/events.php)
WENV=(-e DB_DATABASE=nova_wp -e DB_USERNAME=nova_wp -e DB_PASSWORD=$NOVA_DB_WP_PASSWORD -e DB_HOST=db -e DB_PORT=3306)
MK="mkdir -p $S/framework/sessions $S/framework/views $S/framework/cache/data $S/logs $S/bootstrap"

setup() {
  docker rm -f bench-php bench-nginx bench-fp >/dev/null 2>&1
  docker run -d --name bench-php --network $NET --network-alias php --cpuset-cpus $CPUS \
    -v $SITES:/srv/sites:ro -v $B/${PHPINI:-php.ini}:/usr/local/etc/php/conf.d/zz-bench.ini:ro -v $B/fpm.conf:/usr/local/etc/php-fpm.conf:ro \
    "${WENV[@]}" "${LENV[@]}" --entrypoint sh localhost/nova:dnd3 -c "$MK && chown -R www-data $S && exec php-fpm -F -y /usr/local/etc/php-fpm.conf" >/dev/null
  docker run -d --name bench-nginx --network $NET --cpuset-cpus $CPUS -v $SITES:/srv/sites:ro -v $B/nginx.conf:/etc/nginx/nginx.conf:ro nginx:alpine >/dev/null
  docker run -d --name bench-fp --network $NET --cpuset-cpus $CPUS -v $SITES:/srv/sites:ro -v $B/Caddyfile:/etc/frankenphp/Caddyfile:ro \
    "${WENV[@]}" "${LENV[@]}" -e LARAVEL_OCTANE=1 -e APP_BASE_PATH=/srv/sites/laravel -e MAX_REQUESTS=100000000 \
    --entrypoint sh localhost/frankenphp:bench -c "$MK && exec frankenphp run --config /etc/frankenphp/Caddyfile" >/dev/null
  docker update --cpuset-cpus $CPUS nova-exp-nova-1 >/dev/null
  sleep 8
}

target() { case $1 in nova) echo http://nova-exp-nova-1:8080;; nginx) echo http://bench-nginx;; fp) echo http://bench-fp;; esac; }
scen() { # -> host path
  case $1 in
    html) echo "static.test /index.html";; jpg) echo "static.test /images/hero.jpg";;
    wp) echo "wp.nova.test /";; up) echo "laravel.nova.test /up";; lhome) echo "laravel.nova.test /";;
  esac
}
CURL() { docker run --rm --network $NET --entrypoint curl localhost/nova:dnd3 "$@"; }
AE="${AE:-Accept-Encoding: gzip, deflate, br, zstd}"

check() {
  for s in html jpg wp up lhome; do
    read -r host path <<<"$(scen $s)"
    for srv in nova nginx fp; do
      printf '%-6s %-6s ' $s $srv
      CURL -s -o /dev/null -w '%{http_code} %{size_download}B %{content_type} enc=%header{content-encoding}\n' -H "Host: $host" -H "$AE" "$(target $srv)$path"
    done
  done
}

oha() { # url host conc -> "rps p99ms non200"
  docker run --rm --network $NET --cpuset-cpus $LOADCPUS -v $OHA:/oha:ro --entrypoint /oha localhost/nova:dnd3 \
    --no-tui -z ${D:-10s} -c $3 -H "Host: $2" -H "$AE" "$1" 2>/dev/null | awk '
    /Requests\/sec:/ {rps=int($2)}
    /99.00% in/ {p99=$3; u=$4; if (u ~ /^sec/) p99*=1000; if (u ~ /^us/ || u ~ /^µs/) p99/=1000}
    /^ *\[[0-9]+\]/ {code=$1; gsub(/[\[\]]/,"",code); if (code!="200") bad+=$2}
    END {printf "%d %.1f %d\n", rps, p99, bad+0}'
}

bench() {
  for s in "$@"; do
    read -r host path <<<"$(scen $s)"
    c=32; [ $s = html ] || [ $s = jpg ] && c=64
    declare -A R=()
    for r in 1 2 3; do
      order="nova nginx fp"; [ $r = 2 ] && order="fp nova nginx"; [ $r = 3 ] && order="nginx fp nova"
      for srv in $order; do
        D=3s oha "$(target $srv)$path" $host $c >/dev/null
        R[$srv]="${R[$srv]:-} $(oha "$(target $srv)$path" $host $c | tr ' ' '/')"
      done
    done
    for srv in nova nginx fp; do
      med=$(echo ${R[$srv]} | tr ' ' '\n' | cut -d/ -f1 | sort -n | sed -n 2p)
      printf '%-6s %-6s median %7s req/s   runs:%s\n' $s $srv "$med" "${R[$srv]}"
    done
    unset R
  done
}

teardown() { docker rm -f bench-php bench-nginx bench-fp >/dev/null 2>&1; docker update --cpuset-cpus "" nova-exp-nova-1 >/dev/null 2>&1; }

cmd=${1:-}; shift || true
case $cmd in setup) setup;; check) check;; bench) bench "$@";; teardown) teardown;; *) echo "usage";; esac
