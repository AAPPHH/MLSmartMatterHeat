# Border Router watchdog

The Thread Border Router (ESP32-S3, `192.168.178.33`, own USB power supply) watches a heartbeat from heatd on the Minix. If no valid heartbeat arrives for `timeout_s` (default 600 s, counted from boot), it writes `OccupiedHeatingSetpoint = 18.00 C` to thermostat nodes 5-8, because the thermostats have no on-device fallback.

Firmware: `firmware/br/main/watchdog*.c`. Host test of the logic: `firmware/br/test/run.sh`.

## Status of this build

| Part | State |
| --- | --- |
| `POST /watchdog/heartbeat`, `GET /watchdog/status`, `POST /watchdog/mode`, `POST /watchdog/credentials`, `GET /ambient` | built, host-tested logic, not flashed |
| Trigger, re-arm, per-node retry with backoff for 30 min | built, host-tested |
| `dry_run` (log "would send 18.0 to nodes ...", set `triggered`) | built |
| `armed` (real Matter write from the BR) | built (own minimal CASE + IM write client, see Matter client), verified on the host only: spec vectors plus loopback against an independent Python responder. Never run against a device, see `interop.md`. `armed_capable` is true once credentials are stored; without them `POST /watchdog/mode armed` answers 501 `no_credentials` |
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

## Matter client (armed path)

The user decided against esp-matter (no PSRAM, second OpenThread owner, see git history of this section) and for an own minimal controller client in C. Files: `firmware/br/main/matter.h`, `matter_proto.c` (TLV, message/protocol header codec, HKDF/CCM/ECDSA helpers, Matter-TLV certificate to X.509 TBS conversion and chain check), `matter_client.c` (MRP exchange engine, CASE initiator, Interaction Model write, close), `watchdog_matter.c` (SRP address lookup, NVS address cache, lwIP UDP socket, wiring into the retry state machine). mbedTLS only (ECDSA/ECDH P-256, HMAC-SHA256, AES-CCM-128, SHA-256).

- Address: instance `<compressed fabric id, 16 hex>-<node id, 16 hex>` looked up in the BR's own SRP server (`otSrpServer*` under the OpenThread lock); IPv6 non-link-local addresses, port, TXT `SII`/`SAI`. The last hit is cached in NVS (`mc<node>`) and used when SRP has no entry (e.g. after a BR reboot until the device re-registers). Error `no_address` if neither exists.
- Transport: Matter message layer over UDP, one session, MRP: each reliable message is retransmitted with the Matter backoff (1.1 x base x 1.6^n, +0-25% jitter, cap 60 s, 5 retransmissions); base is the device SII (clamped 2-60 s, default 3 s), SAI (1-10 s, default 1.5 s) after the peer answered; response wait after the ack 20 s. Duplicates are detected (8 counters) and re-acked; every received reliable message is acked (piggybacked on Sigma3, standalone otherwise).
- CASE: Sigma1 (destination id from the IPK-derived operational key), Sigma2 decrypt and peer chain check (NOC against ICAC/RCAC, node id and fabric id must match, signature over TBS2), Sigma3, StatusReport, keys from `SessionKeys`. No resumption. Peer StatusReports map to `case_nosharedroot`, `case_invalidparam`, `case_busy`.
- IM: WriteRequest (endpoint, cluster 0x0201, attribute 0x0012, int16 centi-C, not timed), WriteResponse status parsed (`write_status_NN` for a non-zero status), CloseSession StatusReport.
- Errors reach `results[].err` as short strings (`timeout_sigma1`, `cert_noc_sig`, `sig_sigma2`, ...), the existing retry/backoff of the state machine is unchanged.
- RAM: one static client (about 6.6 KB: tx, rx, plaintext and two work buffers of 1280 B plus state), no other static data; mbedTLS heap use is transient during the handshake. The watchdog task stack is now 14 KB (heap allocated). `GET /watchdog/heap` additionally reports `wd_stack_size` and `wd_stack_free_min` (stack high-water after each write) next to `min_free`; read it after the first armed trigger.

Host tests (`bash firmware/br/test/run.sh`): spec vectors for destination id, compressed fabric id and operational IPK (connectedhomeip `TestCASESession`, `TestGroupDataProvider`), RFC 5869 HKDF, TLV and header encodings, byte-exact X.509 TBS from the CHIP TLV certificates `Root01`, `ICA01`, `Node01_01`, `Node01_02` (SDK `CHIPCert_test_vectors`) with signature verification, and 11 UDP loopback scenarios against `ref_responder.py` (cryptography library, written separately from the C code): normal write, dropped first Sigma1, delayed reply (retransmit), duplicated replies, wrong node id, wrong IPK, busy, bad responder signature, write status error, bad private key, silent peer. What this does not prove: acceptance by a real Matter stack (shared understanding of the spec between both sides is the residual risk). Plan in `interop.md`.

