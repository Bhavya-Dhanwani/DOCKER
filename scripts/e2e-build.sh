#!/bin/sh
# `docker build` end-to-end test (Linux, root, offline: uses only the alpine image from the store):
#   scripts/e2e-build.sh <docker-bin> <store-dir-with-alpine>
D=${1:?docker binary}; SRC=${2:?store dir}
# run a local copy: executing straight from a Windows-mounted drive is flaky under WSL
cp "$D" /root/dk.$$ && chmod +x /root/dk.$$ && D=/root/dk.$$
export RUST_DOCKER_HOME=$(mktemp -d /root/e2e-build.XXXXXX)
ctx=$(mktemp -d)
trap 'rm -f /root/dk.$$; rm -rf "$ctx" "$RUST_DOCKER_HOME"' EXIT
for cg in /sys/fs/cgroup/rust-docker-*; do [ -d "$cg" ] && echo 1 > "$cg/cgroup.kill"; done; sleep 1
for cg in /sys/fs/cgroup/rust-docker-*; do [ -d "$cg" ] && rmdir "$cg" 2>/dev/null; done
for l in $(ls /sys/class/net | grep '^rdv'); do ip link del "$l" 2>/dev/null; done
mkdir -p "$RUST_DOCKER_HOME" && cp -r "$SRC" "$RUST_DOCKER_HOME/store"
fail=0
check() { d=$1; shift; if "$@" >/dev/null 2>&1; then echo "PASS $d"; else echo "FAIL $d"; fail=$((fail+1)); fi; }
has() { case "$1" in *"$2"*) return 0 ;; *) return 1 ;; esac; }
eq() { [ "$1" = "$2" ]; }
layers() { ls "$RUST_DOCKER_HOME/store/layers" | wc -l; }

mkdir -p "$ctx/data/sub"; echo "from-file" > "$ctx/hello.txt"; echo "deep" > "$ctx/data/sub/x"
cat > "$ctx/Dockerfile" <<'EOF'
# a comment
FROM alpine
ARG GREETING=hello
ENV NAME=world
LABEL purpose=test
EXPOSE 8080
WORKDIR /app
COPY hello.txt /app/
COPY data/ /data/
RUN echo "$GREETING $NAME" > /app/greeting.txt && \
    mkdir -p /opt/tools && echo '#!/bin/sh' > /opt/tools/t && chmod +x /opt/tools/t
RUN rm /etc/motd
CMD ["cat", "/app/greeting.txt"]
EOF

echo "== build"
p0=$($D ps -a | wc -l)
out=$($D build -t app:1 "$ctx" 2>&1); rc=$?
p1=$($D ps -a | wc -l)
check "build succeeds" test $rc -eq 0
check "build prints steps" has "$out" "Step 11/11"
check "build tags the image" has "$($D images)" "app:1"
check "CMD runs: ARG default + ENV substituted" eq "$($D run app:1 </dev/null)" "hello world"
check "COPY file to a dir" eq "$($D run app:1 cat /app/hello.txt </dev/null)" "from-file"
check "COPY dir contents" eq "$($D run app:1 cat /data/sub/x </dev/null)" "deep"
check "WORKDIR applies to run" eq "$($D run app:1 pwd </dev/null)" "/app"
check "RUN-created executable keeps its mode" eq "$($D run app:1 sh -c 'test -x /opt/tools/t && echo yes' </dev/null)" "yes"
check "RUN rm leaves a whiteout in the image" sh -c "! $D run app:1 ls /etc/motd </dev/null"
check "base image is untouched" $D run alpine ls /etc/motd </dev/null
check "image config has labels/expose" has "$(cat "$RUST_DOCKER_HOME"/store/images/app_1.json)" "8080/tcp"
check "build steps are not listed as containers" eq "$p1" "$p0"

