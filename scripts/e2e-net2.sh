#!/bin/sh
# Networking, part 2 (Linux, root, offline: alpine from the store; python3 and nft on the host are used if present):
# UDP / host-IP / range publishing, bridge+NAT cleanup, IPv6 addresses, network --subnet / connect / inspect.
#   scripts/e2e-net2.sh <docker-bin> <store-dir-with-alpine>
D=${1:?docker binary}; SRC=${2:?store dir}
# run a local copy: executing straight from a Windows-mounted drive is flaky under WSL
cp "$D" /root/dk.$$ && chmod +x /root/dk.$$ && D=/root/dk.$$
export RUST_DOCKER_HOME=$(mktemp -d /root/e2e-net2.XXXXXX)
# always remove what the run started, even when it fails half way (detached supervisors would otherwise live on)
cleanup() {
  for c in $($D ps -a 2>/dev/null | awk 'NR>1{print $1}'); do $D rm -f "$c" >/dev/null 2>&1; done
  for cg in /sys/fs/cgroup/rust-docker-*; do [ -d "$cg" ] && echo 1 > "$cg/cgroup.kill"; done
  rm -f /root/dk.$$; rm -rf "$RUST_DOCKER_HOME"
}
trap cleanup EXIT
for cg in /sys/fs/cgroup/rust-docker-*; do [ -d "$cg" ] && echo 1 > "$cg/cgroup.kill"; done; sleep 1
for cg in /sys/fs/cgroup/rust-docker-*; do [ -d "$cg" ] && rmdir "$cg" 2>/dev/null; done
for l in $(ls /sys/class/net | grep '^rdv'); do ip link del "$l" 2>/dev/null; done
mkdir -p "$RUST_DOCKER_HOME" && cp -r "$SRC" "$RUST_DOCKER_HOME/store"
fail=0
check() { d=$1; shift; if "$@" >/dev/null 2>&1; then echo "PASS $d"; else echo "FAIL $d"; fail=$((fail+1)); fi; }
has() { case "$1" in *"$2"*) return 0 ;; *) return 1 ;; esac; }
ipof() { $D inspect "$1" | sed -n 's/.*"ip": "\([0-9.]*\)".*/\1/p'; }
# udp <host> <port> <payload>: send one datagram, print the reply (empty if none within 4s)
udp() { python3 -c '
import socket, sys
fam = socket.AF_INET6 if ":" in sys.argv[1] else socket.AF_INET
s = socket.socket(fam, socket.SOCK_DGRAM); s.settimeout(4)
s.sendto(sys.argv[3].encode(), (sys.argv[1], int(sys.argv[2])))
try: print(s.recvfrom(200)[0].decode().strip())
except Exception: pass' "$@"; }
# tcp <host> <port>: connect (v4 or v6), print what the server says
tcp() { python3 -c '
import socket, sys
s = socket.create_connection((sys.argv[1], int(sys.argv[2])), timeout=4)
print(s.recv(100).decode().strip())' "$@"; }
tables() { nft list tables 2>/dev/null; }
HAVE_NFT=0; command -v nft >/dev/null 2>&1 && HAVE_NFT=1

echo "== -p udp, tcp, host ip, ranges"
$D run -d --name keep alpine sleep 1000 </dev/null >/dev/null
# the echo server answers every datagram it gets with "pong:<what it got>"
$D run -d --name u -p 15353:53/udp alpine sh -c 'while true; do nc -u -l -p 53 -w 1 >> /tmp/got; done' </dev/null >/dev/null
$D run -d --name both -p 18083:80 -p 18083:80/udp alpine sh -c 'while true; do echo tcp-hello | nc -l -p 80; done' </dev/null >/dev/null
$D run -d --name loc -p 127.0.0.1:18082:80 alpine sh -c 'while true; do echo loc-hello | nc -l -p 80; done' </dev/null >/dev/null
$D run -d --name rng -p 18100-18102:8000-8002 alpine sh -c 'while true; do echo range-hello | nc -l -p 8001; done' </dev/null >/dev/null
$D run -d --name six -p '[::1]:18084:80' alpine sh -c 'while true; do echo six-hello | nc -l -p 80; done' </dev/null >/dev/null
sleep 2
ps=$($D ps)
check "ps shows the udp mapping" has "$ps" "15353->53/udp"
check "ps shows the host-ip mapping" has "$ps" "127.0.0.1:18082->80"
check "ps shows every port of a range" sh -c "echo '$ps' | grep -q '18100->8000' && echo '$ps' | grep -q '18102->8002'"
check "inspect records the udp port" has "$($D inspect u)" "15353->53/udp"
if command -v python3 >/dev/null 2>&1; then
  # udp: the datagram reaches the container, and a reply comes back through the same published port
  udp 127.0.0.1 15353 hello-udp >/dev/null
  sleep 1
  check "udp: the datagram reached the container" has "$($D exec u cat /tmp/got </dev/null)" "hello-udp"
  # reply path: a container whose nc answers the first datagram
  $D run -d --name echo -p 15354:53/udp alpine sh -c 'while true; do (sleep 1; echo pong) | nc -u -l -p 53 > /dev/null; done' </dev/null >/dev/null
  sleep 2
  check "udp: the reply comes back from the published port" has "$(udp 127.0.0.1 15354 ping)" "pong"
  $D rm -f echo >/dev/null 2>&1
  check "tcp and udp can share one port number" has "$(tcp 127.0.0.1 18083)" "tcp-hello"
  check "host ip binding: reachable on 127.0.0.1" has "$(tcp 127.0.0.1 18082)" "loc-hello"
  check "host ip binding: NOT reachable on the bridge address" sh -c "! python3 -c \"import socket; socket.create_connection(('172.30.0.1', 18082), timeout=2)\""
  check "range: a middle port is forwarded" has "$(tcp 127.0.0.1 18101)" "range-hello"
  check "ipv6 listener [::1]:port forwards to the container" has "$(tcp ::1 18084)" "six-hello"
else
  echo "SKIP udp/tcp client checks: no python3"
fi
check "a udp port conflict fails fast" sh -c "! $D run -d -p 15353:53/udp alpine sleep 1 </dev/null"
check "a tcp port conflict still fails fast" sh -c "! $D run -d -p 18083:80 alpine sleep 1 </dev/null"
check "a range conflict fails fast" sh -c "! $D run -d -p 18101-18102:80-81 alpine sleep 1 </dev/null"
check "unequal range lengths are rejected" sh -c "! $D run -d -p 1000-1002:80-81 alpine sleep 1 </dev/null"
for c in u both loc rng six; do $D rm -f $c >/dev/null 2>&1; done

echo "== IPv6 on the bridge"
$D run -d --name v6a alpine sleep 1000 </dev/null >/dev/null
$D run -d --name v6b alpine sleep 1000 </dev/null >/dev/null
sleep 1
a4=$(ipof v6a); b4=$(ipof v6b)
a6=$($D exec v6a ip -6 -o addr show eth0 scope global </dev/null | sed -n 's/.* inet6 \([0-9a-f:]*\)\/.*/\1/p')
b6=$($D exec v6b ip -6 -o addr show eth0 scope global </dev/null | sed -n 's/.* inet6 \([0-9a-f:]*\)\/.*/\1/p')
check "container has a fd00:5a57:: address" has "$a6" "fd00:5a57:"
check "the two containers have different v6 addresses" test -n "$a6" -a "$a6" != "$b6"
check "the v6 address is derived from the v4 one" has "$a6" "$(printf '%x:%x' $(( $(echo $a4 | cut -d. -f1) * 256 + $(echo $a4 | cut -d. -f2) )) $(( $(echo $a4 | cut -d. -f3) * 256 + $(echo $a4 | cut -d. -f4) )))"
check "v6 default route via the bridge" has "$($D exec v6a ip -6 route </dev/null)" "default via fd00:5a57::1"
check "container pings the bridge over v6" $D exec v6a ping -6 -c1 -W3 fd00:5a57::1
check "container pings another container over v6" $D exec v6a ping -6 -c1 -W3 "$b6"
check "host pings a container over v6" ping -6 -c1 -W3 "$a6"
check "ipv4 still works next to it" $D exec v6a ping -c1 -W3 "$b4"
check "loopback ::1 is there" has "$($D exec v6a ip -6 addr show lo </dev/null)" "::1"
hosts=$($D exec v6a cat /etc/hosts </dev/null)
check "/etc/hosts has ::1" has "$hosts" "::1"
check "/etc/hosts has the container's v6 address" has "$hosts" "$a6"
check "the host forwards ipv6" test "$(cat /proc/sys/net/ipv6/conf/all/forwarding)" = 1
if [ "$HAVE_NFT" = 1 ]; then
  check "NAT66 table exists" has "$(nft list table ip6 rust-docker 2>&1)" "masquerade"
fi
if ip -6 route show default | grep -q . && ping -6 -c1 -W3 2606:4700:4700::1111 >/dev/null 2>&1; then
  check "container reaches the IPv6 internet" $D exec v6a ping -6 -c1 -W4 2606:4700:4700::1111
else
  echo "SKIP internet over IPv6: this host has no working IPv6 route (only the bridge / container-to-container paths were tested)"
fi
$D rm -f v6a >/dev/null 2>&1; $D rm -f v6b >/dev/null 2>&1

echo "== user-defined network options"
check "network create --subnet" sh -c "$D network create --subnet 10.77.0.0/24 snet | grep -q snet"
check "network create --driver bridge is accepted" $D network create --driver bridge plain
check "another driver is rejected" sh -c "! $D network create --driver overlay x"
check "an overlapping subnet is rejected" sh -c "! $D network create --subnet 10.77.0.128/25 y"
check "overlap with the default pool is rejected" sh -c "! $D network create --subnet 172.30.5.0/24 y"
check "a subnet with host bits is rejected" sh -c "! $D network create --subnet 10.78.0.5/24 y"
check "garbage is rejected" sh -c "! $D network create --subnet nope y"
$D run -d --name sa --network snet alpine sh -c 'while true; do echo hi-from-sa | nc -l -p 80; done' </dev/null >/dev/null
sleep 1
sa=$(ipof sa)
check "a container on the subnetted network gets an address from it" has "$sa" "10.77.0."
check "its default route is the subnet gateway" has "$($D exec sa ip route </dev/null)" "default via 10.77.0.1"
check "the prefix is the subnet's" has "$($D exec sa ip -4 addr show eth0 </dev/null)" "/24"
check "the gateway answers (host side of the bridge)" $D exec sa ping -c1 -W3 10.77.0.1
check "the host reaches it directly" ping -c1 -W3 "$sa"
check "a second container on the network finds it by name" has "$($D run --network snet alpine nc -w3 sa 80 </dev/null)" "hi-from-sa"
check "default-pool containers are unaffected" has "$($D run alpine ip -4 addr show eth0 </dev/null)" "172.30."
[ "$HAVE_NFT" = 1 ] && check "the subnet has its own masquerade rule" has "$(nft list table ip rust-docker 2>&1)" "10.77.0.0/24"
insp=$($D network inspect snet)
check "inspect shows the subnet" has "$insp" "10.77.0.0/24"
check "inspect shows the gateway" has "$insp" "10.77.0.1"
check "inspect lists the member with both addresses" sh -c "echo '$insp' | grep -q '\"ip\": \"$sa\"' && echo '$insp' | grep -q fd00:5a57"
check "inspect of the default-pool network shows its subnet" has "$($D network inspect plain)" "172.30.0.0/16"

echo "== network connect / disconnect"
$D run -d --name late alpine sh -c 'while true; do echo hi-from-late | nc -l -p 80; done' </dev/null >/dev/null
$D run -d --name peer --network plain alpine sleep 1000 </dev/null >/dev/null
sleep 1
check "before connecting, the peer cannot resolve late" sh -c "! $D exec peer ping -c1 -W2 late"
check "connect" $D network connect plain late
check "connect again is an error" sh -c "! $D network connect plain late"
check "inspect now lists the container" has "$($D network inspect plain)" "late"
check "the peer resolves late by name" $D exec peer ping -c1 -W3 late
check "late sees the peer too" $D exec late ping -c1 -W3 peer
check "a network with its own subnet cannot be joined after the fact" sh -c "! $D network connect snet late"
check "disconnect" $D network disconnect plain late
check "disconnect again is an error" sh -c "! $D network disconnect plain late"
check "late is no longer a member" sh -c "! $D network inspect plain | grep -q late"
check "the peer forgets it" sh -c "! $D exec peer cat /etc/hosts | grep -q late"
check "late forgot the peer" sh -c "! $D exec late cat /etc/hosts | grep -q peer"
check "a stopped/unknown container cannot be connected" sh -c "! $D network connect plain nosuch"
$D rm -f late >/dev/null 2>&1; $D rm -f peer >/dev/null 2>&1; $D rm -f sa >/dev/null 2>&1
check "network rm (with subnet)" sh -c "$D network rm snet && $D network rm plain"
sleep 1

echo "== cleanup: the bridge and NAT go away with the last container"
check "the bridge exists while 'keep' runs" test -d /sys/class/net/rdbr0
$D rm -f keep >/dev/null 2>&1
sleep 2
check "bridge removed after the last container exits" sh -c "! test -d /sys/class/net/rdbr0"
if [ "$HAVE_NFT" = 1 ]; then
  check "nat table (ip) removed" sh -c "! nft list tables | grep -q 'ip rust-docker'"
  check "nat table (ip6) removed" sh -c "! nft list tables | grep -q 'ip6 rust-docker'"
else
  echo "SKIP nft table checks: no nft on this host"
fi
check "no leftover veth ends" sh -c "! ls /sys/class/net | grep -q '^rdv'"
check "a new container recreates the bridge" has "$($D run alpine ip -4 addr show eth0 </dev/null)" "172.30."
check "...and it goes away again" sh -c "sleep 1; ! test -d /sys/class/net/rdbr0"
# a container that starts while another one exits must never lose its network
ok=0
for i in 1 2 3 4 5 6 7 8; do
  $D run -d --name last alpine sleep 1000 </dev/null >/dev/null
  $D rm -f last >/dev/null 2>&1 &
  out=$($D run alpine sh -c 'ping -c1 -W3 172.30.0.1 >/dev/null && echo ok' </dev/null 2>&1)
  wait
  has "$out" "ok" && ok=$((ok+1))
done
check "8 start-while-the-last-one-exits races all had a working network ($ok/8)" test $ok -eq 8
sleep 2
check "...and in the end nothing is left behind" sh -c "! test -d /sys/class/net/rdbr0"
# NAT still works after the table was rebuilt several times
NAME=registry-1.docker.io; URL=http://detectportal.firefox.com/success.txt
if curl -s -m 8 "$URL" | grep -q success; then
  check "outbound NAT works after teardown/recreate" has "$($D run alpine wget -q -T 10 -O - $URL </dev/null)" "success"
else
  echo "SKIP outbound NAT check: this machine cannot fetch $URL itself"
fi

echo "== cleanup"
check "no leftover run dirs" sh -c "! ls /run/rust-docker/* 2>/dev/null | grep -q ."
check "no leftover cgroups" sh -c "! ls /sys/fs/cgroup | grep -q rust-docker"
if [ $fail -eq 0 ]; then echo "ALL PASSED"; else echo "$fail FAILED"; exit 1; fi
