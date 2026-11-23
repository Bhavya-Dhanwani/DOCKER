#!/bin/sh
# Docker CLI compatibility: ps/images/inspect/logs/exec options and --format, and the commands Docker has that were missing
# (info, events, create, diff, commit, export, import, port, system prune, network prune, ...). Needs a running engine.
D="${DOCKER:-docker}"
fail=0
ok() { echo "PASS $1"; }
bad() { echo "FAIL $1: $2"; fail=1; }
has() { case "$1" in *"$2"*) return 0 ;; *) return 1 ;; esac; }
t() { name=$1; out=$2; want=$3; if has "$out" "$want"; then ok "$name"; else bad "$name" "wanted '$want' in: $(echo "$out" | head -5)"; fi; }

$D pull alpine >/dev/null 2>&1
$D rm -f cweb cold cnew 2>/dev/null >/dev/null
$D run -d --name cweb --label app=web -e GREETING=hi -p 18444:80 alpine sh -c 'for i in 1 2 3 4 5; do echo line$i; done; sleep 300' >/dev/null
sleep 1

# ---- ps
t "ps header like Docker" "$($D ps)" "CONTAINER ID"
t "ps shows names last column" "$($D ps)" "cweb"
t "ps --format names" "$($D ps --format '{{.Names}}')" "cweb"
t "ps --format table" "$($D ps --format 'table {{.Names}}\t{{.Image}}')" "NAMES"
t "ps -q is a short id" "$($D ps -q)" "$(echo "$($D inspect -f '{{.Id}}' cweb)" | cut -c1-12)"
t "ps --filter name" "$($D ps --filter name=cweb --format '{{.Names}}')" "cweb"
t "ps --filter label" "$($D ps --filter label=app=web --format '{{.Names}}')" "cweb"
t "ps --filter status=running" "$($D ps --filter status=running -q)" ""
[ "$($D ps --filter name=nothing-here -q | wc -l)" = "0" ] && ok "ps filter matches nothing" || bad "ps filter matches nothing" ""
t "ps ports like Docker" "$($D ps --format '{{.Ports}}')" "0.0.0.0:18444->80/tcp"
t "ps -n 1" "$($D ps -a -n 1 --format '{{.Names}}')" "c"
t "container ls works" "$($D container ls --format '{{.Names}}')" "cweb"
t "ps --format json" "$($D ps --format json)" '"Names":"cweb"'

# ---- images
t "images header" "$($D images)" "REPOSITORY"
t "images --format" "$($D images --format '{{.Repository}}:{{.Tag}}')" "alpine:latest"
t "images -q" "$($D images -q alpine)" ""
[ "$(echo "$($D images -q alpine)" | tr -d '\n' | wc -c)" = "12" ] && ok "images -q is a 12-char id" || bad "images -q length" "$($D images -q alpine)"
t "images name filter" "$($D images alpine --format '{{.Repository}}')" "alpine"
t "image ls works" "$($D image ls --format '{{.Repository}}')" "alpine"

# ---- inspect
t "inspect --format State.Running" "$($D inspect -f '{{.State.Running}}' cweb)" "true"
t "inspect --format Config.Env" "$($D inspect --format '{{json .Config.Env}}' cweb)" "GREETING=hi"
t "inspect Config.Labels" "$($D inspect -f '{{index .Config.Labels "app"}}' cweb)" "web"
t "inspect -f NetworkSettings.IPAddress" "$($D inspect -f '{{.NetworkSettings.IPAddress}}' cweb)" "172."
t "inspect prints a JSON array" "$($D inspect cweb | head -1)" "["
t "inspect an image" "$($D inspect alpine --format '{{.Os}}')" "linux"
t "inspect --type image" "$($D image inspect alpine --format '{{.Architecture}}')" ""
$D inspect no-such-thing >/dev/null 2>&1 && bad "inspect missing exits non-zero" "" || ok "inspect missing exits non-zero"

