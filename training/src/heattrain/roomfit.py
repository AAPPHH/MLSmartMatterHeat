import json
import math
from datetime import datetime, timezone
from pathlib import Path
import duckdb
import numpy as np
from heattrain.thermal import House, RoomIn, RoomPar, build_rooms, epoch, fit, fit_room, load_events, load_lake, norm_cdf, read_arrows

ROOM_KEYS = ("a1", "a2", "b1", "b2", "b3", "b4", "w", "ts", "ts_night")


def load_window_labels(path):
    p = Path(path)
    out = []
    t = read_arrows(p, "labels")
    if t is not None:
        cols = [t.column(n).to_pylist() for n in ("room", "source", "window_open")]
        out += [(ts, r, bool(o)) for ts, r, s, o in zip(epoch(t.column("ts").to_pylist()), *cols) if s == "window" and o is not None]
    base = p / "parquet" / "labels"
    if base.is_dir() and any(base.rglob("part.parquet")):
        sql = f"SELECT epoch(ts), room::VARCHAR, window_open FROM read_parquet('{base.as_posix()}/**/part.parquet', hive_partitioning=false, union_by_name=true) WHERE source::VARCHAR = 'window' AND window_open IS NOT NULL"
        out += [(float(a), b, bool(c)) for a, b, c in duckdb.connect().execute(sql).fetchall()]
    return sorted(set((round(a, 3), b, c) for a, b, c in out))


def load_ground_truth(path, hold_min):
    f = Path(path) / "ground_truth" / "windows.jsonl"
    if not f.is_file():
        return []
    out = []
    for line in f.read_text(encoding="utf-8").splitlines():
        if not line.strip():
            continue
        w = json.loads(line)
        a = datetime.fromisoformat(w["open_utc"]).timestamp()
        b = datetime.fromisoformat(w["close_utc"]).timestamp() if w.get("close_utc") else a + hold_min * 60
        out.append((a, b, list(w["rooms_open"])))
    return out


def room_doc(d, cfg, rc):
    doc = fit_room(d, cfg)
    p = doc["params"]
    note = None
    if doc["structure"] != "rc2":
        f, _ = fit(d, "rc2", cfg)
        p = f.p
        note = "rc1 chosen by the thermal fit, rc2 refitted for the room model"
    c = {k[2:]: float(v) for k, v in p.items() if k.startswith("c_")}
    return {
        "node": d.node, "struct": "rc2", "params": {k: float(p[k]) for k in ROOM_KEYS}, "c": c, "neighbours": {k: sorted(set(d.info["neighbour_nodes"].get(k, []))) for k in c},
        "valve_gain": float(doc["valve_gain"]), "sigma_u": sigma_u(d, doc["valve_gain"], rc), "noise_r": cfg["noise_r"], "p0": cfg["p0"], "q_r": cfg["q_r"], "q_a": cfg["q_a"], "note": note,
    }


def sigma_u(d, gain, rc):
    ok = np.isfinite(d.yv) & (d.yv > 0) & (d.yv < 1) & np.isfinite(d.sp) & np.isfinite(d.yi)
    if ok.sum() < 30:
        return rc["valve"]["sigma_u_default"]
    r = d.yv[ok] - gain * (d.sp[ok] - d.yi[ok])
    return float(max(np.std(r), rc["valve"]["sigma_u_min"]))


