#!/bin/sh
# Capabilities / seccomp / no-new-privileges end-to-end test (Linux, root, offline: alpine from the store):
#   scripts/e2e-security.sh <docker-bin> <store-dir-with-alpine>
D=${1:?docker binary}; SRC=${2:?store dir}
# run a local copy: executing straight from a Windows-mounted drive is flaky under WSL
cp "$D" /root/dk.$$ && chmod +x /root/dk.$$ && D=/root/dk.$$
export RUST_DOCKER_HOME=$(mktemp -d /root/e2e-sec.XXXXXX)
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
field() { f=$1; shift; "$@" grep "^$f:" /proc/self/status </dev/null 2>/dev/null | tr -d '\t ' | cut -d: -f2; }

echo "== capabilities"
eq_cap() { eq "$(field CapEff $D run --network none alpine)" "$1"; }
check "default CapEff is Docker's (a80425fb)" eq_cap 00000000a80425fb
check "default bounding set equals it" eq "$(field CapBnd $D run --network none alpine)" 00000000a80425fb
check "--cap-drop ALL leaves none" eq "$(field CapEff $D run --network none --cap-drop ALL alpine)" 0000000000000000
check "--cap-add NET_ADMIN adds bit 12" eq "$(field CapEff $D run --network none --cap-add NET_ADMIN alpine)" 00000000a80435fb
check "--cap-drop ALL --cap-add chown = only chown" eq "$(field CapEff $D run --network none --cap-drop ALL --cap-add CAP_CHOWN alpine)" 0000000000000001
check "--privileged grants everything" has "$(field CapEff $D run --network none --privileged alpine)" "ffffffff"
check "unknown capability is rejected" sh -c "! $D run --cap-add nonsense alpine true </dev/null"

echo "== capabilities have real effects"
check "chown works by default" $D run --network none alpine sh -c 'touch /tmp/x && chown 5 /tmp/x' </dev/null
check "chown fails without CAP_CHOWN" sh -c "! $D run --network none --cap-drop CHOWN alpine sh -c 'touch /tmp/x && chown 5 /tmp/x' </dev/null"
check "ping works by default (NET_RAW)" $D run alpine ping -c1 -W2 172.30.0.1 </dev/null
check "ping fails without NET_RAW" sh -c "! $D run --cap-drop NET_RAW alpine ping -c1 -W2 172.30.0.1 </dev/null"
check "mount is refused by default" sh -c "! $D run --network none alpine mount -t tmpfs none /mnt </dev/null"
check "mount works with --privileged" $D run --network none --privileged alpine mount -t tmpfs none /mnt </dev/null

echo "== seccomp"
check "seccomp filter is active (Seccomp: 2)" eq "$(field Seccomp $D run --network none alpine)" 2
check "--security-opt seccomp=unconfined disables it" eq "$(field Seccomp $D run --network none --security-opt seccomp=unconfined alpine)" 0
if $D run --network none --security-opt seccomp=unconfined alpine unshare -U true </dev/null >/dev/null 2>&1; then
  check "unshare is blocked by the filter" sh -c "! $D run --network none alpine unshare -U true </dev/null"
  check "unshare works when unconfined" $D run --network none --security-opt seccomp=unconfined alpine unshare -U true </dev/null
else
  echo "SKIP unshare effect: this kernel/busybox cannot unshare a user namespace even unconfined"
fi
check "unsupported --security-opt is rejected" sh -c "! $D run --security-opt apparmor=x alpine true </dev/null"

echo "== no-new-privileges"
check "default: NoNewPrivs 0" eq "$(field NoNewPrivs $D run --network none alpine)" 0
check "--security-opt no-new-privileges: NoNewPrivs 1" eq "$(field NoNewPrivs $D run --network none --security-opt no-new-privileges alpine)" 1

echo "== USER and exec"
cat > "$ctx/Dockerfile" <<'EOF'
FROM alpine
RUN adduser -D appuser
USER appuser
CMD ["sleep", "60"]
EOF
$D build -t nonroot "$ctx" >/dev/null 2>&1; check "build (RUN steps use the default caps)" test $? -eq 0
nr=$($D run --network none nonroot sh -c "grep '^CapEff:' /proc/self/status | cut -f2" </dev/null)
check "non-root CapEff is 0" eq "$nr" 0000000000000000
$D run -d --network none --name sec --cap-drop NET_RAW --security-opt no-new-privileges alpine sleep 60 </dev/null >/dev/null; sleep 1
ec=$($D exec sec sh -c "grep '^CapEff:' /proc/self/status | cut -f2")
check "exec has the same caps (NET_RAW dropped: a80405fb)" eq "$ec" 00000000a80405fb
check "exec has the seccomp filter" eq "$($D exec sec sh -c "grep '^Seccomp:' /proc/self/status | cut -f2")" 2
check "exec inherits no-new-privileges" eq "$($D exec sec sh -c "grep '^NoNewPrivs:' /proc/self/status | cut -f2")" 1
$D rm -f sec >/dev/null 2>&1
$D run -d --network none --name nr nonroot </dev/null >/dev/null; sleep 1
check "exec runs as the container's USER" eq "$($D exec nr id -u)" 1000
$D rm -f nr >/dev/null 2>&1

echo "== cleanup"
check "no leftover run dirs" sh -c "! ls /run/rust-docker/* 2>/dev/null | grep -q ."
check "no leftover cgroups" sh -c "! ls /sys/fs/cgroup | grep -q rust-docker"
if [ $fail -eq 0 ]; then echo "ALL PASSED"; else echo "$fail FAILED"; exit 1; fi
