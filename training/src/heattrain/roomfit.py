import itertools
import json
import math
import time
from datetime import datetime, timezone
from pathlib import Path
import duckdb
import numpy as np
from scipy.optimize import least_squares
from heattrain.roombatch import BatchHouse, Grid, Inputs, REC_KEYS, norm_cdf_np, run_batch, valve_lag
from heattrain.roomimm import RoomPar, norm_cdf
from heattrain.roomobs import evidence_blocks
from heattrain.thermal import build_rooms, epoch, fit, load_devices, load_events, load_lake, raw_batches

ROOM_KEYS = ("a1", "a2", "b1", "b2", "b3", "b4", "w", "ts", "ts_night", "tau_v")


def load_window_labels(path):
    p = Path(path)
    out = []
    for b in raw_batches(p, "labels"):
        names = b.schema.names
        if not {"room", "source", "window_open", "ts"} <= set(names):
            continue
        cols = [b.column(n).to_pylist() for n in ("room", "source", "window_open")]
        out += [(ts, r, bool(o)) for ts, r, s, o in zip(epoch(b.column("ts").to_pylist()), *cols) if s == "window" and o is not None]
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


def sigma_u(d, gain, rc):
    ok = np.isfinite(d.yv) & (d.yv > 0) & (d.yv < 1) & np.isfinite(d.sp) & np.isfinite(d.yi)
    if ok.sum() < 30:
        return rc["valve"]["sigma_u_default"]
    r = d.yv[ok] - gain * (d.sp[ok] - d.yi[ok])
    return float(max(np.std(r), rc["valve"]["sigma_u_min"]))


def valve_gain(d, cfg):
    e = np.clip(d.sp - d.yi, 0, cfg["gain_err_max"])
    ok = np.isfinite(e) & (e > 0)
    if ok.sum() < 30:
        return cfg["valve_gain_default"]
    return float(np.clip(np.sum(d.u[ok] * e[ok]) / np.sum(e[ok] ** 2), *cfg["valve_gain_bounds"]))




def lag_alpha(tau):
    return 1.0 - math.exp(-1.0 / tau) if tau > 0 else 1.0


