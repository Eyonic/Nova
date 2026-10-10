#!/bin/bash
# wp-setup.sh <stack> <http-port>: install WordPress as site "wordpress" (Host wp.nova.test)
set -euo pipefail
stack=$1 port=$2
dir=/mnt/user/appdata/$stack
img=/mnt/user/appdata/nova-images
cd "$dir"
[ -f "$img/wordpress.tar.gz" ] || curl -fsSL -o "$img/wordpress.tar.gz" https://wordpress.org/latest.tar.gz
p() { head -c 48 /dev/urandom | base64 | tr -dc A-Za-z0-9 | head -c 32; }
grep -q '^NOVA_DB_WP_PASSWORD=' .env || echo "NOVA_DB_WP_PASSWORD=$(p)" >> .env
set -a; . ./.env; set +a
grep -q NOVA_DB_WP_PASSWORD compose.yaml || sed -i 's/^      NOVA_DB_SECOND_PASSWORD: \${NOVA_DB_SECOND_PASSWORD}$/&\n      NOVA_DB_WP_PASSWORD: ${NOVA_DB_WP_PASSWORD}/' compose.yaml
docker exec -i "$stack-db-1" mariadb -uroot -p"$NOVA_DB_ROOT_PASSWORD" <<SQL
CREATE DATABASE IF NOT EXISTS nova_wp;
CREATE USER IF NOT EXISTS 'nova_wp'@'%' IDENTIFIED BY '$NOVA_DB_WP_PASSWORD';
GRANT ALL PRIVILEGES ON nova_wp.* TO 'nova_wp'@'%';
FLUSH PRIVILEGES;
SQL
if [ ! -f sites/wordpress/public/wp-load.php ]; then
  mkdir -p sites/wordpress/public
  tar -xzf "$img/wordpress.tar.gz" -C sites/wordpress/public --strip-components=1
  salts=$(for k in AUTH_KEY SECURE_AUTH_KEY LOGGED_IN_KEY NONCE_KEY AUTH_SALT SECURE_AUTH_SALT LOGGED_IN_SALT NONCE_SALT; do echo "define('$k', '$(p)$(p)');"; done)
  cat > sites/wordpress/public/wp-config.php <<PHP
<?php
// Database settings come from NOVA ([site.database] -> DB_* environment).
define('DB_NAME', getenv('DB_DATABASE'));
define('DB_USER', getenv('DB_USERNAME'));
define('DB_PASSWORD', getenv('DB_PASSWORD'));
define('DB_HOST', getenv('DB_HOST') . ':' . getenv('DB_PORT'));
define('DB_CHARSET', 'utf8mb4');
define('DB_COLLATE', '');
$salts
\$table_prefix = 'wp_';
define('WP_DEBUG', false);
define('AUTOMATIC_UPDATER_DISABLED', true);
define('DISABLE_WP_CRON', true);
if (!defined('ABSPATH')) define('ABSPATH', __DIR__ . '/');
require_once ABSPATH . 'wp-settings.php';
PHP
fi
if ! grep -q 'name = "wordpress"' config/nova.toml; then
  cat >> config/nova.toml <<TOML

# WordPress test site (curl -H 'Host: wp.nova.test' http://127.0.0.1:$port/)
[[site]]
name = "wordpress"
hosts = ["wp.nova.test"]
path = "/srv/sites/wordpress"

[site.php]
front_controller = "index.php"
max_children = 8

[site.database]
service = "main"
name = "nova_wp"
user = "nova_wp"
password_env = "NOVA_DB_WP_PASSWORD"
TOML
fi
docker compose up -d --wait nova 2>&1 | tail -1
until curl -sf -o /dev/null http://127.0.0.1:$port/_nova/health/ready; do sleep 1; done
# WordPress installer (idempotent: an installed site just says so).
adminpw=$(p)
curl -s -o /dev/null -w "install: %{http_code}\n" -H 'Host: wp.nova.test' \
  --data-urlencode weblog_title="NOVA WordPress" --data-urlencode user_name=admin \
  --data-urlencode admin_password="$adminpw" --data-urlencode admin_password2="$adminpw" \
  --data-urlencode pw_weak=1 --data-urlencode admin_email=admin@nova.test --data-urlencode blog_public=0 \
  "http://127.0.0.1:$port/wp-admin/install.php?step=2"
echo "version: $(grep '^\$wp_version' sites/wordpress/public/wp-includes/version.php)"
curl -s -o /dev/null -w "home: %{http_code} %{size_download} bytes %{time_total}s\n" -H 'Host: wp.nova.test' http://127.0.0.1:$port/
