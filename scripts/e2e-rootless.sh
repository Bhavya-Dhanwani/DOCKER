#!/bin/sh
# End-to-end test of rootless mode. Run AS ROOT; every docker command runs as the unprivileged user "rl".
#   scripts/e2e-rootless.sh <docker-bin> <store-dir-with-alpine>
# Needs (installed on demand if apt works): uidmap, slirp4netns, passt (for pasta), curl.
D=${1:?docker binary}; SRC=${2:?store dir}
U=rl
id $U >/dev/null 2>&1 || useradd -m $U
UID_=$(id -u $U)
command -v newuidmap >/dev/null && command -v slirp4netns >/dev/null && command -v pasta >/dev/null || apt-get install -y uidmap slirp4netns passt curl >/dev/null 2>&1
grep -q "^$U:" /etc/subuid || echo "$U:165536:65536" >> /etc/subuid
grep -q "^$U:" /etc/subgid || echo "$U:165536:65536" >> /etc/subgid

HOMEDIR=/home/$U
BIN=$HOMEDIR/docker
RH=$HOMEDIR/rd.$$
cp "$D" "$BIN" && chmod 755 "$BIN" && chown $U "$BIN"
rm -rf "$HOMEDIR"/rd.*            # leftovers
mkdir -p "$RH/store" && cp -r "$SRC/blobs" "$SRC/images" "$RH/store/" && chown -R $U:$U "$RH"
trap 'for p in $(pgrep -u $U); do kill -9 $p 2>/dev/null; done; rmdir /sys/fs/cgroup/rl-deleg/rust-docker-* /sys/fs/cgroup/rl-deleg/sup /sys/fs/cgroup/rl-deleg 2>/dev/null' EXIT
for p in $(pgrep -u $U); do kill -9 $p 2>/dev/null; done

rl() { setpriv --reuid=$U --regid=$U --init-groups env HOME=$HOMEDIR RUST_DOCKER_HOME=$RH "$@"; }
dk() { rl "$BIN" "$@" </dev/null; }
fail=0
check() { d=$1; shift; if "$@" >/dev/null 2>&1; then echo "PASS $d"; else echo "FAIL $d"; fail=$((fail+1)); fi; }
eq() { [ "$1" = "$2" ]; }
has() { case "$1" in *"$2"*) return 0 ;; *) return 1 ;; esac; }
hasnt() { case "$1" in *"$2"*) return 1 ;; *) return 0 ;; esac; }

echo "== environment: kernel $(uname -r), uid of test user $UID_"
check "test user is not root" test "$UID_" -ne 0
check "images: alpine visible in the user's own store" has "$(dk images)" alpine

echo "== basics (full uid mapping if uidmap+subuid are present)"
out=$(dk run alpine echo hello-rootless 2>/tmp/rl.err); check "run echo" eq "$out" hello-rootless
check "no warning about mapping in full mode" hasnt "$(cat /tmp/rl.err)" "single-uid"
check "layers are in the rootless flavour" test -d "$RH/store/layers-rootless"
check "root-engine layer dir was not touched" test ! -d "$RH/store/layers"
out=$(dk run alpine sh -c 'echo data > /tmp/f; cat /tmp/f'); check "write a file in the container" eq "$out" data
out=$(dk run alpine sh -c 'echo data > /etc/changed; cat /etc/changed'); check "write on the overlay root" eq "$out" data
check "container root is the unprivileged user on the host" has "$(dk run -d alpine sleep 30 >/tmp/rl.id; ps -o user= -p $(rl "$BIN" inspect $(cat /tmp/rl.id) 2>/dev/null | sed -n 's/.*"pid": *\([0-9]*\).*/\1/p' | head -1))" $U
dk stop $(cat /tmp/rl.id) >/dev/null 2>&1; dk rm $(cat /tmp/rl.id) >/dev/null 2>&1
check "uid inside is 0" eq "$(dk run alpine id -u)" 0
check "full map: two lines in uid_map" eq "$(dk run alpine sh -c 'wc -l < /proc/self/uid_map')" 2
check "full map: chown to another uid works" eq "$(dk run alpine sh -c 'touch /tmp/x; chown 1234:1234 /tmp/x; stat -c %u /tmp/x')" 1234
check "full map: -u 1000 switches user" eq "$(dk run -u 1000 alpine id -u)" 1000
check "full map: layer file keeps its gid (shadow = 42)" eq "$(dk run alpine stat -c %g /etc/shadow)" 42
check "/dev/null works" eq "$(dk run alpine sh -c 'echo x > /dev/null && echo ok')" ok
check "/dev/zero + /dev/urandom readable" eq "$(dk run alpine sh -c 'head -c 4 /dev/zero | wc -c; head -c 4 /dev/urandom | wc -c' | tr -d '\n ')" 44
check "/dev/null is a char device" has "$(dk run alpine ls -l /dev/null)" "crw"
check "exit code is passed through" eq "$(dk run alpine sh -c 'exit 7' >/dev/null 2>&1; echo $?)" 7