class RoomFit:
    def __init__(self, c, n, kinds, rc, night_ok):
        f = rc["fit"]
        self.c, self.n, self.kinds, self.f = c, n, kinds, f
        self.r_grid, self.gap = rc["house"]["r_grid"], rc["house"]["reset_gap_min"]
        self.names = [k for k in ROOM_KEYS if night_ok or k != "ts_night"] + [f"c_{k}" for k in kinds]
        bnd = lambda k: f["bounds"]["c" if k.startswith("c_") else k]
        self.lo, self.hi = np.log([bnd(k)[0] for k in self.names]), np.log([bnd(k)[1] for k in self.names])
        self.prior = np.clip(np.log([f["init"]["c" if k.startswith("c_") else k] for k in self.names]), self.lo, self.hi)
        self.prior_sd = np.array([f["prior_sigma"].get(k, f["prior_sigma"]["default"]) for k in self.names])
        y = c["y"]
        fin = np.isfinite(y)
        last = np.maximum.accumulate(np.where(fin, np.arange(n), -1))
        self.dyn = np.isfinite(c["u"]) & np.isfinite(c["tout"]) & np.isfinite(c["wind"]) & np.isfinite(c["rad"])
        for a, b in zip(c["ncnt"], c["nsum"]):
            self.dyn &= np.isfinite(a) & np.isfinite(b)
        self.Hs, self.wts = list(f["horizons"]), list(f["weights"])
        js = self.js = np.arange(f["warm_min"], max(n - max(self.Hs) - 1, f["warm_min"]), f["stride"])
        self.Pc = np.where((last[js] >= 0) & (js - last[js] <= f["report_age_min"]), y[np.maximum(last[js], 0)], np.nan)
        badc = np.concatenate([[0], np.cumsum(c["bad"] > 0.5)])
        dync = np.concatenate([[0], np.cumsum(~self.dyn)])
        self.valid = {H: np.isfinite(self.Pc) & fin[js + H] & (badc[js + H + 1] - badc[js] == 0) & (dync[js + H] - dync[js] == 0) for H in self.Hs}
        self.at = np.full(n, -1)
        self.at[js] = np.arange(len(js))
        self.sd = {H: 1.0 for H in self.Hs}

    def params(self, th):
        p = dict(zip(self.names, np.exp(np.asarray(th)).T))
        p.setdefault("ts_night", p["ts"])
        return p

    def coef(self, p):
        c, h = self.c, 1.0 / 60.0
        P = len(p["w"])
        ue = np.stack([valve_lag(c["u"], lag_alpha(t)) for t in p["tau_v"]], -1)
        cn = sum((p[f"c_{k}"] * np.nan_to_num(a)[:, None] for k, a in zip(self.kinds, c["ncnt"])), np.zeros((self.n, P)))
        cs = sum((p[f"c_{k}"] * np.nan_to_num(b)[:, None] for k, b in zip(self.kinds, c["nsum"])), np.zeros((self.n, P)))
        amb = p["b2"] + p["b3"] * np.nan_to_num(c["wind"])[:, None]
        au = p["a1"] * np.nan_to_num(ue)
        ts = np.where(c["night"][:, None] > 0.5, p["ts_night"], p["ts"])
        g2 = h * (amb * np.nan_to_num(c["tout"])[:, None] + p["b4"] * np.nan_to_num(c["rad"])[:, None] / 100.0 + cs)
        return 1.0 - h * (au + p["a2"]), h * p["a2"], h * p["b1"], 1.0 - h * (p["b1"] + amb + cn), h * au * ts, g2, amb, cn, cs, np.nan_to_num(ue)

    def states(self, p, coef, upto, doc):
        c = self.c
        F11, F12, F21, F22, G1, G2, amb, cn, cs, UE = coef
        Q1, Q2 = (doc["q_r"] + doc["q_um"] * UE) / 60.0, (doc["q_a"] + doc["q_ua"] * UE) / 60.0
        w = p["w"]
        w1 = 1 - w
        P = len(w)
        r = self.r_grid + doc["noise_r"] ** 2
        g, su2 = doc["valve_gain"], doc["sigma_u"] ** 2
        hv1, hv2 = -g * w, -g * w1
        y, yv, sp, dyn, at = c["y"].tolist(), c["yv"].tolist(), c["sp"].tolist(), self.dyn.tolist(), self.at.tolist()
        tout, rad = c["tout"], c["rad"]
        valve = doc["valve_obs"]
        M, A = np.full((len(self.js), P), np.nan), np.full((len(self.js), P), np.nan)
        x1 = x2 = p11 = p12 = p22 = None
        since = 0
        for k in range(upto):
            if not dyn[k]:
                since += 1
                continue
            yk = y[k]
            if yk == yk and (x1 is None or since > self.gap):
                d = ((amb[k] + cn[k]) * yk - amb[k] * tout[k] - cs[k] - p["b4"] * rad[k] / 100.0) / (p["b1"] + (amb[k] + cn[k]) * w)
                x2 = yk - w * d
                x1 = x2 + d
                p11, p12, p22 = np.full(P, doc["p0"]), np.zeros(P), np.full(P, doc["p0"])
            if x1 is None:
                continue
            if yk == yk:
                h1, h2 = w * p11 + w1 * p12, w * p12 + w1 * p22
                s = w * h1 + w1 * h2 + r
                v = (yk - (w * x1 + w1 * x2)) / s
                x1, x2 = x1 + h1 * v, x2 + h2 * v
                p11, p12, p22 = p11 - h1 * h1 / s, p12 - h1 * h2 / s, p22 - h2 * h2 / s
                since = 0
            else:
                since += 1
            if valve and 0.0 < yv[k] < 1.0 and sp[k] == sp[k]:
                h1, h2 = hv1 * p11 + hv2 * p12, hv1 * p12 + hv2 * p22
                s = hv1 * h1 + hv2 * h2 + su2
                v = (yv[k] - g * (sp[k] - (w * x1 + w1 * x2))) / s
                x1, x2 = x1 + h1 * v, x2 + h2 * v
                p11, p12, p22 = p11 - h1 * h1 / s, p12 - h1 * h2 / s, p22 - h2 * h2 / s
            if at[k] >= 0:
                M[at[k]], A[at[k]] = x1, x2
            a11, a22 = F11[k], F22[k]
            t11, t12 = a11 * p11 + F12 * p12, a11 * p12 + F12 * p22
            t21, t22 = F21 * p11 + a22 * p12, F21 * p12 + a22 * p22
            p11, p12, p22 = t11 * a11 + t12 * F12 + Q1[k], t11 * F21 + t12 * a22, t21 * F21 + t22 * a22 + Q2[k]
            x1, x2 = a11 * x1 + F12 * x2 + G1[k], F21 * x1 + a22 * x2 + G2[k]
        return M, A

    def roll(self, p, coef, m, a, lam, sel, phi=1.0):
        F11, F12, F21, F22, G1, G2, amb, cn, cs, UE = coef
        js, h, out = self.js[sel], 1.0 / 60.0, {}
        tout = np.nan_to_num(self.c["tout"])[:, None]
        for s in range(max(self.Hs)):
            k = js + s
            dl = h * (lam - 1.0) * phi ** s * amb[k]
            m, a = F11[k] * m + F12 * a + G1[k], F21 * m + (F22[k] - dl) * a + G2[k] + dl * tout[k]
            if s + 1 in self.Hs:
                out[s + 1] = p["w"] * m + (1 - p["w"]) * a
        return out

    def errors(self, p, sel, doc, start=None, phi=1.0):
        coef = self.coef(p)
        P = len(p["w"])
        if start is None:
            M, A = self.states(p, coef, int(self.js[sel].max()) + 1 if sel.any() else 0, doc)
            m, a, lam = M[sel], A[sel], 1.0
        else:
            m, a, lam = (np.repeat(np.asarray(x, float)[self.js[sel]][:, None], P, 1) for x in start)
        pred = self.roll(p, coef, m, a, lam, sel, phi)
        shat = p["w"] * m + (1 - p["w"]) * a
        y, js, Pc = self.c["y"], self.js[sel], self.Pc[sel][:, None]
        return {H: (y[js + H][:, None] - Pc) - (pred[H] - shat) for H in self.Hs}, {H: y[js + H][:, None] - pred[H] for H in self.Hs}

    def residuals(self, th, sel, doc):
        e, _ = self.errors(self.params(th), sel, doc)
        parts = [wt * np.where(self.valid[H][sel][:, None] & np.isfinite(e[H]), e[H], 0.0) / self.sd[H] for H, wt in zip(self.Hs, self.wts)]
        return np.concatenate(parts + [((np.asarray(th) - self.prior) / self.prior_sd).T])

    def fit(self, theta_init, sel, doc):
        y = self.c["y"]
        for H in self.Hs:
            e = (y[self.js + H] - self.Pc)[sel & self.valid[H]]
            self.sd[H] = float(np.sqrt(np.mean(e ** 2))) if len(e) else 1.0
        step, lo, hi = self.f["diff_step"], self.lo, self.hi
        cache = {}

        def batch(x):
            key = x.tobytes()
            if key not in cache:
                sign = np.where(x + step > hi, -1.0, 1.0)
                r = self.residuals(np.vstack([x, x + np.diag(sign * step)]), sel, doc)
                cache.clear()
                cache[key] = (r[:, 0], (r[:, 1:] - r[:, :1]) / (sign * step))
            return cache[key]

        x0 = np.clip(theta_init, lo + 1e-3, hi - 1e-3)
        tol = self.f["tol"]
        sol = least_squares(lambda x: batch(x)[0], x0, jac=lambda x: batch(x)[1], bounds=(lo, hi), x_scale="jac", max_nfev=self.f["max_nfev"], ftol=tol, xtol=tol, gtol=tol)
        at_bound = [n for n, x, a, b in zip(self.names, sol.x, lo, hi) if min(x - a, b - x) < self.f["bound_tol"]]
        return sol.x, {"cost": float(2 * sol.cost), "nfev": int(sol.nfev), "status": int(sol.status), "converged": bool(sol.status > 0), "at_bound": at_bound}

    def doc(self, theta, base):
        p = {k: float(v[0]) for k, v in self.params(np.asarray(theta)[None]).items()}
        return {**base, "params": {k: p[k] for k in ROOM_KEYS}, "c": {k: p[f"c_{k}"] for k in self.kinds}}


