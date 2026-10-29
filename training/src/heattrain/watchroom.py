import json
import tempfile
from collections import Counter
from dataclasses import dataclass, field
from datetime import datetime, timezone
from pathlib import Path
from zoneinfo import ZoneInfo
import duckdb
import numpy as np
import onnx
import onnxruntime as ort
from skl2onnx import convert_sklearn
from skl2onnx.common.data_types import FloatTensorType
from sklearn.calibration import CalibratedClassifierCV
from sklearn.ensemble import GradientBoostingClassifier
from sklearn.neighbors import KNeighborsClassifier
from heattrain.eval import NO_TEMPLATE, Matcher, band_of, evaluate, features, forward, report_md, score


@dataclass
class Scan:
    ts: float
    device: str
    aps: dict
    room: str | None = None
    state: str | None = None
    freqs: dict = field(default_factory=dict)
    hist: list = field(default_factory=list)


class Lake:
    def __init__(self, data_root):
        self.root = Path(data_root) / "parquet"
        self.con = duckdb.connect()

    def columns(self, table):
        if not any((self.root / table).glob("date=*/part.parquet")):
            return set()
        return {r[0] for r in self.con.execute(f"DESCRIBE SELECT * FROM {self.src(table)}").fetchall()}

    def src(self, table):
        return f"read_parquet('{(self.root / table).as_posix()}/*/part.parquet', hive_partitioning=true, union_by_name=true)"

    def query(self, table, select, where="TRUE", order="", need=()):
        cols = self.columns(table)
        if not cols or not set(need) <= cols:
            return []
        sel = ", ".join(e if c is None or c in cols else "NULL" for e, c in select)
        return self.con.execute(f"SELECT {sel} FROM {self.src(table)} WHERE {where} {order}").fetchall()


def load_signals(lake, cfg):
    rows = lake.query("watch_raw", [("epoch(ts)", None), ("device", None), ("sensor_type", None), ("value", None)], "sensor_type IN ('hr','motion','offbody','charging')", "ORDER BY ts", ("sensor_type", "value"))
    sig = {}
    for t, dev, kind, val in rows:
        if val is not None:
            sig.setdefault((str(dev), kind), ([], []))
            sig[(str(dev), kind)][0].append(t)
            sig[(str(dev), kind)][1].append(float(val))
    return {k: (np.array(a), np.array(b)) for k, (a, b) in sig.items()}


def window_mean(series, t, window):
    if series is None:
        return None
    ts, vs = series
    lo, hi = np.searchsorted(ts, t - window, side="right"), np.searchsorted(ts, t, side="right")
    return float(vs[lo:hi].mean()) if hi > lo else None


def last_value(series, t, stale):
    if series is None:
        return None
    ts, vs = series
    hi = np.searchsorted(ts, t, side="right")
    return float(vs[hi - 1]) if hi and t - ts[hi - 1] <= stale else None


def classify_state(cfg, tz, t, dev, sig):
    w = cfg["window_s"]
    charging, offbody = last_value(sig.get((dev, "charging")), t, cfg["stale_s"]), last_value(sig.get((dev, "offbody")), t, cfg["stale_s"])
    hr, motion = window_mean(sig.get((dev, "hr")), t, w), window_mean(sig.get((dev, "motion")), t, w)
    if charging is not None and charging > 0.5:
        return "charging"
    if offbody is not None and offbody < 0.5:
        return "offbody"
    if hr is None and motion is None:
        return "rest"
    start, end = cfg["sleep_hours"]
    hour = datetime.fromtimestamp(t, tz).hour
    night = (hour >= start or hour < end) if start > end else start <= hour < end
    quiet = (hr is None or hr <= cfg["hr_rest_max"]) and (motion is None or motion < cfg["motion_active_min"])
    if night and quiet:
        return "sleep"
    if (hr is not None and hr >= cfg["hr_active_min"]) or (motion is not None and motion >= cfg["motion_active_min"]):
        return "active"
    return "rest"


