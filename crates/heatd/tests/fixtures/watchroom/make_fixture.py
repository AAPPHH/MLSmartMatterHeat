import json
from pathlib import Path
import numpy as np
from skl2onnx import convert_sklearn
from skl2onnx.common.data_types import FloatTensorType
from sklearn.neighbors import KNeighborsClassifier

OUT = Path(__file__).parent / "v1"
ROOMS = ["wohnzimmer", "schlafzimmer", "bad"]
SENDERS = [f"aa:bb:cc:00:00:0{i}" for i in range(1, 5)]
CENTERS = {"wohnzimmer": [-40, -60, -90, -100], "schlafzimmer": [-75, -45, -60, -100], "bad": [-80, -80, -45, -55]}
FLOOR, SPAN = -100, 70
LINKS = [("wohnzimmer", "bad", "open"), ("bad", "schlafzimmer", "wall")]
STAY = 0.9


def scale(rssi):
    return np.clip((np.asarray(rssi, dtype=np.float32) - FLOOR) / SPAN, 0, 1)


def transition():
    linked = {frozenset(l[:2]) for l in LINKS}
    out = {}
    for a in ROOMS:
        weights = {b: (3.0 if frozenset((a, b)) in linked else 1.0) for b in ROOMS if b != a}
        total = sum(weights.values())
        out[a] = {a: STAY, **{b: round((1 - STAY) * w / total, 6) for b, w in weights.items()}}
    return out


def dataset():
    rng = np.random.default_rng(7)
    x, y = [], []
    for i, room in enumerate(ROOMS):
        for _ in range(12):
            x.append(scale(np.array(CENTERS[room]) + rng.integers(-2, 3, 4)))
            y.append(i)
    q = np.array([-58, -52, -75, -100], dtype=float)
    for k in range(3):
        x.append(scale(q + [0.1 * k, 0, 0, 0]))
        y.append(0)
    for k in range(2):
        x.append(scale(q + [-0.1 * (k + 1), 0, 0, 0]))
        y.append(1)
    return np.array(x, dtype=np.float32), np.array(y)


def export(x, y, name):
    clf = KNeighborsClassifier(n_neighbors=5).fit(x, y)
    onx = convert_sklearn(clf, initial_types=[("x", FloatTensorType([None, len(SENDERS)]))], options={id(clf): {"zipmap": False}})
    (OUT / name).write_bytes(onx.SerializeToString())


def main():
    OUT.mkdir(parents=True, exist_ok=True)
    x, y = dataset()
    export(x, y, "all.onnx")
    export(x, (y + 1) % len(ROOMS), "sleep.onnx")
    meta = {
        "model_version": "v1", "created": "2026-10-29T12:00:00+00:00", "matcher": "room", "rooms": ROOMS, "senders": SENDERS, "sentinel_dbm": -100,
        "scale": {"floor": FLOOR, "span": SPAN}, "states": {"all": "all.onnx", "sleep": "sleep.onnx"}, "tau_room": 0.7, "delta_room": 0.3, "t_hold_min": 10,
        "smoothing": {"kind": "hmm", "stay_prob": STAY, "transition": transition()},
        "metrics": {"fixture": True}, "train_window": {"from": "2026-10-01", "to": "2026-10-28"}, "n_labels": {r: 12 for r in ROOMS},
    }
    (OUT / "model.json").write_text(json.dumps(meta, indent=1))


main()
