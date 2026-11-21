#!/bin/sh
# Dockerfile completeness (Linux, root, offline: alpine from the store; URL ADD and push use local python3 servers):
# ADD url/tar, COPY --chmod, heredocs, RUN --mount, SHELL, ONBUILD, VOLUME, STOPSIGNAL, LABEL, ARG/FROM, .dockerignore, push.
#   scripts/e2e-build3.sh <docker-bin> <store-dir-with-alpine>
D=${1:?docker binary}; SRC=${2:?store dir}
HERE=$(cd "$(dirname "$0")" && pwd)
# run a local copy: executing straight from a Windows-mounted drive is flaky under WSL
cp "$D" /root/dk.$$ && chmod +x /root/dk.$$ && D=/root/dk.$$
export RUST_DOCKER_HOME=$(mktemp -d /root/e2e-b3.XXXXXX)
ctx=$(mktemp -d); srv=$(mktemp -d); log=$(mktemp)
trap 'kill $HTTP $REG 2>/dev/null; rm -f /root/dk.$$ "$log"; rm -rf "$ctx" "$srv" "$RUST_DOCKER_HOME"' EXIT
for cg in /sys/fs/cgroup/rust-docker-*; do [ -d "$cg" ] && echo 1 > "$cg/cgroup.kill"; done; sleep 1
for cg in /sys/fs/cgroup/rust-docker-*; do [ -d "$cg" ] && rmdir "$cg" 2>/dev/null; done
for l in $(ls /sys/class/net | grep '^rdv'); do ip link del "$l" 2>/dev/null; done
mkdir -p "$RUST_DOCKER_HOME" && cp -r "$SRC" "$RUST_DOCKER_HOME/store"
fail=0
check() { d=$1; shift; if "$@" >/dev/null 2>&1; then echo "PASS $d"; else echo "FAIL $d"; fail=$((fail+1)); fi; }
has() { case "$1" in *"$2"*) return 0 ;; *) return 1 ;; esac; }
eq() { [ "$1" = "$2" ]; }
N="--network none"
run() { img=$1; shift; $D run $N "$img" "$@" </dev/null; }
fresh() { rm -rf "$ctx"; mkdir -p "$ctx"; }
build() { $D build "$@" 2>&1; }

PORT=$((20000 + $$ % 20000))
echo "hello from http" > "$srv/hello.txt"
mkdir -p "$srv/pk/sub"; echo packed > "$srv/pk/sub/p.txt"
(cd "$srv/pk" && tar czf ../remote.tgz .)
python3 -m http.server $PORT --bind 127.0.0.1 --directory "$srv" >/dev/null 2>&1 & HTTP=$!
sleep 1

