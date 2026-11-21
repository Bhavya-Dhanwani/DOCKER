#!/bin/sh
# login / tag / push / pull round trip against scripts/mock-registry.py (Linux, root, offline, needs python3):
#   scripts/e2e-push.sh <docker-bin> <store-dir-with-alpine>
D=${1:?docker binary}; SRC=${2:?store dir}
HERE=$(cd "$(dirname "$0")" && pwd)
# run a local copy: executing straight from a Windows-mounted drive is flaky under WSL
cp "$D" /root/dk.$$ && chmod +x /root/dk.$$ && D=/root/dk.$$
export RUST_DOCKER_HOME=$(mktemp -d /root/e2e-push.XXXXXX)
ctx=$(mktemp -d); log=$(mktemp)
trap 'kill $REG 2>/dev/null; rm -f /root/dk.$$ "$log"; rm -rf "$ctx" "$RUST_DOCKER_HOME"' EXIT
for cg in /sys/fs/cgroup/rust-docker-*; do [ -d "$cg" ] && echo 1 > "$cg/cgroup.kill"; done; sleep 1
for cg in /sys/fs/cgroup/rust-docker-*; do [ -d "$cg" ] && rmdir "$cg" 2>/dev/null; done
for l in $(ls /sys/class/net | grep '^rdv'); do ip link del "$l" 2>/dev/null; done
mkdir -p "$RUST_DOCKER_HOME" && cp -r "$SRC" "$RUST_DOCKER_HOME/store"
fail=0
check() { d=$1; shift; if "$@" >/dev/null 2>&1; then echo "PASS $d"; else echo "FAIL $d"; fail=$((fail+1)); fi; }
has() { case "$1" in *"$2"*) return 0 ;; *) return 1 ;; esac; }
eq() { [ "$1" = "$2" ]; }

echo "from-file" > "$ctx/hello.txt"; mkdir -p "$ctx/data/sub"; echo deep > "$ctx/data/sub/x"
cat > "$ctx/Dockerfile" <<'EOF'
FROM alpine
ENV NAME=pushed
LABEL purpose=roundtrip
EXPOSE 8080
WORKDIR /app
COPY hello.txt /app/
COPY data/ /data/
RUN echo "hi $NAME" > /app/msg && chmod 640 /app/msg && ln -s /app/msg /app/link && rm /etc/motd
CMD ["cat", "/app/msg"]
EOF
build_app() { $D build -t app "$ctx" >/dev/null 2>&1 || { echo "FAIL could not build the test image"; exit 1; }; }

round_trip() { # <mode> <port>
  mode=$1; port=$2; repo="localhost:$port/team/app"
  build_app   # layers are cached after the first round
  echo "== registry auth: $mode"
  python3 "$HERE/mock-registry.py" $port $mode me:secret 2>"$log" & REG=$!
  sleep 1
  if [ $mode != none ]; then
    check "push without login is refused with a clear message" has "$($D tag app $repo:v1 2>&1; $D push $repo:v1 2>&1)" "docker login"
    check "login with a wrong password fails" sh -c "! $D login -u me -p wrong localhost:$port >/dev/null 2>&1"
    check "login with the right password works" sh -c "echo secret | $D login -u me --password-stdin localhost:$port"
    check "credentials are stored with mode 600" eq "$(stat -c %a $RUST_DOCKER_HOME/store/auth.json)" 600
  else
    $D tag app $repo:v1
  fi
  out=$($D push $repo:v1 2>&1); rc=$?
  check "push succeeds" test $rc -eq 0
  check "push reports a digest" has "$out" "digest: sha256:"
  check "mock saw blob uploads and a manifest PUT" sh -c "grep -q 'PUT /v2/team/app/manifests/v1 -> 201' $log && grep -q 'PUT /v2/team/app/blobs/uploads/' $log"
  check "second push finds every layer already there" has "$($D push $repo:v1 2>&1)" "Layer already exists"

  $D rmi $repo:v1 >/dev/null 2>&1; $D rmi app >/dev/null 2>&1; $D prune >/dev/null
  check "nothing left locally after rmi+prune except the base" sh -c "[ $(ls $RUST_DOCKER_HOME/store/layers | wc -l) -le 1 ]"
  out=$($D pull $repo:v1 2>&1); rc=$?
  check "pull from the registry succeeds (digests verified)" test $rc -eq 0
  img=$repo:v1
  check "pulled image runs the pushed CMD + ENV" eq "$($D run --network none $img </dev/null)" "hi pushed"
  check "COPY'd file survived" eq "$($D run --network none $img cat /app/hello.txt </dev/null)" "from-file"
  check "COPY'd directory survived" eq "$($D run --network none $img cat /data/sub/x </dev/null)" "deep"
  check "file mode survived (640)" eq "$($D run --network none $img stat -c %a /app/msg </dev/null)" 640
  check "symlink survived" eq "$($D run --network none $img readlink /app/link </dev/null)" /app/msg
  check "deletion (whiteout) survived the tarball round trip" sh -c "! $D run --network none $img ls /etc/motd </dev/null >/dev/null 2>&1"
  check "base files are still there" $D run --network none $img ls /etc/passwd </dev/null
  check "WORKDIR/EXPOSE/LABEL are in the pulled config" has "$(cat $RUST_DOCKER_HOME/store/images/localhost_${port}_team_app_v1.json)" "8080/tcp"
  check "WORKDIR is applied" eq "$($D run --network none $img pwd </dev/null)" /app
  $D rmi $repo:v1 >/dev/null 2>&1
  kill $REG; wait $REG 2>/dev/null; $D logout localhost:$port >/dev/null 2>&1
}

round_trip bearer 15001
round_trip basic 15002
round_trip none 15003

echo "== cleanup"
check "no leftover run dirs" sh -c "! ls /run/rust-docker/* 2>/dev/null | grep -q ."
check "no leftover cgroups" sh -c "! ls /sys/fs/cgroup | grep -q rust-docker"
if [ $fail -eq 0 ]; then echo "ALL PASSED"; else echo "$fail FAILED"; exit 1; fi
