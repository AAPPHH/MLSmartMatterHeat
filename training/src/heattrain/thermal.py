import json
import math
from dataclasses import dataclass, field
from datetime import datetime
from pathlib import Path
from zoneinfo import ZoneInfo
import duckdb
import numpy as np
import pyarrow as pa
import pyarrow.ipc as ipc
from scipy.optimize import least_squares

TEMP, DEMAND, SETPOINT, MODE = 0, 8, 18, 28
HEATING_CLUSTER = 513


def read_arrows(directory, table):
    batches = []
    root = Path(directory)
    files = sorted(root.glob(f"{table}__*.arrows")) + sorted((root / table).glob("*.arrows")) + sorted((root / "raw" / table).glob("*.arrows"))
    for f in files:
        try:
            with open(f, "rb") as fh:
                reader = ipc.open_stream(fh)
                while True:
                    try:
                        batches.append(reader.read_next_batch())
                    except StopIteration:
                        break
        except (pa.ArrowInvalid, OSError):
            continue
    return pa.Table.from_batches(batches) if batches else None


def epoch(values):
    return [np.nan if v is None else v.timestamp() for v in values]


def load_raw(directory, exclude):
    t = read_arrows(directory, "attribute_reports")
    reports = []
    if t is not None:
        c = {n: t.column(n).to_pylist() for n in ("node", "cluster", "attribute", "v_f64", "v_i64", "source")}
        ts = epoch(t.column("ts").to_pylist())
        for i in range(t.num_rows):
            v = c["v_f64"][i] if c["v_f64"][i] is not None else c["v_i64"][i]
            if c["cluster"][i] == HEATING_CLUSTER and v is not None and c["source"][i] not in exclude:
                reports.append((ts[i], c["node"][i], c["attribute"][i], float(v)))
    w = read_arrows(directory, "weather")
    weather = []
    if w is not None:
        cols = {n: w.column(n).to_pylist() for n in ("kind", "temp_c", "wind_ms", "radiation_wm2")}
        ts, iss = epoch(w.column("ts").to_pylist()), epoch(w.column("issued_ts").to_pylist())
        weather = [(ts[i], cols["kind"][i], iss[i], cols["temp_c"][i], cols["wind_ms"][i], cols["radiation_wm2"][i]) for i in range(w.num_rows)]
    m = read_arrows(directory, "meta")
    meta = []
    if m is not None:
        meta = list(zip(m.column("key").to_pylist(), m.column("value").to_pylist(), epoch(m.column("valid_from").to_pylist())))
    return reports, weather, meta


def load_parquet(root, exclude):
    base = Path(root) / "parquet"
    con = duckdb.connect()

    def q(table, sql):
        if not any((base / table).rglob("part.parquet")):
            return []
        return con.execute(sql.format(src=f"read_parquet('{(base / table).as_posix()}/**/part.parquet', hive_partitioning=false, union_by_name=true)")).fetchall()

    skip = ",".join(f"'{x}'" for x in exclude) or "''"
    reports = q("attribute_reports", f"SELECT epoch(ts), node, attribute, coalesce(v_f64, v_i64)::DOUBLE FROM {{src}} WHERE cluster={HEATING_CLUSTER} AND attribute IN ({TEMP},{DEMAND},{SETPOINT},{MODE}) AND coalesce(v_f64, v_i64) IS NOT NULL AND source::VARCHAR NOT IN ({skip})")
    weather = q("weather", "SELECT epoch(ts), kind::VARCHAR, epoch(issued_ts), temp_c, wind_ms, radiation_wm2 FROM {src}")
    meta = q("meta", "SELECT key::VARCHAR, value, epoch(valid_from) FROM {src}")
    return [(float(a), int(b), int(c), float(d)) for a, b, c, d in reports], weather, meta


def merge_rows(*parts):
    seen, out = set(), []
    for rows in parts:
        for r in rows:
            key = tuple(round(x * 1e6) if isinstance(x, float) and i == 0 else x for i, x in enumerate(r))
            if key not in seen:
                seen.add(key)
                out.append(r)
    return out


def raw_batches(directory, table):
    root = Path(directory)
    for f in sorted((root / "raw" / table).glob("*.arrows")) + sorted((root / table).glob("*.arrows")):
        try:
            with open(f, "rb") as fh:
                reader = ipc.open_stream(fh)
                while True:
                    try:
                        yield reader.read_next_batch()
                    except StopIteration:
                        break
        except (pa.ArrowInvalid, OSError):
            continue


def load_events(path, kinds=("startup", "shutdown")):
    p = Path(path)
    out = []
    for b in raw_batches(p, "events"):
        out += [(ts, k) for ts, k in zip(epoch(b.column("ts").to_pylist()), b.column("kind").to_pylist()) if k in kinds]
    base = p / "parquet" / "events"
    if base.is_dir() and any(base.rglob("part.parquet")):
        sql = f"SELECT epoch(ts), kind::VARCHAR FROM read_parquet('{base.as_posix()}/**/part.parquet', hive_partitioning=false, union_by_name=true) WHERE kind::VARCHAR IN ({','.join(repr(k) for k in kinds)})"
        out += [(float(a), b) for a, b in duckdb.connect().execute(sql).fetchall()]
    return sorted(set((round(a, 3), b) for a, b in out))


def load_lake(path, exclude=("legacy",)):
    p = Path(path)
    parts = []
    if (p / "parquet").is_dir():
        parts.append(load_parquet(p, exclude))
    parts.append(load_raw(p, exclude))
    return tuple(merge_rows(*(x[i] for x in parts)) for i in range(3))


def latest_meta(meta):
    best = {}
    for k, v, vf in meta:
        if k not in best or (vf or 0) >= best[k][0]:
            best[k] = (vf or 0, v)
    out = {}
    for k, (_, v) in best.items():
        try:
            out[k] = json.loads(v)
        except (TypeError, ValueError):
            pass
    return out


def interp(t, v, grid):
    ok = np.isfinite(v)
    return np.interp(grid, t[ok], v[ok]) if ok.any() else np.full(len(grid), np.nan)


def ffill_at(t, v, grid):
    idx = np.searchsorted(t, grid, side="right") - 1
    return v[np.clip(idx, 0, None)]


