#!/bin/sh
# Docker Engine API end-to-end test (Linux, root, offline, needs python3):
#   scripts/e2e-api.sh <docker-bin> <store-dir-with-alpine>
D=${1:?docker binary}; SRC=${2:?store dir}
HERE=$(cd "$(dirname "$0")" && pwd)
# run a local copy: executing straight from a Windows-mounted drive is flaky under WSL
cp "$D" /root/dk.$$ && chmod +x /root/dk.$$ && D=/root/dk.$$
export RUST_DOCKER_HOME=$(mktemp -d /root/e2e-api.XXXXXX)
sock=$RUST_DOCKER_HOME/docker.sock
trap 'kill $DPID 2>/dev/null; rm -f /root/dk.$$; rm -rf "$RUST_DOCKER_HOME"' EXIT
for cg in /sys/fs/cgroup/rust-docker-*; do [ -d "$cg" ] && echo 1 > "$cg/cgroup.kill"; done; sleep 1
for cg in /sys/fs/cgroup/rust-docker-*; do [ -d "$cg" ] && rmdir "$cg" 2>/dev/null; done
mkdir -p "$RUST_DOCKER_HOME" && cp -r "$SRC" "$RUST_DOCKER_HOME/store"
$D daemon --host "unix://$sock" 2>/dev/null & DPID=$!
for i in 1 2 3 4 5 6 7 8 9 10; do [ -S "$sock" ] && break; sleep 0.3; done
python3 "$HERE/api_client.py" "$sock"
rc=$?
echo "== cleanup"
for c in $($D ps -a | awk 'NR>1{print $1}'); do $D rm -f $c >/dev/null 2>&1; done
if ls /run/rust-docker/* 2>/dev/null | grep -q .; then echo "FAIL leftover run dirs"; rc=1; else echo "PASS no leftover run dirs"; fi
exit $rc
