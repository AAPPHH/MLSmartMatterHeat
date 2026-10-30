# Boost / optimum start (offline reference)

Reference implementation: `training/src/heattrain/thermalml.py` (`rollout`, `optimum_start`, `choose_t_off`). heatd (`crates/heatd/src/thermal.rs`, `boost.rs`) implements the same rollout and the same `t_off` rule. Parity data: `crates/heatd/testdata/thermal_ml_parity.json` (physics, Kalman, feature vectors, ONNX outputs, rollout peaks for all 181 candidates, chosen `t_off`) plus the bundle `crates/heatd/testdata/thermal_bundle/<version>/`, both written by `training/tests/make_thermal_fixture.py` and asserted by Rust and Python tests. heatd refuses a bundle whose `dt_s`, `history_min` or `lag_step_min` differ from its constants (60, 60, 5).

## Idea

No room thermometer. Boost = setpoint 28.0 (valve fully open) until `t_off`; then setpoint = target and the Comet's own PI holds. `t_off` is re-planned after every thermostat measurement so the estimated room AIR temperature peaks at the target. Room air is a latent state. Variants (all in shadow for each heat-up): `grey_box`, `residual`; `baseline` = plain Comet with the normal setpoint (measured).

## Rollout (60 s steps, k = 0..H-1, H = 300)

State `x = (x1, x2)` (radiator/sensor-near store, air), physics doc per room from `thermal.py` (`rc2` or `rc1`), `w = params.w` (rc1: 0), sensor `s = w*x1 + (1-w)*x2`.

Per candidate `T` (minutes) and step k:
1. `s_k = sensor(x)`; for k >= 1 push `s_k` into the sensor history (index 0 = now). At k = 0 the history holds the measured values.
2. `u_k = 1` if `k < T`, else `clip(valve_gain * (target - s_k), 0, 1)`. Push `u_k` into the valve history (index 0 = u_k).
3. `air_k = x2` (grey_box) or `latent(features_k)` (residual).
4. Physics step with `step_terms(params, structure, u_k, tout_k, wind_k, rad_k, night_k, nb, dt/3600)`: `x1' = A11 x1 + A12 x2 + c1`, `x2' = A21 x1 + A22 x2 + c2`.
5. Residual only: `phys_sensor_pred = sensor(x')`, `phys_air = x2` (pre-step), `delta = residual(features_k)`, then `x1' += g*delta`, `x2' += g*delta` with `g = bundle.residual.feedback` (chosen on held-out days from {0, 0.25, 0.5, 1}).
6. `x = x'`.

Predicted peak = `max_k air_k`, k = 0..H-1.

## Choosing t_off

Candidates `T = 0..180` (1 min grid, all rolled out in one batch, no bisection, horizon H = 300 for every candidate). The valve is fully open (`u = 1`) while `k < T`; the PI model is used only afterwards. Feasible: `peak <= target + 0.3`. Choose the feasible `T` with minimal `|peak - target|` (first on ties). If none is feasible, choose `argmin(peak)` (normally `T = 0`, i.e. no boost).

## Features (Rust computes by name)

Histories at 1 min resolution, index 0 = now: `sensor_lag_L = S[L]` (causal forward fill of thermostat reports), `valve_lag_L = V[L]` (PIHeatingDemand 0..1; >100 % invalid -> last valid else 0), `L = 0,5,..,60`. `hour_sin/cos` = sin/cos(2 pi h / 24), h = local (Europe/Berlin) fractional hour. `others_heating` = number of other rooms with valve > 0.05. `minutes_since_boost` = smallest m in 0..239 with `V[m] >= 0.5` and `V[m+1] < 0.5` (start of the current/last >= 50 % run), else 240. `phys_sensor_pred`, `phys_air` as in step 5 (Kalman posterior at now). `ambient_*` NaN when missing. `room_<name>` one-hot.

## Training labels and offset

- `sensor_offset_c`: median of (sensor - Kalman air) over cold phases (valve <= 2 % for > 60 min).
- Latent labels: cold phases `air = sensor - offset`; decay phases after valve close (30..90 min) fit `sensor(t) = a0 + a1 t + A exp(-t/tau)`, `|a1| <= 0.02 K/min`, label `air = a0 + a1 t - offset`.
- Latent ONNX output = `sensor_lag_0 + xgb(x)`; residual target = `next sensor - physics one-step sensor`.

## Backtest score (bundle.metrics.backtest)

Per heat-up cycle (valve >= 50 % after >= 30 min below, window 240 min): roll out the variant from the cycle start with the recorded valve plan and weather, RMSE of predicted vs measured sensor (`grey_box`, `residual`); `*_air` = RMSE of predicted vs the variant's own air estimate. Only cycles whose whole window lies in the held-out days/blocks count. The grey-box physics and the sensor offset used for the backtest are fitted on the training days only (decay labels never span a train/test edge). If a room has no such cycle, all cycles are used and the entry carries `in_sample: true`; heatd ignores in-sample entries for the live gate (>= 3 out-of-sample cycles are required).

## Live switch time

On a live cycle the switch time is `applied = planned + offset` (minutes from cycle start), `offset` uniform in +-`explore_min`, drawn once per cycle. Replans move `planned` only while the elapsed time is below the last planned value; after that `applied` is fixed, and the setpoint is switched to the target as soon as `elapsed >= applied` (also for a positive offset).

## Cycle metrics

Per cycle and variant air estimate: overshoot K = `max(0, peak - target)`, minutes to target +-0.3 K (first `air >= target - 0.3`), minutes valve > 50 %.
