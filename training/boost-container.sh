set -e
cd /app
rc=0
uv run --no-sync python -m heattrain boostcheck /data /models || rc=$?
[ "$rc" -eq 10 ] || exit "$rc"
W=/tmp/b
mkdir -p $W/lake /models/boost
cp -r /data/raw /data/parquet $W/lake/
uv run --no-sync python -m heattrain boostlearn $W/lake $W/models
v=$(ls $W/models/boost | tail -1)
cp -r $W/models/boost/$v /models/boost/
ls -1d /models/boost/*/ | head -n -14 | xargs -r rm -rf
