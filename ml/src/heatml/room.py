import json
from pathlib import Path
import numpy as np
from sklearn.ensemble import RandomForestClassifier
from sklearn.linear_model import LogisticRegression
from sklearn.metrics import confusion_matrix
from sklearn.model_selection import GroupKFold
from skl2onnx import convert_sklearn
from skl2onnx.common.data_types import FloatTensorType


def vectorize(aps, bssids, floor, span):
    f = len(bssids)
    out = np.zeros(2 * f, dtype=np.float32)
    for i, b in enumerate(bssids):
        rssi = aps.get(b)
        if rssi is not None:
            out[i] = min(max((rssi - floor) / span, 0.0), 1.0)
            out[f + i] = 1.0
    return out


class FeatureSelector:
    def __init__(self, cfg):
        self.cfg = cfg

    def select(self, scans):
        scans = sorted(scans, key=lambda s: s.ts)
        rooms = sorted({s.label for s in scans})
        stats = {}
        for b in sorted({b for s in scans for b in s.aps}):
            total = sum(b in s.aps for s in scans)
            best_frac, best_flip = 0.0, 0.0
            for r in rooms:
                seen = np.array([b in s.aps for s in scans if s.label == r], dtype=int)
                if seen.mean() > best_frac:
                    best_frac = seen.mean()
                    best_flip = float(np.abs(np.diff(seen)).mean()) if len(seen) > 1 else 0.0
            stats[b] = (best_frac, best_flip, total)
        keep = [b for b, (frac, flip, total) in stats.items()
                if frac >= self.cfg["min_room_fraction"] and total >= self.cfg["min_scans"] and flip <= self.cfg["max_flip_rate"]]
        keep.sort(key=lambda b: (-stats[b][0], -stats[b][2], b))
        return sorted(keep[: self.cfg["max_features"]])


class RoomClassifier:
    def __init__(self, cfg):
        self.cfg = cfg
        self.bssids = []
        self.labels = []
        self.model = None
        self.report = {}

    def vectorize(self, aps):
        return vectorize(aps, self.bssids, self.cfg["rssi_floor"], self.cfg["rssi_span"])

    def matrix(self, scans):
        return np.stack([self.vectorize(s.aps) for s in scans])

    def candidates(self):
        c = self.cfg
        return {
            "logreg": lambda: LogisticRegression(max_iter=2000, C=c["logreg_c"]),
            "forest": lambda: RandomForestClassifier(n_estimators=c["forest_trees"], max_depth=c["forest_depth"], random_state=c["seed"]),
        }

    def block_groups(self, scans):
        groups = np.zeros(len(scans), dtype=int)
        for r in self.labels:
            idx = [i for i, s in enumerate(scans) if s.label == r]
            for pos, i in enumerate(idx):
                groups[i] = pos * self.cfg["cv_blocks"] // len(idx)
        return groups

    def fit(self, scans):
        scans = sorted(scans, key=lambda s: s.ts)
        self.bssids = FeatureSelector(self.cfg).select(scans)
        self.labels = sorted({s.label for s in scans})
        X = self.matrix(scans)
        y = np.array([self.labels.index(s.label) for s in scans])
        groups = self.block_groups(scans)
        cv = GroupKFold(n_splits=self.cfg["cv_blocks"])
        scores, preds = {}, {}
        for name, make in self.candidates().items():
            pred = np.zeros_like(y)
            for tr, te in cv.split(X, y, groups):
                pred[te] = make().fit(X[tr], y[tr]).predict(X[te])
            scores[name], preds[name] = float((pred == y).mean()), pred
        best = max(scores, key=scores.get)
        self.model = self.candidates()[best]().fit(X, y)
        self.report = {"cv_accuracy": scores, "best": best, "labels": self.labels, "n_features": len(self.bssids),
                       "confusion": confusion_matrix(y, preds[best]).tolist()}
        return self

    def predict(self, scans):
        return [self.labels[i] for i in self.model.predict(self.matrix(scans))]

    def export(self, onnx_path, json_path=None):
        onnx_path = Path(onnx_path)
        json_path = Path(json_path) if json_path else onnx_path.with_suffix(".json")
        onnx_path.parent.mkdir(parents=True, exist_ok=True)
        model = convert_sklearn(self.model, initial_types=[("input", FloatTensorType([None, 2 * len(self.bssids)]))],
                                options={id(self.model): {"zipmap": False}}, target_opset={"": 15, "ai.onnx.ml": 3})
        onnx_path.write_bytes(model.SerializeToString())
        json_path.write_text(json.dumps({"version": 1, "input_name": "input", "bssids": self.bssids, "labels": self.labels,
                                         "rssi_floor": self.cfg["rssi_floor"], "rssi_span": self.cfg["rssi_span"]}, indent=2))
        return onnx_path, json_path
