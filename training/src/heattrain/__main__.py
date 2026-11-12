import sys
from pathlib import Path
from heattrain.boiler import boiler
from heattrain.boostlearn import boostcheck, boostlearn, boostmark
from heattrain.thermal import thermal
from heattrain.roomeval import roommodel
from heattrain.thermalml import thermalml
from heattrain.watchroom import eval_model, train
from heattrain.windowml import run_windowml, run_windowroom, window_bundle

CONFIG = {
    "timezone": "Europe/Berlin",
    "eval_dir": Path(__file__).resolve().parents[2] / "eval",
    "sources": ["watch", "manual", "toothbrush"], "interval_sources": ["toothbrush"],
    "label_tolerance_s": 30.0, "label_intervals": False, "anchor_devices": ["minix-anchor"], "anchor_drift_db": 6.0,
    "state": {"hr_rest_max": 70.0, "hr_active_min": 90.0, "motion_active_min": 0.5, "sleep_hours": [23, 7], "window_s": 600.0, "stale_s": 600.0},
    "scale": {"floor": -100, "span": 70},
    "t_hold_min": 30,
    "min_sender_scans": 10, "min_sender_days": 1, "max_senders": 64,
    "min_room_scans": 5, "min_state_scans": 60, "min_state_room_scans": 10,
    "candidates": ["xgb"], "xgb_trees": 100, "xgb_depth": 3, "xgb_lr": 0.1, "labels_from": "2026-10-29T20:28:00+01:00", "tap_labels_from": "2026-10-30T09:00:00+01:00", "noise_p_min": 0.05, "noise_p_other": 0.8, "noise_min_labels": 8, "svm_c": 10.0, "svm_gamma": "scale", "knn_k": 7, "gb_trees": 60, "gb_depth": 3, "seed": 0,
    "cv_folds": 4, "min_coverage": 0.8, "holdout_frac": 0.3,
    "hmm": {"stay_prob": 0.95, "weight_link": {"wall": 6.0, "open": 8.0, "ceiling": 3.0}, "weight_other": 1.0,
            "default_links": [["keiner", "flur", "open"], ["keiner", "wohnzimmer", "open"]]},
    "feature_version": 2, "history_s": 300.0, "history_scans": 5,
    "parity_rows": 500, "parity_tol": 1e-5,
    "thermal": {
        "timezone": "Europe/Berlin", "dt_s": 60.0, "night_hours": [22, 5], "min_night_steps": 120, "temp_valid": [0.0, 45.0],
        "min_hours": 3.0, "exclude_sources": ["legacy"], "holdout_frac": 0.25, "min_holdout_hours": 3.0, "horizon_min": 30, "origin_stride": 5,
        "noise_r": 0.15, "q_r": 0.5, "q_a": 0.05, "p0": 1.0, "prior_sigma": 3.0, "max_nfev": 60, "mc_samples": 8, "mc_seed": 0, "std_max": 1.0, "rc2_margin": 1.02,
        "outage": {"silence_min": 30, "restart_pad_min": 5, "stale_s": 3600.0}, "device_stale_s": 900.0,
        "min_valve_std": 0.03, "gain_err_max": 5.0, "valve_gain_default": 0.5, "valve_gain_bounds": [0.05, 2.0],
        "params": {"a1": [6.0, 0.05, 40.0], "a2": [2.0, 0.05, 30.0], "b1": [1.0, 0.02, 30.0], "b2": [0.15, 0.005, 3.0], "b3": [0.03, 1e-4, 1.0],
                   "b4": [0.01, 1e-4, 0.5], "w": [0.7, 0.05, 0.99], "ts": [50.0, 25.0, 75.0], "ts_night": [45.0, 20.0, 75.0], "c": [0.2, 1e-3, 5.0]},
    },
    "thermalml": {
        "history_min": 60, "lag_step_min": 5, "boost_cap_min": 240, "boost_valve": 0.5, "others_valve": 0.05, "ambient_stale_s": 900.0,
        "cold_valve": 0.02, "cold_min": 60, "min_valid_hours": 24.0, "min_train_hours": 12.0, "valid_max_persistence_ratio": 1.0, "chunk": 20000,
        "decay_valve_from": 0.3, "decay_lookback_min": 10, "decay_min_min": 30, "decay_max_min": 90, "decay_min_points": 8, "decay_tau": [3.0, 90.0], "decay_drift_k_per_min": 0.02, "decay_max_rmse": 0.2, "decay_min_amp": 0.05,
        "min_latent_labels": 200, "min_residual_rows": 1000, "max_abs_target": 2.0, "residual_feedback": 0.25, "feedback_grid": [0.0, 0.25, 0.5, 1.0], "seed": 0,
        "xgb_latent": {"n_estimators": 150, "max_depth": 4, "learning_rate": 0.08, "subsample": 0.8, "min_child_weight": 5},
        "xgb_residual": {"n_estimators": 80, "max_depth": 3, "learning_rate": 0.05, "subsample": 0.8, "min_child_weight": 20},
        "holdout_frac": 0.25, "min_split_days": 4, "block_min": 360, "eval_horizons": [30, 60, 120], "eval_stride": 15,
        "cycle_closed_min": 30, "cycle_window_min": 240, "cycle_min_len": 60, "target_tol": 0.3,
        "horizon_min": 300, "toff_max": 180, "toff_step": 1, "overshoot_tol": 0.3, "boost_setpoint": 28.0,
        "parity_rows": 400, "parity_tol": 1e-4, "parity_nan_frac": 0.05,
    },
    "roommodel": {
        "hold_min": 20.0, "valve": {"sigma_u_min": 0.05, "sigma_u_default": 0.25},
        "fit": {"horizons": [5, 20, 60], "weights": [1.0, 1.0, 0.5], "stride": 5, "warm_min": 1440, "train_frac": 0.6, "report_age_min": 5, "max_nfev": 200, "tol": 1e-6, "diff_step": 1e-3, "bound_tol": 0.01,
                "prior_sigma": {"default": 3.0, "ts": 0.3, "ts_night": 0.3}, "q_r": 0.5, "q_a": 0.3, "q_um": 8.0, "q_ua": 0.0, "valve_sigma_scale": 0.0, "init_onestep": True, "init_w": [0.05, 0.95], "pool": ["ts", "ts_night"], "setpoint_pct": 95.0, "min_setpoint_n": 60, "guard_horizon": 60, "guard_min_n": 200,
                "init": {"a1": 6.0, "a2": 2.0, "b1": 1.0, "b2": 0.15, "b3": 0.03, "b4": 0.01, "w": 0.5, "ts": 50.0, "ts_night": 45.0, "tau_v": 10.0, "c": 0.2},
                "bounds": {"a1": [0.01, 300.0], "a2": [0.01, 300.0], "b1": [0.01, 300.0], "b2": [1e-3, 10.0], "b3": [1e-5, 3.0], "b4": [1e-5, 5.0], "w": [1e-3, 0.999], "ts": [20.0, 95.0], "ts_night": [15.0, 95.0],
                           "tau_v": [0.1, 120.0], "c": [1e-4, 20.0]}},
        "shock": {"tau_l": 90.0, "hold_min": 20.0, "p_l0": 0.01, "lambda_jump": 3.0, "lambda_w": 2.0, "start": {"q_calm": 0.01, "p_jump": 1e-4, "q_jump": 0.3, "tau_l": 90.0}, "passes": 2, "batch_max": 64, "days": 14, "reset_cost": 3.0,
                  "grid": {"q_calm": [0.003, 0.01, 0.03], "q_jump": [0.3, 0.5, 0.8, 1.2], "p_jump": [3e-5, 1e-4, 3e-4, 1e-3], "tau_l": [30.0, 60.0, 90.0, 180.0]},
                  "label": {"fa_cost": 20.0, "event_weight": 15.0, "pw_floor": 0.02},
                  "lambda_w_grid": [1.5, 2.0, 2.5, 3.0], "hit_min": 20, "pad_min": 30, "fa_weight": 1.0},
        "eval": {"folds": [0.45, 0.6, 0.75], "sensor_split": 0.5, "min_sensor_min": 600, "warm_days": 3, "floor": 0.5 / 12 ** 0.5, "decay_lam": True, "pw_flag": 0.5, "min_episode_min": 5,
                 "shock_variants": [{"q_calm": 0.003, "p_jump": 3e-4, "q_jump": 0.8, "tau_l": 90.0}]},
        "pcnn": {"hidden": 8, "seed": 0, "q0": 0.3, "q_sigma": 0.7, "prior_w": 0.02, "wd": 0.02, "max_iter": 150, "patience": 15, "val_frac": 0.25, "min_n": 300, "horizons": [20, 60], "weights": [1.0, 1.0], "presence_min": 30.0, "coffee_on_mw": 50000.0,
                  "gate": {"horizons": [20, 60], "min_gain": 0.01, "max_room_loss": 0.01}},
        "criteria": {"s1_h20": 0.08, "s1_h60": 0.10, "s2": 0.5, "s2_stretch": 0.2, "s4_resets": 20},
        "house": {"p_up": 0.02, "p_down": 0.001, "p_supply0": 0.95, "r_grid": 0.25 / 12.0, "reset_gap_min": 240.0, "reset_sigma": 8.0, "reset_n": 3, "boost_c": 24.0, "balance_seed": 1, "trv_grid": 0.5, "trv_floor": 0.25},
        "evidence": {"air": {"noise": 0.15, "offset": 0.0, "min_pairs": 60}, "effort": {"sigma": 0.06, "steady_u": 0.05},
                     "select": {"enabled": True, "min_gain": 0.003, "max_resets_up": 10, "effort": [{"sigma": 0.06, "steady_u": 0.05}, {"sigma": 0.25, "steady_u": 0.15}]},
                     "co2": {"out": 420.0, "min_excess": 150.0, "k": 0.004, "gain": 4.0, "sigma": 4.0, "sigma_up": 12.0, "win_min": 10},
                     "hum": {"rh_out": 85.0, "min_excess": 0.8, "k": 0.004, "gain": 4.0, "sigma": 0.03, "sigma_up": 0.3, "win_min": 10}},
    },
    "window": {
        "lake": "data/lake",
        "rooms": ["wohnzimmer", "schlafzimmer", "bad", "kueche"], "nodes": {"wohnzimmer": 5, "schlafzimmer": 6, "bad": 7, "kueche": 8},
        "utc_offset_min": 60, "valid": {"temp": [0.0, 45.0], "valve": [0.0, 100.0]}, "ffill_min": {"temp": 10, "valve": 30, "setpoint": 360}, "weather_max_age_min": 180,
        "open_valve_pct": 10.0, "v1_open_cap_min": 40, "frost_max_c": 8.5, "frost_cap_min": 360,
        "v1_like_min": 10, "floor": {"window_min": 180, "settled_min": 20, "ground": ["wohnzimmer", "kueche", "bad"]},
        "criterion": {"exclude_utc": ["2026-11-07T07:30:00+00:00", "2026-11-07T10:00:00+00:00"], "hit_utc": ["2026-11-07T08:05:00+00:00", "2026-11-07T08:40:00+00:00"],
                      "gt_channels": ["house", "wohnzimmer", "kueche", "bad"], "rate": 0.5, "report_rate": 3.0, "merge_min": 30, "top_k": 3, "list_n": 15, "max_candidates": 1500},
        "phys": {"horizon": 20, "taus": [4.0, 12.0, 40.0], "ridge": 1.0, "trim": 0.03, "min_rows": 500, "sigma_floor": 0.15, "since_open_cap": 90.0, "valve_guard_min": 120,
                 "veto_pad_min": 60, "weights": {"wohnzimmer": 1.0, "kueche": 1.0, "bad": 1.0, "schlafzimmer": 0.3}, "clip": 6.0},
        "presence": {"age_cap_min": 240, "sleep_room": "schlafzimmer", "asleep_age_min": 45, "in_room_min": 10, "home_min": 60},
        "room": {
            "gt": [{"id": "A", "open_utc": "2026-11-07T08:05:00+00:00", "hit_min": 20, "channels": ["house", "bad"], "exclude_utc": ["2026-11-07T07:30:00+00:00", "2026-11-07T10:00:00+00:00"]},
                   {"id": "B", "open_utc": "2026-11-07T17:27:00+00:00", "hit_min": 20, "channels": ["wohnzimmer"], "exclude_utc": ["2026-11-07T17:20:00+00:00", "2026-11-07T18:30:00+00:00"]}],
            "hit_min": 20, "gt_pad_min": 35, "gt_open_min": 60, "margin": 0.9, "ratio_quantile": 0.995, "merge_min": 30, "house_clip": 6.0, "sigma_trim": 0.03, "sigma_floor": 0.15, "list_n": 1000,
            "models": {"ridge": ["PhysicsResidual", False], "xgb": ["XgbResidual", False], "ridge_react": ["PhysicsResidual", True], "xgb_react": ["XgbResidual", True]},
            "react": {"enabled": False, "horizon": 5, "sp_hold": 30, "sigma_floor": 5.0, "trim": 0.03},
            "other_open_pct": 30.0,
            "feat": {"dose": [10, 20, 30, 90], "slope": [5, 10, 20], "floor_window": 180, "floor_settled": 20, "age_cap": 240, "step_k": 0.25, "dv_min": 5},
            "xgb": {"n_estimators": 300, "max_depth": 4, "learning_rate": 0.05, "subsample": 0.8, "colsample_bytree": 0.7, "min_child_weight": 30, "reg_lambda": 5.0, "trim": 0.03, "pooled": True},
        },
    },
    "boost": {
        "seed": 0, "keep_bundles": 14, "dead_zone": 0.25, "strong_dose": 4.0, "ramp_tau_min": 10.0, "merge_gap_min": 3, "pulse_cap_min": 30, "min_dose": 1.0, "pre_min": 15, "post_min": 90, "min_window_min": 20, "min_pulse_tail_min": 10, "loss_scale": 3.0, "kappa_max": 4.0, "min_temp_frac": 0.7, "max_temp_gap_min": 20,
        "kernel_len_min": 120, "kernel_shape": 2.0, "horizon_min": 240, "closed_hold_min": 240, "since_open_cap_min": 360, "resume_cap_min": 180, "setpoint_drop_k": 1.0, "cooldown_min": 30, "out_default": 5.0, "wind_default": 2.0,
        "morning_jump_k": 1.0, "morning_hours": [5.0, 11.0], "home_hours": [11.0, 23.5], "home_jump_k": 2.0, "away_max_c": 18.5, "homecoming_min_away_min": 60, "scenario_pre_min": 5, "scenario_post_min": 90, "boost_match_min": 5, "known_overshoot_k": 0.5, "after_window_min": 180, "window_hold_min": 20, "window_manual_max_min": 720,
        "passive_step_min": 10, "passive_min_points": 200, "passive_a_bounds": [1e-4, 0.02], "passive_delta_bounds": [0.0, 15.0], "passive_default": {"a": 0.003, "delta": 5.0},
        "gain_prior": [0.3, 1.0], "delay_prior": [12.0, 0.7], "gain_bounds": [1e-3, 5.0], "delay_bounds": [2.0, 80.0], "sigma0": 0.3, "sigma_floor": 0.2, "boost_weight": 2.0,
        "eb_iter": 3, "hyper_min_windows": 2, "hyper_shrink": 2.0, "tau_floor": 0.15, "n_samples": 64, "peak_sigma_floor": 0.3,
        "dose_min": 5.0, "dose_max": 20.0, "dose_step": 0.1, "p_max": 0.10, "margin_k": 0.5, "thompson_margin_k": 0.25, "boost_setpoint": 24.0, "abort_c": 24.0, "growth": 1.1,
        "knn_k": 5, "radius_quantile": 0.25, "radius_default": 2.5, "radius_floor": 0.5, "scenario_penalty": 2.0, "known_post_min": 20,
        "closure_excl_min": 90, "closure_gap": [2.0, 6.0], "closure_min_points": 300, "closure_kp": [0.02, 10.0], "closure_gap0_max": 3.0, "closure_active_gap": 1.0, "closure_min_active": 0, "closure_scales": [0.0, 0.03, 0.06, 0.1, 0.2, 0.35, 0.5, 0.75, 1.0], "closure_tol": 0.03, "closure_min_pulses": 20, "closure_default": {"kp": 1.0, "gap0": 1.0},
        "loeo_min_strong": 6, "sigma_max": 4.0, "sigma_step": 0.05, "coverage_target": 0.9, "bounds_trials": 100, "monotone_trials": 100, "retry": {"base_min": 10, "max_min": 360},
        "feature_weight": {"out": 1.0, "wind": 0.5, "rad": 0.5, "start_temp": 1.0, "gap": 1.0, "supply_ok": 1.0, "since_resume": 0.5, "vm60": 0.5, "vm180": 0.5, "since_open": 0.5, "hour_sin": 0.5, "hour_cos": 0.5},
        "typical": {"morning": {"scenario": "morning", "start": 18.0, "target": 20.0, "out": 5.0, "wind": 3.0, "rad": 0.0, "resume": 60.0, "hour": 6.5},
                    "homecoming": {"scenario": "homecoming", "start": 14.0, "target": 20.0, "out": 5.0, "wind": 3.0, "rad": 0.0, "resume": 180.0, "hour": 17.5}},
    },
    "boiler": {
        "timezone": "Europe/Berlin", "dt_s": 60.0, "temp_valid": [0.0, 95.0], "max_gap_s": 900.0, "invalid_wants_k": 1.0, "night_hours": [21, 6],
        "open_frac": 0.3, "full_open": 0.85, "warm_window_min": 20, "warm_rise_k": 1.0, "merge_gap_min": 10, "supply_hold_min": 15,
        "nosupply_min": 30, "flat_k": 0.6, "wants_min_k": 0.5, "min_nights": 3, "min_days": 3.0,
        "recovery_tol_k": 0.0, "recovery_max_min": 240,
        "fit_max_min": 120, "fit_min_points": 4, "fit_min_rise_k": 3.0, "fall_k": 0.5, "tau_bounds": [2.0, 120.0], "max_extrapolation_k": 15.0, "plateau_k": 0.5, "plateau_gap_k": 1.5,
        "sensor_coupling": 0.6, "coupling_range": [0.45, 0.8], "min_curve_events": 4, "min_verdict_events": 2, "min_curve_span_k": 3.0, "curve_slope_min": 0.3, "flow_high_k": 12.0, "flow_low_k": 10.0,
        "curve_ref": {"indoor": 20.0, "design_out": -12.0, "flow": 70.0, "ret": 55.0, "n": 1.33},
        "group_min": 120, "power_valve_min": 0.5, "power_min_minutes": 10, "power_rel": [0.6, 1.6], "delay_flag_min": 15.0, "invalid_warn": 0.1, "invalid_crit": 0.5,
        "cycle_window_min": 60, "cycle_step_min": 30, "cycle_period_min": [5, 30], "cycle_trend_min": 31, "cycle_frac_min": 0.4, "cycle_sigma_min_k": 0.15, "cycle_min_crossings": 3, "cycle_min_windows": 3, "cycle_share_flag": 0.3,
    },
}


