import sys
from heatml.data import REPO_ROOT, Snapshot
from heatml.room import RoomClassifier

CONFIG = {
    "snapshot": None,
    "model_dir": REPO_ROOT / "models",
    "room": {
        "rssi_floor": -100.0, "rssi_span": 70.0,
        "min_room_fraction": 0.5, "min_scans": 10, "max_flip_rate": 0.35, "max_features": 64,
        "cv_blocks": 5, "logreg_c": 1.0, "forest_trees": 100, "forest_depth": 8, "seed": 0,
    },
    "synth": {
        "layout_seed": 7, "rooms": ["wohnzimmer", "schlafzimmer", "bad", "kueche"], "n_aps": 12,
        "scans_per_room": 150, "dropout": 0.08, "sensitivity": -92, "noisy_fraction": 0.15,
        "transient_prob": 0.3, "interval_s": 10.0,
        "trajectory": {
            "rooms": ["wohnzimmer", "schlafzimmer", "bad", "kueche", "flur"], "extra": ["keller"], "walk_place": "flur",
            "adjacent": [["wohnzimmer", "kueche"]], "adjacent_aps": 5, "adjacent_offset": 18.0,
            "days": 5, "stays_per_day": 12, "day_start_h": 7, "dwell_min": [20, 90], "walk_scans": [6, 12],
            "dwell_interval_s": 600.0, "walk_interval_s": 30.0, "labelled_per_room": 4, "extra_weight": 0.35,
        },
        "temperature": {"n": 240, "interval_s": 30.0, "base": 21.0, "noise": 0.03, "drop_per_min": 0.6},
    },
    "window": {"window_s": 300.0, "min_points": 5},
}


def train(config):
    clf = RoomClassifier(config["room"]).fit(Snapshot(config["snapshot"]).scans())
    onnx_path, _ = clf.export(config["model_dir"] / "room_model.onnx")
    print(clf.report)
    print(onnx_path)
    return clf


COMMANDS = {"train": train}


def main(argv):
    if len(argv) < 2 or argv[1] not in COMMANDS:
        sys.exit(f"usage: python -m heatml {{{'|'.join(COMMANDS)}}}")
    COMMANDS[argv[1]](CONFIG)


if __name__ == "__main__":
    main(sys.argv)
