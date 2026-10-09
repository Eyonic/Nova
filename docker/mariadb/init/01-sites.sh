#!/bin/bash
# One database and one user per site; a site's user can reach only its own database.
set -euo pipefail
mariadb --protocol=socket -uroot -p"${MARIADB_ROOT_PASSWORD}" <<SQL
CREATE DATABASE IF NOT EXISTS nova_example;
CREATE USER IF NOT EXISTS 'nova_example'@'%' IDENTIFIED BY '${NOVA_DB_EXAMPLE_PASSWORD}';
GRANT ALL PRIVILEGES ON nova_example.* TO 'nova_example'@'%';
CREATE DATABASE IF NOT EXISTS nova_second;
CREATE USER IF NOT EXISTS 'nova_second'@'%' IDENTIFIED BY '${NOVA_DB_SECOND_PASSWORD}';
GRANT ALL PRIVILEGES ON nova_second.* TO 'nova_second'@'%';
FLUSH PRIVILEGES;
SQL