def init_theta(rf, d, cfg, rc, ns):
    f = rc["fit"]
    p = {k: v for k, v in f["init"].items()}
    if f["init_onestep"]:
        one, _ = fit(d.slice(0, ns), "rc2", cfg)
        p.update({k: v for k, v in one.p.items() if k in ROOM_KEYS or k.startswith("c_")})
    p["w"] = float(np.clip(p["w"], *f["init_w"]))
    th = np.log([p.get(k, f["init"]["c"]) for k in rf.names])
    return np.clip(th, rf.lo + 0.05, rf.hi - 0.05)


def base_doc(d, cfg, rc, ext=True):
    g = valve_gain(d, cfg)
    f = rc["fit"]
    scale = f["valve_sigma_scale"]
    return {"node": d.node, "struct": "rc2", "neighbours": {k: sorted(set(d.info["neighbour_nodes"].get(k, []))) for k in d.nb}, "valve_gain": g, "sigma_u": sigma_u(d, g, rc) * (scale or 1.0),
            "valve_obs": bool(scale), "noise_r": cfg["noise_r"], "p0": cfg["p0"], "q_r": f["q_r"], "q_a": f["q_a"], "q_um": f["q_um"], "q_ua": f["q_ua"], **(evidence_blocks(d.ext, rc["evidence"]) if ext else {})}


