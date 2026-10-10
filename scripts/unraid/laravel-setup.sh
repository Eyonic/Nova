#!/bin/bash
# laravel-setup.sh <stack> <http-port>: install a Laravel skeleton as site "laravel" (Host laravel.nova.test)
set -euo pipefail
stack=$1 port=$2
img=/mnt/user/appdata/nova-images
src=$img/laravel-src
if [ ! -f "$src/artisan" ]; then
  docker run --rm -v "$img:/work" -w /work composer:2 \
    create-project --no-interaction --prefer-dist --no-dev laravel/laravel laravel-src 2>&1 | tail -2
fi
pool=/mnt/cache/appdata/$stack
if [ ! -f "$pool/sites/laravel/artisan" ]; then
  mkdir -p "$pool/sites/laravel" && cp -a "$src/." "$pool/sites/laravel/"
  chmod -R a+rX "$pool/sites/laravel"
fi
key=$(grep '^APP_KEY=' "$pool/sites/laravel/.env" | cut -d= -f2-)
if ! grep -q 'name = "laravel"' "$pool/config/nova.toml"; then
  cat >> "$pool/config/nova.toml" <<TOML

# Laravel test site (curl -H 'Host: laravel.nova.test' http://127.0.0.1:$port/)
[[site]]
name = "laravel"
hosts = ["laravel.nova.test"]
path = "/srv/sites/laravel"

[site.php]
front_controller = "index.php"
max_children = 8

[site.env]
APP_ENV = "production"
APP_DEBUG = "false"
APP_KEY = "$key"
APP_URL = "http://laravel.nova.test"
SESSION_DRIVER = "file"
CACHE_STORE = "file"
LOG_CHANNEL = "stderr"
DB_CONNECTION = "sqlite"
TOML
fi
cd /mnt/user/appdata/$stack && docker compose restart nova >/dev/null 2>&1
until curl -sf -o /dev/null http://127.0.0.1:$port/_nova/health/ready; do sleep 1; done
grep -h "laravel/framework\"" -A1 "$pool/sites/laravel/vendor/composer/installed.json" | grep version | head -1
for i in 1 2; do curl -s -o /tmp/lv -D /tmp/lvh -w "home: %{http_code} %{size_download} bytes %{time_total}s\n" -H 'Host: laravel.nova.test' http://127.0.0.1:$port/; done
tr -d '\r' < /tmp/lvh | grep -ciE '^set-cookie' | sed 's/^/cookies set: /'; tr -d '\r' < /tmp/lvh | grep -i 'nova-cache' || echo "nova-cache: (not cached, correct)"
grep -o '<title>[^<]*' /tmp/lv | head -1; rm -f /tmp/lv /tmp/lvh
