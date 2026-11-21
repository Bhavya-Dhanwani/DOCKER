#!/bin/sh
# save / load end-to-end test (Linux, root, offline: alpine from the store):
#   scripts/e2e-save.sh <docker-bin> <store-dir-with-alpine>
D=${1:?docker binary}; SRC=${2:?store dir}
# run a local copy: executing straight from a Windows-mounted drive is flaky under WSL
cp "$D" /root/dk.$$ && chmod +x /root/dk.$$ && D=/root/dk.$$
export RUST_DOCKER_HOME=$(mktemp -d /root/e2e-save.XXXXXX)
work=$(mktemp -d)
trap 'rm -f /root/dk.$$; rm -rf "$work" "$RUST_DOCKER_HOME"' EXIT
mkdir -p "$RUST_DOCKER_HOME" && cp -r "$SRC" "$RUST_DOCKER_HOME/store"
fail=0
check() { d=$1; shift; if "$@" >/dev/null 2>&1; then echo "PASS $d"; else echo "FAIL $d"; fail=$((fail+1)); fi; }
has() { case "$1" in *"$2"*) return 0 ;; *) return 1 ;; esac; }
eq() { [ "$1" = "$2" ]; }
N="--network none"
layers() { ls "$RUST_DOCKER_HOME/store/layers" | wc -l; }

mkdir -p "$work/ctx"; echo from-file > "$work/ctx/hello.txt"
cat > "$work/ctx/Dockerfile" <<'EOF'
FROM alpine
ENV GREETING=saved
LABEL kind=archive-test
EXPOSE 9090
WORKDIR /app
COPY hello.txt /app/
RUN chmod 640 /app/hello.txt && chown nobody /app/hello.txt && ln -s /app/hello.txt /app/link && rm /etc/motd
USER nobody
CMD ["cat", "/app/hello.txt"]
EOF
$D build -t app:1 "$work/ctx" >/dev/null 2>&1 || { echo "FAIL could not build the test image"; exit 1; }

echo "== save"
$D save -o "$work/app.tar" app:1 alpine 2>/dev/null; check "save writes an archive" test -s "$work/app.tar"
listing=$(tar -tf "$work/app.tar")
check "it is an OCI image layout (oci-layout, index.json, blobs/)" sh -c "echo '$listing' | grep -q '^oci-layout$' && echo '$listing' | grep -q '^index.json$' && echo '$listing' | grep -q '^blobs/sha256/'"
check "the image names are recorded" sh -c "tar -xOf $work/app.tar index.json | grep -q 'app:1' && tar -xOf $work/app.tar index.json | grep -q 'alpine:latest'"
n=$(echo "$listing" | grep -c '^blobs/sha256/[0-9a-f]\{64\}$')
check "layers are stored once even when images share them" sh -c "[ $n -le 8 ]"
check "saving an unknown image fails and leaves no file" sh -c "! $D save -o $work/bad.tar nosuch:1 2>/dev/null && [ ! -e $work/bad.tar ]"
check "save needs -o" sh -c "! $D save app:1 2>/dev/null"

echo "== load"
$D rmi app:1 >/dev/null; $D rmi alpine >/dev/null; $D prune >/dev/null
check "everything is gone locally" eq "$(layers)" 0
out=$($D load -i "$work/app.tar" 2>/dev/null)
check "load reports both images" sh -c "echo '$out' | grep -q 'app:1' && echo '$out' | grep -q 'alpine:latest'"
check "the loaded image runs its CMD (as USER nobody)" eq "$($D run $N app:1 </dev/null)" from-file
check "ENV survived" eq "$($D run $N app:1 sh -c 'echo $GREETING' </dev/null)" saved
check "WORKDIR survived" eq "$($D run $N app:1 pwd </dev/null)" /app
check "file mode survived (640)" eq "$($D run $N app:1 stat -c %a /app/hello.txt </dev/null)" 640
check "file ownership survived (nobody)" eq "$($D run $N app:1 stat -c %U /app/hello.txt </dev/null)" nobody
check "symlink survived" eq "$($D run $N app:1 readlink /app/link </dev/null)" /app/hello.txt
check "deletion (whiteout) survived" sh -c "! $D run $N app:1 ls /etc/motd </dev/null >/dev/null 2>&1"
check "the process runs as nobody" eq "$($D run $N app:1 id -un </dev/null)" nobody
check "labels and exposed ports survived" sh -c "grep -q archive-test $RUST_DOCKER_HOME/store/images/app_1.json && grep -q 9090 $RUST_DOCKER_HOME/store/images/app_1.json"
check "the base image works again" eq "$($D run $N alpine echo ok </dev/null)" ok
check "loading twice is harmless" sh -c "$D load -i $work/app.tar 2>/dev/null | grep -q app:1"

echo "== Docker's own archive format"
hex=; for l in "$RUST_DOCKER_HOME"/store/layers/*; do [ -e "$l/bin/busybox" ] && hex=$(basename "$l") && break; done
mkdir -p "$work/legacy/l0"
tar -C "$RUST_DOCKER_HOME/store/layers/$hex" -cf "$work/legacy/l0/layer.tar" .
echo '{"architecture":"arm64","os":"linux","config":{"Cmd":["echo","from-docker-archive"],"Env":["PATH=/bin:/usr/bin"]},"rootfs":{"type":"layers","diff_ids":[]}}' > "$work/legacy/cfg.json"
echo '[{"Config":"cfg.json","RepoTags":["legacy:1","legacy:latest"],"Layers":["l0/layer.tar"]}]' > "$work/legacy/manifest.json"
(cd "$work/legacy" && tar -cf ../legacy.tar manifest.json cfg.json l0/layer.tar)
out=$($D load -i "$work/legacy.tar" 2>/dev/null)
check "load reads a manifest.json (docker save) archive" sh -c "echo '$out' | grep -q legacy:1 && echo '$out' | grep -q legacy:latest"
check "and the image runs" eq "$($D run $N legacy:1 </dev/null)" from-docker-archive

echo "== bad archives"
head -c 3000 "$work/app.tar" > "$work/trunc.tar"
before=$(layers)
check "a truncated archive is rejected" sh -c "! $D load -i $work/trunc.tar >/dev/null 2>&1"
echo "not a tar" > "$work/junk.tar"
check "a non-archive is rejected" sh -c "! $D load -i $work/junk.tar >/dev/null 2>&1"
mkdir -p "$work/corrupt"; (cd "$work/corrupt" && tar -xf ../app.tar)
blob=$(ls -S "$work/corrupt/blobs/sha256" | head -1); echo tamper >> "$work/corrupt/blobs/sha256/$blob"
(cd "$work/corrupt" && tar -cf ../corrupt.tar .)
# the damaged layer must not already be in the store (a layer present by digest is simply reused)
for t in alpine:latest app:1 legacy:1 legacy:latest; do $D rmi $t >/dev/null 2>&1; done; $D prune >/dev/null
out=$($D load -i "$work/corrupt.tar" 2>&1)
check "a layer whose digest does not match is refused" has "$out" "digest mismatch"
check "nothing half-loaded is left tagged" sh -c "! $D images | grep -q app:1"
check "staging is cleaned up" sh -c "! ls $RUST_DOCKER_HOME/store/tmp 2>/dev/null | grep -q ."

echo "== cleanup"
check "no leftover run dirs" sh -c "! ls /run/rust-docker/* 2>/dev/null | grep -q ."
check "no leftover cgroups" sh -c "! ls /sys/fs/cgroup | grep -q rust-docker"
if [ $fail -eq 0 ]; then echo "ALL PASSED"; else echo "$fail FAILED"; exit 1; fi
