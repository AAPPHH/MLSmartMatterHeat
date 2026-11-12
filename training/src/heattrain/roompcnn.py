import math
import numpy as np

FEATURES = ("sin_hour", "cos_hour", "weekend", "radiation", "outdoor", "present", "co2_rate", "co2_avail", "hum_rate", "hum_avail", "coffee")
REQUIRED = ("sin_hour", "cos_hour", "weekend", "radiation", "outdoor")
DOC_KEYS = ("qmax", "features", "mu", "sd", "w1", "b1", "w2", "b2")
HIDDEN_MAX = 64


def finite_all(xs):
    return all(isinstance(v, (int, float)) and math.isfinite(v) for v in xs)


def feature_row(hour, weekend, rad, tout, present, co2_rate, hum_rate, coffee):
    ang = 2.0 * math.pi * hour / 24.0
    opt = lambda v: (v, 1.0) if math.isfinite(v) else (0.0, 0.0)
    (co2, co2_ok), (hum, hum_ok) = opt(co2_rate), opt(hum_rate)
    flag = lambda v: 1.0 if v > 0.5 else 0.0
    return [math.sin(ang), math.cos(ang), flag(weekend), rad / 100.0, tout, flag(present) if math.isfinite(present) else 0.0, co2, co2_ok, hum, hum_ok, flag(coffee) if math.isfinite(coffee) else 0.0]


def feature_matrix(hour, weekend, rad, tout, present, co2_rate, hum_rate, coffee):
    ang = 2.0 * np.pi * np.asarray(hour, float) / 24.0
    flag = lambda v: np.where(np.nan_to_num(np.asarray(v, float)) > 0.5, 1.0, 0.0)
    ok = lambda v: np.isfinite(np.asarray(v, float))
    zero = lambda v: np.where(ok(v), v, 0.0)
    return np.stack([np.sin(ang), np.cos(ang), flag(weekend), np.asarray(rad, float) / 100.0, np.asarray(tout, float), flag(present), zero(co2_rate), ok(co2_rate).astype(float), zero(hum_rate), ok(hum_rate).astype(float), flag(coffee)], -1)


def check_pcnn_doc(name, doc):
    bad = lambda what: ValueError(f"room model: invalid pcnn {what} for {name}")
    if not isinstance(doc, dict) or not set(DOC_KEYS) <= set(doc):
        raise bad("block")
    if list(doc["features"]) != list(FEATURES):
        raise bad("feature list")
    n, hidden = len(FEATURES), len(doc["w2"])
    if not (isinstance(doc["qmax"], (int, float)) and math.isfinite(doc["qmax"]) and doc["qmax"] > 0.0):
        raise bad("qmax")
    if not 1 <= hidden <= HIDDEN_MAX or len(doc["b1"]) != hidden or len(doc["w1"]) != hidden or any(len(r) != n for r in doc["w1"]) or len(doc["mu"]) != n or len(doc["sd"]) != n:
        raise bad("shape")
    if not (finite_all(doc["mu"]) and finite_all(doc["sd"]) and all(v > 0.0 for v in doc["sd"]) and finite_all(doc["b1"]) and finite_all(doc["w2"]) and finite_all([doc["b2"]]) and all(finite_all(r) for r in doc["w1"])):
        raise bad("weights")


class Pcnn:
    def __init__(self, doc, name="room"):
        check_pcnn_doc(name, doc)
        self.doc = doc
        self.qmax, self.b2 = float(doc["qmax"]), float(doc["b2"])
        self.mu, self.sd, self.w1, self.b1, self.w2 = (np.asarray(doc[k], float) for k in ("mu", "sd", "w1", "b1", "w2"))
        self.w1_rows = [list(map(float, r)) for r in doc["w1"]]

    def q(self, x):
        if not finite_all(x):
            return 0.0
        z = [(v - m) / s for v, m, s in zip(x, self.mu.tolist(), self.sd.tolist())]
        out = self.b2
        for row, b, w in zip(self.w1_rows, self.b1.tolist(), self.w2.tolist()):
            acc = b
            for wi, zi in zip(row, z):
                acc += wi * zi
            out += w * math.tanh(acc)
        return self.qmax * math.tanh(out)

    def q_batch(self, x):
        x = np.asarray(x, float)
        good = np.isfinite(x).all(-1)
        z = (np.where(good[:, None], x, 0.0) - self.mu) / self.sd
        return np.where(good, self.qmax * np.tanh(np.tanh(z @ self.w1.T + self.b1) @ self.w2 + self.b2), 0.0)


def pcnn_gate(report, gate):
    pooled, rooms = report["pooled"], report["rooms"]
    out = {"active": True, "pooled": {}, "rooms": {}, "reasons": []}
    for h in gate["horizons"]:
        b = pooled[f"H{h}"]
        gain = None if b.get("pcnn_skill") is None or b.get("skill") is None else b["pcnn_skill"] - b["skill"]
        out["pooled"][f"H{h}"] = gain
        if gain is None or gain < gate["min_gain"]:
            out["reasons"].append(f"pooled H{h} gain {'n/a' if gain is None else f'{100 * gain:.2f}'} points is below {100 * gate['min_gain']:.1f}")
        for r, blocks in rooms.items():
            rb = blocks.get(f"H{h}") or {}
            if rb.get("pcnn_skill") is None or rb.get("skill") is None:
                continue
            g = rb["pcnn_skill"] - rb["skill"]
            out["rooms"].setdefault(r, {})[f"H{h}"] = g
            if g < -gate["max_room_loss"]:
                out["reasons"].append(f"{r} H{h} loses {-100 * g:.2f} points, more than {100 * gate['max_room_loss']:.1f}")
    out["active"] = not out["reasons"]
    return out
