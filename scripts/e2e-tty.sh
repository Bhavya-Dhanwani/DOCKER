#!/bin/sh
# Terminals (-it), restart policies and health checks end-to-end test (Linux, root, offline, needs python3):
#   scripts/e2e-tty.sh <docker-bin> <store-dir-with-alpine>
D=${1:?docker binary}; SRC=${2:?store dir}
HERE=$(cd "$(dirname "$0")" && pwd)
# run a local copy: executing straight from a Windows-mounted drive is flaky under WSL
cp "$D" /root/dk.$$ && chmod +x /root/dk.$$ && D=/root/dk.$$
export RUST_DOCKER_HOME=$(mktemp -d /root/e2e-tty.XXXXXX)
work=$(mktemp -d)
trap 'rm -f /root/dk.$$; rm -rf "$work" "$RUST_DOCKER_HOME"' EXIT
for cg in /sys/fs/cgroup/rust-docker-*; do [ -d "$cg" ] && echo 1 > "$cg/cgroup.kill"; done; sleep 1
for cg in /sys/fs/cgroup/rust-docker-*; do [ -d "$cg" ] && rmdir "$cg" 2>/dev/null; done
for l in $(ls /sys/class/net | grep '^rdv'); do ip link del "$l" 2>/dev/null; done
mkdir -p "$RUST_DOCKER_HOME" && cp -r "$SRC" "$RUST_DOCKER_HOME/store"
fail=0
check() { d=$1; shift; if "$@" >/dev/null 2>&1; then echo "PASS $d"; else echo "FAIL $d"; fail=$((fail+1)); fi; }
has() { case "$1" in *"$2"*) return 0 ;; *) return 1 ;; esac; }
eq() { [ "$1" = "$2" ]; }
N="--network none"
drive() { python3 "$HERE/ptydrive.py" "$@"; }

echo "== terminals"
printf 'tty\ntest -t 0 && echo IS-A-TTY\nstty size\necho typed-in-$((6*7))\nexit 3\n' > "$work/keys"
out=$(drive 31 97 "$work/keys" -- $D run -it $N alpine sh)
check "run -it gives the container a terminal (test -t 0)" has "$out" "IS-A-TTY"
check "the terminal size is passed through (31 rows, 97 cols)" has "$out" "31 97"
check "keystrokes reach the container and output comes back" has "$out" "typed-in-42"
check "the container's exit code is the exit code of docker run" has "$out" "[exit 3]"
check "tty names a /dev/pts device" has "$out" "/dev/pts/"
out=$(drive 24 80 - -- $D run -t $N alpine sh -c 'test -t 1 && echo STDOUT-IS-TTY')
check "run -t (no stdin) still gets a terminal" has "$out" "STDOUT-IS-TTY"
out=$(echo piped | $D run $N alpine sh -c 'test -t 0 || echo NOT-A-TTY; cat')
check "without -t, stdin is plain and piped data passes through" sh -c "echo '$out' | grep -q NOT-A-TTY && echo '$out' | grep -q piped"
$D run -d $N --name tt alpine sleep 1000 </dev/null >/dev/null; sleep 1
printf 'echo in-exec-$((5*5))\ntest -t 0 && echo EXEC-TTY\nexit 4\n' > "$work/keys2"
out=$(drive 24 80 "$work/keys2" -- $D exec -it tt sh)
check "exec -it: interactive shell in a running container" sh -c "echo '$out' | grep -q in-exec-25 && echo '$out' | grep -q EXEC-TTY"
check "exec -it returns the shell's exit code" has "$out" "[exit 4]"
$D rm -f tt >/dev/null
out=$(drive 24 80 - -- $D run -dit $N --name bg alpine sh)
check "-dit (detached with a terminal) is accepted" sh -c "$D ps | grep -q bg"
$D rm -f bg >/dev/null

echo "== restart policies"
$D run -d $N --name rs-always --restart always alpine sh -c 'sleep 1; exit 0' </dev/null >/dev/null
sleep 6
r=$($D inspect -f '{{.RestartCount}}' rs-always)
check "--restart always brings an exiting container back (restarts >= 2)" sh -c "[ ${r:-0} -ge 2 ]"
$D ps | grep -q rs-always; check "it shows up in ps while restarting or running" test $? -eq 0
$D stop -t 1 rs-always >/dev/null; sleep 2
check "docker stop ends the restart loop" sh -c "$D ps -a | grep rs-always | grep -q Exited"
n1=$($D inspect -f '{{.RestartCount}}' rs-always); sleep 3
n2=$($D inspect -f '{{.RestartCount}}' rs-always)
check "and it stays stopped (no more restarts)" eq "$n1" "$n2"
$D rm -f rs-always >/dev/null

