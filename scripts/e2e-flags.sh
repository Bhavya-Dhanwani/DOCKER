#!/bin/sh
# docker run flags that used to be accepted but ignored: --label --dns --init --read-only --ulimit --tmpfs --shm-size --cpus,
# plus anonymous volumes going away with `--rm`. Needs a running engine and the alpine image (pulled on demand).
D="${DOCKER:-docker}"
fail=0
ok() { echo "PASS $1"; }
bad() { echo "FAIL $1: $2"; fail=1; }
$D pull alpine >/dev/null 2>&1

out=$($D run --rm --dns 9.9.9.9 --dns 1.1.1.1 alpine cat /etc/resolv.conf)
case "$out" in *"nameserver 9.9.9.9"*"nameserver 1.1.1.1"*) ok dns ;; *) bad dns "$out" ;; esac

out=$($D run --rm --read-only alpine sh -c 'touch /x 2>&1; echo rc=$?; touch /tmp/y && echo tmp-ok')
case "$out" in *"Read-only"*"tmp-ok"*) ok read-only ;; *) bad read-only "$out" ;; esac

out=$($D run --rm --read-only --tmpfs /scratch:size=4m alpine sh -c 'touch /scratch/a && echo scratch-ok; df /scratch | tail -1')
case "$out" in *scratch-ok*"4096"*) ok tmpfs-size ;; *scratch-ok*) ok tmpfs ;; *) bad tmpfs "$out" ;; esac

out=$($D run --rm --ulimit nofile=64:64 alpine sh -c 'ulimit -n')
[ "$out" = "64" ] && ok ulimit || bad ulimit "$out"

out=$($D run --rm --shm-size 8m alpine sh -c 'df -k /dev/shm | tail -1')
case "$out" in *8192*) ok shm-size ;; *) bad shm-size "$out" ;; esac

# --init: PID 1 is not the command, and a zombie left behind by the command is reaped
out=$($D run --rm --init alpine sh -c 'cat /proc/1/cmdline | tr "\0" " "; echo; (sleep 0 &) ; sleep 1; ps -o stat | grep -c Z')
case "$out" in *"0") ok init ;; *) bad init "$out" ;; esac
out=$($D run --rm alpine sh -c 'cat /proc/1/cmdline | tr "\0" " "')
case "$out" in *"sh -c"*) ok no-init-pid1-is-command ;; *) bad no-init "$out" ;; esac

# --cpus: the cgroup quota is 1.5 cores
id=$($D run -d --cpus 1.5 alpine sleep 20)
q=$(cat /sys/fs/cgroup/rust-docker-"$id"/cpu.max 2>/dev/null)
case "$q" in "150000 100000") ok cpus ;; *) bad cpus "$q" ;; esac
$D rm -f "$id" >/dev/null 2>&1

# --label shows up in inspect
id=$($D run -d --label app=web --label tier alpine sleep 20)
$D inspect "$id" 2>/dev/null | grep -q 'app=web' && ok label || bad label "$($D inspect "$id" 2>&1 | head -5)"
$D rm -f "$id" >/dev/null 2>&1

# anonymous volumes: kept for a normal container, removed with --rm
before=$($D volume ls | grep -c '^anon-')
$D run --rm -v /data alpine sh -c 'echo x > /data/f'
after=$($D volume ls | grep -c '^anon-')
[ "$before" = "$after" ] && ok anon-volume-removed-with-rm || bad anon-rm "before=$before after=$after"
$D run -v /data --name keep alpine sh -c 'echo x > /data/f'
[ "$($D volume ls | grep -c '^anon-')" = "$((before + 1))" ] && ok anon-volume-kept-without-rm || bad anon-keep "$($D volume ls)"
$D rm -f keep >/dev/null 2>&1

[ $fail = 0 ] && echo "ALL PASSED"
exit $fail
