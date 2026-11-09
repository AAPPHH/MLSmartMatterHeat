import json
import math
from dataclasses import dataclass, field, replace
from datetime import datetime, timezone
from pathlib import Path
from zoneinfo import ZoneInfo
import duckdb
import numpy as np
import onnx
import onnxruntime as ort
from onnx import TensorProto, helper, numpy_helper
from onnxmltools.convert.xgboost.operator_converters.XGBoost import convert_xgboost
from scipy.optimize import least_squares
from skl2onnx import convert_sklearn, update_registered_converter
from skl2onnx.common.data_types import FloatTensorType
from skl2onnx.common.shape_calculator import calculate_linear_regressor_output_shapes
from xgboost import XGBRegressor
from heattrain.thermal import build_rooms, fit, fit_room, kalman, load_events, load_lake, read_arrows, step_terms, valve_gain

update_registered_converter(XGBRegressor, "XGBoostXGBRegressor", calculate_linear_regressor_output_shapes, convert_xgboost)

AMBIENT = ("nvme_c", "pkg_w", "die_c")
BASE = ("outdoor_c", "radiation_wm2", "hour_sin", "hour_cos", "others_heating", "minutes_since_boost")


def lag_list(cfg):
    return list(range(0, cfg["history_min"] + 1, cfg["lag_step_min"]))


def feature_names(cfg, rooms, kind):
    lags = lag_list(cfg)
    names = [f"sensor_lag_{l}" for l in lags] + [f"valve_lag_{l}" for l in lags] + list(BASE)
    if kind == "residual":
        names += ["phys_sensor_pred", "phys_air"]
    return names + [f"ambient_{a}" for a in AMBIENT] + [f"room_{r}" for r in rooms]


def since_boost(V, cfg):
    cap = cfg["boost_cap_min"]
    cross = (V[:, :cap] >= cfg["boost_valve"]) & (V[:, 1:cap + 1] < cfg["boost_valve"])
    return np.where(cross.any(1), cross.argmax(1), cap).astype(float)


def feature_matrix(cfg, names, ctx, room):
    C = len(ctx["S"])
    cols = []
    for n in names:
        if n.startswith("sensor_lag_"):
            c = ctx["S"][:, int(n[11:])]
        elif n.startswith("valve_lag_"):
            c = ctx["V"][:, int(n[10:])]
        elif n == "hour_sin":
            c = np.sin(2 * np.pi * ctx["hour"] / 24)
        elif n == "hour_cos":
            c = np.cos(2 * np.pi * ctx["hour"] / 24)
        elif n == "minutes_since_boost":
            c = since_boost(ctx["V"], cfg)
        elif n.startswith("room_"):
            c = np.full(C, float(n[5:] == room))
        else:
            c = ctx.get(n, np.nan)
        cols.append(np.broadcast_to(np.asarray(c, float), (C,)))
    return np.column_stack(cols).astype(np.float32)