echo "== ADD: URL and local archives"
fresh
mkdir -p "$ctx/t/in"; echo inner > "$ctx/t/in/f.txt"; echo top > "$ctx/t/top.txt"
(cd "$ctx/t" && tar cf ../a.tar . && tar czf ../b.tgz . && tar cjf ../c.tar.bz2 . 2>/dev/null; tar cJf ../d.tar.xz . 2>/dev/null)
rm -rf "$ctx/t"; echo plain > "$ctx/plain.txt"; gzip -c "$ctx/plain.txt" > "$ctx/plain.txt.gz"
cat > "$ctx/Dockerfile" <<'EOF'
FROM alpine
ADD http://127.0.0.1:@PORT@/hello.txt /dl/
ADD http://127.0.0.1:@PORT@/hello.txt /renamed.txt
ADD http://127.0.0.1:@PORT@/remote.tgz /still-a-tgz/
ADD a.tar /x-tar/
ADD b.tgz /x-tgz/
ADD plain.txt plain.txt.gz /files/
EOF
sed -i "s/@PORT@/$PORT/" "$ctx/Dockerfile"
[ -f "$ctx/c.tar.bz2" ] && echo "ADD c.tar.bz2 /x-bz2/" >> "$ctx/Dockerfile"
[ -f "$ctx/d.tar.xz" ] && echo "ADD d.tar.xz /x-xz/" >> "$ctx/Dockerfile"
out=$(build -t add1 "$ctx"); rc=$?
check "ADD build succeeds" test $rc -eq 0
check "ADD <url> /dir/ names the file after the URL" eq "$(run add1 cat /dl/hello.txt)" "hello from http"
check "ADD <url> /file saves under that name" eq "$(run add1 cat /renamed.txt)" "hello from http"
check "a downloaded tarball is NOT extracted" eq "$(run add1 ls /still-a-tgz)" remote.tgz
check "ADD a.tar extracts it" eq "$(run add1 cat /x-tar/in/f.txt)" inner
check "ADD b.tgz extracts it (gzip)" eq "$(run add1 cat /x-tgz/top.txt)" top
[ -f "$ctx/c.tar.bz2" ] && check "ADD .tar.bz2 extracts it" eq "$(run add1 cat /x-bz2/in/f.txt)" inner
[ -f "$ctx/d.tar.xz" ] && check "ADD .tar.xz extracts it" eq "$(run add1 cat /x-xz/in/f.txt)" inner
check "ADD of ordinary files (even .gz that is no tar) copies them" eq "$(run add1 ls /files | tr '\n' ' ')" "plain.txt plain.txt.gz "
check "unchanged rebuild reuses the cache" has "$(build -t add1 "$ctx")" "Using cache"
echo "hello v2" > "$srv/hello.txt"
$D build -t add1 "$ctx" >/dev/null 2>&1
check "a changed download busts the cache" eq "$(run add1 cat /dl/hello.txt)" "hello v2"
printf 'FROM alpine\nADD http://127.0.0.1:%s/nope /x\n' $PORT > "$ctx/Dockerfile"
check "a 404 download fails the build" sh -c "! $D build -t nope '$ctx' >/dev/null 2>&1"

echo "== COPY --chmod / --chown, ADD --chown"
fresh; echo data > "$ctx/f"; mkdir "$ctx/d"; echo x > "$ctx/d/g"
cat > "$ctx/Dockerfile" <<'EOF'
FROM alpine
COPY --chmod=600 f /m600
COPY --chmod=755 d /m755/
ADD --chown=1001:1002 --chmod=0640 f /owned
EOF
$D build -t cm "$ctx" >/dev/null 2>&1; check "chmod build succeeds" test $? -eq 0
check "COPY --chmod=600 on a file" eq "$(run cm stat -c %a /m600)" 600
check "COPY --chmod=755 applies to the directory and files in it" eq "$(run cm stat -c '%a %a' /m755 /m755/g | tr '\n' ' ')" "755 755 "
check "ADD --chown --chmod" eq "$(run cm stat -c '%a %u:%g' /owned)" "640 1001:1002"

echo "== heredocs"
fresh
cat > "$ctx/Dockerfile" <<'DFEND'
FROM alpine
ARG WHO=world
RUN <<EOF
set -e
echo "line one" > /script-out
echo "line two" >> /script-out
EOF
RUN cat <<-'TXT' > /plain
	keep $WHO literally
TXT
COPY <<EOF /etc/greeting
hello $WHO
EOF
COPY <<'EOF' /raw.txt
no $expansion here
EOF
COPY <<EOF /dir/
named after the delimiter
EOF
DFEND
out=$(build -t hd "$ctx"); rc=$?
check "heredoc build succeeds" test $rc -eq 0
check "RUN <<EOF runs the body as a script" eq "$(run hd cat /script-out | tr '\n' ',')" "line one,line two,"
check "RUN cat <<-'X' keeps the text (tabs stripped, no expansion)" eq "$(run hd cat /plain)" 'keep $WHO literally'
check "COPY <<EOF expands ARGs" eq "$(run hd cat /etc/greeting)" "hello world"
check "COPY <<'EOF' does not expand" eq "$(run hd cat /raw.txt)" 'no $expansion here'
check "COPY <<EOF /dir/ names the file after the delimiter" eq "$(run hd cat /dir/EOF)" "named after the delimiter"