def weather_grid(weather, grid):
    best = {}
    for ts, kind, iss, *vals in weather:
        key = (kind == "obs", 0.0 if iss is None or iss != iss else iss)
        if ts not in best or key > best[ts][0]:
            best[ts] = (key, [np.nan if x is None else x for x in vals])
    if not best:
        raise ValueError("no weather data")
    t = np.array(sorted(best))
    cols = np.array([best[x][1] for x in t], dtype=float)
    return [interp(t, cols[:, i], grid) for i in range(3)]


@dataclass
class RoomData:
    room: str
    node: int
    t0: float
    dt: float
    y: np.ndarray
    yi: np.ndarray
    u: np.ndarray
    sp: np.ndarray
    tout: np.ndarray
    wind: np.ndarray
    rad: np.ndarray
    night: np.ndarray
    nb: dict = field(default_factory=dict)
    info: dict = field(default_factory=dict)
    bad: np.ndarray = None
    yv: np.ndarray = None

    @property
    def n(self):
        return len(self.y)

    @property
    def h(self):
        return self.dt / 3600.0

    def slice(self, a, b):
        s = lambda x: x[a:b]
        return RoomData(self.room, self.node, self.t0 + a * self.dt, self.dt, s(self.y), s(self.yi), s(self.u), s(self.sp), s(self.tout), s(self.wind), s(self.rad), s(self.night),
                        {k: (s(n), s(m)) for k, (n, m) in self.nb.items()}, self.info,
                        None if self.bad is None else s(self.bad), None if self.yv is None else s(self.yv))