def shock_defaults(rc, **over):
    s = rc["shock"]
    return {"tau_l": s["tau_l"], "hold_min": s["hold_min"], "p_l0": s["p_l0"], "lambda_jump": s["lambda_jump"], "lambda_w": s["lambda_w"], **s["start"], **over}


def load_live_alarms(path):
    p = Path(path)
    rows = []
    for b in raw_batches(p, "events"):
        if "detail" in b.schema.names:
            rows += [(ts, d) for ts, k, d in zip(epoch(b.column("ts").to_pylist()), b.column("kind").to_pylist(), b.column("detail").to_pylist()) if k == "window_open" and d]
    base = p / "parquet" / "events"
    if base.is_dir() and any(base.rglob("part.parquet")):
        sql = f"SELECT epoch(ts), detail FROM read_parquet('{base.as_posix()}/**/part.parquet', hive_partitioning=false, union_by_name=true) WHERE kind::VARCHAR = 'window_open' AND detail IS NOT NULL"
        rows += [(float(a), d) for a, d in duckdb.connect().execute(sql).fetchall()]
    out = set()
    for t, d in rows:
        try:
            j = json.loads(d)
        except (TypeError, ValueError):
            continue
        if j.get("source") == "auto" and j.get("room"):
            out.add((round(t, 3), j["room"]))
    return sorted(out)


class Truth:
    def __init__(self, src, rc):
        self.rc = rc
        self.gt = load_ground_truth(src, rc["hold_min"])
        self.labels = [x for x in load_window_labels(src) if x[0] > 1e6]
        self.live = load_live_alarms(src)

    def spans(self, rooms):
        return window_spans(self.gt, self.labels, rooms, self.rc["shock"]["pad_min"] * 60)

    def opens(self, rooms, lo=-math.inf, hi=math.inf):
        out = [(r, a) for a, b, rs in self.gt for r in rs if r in rooms] + [(r, t) for t, r, o in self.labels if o and r in rooms]
        return sorted((r, a) for r, a in out if lo <= a < hi)

    def denials(self, rooms, lo=-math.inf, hi=math.inf):
        return sorted((r, t) for t, r, o in self.labels if not o and r in rooms and lo <= t < hi)