echo "== RUN --mount"
fresh; mkdir "$ctx/data"; echo from-bind > "$ctx/data/f"; echo s3cret > "$ctx/tok.txt"
cat > "$ctx/Dockerfile" <<'EOF'
FROM alpine
ARG BUST=1
RUN --mount=type=cache,target=/cache,id=c1 sh -c 'echo $BUST >> /cache/log; cat /cache/log > /seen'
RUN --mount=type=tmpfs,target=/scratch sh -c 'echo x > /scratch/f && cp /scratch/f /from-tmpfs'
RUN --mount=type=bind,source=data,target=/src cp /src/f /copied
RUN --mount=type=bind,source=data,target=/rw,rw sh -c 'echo scribble > /rw/new && ls /rw > /rw-listing'
RUN --mount=type=secret,id=tok test "$(cat /run/secrets/tok)" = s3cret && echo secret-was-readable > /secret-ok
RUN --mount=type=secret,id=other,target=/o,required=false sh -c 'test ! -e /o && echo optional-ok > /optional'
EOF
out=$(build -t mnt --secret id=tok,src="$ctx/tok.txt" "$ctx"); rc=$?
check "--mount build succeeds" test $rc -eq 0
$D build -t mnt --build-arg BUST=2 --secret id=tok,src="$ctx/tok.txt" "$ctx" >/dev/null 2>&1
check "a cache mount survives between builds" eq "$(run mnt cat /seen | tr '\n' ',')" "1,2,"
check "cache contents are not in the image" sh -c "! $D run $N mnt ls /cache/log </dev/null >/dev/null 2>&1"
check "tmpfs mount worked and left nothing behind" sh -c "[ \"\$($D run $N mnt cat /from-tmpfs </dev/null)\" = x ] && ! $D run $N mnt ls /scratch/f </dev/null >/dev/null 2>&1"
check "bind mount from the context" eq "$(run mnt cat /copied)" from-bind
check "the bind source is not left in the image" sh -c "! $D run $N mnt ls /src/f </dev/null >/dev/null 2>&1"
check "rw bind writes do not reach the build context" sh -c "[ ! -e '$ctx/data/new' ]"
check "secret was readable during the RUN" eq "$(run mnt cat /secret-ok)" secret-was-readable
check "secret file is not in the image" sh -c "! $D run $N mnt ls /run/secrets/tok </dev/null >/dev/null 2>&1"
check "secret content is nowhere in the layer store" sh -c "! grep -rq s3cret '$RUST_DOCKER_HOME/store/layers'"
check "optional missing secret is fine" eq "$(run mnt cat /optional)" optional-ok
printf 'FROM alpine\nRUN --mount=type=secret,id=need,required=true true\n' > "$ctx/Dockerfile"
check "a required secret that was not given fails the build" has "$(build -t mnt2 "$ctx")" "secret"

echo "== SHELL"
fresh
cat > "$ctx/Dockerfile" <<'EOF'
FROM alpine
SHELL ["/bin/echo", "ran:"]
RUN this is not executed as a command
CMD all good
EOF
$D build -t sh1 "$ctx" >/dev/null 2>&1; check "SHELL build succeeds" test $? -eq 0
check "SHELL replaces /bin/sh -c for shell-form CMD" eq "$(run sh1)" "ran: all good"