$D run -d $N --name rs-fail --restart on-failure:2 alpine sh -c 'exit 7' </dev/null >/dev/null
sleep 5
check "on-failure:2 stops after 2 restarts" sh -c "[ \"\$($D inspect -f '{{.RestartCount}}' rs-fail)\" = 2 ] && $D ps -a | grep rs-fail | grep -q 'Exited (7)'"
$D rm -f rs-fail >/dev/null
$D run -d $N --name rs-ok --restart on-failure alpine sh -c 'exit 0' </dev/null >/dev/null; sleep 3
check "on-failure does not restart a clean exit" sh -c "[ \"\$($D inspect -f '{{.RestartCount}}' rs-ok)\" = 0 ] && $D ps -a | grep rs-ok | grep -q 'Exited (0)'"
$D rm -f rs-ok >/dev/null
$D run -d $N --name rs-no alpine sh -c 'exit 1' </dev/null >/dev/null; sleep 2
check "the default (no) never restarts" sh -c "[ \"\$($D inspect -f '{{.RestartCount}}' rs-no)\" = 0 ]"
$D rm -f rs-no >/dev/null
$D run -d $N --name rs-rm --restart always alpine sh -c 'exit 1' </dev/null >/dev/null; sleep 1
check "rm -f works on a container that keeps restarting" sh -c "$D rm -f rs-rm && ! $D ps -a | grep -q rs-rm"
check "a bad policy is rejected" sh -c "! $D run --restart sometimes alpine true </dev/null"

echo "== health checks"
$D run -d $N --name hc --health-cmd 'test -f /tmp/ok' --health-interval 1s --health-retries 2 alpine sleep 1000 </dev/null >/dev/null
sleep 1
check "a new container with a check starts as 'starting'" sh -c "$D ps | grep hc | grep -q 'starting'"
sleep 4
check "a failing check turns it unhealthy (after 2 failures)" sh -c "$D ps | grep hc | grep -q unhealthy"
$D exec hc touch /tmp/ok; sleep 3
check "and it recovers to healthy once the check passes" sh -c "$D ps | grep hc | grep -q '(healthy)'"
check "inspect shows the health status" has "$($D inspect -f '{{.State.Health.Status}}' hc)" healthy
$D rm -f hc >/dev/null
$D run -d $N --name hc2 --health-cmd true --health-interval 1s --health-start-period 30s --health-retries 1 alpine sleep 1000 </dev/null >/dev/null
sleep 3; check "a passing check is healthy" sh -c "$D ps | grep hc2 | grep -q '(healthy)'"
$D rm -f hc2 >/dev/null
$D run -d $N --name hc3 --health-cmd false --health-interval 1s --health-start-period 60s --health-retries 1 alpine sleep 1000 </dev/null >/dev/null
sleep 4; check "failures during the start period do not count" sh -c "! $D ps | grep hc3 | grep -q unhealthy"
$D rm -f hc3 >/dev/null

mkdir -p "$work/img"; cat > "$work/img/Dockerfile" <<'EOF'
FROM alpine
HEALTHCHECK --interval=1s --retries=1 CMD test -f /tmp/ready
CMD ["sleep", "1000"]
EOF
$D build -t hcimg "$work/img" >/dev/null 2>&1; check "HEALTHCHECK in a Dockerfile builds" test $? -eq 0
$D run -d $N --name hc4 hcimg </dev/null >/dev/null; sleep 4
check "the image's HEALTHCHECK is used (unhealthy: /tmp/ready missing)" sh -c "$D ps | grep hc4 | grep -q unhealthy"
$D rm -f hc4 >/dev/null
$D run -d $N --name hc5 --no-healthcheck hcimg </dev/null >/dev/null; sleep 3
check "--no-healthcheck switches it off" sh -c "! $D ps | grep hc5 | grep -qE 'healthy|starting'"
$D rm -f hc5 >/dev/null
$D rmi hcimg >/dev/null 2>&1

echo "== cleanup"
check "no leftover run dirs" sh -c "! ls /run/rust-docker/* 2>/dev/null | grep -q ."
check "no leftover cgroups" sh -c "! ls /sys/fs/cgroup | grep -q rust-docker"
check "no leftover helper processes" sh -c "! ps -eo args | grep -v grep | grep -q 'dk\\.$$ '"
if [ $fail -eq 0 ]; then echo "ALL PASSED"; else echo "$fail FAILED"; exit 1; fi
