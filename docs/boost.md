# Boost (learned, scenario-gated)

Trainer: `training/src/heattrain/boostlearn.py` (`boostlearn`, `boostcheck`, `boostmark`, timer `deploy/train/heattrain-boost.timer`, every 10 min, retrains only after a new live `boost_cycle` event). heatd: `crates/heatd/src/boostlearn.rs` (bundle, `Dosing.decide` port) and `crates/heatd/src/boost.rs` (triggers, execution, API state). The grey_box / residual optimum-start model (`thermal.rs`, `thermalml.py` bundle) is no longer read by heatd.

## Rules (fixed)

- Boost happens in exactly two scenarios. MORNING: the watch sleep state (or bedtime) ends between 05:00 and 11:00 local while home; rooms whose desired setpoint rises by at least 1 K within the trigger window (3 min, covers the tick of the wake-up and the next ones). HOMECOMING: occupancy goes from away to home; rooms rising by at least 1 K from the away setpoint. A room change, a schedule step or any other rise never boosts.
- Boost setpoint 24 C, valve open 5 to 20 min. The room is boosted by writing 24 C, held for the chosen minutes, then the room's own target is written. `MaxHeatSetpointLimit` is raised to the boost setpoint (rounded up to 0.5 C, never above `[failsafe] boost_max_c`) right before the write and lowered right after the return write.
- Abort (hand back the target, event `end`): sensor >= `abort_c` of the bundle (24 C, `sensor_limit`), target decrease (`target_decrease`), target change (`target_changed`), stale sensor (`sensor_stale`). Window open, away and a manual change on the device end the cycle without a write (those paths own the setpoint: `window_open`, `away`, `manual_hold`).
- Raise budget (1 per room and hour): the boost write is the room's raise; the cycle does not open when the hour's raise is spent. The return is a decrease and free. The target raise that coincides with the boost trigger is replaced by the boost, so the pair costs one raise.
- Dose: at most 1.1 times the strongest known dose in a similar own-room situation (context vector, trust region), 5 min if there is no similar own-room episode, and at most `d_safe`, the largest dose with P(peak > target + 0.5 K) <= 10 % under the closed-loop posterior (thermostat closure after the boost). The chosen dose is a Thompson pick below `allowed`. Without a loaded bundle there is no boost.

## Bundle

`/models/boost/<YYYYmmddTHHMMSSZ>/bundle.json` (`kind: boost_learn`). Timestamp-named directories only, the newest one that parses wins, an invalid newer one is skipped and the running model stays; re-checked every `[boost] reload_min`. `Dosing.decide` is ported exactly; parity data `crates/heatd/testdata/boost_parity.json` is written by `training/tests/make_boost_fixture.py` (all outputs to 1e-6). The Thompson column index is the only random part: Python records it (`j`), heatd draws it from its own generator.

## Live context vector

`out`, `wind` (default `[boost] wind_default`), `rad` from the weather feed; `start_temp` latest sensor value, `gap` = target - start_temp; `vm60`/`vm180` = sum of `u` over the last 60/180 min, `u = clip((valve - dead_zone) / (1 - dead_zone), 0, 1)` times the supply ramp; `since_open` = minutes since `u` was last above 0 (cap from the bundle); `supply_ok` / `since_resume` from the no-supply state of the room (capped at `resume_cap_min`); `hour` local. The kernel history for the closed-loop rollout is the last `kernel.len_min` values of `u`.

## Events

`boost_cycle`, one row per cycle at the end of the 60 min measurement after the boost (`ts` = end): `room`, `variant = boost_learn`, `mode` (`live`: written; `aborted`: armed but never written; `refused`: scenario seen but no boost, with `reason`), `scenario`, `source` (`auto` or `manual`), `cycle_id`, `start_ts_ms` (write), `boost_min` (applied), `chosen_min`, `allowed_min`, `d_safe_min`, `known_min`, `p_overshoot`, `p_no_boost`, `context`, `decision`, `peak_c`, `overshoot_c` (peak minus target over the 60 min after the boost end, null while incomplete and not above 0.5 K), `end`, `model_version`. The trainer reads `boost_min`, `scenario` and `overshoot_c`; only `mode = live` counts and triggers the retrain.

## API and intents

`POST /api/boost {room, minutes?, scenario?}`: manual boost through the same checks (scenario gate, raise budget, blocked room, fresh sensor, safety bound). The dose is `min(minutes, allowed)`; without `minutes` the Thompson pick. The scenario defaults to `morning` between 05:00 and 11:00 local, else `homecoming`. 200 with the decision when armed (the write follows within the next control step), 409 with `ok: false` and `reason` when refused (including a missing bundle), 404 for an unknown room. `GET /api/boost` lists the per-room state; `/api/state` carries the same per room (`rooms[].boost`) and `boost` (model version, active trigger); metrics `heatd_boost_*`.

All manual inputs go through `crate::intent::apply(state, Intent)` with `Intent::{Boost, WindowOpen, WindowClose}` (`/api/boost` and `/api/window` use it). A future thermostat-button gesture (for example a short local setpoint bump detected in `rules::on_report` where a device-side change is classified) maps to `Intent::Boost { room, minutes: None, scenario: None }` for a bump above the target and to `Intent::WindowOpen` / `WindowClose` for a longer press pattern; the gesture detector only builds the intent and calls `apply`, all safety checks stay in `Boost::request`. No gesture detection exists yet.
