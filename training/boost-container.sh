set -e
cd /app
py="uv run --no-sync python -m heattrain"
rc=0
$py boostcheck /data /models || rc=$?
[ "$rc" -eq 10 ] || exit "$rc"
W=/tmp/b
if rm -rf $W && mkdir -p $W/lake /models/boost && cp -r /data/raw /data/parquet $W/lake/ && $py boostlearn $W/lake $W/models && v=$(ls $W/models/boost | tail -1) && cp -r $W/models/boost/$v /models/boost/ && { ls -1d /models/boost/*/ | head -n -14 | xargs -r rm -rf; }; then
  $py boostmark /models ok
else
  $py boostmark /models fail
  exit 1
fi