def label_loss(grid, rec, truth, rc):
    s = rc["shock"]["label"]
    spans, total = truth.spans(grid.names), 0.0
    t0, t1 = grid.times[0], grid.times[-1]
    ev = rc["eval"]
    for r in grid.names:
        pw = np.nan_to_num(rec[r]["p_window"])
        total += s["fa_cost"] * sum(1 for a, b in episodes(pw > ev["pw_flag"], grid.times, ev["min_episode_min"]) if not any(x <= a <= y for x, y in spans[r]))
        for room, a in truth.opens([r], t0, t1):
            i0, i1 = np.searchsorted(grid.times, a - 300.0), np.searchsorted(grid.times, a + rc["shock"]["hit_min"] * 60.0, side="right")
            peak = float(np.max(pw[i0:i1])) if i1 > i0 else 0.0
            total -= s["event_weight"] * math.log(min(max(peak, s["pw_floor"]), 1.0))
    return total


class ShockScores:
    def __init__(self, grid, docs, rc, drop=None, truth=None):
        self.grid, self.docs, self.rc, self.truth = grid, docs, rc, truth
        self.inp = Inputs(grid, docs, drop)
        self.cache = {}

    def __call__(self, trials):
        key = lambda t: tuple(sorted(t.items()))
        todo = list({key(t): t for t in trials if key(t) not in self.cache}.values())
        if todo:
            bh, recs = run_batch(self.grid, self.docs, [shock_defaults(self.rc, **t) for t in todo], self.rc["house"], inp=self.inp, record=self.truth is not None)
            lab = [label_loss(self.grid, recs[i], self.truth, self.rc) if self.truth is not None else 0.0 for i in range(len(todo))]
            cost = self.rc["shock"].get("reset_cost", 0.0)
            nres = [sum(1 for e in bh.events if e[3] == i) for i in range(len(todo))]
            self.cache.update({key(t): float(-ll) + x + cost * n for t, ll, x, n in zip(todo, bh.loglik, lab, nres)})
        return [self.cache[key(t)] for t in trials]


def fit_shock(grid, docs, rc, drop=None, truth=None):
    s = rc["shock"]
    best = dict(s["start"])
    nll = ShockScores(grid, docs, rc, drop, truth)
    axes = [sorted(set(v) | {best[k]}) for k, v in s["grid"].items()]
    full = [dict(best, **dict(zip(s["grid"], c))) for c in itertools.product(*axes)]
    nll(full if len(full) <= s["batch_max"] else [best])
    score = nll([best])[0]
    trace = [(dict(best), score)]
    for _ in range(s["passes"]):
        for key, values in s["grid"].items():
            nll([dict(best, **{key: v}) for v in values if v != best[key]])
            for v in values:
                if v == best[key]:
                    continue
                trial = dict(best, **{key: v})
                sc = nll([trial])[0]
                trace.append((trial, sc))
                if sc < score:
                    best, score = trial, sc
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


def episodes(flag, times, min_len=1):
    out, start = [], None
    for i, f in enumerate(list(flag) + [False]):
        if f and start is None:
            start = i
        if not f and start is not None:
            if i - start >= min_len:
                out.append((times[start], times[i - 1]))
            start = None
    return out


def detection(grid, rec, lam_w, gt, labels, rc):
    hit_s, pad_s = rc["shock"]["hit_min"] * 60, rc["shock"]["pad_min"] * 60
    res = {"hits": 0, "events": 0, "false_episodes": 0, "no_flagged": 0, "no_labels": 0, "yes_hit": 0, "yes_labels": 0, "latency_min": [], "false_per_day": {}, "missed": []}
    allspans = window_spans(gt, labels, grid.names, pad_s)
    flags = {}
    for r in grid.names:
        lam, sd = rec[r]["lam"], np.maximum(rec[r]["lam_std"], 1e-6)
        with np.errstate(invalid="ignore"):
            flags[r] = np.where(np.isfinite(lam), 1.0 - norm_cdf_np((lam_w - np.nan_to_num(lam)) / sd), 0.0) > 0.5
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
            else:
                res["missed"].append([r, a])
    for t, r, o in labels:
        if r not in flags:
            continue
        i = np.searchsorted(grid.times, t)
        around = flags[r][max(i - int(hit_s / grid.dt), 0):i + int(hit_s / grid.dt) + 1]
        if o:
            res["yes_labels"] += 1
            res["yes_hit"] += int(around.any())
            if not around.any():
                res["missed"].append([r, t])
        else:
            res["no_labels"] += 1
            res["no_flagged"] += int(around.any())
    for r in grid.names:
        n_false = sum(1 for a, b in episodes(flags[r], grid.times) if not any(x <= a <= y for x, y in allspans[r]))
        res["false_episodes"] += n_false
        days = max(np.isfinite(rec[r]["lam"]).sum() * grid.dt / 86400.0, 1e-9)
        res["false_per_day"][r] = n_false / days
    return res


