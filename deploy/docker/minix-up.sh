#!/usr/bin/env bash
set -euo pipefail
M=${MINIX:-aapphh@192.168.178.29}
PORT=${HEATD_PORT:-8090}
cd "$(dirname "$0")/../.."
docker image inspect heatd:local >/dev/null 2>&1 || { echo "image heatd:local fehlt, erst mit docker build bauen (README)" >&2; exit 1; }
T=$(mktemp -d)
sed -e "s/^listen = .*/listen = \"0.0.0.0:$PORT\"/" -e 's/^controller = .*/controller = "mock"/' deploy/docker/heatd.docker.toml > "$T/heatd.toml"
printf 'HEATD_UID=1000\nHEATD_GID=1000\nHEATD_PORT=%s\nNINJA_JOBS=4\n' "$PORT" > "$T/.env"
ssh "$M" 'mkdir -p ~/MLSmartMatterHeat/{deploy/docker,data,models,chip-storage,secrets} && chmod 700 ~/MLSmartMatterHeat/secrets'
scp -q deploy/docker/compose.yaml "$T/.env" "$M":MLSmartMatterHeat/deploy/docker/
scp -q "$T/heatd.toml" "$M":MLSmartMatterHeat/heatd.toml
ssh "$M" 'test -e ~/MLSmartMatterHeat/data/heatd.duckdb' || scp -q data/heatd.duckdb* data/snapshot.duckdb "$M":MLSmartMatterHeat/data/
scp -q secrets/thread_dataset.hex "$M":MLSmartMatterHeat/secrets/
docker save heatd:local | gzip -1 | ssh "$M" 'gunzip | docker load'
ssh "$M" "chmod 600 ~/MLSmartMatterHeat/secrets/*; cd ~/MLSmartMatterHeat/deploy/docker && docker compose up -d --no-build --force-recreate && sleep 25 && curl -fsS localhost:$PORT/health"
code=$(ssh "$M" "curl -s -o /dev/null -w '%{http_code}' -X POST -H 'Content-Type: application/json' -d '{\"device\":\"x\",\"from\":2,\"to\":1,\"label\":\"a\"}' localhost:$PORT/api/label")
[ "$code" = 400 ] || { echo "POST /api/label mit ungueltigem Body lieferte $code statt 400, Image ohne Label-Endpunkt?" >&2; exit 1; }
rm -rf "$T"