def assign_labels(lake, cfg, scans):
    if not scans:
        return
    rows = lake.query("labels", [("epoch(ts)", None), ("epoch(from_ts)", "from_ts"), ("epoch(to_ts)", "to_ts"), ("room", None), ("device", "device")], f"source IN ({','.join(repr(s) for s in cfg['sources'])})", "ORDER BY ts")
    t = np.array([s.ts for s in scans])
    dev = np.array([s.device for s in scans])
    room = np.full(len(scans), None, dtype=object)
    best = np.full(len(scans), np.inf)
    for ts, f, to, name, d in rows:
        if f is not None and to is not None:
            mask = (t >= f) & (t <= to) & ((dev == str(d)) if d else True)
            room[mask], best[mask] = name, -1.0
    for ts, f, to, name, d in rows:
        if f is None or to is None:
            dist = np.abs(t - ts)
            mask = (dist <= cfg["label_tolerance_s"]) & (dist < best) & ((dev == str(d)) if d else True)
            room[mask], best[mask] = name, dist[mask]
    for s, r in zip(scans, room):
        s.room = r


def load_scans(data_root, cfg):
    lake = Lake(data_root)
    cols = lake.columns("watch_raw")
    value = "coalesce(" + ", ".join(f"CAST({c} AS DOUBLE)" for c in ("rssi", "value") if c in cols) + ")"
    rows = lake.query("watch_raw", [("epoch(ts)", None), ("device", None), ("batch_id", None), ("sender", None), (value, None), ("freq", "freq")],
                      "sensor_type IS NULL OR sensor_type = 'rssi'" if "sensor_type" in cols else "TRUE", "ORDER BY ts")
    grouped = {}
    for t, dev, batch, sender, rssi, freq in rows:
        if rssi is None:
            continue
        s = grouped.setdefault((str(dev), batch, round(t)), Scan(float(t), str(dev), {}))
        s.aps[sender] = max(rssi, s.aps.get(sender, rssi))
        if freq is not None:
            s.freqs[sender] = int(freq)
    scans = sorted(grouped.values(), key=lambda s: (s.ts, s.device))
    attach_history(scans, cfg)
    assign_labels(lake, cfg, scans)
    sig, tz = load_signals(lake, cfg), ZoneInfo(cfg["timezone"])
    for s in scans:
        s.state = classify_state(cfg["state"], tz, s.ts, s.device, sig)
    return scans, load_links(lake)


def attach_history(scans, cfg):
    n, h, past = cfg.get("history_scans", 5), cfg.get("history_s", 300.0), {}
    for s in scans:
        prior = past.setdefault(s.device, [])
        s.hist = [x.aps for x in prior[-(n - 1):] if s.ts - x.ts <= h] + [s.aps] if n > 1 else [s.aps]
        prior.append(s)


def feature_spec(cfg, senders, scans):
    spec = {"senders": senders, "scale": cfg["scale"], "sentinel_dbm": cfg["scale"]["floor"]}
    if cfg.get("feature_version", 2) >= 2:
        freq = {}
        for s in scans:
            for b, f in s.freqs.items():
                freq.setdefault(b, Counter())[f] += 1
        bands = [band_of(freq[b].most_common(1)[0][0]) if b in freq else None for b in senders]
        spec |= {"feature_version": 2, "sender_bands": bands, "history_s": cfg.get("history_s", 300.0), "history_scans": cfg.get("history_scans", 5)}
    return spec


def load_links(lake):
    latest = {}
    for key, value, valid_from in lake.query("meta", [("key", None), ("value", None), ("epoch(valid_from)", None)], "key IN ('links','adjacency')", "ORDER BY valid_from"):
        latest[key] = json.loads(value)
    links = [(l["a"], l["b"], l.get("kind", "wall")) for l in latest.get("links", [])]
    return links or [(a, b, "wall") for a, b in latest.get("adjacency", [])]


