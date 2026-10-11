#!/bin/sh
# DNS-01 hook for pebble-challtestsrv: dns-hook.sh set|clear <record name> <value>
case "$1" in
  set)   curl -sf -X POST -d "{\"host\":\"$2.\",\"value\":\"$3\"}" http://challtestsrv:8055/set-txt ;;
  clear) curl -sf -X POST -d "{\"host\":\"$2.\"}" http://challtestsrv:8055/clear-txt ;;
esac
