#!/bin/bash
# cdc-setup.sh: enable database change feeds on the nova-exp stack only
set -euo pipefail
cd /mnt/user/appdata/nova-exp
p() { head -c 48 /dev/urandom | base64 | tr -dc A-Za-z0-9 | head -c 32; }
grep -q '^NOVA_DB_CDC_PASSWORD=' .env || echo "NOVA_DB_CDC_PASSWORD=$(p)" >> .env
set -a; . ./.env; set +a
grep -q NOVA_DB_CDC_PASSWORD compose.yaml || sed -i 's/^      NOVA_DB_WP_PASSWORD: \${NOVA_DB_WP_PASSWORD}$/&\n      NOVA_DB_CDC_PASSWORD: ${NOVA_DB_CDC_PASSWORD}/' compose.yaml
grep -q -- '--log-bin' compose.yaml || sed -i 's#^    image: mariadb:11.8$#&\n    command: ["--log-bin=mariadb-bin", "--server-id=1", "--binlog-format=ROW", "--binlog-row-image=MINIMAL", "--binlog-expire-logs-seconds=86400", "--max-binlog-total-size=1G"]#' compose.yaml
grep -n "command:\|CDC" compose.yaml
docker compose up -d --wait db 2>&1 | tail -1
docker exec -i nova-exp-db-1 mariadb -uroot -p"$NOVA_DB_ROOT_PASSWORD" <<SQL
CREATE USER IF NOT EXISTS 'nova_cdc'@'%' IDENTIFIED BY '$NOVA_DB_CDC_PASSWORD';
GRANT REPLICATION SLAVE, BINLOG MONITOR ON *.* TO 'nova_cdc'@'%';
FLUSH PRIVILEGES;
SHOW BINLOG STATUS;
SQL
f=/mnt/cache/appdata/nova-exp/config/nova.toml
grep -q 'services.database.main.changes' $f || sed -i 's/^\[services.database.main\]$/&/; /^port = 3306$/a\
\
[services.database.main.changes]   # binlog -> db:<table> NOVA Live channels\
user = "nova_cdc"\
password_env = "NOVA_DB_CDC_PASSWORD"' $f
grep -n -A3 "services.database.main" $f