echo "== ONBUILD"
fresh; echo marker > "$ctx/marker"
cat > "$ctx/Dockerfile" <<'EOF'
FROM alpine
ONBUILD COPY marker /m
ONBUILD RUN echo triggered > /trig
EOF
$D build -t obase "$ctx" >/dev/null 2>&1
check "the trigger did not run in the base build" sh -c "! $D run $N obase ls /trig </dev/null >/dev/null 2>&1"
check "triggers are stored in the image metadata" sh -c "grep -q 'ONBUILD\\|COPY marker' '$RUST_DOCKER_HOME'/store/images/obase*.json || grep -q 'RUN echo triggered' '$RUST_DOCKER_HOME'/store/images/obase*.json"
fresh; echo child-marker > "$ctx/marker"; printf 'FROM obase\nRUN cat /trig > /seen-trig\n' > "$ctx/Dockerfile"
out=$(build -t ochild "$ctx"); rc=$?
check "child build runs the triggers" has "$out" "build trigger"
check "ONBUILD COPY used the child's context" eq "$(run ochild cat /m)" child-marker
check "ONBUILD RUN ran before the child's own steps" eq "$(run ochild cat /seen-trig)" triggered
printf 'FROM ochild\nRUN true\n' > "$ctx/Dockerfile"
out=$(build -t ogrand "$ctx")
check "triggers are not inherited by grandchildren" sh -c "! echo '$out' | grep -q 'build trigger'"

echo "== VOLUME and STOPSIGNAL"
fresh
cat > "$ctx/Dockerfile" <<'EOF'
FROM alpine
RUN mkdir /data && echo seed > /data/seed
VOLUME /data
VOLUME ["/other", "/third"]
STOPSIGNAL SIGUSR1
CMD ["sh", "-c", "trap 'echo got-usr1; exit 0' USR1; echo ready; while :; do sleep 0.2; done"]
EOF
$D build -t vol "$ctx" >/dev/null 2>&1; check "VOLUME/STOPSIGNAL build succeeds" test $? -eq 0
img=$RUST_DOCKER_HOME/store/images/vol_latest.json
check "VOLUME and STOPSIGNAL are in the image metadata" sh -c "grep -q '/data' $img && grep -q '/third' $img && grep -q SIGUSR1 $img"
check "the anonymous volume is seeded with the image's content" eq "$(run vol cat /data/seed)" seed
check "VOLUME paths exist and are writable" eq "$(run vol sh -c 'echo w > /other/f && cat /other/f')" w
before=$($D volume ls 2>/dev/null | wc -l)
run vol true
check "each run creates an anonymous volume" test "$($D volume ls 2>/dev/null | wc -l)" -gt "$before"
mkdir -p "$ctx/hv"; echo host > "$ctx/hv/seed"
check "-v on the same path wins over the anonymous volume" eq "$($D run $N -v "$ctx/hv:/data" vol cat /data/seed </dev/null)" host
id=$($D run -d $N --name stopper vol </dev/null)
sleep 1
$D stop -t 5 stopper >/dev/null 2>&1
check "docker stop sends the image's STOPSIGNAL" has "$($D logs stopper 2>&1)" got-usr1
$D rm stopper >/dev/null 2>&1

echo "== LABEL, ARG before FROM, FROM --platform, build flags"
fresh
cat > "$ctx/Dockerfile" <<'EOF'
ARG BASE=alpine
ARG TAG=latest
FROM --platform=$BUILDPLATFORM ${BASE}:${TAG} AS base
ARG TARGETARCH
ARG BASE
LABEL a=1 "b c"="d e"
LABEL legacy this is a value
RUN echo "$TARGETARCH:$BASE" > /args
EOF
out=$(build -t lbl --label extra=yes --label a=override "$ctx"); rc=$?
check "ARG before FROM / in FROM / FROM --platform build" test $rc -eq 0
check "global ARG visible in the stage once redeclared, TARGETARCH predefined" sh -c "[ \"\$($D run $N lbl cat /args </dev/null)\" = \"$(uname -m | sed 's/aarch64/arm64/;s/x86_64/amd64/'):alpine\" ]"
j=$RUST_DOCKER_HOME/store/images/lbl_latest.json
check "LABEL key=value, quoted key, legacy form" sh -c "grep -q '\"b c\": \"d e\"' $j && grep -q 'this is a value' $j"
check "--label adds and overrides" sh -c "grep -q '\"extra\": \"yes\"' $j && grep -q '\"a\": \"override\"' $j"
check "--platform for another arch is refused" has "$(build -t plat --platform linux/s390x "$ctx")" "platform"
check "--platform for the host is accepted" sh -c "$D build -t plat --platform linux/$(uname -m | sed 's/aarch64/arm64/;s/x86_64/amd64/') '$ctx' >/dev/null 2>&1"
q=$($D build -q -t quiet "$ctx" 2>&1)
check "-q prints one line, an id" sh -c "[ \$(printf '%s\n' '$q' | wc -l) -eq 1 ] && printf '%s' '$q' | grep -q sha256:"