echo "== lifecycle: ps / exec / logs / stop / rm"
id=$(dk run -d --name rlweb -e FOO=bar alpine sh -c 'echo started; sleep 120')
check "-d prints an id" test -n "$id"
sleep 1
check "ps shows it Up" has "$(dk ps)" Up
check "ps shows the name" has "$(dk ps)" rlweb
check "logs show output" has "$(dk logs rlweb)" started
check "exec runs in the container" eq "$(dk exec rlweb hostname)" container
check "exec sees container processes" has "$(dk exec rlweb ps)" "sleep 120"
check "exec inherits env" eq "$(dk exec rlweb sh -c 'echo $FOO')" bar
check "exec is root in the userns" eq "$(dk exec rlweb id -u)" 0
dk stop -t 1 rlweb >/dev/null 2>&1
sleep 1
check "stop: container is gone" has "$(dk ps -a)" Exited
dk rm rlweb >/dev/null 2>&1
check "rm: not listed any more" hasnt "$(dk ps -a)" rlweb
check "the root engine's data dir was not used" test ! -e /root/.local/share/sasta-docker/containers

echo "== security still applies"
check "seccomp blocks unshare" sh -c "! rl $BIN run alpine unshare -m true </dev/null"
check "no-new-privileges is set" has "$(dk run --security-opt no-new-privileges alpine grep NoNewPrivs /proc/self/status)" "1"
check "cap-drop ALL: CapEff is 0" has "$(dk run --cap-drop ALL alpine grep CapEff /proc/self/status)" 0000000000000000
check "mount is refused in the container (seccomp)" sh -c "! rl $BIN run alpine mount -t tmpfs x /tmp </dev/null"

echo "== networking"
check "--network none: only lo" eq "$(dk run --network none alpine sh -c 'ls /sys/class/net 2>/dev/null; cat /proc/net/dev | tail -n +3 | wc -l' | tail -1)" 1
check "--network none: no outbound" sh -c "! rl $BIN run --network none alpine wget -q -T3 -O- http://1.1.1.1 </dev/null"
hostifs=$(tail -n +3 /proc/net/dev | wc -l)
check "--network host shares the host's interfaces" eq "$(dk run --network host alpine sh -c 'tail -n +3 /proc/net/dev | wc -l')" "$hostifs"
if command -v slirp4netns >/dev/null; then
  check "slirp4netns: own netns with tap0" has "$(dk run alpine sh -c 'cat /proc/net/dev')" tap0
  check "slirp4netns: not the host's interfaces" has "$(dk run alpine sh -c 'cat /proc/net/dev')" lo
  check "slirp4netns: DNS + outbound HTTP" has "$(dk run alpine wget -q -T10 -O- http://archive.ubuntu.com/ubuntu/ 2>&1 | head -c 2000)" "Index of"
  dk run -d --name pub -p 18080:80 alpine sh -c 'while true; do printf "HTTP/1.0 200 OK

rootless-ok
" | nc -l -p 80; done' >/dev/null
  sleep 1
  check "slirp4netns: -p publishes a TCP port" has "$(curl -s -m 5 http://127.0.0.1:18080/)" "rootless-ok"
  dk stop -t 1 pub >/dev/null 2>&1; dk rm pub >/dev/null 2>&1
fi
if command -v pasta >/dev/null; then
  export SASTA_ROOTLESS_NET=pasta
  check "pasta: outbound HTTP" has "$(dk run alpine wget -q -T10 -O- http://archive.ubuntu.com/ubuntu/ 2>&1 | head -c 2000)" "Index of"
  dk run -d --name pubp -p 18081:80 alpine sh -c 'while true; do printf "HTTP/1.0 200 OK

