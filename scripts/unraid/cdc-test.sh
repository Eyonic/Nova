#!/bin/bash
cd /mnt/user/appdata/nova-exp; set -a; . ./.env; set +a
sql() { docker exec -i nova-exp-db-1 mariadb -uroot -p"$NOVA_DB_ROOT_PASSWORD" -N -e "$1"; }
ev=/tmp/nova-cdc-events; : > $ev
# SSE subscriber: timestamp every received line.
(curl -sN -H 'Host: wp.nova.test' 'http://127.0.0.1:39080/_nova/live/events?channels=db:wp_posts,db:wp_options' |
  while IFS= read -r line; do echo "$(date +%s.%N) $line"; done >> $ev) &
sub=$!
sleep 1
# 1. latency: commit time (from the DB clock) to SSE arrival
for i in $(seq 10); do
  t=$(sql "UPDATE nova_wp.wp_posts SET post_title=CONCAT('Hello from SQL ', $i) WHERE ID=1; SELECT UNIX_TIMESTAMP(NOW(6));")
  sleep 0.4
  r=$(grep "data: db:wp_posts" $ev | tail -1 | cut -d' ' -f1)
  awk -v t="$t" -v r="$r" 'BEGIN { printf "change %d: %.1f ms\n", '"$i"', (r - t) * 1000 }'
  : > $ev.prev; cp $ev $ev.prev
done | tee /tmp/nova-cdc-lat | tail -3
awk '{s+=$3; if ($3>m) m=$3} END {printf "latency over 10 changes: avg %.1f ms, max %.1f ms\n", s/NR, m}' /tmp/nova-cdc-lat
# 2. rollback publishes nothing
before=$(grep -c "data: db:" $ev)
sql "BEGIN; UPDATE nova_wp.wp_posts SET post_title='rolled back' WHERE ID=1; ROLLBACK;" >/dev/null
sleep 0.5; echo "events after a ROLLBACK: $(( $(grep -c 'data: db:' $ev) - before ))"
# 3. another site's database is not published to WordPress subscribers
before=$(grep -c "data: db:" $ev)
sql "CREATE TABLE IF NOT EXISTS nova_example.visits (id INT AUTO_INCREMENT PRIMARY KEY); INSERT INTO nova_example.visits () VALUES ();" >/dev/null
sleep 0.5; echo "events on the WordPress stream for nova_example: $(( $(grep -c 'data: db:' $ev) - before ))"
# 4. micro-cache purge on data change
H='Host: wp.nova.test'
curl -s -o /dev/null -H "$H" http://127.0.0.1:39080/
c=$(curl -s -D - -o /tmp/p1 -H "$H" http://127.0.0.1:39080/ | tr -d '\r' | grep -i nova-cache)
sql "UPDATE nova_wp.wp_posts SET post_title='Edited directly in MariaDB' WHERE ID=1;" >/dev/null
sleep 0.3
c2=$(curl -s -D - -o /tmp/p2 -H "$H" http://127.0.0.1:39080/ | tr -d '\r' | grep -i nova-cache)
echo "before edit: $c | after edit: $c2 | new title on page: $(grep -c 'Edited directly in MariaDB' /tmp/p2)"
pkill -f 'channels=db:wp_posts,db:wp_options'; kill $sub 2>/dev/null; rm -f $ev $ev.prev /tmp/p1 /tmp/p2 /tmp/nova-cdc-lat
