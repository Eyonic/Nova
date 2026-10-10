#!/usr/bin/env bash
# scripts/unraid/deploy.sh <name> <image-tar> <image-ref> <http-port> <https-port> <source-dir>
#
#   podman save -o /tmp/nova.tar localhost/nova:dev
#   UNRAID=root@192.168.1.11 scripts/unraid/deploy.sh nova-stable /tmp/nova.tar localhost/nova:dev 38080 38443 .
#
# Uses plain ssh/scp (an SSH key, or you type the password). One shared
# connection is kept open for 30 minutes.
# Installs one isolated NOVA stack on the Unraid server:
#   /mnt/user/appdata/<name>/{compose.yaml,.env,config,sites,docker}
#   compose project <name>: own MariaDB on a private network, no DB port published.
set -euo pipefail
name=$1 tar=$2 ref=$3 http=$4 https=$5 src=$6
HOST=${UNRAID:-root@192.168.1.11}
SSHOPTS=(-o ControlMaster=auto -o "ControlPath=${XDG_RUNTIME_DIR:-/tmp}/nova-unraid-%C" -o ControlPersist=30m)
R() { ssh "${SSHOPTS[@]}" "$HOST" "$@"; }
R=R
scp_() { scp "${SSHOPTS[@]}" "$@"; }
dir=/mnt/user/appdata/$name
$R "mkdir -p $dir/config /mnt/user/appdata/nova-images"
echo "copying image"; scp_ -q "$tar" "$HOST:/mnt/user/appdata/nova-images/$(basename "$tar")"
$R "docker load -q -i /mnt/user/appdata/nova-images/$(basename "$tar")"
echo "copying sites and config"
tar -C "$src" -czf - sites config docker/mariadb | $R "tar -C $dir -xzf -"
# Passwords are created on the server once and kept across redeploys.
$R "cd $dir && [ -f .env ] || { p(){ head -c 32 /dev/urandom | base64 | tr -dc A-Za-z0-9 | head -c 24; }; printf 'NOVA_MODE=production\nNOVA_DB_ROOT_PASSWORD=%s\nNOVA_DB_EXAMPLE_PASSWORD=%s\nNOVA_DB_SECOND_PASSWORD=%s\n' \$(p) \$(p) \$(p) > .env; chmod 600 .env; }"
$R "cat > $dir/compose.yaml" <<YAML
# NOVA test stack "$name" (managed from the NOVA repo; safe to remove with
#   docker compose -p $name down -v && rm -rf $dir)
name: $name
services:
  nova:
    image: $ref
    ports:
      - "$http:8080"
      - "$https:8443"
      - "$https:8443/udp"
    environment:
      NOVA_MODE: \${NOVA_MODE:-production}
      NOVA_DB_EXAMPLE_PASSWORD: \${NOVA_DB_EXAMPLE_PASSWORD}
      NOVA_DB_SECOND_PASSWORD: \${NOVA_DB_SECOND_PASSWORD}
    volumes:
      - nova-state:/var/lib/nova
      - ./sites:/srv/sites:ro
      - ./config/nova.toml:/etc/nova/nova.toml:ro
    read_only: true
    tmpfs: [/run/nova:mode=0755, /tmp]
    cap_drop: [ALL]
    cap_add: [CHOWN, DAC_OVERRIDE, FOWNER, SETUID, SETGID, KILL]
    security_opt: [no-new-privileges:true]
    depends_on:
      db: {condition: service_healthy}
    restart: unless-stopped
    stop_grace_period: 30s
    deploy:
      resources:
        limits: {cpus: "4", memory: 1g, pids: 512}
  db:
    image: mariadb:11.8
    environment:
      MARIADB_ROOT_PASSWORD: \${NOVA_DB_ROOT_PASSWORD}
      NOVA_DB_EXAMPLE_PASSWORD: \${NOVA_DB_EXAMPLE_PASSWORD}
      NOVA_DB_SECOND_PASSWORD: \${NOVA_DB_SECOND_PASSWORD}
    volumes:
      - db-data:/var/lib/mysql
      - ./docker/mariadb/init:/docker-entrypoint-initdb.d:ro
    healthcheck:
      test: ["CMD", "healthcheck.sh", "--connect", "--innodb_initialized"]
      interval: 5s
      timeout: 5s
      retries: 20
      start_period: 10s
    restart: unless-stopped
    deploy:
      resources:
        limits: {memory: 512m}
volumes:
  nova-state:
  db-data:
YAML
# public_port must match the published HTTPS port (redirects, Alt-Svc).
$R "sed -i 's/^public_port = .*/public_port = $https/' $dir/config/nova.toml"
# Unraid's kernel has no LSM support (no Landlock): run with per-site uids
# and PHP open_basedir instead of refusing to start.
$R "grep -q '^\\[isolation\\]' $dir/config/nova.toml || printf '\\n[isolation]\\nrequire_landlock = false   # Unraid kernel: no Landlock\\n' >> $dir/config/nova.toml"
$R "cd $dir && docker compose up -d --wait 2>&1 | tail -3"
$R "curl -s -o /dev/null -w '$name http %{http_code}\n' http://127.0.0.1:$http/; curl -sk -o /dev/null -w '$name https %{http_code} HTTP/%{http_version}\n' https://127.0.0.1:$https/"