def day_of(s):
    return int(s.ts // 86400)


def vocabulary(scans, cfg):
    seen, days = Counter(), {}
    for s in scans:
        for b in s.aps:
            seen[b] += 1
            days.setdefault(b, set()).add(day_of(s))
    keep = [b for b in seen if seen[b] >= cfg["min_sender_scans"] and len(days[b]) >= cfg["min_sender_days"]]
    return sorted(sorted(keep, key=lambda b: (-seen[b], b))[: cfg["max_senders"]])


def transitions(rooms, links, cfg):
    w = {r: {q: cfg["weight_other"] for q in rooms if q != r} for r in rooms}
    for a, b, kind in list(links) + [tuple(l) for l in cfg.get("default_links", [])]:
        if a in w and b in w:
            w[a][b] = w[b][a] = max(w[a][b], cfg["weight_link"].get(kind, cfg["weight_other"]))
    out = {}
    for r in rooms:
        total = sum(w[r].values())
        out[r] = {q: (1 - cfg["stay_prob"]) * w[r][q] / total if q != r else cfg["stay_prob"] for q in rooms}
        if len(rooms) == 1:
            out[r][r] = 1.0
    return out


def day_folds(days, n):
    uniq = np.unique(days)
    blocks = np.array_split(uniq, min(n, len(uniq)))
    return [(np.where(~np.isin(days, b))[0], np.where(np.isin(days, b))[0]) for b in blocks if len(b) and len(blocks) > 1]


def full_proba(model, X, k):
    out = np.zeros((len(X), k), dtype=np.float64)
    out[:, model.classes_.astype(int)] = model.predict_proba(X)
    return out


class Trainer:
    def __init__(self, cfg):
        self.cfg = cfg

    def make(self, kind, y):
        c = self.cfg
        base = KNeighborsClassifier(c["knn_k"]) if kind == "knn" else GradientBoostingClassifier(n_estimators=c["gb_trees"], max_depth=c["gb_depth"], random_state=c["seed"])
        folds = min(3, int(np.bincount(y)[np.bincount(y) > 0].min()))
        return CalibratedClassifierCV(base, method="sigmoid", cv=folds) if folds >= 2 else base

    def oof(self, kind, X, y, days, k):
        P = np.zeros((len(X), k))
        for tr, te in day_folds(days, self.cfg["cv_folds"]):
            if len(np.unique(y[tr])) < 2:
                continue
            P[te] = full_proba(self.make(kind, y[tr]).fit(X[tr], y[tr]), X[te], k)
        return P

    def smoothed(self, P, keep, order, trans):
        out, state = np.zeros_like(P), {}
        for i in order:
            if P[i].sum() <= 0:
                continue
            prior, last = state.get(keep[i].device, (None, None))
            out[i] = forward(prior, (keep[i].ts - last) / 60.0 if last is not None else 0.0, trans, P[i])
            state[keep[i].device] = (out[i], keep[i].ts)
        return out

    def thresholds(self, P, y, rooms):
        best = None
        srt = np.sort(P, axis=1)
        top, margin, arg = srt[:, -1], srt[:, -1] - srt[:, -2], P.argmax(axis=1)
        for tau in np.arange(0.4, 0.96, 0.05):
            for delta in np.arange(0.0, 0.51, 0.05):
                ok = (top >= tau) & (margin >= delta)
                m = score(y, np.where(ok, arg, -1), rooms)
                key = (m["coverage"] >= self.cfg["min_coverage"], m["macro_f1"] if m["coverage"] >= self.cfg["min_coverage"] else m["coverage"], m["coverage"])
                if best is None or key > best[0]:
                    best = (key, float(round(tau, 2)), float(round(delta, 2)), m)
        return best[1], best[2], best[3]

    def fit(self, scans, links):
        c = self.cfg
        labelled = [s for s in scans if s.room]
        counts = Counter(s.room for s in labelled)
        rooms = sorted(r for r, n in counts.items() if n >= c["min_room_scans"])
        if len(rooms) < 2:
            raise ValueError(f"need at least 2 rooms with {c['min_room_scans']} labelled scans, got {dict(counts)}")
        labelled = [s for s in labelled if s.room in rooms and s.state not in NO_TEMPLATE]
        senders = vocabulary(labelled, c)
        spec = feature_spec(c, senders, labelled)
        keep = [s for s in labelled if any(b in s.aps for b in senders)]
        X = np.stack([features(spec, s.hist or [s.aps]) for s in keep])
        y = np.array([rooms.index(s.room) for s in keep])
        days = np.array([day_of(s) for s in keep])
        states = np.array([s.state or "all" for s in keep])
        trans = transitions(rooms, links, c["hmm"])
        trans_m = np.array([[trans[a][b] for b in rooms] for a in rooms])
        order = sorted(range(len(keep)), key=lambda i: (keep[i].device, keep[i].ts))
        cv = {}
        for kind in c["candidates"]:
            P = self.oof(kind, X, y, days, len(rooms))
            ok = P.sum(axis=1) > 0
            cv[kind] = (P, score(list(y[ok]), [int(p.argmax()) for p in P[ok]], rooms)["macro_f1"])
        kind = max(cv, key=lambda k: cv[k][1])
        P = self.smoothed(cv[kind][0], keep, order, trans_m)
        ok = P.sum(axis=1) > 0
        tau, delta, val = self.thresholds(P[ok], y[ok], rooms)
        models = {"all": self.make(kind, y).fit(X, y)}
        for st in sorted(set(states) - {"all"}):
            m = states == st
            per = np.bincount(y[m], minlength=len(rooms))
            if m.sum() >= c["min_state_scans"] and per.min() >= c["min_state_room_scans"]:
                models[st] = self.make(kind, y[m]).fit(X[m], y[m])
        return {"rooms": rooms, "senders": senders, "spec": spec, "kind": kind, "models": models, "tau": tau, "delta": delta,
                "transition": trans, "n_labels": {r: int((y == i).sum()) for i, r in enumerate(rooms)},
                "window": (min(keep, key=lambda s: s.ts).ts, max(keep, key=lambda s: s.ts).ts), "X": X,
                "metrics": {"cv_macro_f1": {k: v[1] for k, v in cv.items()}, "chosen": kind, "validation": {k: val[k] for k in ("coverage", "accuracy", "macro_f1")}}}


def to_onnx(model, n):
    proto = convert_sklearn(model, initial_types=[("x", FloatTensorType([None, n]))], options={id(model): {"zipmap": False}}, target_opset={"": 15, "ai.onnx.ml": 3})
    keep = [o for o in proto.graph.output if o.name == "probabilities"]
    del proto.graph.output[:]
    proto.graph.output.extend(keep)
    onnx.checker.check_model(proto)
    return proto.SerializeToString()


def export(bundle, cfg, out_dir, version, metrics):
    out = Path(out_dir) / version
    out.mkdir(parents=True, exist_ok=True)
    X = bundle["X"][: cfg["parity_rows"]]
    n, files = X.shape[1], {}
    for key, model in bundle["models"].items():
        blob = to_onnx(model, n)
        got = ort.InferenceSession(blob, providers=["CPUExecutionProvider"]).run(["probabilities"], {"x": X})[0]
        err = float(np.abs(got - full_proba(model, X, len(bundle["rooms"]))).max())
        if err > cfg["parity_tol"]:
            raise RuntimeError(f"onnx parity {key}: {err}")
        files[key] = f"{key}.onnx"
        (out / files[key]).write_bytes(blob)
        metrics.setdefault("onnx_parity_max_abs", {})[key] = err
    lo, hi = (datetime.fromtimestamp(t, timezone.utc).date().isoformat() for t in bundle["window"])
    meta = {"model_version": version, "created": datetime.now(timezone.utc).isoformat(timespec="seconds"), "matcher": "room",
            "rooms": bundle["rooms"], "senders": bundle["senders"], "sentinel_dbm": cfg["scale"]["floor"], "scale": cfg["scale"], "states": files,
            "tau_room": bundle["tau"], "delta_room": bundle["delta"], "t_hold_min": cfg["t_hold_min"],
            "smoothing": {"kind": "hmm", "stay_prob": cfg["hmm"]["stay_prob"], "step_s": 60, "transition": bundle["transition"]},
            "state_rule": cfg["state"], "classifier": bundle["kind"], "metrics": metrics, "train_window": {"from": lo, "to": hi}, "n_labels": bundle["n_labels"]}
    meta |= {k: v for k, v in bundle["spec"].items() if k not in meta}
    (out / "model.json").write_text(json.dumps(meta, indent=2))
    return out


def split_days(scans, frac):
    days = sorted({day_of(s) for s in scans if s.room})
    n = max(1, round(len(days) * frac))
    cut = days[-n] if len(days) > 1 else days[0] + 1
    return [s for s in scans if day_of(s) < cut], [s for s in scans if day_of(s) >= cut]


def holdout(cfg, tr, ho, links):
    with tempfile.TemporaryDirectory() as tmp:
        probe = Matcher(export(Trainer(cfg).fit(tr, links), cfg, tmp, "probe", {}))
    return {"smoothed": evaluate(probe, ho), "raw": evaluate(probe, ho, smooth=False)}


def train(cfg, data_root, models_dir, version=None):
    scans, links = load_scans(data_root, cfg)
    version = version or datetime.now(timezone.utc).strftime("%Y%m%dT%H%M%SZ")
    trainer = Trainer(cfg)
    metrics = {}
    tr, ho = split_days(scans, cfg["holdout_frac"])
    if any(s.room for s in tr) and any(s.room for s in ho):
        metrics["holdout"] = holdout(cfg, tr, ho, links)
        metrics["holdout"]["days"] = sorted({datetime.fromtimestamp(s.ts, timezone.utc).date().isoformat() for s in ho})
        if cfg.get("feature_version", 2) > 1:
            metrics["holdout"]["v1"] = holdout({**cfg, "feature_version": 1}, tr, ho, links)
    bundle = trainer.fit(scans, links)
    metrics.update(bundle["metrics"])
    out = export(bundle, cfg, models_dir, version, metrics)
    if "holdout" in metrics:
        write_report(cfg, version, metrics["holdout"], "Hold-out-Tage (zeitlich getrennt, Modell ohne diese Tage trainiert)")
    return out


def write_report(cfg, version, m, extra):
    d = Path(cfg["eval_dir"])
    d.mkdir(parents=True, exist_ok=True)
    (d / f"watchroom_{version}.md").write_text(report_md(version, m, extra))


def eval_model(cfg, data_root, model_dir):
    scans, links = load_scans(data_root, cfg)
    matcher = Matcher(model_dir)
    meta = matcher.meta
    end = meta["train_window"]["to"]
    new = [s for s in scans if datetime.fromtimestamp(s.ts, timezone.utc).date().isoformat() > end]
    use, note = (new, f"Tage nach {end}") if any(s.room for s in new) else (scans, "alle Daten (im Training enthalten, nur Plausibilitaet)")
    m = {"smoothed": evaluate(matcher, use), "raw": evaluate(matcher, use, smooth=False), "note": note}
    if meta.get("feature_version", 1) > 1:
        cut = {id(s) for s in new}
        old = [s for s in scans if id(s) not in cut] if use is new else scans
        try:
            m["v1"] = holdout({**cfg, "feature_version": 1}, old, use, links)
        except ValueError:
            pass
    meta["metrics"]["eval"] = m
    (Path(model_dir) / "model.json").write_text(json.dumps(meta, indent=2))
    write_report(cfg, meta["model_version"], m, note)
    return m