### Security review checklist

- [x] Constant-time compares: key, public key, MAC tag (inside mbedTLS CCM) and destination-id inputs; the token compare in `wd_token_ok` was already constant-time.
- [x] Peer authentication: NOC chain verified by ECDSA over the reconstructed DER TBS against ICAC/RCAC from NVS, node id and fabric id checked, Sigma2 signature checked, ICAC must be CA, NOC must not be CA. Sigma2 payload is only decrypted with keys derived from the ECDH secret and IPK.
- [ ] Certificate validity times are NOT checked (no wall clock on the BR). A revoked or expired but correctly signed NOC of the same fabric would be accepted. Limited by the fabric root being ours.
- [x] Key zeroization: ephemeral private key, ECDH secret, sigma keys, session keys, salts and the whole client struct (incl. buffers with decrypted TBE data) are wiped with a volatile loop at the end of every run; the credentials copy made by the watchdog task is wiped after the call; the HTTP credentials body and parsed struct are wiped in the handler.
- [x] No key material in logs: only node id, stage name and heap numbers are logged; errors are fixed strings.
- [x] Counters: a fresh session per write, random 28-bit initial counters for the unsecured and the secured session, one key per direction, counter incremented per message and never reused for a (key, nonce); retransmissions resend the identical ciphertext (same counter, as Matter MRP does). Peer duplicates are dropped by an 8-entry window, replays inside a session therefore also fail AEAD-wise only if the counter is old; a stronger sliding window is not implemented because the session lives for three messages.
- [x] Input bounds: all TLV reads are bounds-checked (host test truncated input), message size limited to 1280, certificate sizes limited by the credentials format (400 B), privacy and message-extension flags rejected.
- [x] Single session and static buffers: no heap growth from network input.
- [ ] Credentials are in plain NVS (no flash encryption), see above; the key is the controller's operational key, limited by the ACL (Operate, Thermostat cluster only).
- [ ] The IPK of the fabric is shared knowledge of all nodes; an attacker on the Thread mesh without NOC cannot complete CASE.

## Rust Matter client

`firmware/br/matter_client/` is a Rust port of the C client (same CASE/IM behaviour, with the security fixes below). `src/tlv.rs` (TLV, message/protocol headers, counter window), `src/crypto.rs` (RustCrypto: HKDF, AES-CCM-128, P-256 ECDSA/ECDH, Matter-TLV certificate to X.509 TBS), `src/client.rs` (MRP, CASE initiator, IM write, close), `src/ffi.rs` (C ABI, feature `ffi`, the only `unsafe`). The protocol modules are `#![forbid(unsafe_code)]`, `no_std`, allocation free, and use `Io` (send, recv, now, rng) as the only transport boundary. Panics are denied by clippy lints on `unwrap`/`expect`/`panic`; peer input goes through bounds-checked readers and a mutation fuzz test.

Select it with `CONFIG_BR_WATCHDOG_MATTER_RUST=y` (menuconfig, or `sdkconfig.wifi.local`). The IDF build then runs `cargo rustc --release --features ffi --target xtensa-esp32s3-none-elf -Zbuild-std=core --crate-type staticlib` with `RUSTUP_TOOLCHAIN=esp` (espup toolchain, cargo on PATH) and links the `.a` into the main component instead of `matter_proto.c`/`matter_client.c`. `watchdog_matter.c` (SRP, NVS cache, lwIP) is shared by both. Default stays the C client until the Rust one passed `interop.md` on the device.

Differences from the C client (fixes from the security review; the C fallback still has them):

- C: a StatusReport success in reply to Sigma1 was reported as an established session (matter_client.c, `case_connect`). Rust: after Sigma1 only Sigma2 or a failure StatusReport is accepted; success is `proto_sigma2`; `write_attr` refuses an unsecured session; non-Secure-Channel protocols on session id 0 are dropped.
- C: the final standalone ack used the IM protocol id (matter_client.c, `write_attr`). Rust: protocol 0x0000, opcode 0x10, no R flag, also sent after a write error status.
- C: the shim used `sendto`/`recvfrom` on an unconnected socket (any host could inject datagrams). `watchdog_matter.c` now `connect()`s the socket to the peer, for both clients.
- C: 8-entry duplicate set per exchange. Rust: max counter plus 32-bit sliding window per session (unit test `counter_window`).
- C: credential copies wiped with `memset` before `free` (can be optimised away). `watchdog.c` now uses `mbedtls_platform_zeroize`; Rust uses `zeroize`. NVS credential blob lengths are checked against `WD_CERT_MAX` (watchdog.c and the FFI).
- MRP: `max_retx` of `mc_mrp_t` now means total transmissions (5 including the first), the C client sent 6.
- WriteResponse: the status code must be uint and the AttributePath must equal the request.
- ECDSA nonces come from the ESP RNG like in the C code (not RFC 6979), so frames are comparable.

