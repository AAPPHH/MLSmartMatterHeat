#!/usr/bin/env bash
set -u
IFACE=${NETGUARD_IFACE:-wlo1}
GATEWAY=${NETGUARD_GATEWAY:-192.168.178.1}
ADDR6=${NETGUARD_ADDR6:-fd00:aaaa:aaaa::29/64}
ROUTE6=${NETGUARD_ROUTE6:-fd00:bbbb:bbbb:1::/64}
VIA6=${NETGUARD_VIA6:-fe80::1}
STATE_DIR=${NETGUARD_STATE_DIR:-/var/lib/heatd-netguard}
MAINTENANCE=${NETGUARD_MAINTENANCE:-/var/lib/heatd-failsafe/maintenance}
THRESHOLD=${NETGUARD_THRESHOLD:-3}
RESTART_INTERVAL=${NETGUARD_RESTART_INTERVAL:-3600}
SETTLE=${NETGUARD_SETTLE:-8}
NOW=${NETGUARD_NOW:-$(date +%s)}
STATE=$STATE_DIR/state

log() {
  echo "$*"
}

save() {
  printf 'fails=%s\nlast_restart=%s\nlast=%s\nlimited=%s\n' "$fails" "$last_restart" "$last" "$limited" > "$STATE.tmp" && mv "$STATE.tmp" "$STATE"
}

check() {
  problems=
  local gw
  ip -4 -o addr show dev "$IFACE" scope global 2> /dev/null | grep -q inet || problems="$problems ip4"
  ip -4 route show default dev "$IFACE" 2> /dev/null | grep -q default || problems="$problems def4"
  ip -6 -o addr show dev "$IFACE" 2> /dev/null | grep -qF "${ADDR6%/*}/" || problems="$problems a6"
  ip -6 route show to "$ROUTE6" dev "$IFACE" 2> /dev/null | grep -qF "via $VIA6" || problems="$problems r6"
  wpa_cli -i "$IFACE" status 2> /dev/null | grep -q '^wpa_state=COMPLETED' || problems="$problems assoc"
  fix=$problems
  case "$problems" in
    *ip4* | *def4* | *assoc*) ;;
    *)
      gw=$(ip -4 route show default dev "$IFACE" 2> /dev/null | awk '/via/ { print $3; exit }')
      ping -c 1 -W 2 "${gw:-$GATEWAY}" > /dev/null 2>&1 || problems="$problems gw"
      ;;
  esac
}

report() {
  local msg=${problems# }
  [ -z "$msg" ] && msg=ok
  if [ "$msg" != "$last" ]; then
    case "$msg" in
      ok) log "$IFACE healthy again" ;;
      gw) log "gateway $GATEWAY unreachable, $IFACE config fine, no action" ;;
      *) log "$IFACE problems:$problems (check $fails/$THRESHOLD)" ;;
    esac
    last=$msg
  fi
}

restart_networkd() {
  if [ "$last_restart" -gt 0 ] && [ $((NOW - last_restart)) -lt "$RESTART_INTERVAL" ]; then
    [ "$limited" -eq 0 ] && log "systemd-networkd restart rate-limited (last $((NOW - last_restart))s ago)"
    limited=1
    return
  fi
  log "repair: systemctl restart systemd-networkd"
  systemctl restart systemd-networkd
  last_restart=$NOW
  limited=0
}

repair() {
  local step=$((fails - THRESHOLD))
  case "$fix" in
    *a6*)
      log "repair: re-adding $ADDR6 on $IFACE"
      ip -6 addr add "$ADDR6" dev "$IFACE"
      ;;
  esac
  case "$fix" in
    *r6*)
      log "repair: re-adding route $ROUTE6 via $VIA6 on $IFACE"
      ip -6 route replace "$ROUTE6" via "$VIA6" dev "$IFACE"
      ;;
  esac
  case "$fix" in
    *assoc*) ;;
    *ip4* | *def4*)
      case "$step" in
        0) log "repair: networkctl renew $IFACE"; networkctl renew "$IFACE" ;;
        1) log "repair: networkctl reconfigure $IFACE"; networkctl reconfigure "$IFACE" ;;
        *) restart_networkd ;;
      esac
      ;;
  esac
}

mkdir -p "$STATE_DIR"
[ -e "$MAINTENANCE" ] && exit 0
fails=0 last_restart=0 last=ok limited=0
[ -r "$STATE" ] && . "$STATE"

check
if [ -z "$fix" ]; then
  fails=0 limited=0
  report
  save
  exit 0
fi

fails=$((fails + 1))
report
if [ "$fails" -ge "$THRESHOLD" ]; then
  repair
  sleep "$SETTLE"
  check
  if [ -z "$fix" ]; then
    fails=0 limited=0
    report
  fi
fi
save
