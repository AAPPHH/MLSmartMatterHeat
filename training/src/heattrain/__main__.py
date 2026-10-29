import sys
from pathlib import Path
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
}


def run_train(config, data_root, models_dir):
    print(train(config, data_root, models_dir))


def run_eval(config, data_root, model_dir):
    m = eval_model(config, data_root, model_dir)
    print({k: round(m["smoothed"][k], 3) for k in ("coverage", "accuracy", "macro_f1")})


COMMANDS = {"watchroom": run_train, "eval": run_eval}


def main(argv):
    if len(argv) != 4 or argv[1] not in COMMANDS:
        sys.exit("usage: python -m heattrain watchroom <data_root> <models_dir> | eval <data_root> <model_dir>")
    COMMANDS[argv[1]](CONFIG, argv[2], argv[3])


if __name__ == "__main__":
    main(sys.argv)
