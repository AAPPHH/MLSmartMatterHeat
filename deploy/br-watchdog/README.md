# Border Router watchdog

The Thread Border Router (ESP32-S3, `192.168.178.33`, own USB power supply) watches a heartbeat from heatd on the Minix. If no valid heartbeat arrives for `timeout_s` (default 600 s, counted from boot), it writes `OccupiedHeatingSetpoint = 18.00 C` to thermostat nodes 5-8, because the thermostats have no on-device fallback.

Firmware: `firmware/br/main/watchdog*.c`. Host test of the logic: `firmware/br/test/run.sh`.

## Status of this build

| Part | State |
| --- | --- |
| `POST /watchdog/heartbeat`, `GET /watchdog/status`, `POST /watchdog/mode`, `POST /watchdog/credentials`, `GET /ambient` | built, host-tested logic, not flashed |
| Trigger, re-arm, per-node retry with backoff for 30 min | built, host-tested |
| `dry_run` (log "would send 18.0 to nodes ...", set `triggered`) | built |
| `armed` (real Matter write from the BR) | **not built**, see Gap. `POST /watchdog/mode {"mode":"armed"}` answers 501 `matter_not_built`, `status.armed_capable` is `false` |
| `provision.sh` (operational cert, ACL, delivery) | written, never run |

## HTTP contract (port 80)

Writes need the header `X-Heatd-Token: <secret>`; missing or wrong token gives 401. An empty configured token rejects everything. `GET` routes are open (no secrets in them).

- `POST /watchdog/heartbeat` body `{"ts": ms, "timeout_s": 600, "setpoint_c": 18.0, "nodes": [5,6,7,8], "endpoint": 1}` answers `{"ok":true,"mode":"dry_run","age_s":<age of the previous heartbeat>}`. Fields are optional; valid ones update the config (timeout 60-86400 s, setpoint 5-30 C, 1-8 nodes, endpoint 1-254) and are written to NVS only when they changed. `ts` is ignored, the BR uses its own uptime clock.
- `GET /watchdog/status` answers `mode`, `armed_capable`, `last_heartbeat_age_s`, `timeout_s`, `setpoint_c`, `endpoint`, `triggered`, `triggered_at_uptime_s`, `results[{node,ok,err}]`, `die_temperature_c`, `uptime_s`, `fw`.
- `POST /watchdog/mode` body `{"mode":"dry_run"|"armed"}`. Persisted. Defaults to `dry_run`.
- `POST /watchdog/credentials` body from `provision.sh` (hex fields `fabric_id`, `node_id`, `rcac`, `icac`, `noc`, `key` = 97-byte serialized P-256 keypair, `ipk` = 16-byte epoch key). Stored in NVS.
- `GET /ambient` answers `{"die_temperature_c": 41.5}` from the on-chip sensor (`docs/ambient-sources.md`).

The Thread BR web server of `esp_ot_br_server` is not enabled in this firmware (`CONFIG_OPENTHREAD_BR_START_WEB` is off, which is good: its REST API serves the Thread dataset without authentication). The watchdog therefore runs its own small `esp_http_server` on port 80, started from `app_main` just before the border router launches, with exactly the five routes above. Enabling the BR web UI later would collide on port 80 and is blocked by an `#error` in `watchdog.c`.

Semantics: a trigger fires once per outage (in `dry_run` it only logs and sets `triggered`). A valid heartbeat re-arms and cancels pending retries. Switching `dry_run` to `armed` during an outage starts the writes at once. Failed nodes retry after 30, 60, 120, 240, then every 300 s, for 30 min after the trigger, then the node is reported as gave up.

## Token

```
umask 077; mkdir -p secrets; openssl rand -hex 24 > secrets/br_watchdog_token
scp secrets/br_watchdog_token aapphh@192.168.178.29:MLSmartMatterHeat/secrets/
```

`firmware/build.ps1` turns `secrets/br_watchdog_token` into the gitignored `firmware/br/sdkconfig.<variant>.token.local` (`CONFIG_BR_WATCHDOG_TOKEN`). The token is inside the unencrypted firmware image and sent in clear over the LAN, like the rest of the BR web interface.

heatd side: `[br_watchdog] url = "http://192.168.178.33"`, `token_file = "/secrets/br_watchdog_token"`, `interval_s = 30`, `timeout_s = 600`, `enabled = true`.

## Flashing

The BR gets its own USB supply; the data cable to the Minix can go. Flashing needs the BR on the PC or Minix once.

`.\firmware\build.ps1 -Variant wifi -Port COM7` runs `erase-flash` first. That wipes NVS including the Thread dataset; the thermostats then cannot rejoin until the dataset from `secrets/thread_dataset.hex` is restored. Partition table and flash size are unchanged by this build, so use `-NoErase`:

`.\firmware\build.ps1 -Variant wifi -Port COM7 -NoErase`

After the first boot `GET /watchdog/status` shows mode `dry_run`; `triggered` turns true after 600 s without heartbeat and false again with the next heartbeat.

## Provisioning the armed path (not needed for dry_run)

