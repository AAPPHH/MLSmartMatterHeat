#!/usr/bin/env bash
set -u
HERE=$(cd "$(dirname "$0")" && pwd)
T=$(mktemp -d)
trap 'rm -rf "$T"' EXIT
N=$T/net
BIN=$T/bin
mkdir -p "$BIN"
PASS=0 FAIL=0

stub() {
  printf '#!/usr/bin/env bash\n%s\n' "$2" > "$BIN/$1"
  chmod +x "$BIN/$1"
}

stub ip 'N=$NG_NET
case "$*" in
  "-4 -o addr show dev wlo1 scope global") [ -e $N/v4 ] && echo "2: wlo1 inet 192.168.178.29/24 scope global" ;;
  "-4 route show default dev wlo1") [ -e $N/def4 ] && echo "default via 192.168.178.1 proto dhcp src 192.168.178.29" ;;
  "-6 -o addr show dev wlo1") [ -e $N/a6 ] && echo "2: wlo1 inet6 fd00:aaaa:aaaa::29/64 scope global" ;;
  "-6 route show to fd00:bbbb:bbbb:1::/64 dev wlo1") [ -e $N/r6 ] && echo "fd00:bbbb:bbbb:1::/64 via fe80::1 metric 1024" ;;
  "-6 addr add"*) touch $N/a6; echo "ip $*" >> $NG_CALLS ;;
  "-6 route replace"*) touch $N/r6; echo "ip $*" >> $NG_CALLS ;;
esac
exit 0'
stub ping '[ -e $NG_NET/gw ]'
stub wpa_cli '[ -e $NG_NET/assoc ] && echo wpa_state=COMPLETED; exit 0'
stub networkctl 'echo "networkctl $*" >> $NG_CALLS
[ -e $NG_NET/fix_$1 ] && touch $NG_NET/v4 $NG_NET/def4 $NG_NET/gw
exit 0'
stub systemctl 'echo "systemctl $*" >> $NG_CALLS
[ -e $NG_NET/fix_restart ] && touch $NG_NET/v4 $NG_NET/def4 $NG_NET/gw
exit 0'

export NG_NET=$N NG_CALLS=$T/calls
export NETGUARD_ADDR6=fd00:aaaa:aaaa::29/64 NETGUARD_ROUTE6=fd00:bbbb:bbbb:1::/64 NETGUARD_VIA6=fe80::1
export NETGUARD_STATE_DIR=$T/state NETGUARD_MAINTENANCE=$T/maint NETGUARD_SETTLE=0
export PATH="$BIN:$PATH"

reset() {
  rm -rf "$N" "$T/state" "$T/calls" "$T/out" "$T/maint"
  mkdir -p "$N"
  touch "$T/calls" "$T/out" "$N/v4" "$N/def4" "$N/a6" "$N/r6" "$N/gw" "$N/assoc"
}

run() {
  NETGUARD_NOW=${1:-1000000} bash "$HERE/heatd-netguard.sh" >> "$T/out" 2>&1
}

runs() {
  local i
  for ((i = 0; i < $1; i++)); do run "${2:-1000000}"; done
}

expect() {
  local name=$1 got=$2 want=$3
  if [ "$got" = "$want" ]; then
    PASS=$((PASS + 1))
  else
    FAIL=$((FAIL + 1))
    echo "FAIL $name: got [$got] want [$want]"
  fi
}

calls() { tr '\n' '|' < "$T/calls"; }
lines() { wc -l < "$T/out" | tr -d ' '; }

reset
runs 5
expect healthy_calls "$(calls)" ""
expect healthy_quiet "$(lines)" "0"

reset
rm "$N/v4" "$N/def4"
touch "$N/fix_renew"
runs 2
expect renew_waits "$(calls)" ""
run
expect renew_called "$(calls)" "networkctl renew wlo1|"
expect renew_recovered "$(grep -c 'healthy again' "$T/out")" "1"
before=$(lines)
runs 3
expect renew_quiet_after "$(lines)" "$before"

reset
rm "$N/v4" "$N/def4"
touch "$N/fix_reconfigure"
runs 4
expect reconfigure_after_renew "$(calls)" "networkctl renew wlo1|networkctl reconfigure wlo1|"
expect reconfigure_recovered "$(grep -c 'healthy again' "$T/out")" "1"

reset
rm "$N/v4" "$N/def4"
runs 5 10000
expect escalation "$(calls)" "networkctl renew wlo1|networkctl reconfigure wlo1|systemctl restart systemd-networkd|"
run 10060
run 10120
run 13599
expect restart_limited "$(grep -c 'systemctl restart' "$T/calls")" "1"
expect restart_limit_logged_once "$(grep -c 'rate-limited' "$T/out")" "1"
run 13601
expect restart_after_interval "$(grep -c 'systemctl restart' "$T/calls")" "2"

reset
rm "$N/gw"
runs 6
expect gateway_only_calls "$(calls)" ""
expect gateway_only_logged_once "$(grep -c 'gateway' "$T/out")" "1"
touch "$N/gw"
run
expect gateway_back "$(grep -c 'healthy again' "$T/out")" "1"

reset
rm "$N/r6"
runs 2
expect route_waits "$(calls)" ""
run
expect route_readded "$(calls)" "ip -6 route replace fd00:bbbb:bbbb:1::/64 via fe80::1 dev wlo1|"
expect route_back "$(grep -c 'healthy again' "$T/out")" "1"

reset
rm "$N/a6"
runs 3
expect addr_readded "$(calls)" "ip -6 addr add fd00:aaaa:aaaa::29/64 dev wlo1|"

reset
rm "$N/v4" "$N/def4" "$N/assoc"
runs 6
expect assoc_down_no_repair "$(calls)" ""

reset
rm "$N/v4" "$N/def4"
touch "$T/maint"
runs 6
expect maintenance_calls "$(calls)" ""
expect maintenance_silent "$(lines)" "0"
expect maintenance_no_state "$([ -e "$T/state/state" ] && echo yes || echo no)" "no"

reset
rm "$N/a6" "$N/r6"
NETGUARD_ADDR6= NETGUARD_ROUTE6= NETGUARD_VIA6= runs 6
expect ipv6_unset_skipped "$(calls)" ""
expect ipv6_unset_quiet "$(lines)" "0"

reset
rm "$N/a6" "$N/r6"
cp "$HERE/heatd-netguard.sh" "$T/ng.sh"
printf 'NETGUARD_ADDR6=fd00:cccc::5/64
NETGUARD_ROUTE6=fd00:dddd::/64
NETGUARD_VIA6=fe80::2
' > "$T/netguard.env"
unset NETGUARD_ADDR6 NETGUARD_ROUTE6 NETGUARD_VIA6
for i in 1 2 3; do NETGUARD_NOW=1000000 bash "$T/ng.sh" >> "$T/out" 2>&1; done
expect env_file_used "$(calls)" "ip -6 addr add fd00:cccc::5/64 dev wlo1|ip -6 route replace fd00:dddd::/64 via fe80::2 dev wlo1|"

echo "pass=$PASS fail=$FAIL"
[ "$FAIL" -eq 0 ]
