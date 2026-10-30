# Router watchdog (both directions)

The Thread Border Router (192.168.178.33, own USB power supply) holds the fallback because the thermostats have no schedule. If heatd or the Minix dies, the router sends 18.0 to nodes 5-8 after `timeout_s`. heatd in turn watches the router.

## Contract (router HTTP, port 80)

- `POST /watchdog/heartbeat` `{"ts", "timeout_s", "setpoint_c", "nodes", "endpoint"}` with header `X-Heatd-Token`; reply `{"ok", "mode", "age_s"}`.
- `GET /watchdog/status` -> `mode` (dry_run | armed), `last_heartbeat_age_s`, `timeout_s`, `triggered`, `results`, `die_temperature_c`, `uptime_s`, `fw`.
- `POST /watchdog/mode` `{"mode": "armed"}` with token. Missing or wrong token gives 401. The router starts in `dry_run` and counts the timeout from boot.

## heatd side (`crates/heatd/src/br_watchdog.rs`, config `[br_watchdog]`)

Every `interval_s` (30) heatd polls the status, then sends the heartbeat only while it is healthy: the last control step is younger than `control_stale_s` (120 s) and at least one thermostat is subscribed (Unknown counts as healthy only for the first 10 minutes after start), at least one thermostat reported within `thread_silent_s` (900 s), and node count (1-8) and endpoint (1-254) fit the router limits (violation is logged once). A hung or zombie heatd therefore stops feeding the router. The token is read from `token_file` (`/secrets/br_watchdog_token` in the container, `~/MLSmartMatterHeat/secrets/br_watchdog_token` on the Minix, gitignored, mounted read-only by `compose.yaml`).

State (in `/api/state` as `br_watchdog`, lake event `br_watchdog` on every change, section "Router-Watchdog" in the daily report):

| state | meaning |
|---|---|
| ok | router answers, thermostats report |
| br_http_down | `down_after` (3) failed status polls or heartbeats in a row |
| thread_down | router answers, but every thermostat was silent for `thread_silent_s` (900 s) |
| both | both conditions |

A non-ok state adds `alert: {id, title, text}` to every `/api/scan` and `/api/scans` reply; the watch app shows it once per id as a notification. If the router reports `triggered: true` while heatd is healthy (for example after a network gap), heatd forgets its write bookkeeping and the next control step writes the desired setpoints again (event `br_watchdog`, `action: rewrite`), and the next heartbeat re-arms the router. `die_temperature_c` goes into the `ambient` table as source `br`, sensor `die_c`; the separate collector source is not needed.

## Ops

1. Create the token once as described in `deploy/br-watchdog/README.md` (section Token: `openssl rand -hex 24` on the PC, `scp` to `secrets/br_watchdog_token` on the Minix; `firmware/build.ps1` bakes the same value into the router). Then set `[br_watchdog] enabled = true` in `deploy/docker/heatd.docker.toml` (it is `false` until the router is flashed and the token exists) and restart heatd (`minix-up.sh`).
2. Check: `curl http://192.168.178.33/watchdog/status` shows `mode: dry_run`, `last_heartbeat_age_s` below 60 and `triggered: false`; `curl http://127.0.0.1:8090/api/state` shows `br_watchdog.state: ok`.
3. Test while dry_run: `touch /var/lib/heatd-failsafe/maintenance` (so the host failsafe does not act during the test), `docker compose stop heatd`, wait 10 minutes, `curl .../watchdog/status` must show `triggered: true` (log says "would send 18.0 to nodes 5,6,7,8"). Start heatd again: the next heartbeat clears `triggered`. Afterwards `rm /var/lib/heatd-failsafe/maintenance`.
4. Arm: `curl -X POST -H "X-Heatd-Token: $(cat secrets/br_watchdog_token)" -d '{"mode":"armed"}' http://192.168.178.33/watchdog/mode`. Repeat step 3 and check `results` per node (sleepy thermostats may take up to 30 minutes), then restore the desired setpoints by starting heatd and remove the maintenance file again. Heatd rewrites its desired setpoints only when the router reports `triggered` in mode `armed` (manual holds are kept).
5. Back to dry_run with the same call and `"mode":"dry_run"`.