class Grid:
    def __init__(self, rooms, docs):
        self.dt = rooms[0].dt
        self.t0 = math.floor(min(d.t0 for d in rooms) / self.dt) * self.dt
        offs = {d.room: int(round((d.t0 - self.t0) / self.dt)) for d in rooms}
        self.n = max(offs[d.room] + d.n for d in rooms)
        self.names = [d.room for d in rooms]
        self.kinds = {d.room: sorted(docs[d.room]["c"]) for d in rooms}
        self.cols = {}
        for d in rooms:
            o = offs[d.room]

            def pad(x, fill=np.nan):
                out = np.full(self.n, fill, dtype=float)
                out[o:o + d.n] = x
                return out

            nb = {k: (pad(np.asarray(d.nb[k][0], float)) if k in d.nb else np.zeros(self.n), pad(np.asarray(d.nb[k][1], float)) if k in d.nb else np.zeros(self.n)) for k in self.kinds[d.room]}
            self.cols[d.room] = {"y": pad(d.y), "yv": pad(d.yv), "u": pad(d.u), "sp": pad(d.sp), "tout": pad(d.tout), "wind": pad(d.wind), "rad": pad(d.rad), "night": pad(d.night.astype(float)), "yi": pad(d.yi),
                                 "bad": pad(d.bad.astype(float), 1.0), "ncnt": [nb[k][0] for k in self.kinds[d.room]], "nsum": [nb[k][1] for k in self.kinds[d.room]]}
        self.times = self.t0 + np.arange(self.n) * self.dt

    def tail(self, start):
        g = object.__new__(Grid)
        g.__dict__.update(self.__dict__)
        g.n = self.n - start
        g.times = self.times[start:]
        g.cols = {r: {k: ([x[start:] for x in v] if isinstance(v, list) else v[start:]) for k, v in c.items()} for r, c in self.cols.items()}
        return g

    def span_mask(self, spans):
        m = np.zeros(self.n, bool)
        for a, b in spans:
            m[np.searchsorted(self.times, a, side="left"):np.searchsorted(self.times, b, side="right")] = True
        return m

    def inputs(self, k, drop=None):
        out = {}
        for name in self.names:
            c = self.cols[name]
            hide = drop is not None and drop[name][k]
            out[name] = RoomIn(math.nan if hide else c["y"][k], math.nan if hide else c["yv"][k], c["u"][k], c["sp"][k], c["tout"][k], c["wind"][k], c["rad"][k], bool(c["night"][k] > 0.5),
                               tuple(a[k] for a in c["ncnt"]), tuple(a[k] for a in c["nsum"]))
        return out


def run_house(grid, docs, shock, house, drop=None):
    h = House({n: docs[n] for n in grid.names}, shock, house)
    keys = ("m", "a", "lam", "lam_std", "p_window", "p_supply")
    rec = {n: {k: np.full(grid.n, np.nan) for k in keys} for n in grid.names}
    for k in range(grid.n):
        for n, o in h.step(grid.inputs(k, drop)).items():
            if o["ok"]:
                for key in keys:
                    rec[n][key][k] = o[key]
    return h, rec


def shock_defaults(rc, **over):
    s = rc["shock"]
    return {"tau_l": s["tau_l"], "hold_min": s["hold_min"], "p_l0": s["p_l0"], "lambda_jump": s["lambda_jump"], "lambda_w": s["lambda_w"], **s["start"], **over}


def fit_shock(grid, docs, rc, drop):
    best = dict(rc["shock"]["start"])
    house = rc["house"]

    def nll(vals):
        h, _ = run_house(grid, docs, shock_defaults(rc, **vals), house, drop)
        return -h.loglik

    score = nll(best)
    trace = [(dict(best), score)]
    for _ in range(rc["shock"]["passes"]):
        for key, values in rc["shock"]["grid"].items():
            for v in values:
                if v == best[key]:
                    continue
                trial = dict(best, **{key: v})
                s = nll(trial)
                trace.append((trial, s))
                if s < score:
                    best, score = trial, s
    return best, score, trace


def window_spans(gt, labels, rooms, pad_s):
    spans = {r: [] for r in rooms}
    for a, b, open_rooms in gt:
        for r in open_rooms:
            if r in spans:
                spans[r].append((a - pad_s, b + pad_s))
    yes = [(t, r) for t, r, o in labels if o]
    for t, r in yes:
        if r in spans:
            spans[r].append((t - pad_s, t + pad_s))
    return spans


def episodes(flag, times):
    out, start = [], None
    for i, f in enumerate(flag):
        if f and start is None:
            start = i
        if not f and start is not None:
            out.append((times[start], times[i - 1]))
            start = None
    if start is not None:
        out.append((times[start], times[-1]))
    return out


