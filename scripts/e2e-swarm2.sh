#!/bin/sh
# Single-node swarm extras: node, secret, config, service rollback, drain, --secret/--config/--user/--limit-memory, ls --format.
D=docker
fail=0
ok() { echo "PASS $1"; }
bad() { echo "FAIL $1: $2"; fail=1; }
has() { case "$1" in *"$2"*) return 0 ;; *) return 1 ;; esac; }
t() { if has "$2" "$3"; then ok "$1"; else bad "$1" "wanted '$3' in: $(echo "$2" | head -6)"; fi; }
$D pull alpine >/dev/null 2>&1
$D swarm leave -f >/dev/null 2>&1
$D swarm init >/dev/null 2>&1
t "node ls" "$($D node ls)" "Leader"
t "node ls --format" "$($D node ls --format '{{.Status}}')" "Ready"
t "node inspect self" "$($D node inspect self --format '{{.Spec.Role}}')" "manager"
echo "s3cret-value" | $D secret create db_pass - >/dev/null 2>&1
echo "cfg-value" > /tmp/sw2.cfg; $D config create app_cfg /tmp/sw2.cfg >/dev/null 2>&1
t "secret ls" "$($D secret ls)" "db_pass"
t "config ls" "$($D config ls)" "app_cfg"
t "secret inspect" "$($D secret inspect db_pass --format '{{.Spec.Name}}')" "db_pass"
$D service create --name swsvc --secret db_pass --config source=app_cfg,target=/etc/app.cfg -l tier=web -u 1000 --limit-memory 64m alpine sh -c 'cat /run/secrets/db_pass; cat /etc/app.cfg; id -u; sleep 300' >/dev/null 2>&1
sleep 5
cid=$($D ps -q --filter name=swsvc | head -1)
t "secret is mounted in the replica" "$($D logs $cid 2>&1)" "s3cret-value"
t "config is mounted at its target" "$($D logs $cid 2>&1)" "cfg-value"
t "service --user applies" "$($D logs $cid 2>&1)" "1000"
t "service ls --format" "$($D service ls --format '{{.Name}} {{.Replicas}}')" "swsvc 1/1"
$D secret rm db_pass >/dev/null 2>&1 && bad "secret in use cannot be removed" "" || ok "secret in use cannot be removed"
$D service update --env-add NEWVAR=1 swsvc >/dev/null 2>&1; sleep 6
$D service rollback swsvc >/dev/null 2>&1; sleep 6
t "rollback goes back" "$($D service inspect swsvc)" "swsvc"
cid=$($D ps -q --filter name=swsvc | head -1)
$D exec $cid env | grep -q NEWVAR && bad "rollback removed the update" "" || ok "rollback removed the update"
$D node update --availability drain self >/dev/null 2>&1; sleep 6
[ "$($D ps -q --filter name=swsvc | wc -l | tr -d ' ')" = "0" ] && ok "drain stops the tasks" || bad "drain" "$($D ps)"
$D node update --availability active self >/dev/null 2>&1; sleep 6
t "active starts them again" "$($D service ls --format '{{.Replicas}}')" "1/1"
$D service rm swsvc >/dev/null 2>&1
$D secret rm db_pass >/dev/null 2>&1 && ok "unused secret can be removed" || bad "secret rm" ""
$D swarm leave -f >/dev/null 2>&1
[ $fail = 0 ] && echo "ALL PASSED"
exit $fail
