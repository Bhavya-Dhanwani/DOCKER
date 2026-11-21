#!/bin/sh
# Lifecycle / cp / top / stats / volumes end-to-end test (Linux, root, offline: alpine from the store):
#   scripts/e2e-ops.sh <docker-bin> <store-dir-with-alpine>
D=${1:?docker binary}; SRC=${2:?store dir}
# run a local copy: executing straight from a Windows-mounted drive is flaky under WSL
cp "$D" /root/dk.$$ && chmod +x /root/dk.$$ && D=/root/dk.$$
export RUST_DOCKER_HOME=$(mktemp -d /root/e2e-ops.XXXXXX)
work=$(mktemp -d)
trap 'rm -f /root/dk.$$; rm -rf "$work" "$RUST_DOCKER_HOME" /root/hostsecret.$$' EXIT
for cg in /sys/fs/cgroup/rust-docker-*; do [ -d "$cg" ] && echo 1 > "$cg/cgroup.kill"; done; sleep 1
for cg in /sys/fs/cgroup/rust-docker-*; do [ -d "$cg" ] && rmdir "$cg" 2>/dev/null; done
for l in $(ls /sys/class/net | grep '^rdv'); do ip link del "$l" 2>/dev/null; done
mkdir -p "$RUST_DOCKER_HOME" && cp -r "$SRC" "$RUST_DOCKER_HOME/store"
fail=0
check() { d=$1; shift; if "$@" >/dev/null 2>&1; then echo "PASS $d"; else echo "FAIL $d"; fail=$((fail+1)); fi; }
has() { case "$1" in *"$2"*) return 0 ;; *) return 1 ;; esac; }
eq() { [ "$1" = "$2" ]; }
idof() { $D ps -a | awk -v n="$1" 'NR>1 && $2==n {print $1}'; }
N="--network none"

echo "== stop / kill"
$D run -d $N --name term alpine sh -c 'trap "echo handled-term; exit 3" TERM; sleep 1000 & wait' </dev/null >/dev/null; sleep 1
t0=$(date +%s); $D stop -t 10 term >/dev/null; t1=$(date +%s)
check "stop delivers SIGTERM and honours the handler (fast, exit 3)" sh -c "[ $((t1-t0)) -le 3 ] && $D ps -a | grep term | grep -q 'Exited (3)'"
check "the handler's output is in the log" has "$($D logs term)" "handled-term"
$D run -d $N --name stubborn alpine sleep 1000 </dev/null >/dev/null; sleep 1
t0=$(date +%s); $D stop -t 1 stubborn >/dev/null; t1=$(date +%s)
check "stop falls back to SIGKILL after the timeout (137)" sh -c "[ $((t1-t0)) -le 4 ] && $D ps -a | grep stubborn | grep -q 'Exited (137)'"
$D run -d $N --name k alpine sleep 1000 </dev/null >/dev/null; sleep 1
$D kill k >/dev/null; sleep 1
check "kill is immediate" sh -c "$D ps -a | grep ' k ' | grep -q Exited"
check "stop on a stopped container is an error" sh -c "! $D stop k"

echo "== start / restart / pause / wait / rename"
$D run -d $N --name cyc alpine sh -c 'echo run-$$; sleep 1000' </dev/null >/dev/null; sleep 1
check "restart gives the container a fresh run" sh -c "$D restart -t 1 cyc >/dev/null && sleep 1 && $D ps | grep -q 'cyc.*Up'"
$D kill cyc >/dev/null; sleep 1
check "start re-runs an exited container under the same name" sh -c "$D start cyc >/dev/null && sleep 1 && $D ps | grep -q 'cyc.*Up'"
check "start refuses a running container" sh -c "! $D start cyc"
id=$(idof cyc)
$D pause cyc >/dev/null; sleep 1
check "pause freezes the cgroup (kernel says frozen 1)" sh -c "grep -q 'frozen 1' /sys/fs/cgroup/rust-docker-$id/cgroup.events"
$D unpause cyc >/dev/null; sleep 1
check "unpause thaws it (frozen 0)" sh -c "grep -q 'frozen 0' /sys/fs/cgroup/rust-docker-$id/cgroup.events"
$D run -d $N --name w alpine sh -c 'sleep 2; exit 5' </dev/null >/dev/null
check "wait returns the exit code" eq "$($D wait w)" 5
check "rename works" sh -c "$D rename w renamed && $D ps -a | grep -q ' renamed '"
check "rename onto an existing name fails" sh -c "! $D rename renamed cyc"
check "top lists the container's processes" has "$($D top cyc)" "sleep 1000"
check "stats shows usage for a running container" has "$($D stats --no-stream cyc)" "$id"
check "stats MEM column has a limit" has "$($D stats --no-stream cyc)" " / "