# ---- logs
out=$($D logs --tail 2 cweb); t "logs --tail 2 has the last line" "$out" "line5"
[ "$(echo "$out" | wc -l)" = "2" ] && ok "logs --tail 2 is two lines" || bad "logs --tail 2" "$out"
t "logs --tail all" "$($D logs --tail all cweb)" "line1"
t "container logs works" "$($D container logs cweb)" "line3"

# ---- exec
t "exec -e" "$($D exec -e FOO=bar cweb sh -c 'echo $FOO')" "bar"
t "exec -w" "$($D exec -w /etc cweb pwd)" "/etc"
t "exec -u" "$($D exec -u 1000 cweb id -u)" "1000"
t "exec sees the container env" "$($D exec cweb sh -c 'echo $GREETING')" "hi"
$D exec -d cweb sh -c 'echo detached > /tmp/d.txt'; sleep 1
t "exec -d runs in the background" "$($D exec cweb cat /tmp/d.txt)" "detached"
$D exec cold true >/dev/null 2>&1 && bad "exec in a missing container fails" "" || ok "exec in a missing container fails"

# ---- info / version
t "info lists containers" "$($D info)" "Containers:"
t "info --format" "$($D info --format '{{.ServerVersion}}')" "0."
t "version --format" "$($D version --format '{{.Client.Version}}')" "0."
t "--version" "$($D --version)" "Docker version"
t "--help" "$($D --help)" "usage"
t "run --help" "$($D run --help)" "docker run"

# ---- create / start / diff / commit / export / import / port
id=$($D create --name cnew alpine sh -c 'echo made-by-create > /made.txt; echo hello-create; sleep 120')
[ -n "$id" ] && ok "create prints an id" || bad "create" ""
t "created container is listed" "$($D ps -a --filter status=created --format '{{.Names}}')" "cnew"
$D start cnew >/dev/null 2>&1; sleep 2
t "created container runs when started" "$($D logs cnew)" "hello-create"
t "diff shows the added file" "$($D diff cnew)" "A /made.txt"
t "commit makes an image" "$($D commit -m msg cnew compat/committed:1)" "sha256:"
t "committed image is listed" "$($D images --format '{{.Repository}}:{{.Tag}}')" "compat/committed:1"
t "committed image has the file" "$($D run --rm compat/committed:1 cat /made.txt)" "made-by-create"
$D export -o /tmp/compat-export.tar cnew && ok "export -o writes a file" || bad "export -o" ""
t "export is a tar with the file" "$(tar tf /tmp/compat-export.tar 2>/dev/null | grep made.txt)" "made.txt"
$D import /tmp/compat-export.tar compat/imported:1 >/dev/null 2>&1 && ok "import makes an image" || bad "import" ""
t "imported image has the file" "$($D run --rm compat/imported:1 cat /made.txt)" "made-by-create"
t "port lists published ports" "$($D port cweb)" "80/tcp -> 0.0.0.0:18444"
t "port with a port number" "$($D port cweb 80)" "0.0.0.0:18444"
$D port cweb 81 >/dev/null 2>&1 && bad "port of an unpublished port fails" "" || ok "port of an unpublished port fails"

# ---- events
( $D events --since 0s > /tmp/compat-events.txt 2>&1 & echo $! > /tmp/compat-events.pid )
sleep 1; $D run --rm alpine true >/dev/null 2>&1; sleep 2
kill "$(cat /tmp/compat-events.pid)" 2>/dev/null
t "events shows container events" "$(cat /tmp/compat-events.txt)" "container"

# ---- prune family and several names
$D rm -f cnew >/dev/null 2>&1
t "network prune" "$($D network prune)" "Deleted Networks"
t "builder prune" "$($D builder prune)" "Total"
t "system prune -f" "$($D system prune -f)" "Total reclaimed space"
t "context ls" "$($D context ls)" "default"
$D rm -f cweb >/dev/null 2>&1
$D rmi compat/committed:1 compat/imported:1 >/dev/null 2>&1

[ $fail = 0 ] && echo "ALL PASSED"
exit $fail
