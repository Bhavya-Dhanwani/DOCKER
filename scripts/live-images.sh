#!/bin/sh
# Live check of popular Docker Hub images on this engine (needs internet and a LOT of disk: run it in a
# VM whose disk has room, e.g. a WSL distro on a big drive). Pulls each image, starts or runs it, checks
# that it really works, then removes the container and the image again.
#   scripts/live-images.sh [docker-binary] [only-this-image-name ...]
D=${1:-docker}; shift
ONLY="$*"
RES=${RESULTS:-/tmp/live-images.results}
TMPB=${TMPBASE:-/tmp/live.$$}   # per-worker scratch files, so several workers can run at once
: > "$RES"
ok()  { echo "PASS $1 | $2" | tee -a "$RES"; }
bad() { echo "FAIL $1 | $2" | tee -a "$RES"; }
want() { [ -z "$ONLY" ] && return 0; for n in $ONLY; do [ "$n" = "$1" ] && return 0; done; return 1; }
pull() { timeout ${PULL_TIMEOUT:-2400} $D pull "$1" >$TMPB.pull 2>&1 || { bad "$2" "pull failed: $(tail -n 2 $TMPB.pull | tr '\n' ' ' | cut -c1-160)"; return 1; }; }
clean() { $D rm -f "live-$1" >/dev/null 2>&1; $D rmi "$2" >/dev/null 2>&1; }
wait_http() { i=0; while [ $i -lt "$4" ]; do r=$(curl -sL -m 3 "http://127.0.0.1:$1$2" 2>/dev/null | head -c 400); case "$r" in *"$3"*) return 0;; esac; sleep 2; i=$((i + 2)); done; return 1; }
wait_log() { i=0; while [ $i -lt "$3" ]; do $D logs "live-$1" 2>&1 | grep -q "$2" && return 0; sleep 2; i=$((i + 2)); done; return 1; }

# once NAME IMAGE EXPECT cmd...   -> run to completion and look for EXPECT in the output
once() {
  n=$1; img=$2; exp=$3; shift 3
  want "$n" || return 0
  pull "$img" "$n" || return 0
  out=$(timeout 180 $D run --rm "$img" "$@" 2>&1 | head -c 800)
  case "$out" in *"$exp"*) ok "$n" "$img ran: found '$exp'";; *) bad "$n" "$img output: $(echo "$out" | tr '\n' ' ' | cut -c1-170)";; esac
  $D rmi "$img" >/dev/null 2>&1
}
# http NAME IMAGE PORT PATH EXPECT SECS [run flags...]  -> start it with -p PORT, wait for the page
http() {
  n=$1; img=$2; port=$3; path=$4; exp=$5; secs=$6; shift 6
  want "$n" || return 0
  pull "$img" "$n" || return 0
  $D run -d --name "live-$n" -p "$port:${PORT_IN:-$port}" "$@" "$img" $CMD >/dev/null 2>$TMPB.err || { bad "$n" "start failed: $(cut -c1-160 $TMPB.err)"; clean "$n" "$img"; return 0; }
  if wait_http "$port" "$path" "$exp" "$secs"; then ok "$n" "$img served '$exp' on :$port"; else bad "$n" "$img no '$exp' after ${secs}s; log: $($D logs "live-$n" 2>&1 | tail -n 2 | tr '\n' ' ' | cut -c1-170)"; fi
  clean "$n" "$img"
}
# logged NAME IMAGE EXPECT SECS [run flags...] -> start it and wait for EXPECT in its log (CMD = optional command)
logged() {
  n=$1; img=$2; exp=$3; secs=$4; shift 4
  want "$n" || return 0
  pull "$img" "$n" || return 0
  $D run -d --name "live-$n" "$@" "$img" $CMD >/dev/null 2>$TMPB.err || { bad "$n" "start failed: $(cut -c1-160 $TMPB.err)"; clean "$n" "$img"; return 0; }
  if [ -z "$exp" ]; then
    # no log line to wait for: it counts as up when it is still running a few seconds later
    sleep 3
    if $D exec "live-$n" true >/dev/null 2>&1; then ok "$n" "$img is running"; LASTOK=1; else bad "$n" "$img exited at once: $($D logs "live-$n" 2>&1 | tail -n 2 | tr '\n' ' ' | cut -c1-170)"; LASTOK=0; fi
  elif wait_log "$n" "$exp" "$secs"; then ok "$n" "$img log shows '$exp'"; LASTOK=1; else bad "$n" "$img no '$exp' after ${secs}s; log: $($D logs "live-$n" 2>&1 | tail -n 2 | tr '\n' ' ' | cut -c1-170)"; LASTOK=0; fi
  [ -z "$KEEP" ] && clean "$n" "$img"
}

