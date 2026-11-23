#!/bin/sh
# User-defined networks + name resolution end-to-end test (Linux, root, offline: alpine from the store):
#   scripts/e2e-netname.sh <docker-bin> <store-dir-with-alpine>
D=${1:?docker binary}; SRC=${2:?store dir}
# run a local copy: executing straight from a Windows-mounted drive is flaky under WSL
cp "$D" /root/dk.$$ && chmod +x /root/dk.$$ && D=/root/dk.$$
export RUST_DOCKER_HOME=$(mktemp -d /root/e2e-nn.XXXXXX)
trap 'rm -f /root/dk.$$; rm -rf "$RUST_DOCKER_HOME"' EXIT
for cg in /sys/fs/cgroup/rust-docker-*; do [ -d "$cg" ] && echo 1 > "$cg/cgroup.kill"; done; sleep 1
for cg in /sys/fs/cgroup/rust-docker-*; do [ -d "$cg" ] && rmdir "$cg" 2>/dev/null; done
for l in $(ls /sys/class/net | grep '^rdv'); do ip link del "$l" 2>/dev/null; done
mkdir -p "$RUST_DOCKER_HOME" && cp -r "$SRC" "$RUST_DOCKER_HOME/store"
fail=0
check() { d=$1; shift; if "$@" >/dev/null 2>&1; then echo "PASS $d"; else echo "FAIL $d"; fail=$((fail+1)); fi; }
has() { case "$1" in *"$2"*) return 0 ;; *) return 1 ;; esac; }
eq() { [ "$1" = "$2" ]; }

echo "== network commands"
check "network create" sh -c "$D network create appnet | grep -q appnet"
check "network ls lists it" has "$($D network ls)" "appnet"
check "a duplicate name is rejected" sh -c "! $D network create appnet"
check "built-in names are reserved" sh -c "! $D network create bridge && ! $D network create host"
check "bad names are rejected" sh -c "! $D network create ../x"
check "running on an unknown network fails clearly" has "$($D run --network nosuch alpine true </dev/null 2>&1)" "docker network create"

echo "== names resolve between containers"
$D run -d --name srv --network appnet --network-alias backend alpine sh -c 'while true; do echo hello-from-srv | nc -l -p 80; done' </dev/null >/dev/null
sleep 1
check "a container finds a peer by name" has "$($D run --network appnet alpine nc -w3 srv 80 </dev/null)" "hello-from-srv"
check "...and by --network-alias" $D run --network appnet alpine ping -c1 -W2 backend </dev/null
check "ping by name works" $D run --network appnet alpine ping -c1 -W2 srv </dev/null
hosts=$($D run --network appnet --name me alpine cat /etc/hosts </dev/null)
check "/etc/hosts has the container's own name and its peer" sh -c "echo '$hosts' | grep -q ' me ' && echo '$hosts' | grep -q 'srv'"
check "the peer's alias is listed" has "$hosts" "backend"
check "localhost is there" has "$hosts" "127.0.0.1"

echo "== a peer that starts later becomes visible to existing containers"
$D run -d --name late --network appnet alpine sleep 1000 </dev/null >/dev/null
sleep 2
check "srv (already running) can now resolve 'late'" sh -c "$D exec srv ping -c1 -W3 late"
check "inspect lists both members" sh -c "$D network inspect appnet | grep -q late && $D network inspect appnet | grep -q srv"
check "network ls counts the running containers" sh -c "$D network ls | grep appnet | grep -q ' 2'"
$D rm -f late >/dev/null; sleep 2
check "after it stops, it disappears from its peers' /etc/hosts" sh -c "! $D exec srv cat /etc/hosts | grep -q late"

echo "== the default bridge has no name resolution"
check "a container on the default bridge cannot resolve srv" sh -c "! $D run alpine ping -c1 -W2 srv </dev/null"
ip=$($D inspect -f '{{.NetworkSettings.IPAddress}}' srv)
check "networks do not isolate: the IP is still reachable (documented limitation)" $D run alpine ping -c1 -W2 "$ip" </dev/null
check "a container's own name resolves on the default bridge" has "$($D run --name solo alpine cat /etc/hosts </dev/null)" " solo "

echo "== removal"
check "network rm refuses a network in use" sh -c "! $D network rm appnet"
$D rm -f srv >/dev/null
check "network rm works once nothing uses it" sh -c "$D network rm appnet && ! $D network ls | grep -q appnet"
check "removing an unknown network fails" sh -c "! $D network rm nope"

echo "== cleanup"
check "no leftover run dirs" sh -c "! ls /run/rust-docker/* 2>/dev/null | grep -q ."
check "no leftover cgroups" sh -c "! ls /sys/fs/cgroup | grep -q rust-docker"
if [ $fail -eq 0 ]; then echo "ALL PASSED"; else echo "$fail FAILED"; exit 1; fi