rootless-ok
" | nc -l -p 80; done' >/dev/null
  sleep 1
  check "pasta: -p publishes a TCP port" has "$(curl -s -m 5 http://127.0.0.1:18081/)" "rootless-ok"
  dk stop -t 1 pubp >/dev/null 2>&1; dk rm pubp >/dev/null 2>&1
  unset SASTA_ROOTLESS_NET
fi
export SASTA_ROOTLESS_NET=host
err=$(dk run alpine true 2>&1 >/dev/null)
check "no helper: warns about sharing the host netns" has "$err" "HOST network namespace"
check "no helper: -p with different ports is refused" sh -c "! rl $BIN run -p 18082:80 alpine true </dev/null"
unset SASTA_ROOTLESS_NET

echo "== user-defined networks (all containers of a network share one namespace)"
if command -v slirp4netns >/dev/null; then
  dk network create rlnet >/dev/null 2>&1
  check "network create" has "$(dk network ls)" rlnet
  dk run -d --name rlsrv --network rlnet alpine sh -c 'while true; do printf "HTTP/1.0 200 OK\n\nnet-ok\n" | nc -l -p 8080; done' >/dev/null
  sleep 2
  check "a container reaches another one on the network by name" has "$(dk run --rm --network rlnet alpine wget -q -T5 -O- http://rlsrv:8080/ 2>&1)" net-ok
  check "/etc/hosts lists the members" has "$(dk exec rlsrv cat /etc/hosts)" rlsrv
  dk run -d --name rlpub --network rlnet -p 18090:8081 alpine sh -c 'while true; do printf "HTTP/1.0 200 OK\n\npub-ok\n" | nc -l -p 8081; done' >/dev/null
  sleep 2
  check "-p publishes a port of a container on the network" has "$(curl -s -m 5 http://127.0.0.1:18090/)" pub-ok
  check "the other container on the network reaches it too" has "$(dk exec rlsrv wget -q -T5 -O- http://rlpub:8081/ 2>&1)" pub-ok
  dk stop -t 1 rlpub >/dev/null 2>&1; sleep 1
  check "-p is released when the container stops" sh -c "! curl -s -m 3 http://127.0.0.1:18090/ | grep -q pub-ok"
  check "outbound DNS + HTTP from the network" has "$(dk run --rm --network rlnet alpine wget -q -T10 -O- http://archive.ubuntu.com/ubuntu/ 2>&1 | head -c 2000)" "Index of"
  check "a container outside the network cannot reach it by name" sh -c "! rl $BIN run --rm alpine wget -q -T3 -O- http://rlsrv:8080/ </dev/null"
  check "an unknown network is refused" sh -c "! rl $BIN run --network nonet alpine true </dev/null"
  dk rm -f rlsrv rlpub >/dev/null 2>&1
  dk network rm rlnet >/dev/null 2>&1
  for _ in $(seq 1 30); do pgrep -u $U -f '[s]lirp4netns --configure' >/dev/null || break; sleep 1; done
  check "the network's background process goes away when it is idle" sh -c "! pgrep -u $U -f '[s]lirp4netns --configure' >/dev/null"
  pgrep -u $U -af '[s]lirp4netns --configure' | sed 's/^/    still running: /'
  dk ps | sed 's/^/    ps: /'; ls $RH/run 2>&1 | sed 's/^/    run: /'; tail -3 $RH/run/net-rlnet/log | sed 's/^/    log: /'
fi