echo "== base systems"
once ubuntu ubuntu:latest "Ubuntu" cat /etc/os-release
once debian debian:stable-slim "Debian" cat /etc/os-release
once busybox busybox:latest "busy-ok" echo busy-ok
once fedora fedora:latest "Fedora" cat /etc/os-release
once kali kalilinux/kali-rolling:latest "Kali" cat /etc/os-release
once archlinux archlinux:latest "Arch Linux" cat /etc/os-release   # Intel-only image: runs through x86-64 emulation
echo "== languages"
once golang golang:alpine "go version" go version
once rust rust:alpine "rustc" rustc --version
once php php:cli-alpine "PHP" php -v
once ruby ruby:alpine "ruby" ruby -v
once eclipse-temurin eclipse-temurin:21-jre-alpine "openjdk" java -version
once dotnet mcr.microsoft.com/dotnet/aspnet:8.0-alpine "Microsoft.AspNetCore.App" dotnet --list-runtimes
echo "== web servers and proxies"
PORT_IN=80 http httpd httpd:alpine 18101 / "<html" 40
PORT_IN=80 http caddy caddy:alpine 18102 / "Caddy" 40
once traefik traefik:latest "Version" traefik version
once haproxy haproxy:alpine "HAProxy" haproxy -v
echo "== databases"
KEEP=1 logged mysql mysql:8 "ready for connections" 240 -e MYSQL_ALLOW_EMPTY_PASSWORD=yes
if [ "$LASTOK" = 1 ] && want mysql; then sleep 8; r=$($D exec live-mysql mysql -uroot -e 'select 6*7 as answer' 2>&1); case "$r" in *42*) ok mysql-query "select 6*7 = 42";; *) bad mysql-query "$(echo "$r" | tr '\n' ' ' | cut -c1-150)";; esac; fi
clean mysql mysql:8
KEEP=1 logged mongo mongo:latest "Waiting for connections" 180
if [ "$LASTOK" = 1 ] && want mongo; then r=$($D exec live-mongo mongosh --quiet --eval 'db.runCommand({ping:1}).ok' 2>&1); case "$r" in *1*) ok mongo-ping "ping ok";; *) bad mongo-ping "$(echo "$r" | tr '\n' ' ' | cut -c1-150)";; esac; fi
clean mongo mongo:latest
KEEP=1 logged memcached memcached:alpine "" 1
if [ "$LASTOK" = 1 ] && want memcached; then sleep 2; r=$($D exec live-memcached sh -c 'echo stats | nc -w1 127.0.0.1 11211' 2>&1); case "$r" in *"STAT pid"*) ok memcached-stats "stats answered";; *) bad memcached-stats "$(echo "$r" | tr '\n' ' ' | cut -c1-150)";; esac; fi
clean memcached memcached:alpine
CMD="" logged cassandra cassandra:latest "Starting listening for CQL clients" 360 -e MAX_HEAP_SIZE=512M -e HEAP_NEWSIZE=128M
PORT_IN=5984 http couchdb couchdb:latest 18110 / "couchdb" 120 -e COUCHDB_USER=admin -e COUCHDB_PASSWORD=secret
PORT_IN=8086 http influxdb influxdb:2 18111 /health "pass" 120
echo "== messaging"
logged rabbitmq rabbitmq:alpine "Server startup complete" 180
logged nats nats:alpine "Server is ready" 40
once redpanda redpandadata/redpanda:latest "v" rpk version
logged eclipse-mosquitto eclipse-mosquitto:latest "mosquitto version" 40
echo "== apps and tools"
PORT_IN=80 http wordpress wordpress:apache 18112 / "WordPress" 90
PORT_IN=2368 http ghost ghost:alpine 18113 / "Ghost" 240 -e url=http://127.0.0.1:18113 -e database__client=sqlite3 -e database__connection__filename=/var/lib/ghost/content/data/ghost.db   # Ghost needs MySQL unless told to use SQLite
PORT_IN=3000 http grafana grafana/grafana:latest 18114 /api/health "ok" 90
PORT_IN=9090 http prometheus prom/prometheus:latest 18115 /-/ready "Ready" 60
once vault hashicorp/vault:latest "Vault v" vault version
PORT_IN=5000 http registry registry:2 18116 /v2/ "{}" 40
CMD="server /data" logged minio quay.io/minio/minio:latest "API:" 60   # minio/minio left Docker Hub: it lives on quay.io now
once mailhog mailhog/mailhog:latest "Binding to address" MailHog   # Intel-only image: runs under x86-64 emulation (it is a server: it prints its listeners and we stop after 180s)
PORT_IN=8080 http adminer adminer:latest 18118 / "Adminer" 60
echo "== CI and dev"
PORT_IN=8080 http jenkins jenkins/jenkins:lts-jdk17 18119 /login "Jenkins" 420
PORT_IN=3000 http gitea gitea/gitea:latest 18120 / "Gitea" 120
echo
echo "== summary: $(grep -c '^PASS' "$RES") passed, $(grep -c '^FAIL' "$RES") failed (details in $RES)"
