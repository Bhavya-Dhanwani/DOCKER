#!/bin/sh
# Registry completeness against scripts/mock-registry.py (Linux, root, offline, needs python3):
# chunked + resumable upload, cross-repo mount, retries on 503, digest pulls, zstd layers, manifest-list platform
# choice, credential helpers, ~/.docker/config.json auths, image inspect / history.
#   scripts/e2e-registry2.sh <docker-bin> <store-dir-with-alpine>
# zstd layers are made by the mock registry with hand-built zstd frames of raw blocks (no zstd CLI/python module needed).
D=${1:?docker binary}; SRC=${2:?store dir}
HERE=$(cd "$(dirname "$0")" && pwd)
cp "$D" /root/dk.$$ && chmod +x /root/dk.$$ && D=/root/dk.$$
export RUST_DOCKER_HOME=$(mktemp -d /root/e2e-reg2.XXXXXX)
ctx=$(mktemp -d); log=$(mktemp); work=$(mktemp -d)
export DOCKER_CONFIG=$work/dockercfg; mkdir -p "$DOCKER_CONFIG" "$work/bin"
trap 'kill $REG 2>/dev/null; rm -f /root/dk.$$ "$log"; rm -rf "$ctx" "$work" "$RUST_DOCKER_HOME"' EXIT
for cg in /sys/fs/cgroup/rust-docker-*; do [ -d "$cg" ] && echo 1 > "$cg/cgroup.kill"; done; sleep 1
for cg in /sys/fs/cgroup/rust-docker-*; do [ -d "$cg" ] && rmdir "$cg" 2>/dev/null; done
for l in $(ls /sys/class/net | grep '^rdv'); do ip link del "$l" 2>/dev/null; done
mkdir -p "$RUST_DOCKER_HOME" && cp -r "$SRC" "$RUST_DOCKER_HOME/store"
fail=0
check() { d=$1; shift; if "$@" >/dev/null 2>&1; then echo "PASS $d"; else echo "FAIL $d"; fail=$((fail+1)); fi; }
has() { case "$1" in *"$2"*) return 0 ;; *) return 1 ;; esac; }
eq() { [ "$1" = "$2" ]; }
inlog() { grep -q -- "$1" "$log"; }
jget() { python3 -c 'import json,sys; print(json.load(sys.stdin)[sys.argv[1]])' "$1"; }

echo "from-file" > "$ctx/hello.txt"
cat > "$ctx/Dockerfile" <<'EOF'
FROM alpine
ENV NAME=pushed
COPY hello.txt /app/
RUN echo "hi $NAME" > /app/msg
CMD ["cat", "/app/msg"]
EOF
$D build -t app "$ctx" >/dev/null 2>&1 || { echo "FAIL could not build the test image"; exit 1; }
case $(uname -m) in x86_64) HOSTARCH=amd64; OTHER=arm64 ;; *) HOSTARCH=arm64; OTHER=amd64 ;; esac

start_reg() { # <port> <mode> [flaky]
  : > "$log"; python3 "$HERE/mock-registry.py" $1 $2 me:secret $3 2>"$log" & REG=$!; sleep 1
}
stop_reg() { kill $REG 2>/dev/null; wait $REG 2>/dev/null; }

P=15021; R=localhost:$P/team
echo "== flaky registry: chunked upload, retries, mount, digest pull, zstd, platforms"
start_reg $P none flaky
$D tag app $R/app:v1
out=$(RUST_DOCKER_PUSH_CHUNK=524288 $D push $R/app:v1 2>&1); rc=$?
check "push succeeds against a registry that 503s the first try of everything" test $rc -eq 0
check "mock really returned 503s (retries were needed)" inlog " -> 503"
check "a big layer went up in PATCH chunks" inlog "PATCH /v2/team/app/blobs/uploads/"
check "chunked blob was assembled and verified by the registry (resumed after a dropped chunk)" inlog "BLOB team/app sha256:.* chunked"
check "small blobs still go monolithic" inlog "BLOB team/app sha256:.* monolithic"
check "upload status GET was used to resume" inlog "GET /v2/team/app/blobs/uploads/"
DIGEST=$(echo "$out" | sed -n 's/.*digest: \(sha256:[0-9a-f]*\).*/\1/p' | tail -1)
check "push reports the manifest digest" has "$DIGEST" sha256:

