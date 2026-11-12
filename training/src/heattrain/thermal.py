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
DEV_TEMP, DEV_RH, DEV_CO2 = 0x0402, 0x0405, 0x040D
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


def load_devices(path):
    p = Path(path)
    rows = []
    t = read_arrows(p, "device_readings")
    if t is not None:
        c = {n: t.column(n).to_pylist() for n in ("node", "cluster", "attribute", "value")}
        ts = epoch(t.column("ts").to_pylist())
        rows += [(ts[i], c["node"][i], c["cluster"][i], float(c["value"][i])) for i in range(t.num_rows) if c["attribute"][i] == 0 and c["cluster"][i] in (DEV_TEMP, DEV_RH, DEV_CO2) and c["value"][i] is not None]
    base = p / "parquet" / "device_readings"
    if base.is_dir() and any(base.rglob("part.parquet")):
        sql = f"SELECT epoch(ts), node, cluster, value FROM read_parquet('{base.as_posix()}/**/part.parquet', hive_partitioning=false, union_by_name=true) WHERE attribute = 0 AND cluster IN ({DEV_TEMP},{DEV_RH},{DEV_CO2}) AND value IS NOT NULL"
        rows += [(float(a), int(b), int(c), float(d)) for a, b, c, d in duckdb.connect().execute(sql).fetchall()]
    return sorted(merge_rows(rows))


def device_ext(devices, nodes, grid, bad, cfg):
    by = {}
    for ts, node, cluster, v in devices:
        if node in nodes:
            by.setdefault((node, cluster), []).append((ts, v))
    by = {k: tuple(np.array(x) for x in zip(*sorted(v))) for k, v in by.items()}
    dt, n = cfg["dt_s"], len(grid)
    held = lambda key, scale: np.where(stale_mask(grid, by[key][0], cfg["device_stale_s"]), np.nan, ffill_at(by[key][0], by[key][1] / scale, grid))
    ext = {"ya": [], "co2": np.full(n, np.nan), "rh": np.full(n, np.nan), "th": np.full(n, np.nan), "nodes": {"air": [], "co2": None, "hum": None}}
    for node in sorted(nodes):
        if (node, DEV_TEMP) in by:
            t, v = by[(node, DEV_TEMP)]
            v = v / 100.0
            ok = (v > cfg["temp_valid"][0]) & (v < cfg["temp_valid"][1])
            arr = np.full(n, np.nan)
            arr[np.clip(np.rint((t[ok] - grid[0]) / dt).astype(int), 0, n - 1)] = v[ok]
            arr[bad] = np.nan
            if np.isfinite(arr).any():
                ext["ya"].append(arr)
                ext["nodes"]["air"].append(node)
    for node in sorted(nodes):
        if ext["nodes"]["co2"] is None and (node, DEV_CO2) in by:
            ext["co2"], ext["nodes"]["co2"] = held((node, DEV_CO2), 1.0), node
        if ext["nodes"]["hum"] is None and (node, DEV_RH) in by and (node, DEV_TEMP) in by:
            ext["rh"], ext["th"], ext["nodes"]["hum"] = held((node, DEV_RH), 100.0), held((node, DEV_TEMP), 100.0), node
    return ext


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


def ffill_finite(t, v, grid):
    ok = np.isfinite(v)
    return ffill_at(t[ok], v[ok], grid) if ok.any() else np.full(len(grid), np.nan)


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
    return [ffill_finite(t, cols[:, i], grid) for i in range(3)]


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
    ext: dict = None

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
                        None if self.bad is None else s(self.bad), None if self.yv is None else s(self.yv),
                        None if self.ext is None else {k: (v if k == "nodes" else [s(a) for a in v] if isinstance(v, list) else s(v)) for k, v in self.ext.items()})


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


def build_rooms(reports, weather, meta, cfg, events=(), devices=()):
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
    dev_room = {d["node"]: d["room"] for d in m.get("devices", []) if d.get("room") is not None}
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
        room = base_room = node_room.get(node, f"node{node}")
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
                    ni = ffill_finite(nt, nv / 100.0, grid)
                    nbn.setdefault(l.get("kind", "wall"), []).append(nn)
                    cnt, tot = nb.setdefault(l.get("kind", "wall"), (np.zeros(len(grid)), np.zeros(len(grid))))
                    cnt += 1
                    tot += ni
        d = RoomData(room, node, float(grid[0]), float(dt), y, interp(t, v, grid), u, sp, tout, wind, rad, night_mask(grid, cfg), nb,
                     {"base_room": base_room, "n_invalid_demand": n_invalid, "n_temp_reports": int(len(t)), "n_demand_reports": int(good.sum()), "start": float(grid[0]), "end": float(grid[-1]), "n_masked": int(bad.sum()), "neighbour_nodes": nbn}, bad, yv)
        if devices:
            d.ext = device_ext(devices, {n for n, r in dev_room.items() if r == base_room}, grid, bad, cfg)
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
    w1 = 1 - w
    ww, w2, w11 = w * w, 2 * w * w1, w1 ** 2
    for k, (yk, a11, a12, a21, a22, e1, e2) in enumerate(zip(y, A11, A12, A21, A22, c1, c2)):
        if yk == yk:
            s = ww * p11 + w2 * p12 + w11 * p22 + r
            v = yk - (w * x1 + w1 * x2)
            h1, h2 = w * p11 + w1 * p12, w * p12 + w1 * p22
            k1, k2 = h1 / s, h2 / s
            x1 += k1 * v
            x2 += k2 * v
            p11, p12, p22 = p11 - k1 * h1, p12 - k1 * h2, p22 - k2 * h2
            ks.append(k)
            vs.append(v)
            ss.append(s)
        if collect:
            xs.append((x1, x2))
            ps.append((p11, p12, p22) if collect == "full" else p22)
        t11, t12 = a11 * p11 + a12 * p12, a11 * p12 + a12 * p22
        t21, t22 = a21 * p11 + a22 * p12, a21 * p12 + a22 * p22
        p11, p12, p22 = t11 * a11 + t12 * a12 + q1, t11 * a21 + t12 * a22, t21 * a21 + t22 * a22 + q2
        x1, x2 = a11 * x1 + a12 * x2 + e1, a21 * x1 + a22 * x2 + e2
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
    held = {}
    for struct in ("rc2", "rc1"):
        tr, x = fit(d.slice(0, ns) if can_hold else d, struct, cfg)
        held[struct] = (x, horizon_error(tr.p, d, struct, cfg, ns))
    f2, _ = fit(d, "rc2", cfg, held["rc2"][0])
    m2, m1 = held["rc2"][1], held["rc1"][1]
    key = [n for n in ("a2", "b1", "w") if n in f2.rel_std]
    ident = all(f2.rel_std[n] < cfg["std_max"] for n in key) and not (set(key) & set(f2.at_bound))
    better = m2["rmse_horizon"] is not None and m1["rmse_horizon"] is not None and m2["rmse_horizon"] <= m1["rmse_horizon"] * cfg["rc2_margin"]
    chosen = "rc2" if ident and better else "rc1"
    if chosen == "rc1":
        warnings.append("2-state model not identifiable or not better on held-out block: 1-state fallback" if not ident else "2-state not better than 1-state on held-out block: 1-state used")
    f, m = f2 if chosen == "rc2" else fit(d, "rc1", cfg, held["rc1"][0])[0], held[chosen][1]
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
        "metrics": {chosen: m, "alternative": {"structure": "rc1" if chosen == "rc2" else "rc2", **held["rc1" if chosen == "rc2" else "rc2"][1]}, "holdout": can_hold, "fit_rmse_normalised": f.cost},
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
