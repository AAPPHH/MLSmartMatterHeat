#!/usr/bin/env bash
set -euo pipefail
U=${DEPLOY_USER:?set DEPLOY_USER=<linux user owning the checkout>}
R=${HEATD_ROOT:?set HEATD_ROOT=<absolute path of the checkout on this host>}
OUT=${UNIT_DIR:-/etc/systemd/system}
cd "$(dirname "$0")"
for f in heatd.service failsafe/* netguard/*.service netguard/*.timer train/*.service train/*.timer; do
  case "$f" in *.service|*.timer) sed -e "s|@DEPLOY_USER@|$U|g" -e "s|@HEATD_ROOT@|$R|g" "$f" > "$OUT/$(basename "$f")";; esac
done
