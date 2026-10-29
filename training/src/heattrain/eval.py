import json
from pathlib import Path
import numpy as np
import onnxruntime as ort


def vectorize(aps, senders, floor, span):
    return np.array([min(max((aps[s] - floor) / span, 0.0), 1.0) if s in aps else 0.0 for s in senders], dtype=np.float32)


BANDS = ("2.4", "5", "6")


def band_of(freq):
    if freq is None:
        return None
    return "2.4" if 2400 <= freq < 2500 else "5" if 5000 <= freq < 5925 else "6" if 5925 <= freq < 7200 else None


def features(spec, hist):
    fl, sp, senders = spec["scale"]["floor"], spec["scale"]["span"], spec["senders"]
    if spec.get("feature_version", 1) < 2:
        return vectorize(hist[-1], senders, fl, sp)
    norm = lambda r: min(max((r - fl) / sp, 0.0), 1.0)
    sent, cur, bands = norm(spec["sentinel_dbm"]), hist[-1], spec["sender_bands"]
    now, mean, pres = [], [], []
    for s in senders:
        vals = [norm(h[s]) for h in hist if s in h]
        now.append(norm(cur[s]) if s in cur else sent)
        mean.append(sum(vals) / len(vals) if vals else sent)
        pres.append(len(vals) / len(hist))
    out = now + mean + pres
    for b in BANDS:
        idx = [i for i in range(len(senders)) if bands[i] == b]
        vis = sorted((now[i] for i in idx if senders[i] in cur), reverse=True)[:3]
        out += [vis[0] if vis else sent, sum(vis) / len(vis) if vis else sent, sum(senders[i] in cur for i in idx) / len(idx) if idx else 0.0]
    return np.array(out, dtype=np.float32)


def decide(p, tau, delta):
    order = np.argsort(p)[::-1]
    top, second = float(p[order[0]]), float(p[order[1]]) if len(p) > 1 else 0.0
    if top < tau:
        return int(order[0]), "low_confidence"
    if top - second < delta:
        return int(order[0]), "low_margin"
    return int(order[0]), None


NO_TEMPLATE = ("offbody", "charging")
MAX_STEPS = 60
BELIEF_FLOOR = 1e-4


def forward(prior, dt_min, trans, emission):
    n = len(emission)
    if prior is not None and dt_min <= MAX_STEPS:
        belief = prior @ np.linalg.matrix_power(trans, int(np.clip(np.floor(dt_min + 0.5), 1, MAX_STEPS)))
    else:
        belief = np.full(n, 1.0 / n)
    belief = belief * emission
    if not belief.sum() > 0:
        belief = np.asarray(emission, dtype=float).copy()
    belief = np.maximum(belief / belief.sum(), BELIEF_FLOOR)
    return belief / belief.sum()


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
        m = self.meta
        n, k = len(scans), len(self.rooms)
        P = np.full((n, k), np.nan)
        reason = [None] * n
        keys = [None] * n
        for i, s in enumerate(scans):
            if not any(b in s.aps for b in self.senders):
                reason[i] = "no_known_ap"
            elif s.state in NO_TEMPLATE:
                reason[i] = "unknown_state"
            elif s.state in self.sessions:
                keys[i] = s.state
            elif "all" in self.sessions:
                keys[i] = "all"
            else:
                reason[i] = "unknown_state"
        for key in set(k_ for k_ in keys if k_):
            idx = [i for i in range(n) if keys[i] == key]
            X = np.stack([features(m, scans[i].hist or [scans[i].aps]) for i in idx])
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
                if smooth:
                    p = forward(alpha, (s.ts - last_ts) / 60.0 if last_ts is not None else 0.0, self.trans, p)
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


def anchor_drift(rows, window, threshold):
    base, days = {}, {}
    for day, sender, rssi in rows:
        target = base if window[0] <= day <= window[1] else days.setdefault(day, {}) if day > window[1] else None
        if target is not None:
            target.setdefault(sender, []).append(rssi)
    med = lambda d: {s: float(np.median(v)) for s, v in d.items()}
    base, days = med(base), {d: med(v) for d, v in sorted(days.items())}
    flags = []
    for day, seen in days.items():
        for sender, ref in sorted(base.items()):
            if sender not in seen:
                flags.append({"day": day, "sender": sender, "kind": "missing", "baseline": ref})
            elif abs(seen[sender] - ref) > threshold:
                flags.append({"day": day, "sender": sender, "kind": "shift", "baseline": ref, "median": seen[sender], "delta": seen[sender] - ref})
    return {"baseline": base, "days": days, "flags": flags, "threshold_db": threshold}


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
    if "v1" in metrics:
        lines += ["## Features v1 vs v2", "", "| variante | feature | n | coverage | accuracy | macro_f1 | abstain |", "|---|---|---|---|---|---|---|"]
        for name in ("smoothed", "raw"):
            for ver, m in (("v1", metrics["v1"][name]), ("v2", metrics[name])):
                lines.append(f"| {name} | {ver} | {m['n']} | {m['coverage']:.3f} | {m['accuracy']:.3f} | {m['macro_f1']:.3f} | {m['abstain_reasons']} |")
        lines.append("")
    if "anchor_drift" in metrics:
        d = metrics["anchor_drift"]
        lines += ["## Anchor-Drift", "", f"{len(d['baseline'])} Sender im Trainingsfenster, Schwelle {d['threshold_db']} dB, {len(d['days'])} Tage danach", ""]
        lines += [f"- {f['day']} {f['sender']}: {f['kind']}" + (f" {f['delta']:+.1f} dB" if f["kind"] == "shift" else "") for f in d["flags"]] or ["keine Auffaelligkeiten"]
        lines.append("")
    b = metrics["smoothed"]["baselines"]
    lines += ["## Baseline", "", f"kein Matcher: coverage 0, macro_f1 0 (Anwesenheit nur aus Labels). Mehrheitsraum `{b['majority_room']['room']}`: accuracy {b['majority_room']['accuracy']:.3f}", ""]
    return "\n".join(lines)