def run_train(config, data_root, models_dir):
    print(train(config, data_root, models_dir))


def run_eval(config, data_root, model_dir):
    m = eval_model(config, data_root, model_dir)
    print({k: round(m["smoothed"][k], 3) for k in ("coverage", "accuracy", "macro_f1")})


def run_thermal(config, src, models_dir):
    docs, out = thermal(config, src, models_dir)
    for d in docs:
        print(d["room"], d["structure"], [round(t) for t in d["tau_min"]], round(d["params"].get("w", 0), 2), round(d["params"]["ts"], 1), d["metrics"][d["structure"]]["rmse_horizon"], d["warnings"][:2])
    print(out)


def run_roommodel(config, src, models_dir):
    out, bundle, report = roommodel(config, src, models_dir)
    print(out, {h: round(v["skill"], 3) for h, v in report["holdout"]["pooled"].items() if "skill" in v}, report["resets"], {k: v["pass"] for k, v in report["criteria"].items()}, "pcnn", report["pcnn"]["active"], report["flags"])


def run_boiler(config, src, out_dir):
    doc, out = boiler(config, src, out_dir)
    print(out, doc["belastbar"], len(doc["befunde"]))


def run_thermalml(config, src, models_dir):
    out, bundle = thermalml(config, src, models_dir)
    print(out, {r: v["valid"] for r, v in bundle["rooms"].items()}, bundle["metrics"]["backtest"])


