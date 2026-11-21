#!/bin/sh
# Multi-stage builds, COPY --from / --chown, .dockerignore, --target (Linux, root, offline: alpine from the store):
#   scripts/e2e-build2.sh <docker-bin> <store-dir-with-alpine>
D=${1:?docker binary}; SRC=${2:?store dir}
# run a local copy: executing straight from a Windows-mounted drive is flaky under WSL
cp "$D" /root/dk.$$ && chmod +x /root/dk.$$ && D=/root/dk.$$
export RUST_DOCKER_HOME=$(mktemp -d /root/e2e-b2.XXXXXX)
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
N="--network none"

echo "== multi-stage"
cat > "$ctx/Dockerfile" <<'EOF'
FROM alpine AS builder
RUN mkdir -p /out/sub && echo built-artifact > /out/app && echo nested > /out/sub/n && echo temp > /out/gone && chmod 750 /out/app
RUN rm /out/gone

FROM builder AS second
RUN echo second-stage > /second

FROM alpine
COPY --from=builder /out /app
COPY --from=builder --chown=1000:1000 /out/app /owned
COPY --from=0 /out/sub/n /by-index
COPY --from=alpine /etc/alpine-release /rel
CMD ["cat", "/app/app"]
EOF
out=$($D build -t multi "$ctx" 2>&1); rc=$?
check "multi-stage build succeeds" test $rc -eq 0
check "the final image holds only the last stage" eq "$($D run $N multi </dev/null)" built-artifact
check "COPY --from=<stage> copies a directory tree" eq "$($D run $N multi cat /app/sub/n </dev/null)" nested
check "a file deleted in the stage (whiteout) is not copied" sh -c "! $D run $N multi ls /app/gone </dev/null >/dev/null 2>&1"
check "the stage's own files are NOT in the final image" sh -c "! $D run $N multi ls /out </dev/null >/dev/null 2>&1"
check "COPY --from=<index> works" eq "$($D run $N multi cat /by-index </dev/null)" nested
check "COPY --from=<image> reads another image" sh -c "$D run $N multi cat /rel </dev/null | grep -q ."
check "file mode survives the stage copy (750)" eq "$($D run $N multi stat -c %a /owned </dev/null)" 750
check "--chown sets the owner (1000:1000)" eq "$($D run $N multi stat -c '%u:%g' /owned </dev/null)" 1000:1000
check "without --chown files are root-owned" eq "$($D run $N multi stat -c %u /app/app </dev/null)" 0
check "only the last stage is tagged" sh -c "$D images | grep -c multi | grep -q 1 && ! $D images | grep -q builder"
out=$($D build -t multi "$ctx" 2>&1)
n=$(printf '%s\n' "$out" | grep -c "Using cache")
check "an unchanged rebuild uses the cache for every layer step" test "$n" -ge 6

echo "== --target and FROM <stage>"
$D build --target second -t second "$ctx" >/dev/null 2>&1; check "--target second builds" test $? -eq 0
check "the target image has the earlier stage's files" eq "$($D run $N second cat /out/app </dev/null)" built-artifact
check "and its own" eq "$($D run $N second cat /second </dev/null)" second-stage
check "but nothing from later stages" sh -c "! $D run $N second ls /owned </dev/null >/dev/null 2>&1"
$D build --target builder -t first "$ctx" >/dev/null 2>&1; check "--target on the first stage" eq "$($D run $N first cat /out/app </dev/null)" built-artifact
check "an unknown --target is an error" sh -c "! $D build --target nope -t x '$ctx' >/dev/null 2>&1"
printf 'FROM alpine\nCOPY --from=nosuchstage-and-no-image /x /y\n' > "$ctx/bad.df"
check "COPY --from an unknown stage/image fails the build" sh -c "! $D build -t bad -f '$ctx/bad.df' '$ctx' >/dev/null 2>&1"

echo "== .dockerignore"
rm -rf "$ctx"/*; mkdir -p "$ctx/logs" "$ctx/node_modules/pkg" "$ctx/src"
echo keep > "$ctx/keep.txt"; echo SECRET > "$ctx/secret.env"; echo l > "$ctx/logs/a.log"; echo l2 > "$ctx/logs/keep.log"
echo m > "$ctx/node_modules/pkg/i.js"; echo code > "$ctx/src/main.rs"
printf '# ignore\nsecret.env\n**/*.log\n!logs/keep.log\nnode_modules\n' > "$ctx/.dockerignore"
printf 'FROM alpine\nCOPY . /ctx/\nCMD ["ls", "-R", "/ctx"]\n' > "$ctx/Dockerfile"
$D build -t ign "$ctx" >/dev/null 2>&1; check "build with a .dockerignore succeeds" test $? -eq 0
listing=$($D run $N ign </dev/null)
check "kept files are copied" sh -c "echo '$listing' | grep -q keep.txt && echo '$listing' | grep -q main.rs"
check "ignored files are not" sh -c "! echo '$listing' | grep -q secret.env"
check "ignored directories are not" sh -c "! echo '$listing' | grep -q node_modules"
check "**/*.log ignores nested logs" sh -c "! echo '$listing' | grep -q 'a.log'"
check "! re-includes logs/keep.log" sh -c "echo '$listing' | grep -q 'keep.log'"
echo "changed secret" > "$ctx/secret.env"
out=$($D build -t ign "$ctx" 2>&1)
check "changing an ignored file does not bust the cache" has "$out" "Using cache"
printf 'FROM alpine\nCOPY secret.env /s\n' > "$ctx/Dockerfile.2"
check "COPY of an ignored file is an error" has "$($D build -t ign2 -f "$ctx/Dockerfile.2" "$ctx" 2>&1)" ".dockerignore"

echo "== cleanup"
for t in multi second first ign; do $D rmi $t >/dev/null 2>&1; done
$D prune >/dev/null
check "prune leaves only the base layer" eq "$(ls $RUST_DOCKER_HOME/store/layers | wc -l)" 1
check "no leftover run dirs" sh -c "! ls /run/rust-docker/* 2>/dev/null | grep -q ."
check "no leftover cgroups" sh -c "! ls /sys/fs/cgroup | grep -q rust-docker"
if [ $fail -eq 0 ]; then echo "ALL PASSED"; else echo "$fail FAILED"; exit 1; fi
