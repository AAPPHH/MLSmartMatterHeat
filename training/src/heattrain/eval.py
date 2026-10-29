import json
from pathlib import Path
import numpy as np
import onnxruntime as ort


def vectorize(aps, senders, floor, span):
    return np.array([min(max((aps[s] - floor) / span, 0.0), 1.0) if s in aps else 0.0 for s in senders], dtype=np.float32)


def decide(p, tau, delta):
    order = np.argsort(p)[::-1]
    top, second = float(p[order[0]]), float(p[order[1]]) if len(p) > 1 else 0.0
    if top < tau:
        return int(order[0]), "low_confidence"
    if top - second < delta:
        return int(order[0]), "low_margin"
    return int(order[0]), None


def score(truth, pred, rooms):
    k, truth, pred = len(rooms), np.asarray(truth, dtype=int), np.array([-1 if p is None else p for p in pred], dtype=int)
    n, done = len(truth), pred >= 0
    conf = np.zeros((k, k), dtype=int)
    np.add.at(conf, (truth[done], pred[done]), 1)
    per_room, f1s = {}, []
    for r, name in enumerate(rooms):
        tp, fp, fn = int(conf[r, r]), int(conf[:, r].sum() - conf[r, r]), int(conf[r].sum() - conf[r, r])
        prec = tp / (tp + fp) if tp + fp else 0.0
        rec = tp / (tp + fn) if tp + fn else 0.0
        f1 = 2 * prec * rec / (prec + rec) if prec + rec else 0.0
        per_room[name] = {"precision": prec, "recall": rec, "f1": f1, "accuracy": rec, "support": int(conf[r].sum())}
        if conf[r].sum():
            f1s.append(f1)
    return {"n": n, "coverage": float(done.mean()) if n else 0.0, "accuracy": float(np.trace(conf) / done.sum()) if done.any() else 0.0,
            "macro_f1": float(np.mean(f1s)) if f1s else 0.0, "per_room": per_room, "confusion": conf.tolist(), "rooms": list(rooms)}


def baselines(truth, rooms):
    counts = np.bincount(truth, minlength=len(rooms)) if len(truth) else np.zeros(len(rooms), dtype=int)
    major = int(counts.argmax()) if len(truth) else 0
    return {"no_matcher": {"coverage": 0.0, "accuracy": 0.0, "macro_f1": 0.0},
            "majority_room": {"coverage": 1.0, "room": rooms[major], "accuracy": float(counts[major] / max(len(truth), 1))}}


class Matcher:
    def __init__(self, model_dir):
        self.dir = Path(model_dir)
        self.meta = json.loads((self.dir / "model.json").read_text())
        self.rooms, self.senders = self.meta["rooms"], self.meta["senders"]
        self.sessions = {k: ort.InferenceSession(str(self.dir / f), providers=["CPUExecutionProvider"]) for k, f in self.meta["states"].items()}
        sm = self.meta["smoothing"]
        self.trans = np.array([[sm["transition"][a][b] for b in self.rooms] for a in self.rooms])

    def probs(self, key, X):
        return self.sessions[key].run(["probabilities"], {"x": X})[0]

    def run(self, scans, smooth=True):
        m, sc = self.meta, self.meta["scale"]
        n, k = len(scans), len(self.rooms)
        P = np.full((n, k), np.nan)
        reason = [None] * n
        keys = [None] * n
        for i, s in enumerate(scans):
            if not any(b in s.aps for b in self.senders):
                reason[i] = "no_scan"
            elif s.state in self.sessions:
                keys[i] = s.state
            elif "all" in self.sessions:
                keys[i] = "all"
            else:
                reason[i] = "unknown_state"
        for key in set(k_ for k_ in keys if k_):
            idx = [i for i in range(n) if keys[i] == key]
            X = np.stack([vectorize(scans[i].aps, self.senders, sc["floor"], sc["span"]) for i in idx])
            P[idx] = self.probs(key, X)
        out = [None] * n
        held = [None] * n
        state = {}
        for i in sorted(range(n), key=lambda j: (scans[j].device, scans[j].ts)):
            s = scans[i]
            if reason[i]:
                out[i] = (None, reason[i])
            else:
                alpha, last_ts = state.get(s.device, (None, None))
                p = P[i]
                if smooth and alpha is not None:
                    steps = int(np.clip(round((s.ts - last_ts) / 60.0), 1, 60))
                    post = (alpha @ np.linalg.matrix_power(self.trans, steps)) * p
                    p = post / post.sum() if post.sum() > 0 else p
                state[s.device] = (p, s.ts)
                room, why = decide(p, m["tau_room"], m["delta_room"])
                out[i] = (room if why is None else None, why)
        last = {}
        for i in sorted(range(n), key=lambda j: (scans[j].device, scans[j].ts)):
            s = scans[i]
            if out[i][0] is not None:
                last[s.device] = (out[i][0], s.ts)
                held[i] = out[i][0]
            elif s.device in last and s.ts - last[s.device][1] <= m["t_hold_min"] * 60:
                held[i] = last[s.device][0]
        return out, held


def evaluate(matcher, scans, smooth=True):
    rooms = matcher.rooms
    idx = [i for i, s in enumerate(scans) if s.room in rooms]
    out, held = matcher.run(scans, smooth)
    truth = [rooms.index(scans[i].room) for i in idx]
    res = score(truth, [out[i][0] for i in idx], rooms)
    res["abstain_reasons"] = {r: sum(1 for i in idx if out[i][1] == r) for r in sorted({out[i][1] for i in idx if out[i][1]})}
    res["held_accuracy"] = float(np.mean([held[i] == t for i, t in zip(idx, truth) if held[i] is not None])) if any(held[i] is not None for i in idx) else 0.0
    res["held_coverage"] = float(np.mean([held[i] is not None for i in idx])) if idx else 0.0
    res["baselines"] = baselines(np.array(truth, dtype=int), rooms)
    return res


def report_md(version, metrics, extra):
    lines = [f"# watchroom {version}", "", f"Auswertung: {extra}", ""]
    for name in ("smoothed", "raw"):
        m = metrics[name]
        lines += [f"## {name}", "", f"n={m['n']} coverage={m['coverage']:.3f} accuracy={m['accuracy']:.3f} macro_f1={m['macro_f1']:.3f}",
                  f"abstain: {m['abstain_reasons']}", "", "| room | precision | recall | f1 | n |", "|---|---|---|---|---|"]
        lines += [f"| {r} | {v['precision']:.3f} | {v['recall']:.3f} | {v['f1']:.3f} | {v['support']} |" for r, v in m["per_room"].items()]
        lines += ["", "confusion (rows true, cols predicted, non-abstained):", "", "```"]
        lines += [f"{r:>14} " + " ".join(f"{c:5d}" for c in row) for r, row in zip(m["rooms"], m["confusion"])]
        lines += ["```", ""]
    b = metrics["smoothed"]["baselines"]
    lines += ["## Baseline", "", f"kein Matcher: coverage 0, macro_f1 0 (Anwesenheit nur aus Labels). Mehrheitsraum `{b['majority_room']['room']}`: accuracy {b['majority_room']['accuracy']:.3f}", ""]
    return "\n".join(lines)
