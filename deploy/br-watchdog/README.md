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
| `POST /ota`, rollback, `GET /watchdog/heap` | built, not flashed |
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

`.\firmware\build.ps1 -Variant wifi -Port COM7` runs `erase-flash` first. That wipes NVS including the Thread dataset; the thermostats then cannot rejoin until the dataset from `secrets/thread_dataset.hex` is restored. This build moves ota_0/ota_1 from 2 MB to 3 MB (new offsets, see Build result). NVS (0x9000) and its Thread dataset stay where they are, so `-NoErase` still works; the table, both OTA slots, otadata, web_storage and rcp_fw are rewritten by the flash:

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

## OTA over WiFi

`POST /ota` (header `X-Heatd-Token`, body = raw `esp_ot_br.bin`) streams the image into the inactive OTA slot with `esp_ota_*`, checks the image hash and that the project name matches, sets the boot partition and reboots 1.5 s after the answer `{"ok":true,"bytes":..,"slot":..,"version":..}`. Errors: 401, 409 `busy`, 400 `size|recv|write|image_invalid|wrong_project|no_slot`, 500 `begin|set_boot|oom`. The watchdog keeps running during the upload (own task).

Rollback: `CONFIG_BOOTLOADER_APP_ROLLBACK_ENABLE=y`. A freshly OTA'd image boots as pending-verify. `validate_task` marks it valid after 60 consecutive seconds in which the watchdog task ticks, the httpd is up and the Thread role is child/router/leader. If that does not happen within 600 s it calls `esp_ota_mark_app_invalid_rollback_and_reboot()`; a crash or reset before validation makes the bootloader roll back on its own. An image flashed over USB is not pending-verify and is not subject to this.

`./ota.sh [image]` (defaults: `BR_URL=http://192.168.178.33`, `TOKEN_FILE=secrets/br_watchdog_token`, image `firmware/br/build-wifi/esp_ot_br.bin`) uploads and polls `/watchdog/status` until the BR reports an uptime below 120 s again. `GET /watchdog/heap` returns `free`, `min_free`, `largest_block` for heap checks.

RCP (ESP32-H2) over WiFi is not built: `rcp_fw` is a SPIFFS image generated by `esp_rcp_update` from the `ot_rcp` build and consumed by `AUTO_UPDATE_RCP` at boot; writing it over HTTP means shipping a ready SPIFFS image and a second authenticated writer into a partition that is mounted. Reflash it over USB (`build.ps1 -NoErase`) when the RCP changes.

## Gap: armed path (esp-matter controller) not built

`POST /watchdog/mode armed` still answers 501 `matter_not_built`, `armed_capable` is false, `CONFIG_BR_WATCHDOG_MATTER=y` stops with `#error`. Nothing in the firmware pretends otherwise. What was found (research and static numbers; esp-matter itself was not cloned or built, no device run was possible):

1. PSRAM. Espressif's own controller example (`esp-matter/examples/controller`, ESP32-S3) is documented as "RAM heavy": its `sdkconfig.defaults` moves Wi-Fi/lwIP, Matter, mbedTLS and BSS segments into octal PSRAM, and it has an extra `sdkconfig.defaults.otbr` for the Thread BR board. This board has no PSRAM (confirmed by the task brief). So the supported configuration does not exist here.
2. Static RAM of the current build (IDF v5.5.4): D/IRAM used statically 164,135 B of 342 KB (`.data` 21,396 + `.bss` 68,064 + `.text` in IRAM/DRAM 74,675), 177,625 B left for the heap before Wi-Fi, lwIP, OpenThread (8 KB task, border router, SRP server) and mDNS allocate at run time. The Matter stack (CHIP `.bss`, packet buffer pool, fabric/session tables, a CASE exchange with P-256 sign/verify and a chain of three certificates, its own task stacks) comes on top. Not measurable without flashing; the new `GET /watchdog/heap` gives the real idle number on the first boot of this build.
3. Two Thread stack owners. esp-matter's `ThreadStackManager` initialises and drives OpenThread itself (`esp_openthread_init`, own netif glue and mainloop). `esp-thread-br` already owns the single OpenThread instance and the BR launch. Running CHIP's transport over the existing instance and netif means replacing CHIP's ESP32 `ThreadStackManagerImpl` and platform layer with a stub that only provides the lwIP UDP/IPv6 path; there is no supported switch for it, it is a port.
4. Host tooling. connectedhomeip builds with GN from a git submodule tree of several GB; esp-matter documents Linux/macOS (WSL on Windows, repo inside the WSL filesystem), and the `esp_matter` registry component (1.6.1) does not carry the CHIP sources. The existing local build is native Windows.
5. Flash is not the limit: the app slots are now 3 MB each (0x300000) and the current image is 0x175080 B (1,527,936 B, 51% of the slot free); CHIP typically adds 1-1.5 MB.

Recommended alternative: a minimal CASE initiator plus Interaction Model WriteRequest client in C on top of mbedTLS (already in the image: ECDSA/ECDH P-256, AES-CCM, HKDF, SHA-256), over lwIP UDP with MRP retransmission. Scope: TLV encode/decode, Sigma1/2/3 with the stored NOC/ICAC/RCAC and IPK-derived destination id, session key derivation, message counters, one WriteRequest (cluster 0x0201, attribute 0x0012, int16) and status parsing. Roughly 1,500-2,000 lines, a few tens of KB of heap for the seconds of a handshake, no second Thread stack. It cannot be verified without a peer, so it needs `chip-tool`/a real thermostat for interop. The signature of `wd_matter_write_setpoint` and the credential format would stay as they are. Alternative without any firmware work: a second controller on the Minix (heatd already has chip-tool/CASE) acting as the watchdog, which defeats the purpose of an independent BR watchdog only if the Minix is the thing that failed.

## Build result

IDF v5.5.4, esp32s3, wifi variant: `esp_ot_br.bin` 0x175080 bytes (1,527,936) in a 0x300000 OTA slot, 51% free (previous firmware with 2 MB slots: 1,525,296 bytes, 27% free). Partitions: nvs 0x9000 (0x6000), otadata 0xf000, phy_init 0x11000, ota_0 0x20000 (3M), ota_1 0x320000 (3M), web_storage 0x620000 (200K), rcp_fw 0x652000 (640K), end 0x6F2000 of 8 MB. Host test: `bash firmware/br/test/run.sh` (gcc, cJSON from IDF), 5572 checks. The worktree build needs `firmware/esp-thread-br` (submodule) present. Nothing was flashed or run on the device; HTTP handlers, OTA, rollback, NVS, the sensor and the task are compile-checked only.

## First flash and later updates

First flash of this build over USB (partition table changes, Thread dataset kept): `.irmwareuild.ps1 -Variant wifi -Port COM7 -NoErase` (the BR on the PC or Minix once). Then check `GET /watchdog/status` (`fw`, mode `dry_run`) and `GET /watchdog/heap`. The first USB-flashed image is not rollback-protected. If the Thread network does not come back after the table change, restore the dataset from `secrets/thread_dataset.hex`.

Later updates: `./deploy/br-watchdog/ota.sh` from the Minix (build on the PC, copy `esp_ot_br.bin`, pass it as argument).
