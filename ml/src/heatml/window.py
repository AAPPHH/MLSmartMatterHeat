import numpy as np


class SlopeFeatures:
    def __init__(self, cfg):
        self.cfg = cfg

    def extract(self, readings):
        ts, node, value = readings["ts"], readings["node"], readings["value"]
        rows = []
        for n in np.unique(node):
            m = node == n
            t, v = ts[m], value[m]
            order = np.argsort(t)
            t, v = t[order], v[order]
            for i in range(len(t)):
                w = (t > t[i] - self.cfg["window_s"]) & (t <= t[i])
                if w.sum() >= self.cfg["min_points"]:
                    rows.append((int(n), float(t[i]), float(np.polyfit((t[w] - t[i]) / 60.0, v[w], 1)[0])))
        return rows

    def matrix(self, rows):
        return np.array([[r[2]] for r in rows], dtype=float)


class ThresholdWindowDetector:
    def __init__(self, threshold=-0.2):
        self.threshold = threshold

    def fit(self, X, y):
        X, y = np.asarray(X, float).ravel(), np.asarray(y, bool)
        if y.any() and (~y).any():
            self.threshold = float((X[y].mean() + X[~y].mean()) / 2)
        return self

    def predict(self, X):
        return np.asarray(X, float).ravel() < self.threshold

    def export_onnx(self, path):
        raise NotImplementedError