Tests: `cargo test --release` in `firmware/br/matter_client` (also run by `bash firmware/br/test/run.sh`): spec vectors (destination id, compressed fabric id, IPK, RFC 5869, TLV/headers, byte-exact TBS and signatures of Root01/ICA01/Node01_01/Node01_02, counter window, MRP backoff), mutation fuzz of TLV/cert/header parsers and of the client against a garbage peer, 13 UDP loopback scenarios against the strict `ref_responder.py` (the 11 above plus Sigma1 answered with StatusReport success and a peer chain with ICAC; the responder verifies the initiator chain, the piggybacked ack on Sigma3, and that the final ack is Secure Channel/no R flag/acking the WriteResponse), and a differential test: C and Rust clients with a constant RNG produce byte-identical Sigma1, Sigma3 and WriteRequest datagrams (responder messages replayed from a tape recorded with the C client, so both see the same Sigma2).

Build result of the Rust variant (IDF v5.5.4, wifi, `CONFIG_BR_WATCHDOG_MATTER_RUST=y`, esp Rust 1.97.0-nightly, `opt-level=z`, LTO): `esp_ot_br.bin` 1,592,704 B vs 1,541,536 B with the C client (+51,168 B), 50.6% of the 0x300000 slot (3,145,728 B). `.flash.text` 1,161,848 vs 1,111,776, `.flash.rodata` 317,960 vs 316,864, `.dram0.bss` 74,472 vs 74,752 (the Rust client is one 6,400 B static scratch struct), `.dram0.data` 21,396 and `.iram0.text` 90,067 unchanged. Stack use of the Rust path (client struct, 1280 B re-ack copy, P-256 temporaries) was not measured on the device; the watchdog task has 14 KB, read `wd_stack_free_min` after the first armed trigger. Nothing was flashed or run on hardware.

## Build result

IDF v5.5.4, esp32s3, wifi variant, with the Matter client (`CONFIG_BR_WATCHDOG_MATTER=y`): `esp_ot_br.bin` 0x178570 bytes (1,541,488) in a 0x300000 OTA slot, 51% free (+13,552 B over the build without the client, 1,527,936). Static RAM: `.dram0.bss` 74,752 B (was 68,064; the client is one 6,688 B static struct), `.dram0.data` 21,396 B unchanged, `.iram0.text` 90,067 B; D/IRAM static in total about 96 KB of 342 KB before the heap. The runtime heap and stack numbers need the device: `GET /watchdog/heap` (`min_free`, `wd_stack_free_min`) after the first armed trigger. Partitions: nvs 0x9000 (0x6000), otadata 0xf000, phy_init 0x11000, ota_0 0x20000 (3M), ota_1 0x320000 (3M), web_storage 0x620000 (200K), rcp_fw 0x652000 (640K), end 0x6F2000 of 8 MB. Host tests: `bash firmware/br/test/run.sh` (gcc, cJSON and mbedTLS from the IDF tree, python with `cryptography` for the loopback responder): 5572 watchdog checks, 68 Matter checks, 11 loopback scenarios. The worktree build needs `firmware/esp-thread-br` (submodule) present. Nothing was flashed or run on the device; HTTP handlers, OTA, rollback, NVS, SRP lookup, the lwIP socket, the sensor and the task are compile-checked only.

## First flash and later updates

First flash of this build over USB (partition table changes, Thread dataset kept): `.irmwareuild.ps1 -Variant wifi -Port COM7 -NoErase` (the BR on the PC or Minix once). Then check `GET /watchdog/status` (`fw`, mode `dry_run`) and `GET /watchdog/heap`. The first USB-flashed image is not rollback-protected. If the Thread network does not come back after the table change, restore the dataset from `secrets/thread_dataset.hex`.

Later updates: `./deploy/br-watchdog/ota.sh` from the Minix (build on the PC, copy `esp_ot_br.bin`, pass it as argument).
