#!/bin/sh
# Kernel/userland probe for rootless mode. Run as root; probes as user "rl" (created if missing).
id rl >/dev/null 2>&1 || useradd -m rl
uname -r
for f in kernel/unprivileged_userns_clone user/max_user_namespaces kernel/apparmor_restrict_unprivileged_userns; do
  printf '%s=' "$f"; cat /proc/sys/$f 2>/dev/null || echo n/a
done
for t in newuidmap newgidmap slirp4netns pasta fuse-overlayfs; do printf '%s: ' "$t"; command -v $t || echo missing; done
grep rl /etc/subuid /etc/subgid
ls /sys/fs/cgroup/user.slice 2>&1 | head -3
setpriv --reuid=rl --regid=rl --init-groups sh -c '
id
unshare -U -r id
d=$(mktemp -d)
mkdir -p $d/l $d/u $d/w $d/m
unshare -Urm sh -c "mount -t overlay overlay -o lowerdir=$d/l,upperdir=$d/u,workdir=$d/w,userxattr $d/m && echo overlay-userxattr-ok"
unshare -Urm sh -c "mount -t tmpfs tmpfs $d/m && mkdir $d/m/a $d/m/b && mount -t overlay overlay -o lowerdir=$d/l,upperdir=$d/m/a,workdir=$d/m/b,userxattr $d/l 2>&1 && echo overlay-tmpfs-upper-ok"
unshare -Urm sh -c "mknod $d/n c 1 3 2>&1; mknod $d/wh c 0 0 && echo whiteout-mknod-ok"
'
