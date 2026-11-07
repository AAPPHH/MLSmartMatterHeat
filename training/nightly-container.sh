set -e
W=/tmp/w
mkdir -p $W/lake
cp -r /data/raw /data/parquet $W/lake/
[ -d /data/ground_truth ] && cp -r /data/ground_truth $W/lake/ || true
cd /app
uv run --no-sync python -m heattrain thermalml $W/lake $W/thermal
v=$(ls $W/thermal/thermal | tail -1)
cp -r $W/thermal/thermal/$v /models/thermal/
uv run --no-sync python -m heattrain watchroom $W/lake $W/room && v=$(ls $W/room | tail -1) && cp -r $W/room/$v /models/watchroom/ || echo "watchroom training skipped"
mkdir -p /models/window
uv run --no-sync python -m heattrain window $W/lake $W/winmodel && v=$(ls $W/winmodel/window | tail -1) && cp -r $W/winmodel/window/$v /models/window/ || echo "window export skipped"
for d in thermal watchroom window; do ls -1d /models/$d/*/ | head -n -14 | xargs -r rm -rf; done
date -u > /models/last_nightly
