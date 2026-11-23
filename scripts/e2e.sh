#!/bin/sh
# End-to-end test of the Linux backend. Run as root:
#   scripts/e2e.sh <docker-bin> <store-dir-with-alpine>   (store made by `docker pull alpine`)
D=${1:?docker binary}; SRC=${2:?store dir}
# run a local copy: executing straight from a Windows-mounted drive is flaky under WSL
cp "$D" /root/dk.$$ && chmod +x /root/dk.$$ && D=/root/dk.$$
trap 'rm -f /root/dk.$$' EXIT
# kill leftovers of earlier runs (they could hold the same bridge addresses)
for cg in /sys/fs/cgroup/rust-docker-*; do [ -d "$cg" ] && echo 1 > "$cg/cgroup.kill"; done; sleep 1
for cg in /sys/fs/cgroup/rust-docker-*; do [ -d "$cg" ] && rmdir "$cg" 2>/dev/null; done
for l in $(ls /sys/class/net | grep '^rdv'); do ip link del "$l" 2>/dev/null; done
export RUST_DOCKER_HOME=$(mktemp -d /root/e2e.XXXXXX)
mkdir -p "$RUST_DOCKER_HOME" && cp -r "$SRC" "$RUST_DOCKER_HOME/store"
fail=0
check() { # check "description" <command...>
  d=$1; shift
  if "$@" >/dev/null 2>&1; then echo "PASS $d"; else echo "FAIL $d"; fail=$((fail+1)); fi
}
eq() { [ "$1" = "$2" ]; }
has() { case "$1" in *"$2"*) return 0 ;; *) return 1 ;; esac; }

vol=$(mktemp -d); echo seed > "$vol/a"

out=$($D run -e FOO=bar -e BAZ alpine sh -c 'echo "$FOO"' </dev/null);       check "-e K=V" eq "$out" bar
out=$(BAZ=host $D run -e BAZ alpine sh -c 'echo "$BAZ"' </dev/null);          check "-e K copies host value" eq "$out" host
out=$($D run -v "$vol:/data:ro" alpine cat /data/a </dev/null);               check "-v ro: reads host file" eq "$out" seed
$D run -v "$vol:/data:ro" alpine sh -c 'echo x > /data/n' </dev/null 2>/dev/null; check "-v ro: write is refused" test $? -ne 0
$D run -v "$vol:/data" alpine sh -c 'echo new > /data/b' </dev/null;          check "-v rw: write reaches host" test -f "$vol/b"
$D run -v "$vol/a:/etc/single" alpine cat /etc/single </dev/null >/dev/null;   check "-v single file" test $? -eq 0

id=$($D run -d --name web -e FOO=bar -v "$vol:/data" alpine sh -c 'echo started; sleep 60' </dev/null)
check "-d prints an id" test -n "$id"
sleep 1
check "ps shows it Up" has "$($D ps)" "Up"
check "ps shows the name" has "$($D ps)" "web"
check "logs show output" has "$($D logs web)" "started"
check "name must be unique" sh -c "! $D run -d --name web alpine sleep 1 </dev/null"
check "exec: same hostname/namespaces" eq "$($D exec web hostname)" container
check "exec: sees container processes" has "$($D exec web ps)" "sleep 60"
check "exec: inherits env" eq "$($D exec web sh -c 'echo $FOO')" bar
check "exec: sees the volume" eq "$($D exec web cat /data/a)" seed
check "exec: exit code propagates" sh -c "$D exec web sh -c 'exit 5'; test \$? -eq 5"
check "id prefix works" has "$($D inspect -f '{{.Name}}' "$(echo "$id" | cut -c1-8)")" "/web"
check "rm refuses a running container" sh -c "! $D rm web"
$D stop web >/dev/null
sleep 1
check "stopped: not in ps" sh -c "! $D ps | grep -q web"
check "stopped: in ps -a as Exited" has "$($D ps -a)" "Exited"
check "logs survive exit" has "$($D logs web)" "started"
check "exec on stopped fails" sh -c "! $D exec web true"
check "rm removes it" sh -c "$D rm web && ! $D ps -a | grep -q web"
check "rm deletes the log" sh -c "! ls $RUST_DOCKER_HOME/logs/*.log"

id2=$($D run -d --name tmp alpine sleep 60 </dev/null); sleep 1
check "rm -f kills and removes" sh -c "$D rm -f tmp && ! $D ps -a | grep -q tmp"

id3=$($D run -d --name bad alpine /nonexistent </dev/null); sleep 1
check "failed exec is reported in logs" has "$($D logs bad)" "exec"
check "failed exec records exit code" has "$($D ps -a)" "Exited (127)"
$D rm bad >/dev/null

check "missing image fails fast" sh -c "! $D run -d nosuchimage </dev/null"

check "no leftover run dirs" sh -c "! ls /run/rust-docker/* 2>/dev/null | grep -q ."
check "no leftover cgroups" sh -c "! ls /sys/fs/cgroup | grep -q rust-docker"
check "no leftover mounts" sh -c "! grep -q rust-docker /proc/mounts"

rm -rf "$vol" "$RUST_DOCKER_HOME"
if [ $fail -eq 0 ]; then echo "ALL PASSED"; else echo "$fail FAILED"; exit 1; fi
