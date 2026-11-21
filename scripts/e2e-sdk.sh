#!/bin/sh
# The Docker SDK for Python against our daemon (Linux, root, offline, needs python3 and the unpacked SDK):
#   scripts/e2e-sdk.sh <docker-bin> <store-dir-with-alpine> <dir-with-unpacked-docker-sdk-and-requests>
D=${1:?docker binary}; SRC=${2:?store dir}; LIB=${3:?SDK dir}
HERE=$(cd "$(dirname "$0")" && pwd)
cp "$D" /root/dk.$$ && chmod +x /root/dk.$$ && D=/root/dk.$$
export RUST_DOCKER_HOME=$(mktemp -d /root/e2e-sdk.XXXXXX)
sock=$RUST_DOCKER_HOME/docker.sock
trap 'kill $DPID 2>/dev/null; rm -f /root/dk.$$; rm -rf "$RUST_DOCKER_HOME"' EXIT
mkdir -p "$RUST_DOCKER_HOME" && cp -r "$SRC" "$RUST_DOCKER_HOME/store"
$D daemon --host "unix://$sock" 2>/dev/null & DPID=$!
for i in 1 2 3 4 5 6 7 8 9 10; do [ -S "$sock" ] && break; sleep 0.3; done
python3 "$HERE/sdk_test.py" "$sock" "$LIB"
rc=$?
for c in $($D ps -a | awk 'NR>1{print $1}'); do $D rm -f $c >/dev/null 2>&1; done
exit $rc
