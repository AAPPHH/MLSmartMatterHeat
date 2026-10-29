import sys
from pathlib import Path
from heattrain.thermal import thermal
from heattrain.watchroom import eval_model, train

CONFIG = {
    "timezone": "Europe/Berlin",
    "eval_dir": Path(__file__).resolve().parents[2] / "eval",
    "sources": ["watch", "manual"],
    "label_tolerance_s": 300.0,
    "state": {"hr_rest_max": 70.0, "hr_active_min": 90.0, "motion_active_min": 0.5, "sleep_hours": [23, 7], "window_s": 600.0, "stale_s": 600.0},
    "scale": {"floor": -100, "span": 70},
    "t_hold_min": 30,
    "min_sender_scans": 10, "min_sender_days": 2, "max_senders": 64,
    "min_room_scans": 20, "min_state_scans": 60, "min_state_room_scans": 10,
    "candidates": ["knn", "gb"], "knn_k": 7, "gb_trees": 60, "gb_depth": 3, "seed": 0,
    "cv_folds": 4, "min_coverage": 0.8, "holdout_frac": 0.3,
    "hmm": {"stay_prob": 0.95, "weight_link": {"wall": 6.0, "open": 8.0, "ceiling": 3.0}, "weight_other": 1.0,
            "default_links": [["keiner", "flur", "open"], ["keiner", "wohnzimmer", "open"]]},
    "parity_rows": 500, "parity_tol": 1e-5,
    "thermal": {
        "timezone": "Europe/Berlin", "dt_s": 60.0, "night_hours": [22, 5], "min_night_steps": 120, "temp_valid": [0.0, 45.0],
        "min_hours": 3.0, "exclude_sources": ["legacy"], "holdout_frac": 0.25, "min_holdout_hours": 3.0, "horizon_min": 30, "origin_stride": 5,
        "noise_r": 0.15, "q_r": 0.5, "q_a": 0.05, "p0": 1.0, "prior_sigma": 3.0, "max_nfev": 60, "mc_samples": 8, "mc_seed": 0, "std_max": 1.0, "rc2_margin": 1.02,
        "min_valve_std": 0.03, "gain_err_max": 5.0, "valve_gain_default": 0.5, "valve_gain_bounds": [0.05, 2.0],
        "params": {"a1": [6.0, 0.05, 40.0], "a2": [2.0, 0.05, 30.0], "b1": [1.0, 0.02, 30.0], "b2": [0.15, 0.005, 3.0], "b3": [0.03, 1e-4, 1.0],
                   "b4": [0.01, 1e-4, 0.5], "w": [0.7, 0.05, 0.99], "ts": [50.0, 25.0, 75.0], "ts_night": [45.0, 20.0, 75.0], "c": [0.2, 1e-3, 5.0]},
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


COMMANDS = {"watchroom": run_train, "eval": run_eval, "thermal": run_thermal}


def main(argv):
    if len(argv) != 4 or argv[1] not in COMMANDS:
        sys.exit("usage: python -m heattrain watchroom <data_root> <models_dir> | eval <data_root> <model_dir> | thermal <data_root_or_raw_dir> <models_dir>")
    COMMANDS[argv[1]](CONFIG, argv[2], argv[3])


if __name__ == "__main__":
    main(sys.argv)
