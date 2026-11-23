#!/bin/sh
# docker compose end-to-end test (Linux, root, offline: alpine from the store):
#   scripts/e2e-compose.sh <docker-bin> <store-dir-with-alpine>
D=${1:?docker binary}; SRC=${2:?store dir}
# run a local copy: executing straight from a Windows-mounted drive is flaky under WSL
cp "$D" /root/dk.$$ && chmod +x /root/dk.$$ && D=/root/dk.$$
export RUST_DOCKER_HOME=$(mktemp -d /root/e2e-cmp.XXXXXX)
proj=$(mktemp -d)/shop
mkdir -p "$proj"
trap 'rm -f /root/dk.$$; rm -rf "$RUST_DOCKER_HOME" "$(dirname "$proj")"' EXIT
for cg in /sys/fs/cgroup/rust-docker-*; do [ -d "$cg" ] && echo 1 > "$cg/cgroup.kill"; done; sleep 1
for cg in /sys/fs/cgroup/rust-docker-*; do [ -d "$cg" ] && rmdir "$cg" 2>/dev/null; done
for l in $(ls /sys/class/net | grep '^rdv'); do ip link del "$l" 2>/dev/null; done
mkdir -p "$RUST_DOCKER_HOME" && cp -r "$SRC" "$RUST_DOCKER_HOME/store"
fail=0
check() { d=$1; shift; if "$@" >/dev/null 2>&1; then echo "PASS $d"; else echo "FAIL $d"; fail=$((fail+1)); fi; }
has() { case "$1" in *"$2"*) return 0 ;; *) return 1 ;; esac; }
eq() { [ "$1" = "$2" ]; }

mkdir -p "$proj/site" "$proj/app"
echo "from-bind-mount" > "$proj/site/index.txt"
cat > "$proj/.env" <<'EOF'
GREETING=hello-from-dotenv
EOF
cat > "$proj/app/Dockerfile" <<'EOF'
FROM alpine
ARG WHO=nobody
RUN echo "built for $WHO" > /who
CMD ["cat", "/who"]
EOF
cat > "$proj/compose.yaml" <<'EOF'
services:
  db:
    image: alpine
    command: sh -c "echo db-ready; while true; do echo hello-from-db | nc -l -p 5432; done"
    healthcheck:
      test: ["CMD-SHELL", "true"]
      interval: 1s
    volumes:
      - dbdata:/var/lib/db
  web:
    image: alpine
    depends_on:
      db:
        condition: service_healthy
    command: sh -c "nc -w3 db 5432; echo web-greets-${GREETING}; cat /site/index.txt; sleep 1000"
    environment:
      MODE: production
      FROM_HOST:
    ports:
      - "18099:80"
    volumes:
      - ./site:/site:ro
    restart: always
  builder:
    build:
      context: ./app
      args:
        WHO: compose
    depends_on: [db]
volumes:
  dbdata: {}
EOF

echo "== config"
cfg=$(cd "$proj" && $D compose config 2>&1)
check "config prints the resolved project" has "$cfg" "name: shop"
check "config interpolates .env" has "$cfg" "web-greets-hello-from-dotenv"
check "config maps volumes with the project prefix" has "$cfg" "dbdata:/var/lib/db"
check "config resolves bind mounts against the project dir" has "$cfg" "/site:/site"

echo "== up"
out=$(cd "$proj" && FROM_HOST=host-value $D compose up -d 2>&1); rc=$?
check "up -d succeeds" test $rc -eq 0
check "it created the project network" has "$out" "shop_default"
check "it waited for the db to be healthy before starting web" sh -c "echo '$out' | grep -n 'Waiting for db' | head -1 | grep -q ."
check "ps lists the containers" sh -c "cd $proj && $D compose ps | grep -q shop-db-1 && $D compose ps | grep -q shop-web-1"
sleep 3
check "db is healthy" sh -c "$D ps | grep shop-db-1 | grep -q healthy"
check "web resolves 'db' by service name (DNS on the project network)" sh -c "cd $proj && $D compose logs web | grep -q hello-from-db"
check ".env values reach the command" sh -c "cd $proj && $D compose logs web | grep -q web-greets-hello-from-dotenv"
check "bind mounts work" sh -c "cd $proj && $D compose logs web | grep -q from-bind-mount"
check "the published port (18099 = 0x46B3) is bound" sh -c "grep -qi ':46B3' /proc/net/tcp"
check "environment: map + passthrough" eq "$($D exec shop-web-1 sh -c 'echo $MODE-$FROM_HOST')" "production-host-value"
check "restart policy is applied" has "$($D inspect -f '{{.HostConfig.RestartPolicy.Name}}' shop-web-1)" "always"
check "the named volume has the project prefix" has "$($D volume ls)" "shop_dbdata"
check "build: services are built and tagged" has "$($D images)" "shop-builder"
check "build args are passed" eq "$($D run --network none shop-builder </dev/null)" "built for compose"
$D container prune >/dev/null   # drop that test run (and the finished builder service) so counts below are about compose only
check "a second up is a no-op for running services" has "$(cd $proj && $D compose up -d 2>&1)" "already running"

echo "== logs"
check "logs are prefixed with the service name" sh -c "cd $proj && $D compose logs | grep -q '^web *| '"
check "logs for one service only" sh -c "cd $proj && ! $D compose logs db | grep -q '^web'"
check "unknown service in logs is an error" sh -c "cd $proj && ! $D compose logs nope >/dev/null 2>&1"

echo "== down"
check "down removes containers and the network" sh -c "cd $proj && $D compose down | grep -q 'Network shop_default removed' && ! $D ps -a | grep -q shop-"
sleep 2
check "nothing is restarted behind down's back (no ghost supervisors)" sh -c "[ \$($D ps -a | wc -l) -eq 1 ]"
check "down keeps volumes by default" has "$($D volume ls)" "shop_dbdata"
(cd "$proj" && $D compose up -d >/dev/null 2>&1); sleep 2
check "down -v removes the volumes too" sh -c "cd $proj && $D compose down -v | grep -q 'Volume shop_dbdata removed' && ! $D volume ls | grep -q shop_dbdata"

echo "== errors"
cat > "$proj/bad.yaml" <<'EOF'
services:
  a: {image: alpine, depends_on: [b]}
  b: {image: alpine, depends_on: [a]}
EOF
check "a dependency cycle is reported" has "$(cd $proj && $D compose -f bad.yaml up -d 2>&1)" "cycle"
check "a missing file is reported" sh -c "cd $proj && ! $D compose -f nope.yaml ps >/dev/null 2>&1"
echo "services: {}" > "$proj/empty.yaml"
check "no services is an error" sh -c "cd $proj && ! $D compose -f empty.yaml up -d >/dev/null 2>&1"
$D rmi shop-builder >/dev/null 2>&1

echo "== cleanup"
check "no leftover run dirs" sh -c "! ls /run/rust-docker/* 2>/dev/null | grep -q ."
check "no leftover cgroups" sh -c "! ls /sys/fs/cgroup | grep -q rust-docker"
if [ $fail -eq 0 ]; then echo "ALL PASSED"; else echo "$fail FAILED"; exit 1; fi
