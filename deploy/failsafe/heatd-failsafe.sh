#!/usr/bin/env bash
set -u
ROOT=${HEATD_ROOT:-/home/aapphh/MLSmartMatterHeat}
PORT=${HEATD_PORT:-8090}
STATE_DIR=${FAILSAFE_STATE_DIR:-/var/lib/heatd-failsafe}
LOG=${FAILSAFE_LOG:-/var/log/heatd-failsafe.log}
SETPOINT_C=${FAILSAFE_C:-18.0}
THRESHOLD=${FAILSAFE_THRESHOLD:-3}
IMAGE=${HEATD_IMAGE:-heatd:local}
RUN_AS=${HEATD_RUN_AS:-1000:1000}
NODES=${FAILSAFE_NODES:-$(grep -E '^nodes *= *\[' "$ROOT/heatd.toml" 2>/dev/null | grep -oE '[0-9]+' | sort -un | tr '\n' ' ')}
NODES=${NODES:-5 6 7 8}
STATE=$STATE_DIR/state
EVENT_FILE=$ROOT/data/failsafe/watchdog.json
COMPOSE="docker compose -f $ROOT/deploy/docker/compose.yaml"
CENTI=$(awk -v c="$SETPOINT_C" 'BEGIN { printf "%d", c * 100 + 0.5 }')

log() {
  local line="$(date -u +%FT%TZ) $*"
  echo "$line" >> "$LOG"
  logger -t heatd-failsafe -- "$*"
}

save() {
  printf 'fails=%s\nrestarted=%s\nwritten=%s\n' "$fails" "$restarted" "$written" > "$STATE"
}

event() {
  mkdir -p "$(dirname "$EVENT_FILE")" && chown --reference="$ROOT/data" "$(dirname "$EVENT_FILE")"
  printf '{"ts":"%s","action":"%s","detail":"%s"}\n' "$(date -u +%FT%TZ)" "$1" "$2" > "$EVENT_FILE.tmp" && mv "$EVENT_FILE.tmp" "$EVENT_FILE"
}

container_status() {
  docker inspect -f '{{.State.Status}}' "$($COMPOSE ps -aq heatd | head -n1)" 2>/dev/null || echo missing
}

write_all() {
  local node ok=0 failed=0 try out
  for node in $NODES; do
    for try in 1 2 3; do
      out=$(timeout 150 docker run --rm --network host --user "$RUN_AS" --cap-drop ALL --security-opt no-new-privileges:true \
        --entrypoint /usr/local/bin/chip-tool \
        -v "$ROOT/chip-storage:/chip-storage" -v "$ROOT/paa:/opt/paa-root-certs:ro" "$IMAGE" \
        thermostat write occupied-heating-setpoint "$CENTI" "$node" 1 \
        --storage-directory /chip-storage --paa-trust-store-path /opt/paa-root-certs 2>&1)
      if [ $? -eq 0 ] && ! grep -qE 'Run command failure|Response Failure|Error:|[Ss]tatus ?= ?0x[0-9A-Fa-f]*[1-9A-Fa-f]' <<< "$out"; then
        ok=$((ok + 1))
        log "node $node set to $SETPOINT_C"
        continue 2
      fi
      log "node $node write attempt $try failed: $(tail -n 2 <<< "$out" | tr '\n"' '  ')"
      sleep 5
    done
    failed=$((failed + 1))
  done
  log "failsafe write finished: $ok ok, $failed failed"
  event failsafe_write "$ok ok, $failed failed, setpoint $SETPOINT_C"
}

mkdir -p "$STATE_DIR"
[ -e "$STATE_DIR/maintenance" ] && exit 0
fails=0 restarted=0 written=0
[ -r "$STATE" ] && . "$STATE"

if curl -fsS --max-time 8 "http://127.0.0.1:$PORT/health" > /dev/null 2>&1; then
  [ "$fails" -ge "$THRESHOLD" ] && log "heatd healthy again after outage (restarted=$restarted written=$written)"
  fails=0 restarted=0 written=0
  save
  exit 0
fi

fails=$((fails + 1))
[ "$fails" -eq 1 ] && log "heatd health check failed"
if [ "$fails" -lt "$THRESHOLD" ]; then
  save
  exit 0
fi

if [ "$restarted" -eq 0 ]; then
  log "heatd down for $fails checks, restarting once"
  $COMPOSE restart heatd >> "$LOG" 2>&1
  restarted=1
  event restart "after $fails failed checks"
  save
  exit 0
fi

if [ "$written" -eq 0 ]; then
  status=$(container_status)
  if [ "$status" != exited ] && [ "$status" != created ] && [ "$status" != dead ] && [ "$status" != missing ]; then
    log "heatd still unhealthy after restart (container $status), stopping it so heatd writes $SETPOINT_C itself and chip-tool can take the storage"
    $COMPOSE stop -t 60 heatd >> "$LOG" 2>&1
  fi
  log "heatd still down after restart, writing $SETPOINT_C to nodes: $NODES"
  write_all
  written=1
  save
  exit 0
fi

if [ "$(container_status)" != running ] && [ "$(container_status)" != restarting ]; then
  log "bringing heatd back up after failsafe write"
  $COMPOSE up -d --no-build heatd >> "$LOG" 2>&1
fi
save