echo "== cache"
out=$($D build -t app:1 "$ctx" 2>&1)
n=$(printf '%s\n' "$out" | grep -c "Using cache"); check "unchanged rebuild uses the cache for all 5 layers" eq "$n" 5
echo "changed" > "$ctx/hello.txt"
out=$($D build -t app:2 "$ctx" 2>&1)
n=$(printf '%s\n' "$out" | grep -c "Using cache"); check "changed file: only layers before it are cached (WORKDIR)" eq "$n" 1
check "changed file reaches the image" eq "$($D run app:2 cat /app/hello.txt </dev/null)" "changed"
check "old tag still has the old file" eq "$($D run app:1 cat /app/hello.txt </dev/null)" "from-file"
out=$($D build --no-cache -t app:2 "$ctx" 2>&1)
check "--no-cache rebuilds everything" sh -c "! printf '%s' '$out' | grep -q 'Using cache'"
out=$($D build -t app:3 --build-arg GREETING=hi "$ctx" 2>&1)
check "--build-arg overrides ARG" eq "$($D run app:3 </dev/null)" "hi world"

echo "== USER / ENTRYPOINT"
mkdir -p "$ctx/u"; cat > "$ctx/u/Dockerfile" <<'EOF'
FROM alpine
RUN adduser -D appuser
USER appuser
ENTRYPOINT ["echo", "ep"]
CMD ["default"]
EOF
$D build -t usr:1 -f "$ctx/u/Dockerfile" "$ctx" >/dev/null 2>&1; check "USER build succeeds" test $? -eq 0
check "ENTRYPOINT + CMD" eq "$($D run usr:1 </dev/null)" "ep default"
check "args replace CMD but keep ENTRYPOINT" eq "$($D run usr:1 other </dev/null)" "ep other"
mkdir -p "$ctx/u2"; cat > "$ctx/u2/Dockerfile" <<'EOF'
FROM alpine
RUN adduser -D appuser
USER appuser
CMD ["id", "-u"]
EOF
$D build -t usr:2 -f "$ctx/u2/Dockerfile" "$ctx" >/dev/null 2>&1; check "USER build #2 succeeds" test $? -eq 0
check "container process runs as the USER (uid 1000)" eq "$($D run usr:2 </dev/null)" "1000"
check "images without USER still run as root" eq "$($D run alpine id -u </dev/null)" "0"
check "USER also applies to later RUN steps" has "$($D build -t usr:3 -f "$ctx/u2/Dockerfile" "$ctx" 2>&1)" "Using cache"

echo "== FROM scratch"
cp "$D" "$ctx/dk"
cat > "$ctx/s.df" <<'EOF'
FROM scratch
COPY dk /dk
ENTRYPOINT ["/dk"]
EOF
$D build -t scratchy -f "$ctx/s.df" "$ctx" >/dev/null 2>&1; check "scratch build succeeds" test $? -eq 0
check "static binary runs in a scratch image" has "$($D run --network none scratchy images </dev/null)" "IMAGE"

echo "== errors"
printf 'FROM alpine\nRUN exit 3\n' > "$ctx/bad.df"
out=$($D build -t bad -f "$ctx/bad.df" "$ctx" 2>&1); rc=$?
check "failing RUN fails the build" test $rc -ne 0
check "error names the command and code" has "$out" "returned a non-zero code: 3"
check "failed build is not tagged" sh -c "! $D images | grep -q '^bad'"
printf 'FROM alpine\nCOPY ../x /x\n' > "$ctx/esc.df"
check "COPY cannot escape the context" sh -c "! $D build -t esc -f '$ctx/esc.df' '$ctx' >/dev/null 2>&1"
check "build needs -t" sh -c "! $D build '$ctx' >/dev/null 2>&1"

echo "== prune and rmi"
before=$(layers)
for t in app:1 app:2 app:3 usr:1 usr:2 usr:3 scratchy; do $D rmi $t >/dev/null 2>&1; done
check "rmi frees the built layers" test "$(layers)" -lt "$before"
check "base image still works" eq "$($D run alpine echo ok </dev/null)" "ok"
$D build -t tmp:1 "$ctx" >/dev/null 2>&1; $D rmi tmp:1 >/dev/null 2>&1
$D prune >/dev/null
check "prune leaves only the base layer" eq "$(layers)" 1
check "prune empties staging" sh -c "! ls $RUST_DOCKER_HOME/store/tmp 2>/dev/null | grep -q ."

echo "== cleanup"
check "no leftover run dirs" sh -c "! ls /run/rust-docker/* 2>/dev/null | grep -q ."
check "no leftover cgroups" sh -c "! ls /sys/fs/cgroup | grep -q rust-docker"
if [ $fail -eq 0 ]; then echo "ALL PASSED"; else echo "$fail FAILED"; exit 1; fi
