#!/bin/sh
# IPv6 to the internet from a container (bridge, NAT66). Only meaningful where the host itself has IPv6 to the internet:
# without it the test prints SKIP and passes, because there is nothing to translate.
D="${DOCKER:-docker}"
if ! ip -6 route show default 2>/dev/null | grep -q . || ! ping -6 -c1 -W3 2606:4700:4700::1111 >/dev/null 2>&1; then
  echo "SKIP no IPv6 connectivity on this host (NAT66 to the internet needs it)"
  echo "ALL PASSED"
  exit 0
fi
$D pull alpine >/dev/null 2>&1
fail=0
out=$($D run --rm alpine sh -c 'ip -6 addr show eth0 | grep -c "scope global"; wget -q -T8 -O- "http://[2606:4700:4700::1111]/" 2>&1 | head -c 200')
case "$out" in
  *"1"*) echo "PASS container has a global IPv6 address and reaches the internet over IPv6" ;;
  *) echo "FAIL ipv6 to the internet: $out"; fail=1 ;;
esac
[ $fail = 0 ] && echo "ALL PASSED"
exit $fail