def run_window(config, name, lake=None):
    run_windowml(config, name, lake)


def run_window_export(config, src, models_dir):
    out, bundle, meta, _, _ = window_bundle(config, src, models_dir)
    print(out, meta["eval"], meta["rows"])


def run_boost(config, src, models_dir, validate=False):
    out, bundle, meta = boostlearn(config, src, models_dir, validate)
    print(out, meta["episodes"], meta["radius"])
    return meta


def run_boostcheck(config, src, models_dir):
    sys.exit(boostcheck(config, src, models_dir))


def run_boostmark(config, models_dir, status):
    print(boostmark(config, models_dir, status == "ok"))


COMMANDS = {"roommodel": run_roommodel, "boostmark": run_boostmark, "boostlearn": run_boost, "boostcheck": run_boostcheck, "window": run_window_export, "windowml": run_window, "watchroom": run_train, "eval": run_eval, "thermal": run_thermal, "thermalml": run_thermalml, "boiler": run_boiler}


def main(argv):
    if len(argv) in (3, 4) and argv[1] == "windowml":
        return run_window(CONFIG, *argv[2:])
    if len(argv) in (2, 3, 4) and argv[1] == "windowroom":
        return run_windowroom(CONFIG, *(argv[3:] or [None]), lake=argv[2] if len(argv) > 2 else None)
    if len(argv) == 5 and argv[1] == "boostlearn":
        return run_boost(CONFIG, argv[2], argv[3], argv[4] == "validate")
    if len(argv) != 4 or argv[1] not in COMMANDS:
        sys.exit("usage: python -m heattrain watchroom <data_root> <models_dir> | eval <data_root> <model_dir> | thermal <data_root_or_raw_dir> <models_dir> | thermalml <data_root_or_raw_dir> <models_dir> | roommodel <lake> <models_dir> | boiler <lake> <out_dir> | boostlearn <lake> <models_dir> [validate] | boostcheck <lake> <models_dir> | boostmark <models_dir> ok|fail | window <lake> <models_dir> | windowml <ClassName|module:Class> [lake] | windowroom [lake] [model,model]")
    COMMANDS[argv[1]](CONFIG, argv[2], argv[3])


if __name__ == "__main__":
    main(sys.argv)