def tune_windows(grid, rec, rc, gt, labels):
    s = rc["shock"]
    if not gt and not labels:
        return s["lambda_w"], s["lambda_jump"], None
    rows = []
    for lw in s["lambda_w_grid"]:
        d = detection(grid, rec, lw, gt, labels, rc)
        rows.append((d["hits"] + d["yes_hit"] - s["fa_weight"] * (d["false_episodes"] + d["no_flagged"]), -(d["false_episodes"] + d["no_flagged"]), lw, d))
    cands = [{"lambda_w": lw, "score": sc, **d} for sc, _, lw, d in rows]
    if not any(r[3]["hits"] + r[3]["yes_hit"] for r in rows):
        return s["lambda_w"], s["lambda_jump"], {"candidates": cands, "refused": True, "warning": "no lambda_w detects any known opening; window threshold not exported, config default kept"}
    best = max(rows, key=lambda r: r[:3])
    lam_j = []
    for a, b, open_rooms in gt:
        i0, i1 = np.searchsorted(grid.times, a + 600), np.searchsorted(grid.times, min(b, a + 2400), side="right")
        for r in open_rooms:
            if r in rec and i1 > i0:
                v = rec[r]["lam"][i0:i1]
                v = v[np.isfinite(v)]
                if len(v):
                    lam_j.append(float(np.median(v)))
    lj = float(np.clip(np.median(lam_j), best[2] + 0.25, 8.0)) if lam_j else s["lambda_jump"]
    return best[2], lj, {"candidates": cands, "refused": False, "lambda_j_events": len(lam_j)}


class FoldFit:
    def __init__(self, rooms, full, cfg, rc):
        self.rooms, self.full, self.cfg, self.rc = rooms, full, cfg, rc
        self.rfs = {}
        for d in rooms:
            night = d.night
            self.rfs[d.room] = RoomFit(full.cols[d.room], full.n, sorted(d.nb), rc, min(night.sum(), (~night).sum()) >= cfg["min_night_steps"])

    def run(self, cut, warm=None, ext=False):
        docs, th, info = {}, {}, {}
        for d in self.rooms:
            ns = max(cut - int(round((d.t0 - self.full.t0) / self.full.dt)), 0)
            rf = self.rfs[d.room]
            base = base_doc(d.slice(0, ns), self.cfg, self.rc, ext)
            x0 = warm[d.room] if warm else init_theta(rf, d, self.cfg, self.rc, ns)
            th[d.room], info[d.room] = rf.fit(x0, rf.js + max(rf.Hs) < cut, base)
            docs[d.room] = rf.doc(th[d.room], base)
            info[d.room]["names"] = rf.names
        return docs, th, info


def calibrate_air(grid, docs, rc, lo, hi, rec):
    out, floor = {}, rc["evidence"]["air"]["noise"]
    for r in grid.names:
        arrs = grid.cols[r]["ya"]
        if not arrs:
            continue
        blocks = []
        for ya in arrs:
            sel = np.isfinite(ya) & np.isfinite(rec[r]["a"]) & (np.arange(grid.n) >= lo) & (np.arange(grid.n) < hi)
            if sel.sum() < rc["evidence"]["air"]["min_pairs"]:
                break
            res = ya[sel] - rec[r]["a"][sel]
            off = float(np.median(res))
            blocks.append({"offset": off, "noise": float(max(1.4826 * np.median(np.abs(res - off)), floor)), "pairs": int(sel.sum())})
        else:
            out[r] = blocks
    return out
