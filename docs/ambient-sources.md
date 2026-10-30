# Ambient proxies

The thermal model has no room thermometer and no measurement of the house heat load, so device temperatures and power are logged as weak proxies into the lake table `ambient` (schema in `docs/data-foundation.md`).

## Minix collector

`deploy/ambient/ambient.py` runs as `deploy/ambient/ambient.service` (root, Minix Ubuntu 24.04, `Restart=always`). Install: copy `ambient.py` to `/opt/heatd-ambient/`, copy the unit to `/etc/systemd/system/`, `systemctl enable --now ambient`. Every 60 s it posts to `http://127.0.0.1:8090/api/ambient`:

- `nvme_c` (C): `/sys/class/hwmon/hwmon*/` with `name == nvme`, `temp1_input` in millidegrees.
- `pkg_w` (W): CPU package power from the delta of `/sys/class/powercap/intel-rapl:0/energy_uj` between two samples (counter wrap corrected with `max_energy_range_uj`; the first sample after start is skipped). Needs root.

A missing sensor is skipped; a failed POST is logged to stderr and the next cycle continues. All settings are the plain dict `CONFIG` at the top of the script.

## Border Router die temperature

`firmware/br/main/watchdog.c` installs the on-chip sensor (`esp_driver_tsens`, range -10..80 C) and serves `GET /ambient` on the watchdog's own HTTP server (port 80) as `{"die_temperature_c": 41.5}`; the same value is in `GET /watchdog/status`. The BR web UI of `esp_ot_br_server` is not enabled in this firmware, so the watchdog runs its own httpd (see `deploy/br-watchdog/README.md`). It exists in source and builds, but is not flashed yet. The REST API of `esp_ot_br_server` itself has no temperature, and the OpenThread diagnostic TLVs contain none either.

Collector side: set `CONFIG["br_http"]["enabled"] = True` and point `url` / `json_path` at the endpoint (default `http://192.168.178.33/ambient`, key `die_temperature_c`). It is disabled by default and posts with `source = "br"`, `sensor = "die_c"`, unit `C`. Until the new firmware is flashed, `die_c` stays missing and the model treats it as NaN.

With the router watchdog (`docs/br-watchdog.md`) heatd itself reads `die_temperature_c` from `GET /watchdog/status` every 30 s and stores it as source `br`, sensor `die_c`; the `br_http` source of the collector can stay disabled.
