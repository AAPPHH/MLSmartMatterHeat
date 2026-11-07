set -e
M=aapphh@192.168.178.29
R=/c/clones/MLSmartMatterHeat
W="${LOCALAPPDATA:-/c/Users/$USERNAME/AppData/Local}/heatd-train"
mkdir -p "$W"
cd "$W"
rm -rf lake models_thermal models_room models_window && mkdir lake
ssh -o ConnectTimeout=20 "$M" "cd ~/MLSmartMatterHeat/data && tar cz raw parquet \$(test -d ground_truth && echo ground_truth)" | tar xz -C lake
cd "$R/training"
uv run -q python -m heattrain thermalml "$W/lake" "$W/models_thermal" > "$W/thermal.log" 2>&1
v=$(ls "$W/models_thermal/thermal" | tail -1)
scp -qr "$W/models_thermal/thermal/$v" "$M":MLSmartMatterHeat/models/thermal/
uv run -q python -m heattrain watchroom "$W/lake" "$W/models_room" > "$W/room.log" 2>&1 && v=$(ls "$W/models_room" | tail -1) && scp -qr "$W/models_room/$v" "$M":MLSmartMatterHeat/models/watchroom/
uv run -q python -m heattrain window "$W/lake" "$W/models_window" > "$W/window.log" 2>&1 && v=$(ls "$W/models_window/window" | tail -1) && scp -qr "$W/models_window/window/$v" "$M":MLSmartMatterHeat/models/window/ || echo "window export skipped"
ssh "$M" "cd ~/MLSmartMatterHeat/models && mkdir -p window && for d in thermal watchroom window; do ls -1d \$d/*/ | head -n -14 | xargs -r rm -rf; done"
date > "$W/last_ok"