$D tag app $R/app2:v1
out=$(RUST_DOCKER_PUSH_CHUNK=524288 $D push $R/app2:v1 2>&1); rc=$?
check "push of a second repo succeeds" test $rc -eq 0
check "layers were mounted from the first repo" has "$out" "Mounted from team/app"
check "mock saw the mount" inlog "MOUNT team/app2 <- team/app"
check "mount sent no chunks to the new repo" sh -c "! grep -q 'PATCH /v2/team/app2/' $log"
check "second push of the same repo skips every layer" has "$(RUST_DOCKER_PUSH_CHUNK=524288 $D push $R/app2:v1 2>&1)" "Layer already exists"

$D rmi $R/app:v1 >/dev/null 2>&1; $D rmi $R/app2:v1 >/dev/null 2>&1; $D rmi app >/dev/null 2>&1; $D prune >/dev/null
out=$($D pull $R/app@$DIGEST 2>&1); rc=$?
check "pull by digest succeeds" test $rc -eq 0
check "pull by digest asked the registry for that digest" inlog "GET /v2/team/app/manifests/$DIGEST"
check "digest-pulled image runs" eq "$($D run --network none $R/app@$DIGEST </dev/null)" "hi pushed"
check "image is listed under its digest name" has "$($D images)" "app@sha256:"
$D rmi $R/app@$DIGEST >/dev/null 2>&1
check "repo:tag@digest pulls too" sh -c "$D pull $R/app:v1@$DIGEST >/dev/null 2>&1"
check "a wrong digest is refused" sh -c "! $D pull $R/app@sha256:$(printf 'b%.0s' $(seq 64)) >/dev/null 2>&1"
$D rmi $R/app@$DIGEST >/dev/null 2>&1; $D prune >/dev/null

echo "== zstd layers"
curl -s -X POST "http://127.0.0.1:$P/__recompress/team/app/v1/zst" >/dev/null
check "pull of a zstd-layer image succeeds" sh -c "$D pull $R/app:zst >/dev/null 2>&1"
check "stored media type is zstd" has "$(cat $RUST_DOCKER_HOME/store/images/localhost_${P}_team_app_zst.json)" "+zstd"
check "zstd image runs and has the right files" eq "$($D run --network none $R/app:zst </dev/null)" "hi pushed"
check "zstd layer content survived" eq "$($D run --network none $R/app:zst cat /app/hello.txt </dev/null)" "from-file"
$D rmi $R/app:zst >/dev/null 2>&1; $D prune >/dev/null

echo "== multi-arch index"
idx=$(curl -s -X POST "http://127.0.0.1:$P/__index/team/app/v1/idx/$HOSTARCH")
REAL=$(echo "$idx" | jget real); DECOY=$(echo "$idx" | jget decoy)
: > "$log"
check "pull of an index picks the host architecture" sh -c "$D pull $R/app:idx >/dev/null 2>&1"
check "  ... by fetching the host-arch manifest" inlog "manifests/$REAL"
check "  ... and not the other one" sh -c "! grep -q manifests/$DECOY $log"
: > "$log"
check "--platform linux/$OTHER selects the other entry" sh -c "$D pull --platform linux/$OTHER $R/app:idx >/dev/null 2>&1"
check "  ... by fetching its manifest" inlog "manifests/$DECOY"
check "--platform with a missing arch fails clearly" has "$($D pull --platform linux/riscv64 $R/app:idx 2>&1)" "no linux/riscv64 build"
check "a malformed --platform is rejected" sh -c "! $D pull --platform nonsense/x/y/z $R/app:idx >/dev/null 2>&1"