echo "== docker build, rootless"
B=$RH/ctx; rm -rf "$B"; mkdir -p "$B/sub"; echo from-context > "$B/a.txt"; echo nested > "$B/sub/b.txt"; echo one > "$B/pkg1.json"; echo two > "$B/pkg2.json"
cat > "$B/Dockerfile" <<'EOF'
FROM alpine AS build
RUN mkdir -p /out && echo built-in-stage > /out/artifact && chmod 640 /out/artifact
FROM alpine
ENV GREETING=hi
WORKDIR /app
COPY a.txt ./
COPY sub ./sub/
COPY pkg*.json /cfg/
COPY --chown=1000:1000 a.txt /owned.txt
COPY --from=build /out/artifact /app/artifact
RUN echo "run-step $(cat /app/a.txt)" > /app/ran.txt && rm /app/sub/b.txt
CMD ["sh", "-c", "cat /app/ran.txt; cat /app/artifact; ls /cfg; stat -c '%u' /owned.txt; ls /app/sub | wc -l; echo $GREETING"]
EOF
chown -R $U:$U "$B"
out=$(dk build -t rlbuilt:1 "$B" 2>&1); check "build succeeds" has "$out" "Successfully built and tagged rlbuilt:1"
check "the image is listed" has "$(dk images)" rlbuilt
check "its layers are the rootless flavour" test -d "$RH/store/layers-rootless"
out=$(dk run --rm rlbuilt:1 2>&1)
check "RUN result is in the image" has "$out" "run-step from-context"
check "COPY --from stage works" has "$out" "built-in-stage"
check "COPY with a wildcard works" has "$out" "pkg1.json"
check "COPY --chown sets the owner (full map)" has "$out" "1000"
check "a file removed by a later RUN stays removed" has "$out" "0"
check "ENV is kept" has "$out" "hi"
out2=$(dk build -t rlbuilt:1 "$B" 2>&1); check "a second build uses the cache" has "$out2" "Using cache"
dk rmi rlbuilt:1 >/dev/null 2>&1

echo "== single-uid fallback (forced)"
export SASTA_ROOTLESS_MAP=single
err=$(dk run alpine true 2>&1 >/dev/null)
check "single map is reported" has "$err" "single-uid mapping"
check "single map: one line in uid_map" eq "$(dk run alpine sh -c 'wc -l < /proc/self/uid_map' 2>/dev/null)" 1
check "single map: echo still works" eq "$(dk run alpine echo ok 2>/dev/null)" ok
check "single map: chown to another uid fails" sh -c "! rl $BIN run alpine sh -c 'touch /tmp/x; chown 1234 /tmp/x' </dev/null 2>/dev/null"
check "single map: -u 1000 is ignored with a warning" has "$(dk run -u 1000 alpine id -u 2>&1)" "USER 1000 ignored"
unset SASTA_ROOTLESS_MAP

echo "== copy-up fallback (forced)"
export SASTA_ROOTLESS_COPY=1
out=$(dk run alpine sh -c 'echo c > /tmp/f; cat /tmp/f; ls /bin/sh' 2>/tmp/rl.err | tr '\n' ' ')
check "copy fallback runs the image" has "$out" "c /bin/sh"
check "copy fallback is reported" has "$(cat /tmp/rl.err)" "copying the image into RAM"
unset SASTA_ROOTLESS_COPY

echo "== cgroups"
err=$(dk run --mem 64 alpine true 2>&1 >/dev/null)
check "no delegation: limit is skipped with a warning" has "$err" "NOT enforced"
# a delegated subtree, the way systemd hands one to a user
mkdir -p /sys/fs/cgroup/rl-deleg
echo "+memory +pids +cpu" > /sys/fs/cgroup/rl-deleg/cgroup.subtree_control 2>/dev/null
mkdir -p /sys/fs/cgroup/rl-deleg/sup && chown -R $U /sys/fs/cgroup/rl-deleg
indeleg() { sh -c 'echo $$ > /sys/fs/cgroup/rl-deleg/sup/cgroup.procs; exec "$@"' sh "$@"; }
indeleg setpriv --reuid=$U --regid=$U --init-groups env HOME=$HOMEDIR RUST_DOCKER_HOME=$RH "$BIN" run -d --name lim --mem 64 --procs 50 alpine sleep 60 </dev/null >/dev/null
sleep 1
check "delegated: memory.max applied" eq "$(cat /sys/fs/cgroup/rl-deleg/rust-docker-*/memory.max 2>/dev/null)" 67108864
check "delegated: pids.max applied" eq "$(cat /sys/fs/cgroup/rl-deleg/rust-docker-*/pids.max 2>/dev/null)" 50
check "delegated: stats work" has "$(dk stats --no-stream lim 2>&1)" "lim"
dk stop -t 1 lim >/dev/null 2>&1; dk rm lim >/dev/null 2>&1

echo "== root engine unaffected: run scripts/e2e.sh separately"
echo "failures: $fail"
[ "$fail" -eq 0 ]