def detection(grid, rec, lam_w, gt, labels, rc):
    hit_s, pad_s = rc["shock"]["hit_min"] * 60, rc["shock"]["pad_min"] * 60
    res = {"hits": 0, "events": 0, "false_episodes": 0, "no_flagged": 0, "no_labels": 0, "yes_hit": 0, "yes_labels": 0, "latency_min": []}
    allspans = window_spans(gt, labels, grid.names, pad_s)
    flags = {}
    for r in grid.names:
        lam, sd = rec[r]["lam"], np.maximum(rec[r]["lam_std"], 1e-6)
        p = np.array([1.0 - norm_cdf((lam_w - m) / s) if math.isfinite(m) else 0.0 for m, s in zip(lam, sd)])
        flags[r] = p > 0.5
    for a, b, open_rooms in gt:
        for r in open_rooms:
            if r not in flags:
                continue
            res["events"] += 1
            i0, i1 = np.searchsorted(grid.times, a), np.searchsorted(grid.times, a + hit_s, side="right")
            idx = np.nonzero(flags[r][i0:i1])[0]
            if len(idx):
                res["hits"] += 1
                res["latency_min"].append(float(idx[0]) * grid.dt / 60.0)
    for t, r, o in labels:
        if r not in flags:
            continue
        i = np.searchsorted(grid.times, t)
        around = flags[r][max(i - int(hit_s / grid.dt), 0):i + int(hit_s / grid.dt) + 1]
        if o:
            res["yes_labels"] += 1
            res["yes_hit"] += int(around.any())
        else:
            res["no_labels"] += 1
            res["no_flagged"] += int(around.any())
    for r in grid.names:
        for a, b in episodes(flags[r], grid.times):
            if not any(x <= a <= y for x, y in allspans[r]):
                res["false_episodes"] += 1
    return res


def tune_windows(grid, rec, rc, gt, labels):
    s = rc["shock"]
    if not gt and not labels:
        return s["lambda_w"], s["lambda_jump"], None
    rows = []
    for lw in s["lambda_w_grid"]:
        d = detection(grid, rec, lw, gt, labels, rc)
        rows.append((d["hits"] + d["yes_hit"] - s["fa_weight"] * (d["false_episodes"] + d["no_flagged"]), lw, d))
    best = max(rows, key=lambda r: (r[0], r[1]))
    lam_j = []
    for a, b, open_rooms in gt:
        i0, i1 = np.searchsorted(grid.times, a + 600), np.searchsorted(grid.times, min(b, a + 2400), side="right")
        for r in open_rooms:
            if r in rec and i1 > i0:
                v = rec[r]["lam"][i0:i1]
                v = v[np.isfinite(v)]
                if len(v):
                    lam_j.append(float(np.median(v)))
    lj = float(np.clip(np.median(lam_j), best[1] + 0.25, 8.0)) if lam_j else s["lambda_jump"]
    return best[1], lj, {"candidates": [{"lambda_w": lw, "score": sc, **d} for sc, lw, d in rows], "lambda_j_events": len(lam_j)}


def forecast_error(grid, docs, rec, rc):
    f = rc["forecast"]
    H = int(f["horizon_min"])
    out = {}
    for r in grid.names:
        par, c = RoomPar(docs[r]), grid.cols[r]
        err, base, n = [], [], 0
        for j in range(0, grid.n - H, f["stride"]):
            if not math.isfinite(rec[r]["m"][j]) or c["bad"][j:j + H + 1].any() or not np.isfinite(c["yi"][j]) or not np.isfinite(c["yi"][j + H]):
                continue
            m, a, lam = rec[r]["m"][j], rec[r]["a"][j], rec[r]["lam"][j]
            for k in range(j, j + H):
                cn, cs = par.coupling(tuple(x[k] for x in c["ncnt"]), tuple(x[k] for x in c["nsum"]))
                if not math.isfinite(c["u"][k]) or not math.isfinite(c["tout"][k]) or not math.isfinite(cn + cs):
                    break
                f11, f12, f21, f22, g1, g2, _ = par.terms(lam, c["u"][k], c["tout"][k], c["wind"][k], c["rad"][k], c["night"][k] > 0.5, cn, cs, 1.0 / 60.0)
                m, a = f11 * m + f12 * a + g1, f21 * m + f22 * a + g2
            else:
                err.append(c["yi"][j + H] - par.sensor(m, a))
                base.append(c["yi"][j + H] - c["yi"][j])
        rm = lambda x: float(np.sqrt(np.mean(np.square(x)))) if len(x) else None
        out[r] = {"rmse_model": rm(err), "rmse_persistence": rm(base), "n": len(err)}
    return out