echo "== image inspect / history"
$D pull $R/app:idx >/dev/null 2>&1
insp=$($D image inspect $R/app:idx)
check "image inspect shows the image ID (config digest)" has "$insp" '"Id": "sha256:'
check "image inspect shows the repo digest" has "$insp" "team/app@sha256:"
check "image inspect lists the layers and size" sh -c "echo '$insp' | python3 -c 'import json,sys; d=json.load(sys.stdin)[0]; assert d[\"Size\"]>0 and d[\"RootFS\"][\"Layers\"]'"
check "docker inspect falls back to images" has "$($D inspect $R/app:idx)" '"RootFS"'
check "history lists the layers" has "$($D history $R/app:idx)" "CREATED BY"
check "image history alias works" has "$($D image history $R/app:idx)" "SIZE"
stop_reg

echo "== credential helper (credsStore) and config.json auths"
$D build -t app "$ctx" >/dev/null 2>&1 || { echo "FAIL could not rebuild the test image"; exit 1; }
cat > "$work/bin/docker-credential-fake" <<EOF
#!/bin/sh
f=$work/helper-creds; case "\$1" in
  store) cat > \$f; echo store >> $work/helper-calls ;;
  get) cat >/dev/null; [ -f \$f ] || { echo "credentials not found in native keychain"; exit 1; }
       python3 -c 'import json,sys; d=json.load(open(sys.argv[1])); print(json.dumps({"ServerURL":d["ServerURL"],"Username":d["Username"],"Secret":d["Secret"]}))' \$f; echo get >> $work/helper-calls ;;
  erase) cat >/dev/null; rm -f \$f; echo erase >> $work/helper-calls ;;
  *) exit 2 ;;
esac
EOF
chmod +x "$work/bin/docker-credential-fake"; export PATH="$work/bin:$PATH"
echo '{"credsStore":"fake"}' > "$DOCKER_CONFIG/config.json"
P=15022; R=localhost:$P/team; start_reg $P bearer
$D tag app $R/app:v1
check "login succeeds and hands the secret to the helper" sh -c "echo secret | $D login -u me --password-stdin localhost:$P"
check "helper got a store call" grep -q store "$work/helper-calls"
check "no plain-text auth.json was written" sh -c "! test -e $RUST_DOCKER_HOME/store/auth.json"
check "push authenticates via the helper" sh -c "$D push $R/app:v1 >/dev/null 2>&1"
check "helper was asked for the credentials" grep -q get "$work/helper-calls"
$D logout localhost:$P >/dev/null 2>&1
check "logout erased them from the helper" grep -q erase "$work/helper-calls"
check "after logout push is refused again" has "$($D push $R/app:v1 2>&1)" "docker login"
stop_reg

echo '{}' > "$DOCKER_CONFIG/config.json"; rm -f "$work/bin/docker-credential-fake"
P=15023; R=localhost:$P/team; start_reg $P basic; $D tag app $R/app:v1
AUTH=$(printf 'me:secret' | base64)
check "without any login a basic registry is refused" has "$($D push $R/app:v1 2>&1)" "docker login"
echo "{\"auths\":{\"localhost:$P\":{\"auth\":\"$AUTH\"}}}" > "$DOCKER_CONFIG/config.json"
check "an existing ~/.docker/config.json base64 login is used" sh -c "$D push $R/app:v1 >/dev/null 2>&1"
check "  ... and the mock saw the push" inlog "PUT /v2/team/app/manifests/v1 -> 201"
check "auth.json still not created" sh -c "! test -e $RUST_DOCKER_HOME/store/auth.json"
stop_reg

echo "== cleanup"
$D rmi $R/app:v1 >/dev/null 2>&1; $D rmi $R/app:idx >/dev/null 2>&1
check "no leftover run dirs" sh -c "! ls /run/rust-docker/* 2>/dev/null | grep -q ."
check "no leftover cgroups" sh -c "! ls /sys/fs/cgroup | grep -q rust-docker"
if [ $fail -eq 0 ]; then echo "ALL PASSED"; else echo "$fail FAILED"; exit 1; fi