echo "== cp"
echo from-host > "$work/in.txt"; mkdir -p "$work/dir/sub"; echo deep > "$work/dir/sub/f"
$D cp "$work/in.txt" cyc:/tmp/in.txt
check "cp host -> container" eq "$($D exec cyc cat /tmp/in.txt)" from-host
$D cp "$work/dir" cyc:/tmp/dir
check "cp a directory host -> container" eq "$($D exec cyc cat /tmp/dir/sub/f)" deep
$D exec cyc sh -c 'echo made-inside > /tmp/out.txt; mkdir -p /tmp/od; echo x > /tmp/od/y'
mkdir -p "$work/back"; $D cp cyc:/tmp/out.txt "$work/back/"
check "cp container -> host (into a directory)" eq "$(cat $work/back/out.txt)" made-inside
$D cp cyc:/tmp/od "$work/back/od"
check "cp a container directory -> host" eq "$(cat $work/back/od/y)" x
echo "TOP SECRET" > /root/hostsecret.$$
$D exec cyc ln -s /root/hostsecret.$$ /tmp/evil
check "a container symlink to a host path cannot be read via cp" sh -c "! $D cp cyc:/tmp/evil $work/leak 2>/dev/null; [ ! -e $work/leak ]"
$D exec cyc ln -s ../../../../root /tmp/escape
check "a relative symlink cannot climb out either" sh -c "! $D cp cyc:/tmp/escape/hostsecret.$$ $work/leak2 2>/dev/null; [ ! -e $work/leak2 ]"
check "cp needs exactly one container side" sh -c "! $D cp a b"
$D rm -f cyc >/dev/null 2>&1

echo "== named volumes"
check "volume create" sh -c "$D volume create data | grep -q data"
$D run $N -v data:/data alpine sh -c 'echo persisted > /data/f' </dev/null
check "data written to a named volume survives the container" eq "$($D run $N -v data:/data alpine cat /data/f </dev/null)" persisted
check "a volume is created on first use" sh -c "$D run $N -v auto:/x alpine true </dev/null && $D volume ls | grep -q auto"
check "volume ls shows both" sh -c "$D volume ls | grep -q data && $D volume ls | grep -q auto"
$D run -d $N --name vuser -v data:/data alpine sleep 1000 </dev/null >/dev/null; sleep 1
check "volume inspect lists who uses it" has "$($D volume inspect data)" "vuser"
check "volume rm refuses a volume in use" sh -c "! $D volume rm data"
$D rm -f vuser >/dev/null
check "volume rm works once nothing uses it" sh -c "$D volume rm data && ! $D volume ls | grep -q data"
check "volume prune removes the rest" sh -c "$D volume prune | grep -q auto && ! $D volume ls | grep -q auto"
check "bad volume names are rejected" sh -c "! $D volume create ../x"

echo "== housekeeping"
check "system df reports images/containers/volumes" sh -c "$D system df | grep -q Images && $D system df | grep -q Containers && $D system df | grep -q Volumes"
check "container ls is an alias of ps" has "$($D container ls -a)" "CONTAINER ID"
check "image ls is an alias of images" has "$($D image ls)" "alpine"
check "container prune removes exited containers" sh -c "$D container prune | grep -q 'Deleted containers' && [ \$($D ps -a | wc -l) -eq 1 ]"

echo "== cleanup"
check "no leftover run dirs" sh -c "! ls /run/rust-docker/* 2>/dev/null | grep -q ."
check "no leftover cgroups" sh -c "! ls /sys/fs/cgroup | grep -q rust-docker"
if [ $fail -eq 0 ]; then echo "ALL PASSED"; else echo "$fail FAILED"; exit 1; fi
