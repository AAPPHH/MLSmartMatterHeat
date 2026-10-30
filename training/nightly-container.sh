set -e
W=/tmp/w
mkdir -p $W/lake
cp -r /data/raw /data/parquet $W/lake/
cd /app
uv run --no-sync python -m heattrain thermalml $W/lake $W/thermal
v=$(ls $W/thermal/thermal | tail -1)
cp -r $W/thermal/thermal/$v /models/thermal/
uv run --no-sync python -m heattrain watchroom $W/lake $W/room && v=$(ls $W/room | tail -1) && cp -r $W/room/$v /models/watchroom/ || echo "watchroom training skipped"
for d in thermal watchroom; do ls -1d /models/$d/*/ | head -n -14 | xargs -r rm -rf; done
date -u > /models/last_nightly
