#!/usr/bin/env bash
set -euo pipefail
BR_URL=${BR_URL:-http://192.168.178.33}
TOKEN_FILE=${TOKEN_FILE:-secrets/br_watchdog_token}
BIN=${1:-firmware/br/build-wifi/esp_ot_br.bin}
[ -s "$BIN" ] || { echo "ota: image $BIN not found" >&2; exit 1; }
[ -s "$TOKEN_FILE" ] || { echo "ota: token file $TOKEN_FILE not found" >&2; exit 1; }
HDR=$(mktemp)
trap 'rm -f "$HDR"' EXIT
chmod 600 "$HDR"
printf 'X-Heatd-Token: %s\n' "$(tr -d '\r\n' < "$TOKEN_FILE")" > "$HDR"
field() { python3 -I -c 'import json,sys;d=json.load(sys.stdin);print(*[d[k] for k in sys.argv[1:]])' "$@"; }
echo "ota: before: $(curl -fsS --max-time 10 "$BR_URL/watchdog/status" | field fw mode)"
curl -fsS --max-time 300 -X POST -H "Content-Type: application/octet-stream" -H @"$HDR" --data-binary @"$BIN" "$BR_URL/ota"
echo
for _ in $(seq 1 40); do
    sleep 5
    if v=$(curl -fsS --max-time 4 "$BR_URL/watchdog/status" 2>/dev/null); then
        up=$(field uptime_s <<<"$v")
        if [ "$up" -lt 120 ]; then
            echo "ota: after reboot: $(field fw mode uptime_s <<<"$v")"
            break
        fi
    fi
done
echo "ota: the image counts as valid after 60 s with the watchdog task and Thread attached; otherwise the BR rolls back within 10 min"
