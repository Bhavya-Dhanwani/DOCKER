#!/bin/sh
# Drive OUR daemon with the REAL Docker CLI (Linux, root, offline):
#   scripts/e2e-realcli.sh <our-docker-bin> <store-dir-with-alpine> <real-docker-cli>
D=${1:?our docker binary}; SRC=${2:?store dir}; REAL=${3:?real docker cli (linux)}
cp "$D" /root/dk.$$ && chmod +x /root/dk.$$ && D=/root/dk.$$
cp "$REAL" /root/realdocker.$$ && chmod +x /root/realdocker.$$ && R=/root/realdocker.$$
export RUST_DOCKER_HOME=$(mktemp -d /root/e2e-real.XXXXXX)
sock=$RUST_DOCKER_HOME/docker.sock
trap 'kill $DPID 2>/dev/null; rm -f /root/dk.$$ /root/realdocker.$$; rm -rf "$RUST_DOCKER_HOME"' EXIT
for cg in /sys/fs/cgroup/rust-docker-*; do [ -d "$cg" ] && echo 1 > "$cg/cgroup.kill"; done; sleep 1
for cg in /sys/fs/cgroup/rust-docker-*; do [ -d "$cg" ] && rmdir "$cg" 2>/dev/null; done
mkdir -p "$RUST_DOCKER_HOME" && cp -r "$SRC" "$RUST_DOCKER_HOME/store"
$D daemon --host "unix://$sock" 2>/dev/null & DPID=$!
for i in 1 2 3 4 5 6 7 8 9 10; do [ -S "$sock" ] && break; sleep 0.3; done
export DOCKER_HOST="unix://$sock"
fails=0
ok() { echo "PASS $1"; }
bad() { echo "FAIL $1"; fails=$((fails + 1)); }
check() { # name, expected-substring, actual
  case "$3" in *"$2"*) ok "$1" ;; *) bad "$1 (wanted '$2', got '$3')" ;; esac
}
t() { timeout 30 "$@" 2>&1; }

check "version talks to the server" "Server:" "$(t $R version)"
check "images lists alpine" "alpine" "$(t $R images)"
check "run --rm prints the output" "hello-real" "$(t $R run --rm alpine echo hello-real)"
t $R run --rm alpine sh -c 'exit 3' >/dev/null; rc=$?
[ "$rc" = 3 ] && ok "run propagates the exit code" || bad "run exit code ($rc)"
id=$(t $R run -d --name web alpine sh -c 'echo booted; sleep 120')
check "run -d prints an id" "$(echo "$id" | cut -c1-6)" "$id"
sleep 1
check "ps shows it running" "web" "$(t $R ps)"
check "logs shows its output" "booted" "$(t $R logs web)"
check "exec prints output" "exec-out" "$(t $R exec web sh -c 'echo exec-out')"
t $R exec web sh -c 'exit 4' >/dev/null; rc=$?
[ "$rc" = 4 ] && ok "exec propagates the exit code" || bad "exec exit code ($rc)"
check "exec -i feeds stdin" "from-stdin" "$(echo from-stdin | t $R exec -i web cat)"
check "inspect shows the name" '"Name": "/web"' "$(t $R inspect web)"
t $R rename web web2 >/dev/null
check "rename takes effect" "web2" "$(t $R ps --format '{{.Names}}')"
check "stats-free top-level info works" "Containers" "$(t $R info)"
t $R stop web2 >/dev/null
check "stop leaves it exited" "Exited" "$(t $R ps -a)"
check "rm removes it" "web2" "$(t $R rm web2)"
mkdir -p /root/ctx.$$ && printf 'FROM alpine\nRUN echo built-ok > /b\nCMD ["cat","/b"]\n' > /root/ctx.$$/Dockerfile
check "build succeeds" "built" "$(t $R build -t realbuilt:1 /root/ctx.$$)"
check "the built image runs" "built-ok" "$(t $R run --rm realbuilt:1)"
rm -rf /root/ctx.$$
timeout 8 $R events --since 0 >/root/ev.$$ 2>&1 &
sleep 1; t $R run --rm alpine true >/dev/null; sleep 3
check "events reports container activity" "container" "$(cat /root/ev.$$)"; rm -f /root/ev.$$

# terminals: the real CLI needs a tty on its own side, which `script` provides
check "run -it gives a shell that runs typed commands" "it-42" "$( (sleep 2; echo 'echo it-$((6*7))'; sleep 1; echo exit) | timeout 40 script -qec "$R run -it --rm alpine sh" /dev/null 2>&1)"
$R run -d --name tt alpine sleep 120 >/dev/null 2>&1; sleep 1
check "exec -it gives a shell that runs typed commands" "ex-42" "$( (sleep 2; echo 'echo ex-$((6*7))'; sleep 1; echo exit) | timeout 40 script -qec "$R exec -it tt sh" /dev/null 2>&1)"
t $R update --memory 32m tt >/dev/null
check "update changes the memory limit" "33554432" "$(cat /sys/fs/cgroup/rust-docker-$($R inspect -f '{{.Id}}' tt)/memory.max 2>&1)"
$R rm -f tt >/dev/null 2>&1

echo "== cleanup"
for c in $($D ps -a | awk 'NR>1{print $1}'); do $D rm -f $c >/dev/null 2>&1; done
[ "$fails" = 0 ] && echo "ALL PASSED" || echo "$fails FAILED"
exit $fails
