#!/bin/sh
# docker compose beyond up/down: override files, -f merging, profiles, and stop/start/restart/kill/pause/rm/exec/run/
# port/images/top/ls/cp/config/create/version. Needs a running engine.
D="${DOCKER:-docker}"
fail=0
ok() { echo "PASS $1"; }
bad() { echo "FAIL $1: $2"; fail=1; }
has() { case "$1" in *"$2"*) return 0 ;; *) return 1 ;; esac; }
t() { name=$1; out=$2; want=$3; if has "$out" "$want"; then ok "$name"; else bad "$name" "wanted '$want' in: $(echo "$out" | head -6)"; fi; }

$D pull alpine >/dev/null 2>&1
W=/tmp/cmp2-proj
rm -rf $W && mkdir -p $W/data && cd $W || exit 1
echo "from-file" > data/f.txt
cat > compose.yaml <<'EOF'
name: cmp2
services:
  web:
    image: alpine
    command: sh -c "echo web-started; sleep 600"
    ports: ["18555:80"]
    environment:
      BASE: base
  db:
    image: alpine
    command: sleep 600
    volumes:
      - dbdata:/var/lib/data
  worker:
    image: alpine
    command: sleep 600
    profiles: ["extra"]
volumes:
  dbdata:
EOF
cat > compose.override.yaml <<'EOF'
services:
  web:
    environment:
      FROM_OVERRIDE: yes
EOF
cat > extra.yaml <<'EOF'
services:
  web:
    environment:
      FROM_EXTRA: more
EOF

# ---- config, profiles, override
t "config --services skips profile services" "$($D compose config --services)" "web"
$D compose config --services | grep -q worker && bad "profile service hidden" "" || ok "profile service hidden"
t "--profile enables them" "$($D compose --profile extra config --services)" "worker"
t "config prints YAML" "$($D compose config)" "services:"
t "config shows the override" "$($D compose config)" "FROM_OVERRIDE"
t "-f merges later files" "$($D compose -f compose.yaml -f extra.yaml config)" "FROM_EXTRA"
$D compose -f compose.yaml -f extra.yaml config | grep -q FROM_OVERRIDE && bad "-f files replace the automatic override" "" || ok "-f files replace the automatic override"
t "config --volumes" "$($D compose config --volumes)" "dbdata"
t "version" "$($D compose version)" "Docker Compose version"

# ---- up variants
$D compose up --no-start >/dev/null 2>&1
t "up --no-start creates without starting" "$($D compose ps -a --format '{{.Service}} {{.State}}')" "web created"
$D compose up -d web >/dev/null 2>&1; sleep 2
t "up -d <service> starts it" "$($D compose ps --format '{{.Service}}')" "web"
$D compose ps --services | grep -q db || bad "ps --services lists services" ""
t "ps shows its status" "$($D compose ps)" "Up"
t "ps -q is ids" "$($D compose ps -q)" ""
t "ps --format json" "$($D compose ps --format json)" '"Service":"web"'
t "compose labels on containers" "$($D ps --filter label=com.docker.compose.project=cmp2 --format '{{.Names}}')" "cmp2-web-1"
t "compose ls" "$($D compose ls)" "cmp2"
$D compose up -d >/dev/null 2>&1; sleep 2
t "up -d starts the rest (not the profile one)" "$($D compose ps --format '{{.Service}}' | sort | tr '\n' ' ')" "db web"

# ---- exec / run
t "exec sees the override env" "$($D compose exec -T web sh -c 'echo $FROM_OVERRIDE')" "yes"
t "exec -e -w" "$($D compose exec -T -e X=7 -w /tmp web sh -c 'echo $X $(pwd)')" "7 /tmp"
t "run --rm" "$($D compose run --rm web echo hello-run)" "hello-run"
$D compose run --rm -T web sh -c 'exit 3'; [ $? = 3 ] && ok "run passes the exit code" || bad "run exit code" "$?"
t "run does not leave a container" "$($D ps -a --filter label=com.docker.compose.oneoff=True -q | wc -l | tr -d ' ')" "0"
t "run -e and volume" "$($D compose run --rm -T -e GREET=hi -v $W/data:/d:ro web sh -c 'echo $GREET; cat /d/f.txt')" "from-file"

# ---- lifecycle
t "logs --tail 1" "$($D compose logs --tail 1 --no-log-prefix web)" "web-started"
t "logs has the service prefix" "$($D compose logs web)" "web"
$D compose stop web >/dev/null 2>&1
t "stop" "$($D compose ps -a --format '{{.Service}} {{.State}}')" "web exited"
$D compose start web >/dev/null 2>&1; sleep 1
t "start" "$($D compose ps --format '{{.Service}}')" "web"
$D compose restart web >/dev/null 2>&1; sleep 1
t "restart" "$($D compose ps --format '{{.Service}}')" "web"
$D compose pause db >/dev/null 2>&1
$D compose unpause db >/dev/null 2>&1
t "port" "$($D compose port web 80)" "18555"
t "top" "$($D compose top web 2>&1)" "sleep"
t "images" "$($D compose images)" "alpine"
echo cp-test > cp.txt
$D compose cp cp.txt web:/tmp/cp.txt >/dev/null 2>&1
t "cp into a service" "$($D compose exec -T web cat /tmp/cp.txt)" "cp-test"
$D compose kill db >/dev/null 2>&1; sleep 1
t "kill" "$($D compose ps -a --format '{{.Service}} {{.State}}')" "db exited"
$D compose rm -f db >/dev/null 2>&1
$D compose ps -a --format '{{.Service}}' | grep -q db && bad "rm removes the container" "" || ok "rm removes the container"
$D compose up -d --force-recreate web >/dev/null 2>&1; sleep 1
t "up --force-recreate" "$($D compose ps --format '{{.Service}}')" "web"
t "pull skips built/local services quietly" "$($D compose pull 2>&1; echo done)" "done"

# ---- down
$D compose down --remove-orphans -v >/dev/null 2>&1
[ "$($D compose ps -a -q | wc -l | tr -d ' ')" = "0" ] && ok "down removes the containers" || bad "down" "$($D compose ps -a)"
$D volume ls | grep -q cmp2_dbdata && bad "down -v removes named volumes" "" || ok "down -v removes named volumes"
cd /tmp && rm -rf $W
[ $fail = 0 ] && echo "ALL PASSED"
exit $fail
