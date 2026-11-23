#!/bin/sh
# single-node swarm end-to-end test (Linux, root, offline: alpine from the store):
#   scripts/e2e-swarm.sh <docker-bin> <store-dir-with-alpine>
D=${1:?docker binary}; SRC=${2:?store dir}
cp "$D" /root/dk.$$ && chmod +x /root/dk.$$ && D=/root/dk.$$
export RUST_DOCKER_HOME=$(mktemp -d /root/e2e-swm.XXXXXX)
work=$(mktemp -d)
cleanup() { $D swarm leave >/dev/null 2>&1; rm -f /root/dk.$$; rm -rf "$RUST_DOCKER_HOME" "$work"; }
trap cleanup EXIT
for cg in /sys/fs/cgroup/rust-docker-*; do [ -d "$cg" ] && echo 1 > "$cg/cgroup.kill"; done; sleep 1
for cg in /sys/fs/cgroup/rust-docker-*; do [ -d "$cg" ] && rmdir "$cg" 2>/dev/null; done
for l in $(ls /sys/class/net | grep '^rdv'); do ip link del "$l" 2>/dev/null; done
mkdir -p "$RUST_DOCKER_HOME" && cp -r "$SRC" "$RUST_DOCKER_HOME/store"
fail=0
check() { d=$1; shift; if "$@" >/dev/null 2>&1; then echo "PASS $d"; else echo "FAIL $d"; fail=$((fail+1)); fi; }
has() { case "$1" in *"$2"*) return 0 ;; *) return 1 ;; esac; }
# wait up to $1 seconds for the command to succeed
until_ok() { n=$1; shift; i=0; while [ $i -lt $n ]; do "$@" >/dev/null 2>&1 && return 0; sleep 1; i=$((i+1)); done; return 1; }
replicas() { $D service ls --format '{{.Name}} {{.Replicas}}' | awk -v n="$1" '$1==n {print $2}'; }
want() { [ "$(replicas "$1")" = "$2" ]; }
tasks() { $D ps | grep -c "$1\.[0-9]*\.[0-9a-f]\{6\}"; }
env_of_all() { for c in $($D ps --format '{{.ID}} {{.Names}}' | awk -v n="$1" '$2 ~ "^"n {print $1}'); do $D exec "$c" sh -c 'echo $V'; done | sort | uniq -c | tr -s ' ' | tr '\n' ';'; }
get() { timeout 4 bash -c "exec 3<>/dev/tcp/127.0.0.1/$1; cat <&3" 2>/dev/null; }

echo "== swarm"
check "status before init says inactive" has "$($D swarm status)" "inactive"
check "service create without a swarm is an error" sh -c "! $D service create alpine sleep 1"
check "join is clearly unsupported" has "$($D swarm join --token x 1.2.3.4:2377 2>&1)" "single-node only"
check "init" has "$($D swarm init)" "is now a manager"
check "a second init is refused" sh -c "! $D swarm init"
check "status is active with a running reconciler" has "$($D swarm status)" "Reconciler: running"

echo "== service: replicas, healing, scale"
check "create" has "$($D service create --name sl --replicas 3 alpine sleep 1000)" "sl"
check "ls shows 3/3" until_ok 20 want sl 3/3
check "tasks appear in docker ps with service names" sh -c "[ $(tasks sl) -eq 3 ]"
victim=$($D ps --format '{{.ID}} {{.Names}}' | awk '$2 ~ /^sl\.2\./ {print $1}')
$D kill "$victim" >/dev/null 2>&1
check "killing a replica gets it replaced (back to 3/3, new id)" sh -c "sleep 3; $D service ls | grep -q '3/3' && ! $D ps | grep -q $victim"
check "slot 2 exists again" sh -c "$D ps | grep -q 'sl\.2\.'"
$D service scale sl=5 >/dev/null
check "scale up to 5" until_ok 20 want sl 5/5
$D service scale sl=2 >/dev/null
check "scale down to 2" until_ok 20 want sl 2/2
check "ps lists the tasks" sh -c "[ \$($D service ps sl | grep -c 'sl\.') -eq 2 ]"
check "inspect is JSON with the spec" has "$($D service inspect sl)" '"replicas": 2'
check "duplicate name is refused" sh -c "! $D service create --name sl alpine sleep 1"
check "missing image is refused" sh -c "! $D service create --name nope nosuchimage"

echo "== restart conditions"
$D service create --name once --restart-condition none alpine sh -c 'sleep 1' >/dev/null
sleep 6
check "condition none: finished task is not replaced" sh -c "$D service ls | grep '^once' | grep -q '0/1'"
$D service create --name ofail --restart-condition on-failure alpine sh -c 'sleep 1; exit 0' >/dev/null
sleep 6
check "condition on-failure: clean exit is not replaced" sh -c "$D service ls | grep '^ofail' | grep -q '0/1'"
$D service rm once ofail >/dev/null