def window_model_sigma(models_dir):
    base = Path(models_dir) / "window"
    out = {}
    for d in sorted(base.glob("*Z"), reverse=True) if base.is_dir() else []:
        f = d / "bundle.json"
        if f.is_file():
            b = json.loads(f.read_text())
            out = {r: m["sigma"] for r, m in b.get("rooms", {}).items() if "sigma" in m}
            break
    return out


def build_data(config, src):
    cfg = config["thermal"]
    reports, weather, meta = load_lake(src, cfg["exclude_sources"])
    rooms, skipped = build_rooms(reports, weather, meta, cfg, load_events(src))
    if not rooms:
        raise SystemExit(f"no usable rooms: {skipped}")
    return rooms, skipped


def fit_bundle(config, rooms, src, version=None):
    cfg, rc = config["thermal"], config["roommodel"]
    docs = {d.room: room_doc(d, cfg, rc) for d in rooms}
    full = Grid(rooms, docs)
    grid = full.tail(max(0, full.n - int(rc["shock"]["days"] * 1440)))
    gt = load_ground_truth(src, rc["hold_min"])
    labels = load_window_labels(src)
    spans = window_spans(gt, labels, grid.names, rc["shock"]["pad_min"] * 60)
    drop = {r: grid.span_mask(spans[r]) for r in grid.names}
    best, nll, trace = fit_shock(grid, docs, rc, drop)
    shock = shock_defaults(rc, **best)
    _, rec = run_house(grid, docs, shock, rc["house"])
    lw, lj, tuning = tune_windows(grid, rec, rc, gt, labels)
    shock = dict(shock, lambda_w=lw, lambda_jump=lj)
    run, rec = run_house(grid, docs, shock, rc["house"])
    version = version or datetime.now(timezone.utc).strftime("%Y%m%dT%H%M%SZ")
    house = dict(rc["house"], night_hours=cfg["night_hours"], timezone=cfg["timezone"])
    bundle = {"kind": "room_model", "version": version, "dt_s": grid.dt, "rooms": {n: {k: v for k, v in d.items() if k != "note"} for n, d in docs.items()},
              "shock": {k: shock[k] for k in ("tau_l", "q_calm", "q_jump", "p_jump", "hold_min", "lambda_jump", "lambda_w", "p_l0")}, "house": house}
    report = {"version": version, "nll": nll, "trace": [{"shock": t, "nll": s} for t, s in trace], "tuning": tuning, "forecast_20min": forecast_error(grid, docs, rec, rc), "resets": len(run.resets),
              "reset_causes": [r["cause"] for r in run.resets], "notes": {n: d["note"] for n, d in docs.items() if d["note"]}, "ground_truth_events": len(gt), "labels": len(labels), "loglik": run.loglik,
              "minutes": grid.n, "masked_minutes": {d.room: d.info["n_masked"] for d in rooms}, "params": {n: {**d["params"], "c": d["c"], "valve_gain": d["valve_gain"], "sigma_u": d["sigma_u"]} for n, d in docs.items()}}
    return bundle, report


def roommodel(config, src, models_dir):
    rooms, skipped = build_data(config, src)
    bundle, report = fit_bundle(config, rooms, src)
    out = Path(models_dir) / "room_model" / bundle["version"]
    out.mkdir(parents=True, exist_ok=True)
    (out / "bundle.json").write_text(json.dumps(bundle, indent=1, default=float))
    report["skipped"] = skipped
    sigma = window_model_sigma(models_dir)
    report["window_model_sigma"] = sigma
    (out / "report.json").write_text(json.dumps(report, indent=1, default=float))
    return out, bundle, report