Run on the Minix as the user that owns `~/MLSmartMatterHeat`. Nothing is written unless `APPLY=1`.

1. Build chip-cert once on the PC (Docker Desktop) and ship it: `docker build -f deploy/br-watchdog/Dockerfile.chipcert -t chipcert:local .` then `docker save chipcert:local | gzip | ssh aapphh@192.168.178.29 'gunzip | docker load'`. The heatd image has chip-tool only. Alternative: point `CHIP_CERT_BIN` at any chip-cert binary of connectedhomeip v1.5.1.0.
2. `scp deploy/br-watchdog/provision.sh aapphh@192.168.178.29:MLSmartMatterHeat/` and `ssh` there.
3. Dry look: `STEP=cert ./provision.sh` creates `secrets/br_watchdog_creds.json` (0600): a fresh P-256 key and a NOC for node `0x1B2D0001` signed by the chip-tool ICA (`ExampleOpCredsICAKey0`, `ExampleCAIntermediateCert0`, `ExampleCARootCert0` from the chip-tool storage). It works on a copy of `chip-storage`, so the running heatd is not disturbed. The fabric id is read from the commissioner NOC (`f/<n>/n`); set `FABRIC_ID=<hex>` if several fabrics exist.
4. `STEP=acl ./provision.sh` reads the ACL of each node (5 6 7 8), parses it, and prints what it would write. With `APPLY=1` it appends one entry (Operate, CASE, subject `0x1B2D0001`, target cluster 0x0201 endpoint 1) to the existing list, never replaces it. It refuses a node whose current ACL has no Administer entry, reads the ACL back, and restores the old list if the old entries are not all still there. Sleepy devices may need the retries (`TRIES`, `TOOL_TIMEOUT`).
5. `STEP=push APPLY=1 ./provision.sh` posts the credentials to `http://192.168.178.33/watchdog/credentials` (token from `secrets/br_watchdog_token`, passed through a 0600 header file, not on the command line).
6. `STEP=all APPLY=1 ./provision.sh` does 3-5 in one go.

Assumptions in `provision.sh` that were not checked against a live chip-tool: the text layout of `accesscontrol read acl` (the script aborts if the entry count it parsed differs from the count chip-tool printed, and shows the raw output), the chip-tool storage key suffix `0` (`ISSUER_INDEX`), and the IPK `temporary ipk 01` (`IPK_HEX`), which is what chip-tool's controller factory sets for its fabric. A wrong IPK only shows up as CASE failures on the BR. The BR must derive the operational group key from the IPK epoch key and the compressed fabric id before CASE.

Encrypted NVS is not used: on the ESP32-S3 it needs flash encryption or an HMAC eFuse (irreversible), and no flash encryption is enabled. The credentials sit in plain NVS. The blast radius is limited by the ACL: Operate on the Thermostat cluster, no Administer.

## Gap: armed path not built

What exists: `watchdog_matter.h` declares `wd_matter_write_setpoint(creds, node, endpoint, centi_c, err, err_len)`. `watchdog_matter.c` returns `matter_not_built`; with `CONFIG_BR_WATCHDOG_MATTER=y` it stops with `#error`, so the flag cannot be switched on by accident.

Why it is not built:

- esp-matter (with its connectedhomeip submodules, several GB) is not in the tree or the local toolchain, and the controller component has to match IDF 5.5.4, which pins the esp-matter release to try first.
- Size: see "Build result" below. The app slot is 2 MB; the CHIP stack with a CASE initiator adds on the order of 1 MB or more, so the OTA slots must grow (flash is 8 MB, the table uses about 4.9 MB; two 3 MB slots fit) which needs one `erase-flash` flash of the partition table.
- RAM is the harder limit: the S3 has no PSRAM in this configuration, and Wi-Fi, OpenThread BR and mDNS are already resident. A CASE handshake needs tens of KB of heap for a few seconds; this must be measured on the device.
- Reaching the thermostats: operational addresses come from the thermostats' SRP registrations. The BR's SRP server (`otSrpServerGetNextHost`) has them, so no mDNS lookup is required; the write goes out over the Thread netif.

What is needed to finish: add `espressif/esp_matter` (controller enabled, `CONFIG_ESP_MATTER_CONTROLLER_ENABLE=y`) to `firmware/br/main/idf_component.yml`, grow the OTA partitions in `partitions.csv`, implement `wd_matter_write_setpoint` as a CASE session from the stored `rcac/icac/noc/key/ipk` followed by a Thermostat write of attribute 0x0012, report the result string (for example `timeout`, `case_failed`, `status_0x..`) back as `err`, and switch the flag on.

## Build result

IDF v5.5.4, esp32s3, wifi variant: `esp_ot_br.bin` 0x174630 bytes (1,525,296) in a 0x200000 OTA slot, 27% free. The previous firmware was 1,497,952 bytes, so the watchdog costs about 27 KB. Host test: `bash firmware/br/test/run.sh` (gcc, cJSON from IDF), 5572 checks. Nothing was flashed or run on the device; HTTP handlers, NVS, the sensor and the task are compile-checked only.
