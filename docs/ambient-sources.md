# Ambient proxies

The thermal model has no room thermometer and no measurement of the house heat load, so device temperatures and power are logged as weak proxies into the lake table `ambient` (schema in `docs/data-foundation.md`).

## Minix collector

`deploy/ambient/ambient.py` runs as `deploy/ambient/ambient.service` (root, Minix Ubuntu 24.04, `Restart=always`). Install: copy `ambient.py` to `/opt/heatd-ambient/`, copy the unit to `/etc/systemd/system/`, `systemctl enable --now ambient`. Every 60 s it posts to `http://127.0.0.1:8090/api/ambient`:

- `nvme_c` (C): `/sys/class/hwmon/hwmon*/` with `name == nvme`, `temp1_input` in millidegrees.
- `pkg_w` (W): CPU package power from the delta of `/sys/class/powercap/intel-rapl:0/energy_uj` between two samples (counter wrap corrected with `max_energy_range_uj`; the first sample after start is skipped). Needs root.

A missing sensor is skipped; a failed POST is logged to stderr and the next cycle continues. All settings are the plain dict `CONFIG` at the top of the script.

## Border Router die temperature (gap)

Checked in `firmware/br` and in the used component `esp-thread-br/components/esp_ot_br_server` (REST routes in `src/openapi.yaml`): the ESP32-S3 BR firmware exposes no temperature. The REST API serves only OpenThread data (`/diagnostics`, `/node`, `/node/...`, dataset, Wi-Fi config); the OpenThread diagnostic TLVs contain no chip temperature either. `die_c` therefore cannot be collected without a firmware change.

Firmware change that closes the gap (not done, nothing flashed):

1. In `firmware/br/main/esp_ot_br.c` install the on-chip sensor with the `esp_driver_tsens` component (`#include "driver/temperature_sensor.h"`, `temperature_sensor_install` with range -10..80 C, `temperature_sensor_enable`).
2. Register one more `httpd` URI (for example `GET /ambient`) on the server started by `esp_br_web_start`, answering `{"die_temperature_c": <temperature_sensor_get_celsius>}`; the web server handle is created inside `esp_ot_br_server`, so either add the route there or start a second small `esp_http_server` instance on another port.
3. Add `esp_driver_tsens` to the `REQUIRES` of `firmware/br/main/CMakeLists.txt`.

Collector side is ready: set `CONFIG["br_http"]["enabled"] = True` and point `url` / `json_path` at the endpoint (default `http://192.168.178.33/ambient`, key `die_temperature_c`). It is disabled by default and posts with `source = "br"`, `sensor = "die_c"`, unit `C`. Until the firmware exists, `die_c` stays missing and the model treats it as NaN.