echo "== .dockerignore"
fresh; mkdir -p "$ctx/a/b" "$ctx/keepdir" "$ctx/docs"
for f in a/b/x.tmp a/y.tmp keepdir/k.txt docs/one.md docs/two.md top.md fileA.txt fileB.txt; do echo c > "$ctx/$f"; done
printf '**/*.tmp\n!a/b/x.tmp\n*.md\n!docs/*.md\nfile[A].txt\ndocs/tw?.md\n' > "$ctx/.dockerignore"
printf 'FROM alpine\nCOPY . /c/\nCMD ["find", "/c", "-type", "f"]\n' > "$ctx/Dockerfile"
$D build -t ign "$ctx" >/dev/null 2>&1
l=$(run ign)
check "** + ! : nested .tmp dropped" sh -c "! echo '$l' | grep -q 'a/y.tmp'"
check "! re-includes a file under a ** rule" has "$l" "a/b/x.tmp"
check "*.md at the root dropped, !docs/*.md re-included" sh -c "! echo '$l' | grep -q 'c/top.md' && echo '$l' | grep -q 'docs/one.md'"
check "[A] class and ? wildcard, later rules win" sh -c "! echo '$l' | grep -q 'fileA.txt' && echo '$l' | grep -q 'fileB.txt' && ! echo '$l' | grep -q 'two.md'"
printf 'fileB.txt\n' > "$ctx/Dockerfile.dockerignore"
$D build -t ign2 "$ctx" >/dev/null 2>&1
l=$(run ign2)
check "<Dockerfile>.dockerignore takes precedence" sh -c "! echo '$l' | grep -q 'fileB.txt' && echo '$l' | grep -q 'fileA.txt'"

echo "== a built image can be pushed and pulled back with its metadata"
python3 "$HERE/mock-registry.py" $((PORT + 1)) none 2>"$log" & REG=$!
sleep 1
repo=localhost:$((PORT + 1))/team/vol
$D tag vol $repo:v1 >/dev/null 2>&1
out=$($D push $repo:v1 2>&1); rc=$?
check "push of a built image succeeds" test $rc -eq 0
check "the registry got the manifest" sh -c "grep -q 'PUT /v2/team/vol/manifests/v1 -> 201' '$log'"
$D rmi $repo:v1 >/dev/null 2>&1; $D rmi vol >/dev/null 2>&1; $D prune >/dev/null 2>&1
$D pull $repo:v1 >/dev/null 2>&1; check "pull back succeeds" test $? -eq 0
pj=$(ls "$RUST_DOCKER_HOME"/store/images/*team*vol*.json 2>/dev/null | head -1)
check "pulled image keeps VOLUME, STOPSIGNAL" sh -c "grep -q '/third' '$pj' && grep -q SIGUSR1 '$pj'"
check "pulled image still runs, volume seeded" eq "$($D run $N $repo:v1 cat /data/seed </dev/null)" seed

echo "== cleanup"
for t in add1 cm hd mnt sh1 obase ochild ogrand lbl plat quiet ign ign2 $repo:v1; do $D rmi $t >/dev/null 2>&1; done
check "no leftover run dirs" sh -c "! ls /run/rust-docker/* 2>/dev/null | grep -q ."
check "no leftover cgroups" sh -c "! ls /sys/fs/cgroup | grep -q rust-docker"
if [ $fail -eq 0 ]; then echo "ALL PASSED"; else echo "$fail FAILED"; exit 1; fi