def night_mask(grid, cfg):
    tz = ZoneInfo(cfg["timezone"])
    keys = (grid // 900).astype(np.int64)
    hours = {k: datetime.fromtimestamp(int(k) * 900, tz).hour for k in np.unique(keys)}
    h = np.array([hours[k] for k in keys])
    a, b = cfg["night_hours"]
    return (h >= a) | (h < b)


def outage_intervals(reports, events, cfg):
    oc = cfg["outage"]
    t = np.unique(np.array([r[0] for r in reports], dtype=float))
    out = [(float(t[i]), float(t[i + 1])) for i in np.nonzero(np.diff(t) > oc["silence_min"] * 60)[0]] if len(t) else []
    down = None
    pad = oc["restart_pad_min"] * 60
    for ts, kind in sorted(events):
        if kind == "shutdown":
            down = ts
        elif kind == "startup":
            out.append((ts if down is None else down, ts + pad))
            down = None
    if down is not None:
        out.append((down, float(t[-1]) if len(t) else down))
    return sorted(out)


def interval_mask(grid, intervals):
    bad = np.zeros(len(grid), bool)
    for a, b in intervals:
        bad[np.searchsorted(grid, a, side="left"):np.searchsorted(grid, b, side="right")] = True
    return bad


def stale_mask(grid, times, stale_s):
    if len(times) == 0:
        return np.ones(len(grid), bool)
    idx = np.searchsorted(times, grid, side="right") - 1
    age = np.where(idx >= 0, grid - times[np.clip(idx, 0, None)], np.inf)
    return age > stale_s


def build_rooms(reports, weather, meta, cfg, events=()):
    series = {}
    for ts, node, attr, v in reports:
        series.setdefault((node, attr), []).append((ts, v))
    series = {k: tuple(np.array(x) for x in zip(*sorted(v))) for k, v in series.items()}
    m = latest_meta(meta)
    node_room = {}
    room_nodes = {}
    for r in m.get("rooms", []):
        room_nodes[r["name"]] = list(r.get("nodes", []))
        for n in r.get("nodes", []):
            node_room[n] = r["name"]
    links = m.get("links") or [{"a": a, "b": b, "kind": "wall"} for a, b in m.get("adjacency", [])]
    nodes = sorted({n for n, a in series if a == TEMP})
    dt = cfg["dt_s"]
    out, skipped = [], {}
    outages = outage_intervals(reports, events, cfg)
    for node in nodes:
        t, v = series[(node, TEMP)]
        v = v / 100.0
        ok = (v > cfg["temp_valid"][0]) & (v < cfg["temp_valid"][1])
        t, v = t[ok], v[ok]
        room = node_room.get(node, f"node{node}")
        if len(room_nodes.get(room, [])) > 1:
            room = f"{room}_{node}"
        if len(t) < 3 or (t[-1] - t[0]) / 3600 < cfg["min_hours"]:
            skipped[room] = "too little temperature data"
            continue
        grid = np.arange(t[0], t[-1] + 1e-6, dt)
        dem = series.get((node, DEMAND))
        if dem is None:
            skipped[room] = "no valve demand data"
            continue
        good = (dem[1] >= 0) & (dem[1] <= 100)
        n_invalid = int((~good).sum())
        if not good.any():
            skipped[room] = "no valid valve demand"
            continue
        u = np.clip(ffill_at(dem[0][good], dem[1][good], grid) / 100.0, 0, 1)
        if (node, MODE) in series:
            mt, mv = series[(node, MODE)]
            u = np.where((mt[0] <= grid) & (ffill_at(mt, mv, grid) == 0), 0.0, u)
        sp = ffill_at(*series[(node, SETPOINT)], grid) / 100.0 if (node, SETPOINT) in series else np.full(len(grid), np.nan)
        y = np.full(len(grid), np.nan)
        idx = np.clip(np.rint((t - grid[0]) / dt).astype(int), 0, len(grid) - 1)
        y[idx] = v
        yv = np.full(len(grid), np.nan)
        yv[np.clip(np.rint((dem[0][good] - grid[0]) / dt).astype(int), 0, len(grid) - 1)] = dem[1][good] / 100.0
        bad = interval_mask(grid, outages) | stale_mask(grid, t, cfg["outage"]["stale_s"]) | stale_mask(grid, dem[0][good], cfg["outage"]["stale_s"])
        y[bad] = np.nan
        yv[bad] = np.nan
        tout, wind, rad = weather_grid(weather, grid)
        nb = {}
        nbn = {}
        for l in links:
            for here, there in ((l["a"], l["b"]), (l["b"], l["a"])):
                if here != node_room.get(node):
                    continue
                for nn in room_nodes.get(there, []):
                    if (nn, TEMP) not in series or nn == node:
                        continue
                    nt, nv = series[(nn, TEMP)]
                    ni = interp(nt, nv / 100.0, grid)
                    nbn.setdefault(l.get("kind", "wall"), []).append(nn)
                    cnt, tot = nb.setdefault(l.get("kind", "wall"), (np.zeros(len(grid)), np.zeros(len(grid))))
                    cnt += 1
                    tot += ni
        d = RoomData(room, node, float(grid[0]), float(dt), y, interp(t, v, grid), u, sp, tout, wind, rad, night_mask(grid, cfg), nb,
                     {"n_invalid_demand": n_invalid, "n_temp_reports": int(len(t)), "n_demand_reports": int(good.sum()), "start": float(grid[0]), "end": float(grid[-1]), "n_masked": int(bad.sum()), "neighbour_nodes": nbn}, bad, yv)
        out.append(d)
    return out, skipped


def step_terms(p, struct, u, tout, wind, rad, night, nb, h):
    au = p["a1"] * u
    ts = np.where(night, p["ts_night"], p["ts"])
    cn = sum((p[f"c_{k}"] * n for k, (n, s) in nb.items()), 0.0)
    cs = sum((p[f"c_{k}"] * s for k, (n, s) in nb.items()), 0.0)
    amb = p["b2"] + p["b3"] * wind
    ga = amb * tout + p["b4"] * rad / 100.0 + cs
    da = amb + cn
    z = 0.0 * au
    if struct == "rc2":
        f11, f12, f21, f22, g1, g2 = -(au + p["a2"]), p["a2"] + z, p["b1"] + z, -(p["b1"] + da), au * ts, ga
    else:
        f11, f12, f21, f22, g1, g2 = -1.0 + z, z, z, -(au + da), z, ga + au * ts
    return 1 + h * f11, h * f12, h * f21, 1 + h * f22, h * g1, h * g2


def kalman(p, d, struct, cfg, collect=False):
    A11, A12, A21, A22, c1, c2 = [x.tolist() if hasattr(x, "tolist") else [x] * d.n for x in step_terms(p, struct, d.u, d.tout, d.wind, d.rad, d.night, d.nb, d.h)]
    w = p["w"] if struct == "rc2" else 0.0
    q1, q2, r = cfg["q_r"] * d.h, cfg["q_a"] * d.h, cfg["noise_r"] ** 2
    y = d.y.tolist()
    first = next((v for v in y if v == v), 20.0)
    x1 = x2 = first
    p11, p12, p22 = cfg["p0"], 0.0, cfg["p0"]
    ks, vs, ss, xs, ps = [], [], [], [], []
    for k in range(d.n):
        yk = y[k]
        if yk == yk:
            s = w * w * p11 + 2 * w * (1 - w) * p12 + (1 - w) ** 2 * p22 + r
            v = yk - (w * x1 + (1 - w) * x2)
            k1 = (w * p11 + (1 - w) * p12) / s
            k2 = (w * p12 + (1 - w) * p22) / s
            x1 += k1 * v
            x2 += k2 * v
            n11 = p11 - k1 * (w * p11 + (1 - w) * p12)
            n12 = p12 - k1 * (w * p12 + (1 - w) * p22)
            n22 = p22 - k2 * (w * p12 + (1 - w) * p22)
            p11, p12, p22 = n11, n12, n22
            ks.append(k)
            vs.append(v)
            ss.append(s)
        if collect:
            xs.append((x1, x2))
            ps.append((p11, p12, p22) if collect == "full" else p22)
        a11, a12, a21, a22 = A11[k], A12[k], A21[k], A22[k]
        t11, t12 = a11 * p11 + a12 * p12, a11 * p12 + a12 * p22
        t21, t22 = a21 * p11 + a22 * p12, a21 * p12 + a22 * p22
        p11, p12, p22 = t11 * a11 + t12 * a12 + q1, t11 * a21 + t12 * a22, t21 * a21 + t22 * a22 + q2
        x1, x2 = a11 * x1 + a12 * x2 + c1[k], a21 * x1 + a22 * x2 + c2[k]
    return np.array(ks), np.array(vs), np.array(ss), np.array(xs), np.array(ps)


def active_names(d, struct, cfg):
    names = ["a1", "b2", "b3", "b4", "ts"] if struct == "rc1" else ["a1", "a2", "b1", "b2", "b3", "b4", "w", "ts"]
    night = d.night
    if night.sum() >= cfg["min_night_steps"] and (~night).sum() >= cfg["min_night_steps"]:
        names.append("ts_night")
    return names + [f"c_{k}" for k in d.nb]


def spec(cfg, name):
    return cfg["params"].get(name, cfg["params"]["c"])


def unpack(theta, names, cfg, kinds):
    p = {n: spec(cfg, n)[0] for n in list(cfg["params"]) if n != "c"}
    for k in kinds:
        p[f"c_{k}"] = spec(cfg, f"c_{k}")[0]
    p.update(zip(names, np.exp(theta)))
    if "ts_night" not in names:
        p["ts_night"] = p["ts"]
    return p


@dataclass
class Fit:
    p: dict
    names: list
    struct: str
    rel_std: dict
    at_bound: list
    cost: float
    nres: int
    cond: float
    cov: np.ndarray


def fit(d, struct, cfg, theta0=None):
    names = active_names(d, struct, cfg)
    kinds = list(d.nb)
    lo = np.log([spec(cfg, n)[1] for n in names])
    hi = np.log([spec(cfg, n)[2] for n in names])
    init = np.log([spec(cfg, n)[0] for n in names])
    x0 = np.clip(init if theta0 is None else theta0, lo + 1e-6, hi - 1e-6)
    sig = cfg["prior_sigma"]

    def resid(theta):
        ks, v, s, _, _ = kalman(unpack(theta, names, cfg, kinds), d, struct, cfg)
        return np.concatenate([v / np.sqrt(s), (theta - init) / sig])

    sol = least_squares(resid, x0, bounds=(lo, hi), x_scale="jac", diff_step=1e-3, max_nfev=cfg["max_nfev"])
    m = len(sol.fun) - len(names)
    J = sol.jac[:m]
    jtj = J.T @ J
    ev = np.linalg.eigvalsh(jtj)
    cond = float(ev[-1] / max(ev[0], 1e-300))
    s2 = max(1.0, float(np.sum(sol.fun[:m] ** 2)) / max(m - len(names), 1))
    cov = np.linalg.pinv(jtj + 1e-12 * np.trace(jtj) * np.eye(len(names))) * s2
    std = np.sqrt(np.clip(np.diag(cov), 0, None))
    at_bound = [n for n, x, a, b in zip(names, sol.x, lo, hi) if min(x - a, b - x) < 1e-3]
    p = unpack(sol.x, names, cfg, kinds)
    return Fit(p, names, struct, {n: float(s) for n, s in zip(names, std)}, at_bound, float(np.sqrt(np.mean(sol.fun[:m] ** 2))), m, cond, cov), sol.x


def horizon_error(p, d, struct, cfg, start):
    ks, v, s, xs, ps = kalman(p, d, struct, cfg, collect=True)
    A = [np.asarray(x) for x in step_terms(p, struct, d.u, d.tout, d.wind, d.rad, d.night, d.nb, d.h)]
    H = int(cfg["horizon_min"] * 60 / d.dt)
    w = p["w"] if struct == "rc2" else 0.0
    err, base = [], []
    for j in range(start, d.n - H, cfg["origin_stride"]):
        if d.bad is not None and d.bad[j:j + H + 1].any():
            continue
        x1, x2 = xs[j]
        for k in range(j, j + H):
            x1, x2 = A[0][k] * x1 + A[1][k] * x2 + A[4][k], A[2][k] * x1 + A[3][k] * x2 + A[5][k]
        err.append(d.yi[j + H] - (w * x1 + (1 - w) * x2))
        base.append(d.yi[j + H] - d.yi[j])
    inn = v[ks >= start]
    rm = lambda a: float(np.sqrt(np.mean(np.square(a)))) if len(a) else None
    return {"rmse_horizon": rm(err), "rmse_persistence": rm(base), "rmse_innovation": rm(inn), "n_origins": len(err), "horizon_min": cfg["horizon_min"]}


def tau_minutes(p, struct, wind):
    amb = p["b2"] + p["b3"] * wind
    F = np.array([[-p["a2"], p["a2"]], [p["b1"], -(p["b1"] + amb)]]) if struct == "rc2" else np.array([[-amb]])
    return sorted(float(60.0 / -e) for e in np.linalg.eigvals(F).real)


def valve_gain(d, cfg):
    e = np.clip(d.sp - d.yi, 0, cfg["gain_err_max"])
    ok = np.isfinite(e) & (e > 0)
    if ok.sum() < 30:
        return cfg["valve_gain_default"]
    return float(np.clip(np.sum(d.u[ok] * e[ok]) / np.sum(e[ok] ** 2), *cfg["valve_gain_bounds"]))


def span_info(d):
    f = lambda t: datetime.fromtimestamp(t, ZoneInfo("UTC")).isoformat()
    return {"start": f(d.info["start"]), "end": f(d.info["end"]), "hours": round(d.n * d.dt / 3600, 2)}


def fit_room(d, cfg):
    warnings = []
    if d.u.std() < cfg["min_valve_std"]:
        warnings.append("valve demand nearly constant: heating parameters weakly identifiable")
    if d.info["n_invalid_demand"]:
        warnings.append(f"{d.info['n_invalid_demand']} invalid demand values (>100) treated as missing")
    if not d.nb:
        warnings.append("no neighbour coupling (no links or no neighbour data)")
    if d.tout.std() < 0.3:
        warnings.append("outdoor temperature almost constant: b2 weakly identifiable")
    ns = int(d.n * (1 - cfg["holdout_frac"]))
    can_hold = (d.n - ns) * d.dt / 3600 >= cfg["min_holdout_hours"]
    if not can_hold:
        warnings.append("span too short for held-out block; metrics are in-sample")
        ns = 0
    res = {}
    for struct in ("rc2", "rc1"):
        tr, x = fit(d.slice(0, ns) if can_hold else d, struct, cfg)
        full, x_full = fit(d, struct, cfg, x)
        res[struct] = (full, horizon_error(tr.p, d, struct, cfg, ns))
    f2, m2 = res["rc2"]
    f1, m1 = res["rc1"]
    key = [n for n in ("a2", "b1", "w") if n in f2.rel_std]
    ident = all(f2.rel_std[n] < cfg["std_max"] for n in key) and not (set(key) & set(f2.at_bound))
    better = m2["rmse_horizon"] is not None and m1["rmse_horizon"] is not None and m2["rmse_horizon"] <= m1["rmse_horizon"] * cfg["rc2_margin"]
    chosen = "rc2" if ident and better else "rc1"
    if chosen == "rc1":
        warnings.append("2-state model not identifiable or not better on held-out block: 1-state fallback" if not ident else "2-state not better than 1-state on held-out block: 1-state used")
    f, m = res[chosen]
    for n, s in f.rel_std.items():
        if s > cfg["std_max"]:
            warnings.append(f"{n} poorly identified (relative std {s:.2f})")
    if f.at_bound:
        warnings.append("at bound: " + ",".join(f.at_bound))
    wind = float(np.mean(d.wind))
    return {
        "room": d.room, "node": d.node, "structure": chosen, "params": f.p, "rel_std": f.rel_std,
        "bounds": {n: spec(cfg, n)[1:] for n in f.names}, "at_bound": f.at_bound, "cond": f.cond, "names": f.names, "cov_log": f.cov.tolist(),
        "tau_min": tau_minutes(f.p, chosen, wind), "valve_gain": valve_gain(d, cfg), "night_hours": cfg["night_hours"],
        "metrics": {chosen: m, "alternative": {"structure": "rc1" if chosen == "rc2" else "rc2", **res["rc1" if chosen == "rc2" else "rc2"][1]}, "holdout": can_hold, "fit_rmse_normalised": f.cost},
        "span": span_info(d), "neighbours": {k: int(v[0].max()) for k, v in d.nb.items()}, "dt_s": d.dt, "warnings": warnings,
    }


class ThermalModel:
    def __init__(self, doc, cfg):
        self.doc, self.cfg = doc, cfg
        self.p, self.struct = doc["params"], doc["structure"]
        self.w = self.p["w"] if self.struct == "rc2" else 0.0

    @classmethod
    def load(cls, path, cfg):
        return cls(json.loads(Path(path).read_text()), cfg)

    def param_draws(self, n):
        names = self.doc["names"]
        rng = np.random.default_rng(self.cfg["mc_seed"])
        lo, hi = (np.log([self.doc["bounds"][k][i] for k in names]) for i in (0, 1))
        draws = rng.multivariate_normal(np.log([self.p[k] for k in names]), np.array(self.doc["cov_log"]), n, method="eigh")
        out = []
        for x in np.clip(draws, lo, hi):
            q = {**self.p, **dict(zip(names, np.exp(x)))}
            q["ts_night"] = q["ts_night"] if "ts_night" in names else q["ts"]
            out.append(q)
        return out

    def estimate(self, d):
        ks, v, s, xs, ps = kalman(self.p, d, self.struct, self.cfg, collect=True)
        air_std = np.sqrt(np.clip(ps, 0, None))
        param_std = np.zeros(d.n)
        if self.cfg["mc_samples"] and "cov_log" in self.doc:
            param_std = np.std([kalman(q, d, self.struct, self.cfg, collect=True)[3][:, 1] for q in self.param_draws(self.cfg["mc_samples"])], axis=0)
        heat = self.p["a1"] * d.u * (np.where(d.night, self.p["ts_night"], self.p["ts"]) - xs[:, 0]) if self.struct == "rc2" else None
        return {"t": d.t0 + np.arange(d.n) * d.dt, "air": xs[:, 1], "radiator": xs[:, 0], "air_std": air_std, "air_param_std": param_std, "air_total_std": np.sqrt(air_std ** 2 + param_std ** 2), "sensor": d.yi, "heat_k_per_h": heat}

    def predict(self, x0, steps, tout, wind=0.0, rad=0.0, u=None, setpoint=None, night=False, nb=None, dt=60.0):
        h = dt / 3600.0
        x1, x2 = x0
        gain = self.doc["valve_gain"]
        out = []
        at = lambda a, k: float(np.broadcast_to(a, (steps,))[k])
        for k in range(steps):
            sensor = self.w * x1 + (1 - self.w) * x2
            uk = at(u, k) if u is not None else float(np.clip(gain * (at(setpoint, k) - sensor), 0, 1))
            a = [float(x) for x in step_terms(self.p, self.struct, uk, at(tout, k), at(wind, k), at(rad, k), night, {kind: (np.float64(n), np.float64(s)) for kind, (n, s) in (nb or {}).items()}, h)]
            x1, x2 = a[0] * x1 + a[1] * x2 + a[4], a[2] * x1 + a[3] * x2 + a[5]
            out.append((x1, x2, self.w * x1 + (1 - self.w) * x2, uk))
        return np.array(out)

    def lead_time(self, x0, target, tout, max_min=360, tol=0.2, **kw):
        tr = self.predict(x0, max_min, tout, **({} if "setpoint" in kw else {"u": 1.0}), **kw)
        hit = np.nonzero(tr[:, 1] >= target - tol)[0]
        return int(hit[0]) + 1 if len(hit) else None


def estimate_room_air(doc, d, cfg):
    return ThermalModel(doc, cfg).estimate(d)


def predict(doc, x0, steps, tout, cfg, **kw):
    return ThermalModel(doc, cfg).predict(x0, steps, tout, **kw)


def default_date(rooms):
    end = max(d.info["end"] for d in rooms)
    return datetime.fromtimestamp(end, ZoneInfo("Europe/Berlin")).date().isoformat()


def thermal(config, src, models_dir):
    cfg = config["thermal"] if "thermal" in config else config
    reports, weather, meta = load_lake(src, cfg["exclude_sources"])
    rooms, skipped = build_rooms(reports, weather, meta, cfg, load_events(src))
    if not rooms:
        raise SystemExit(f"no usable rooms: {skipped}")
    out = Path(models_dir) / "thermal" / default_date(rooms)
    out.mkdir(parents=True, exist_ok=True)
    docs = []
    for d in rooms:
        doc = fit_room(d, cfg)
        doc["generated_from"] = str(src)
        (out / f"{d.room}.json").write_text(json.dumps(doc, indent=1, default=float))
        docs.append(doc)
    if skipped:
        (out / "skipped.json").write_text(json.dumps(skipped, indent=1))
    return docs, out


SQRT2 = math.sqrt(2.0)
HORIZON = 60
PROBES = (5, 10, 15)
BOOSTS = (5, 10, 15, 20)
FORECAST_MIN = 20
EPS = 1e-9


def erf_as(x):
    t = 1.0 / (1.0 + 0.3275911 * abs(x))
    y = 1.0 - (((((1.061405429 * t - 1.453152027) * t) + 1.421413741) * t - 0.284496736) * t + 0.254829592) * t * math.exp(-x * x)
    return y if x >= 0 else -y


def norm_cdf(z):
    return 0.5 * (1.0 + erf_as(z / SQRT2))


def lg_pdf(v, s):
    return -0.5 * (v * v / s + math.log(2.0 * math.pi * s)) if s > 0 else math.nan


def finite(*xs):
    return all(math.isfinite(x) for x in xs)


def mat_mul(a, b):
    return [(a[3 * i] * b[j] + a[3 * i + 1] * b[3 + j]) + a[3 * i + 2] * b[6 + j] for i in range(3) for j in range(3)]


def mat_t(a):
    return [a[3 * j + i] for i in range(3) for j in range(3)]


def psd_ok(p):
    if any(p[4 * i] < -1e-12 for i in range(3)):
        return False
    return all(p[3 * i + j] ** 2 <= p[4 * i] * p[4 * j] * (1.0 + 1e-6) + 1e-12 for i in range(3) for j in range(i + 1, 3))


class RoomPar:
    def __init__(self, doc):
        p = doc["params"]
        self.a1, self.a2, self.b1, self.b2, self.b3, self.b4, self.w, self.ts, self.ts_night = (p[k] for k in ("a1", "a2", "b1", "b2", "b3", "b4", "w", "ts", "ts_night"))
        self.kinds = sorted(doc["c"])
        self.ck = [doc["c"][k] for k in self.kinds]
        self.g, self.sigma_u, self.noise_r, self.p0, self.q_r, self.q_a = (doc[k] for k in ("valve_gain", "sigma_u", "noise_r", "p0", "q_r", "q_a"))

    def coupling(self, ncnt, nsum):
        cn = cs = 0.0
        for c, n, s in zip(self.ck, ncnt, nsum):
            cn += c * n
            cs += c * s
        return cn, cs

    def terms(self, lam, ueff, tout, wind, rad, night, cn, cs, h):
        au = self.a1 * ueff
        ts = self.ts_night if night else self.ts
        amb = self.b2 + self.b3 * wind
        return 1 + h * (-(au + self.a2)), h * self.a2, h * self.b1, 1 + h * (-(self.b1 + (lam * amb + cn))), h * (au * ts), h * (lam * amb * tout + self.b4 * rad / 100.0 + cs), amb

    def sensor(self, m, a):
        return self.w * m + (1 - self.w) * a


def obs_update(x, p, hrow, v, r):
    ph = [p[3 * i] * hrow[0] + p[3 * i + 1] * hrow[1] + p[3 * i + 2] * hrow[2] for i in range(3)]
    s = hrow[0] * ph[0] + hrow[1] * ph[1] + hrow[2] * ph[2] + r
    if not s > 0:
        return s
    k = [q / s for q in ph]
    for i in range(3):
        x[i] += k[i] * v
    q = [p[3 * i + j] - k[i] * ph[j] for i in range(3) for j in range(3)]
    p[:] = [0.5 * (q[3 * i + j] + q[3 * j + i]) for i in range(3) for j in range(3)]
    return s


@dataclass
class RoomIn:
    y: float = math.nan
    yv: float = math.nan
    u: float = 0.0
    sp: float = math.nan
    tout: float = 0.0
    wind: float = 0.0
    rad: float = 0.0
    night: bool = False
    ncnt: tuple = ()
    nsum: tuple = ()


class RoomFilter:
    def __init__(self, name, par, shock, house):
        self.name, self.par, self.shock, self.house = name, par, shock, house
        self.h = 1.0 / 60.0
        self.phi = math.exp(-1.0 / shock["tau_l"])
        self.tm = [[1.0 - shock["p_jump"], shock["p_jump"]], [1.0 / shock["hold_min"], 1.0 - 1.0 / shock["hold_min"]]]
        self.q_l = (shock["q_calm"] ** 2, shock["q_jump"] ** 2)
        self.x = [[0.0, 0.0, 1.0] for _ in range(4)]
        self.p = [[0.0] * 9 for _ in range(4)]
        self.nu = [[1.0, 0.0], [1.0, 0.0]]
        self.init = False
        self.since_obs = 0
        self.hot = 0
        self.flag = None
        self.skipped = True
        self.pending = None
        self.events = []
        self.comps = []

    def weights(self, w):
        return [w[j // 2] * self.nu[j // 2][j % 2] for j in range(4)]

    def seed(self, y, inp):
        m = a = y
        if self.house["balance_seed"] and finite(inp.u, inp.tout, inp.wind, inp.rad, *inp.nsum):
            par = self.par
            cn, cs = par.coupling(inp.ncnt, inp.nsum)
            amb = par.b2 + par.b3 * inp.wind
            d = ((amb + cn) * y - amb * inp.tout - cs - par.b4 * inp.rad / 100.0) / (par.b1 + (amb + cn) * par.w)
            a = y - par.w * d
            m = a + d
        for j in range(4):
            self.x[j] = [m, a, 1.0]
            self.p[j] = [self.par.p0, 0.0, 0.0, 0.0, self.par.p0, 0.0, 0.0, 0.0, self.shock["p_l0"]]
        self.nu = [[1.0, 0.0], [1.0, 0.0]]
        self.init, self.since_obs, self.hot = True, 0, 0

    def reset(self, cause, inp, w, step):
        mu = self.weights(w)
        prior = [sum(mu[j] * self.x[j][i] for j in range(4)) for i in range(3)] if self.init else [math.nan] * 3
        self.events.append({"room": self.name, "cause": cause, "step": step, "prior": prior})
        if math.isfinite(inp.y):
            self.seed(inp.y, inp)
        else:
            self.init = False

    def update(self, inp, w, step):
        self.pending, self.flag, self.skipped = None, None, True
        par = self.par
        if not finite(inp.u, inp.tout, inp.wind, inp.rad, *inp.nsum):
            self.since_obs += 1
            return None
        y_ok = math.isfinite(inp.y)
        if not self.init:
            if not y_ok:
                self.since_obs += 1
                return None
            self.seed(inp.y, inp)
        elif y_ok and self.since_obs > self.house["reset_gap_min"]:
            self.reset("gap", inp, w, step)
        self.skipped = False
        v_ok = math.isfinite(inp.yv) and 0.0 < inp.yv < 1.0 and math.isfinite(inp.sp)
        r1 = self.house["r_grid"] + par.noise_r ** 2
        ll = [0.0] * 4
        yhat = [0.0] * 4
        s1 = [0.0] * 4
        for j in range(4):
            x, p = self.x[j], self.p[j]
            if y_ok:
                yhat[j] = par.sensor(x[0], x[1])
                s1[j] = obs_update(x, p, (par.w, 1 - par.w, 0.0), inp.y - yhat[j], r1)
                ll[j] += lg_pdf(inp.y - yhat[j], s1[j])
            if v_ok:
                v = inp.yv - par.g * (inp.sp - par.sensor(x[0], x[1]))
                ll[j] += lg_pdf(v, obs_update(x, p, (-par.g * par.w, -par.g * (1 - par.w), 0.0), v, par.sigma_u ** 2))
        if y_ok:
            mu = self.weights(w)
            mean = sum(mu[j] * yhat[j] for j in range(4))
            var = sum(mu[j] * (s1[j] + (yhat[j] - mean) ** 2) for j in range(4))
            z = abs(inp.y - mean) / math.sqrt(var) if math.isfinite(var) and var > 0 else math.inf
            self.hot = self.hot + 1 if z > self.house["reset_sigma"] else 0
            self.since_obs = 0
        else:
            self.since_obs += 1
        if not all(math.isfinite(v) for v in ll):
            self.flag = "nan"
            return None
        top = max(ll)
        e = [math.exp(v - top) for v in ll]
        self.pending = e
        return [sum(self.nu[s][m] * e[2 * s + m] for m in range(2)) for s in range(2)], top

    def finish(self, inp, w, step):
        if self.skipped:
            return {"ok": False}
        if self.pending is not None:
            for s in range(2):
                t = [self.nu[s][m] * self.pending[2 * s + m] for m in range(2)]
                z = t[0] + t[1]
                self.nu[s] = [t[0] / z, t[1] / z] if z > 0 else [1.0, 0.0]
        bad = self.flag
        if bad is None:
            allx = [v for xs in self.x for v in xs] + [v for ps in self.p for v in ps] + [v for n in self.nu for v in n] + list(w)
            if not all(math.isfinite(v) for v in allx):
                bad = "nan"
            elif not all(psd_ok(p) for p in self.p):
                bad = "psd"
            else:
                mu = self.weights(w)
                lam = sum(mu[j] * self.x[j][2] for j in range(4))
                bad = "lambda" if lam < 0.5 or lam > 8.0 else "innovation" if self.hot >= self.house["reset_n"] else None
        if bad is not None:
            self.reset(bad, inp, w, step)
            if not self.init:
                return {"ok": False}
        out = self.report(w)
        self.advance(inp, w)
        return out

    def report(self, w):
        par, lam_w = self.par, self.shock["lambda_w"]
        mu = self.weights(w)
        m, a, lam = (sum(mu[j] * self.x[j][i] for j in range(4)) for i in range(3))
        var = sum(mu[j] * (self.p[j][8] + (self.x[j][2] - lam) ** 2) for j in range(4))
        pw = sum(mu[j] * (1.0 - norm_cdf((lam_w - self.x[j][2]) / math.sqrt(max(self.p[j][8], 1e-18)))) for j in range(4))
        self.comps = [(mu[j], list(self.x[j]), list(self.p[j])) for j in range(4)]
        return {"ok": True, "m": m, "a": a, "sensor": par.sensor(m, a), "lam": lam, "lam_std": math.sqrt(var), "p_window": pw, "p_supply": w[1]}

    def advance(self, inp, w):
        par, h = self.par, self.h
        mu = self.weights(w)
        cn, cs = par.coupling(inp.ncnt, inp.nsum)
        new_x, new_p, new_nu = [], [], [[0.0, 0.0], [0.0, 0.0]]
        tr = self.house["ts_tr"]
        for jt in range(4):
            st, mt = divmod(jt, 2)
            om = [tr[j // 2][st] * self.tm[j % 2][mt] * mu[j] for j in range(4)]
            c = sum(om)
            if c > 0:
                xb = [sum(om[j] * self.x[j][i] for j in range(4)) / c for i in range(3)]
                pb = [sum(om[j] * (self.p[j][3 * i + k] + (self.x[j][i] - xb[i]) * (self.x[j][k] - xb[k])) for j in range(4)) / c for i in range(3) for k in range(3)]
            else:
                xb, pb = list(self.x[jt]), list(self.p[jt])
            new_nu[st][mt] = c
            f11, f12, f21, f22, g1, g2, amb = par.terms(xb[2], inp.u * st, inp.tout, inp.wind, inp.rad, inp.night, cn, cs, h)
            fm = [f11, f12, 0.0, f21, f22, h * amb * (inp.tout - xb[1]), 0.0, 0.0, self.phi]
            fp = mat_mul(mat_mul(fm, pb), mat_t(fm))
            fp[0] += par.q_r * h
            fp[4] += par.q_a * h
            fp[8] += self.q_l[mt]
            new_x.append([f11 * xb[0] + f12 * xb[1] + g1, f21 * xb[0] + f22 * xb[1] + g2, 1.0 + self.phi * (xb[2] - 1.0)])
            new_p.append([0.5 * (fp[3 * i + k] + fp[3 * k + i]) for i in range(3) for k in range(3)])
        for s in range(2):
            t = new_nu[s][0] + new_nu[s][1]
            new_nu[s] = [new_nu[s][0] / t, new_nu[s][1] / t] if t > 0 else [1.0, 0.0]
        self.x, self.p, self.nu = new_x, new_p, new_nu


class House:
    def __init__(self, rooms, shock, house):
        self.shock = shock
        self.house = dict(house)
        pu, pd = house["p_up"], house["p_down"]
        self.house["ts_tr"] = [[1.0 - pu, pu], [pd, 1.0 - pd]]
        self.rooms = {n: RoomFilter(n, RoomPar(d), shock, self.house) for n, d in rooms.items()}
        self.w_prior = [1.0 - house["p_supply0"], house["p_supply0"]]
        self.w = list(self.w_prior)
        self.loglik = 0.0
        self.step_n = 0
        self.resets = []

    def step(self, inputs):
        n = self.step_n
        self.step_n += 1
        wp = list(self.w_prior)
        scale = 0.0
        for name, f in self.rooms.items():
            r = f.update(inputs[name], self.w_prior, n)
            if r is not None:
                wp = [wp[s] * r[0][s] for s in range(2)]
                scale += r[1]
        tot = wp[0] + wp[1]
        if tot > 0 and math.isfinite(tot):
            self.loglik += scale + math.log(tot)
            self.w = [wp[0] / tot, wp[1] / tot]
        else:
            self.w = list(self.w_prior)
            for f in self.rooms.values():
                f.flag = "nan" if not f.skipped else None
        outs = {}
        for name, f in self.rooms.items():
            outs[name] = f.finish(inputs[name], self.w, n)
            self.resets += f.events
            f.events = []
        tr = self.house["ts_tr"]
        self.w_prior = [self.w[0] * tr[0][0] + self.w[1] * tr[1][0], self.w[0] * tr[0][1] + self.w[1] * tr[1][1]]
        return outs


def rollout(par, m, a, lam, s, sp, env):
    h = 1.0 / 60.0
    en, co, sens = [], [], []
    for k in range(HORIZON):
        u = min(max(par.g * (sp[k] - par.sensor(m, a)), 0.0), 1.0)
        f11, f12, f21, f22, g1, g2, _ = par.terms(lam, u * s, env["tout"], env["wind"], env["rad"], env["night"], env["cn"], env["cs"], h)
        m, a = f11 * m + f12 * a + g1, f21 * m + f22 * a + g2
        sens.append(par.sensor(m, a))
        en.append(u * s)
        co.append(max(0.0, env["target"] - sens[-1]) * env["present"])
    return en, co, sens


def profile(first, d, then):
    return [first] * d + [then] * (HORIZON - d)


def plan(par, shock, comps, p_supply, env):
    lam_w = shock["lambda_w"]
    mu = [c[0] for c in comps]
    m, a = (sum(mu[j] * comps[j][1][i] for j in range(len(comps))) for i in (0, 1))
    q = [1.0 - norm_cdf((lam_w - c[1][2]) / math.sqrt(max(c[2][8], 1e-18))) for c in comps]
    pw = sum(mu[j] * q[j] for j in range(len(comps)))
    wt = [mu[j] * (1.0 - q[j]) for j in range(len(comps))]
    lam_c = sum(wt[j] * min(comps[j][1][2], lam_w) for j in range(len(comps))) / sum(wt) if sum(wt) > 0 else 1.0
    lam_o = max(shock["lambda_jump"], sum(mu[j] * q[j] * comps[j][1][2] for j in range(len(comps))) / pw) if pw > 0 else shock["lambda_jump"]
    env = dict(env)
    env["cn"], env["cs"] = par.coupling(env["ncnt"], env["nsum"])
    modes = ((1.0 - pw, lam_c), (pw, lam_o))
    supplies = ((1, p_supply), (0, 1.0 - p_supply))
    calm = (1.0, 0.0)
    target, frost, cur, cold_w = env["target"], env["frost"], env["sp_cur"], env["cold_cost"]
    sensor0 = par.sensor(m, a)
    limit = max(target + 0.5, sensor0) + EPS

    def evaluate(name, kind, sp, first, **meta):
        runs = [[rollout(par, m, a, lam, s, sp, env) for s, _ in supplies] for _, lam in modes]
        mode_cost = [sum(supplies[si][1] * (sum(r[si][0]) + calm[mi] * cold_w * sum(r[si][1])) for si in range(2)) for mi, r in enumerate(runs)]
        rz = cur is not None and first > cur + EPS
        raise_c = env["raise_cost"] if rz else 0.0
        energy = sum(modes[mi][0] * supplies[si][1] * sum(runs[mi][si][0]) for mi in range(2) for si in range(2))
        cold = sum(modes[0][0] * supplies[si][1] * sum(runs[0][si][1]) for si in range(2))
        return {"name": name, "kind": kind, "first": first, "runs": runs, "mode_cost": [c + raise_c for c in mode_cost], "energy": energy, "cold": cold_w * cold, "raise": raise_c, "raise_flag": rz,
                "total": sum(modes[mi][0] * mode_cost[mi] for mi in range(2)) + raise_c, "peak": max(runs[0][0][2]), "boost_min": None, "probe_min": None, **meta}

    base = [("live", env["sp_live"]), ("current", cur), ("target", target), ("frost", frost), ("minus", target - 0.5), ("plus", target + 0.5)]
    cands, seen = [], []
    for name, v in base:
        if v is None or any(abs(v - s) < EPS for s in seen):
            continue
        seen.append(v)
        cands.append(evaluate(name, name, profile(v, HORIZON, v), v))
    live = cands[0]
    if env["boost_ok"]:
        for d in BOOSTS:
            cands.append(evaluate(f"boost{d}", "boost", profile(env["boost_c"], d, target), env["boost_c"], boost_min=d))
    allowed = [c for c in cands if c["kind"] == "frost" or c["peak"] <= limit] or [min(cands, key=lambda c: c["peak"])]
    best = allowed[0]
    for c in allowed[1:]:
        if c["total"] < best["total"] - 1e-12:
            best = c
    perf = sum(modes[mi][0] * min(c["mode_cost"][mi] for c in allowed) for mi in range(2))
    gain = max(best["total"] - perf, 0.0)
    sigma_obs = math.sqrt(env["r_grid"] + par.noise_r ** 2)
    chosen, evoi_best = best, 0.0
    for d in PROBES:
        pr = evaluate(f"probe{d}", "probe", profile(target, d, frost), target, probe_min=d)
        if pr["peak"] > limit:
            continue
        sep = abs(pr["runs"][1][0][2][d - 1] - pr["runs"][0][0][2][d - 1]) / sigma_obs
        rho = 1.0 - 2.0 * norm_cdf(-sep / 2.0)
        prefix = lambda c: sum(modes[mi][0] * supplies[si][1] * (sum(c["runs"][mi][si][0][:d]) + calm[mi] * cold_w * sum(c["runs"][mi][si][1][:d])) for mi in range(2) for si in range(2))
        evoi = rho * gain - max(0.0, prefix(pr) - prefix(best) + pr["raise"] - best["raise"])
        if evoi > evoi_best + 1e-12:
            chosen, evoi_best = pr, evoi
    fc = sum(modes[mi][0] * supplies[si][1] * live["runs"][mi][si][2][FORECAST_MIN - 1] for mi in range(2) for si in range(2))
    return {"kind": chosen["kind"], "name": chosen["name"], "setpoint": chosen["first"], "raise": chosen["raise_flag"], "probe_min": chosen["probe_min"], "boost_min": chosen["boost_min"],
            "energy": chosen["energy"], "cold": chosen["cold"], "raise_cost": chosen["raise"], "cost_live": live["total"], "total": chosen["total"], "p_window": pw, "lam_closed": lam_c, "lam_open": lam_o,
            "mass": m, "air": a, "sensor": sensor0, "forecast": fc, "evoi": evoi_best, "best_cost": best["total"], "perfect_cost": perf, "n_allowed": len(allowed)}