def local_hour(t, tz):
    z = ZoneInfo(tz)
    keys = (t // 900).astype(np.int64)
    off = {k: datetime.fromtimestamp(int(k) * 900, z).utcoffset().total_seconds() for k in np.unique(keys)}
    return ((t + np.array([off[k] for k in keys])) % 86400) / 3600


def causal(y):
    idx = np.maximum.accumulate(np.where(np.isfinite(y), np.arange(len(y)), -1))
    return np.where(idx >= 0, y[np.clip(idx, 0, None)], np.nan)


def windows(a, ks, length, shift):
    return a[ks[:, None] - shift - np.arange(length)[None, :]]


def load_ambient(src):
    p = Path(src)
    if (p / "parquet" / "ambient").is_dir() and any((p / "parquet" / "ambient").rglob("part.parquet")):
        rows = duckdb.connect().execute(f"SELECT epoch(ts), sensor::VARCHAR, value FROM read_parquet('{(p / 'parquet' / 'ambient').as_posix()}/**/part.parquet', hive_partitioning=false, union_by_name=true)").fetchall()
    else:
        t = read_arrows(p, "ambient")
        rows = [] if t is None else [(a.timestamp(), b, c) for a, b, c in zip(t.column("ts").to_pylist(), t.column("sensor").to_pylist(), t.column("value").to_pylist())]
    out = {}
    for ts, s, v in rows:
        if v is not None:
            out.setdefault(s, []).append((float(ts), float(v)))
    return {s: tuple(np.array(x) for x in zip(*sorted(v))) for s, v in out.items()}


def ambient_ctx(ambient, t, stale_s):
    out = {}
    for s in AMBIENT:
        if s not in (ambient or {}):
            continue
        at, av = ambient[s]
        idx = np.searchsorted(at, t, side="right") - 1
        ok = (idx >= 0) & (t - at[np.clip(idx, 0, None)] <= stale_s)
        out[f"ambient_{s}"] = np.where(ok, av[np.clip(idx, 0, None)], np.nan)
    return out


@dataclass
class RoomML:
    d: object
    doc: dict
    valid: bool
    trainable: bool = False
    reasons: list = field(default_factory=list)
    offset: float = 0.0
    n_cold: int = 0
    offset_tr: float = 0.0
    doc_tr: dict = None
    xs_tr: np.ndarray = None
    lab_tr: np.ndarray = None
    xs: np.ndarray = None
    kair: np.ndarray = None
    pred1: np.ndarray = None
    yc: np.ndarray = None
    hour: np.ndarray = None
    others: np.ndarray = None
    amb: dict = field(default_factory=dict)
    ks: np.ndarray = None
    F: np.ndarray = None
    lab: np.ndarray = None
    tgt: np.ndarray = None
    test: np.ndarray = None
    lat_split: np.ndarray = None
    lat_final: np.ndarray = None
    n_decay: int = 0

    @property
    def name(self):
        return self.d.room

    def times(self):
        return grid_times(self.d)


def physics_terms(doc, d):
    return [np.broadcast_to(np.asarray(a, float), (d.n,)) for a in step_terms(doc["params"], doc["structure"], d.u, d.tout, d.wind, d.rad, d.night, d.nb, d.h)]


def sensor_of(w, x1, x2):
    return w * x1 + (1 - w) * x2


def grid_times(d):
    return d.t0 + np.arange(d.n) * d.dt


def others_heating(rooms, cfg):
    out = {}
    for r in rooms:
        t = grid_times(r)
        tot = np.zeros(len(t))
        for o in rooms:
            if o is not r:
                i = np.floor((t - o.t0) / o.dt).astype(int)
                tot += np.where((i >= 0) & (i < o.n), o.u[np.clip(i, 0, o.n - 1)] > cfg["others_valve"], 0.0)
        out[r.room] = tot
    return out


def cold_mask(u, cfg):
    cold = np.zeros(len(u), bool)
    run = 0
    for k, v in enumerate(u):
        run = run + 1 if v <= cfg["cold_valve"] else 0
        if run > cfg["cold_min"]:
            cold[k] = True
    return cold


def decay_fit(m, y, cfg):
    ok = np.isfinite(y)
    if ok.sum() < cfg["decay_min_points"]:
        return None
    lo, hi = cfg["decay_tau"]
    dr = cfg["decay_drift_k_per_min"]
    yo, mo = y[ok], m[ok]
    f = lambda q: q[0] + q[1] * mo + q[2] * np.exp(-mo / q[3]) - yo
    x0 = [yo[-1], 0.0, max(yo[0] - yo[-1], 0.0), min(max(15.0, lo + 1), hi - 1)]
    sol = least_squares(f, x0, bounds=([-50, -dr, 0.0, lo], [100, dr, 20.0, hi]), x_scale=[1.0, 0.01, 1.0, 10.0])
    rmse = float(np.sqrt(np.mean(sol.fun ** 2)))
    if rmse > cfg["decay_max_rmse"] or sol.x[2] < cfg["decay_min_amp"]:
        return None
    return sol.x, rmse


def decay_labels(R, cfg):
    d, test = R.d, R.test
    lab = np.full(d.n, np.nan)
    count = 0
    k = cfg["decay_lookback_min"]
    while k < d.n:
        if d.u[k] <= cfg["cold_valve"] and d.u[max(0, k - cfg["decay_lookback_min"]):k].max() > cfg["decay_valve_from"]:
            e = k
            while e < d.n and e - k < cfg["decay_max_min"] and d.u[e] <= cfg["cold_valve"] and test[e] == test[k]:
                e += 1
            if e - k >= cfg["decay_min_min"]:
                fit = decay_fit(np.arange(e - k, dtype=float), d.y[k:e], cfg)
                if fit:
                    q = fit[0]
                    m = np.arange(e - k, dtype=float)
                    lab[k:e] = q[0] + q[1] * m
                    count += 1
            k = e
        k += 1
    return lab, count


def prepare_room(d, doc, cfg, tcfg, others, ambient, test):
    span_h = d.n * d.dt / 3600
    p = doc["params"]
    m = doc["metrics"][doc["structure"]]
    reasons = []
    if not all(np.isfinite(v) for v in p.values()) or m["rmse_horizon"] is None:
        reasons.append("physics fit not finite or without held-out horizon error")
    if d.u.std() < tcfg["min_valve_std"]:
        reasons.append(f"valve demand std {d.u.std():.3f} < {tcfg['min_valve_std']}")
    if span_h < cfg["min_train_hours"]:
        reasons.append(f"span {span_h:.1f} h < {cfg['min_train_hours']} h minimum for labels")
    trainable = not reasons
    if span_h < cfg["min_valid_hours"]:
        reasons.append(f"span {span_h:.1f} h < {cfg['min_valid_hours']} h required for a valid room")
    if trainable and m["rmse_persistence"] is not None and m["rmse_horizon"] > m["rmse_persistence"] * cfg["valid_max_persistence_ratio"]:
        reasons.append(f"physics {m['rmse_horizon']:.3f} K does not beat persistence {m['rmse_persistence']:.3f} K at {m['horizon_min']} min")
    R = RoomML(d, doc, not reasons, trainable, reasons)
    R.test = test
    R.others, R.hour = others, local_hour(R.times(), tcfg["timezone"])
    R.amb = ambient_ctx(ambient, R.times(), cfg["ambient_stale_s"])
    R.yc = causal(d.y)
    if not R.trainable:
        return R
    ks, v, s, xs, ps = kalman(p, d, doc["structure"], tcfg, collect=True)
    R.xs, R.kair = xs, xs[:, 1]
    A = physics_terms(doc, d)
    w = p["w"] if doc["structure"] == "rc2" else 0.0
    R.pred1 = sensor_of(w, A[0] * xs[:, 0] + A[1] * xs[:, 1] + A[4], A[2] * xs[:, 0] + A[3] * xs[:, 1] + A[5])
    cold = cold_mask(d.u, cfg)
    R.n_cold = int(cold.sum())
    R.offset = float(np.nanmedian(d.yi[cold] - R.kair[cold])) if R.n_cold else 0.0
    cold_tr = cold & ~test
    R.offset_tr = float(np.nanmedian(d.yi[cold_tr] - R.kair[cold_tr])) if cold_tr.any() else R.offset
    dtr = replace(d, y=np.where(test, np.nan, d.y), yi=np.where(test, np.nan, d.yi))
    try:
        ftr, _ = fit(dtr, doc["structure"], tcfg)
        R.doc_tr = dict(doc, params=ftr.p, valve_gain=valve_gain(dtr, tcfg))
    except Exception:
        R.doc_tr = doc
    R.xs_tr = kalman(R.doc_tr["params"], d, doc["structure"], tcfg, collect=True)[3]
    return R


def fill_rows(R, cfg, names):
    d = R.d
    lo = cfg["boost_cap_min"] + 1
    R.ks = np.arange(lo, d.n - 1)
    if len(R.ks) < 1:
        R.F = np.zeros((0, len(names)), np.float32)
        R.lab = R.lab_tr = R.tgt = np.zeros(0)
        return
    parts = []
    for a in range(0, len(R.ks), cfg["chunk"]):
        k = R.ks[a:a + cfg["chunk"]]
        ctx = {"S": windows(R.yc, k, cfg["history_min"] + 1, 0), "V": windows(d.u, k, cfg["boost_cap_min"] + 1, 0), "hour": R.hour[k], "outdoor_c": d.tout[k], "radiation_wm2": d.rad[k],
               "others_heating": R.others[k], "phys_sensor_pred": R.pred1[k], "phys_air": R.kair[k], **{n: x[k] for n, x in R.amb.items()}}
        parts.append(feature_matrix(cfg, names, ctx, R.name))
    R.F = np.vstack(parts)
    cold = cold_mask(d.u, cfg)
    dl, cnt = decay_labels(R, cfg)
    def labels(offset):
        lab = np.full(d.n, np.nan)
        lab[cold] = d.yi[cold] - offset
        return np.where(np.isfinite(dl), dl - offset, lab)[R.ks] - R.yc[R.ks]
    R.lab, R.lab_tr = labels(R.offset), labels(R.offset_tr)
    R.tgt = d.yi[R.ks + 1] - R.pred1[R.ks]
    R.n_decay = cnt


def split_masks(rooms_data, cfg):
    times = {d.room: grid_times(d) for d in rooms_data}
    days = sorted({int(t // 86400) for x in times.values() for t in x[::60]})
    if len(days) >= cfg["min_split_days"]:
        test_days = set(days[-max(1, int(round(len(days) * cfg["holdout_frac"]))):])
        return "days", {n: np.array([int(t // 86400) in test_days for t in x]) for n, x in times.items()}
    return "blocks", {n: (np.floor(x / (cfg["block_min"] * 60)).astype(int) % 4) == 3 for n, x in times.items()}


class Net:
    def __init__(self, model, cols, lag=None):
        self.model, self.cols, self.lag = model, cols, lag

    def __call__(self, F):
        X = F[:, self.cols]
        y = self.model.predict(X)
        return y + X[:, self.lag] if self.lag is not None else y


def xgb(cfg, key):
    return XGBRegressor(base_score=0.0, n_jobs=1, random_state=cfg["seed"], **cfg[key])


def fit_nets(cfg, rooms, names, lat_names, full):
    ci = {n: i for i, n in enumerate(names)}
    lat_cols = [ci[n] for n in lat_names]
    Xl, yl, Xr, yr = [], [], [], []
    for R in rooms:
        if not R.trainable or len(R.ks) == 0:
            continue
        m = np.ones(len(R.ks), bool) if full else ~R.test[R.ks]
        lab = R.lab if full else R.lab_tr
        lm = m & np.isfinite(lab)
        Xl.append(R.F[lm][:, lat_cols])
        yl.append(lab[lm])
        rm = m & np.isfinite(R.tgt) & (np.abs(R.tgt) < cfg["max_abs_target"]) & np.isfinite(R.F[:, 0])
        Xr.append(R.F[rm])
        yr.append(R.tgt[rm])
    nl, nr = sum(len(y) for y in yl), sum(len(y) for y in yr)
    out = {"n_latent": nl, "n_residual": nr, "latent": None, "residual": None}
    if nl >= cfg["min_latent_labels"]:
        out["latent"] = Net(xgb(cfg, "xgb_latent").fit(np.vstack(Xl), np.concatenate(yl)), lat_cols, lat_names.index("sensor_lag_0"))
        if nr >= cfg["min_residual_rows"]:
            out["residual"] = Net(xgb(cfg, "xgb_residual").fit(np.vstack(Xr), np.concatenate(yr)), list(range(len(names))))
    return out


def rollout(cfg, tcfg, doc, room, names, x0, S0, V0, env, policy, H, latent=None, residual=None):
    p, struct = doc["params"], doc["structure"]
    w = p["w"] if struct == "rc2" else 0.0
    h = tcfg["dt_s"] / 3600.0
    C = len(x0)
    x1, x2 = x0[:, 0].astype(float).copy(), x0[:, 1].astype(float).copy()
    S, V = S0.astype(float).copy(), V0.astype(float).copy()
    air, sens, us = np.zeros((C, H)), np.zeros((C, H + 1)), np.zeros((C, H))
    sens[:, 0] = sensor_of(w, x1, x2)
    for k in range(H):
        sen = sensor_of(w, x1, x2)
        if k > 0:
            S = np.concatenate([sen[:, None], S[:, :-1]], 1)
        u = np.broadcast_to(np.asarray(policy(k, sen), float), (C,))
        V = np.concatenate([u[:, None], V[:, :-1]], 1)
        a = [np.broadcast_to(np.asarray(z, float), (C,)) for z in step_terms(p, struct, u, env["tout"][:, k], env["wind"][:, k], env["rad"][:, k], env["night"][:, k],
                                                                        {kd: (n[:, k], s[:, k]) for kd, (n, s) in env["nb"].items()}, h)]
        n1, n2 = a[0] * x1 + a[1] * x2 + a[4], a[2] * x1 + a[3] * x2 + a[5]
        pred = sensor_of(w, n1, n2)
        if latent is not None or residual is not None:
            ctx = {"S": S, "V": V, "hour": env["hour"][:, k], "outdoor_c": env["tout"][:, k], "radiation_wm2": env["rad"][:, k], "others_heating": env["others"][:, k],
                   "phys_sensor_pred": pred, "phys_air": x2, **{n: x[:, k] for n, x in env.get("amb", {}).items()}}
            F = feature_matrix(cfg, names, ctx, room)
        air[:, k] = latent(F) if latent is not None else x2
        if residual is not None:
            dl = cfg["residual_feedback"] * residual(F).astype(float)
            n1, n2 = n1 + dl, n2 + dl
        x1, x2 = n1, n2
        sens[:, k + 1] = sensor_of(w, x1, x2)
        us[:, k] = u
    return {"air": air, "sensor": sens, "u": us}


def env_from(R, ks, H):
    d = R.d
    idx = np.clip(ks[:, None] + np.arange(H)[None, :], 0, d.n - 1)
    env = {"tout": d.tout[idx], "wind": d.wind[idx], "rad": d.rad[idx], "night": d.night[idx], "hour": R.hour[idx], "others": R.others[idx],
           "nb": {k: (n[idx], s[idx]) for k, (n, s) in d.nb.items()}, "amb": {n: x[idx] for n, x in R.amb.items()}}
    return env


def tile_env(env, C):
    t = lambda a: np.repeat(a, C, axis=0) if len(a) == 1 else a
    return {k: ({kd: (t(v[0]), t(v[1])) for kd, v in x.items()} if k == "nb" else {kd: t(v) for kd, v in x.items()} if k == "amb" else t(x)) for k, x in env.items()}


def origin_state(cfg, R, ks, xs=None):
    return (R.xs if xs is None else xs)[ks], windows(R.yc, ks, cfg["history_min"] + 1, 0), windows(R.d.u, ks, cfg["boost_cap_min"] + 1, 1)


def recorded(R, ks):
    return lambda k, sen: R.d.u[np.clip(ks + k, 0, R.d.n - 1)]


def rmse(a):
    a = np.asarray(a, float)
    a = a[np.isfinite(a)]
    return float(np.sqrt(np.mean(a ** 2))) if len(a) else None


def holdout_eval(cfg, tcfg, rooms, names, nets, split):
    Hm = max(cfg["eval_horizons"])
    err = {h: {"physics": [], "residual": []} for h in cfg["eval_horizons"]}
    n_or = 0
    if nets["residual"] is None:
        return {"split": split, "n_origins": 0, "horizons": {}}
    for R in rooms:
        if not R.trainable or R.d.n < Hm + cfg["boost_cap_min"] + 3:
            continue
        ok = np.lib.stride_tricks.sliding_window_view(R.test, Hm + 1).all(1)
        ks = np.array([k for k in range(cfg["boost_cap_min"] + 1, R.d.n - Hm - 1, cfg["eval_stride"]) if ok[k]], int)
        if len(ks) == 0:
            continue
        x0, S0, V0 = origin_state(cfg, R, ks)
        env = env_from(R, ks, Hm)
        for key, res in (("physics", None), ("residual", nets["residual"])):
            out = rollout(cfg, tcfg, R.doc, R.name, names, x0, S0, V0, env, recorded(R, ks), Hm, residual=res)
            for h in cfg["eval_horizons"]:
                err[h][key].append(out["sensor"][:, h] - R.d.yi[ks + h])
        n_or += len(ks)
    hz = {}
    for h, e in err.items():
        if e["physics"]:
            hz[str(h)] = {"physics_rmse": rmse(np.concatenate(e["physics"])), "residual_rmse": rmse(np.concatenate(e["residual"])), "n": int(sum(len(x) for x in e["physics"]))}
    return {"split": split, "n_origins": n_or, "horizons": hz}


def find_cycles(R, cfg):
    d = R.d
    starts, run = [], cfg["cycle_closed_min"]
    for k in range(d.n):
        if d.u[k] >= cfg["boost_valve"] and run >= cfg["cycle_closed_min"]:
            starts.append(k)
        run = run + 1 if d.u[k] < cfg["boost_valve"] else 0
    out = []
    for i, s in enumerate(starts):
        e = min(s + cfg["cycle_window_min"], (starts[i + 1] - 1) if i + 1 < len(starts) else d.n - 2, d.n - 2)
        if s < cfg["boost_cap_min"] + 1 or e - s < cfg["cycle_min_len"]:
            continue
        sp = d.sp[s:e + 1]
        if not np.isfinite(sp).any():
            continue
        out.append((s, e, float(np.nanmedian(sp))))
    return out


def cycle_metrics(air, u, target, cfg):
    over = max(0.0, float(np.max(air)) - target)
    hit = np.nonzero(air >= target - cfg["target_tol"])[0]
    return {"overshoot_c": over, "minutes_to_target_0_3": float(hit[0]) if len(hit) else None, "valve_over_50_min": float(np.sum(u > 0.5))}


def latent_series(R, net):
    arr = np.full(R.d.n, np.nan)
    if net is not None and len(R.ks):
        arr[R.ks] = net(R.F)
    return arr


def evaluate_cycles(cfg, tcfg, rooms, names, split_nets, final_nets):
    bt, table = {}, {}
    for R in rooms:
        if not R.trainable:
            continue
        cyc = find_cycles(R, cfg)
        lat_split, lat_final = latent_series(R, split_nets["latent"]), latent_series(R, final_nets["latent"])
        rows = []
        for s, e, tgt in cyc:
            seg = slice(s, e + 1)
            rows.append({"start_ts": R.d.t0 + s * R.d.dt, "minutes": e - s, "target": tgt, "oos": bool(R.test[s:e + 1].all()), "grey_box": cycle_metrics(R.kair[seg], R.d.u[seg], tgt, cfg),
                         "residual": cycle_metrics(lat_final[seg], R.d.u[seg], tgt, cfg) if np.isfinite(lat_final[seg]).all() else None})
        oos = [i for i, r in enumerate(rows) if r["oos"]] if split_nets["residual"] is not None else []
        in_sample = not oos
        use = oos or list(range(len(rows)))
        nets, lat_est = (final_nets, lat_final) if in_sample else (split_nets, lat_split)
        res = {"grey_box": None, "residual": None, "grey_box_air": None, "residual_air": None, "cycles": len(use), "days": round(R.d.n * R.d.dt / 86400, 2), "in_sample": in_sample}
        if use:
            sel = np.array([cyc[i][0] for i in use])
            Hc = max(cyc[i][1] - cyc[i][0] for i in use)
            x0, S0, V0 = origin_state(cfg, R, sel, R.xs_tr)
            env = env_from(R, sel, Hc)
            for key, ln, rn in (("grey_box", None, None), ("residual", nets["latent"], nets["residual"])):
                if key == "residual" and rn is None:
                    continue
                out = rollout(cfg, tcfg, R.doc_tr, R.name, names, x0, S0, V0, env, recorded(R, sel), Hc, latent=ln, residual=rn)
                es, ea = [], []
                for j, i in enumerate(use):
                    s, e, _ = cyc[i]
                    L = e - s
                    es.append(out["sensor"][j, 1:L + 1] - R.d.yi[s + 1:s + L + 1])
                    ea.append(out["air"][j, :L] - (R.xs_tr[:, 1] if key == "grey_box" else lat_est)[s:s + L])
                res[key], res[key + "_air"] = rmse(np.concatenate(es)), rmse(np.concatenate(ea))
        bt[R.name], table[R.name] = res, rows
    return bt, table


def boost_policy(t_off, target, gain):
    return lambda k, sen: np.where(k < t_off, 1.0, np.clip(gain * (target - sen), 0.0, 1.0))


def choose_t_off(peak, cand, target, tol):
    ok = peak <= target + tol
    i = int(np.argmin(np.where(ok, np.abs(peak - target), np.inf))) if ok.any() else int(np.argmin(peak))
    return i


def optimum_start(cfg, tcfg, doc, room, names, x0, S0, V0, env, target, latent=None, residual=None):
    cand = np.arange(0, cfg["toff_max"] + 1, cfg["toff_step"])
    C = len(cand)
    H = cfg["horizon_min"]
    rep = lambda a: np.repeat(a, C, axis=0) if len(a) == 1 else a
    env = tile_env(env, C)
    out = rollout(cfg, tcfg, doc, room, names, rep(x0), rep(S0), rep(V0), env, boost_policy(cand.astype(float), target, doc["valve_gain"]), H, latent, residual)
    peak = out["air"].max(1)
    i = choose_t_off(peak, cand, target, cfg["overshoot_tol"])
    return {"t_off_min": int(cand[i]), "predicted_peak_c": float(peak[i]), "peaks": peak, "air": out["air"][i], "sensor": out["sensor"][i], "candidates": cand}


def jsonable(x):
    if isinstance(x, dict):
        return {k: jsonable(v) for k, v in x.items()}
    if isinstance(x, (list, tuple)):
        return [jsonable(v) for v in x]
    if isinstance(x, np.ndarray):
        return jsonable(x.tolist())
    if isinstance(x, (float, np.floating)):
        return None if not np.isfinite(x) else float(x)
    if isinstance(x, (np.integer,)):
        return int(x)
    if isinstance(x, np.bool_):
        return bool(x)
    return x


def to_onnx(model, n, output, lag=None):
    proto = convert_sklearn(model, initial_types=[("x", FloatTensorType([None, n]))], target_opset={"": 15, "ai.onnx.ml": 3})
    g = proto.graph
    old = g.output[0].name
    new = "raw" if lag is not None else output
    for node in g.node:
        for i, x in enumerate(node.output):
            if x == old:
                node.output[i] = new
    del g.output[:]
    if lag is not None:
        g.initializer.extend([numpy_helper.from_array(np.array([lag], np.int64), "lag_s"), numpy_helper.from_array(np.array([lag + 1], np.int64), "lag_e"), numpy_helper.from_array(np.array([1], np.int64), "lag_a")])
        g.node.extend([helper.make_node("Slice", ["x", "lag_s", "lag_e", "lag_a"], ["lag0"]), helper.make_node("Add", ["raw", "lag0"], [output])])
    g.output.extend([helper.make_tensor_value_info(output, TensorProto.FLOAT, [None, 1])])
    onnx.checker.check_model(proto)
    return proto.SerializeToString()


def parity_input(cfg, F, rng):
    X = F[: cfg["parity_rows"]].copy()
    hole = rng.random(X.shape) < cfg["parity_nan_frac"]
    hole[:, 0] = False
    X[hole] = np.nan
    return X


def export_net(cfg, net, n_full, path, output, X):
    sub = X[:, net.cols]
    blob = to_onnx(net.model, len(net.cols), output, net.lag)
    got = ort.InferenceSession(blob, providers=["CPUExecutionProvider"]).run([output], {"x": sub})[0][:, 0]
    err = float(np.abs(got - net(X)).max())
    if not (np.isfinite(got).all() and err <= cfg["parity_tol"]):
        raise RuntimeError(f"onnx parity {output}: {err}")
    Path(path).write_bytes(blob)
    return err


def build_bundle(config, rooms_data, ambient, out_root, version=None, src=""):
    cfg, tcfg = config["thermalml"], config["thermal"]
    version = version or datetime.now(timezone.utc).strftime("%Y%m%dT%H%M%SZ")
    others = others_heating(rooms_data, cfg)
    split, tests = split_masks(rooms_data, cfg)
    rooms, warn = [], {}
    for d in rooms_data:
        try:
            doc = fit_room(d, tcfg)
            R = prepare_room(d, doc, cfg, tcfg, others[d.room], ambient, tests[d.room])
        except Exception as e:
            warn[d.room] = f"physics fit failed: {e}"
            R = RoomML(d, None, False, others=others[d.room], test=tests[d.room])
            R.hour, R.yc = local_hour(R.times(), tcfg["timezone"]), causal(d.y)
        rooms.append(R)
    index = [R.name for R in rooms]
    names, lat_names = feature_names(cfg, index, "residual"), feature_names(cfg, index, "latent")
    for R in rooms:
        if R.trainable:
            fill_rows(R, cfg, names)
    for R in rooms:
        if R.trainable and not len(R.ks):
            R.valid = R.trainable = False
            R.reasons.append("no usable rows after history window")
    nets_split = fit_nets(cfg, rooms, names, lat_names, False)
    nets_final = fit_nets(cfg, rooms, names, lat_names, True)
    metrics = {"split": split, "n_latent_labels": nets_final["n_latent"], "n_residual_rows": nets_final["n_residual"], "offsets": {R.name: {"sensor_offset_c": R.offset, "cold_minutes": R.n_cold} for R in rooms if R.trainable}}
    evals = {g: holdout_eval(dict(cfg, residual_feedback=g), tcfg, rooms, names, nets_split, split) for g in cfg["feedback_grid"]}
    score = lambda e: float(np.mean([v["residual_rmse"] for v in e["horizons"].values()])) if e["horizons"] else math.inf
    best = min(evals, key=lambda g: score(evals[g]))
    metrics["residual"] = dict(evals[best], feedback=best, by_feedback={str(g): {h: v["residual_rmse"] for h, v in e["horizons"].items()} for g, e in evals.items()})
    if not evals[best]["horizons"]:
        best = cfg["residual_feedback"]
    cfg = dict(cfg, residual_feedback=best)
    if nets_split["latent"] is not None:
        lat = {"label_rmse_test": [], "kalman_agreement_rmse_test": [], "kalman_agreement_rmse_hot_test": []}
        ci = {n: i for i, n in enumerate(names)}
        for R in rooms:
            if not R.trainable or not len(R.ks):
                continue
            t = R.test[R.ks]
            if not t.any():
                continue
            la = nets_split["latent"](R.F[t])
            lat["kalman_agreement_rmse_test"].append(la - R.kair[R.ks][t])
            hot = R.F[t][:, ci["valve_lag_0"]] > 0.5
            lat["kalman_agreement_rmse_hot_test"].append((la - R.kair[R.ks][t])[hot])
            lm = np.isfinite(R.lab_tr[t])
            lat["label_rmse_test"].append(la[lm] - (R.lab_tr[t][lm] + R.yc[R.ks][t][lm]))
        metrics["latent"] = {k: rmse(np.concatenate(v)) if v else None for k, v in lat.items()}
    metrics["backtest"], table = evaluate_cycles(cfg, tcfg, rooms, names, nets_split, nets_final)
    out = Path(out_root) / version
    out.mkdir(parents=True, exist_ok=True)
    avail = nets_final["latent"] is not None and nets_final["residual"] is not None
    files = {"latent": None, "residual": None}
    parity = {}
    if avail:
        X = np.vstack([R.F[: cfg["parity_rows"]] for R in rooms if R.trainable and len(R.ks)])
        X = parity_input(cfg, X, np.random.default_rng(cfg["seed"]))
        parity["room_temp"] = export_net(cfg, nets_final["latent"], len(names), out / "latent.onnx", "room_temp", X)
        parity["delta"] = export_net(cfg, nets_final["residual"], len(names), out / "residual.onnx", "delta", X)
        files = {"latent": {"file": "latent.onnx", "features": lat_names}, "residual": {"file": "residual.onnx", "features": names, "feedback": best}}
    else:
        metrics["residual_unavailable"] = f"latent labels {nets_final['n_latent']}, residual rows {nets_final['n_residual']}"
    metrics["onnx_parity_max_abs"] = parity
    metrics["backtest_note"] = "rmse of predicted vs measured sensor over heat-up cycles (out-of-sample unless in_sample=true); *_air vs own air estimate"
    bundle = {"version": version, "created": datetime.now(timezone.utc).isoformat(timespec="seconds"), "dt_s": int(tcfg["dt_s"]), "history_min": cfg["history_min"], "lag_step_min": cfg["lag_step_min"],
              "rooms": {R.name: {"physics": R.doc, "sensor_offset_c": R.offset, "valid": bool(R.valid), "trainable": bool(R.trainable), "reasons": R.reasons} for R in rooms}, "room_index": index,
              "latent": files["latent"], "residual": files["residual"], "metrics": metrics, "warnings": warn, "source": str(src)}
    (out / "bundle.json").write_text(json.dumps(jsonable(bundle), indent=1))
    (out / "cycles.json").write_text(json.dumps(jsonable(table), indent=1))
    return out, bundle, {"rooms": rooms, "names": names, "lat_names": lat_names, "nets": nets_final, "cfg": cfg}


def thermalml(config, src, models_dir):
    tcfg = config["thermal"]
    reports, weather, meta = load_lake(src, tcfg["exclude_sources"])
    rooms, skipped = build_rooms(reports, weather, meta, tcfg, load_events(src))
    if not rooms:
        raise SystemExit(f"no usable rooms: {skipped}")
    out, bundle, _ = build_bundle(config, rooms, load_ambient(src), Path(models_dir) / "thermal", src=src)
    return out, bundle