echo "== rolling update"
$D service rm sl >/dev/null
check "rm removed the tasks" sh -c "[ $(tasks sl) -eq 0 ]"
$D service create --name roll --replicas 3 -e V=one --update-delay 2 --update-parallelism 1 alpine sleep 1000 >/dev/null
until_ok 20 want roll 3/3
check "all replicas start with V=one" has "$(env_of_all roll)" "3 one"
$D service update -e V=two roll >/dev/null
sleep 3
mid=$(env_of_all roll)
check "mid-update the replicas are mixed (one at a time)" sh -c "echo '$mid' | grep -q one && echo '$mid' | grep -q two"
check "update converges: all V=two" until_ok 30 sh -c "[ \"\$(for c in \$($D ps --format '{{.ID}} {{.Names}}' | awk '\$2 ~ /^roll/ {print \$1}'); do $D exec \$c sh -c 'echo \$V'; done | grep -c two)\" = 3 ]"
check "still 3/3 after the update" want roll 3/3
$D service rm roll >/dev/null

echo "== published port is balanced over replicas"
cmd='ID=$(cat /proc/sys/kernel/random/uuid); while true; do echo "$ID" | nc -l -p 8000; done'
$D service create --name lb --replicas 2 -p 18090:8000 alpine sh -c "$cmd" >/dev/null
until_ok 20 want lb 2/2
sleep 2
for i in 1 2 3 4 5 6; do get 18090; done | sort | uniq > "$work/seen"
check "both replicas answered through port 18090" sh -c "[ \$(wc -l < $work/seen) -eq 2 ]"
$D service rm lb >/dev/null
check "the port is released after rm" sh -c "sleep 1; ! grep -qi ' [0-9A-F:]*:46AA [0-9A-F:]* 0A ' /proc/net/tcp"

echo "== service logs"
$D service create --name lg --replicas 2 alpine sh -c 'echo hello-log; sleep 1000' >/dev/null
until_ok 20 want lg 2/2
sleep 1
check "logs are prefixed with the replica name" sh -c "$D service logs lg | grep -c '^lg\.[12]\.[0-9a-f]* | hello-log' | grep -q 2"
$D service rm lg >/dev/null

echo "== stack"
cat > "$work/stack.yml" <<'EOF'
services:
  web:
    image: alpine
    command: sh -c "sleep 1000"
    environment:
      V: s1
    deploy:
      replicas: 2
      restart_policy: {condition: any}
      update_config: {parallelism: 1, delay: 1s}
      resources:
        limits: {memory: 64M}
  db:
    image: alpine
    command: sh -c "while true; do echo db-here | nc -l -p 5432; done"
EOF
out=$($D stack deploy -c "$work/stack.yml" shop 2>&1)
check "deploy creates the services" has "$out" "Creating service shop_web"
check "stack ls" has "$($D stack ls)" "shop"
check "stack services shows replicas" until_ok 20 sh -c "$D stack services shop | grep shop_web | grep -q '2/2'"
check "db is 1/1" until_ok 20 want shop_db 1/1
check "stack ps lists tasks" sh -c "$D stack ps shop | grep -c 'shop_' | grep -qv '^0'"
check "deploy: resources.limits.memory is applied" has "$($D inspect -f '{{.HostConfig.Memory}}' $($D ps --format '{{.ID}} {{.Names}}' | awk '$2 ~ /^shop_web/ {print $1; exit}'))" 67108864
check "service name is a DNS alias on the stack network" sh -c "$D exec \$($D ps --format '{{.ID}} {{.Names}}' | awk '\$2 ~ /^shop_web/ {print \$1; exit}') sh -c 'nc -w3 db 5432' | grep -q db-here"
sed -i 's/V: s1/V: s2/' "$work/stack.yml"
check "redeploy updates" has "$($D stack deploy -c "$work/stack.yml" shop 2>&1)" "Updating service shop_web"
check "redeploy rolls the new env out" until_ok 30 sh -c "[ \"\$($D exec \$($D ps --format '{{.ID}} {{.Names}}' | awk '\$2 ~ /^shop_web/ {print \$1}' | head -1) sh -c 'echo \$V')\" = s2 ] && [ \"\$($D exec \$($D ps --format '{{.ID}} {{.Names}}' | awk '\$2 ~ /^shop_web/ {print \$1}' | tail -1) sh -c 'echo \$V')\" = s2 ]"
check "stack rm removes services, tasks and network" sh -c "$D stack rm shop >/dev/null && [ $(tasks shop_) -eq 0 ] && ! $D network ls | grep -q shop_default && ! $D stack ls | grep -q shop"

echo "== leave"
$D service create --name left --replicas 2 alpine sleep 1000 >/dev/null
until_ok 20 want left 2/2
pid=$(cat "$RUST_DOCKER_HOME/swarm/supervisor.pid")
check "leave succeeds" has "$($D swarm leave)" "left the swarm"
check "containers are gone" sh -c "[ \$($D ps -a | wc -l) -eq 1 ]"
check "the reconciler process exited" sh -c "! kill -0 $pid 2>/dev/null"
check "state dir is gone" sh -c "[ ! -e $RUST_DOCKER_HOME/swarm ]"
check "status says inactive again" has "$($D swarm status)" "inactive"

echo "== cleanup"
check "no leftover run dirs" sh -c "! ls /run/rust-docker/* 2>/dev/null | grep -q ."
check "no leftover cgroups" sh -c "! ls /sys/fs/cgroup | grep -q rust-docker"
if [ $fail -eq 0 ]; then echo "ALL PASSED"; else echo "$fail FAILED"; exit 1; fi
