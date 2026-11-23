#!/bin/sh
# Networking end-to-end test (Linux, root, needs internet for the last checks):
#   scripts/e2e-net.sh <docker-bin> <store-dir-with-alpine>
D=${1:?docker binary}; SRC=${2:?store dir}
# run a local copy: executing straight from a Windows-mounted drive is flaky under WSL
cp "$D" /root/dk.$$ && chmod +x /root/dk.$$ && D=/root/dk.$$
trap 'rm -f /root/dk.$$' EXIT
# kill leftovers of earlier runs (they could hold the same bridge addresses)
for cg in /sys/fs/cgroup/rust-docker-*; do [ -d "$cg" ] && echo 1 > "$cg/cgroup.kill"; done; sleep 1
for cg in /sys/fs/cgroup/rust-docker-*; do [ -d "$cg" ] && rmdir "$cg" 2>/dev/null; done
for l in $(ls /sys/class/net | grep '^rdv'); do ip link del "$l" 2>/dev/null; done
export RUST_DOCKER_HOME=$(mktemp -d /root/e2e-net.XXXXXX)
mkdir -p "$RUST_DOCKER_HOME" && cp -r "$SRC" "$RUST_DOCKER_HOME/store"
fail=0
check() { d=$1; shift; if "$@" >/dev/null 2>&1; then echo "PASS $d"; else echo "FAIL $d"; fail=$((fail+1)); fi; }
has() { case "$1" in *"$2"*) return 0 ;; *) return 1 ;; esac; }
ipof() { $D inspect -f '{{.NetworkSettings.IPAddress}}' "$1"; }

echo "== bridge (default)"
# the bridge now disappears with the last container, so keep one around for the host-side checks below
$D run -d --name keep alpine sleep 1000 </dev/null >/dev/null
out=$($D run alpine ip -4 addr show eth0 </dev/null)
check "eth0 has a 172.30.x.x/16 address" has "$out" "172.30."
check "default route via the gateway" has "$($D run alpine ip route </dev/null)" "default via 172.30.0.1"
check "loopback is up" has "$($D run alpine ip addr show lo </dev/null)" "LOOPBACK,UP"
check "resolv.conf is generated" has "$($D run alpine cat /etc/resolv.conf </dev/null)" "nameserver"
check "bridge exists on host" test -d /sys/class/net/rdbr0
check "host can ping the gateway" ping -c1 -W2 172.30.0.1

$D run -d --name srv alpine sh -c 'while true; do echo hello-from-srv | nc -l -p 80; done' </dev/null >/dev/null
$D run -d --name pub -p 18080:80 alpine sh -c 'while true; do echo hello-from-pub | nc -l -p 80; done' </dev/null >/dev/null
sleep 1
srv=$(ipof srv); pub=$(ipof pub)
check "containers get different addresses" test "$srv" != "$pub" -a -n "$srv"
check "ps shows IP and PORTS" has "$($D ps)" "18080->80"
check "host -> container by IP" has "$(nc -w3 "$srv" 80 </dev/null)" "hello-from-srv"
check "host -> -p published port" has "$(nc -w3 127.0.0.1 18080 </dev/null)" "hello-from-pub"
check "container -> container" has "$($D run alpine nc -w3 "$srv" 80 </dev/null)" "hello-from-srv"
check "-p port conflict fails fast" sh -c "! $D run -d -p 18080:80 alpine sleep 1 </dev/null"
$D rm -f srv >/dev/null 2>&1; $D rm -f pub >/dev/null 2>&1
sleep 1
n=$(ls /sys/class/net | grep -c '^rdv')
check "veth ends are cleaned up (only keep's is left)" test "$n" -le 1

echo "== outbound through NAT"
check "container reaches an outside IP (ICMP via masquerade)" $D run alpine ping -c1 -W3 1.1.1.1 </dev/null
# Use hosts that answer plain DNS/HTTP everywhere. (example.com is blocked by some networks, which once
# made this test skip for the wrong reason.)
NAME=registry-1.docker.io; URL=http://detectportal.firefox.com/success.txt
if curl -s -m 8 "$URL" | grep -q success; then
  check "container resolves a DNS name" $D run alpine nslookup $NAME </dev/null
  check "container fetches a web page" has "$($D run alpine wget -q -T 10 -O - $URL </dev/null)" "success"
else
  echo "SKIP DNS/HTTP checks: this machine cannot fetch $URL itself"
fi

echo "== other modes"
out=$($D run --network none alpine ip -o link </dev/null)
check "none: only loopback" sh -c "! echo '$out' | grep -q eth0"
check "none: loopback up" has "$($D run --network none alpine ip addr show lo </dev/null)" "LOOPBACK,UP"
check "none: -p is rejected" sh -c "! $D run --network none -p 1:1 alpine true </dev/null"
check "host: shares the host interfaces" has "$($D run --network host alpine ip -o link </dev/null)" "eth0"
check "host: container sees the bridge too" has "$($D run --network host alpine ip -o link </dev/null)" "rdbr0"

$D rm -f keep >/dev/null 2>&1
echo "== cleanup"
check "no leftover run dirs" sh -c "! ls /run/rust-docker/* 2>/dev/null | grep -q ."
check "no leftover cgroups" sh -c "! ls /sys/fs/cgroup | grep -q rust-docker"
rm -rf "$RUST_DOCKER_HOME"
if [ $fail -eq 0 ]; then echo "ALL PASSED"; else echo "$fail FAILED"; exit 1; fi
