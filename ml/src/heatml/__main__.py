import sys
from collections import Counter
from heatml.cluster import PlaceClusterer, SemiSupervisedRoom
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
    "cluster": {
        "min_scan_fraction": 0.05, "min_scans": 10, "max_flip_rate": 0.35, "max_features": 64, "miss_penalty": 0.3,
        "jump_window": 3, "jump_threshold": 0.2, "max_gap_s": 900.0, "merge_threshold": 0.09,
        "min_labels": 2, "min_purity": 0.75, "min_cluster_scans": 10, "min_labelled_per_room": 30,
    },
    "synth": {
        "layout_seed": 7, "rooms": ["wohnzimmer", "schlafzimmer", "bad", "kueche"], "n_aps": 12,
        "scans_per_room": 150, "dropout": 0.08, "sensitivity": -92, "noisy_fraction": 0.15,
        "transient_prob": 0.3, "interval_s": 10.0,
        "trajectory": {
            "rooms": ["wohnzimmer", "schlafzimmer", "bad", "kueche", "flur"], "extra": ["keller"], "walk_place": "flur",
            "adjacent": [["wohnzimmer", "kueche"]], "min_separation": 16.0, "adjacent_aps": 5, "adjacent_offset": 18.0,
            "days": 5, "stays_per_day": 12, "day_start_h": 7, "dwell_min": [20, 90], "walk_scans": [6, 12],
            "dwell_interval_s": 600.0, "walk_interval_s": 30.0, "labelled_per_room": 4, "extra_weight": 0.35,
        },
        "temperature": {"n": 240, "interval_s": 30.0, "base": 21.0, "noise": 0.03, "drop_per_min": 0.6},
    },
    "window": {"window_s": 300.0, "min_points": 5},
}


def fit_model(config):
    scans = Snapshot(config["snapshot"]).scans(labelled_only=False)
    per_room = Counter(s.label for s in scans if s.label)
    if per_room and min(per_room.values()) >= config["cluster"]["min_labelled_per_room"]:
        return RoomClassifier(config["room"]).fit([s for s in scans if s.label]), None
    model = SemiSupervisedRoom(config).fit(scans)
    return model.classifier, model.clusterer


def train(config):
    clf, clusterer = fit_model(config)
    onnx_path, _ = clf.export(config["model_dir"] / "room_model.onnx")
    if clusterer:
        print(clusterer.report())
    print(clf.report)
    print(onnx_path)
    return clf


def cluster(config):
    clusterer = PlaceClusterer(config["cluster"], config["room"]).fit(Snapshot(config["snapshot"]).scans(labelled_only=False))
    print(clusterer.report())
    return clusterer


COMMANDS = {"train": train, "cluster": cluster}


def main(argv):
    if len(argv) < 2 or argv[1] not in COMMANDS:
        sys.exit(f"usage: python -m heatml {{{'|'.join(COMMANDS)}}}")
    COMMANDS[argv[1]](CONFIG)


if __name__ == "__main__":
    main(sys.argv)
