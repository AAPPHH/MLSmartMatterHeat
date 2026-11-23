import csv
import json
import tomllib
from datetime import datetime, timezone
from pathlib import Path
import numpy as np

CONFIG = {
    "airwin": {
        "quantiles": [0.001, 0.002, 0.004, 0.008, 0.015, 0.03, 0.06, 0.12, 0.25, 0.5, 0.75, 0.9, 0.97, 0.99],
        "pseudo_min": 25.0,
        "cap": 6.0,
        "guard_min": 10,
        "weak_quantile": 0.004,
        "weak_channels": 2,
        "use_weak": True,
        "temper_floor": 0.06,
        "temper_cap": 0.5,
        "censor_k": 0.25,
        "alarm_clip": [1.0, 7.0],
        "neutral_floor": 0.0,
        "neutral_quantile": 0.8,
        "act_lag_air": 6,
        "act_lag_trv": 20,
        "physics": {
            "fit_min": 15,
            "fit_points": 4,
            "open_lambda_h": 3.0,
            "outdoor_co2_ppm": 420.0,
            "outdoor_rh_pct": 80.0,
            "min_excess_ppm": 100.0,
            "min_excess_ah": 1.0,
            "valve_open_pct": 50.0,
            "flow_k": 25.0,
            "scale_lo": 0.25,
            "scale_hi": 4.0,
            "shrink": 400.0,
            "default_weight": [0.14, 0.086, 0.06, 0.05],
            "raw_columns": ["r_co2", "r_rh", "r_temp", "r_trv"],
            "raw_cap": 12.0,
            "noise_exclude_days": ["11-22", "11-23"],
            "noise_lambda_h": 0.4,
            "noise_lag_max": 40,
            "sigma_co2_ppm": 32.0,
            "sigma_rh_pct": 0.316,
            "sigma_temp_k": 0.0218,
        },
        "confirmed": [
            ["schlafzimmer", "2026-11-23T07:27", "2026-11-23T07:36"],
            ["schlafzimmer", "2026-11-23T08:00", "2026-11-23T08:50"],
            ["kueche", "2026-11-22T09:43", "2026-11-22T09:48"],
            ["wohnzimmer", "2026-11-22T09:43", "2026-11-22T09:48"],
        ],
    }
}
MIN_MS = 60_000
CHANNELS = ("co2", "rh", "temp")
RAW = ("r_trv", "r_co2", "r_rh", "r_temp", "o_co2_rate", "o_co2_level", "o_co2_sd", "o_ah_rate", "o_ah_level", "o_ah_sd", "o_t_rate", "o_t_level", "o_t_sd")


def utc_ms(text):
    return int(datetime.strptime(text, "%Y-%m-%dT%H:%M").replace(tzinfo=timezone.utc).timestamp() * 1000)


def read_csv(path):
    with open(path, newline="") as f:
        return list(csv.DictReader(f))


def load_trace(path):
    rows = read_csv(path)
    out = {}
    for r in rows:
        out.setdefault(r["room"], []).append(r)
    cols = {}
    for room, rs in out.items():
        cols[room] = {"ms": np.array([int(r["ms"]) for r in rs]), **{k: np.array([float(r[k]) for r in rs]) for k in ("z", "valve", "co2", "rh", "temp") + tuple(k for k in RAW if k in rs[0])}}
    return cols


def yes_episodes(dump):
    out = []
    for r in read_csv(Path(dump) / "events.csv"):
        if r["kind"] != "window_episode":
            continue
        d = json.loads(r["detail"])
        if d.get("label") == "yes":
            out.append([d["room"], int(d["start_ms"]), int(d["end_ms"])])
    return out


def positives(cfg, trace, dump, exclude_days):
    spans = [[room, utc_ms(a), utc_ms(b)] for room, a, b in cfg["confirmed"]] + yes_episodes(dump)
    skip = [(utc_ms(f"2026-{d}T00:00"), utc_ms(f"2026-{d}T00:00") + 86_400_000) for d in exclude_days]
    spans = [s for s in spans if not any(a <= s[1] < b for a, b in skip)]
    pos = {room: np.zeros(len(t["ms"]), bool) for room, t in trace.items()}
    for room, a, b in spans:
        if room in trace:
            pos[room] |= (trace[room]["ms"] >= a) & (trace[room]["ms"] <= b)
    return pos, spans


def weak_positives(cfg, trace, strong):
    out = {}
    cuts = {c: np.nanquantile(np.concatenate([t[c][~strong[r]] for r, t in trace.items() if np.isfinite(t[c]).any()]), cfg["weak_quantile"]) for c in CHANNELS}
    for room, t in trace.items():
        hits = sum((np.nan_to_num(t[c], nan=1e9) <= cuts[c]).astype(int) for c in CHANNELS)
        out[room] = hits >= cfg["weak_channels"]
    return out, cuts


def pav_decreasing(y, w):
    y, w = list(y), list(w)
    blocks = [[v, wt, 1] for v, wt in zip(y, w)]
    i = 0
    while i < len(blocks) - 1:
        if blocks[i][0] < blocks[i + 1][0]:
            a, b = blocks[i], blocks[i + 1]
            blocks[i] = [(a[0] * a[1] + b[0] * b[1]) / (a[1] + b[1]), a[1] + b[1], a[2] + b[2]]
            del blocks[i + 1]
            i = max(i - 1, 0)
        else:
            i += 1
    return np.concatenate([[b[0]] * b[2] for b in blocks])


def curve(cfg, values, pos):
    ok = np.isfinite(values)
    v, p = values[ok], pos[ok]
    prior = max(p.mean(), 1e-6)
    qs = np.unique(np.quantile(v[~p], cfg["quantiles"]))
    idx = np.clip(np.searchsorted(qs, v), 0, len(qs))
    edges, llr, weight = [], [], []
    for b in range(len(qs) + 1):
        m = idx == b
        if not m.any():
            continue
        n, k = m.sum(), p[m].sum()
        frac = (k + cfg["pseudo_min"] * prior) / (n + cfg["pseudo_min"])
        edges.append(float(np.median(v[m])))
        llr.append(float(np.log(frac / (1 - frac)) - np.log(prior / (1 - prior))))
        weight.append(float(n))
    llr = np.clip(pav_decreasing(llr, weight), -cfg["cap"], cfg["cap"])
    calm = np.quantile(v[~p], cfg["neutral_quantile"])
    llr = np.where(np.array(edges) <= calm, np.maximum(llr, cfg["neutral_floor"]), llr)
    return {"edges": edges, "llr": [round(float(x), 3) for x in llr]}, {"pos": int(p.sum()), "all": int(len(p))}


def interp(c, x):
    return np.interp(x, c["edges"], c["llr"])


def act(series_list, lag_max):
    num = den = 0.0
    tau = []
    for s in series_list:
        s = s[np.isfinite(s)]
        if len(s) < 200:
            continue
        s = s - s.mean()
        var = (s * s).mean()
        if var <= 0:
            continue
        tot = 1.0
        for k in range(1, lag_max):
            rho = (s[:-k] * s[k:]).mean() / var
            if rho < 0.05:
                break
            tot += 2 * rho
        tau.append(tot)
    return float(np.median(tau)) if tau else 1.0


def trv_params(cfg, trace, pos, bundle):
    z_closed = {0: [], 1: []}
    z_open = {0: [], 1: []}
    for room, t in trace.items():
        regime = (t["valve"] > bundle["open_valve_pct"]).astype(int)
        for r in (0, 1):
            ok = np.isfinite(t["z"]) & (regime == r)
            z_closed[r].append(t["z"][ok & ~pos[room]])
            z_open[r].append(t["z"][ok & pos[room]])
    allc = np.concatenate(z_closed[0] + z_closed[1])
    mad = lambda x: 1.4826 * np.median(np.abs(x - np.median(x)))
    closed_sd = float(max(mad(allc), 0.8))
    mean, counts = [], []
    for r in (0, 1):
        o = np.concatenate(z_open[r])
        counts.append(int(len(o)))
        mean.append(float(max(np.median(o), 1.0)) if len(o) >= 15 else 2.0)
    return {"censor_k": cfg["censor_k"], "closed_sd": round(closed_sd, 3), "open_mean": [round(m, 3) for m in mean], "cap": 3.0}, counts


def alarm_llr(cfg, trace, pos, bundle):
    hit_o = hit_c = n_o = n_c = 0
    for room, t in trace.items():
        thr = bundle["thresholds"].get(room)
        ok = np.isfinite(t["z"])
        if thr is None:
            continue
        above = ok & (t["z"] > thr)
        hit_o += int(above[pos[room]].sum())
        n_o += int((ok & pos[room]).sum())
        hit_c += int(above[~pos[room]].sum())
        n_c += int((ok & ~pos[room]).sum())
    po, pc = (hit_o + 1) / (n_o + 2), (hit_c + 1) / (n_c + 2)
    lo, hi = cfg["alarm_clip"]
    return float(np.clip(np.log(po / pc), lo, hi)), {"p_alarm_open": po, "p_alarm_closed": pc}


def learn(config, trace_path, dump, out_path, exclude_days=()):
    cfg = config["airwin"]
    trace = load_trace(trace_path)
    if exclude_days:
        for room, t in trace.items():
            days = np.array([datetime.fromtimestamp(m / 1000, timezone.utc).strftime("%m-%d") for m in t["ms"]])
            keep = ~np.isin(days, list(exclude_days))
            trace[room] = {k: v[keep] for k, v in t.items()}
    bundle = json.loads((Path(dump) / "bundle.json").read_text())
    strong, spans = positives(cfg, trace, dump, exclude_days)
    pos = {r: strong[r].copy() for r in strong}
    if cfg["use_weak"]:
        weak, cuts = weak_positives(cfg, trace, strong)
        for r in pos:
            pos[r] |= weak[r]
    guard = cfg["guard_min"] * MIN_MS
    for room, t in trace.items():
        near = np.zeros(len(t["ms"]), bool)
        for a, b in [(s[1], s[2]) for s in spans if s[0] == room]:
            near |= (t["ms"] >= a - guard) & (t["ms"] <= b + guard)
        pos[room] = pos[room] & ~(near & ~strong[room])
    tables = {"version": "airwin-" + datetime.now(timezone.utc).strftime("%Y%m%dT%H%M%SZ")}
    stats = {}
    for c in CHANNELS:
        feats = np.concatenate([t[c] for t in trace.values()])
        flags = np.concatenate([pos[r] for r in trace])
        tables[c], stats[c] = curve(cfg, feats, flags)
    tables["trv"], stats["trv_open_minutes"] = trv_params(cfg, trace, pos, bundle)
    llr_series = {c: [interp(tables[c], t[c]) for t in trace.values()] for c in CHANNELS}
    temper = {c: round(float(np.clip(1.0 / act(llr_series[c], cfg["act_lag_air"]), cfg["temper_floor"], cfg["temper_cap"])), 3) for c in CHANNELS}
    z_series = [np.nan_to_num(t["z"], nan=np.nan) for t in trace.values()]
    temper["trv"] = round(float(np.clip(1.0 / act(z_series, cfg["act_lag_trv"]), cfg["temper_floor"], cfg["temper_cap"])), 3)
    tables["temper"] = {"trv": temper["trv"], "co2": temper["co2"], "rh": temper["rh"], "temp": temper["temp"]}
    alarm, alarm_stats = alarm_llr(cfg, trace, pos, bundle)
    tables["alarm_llr"] = round(alarm, 3)
    tables["physics"] = physics_fit(config, dump, [tuple(sp) for sp in spans], trace, pos)
    tables["stats"] = {"positive_minutes": {r: int(p.sum()) for r, p in pos.items()}, "strong_minutes": {r: int(p.sum()) for r, p in strong.items()}, "channels": stats, "alarm": alarm_stats, "excluded_days": list(exclude_days), "spans": len(spans)}
    Path(out_path).write_text(json.dumps(tables, indent=1))
    return tables


def absolute_humidity(rh, t):
    return 216.7 * rh / 100.0 * 6.112 * np.exp(17.62 * t / (243.12 + t)) / (273.15 + t)


def decay_lambda(ms, excess, min_points):
    ok = excess > 0
    if ok.sum() < min_points:
        return None
    t_h = (ms[ok] - ms[ok][0]) / 3_600_000.0
    slope = np.polyfit(t_h, np.log(excess[ok]), 1)[0]
    return float(-slope) if np.isfinite(slope) else None


class DumpAir:
    def __init__(self, dump):
        dump = Path(dump)
        conf = tomllib.loads((dump / "config.toml").read_text())
        self.room_of_air = {d["node"]: d.get("room") for d in conf.get("devices", []) if d.get("room")}
        self.room_of_trv = {n: r["name"] for r in conf["rooms"] for n in r.get("nodes", [])}
        self.air = {}
        for r in read_csv(dump / "air.csv"):
            self.air.setdefault((int(r["node"]), int(r["cluster"])), []).append((int(r["ms"]), float(r["value"])))
        self.trv = {}
        for r in read_csv(dump / "trv.csv"):
            if r["value"]:
                self.trv.setdefault((int(r["node"]), int(r["attribute"])), []).append((int(r["ms"]), float(r["value"])))
        self.weather = np.array([(int(r["ms"]), float(r["temp_c"])) for r in read_csv(dump / "weather.csv")])

    def outdoor(self, ms):
        return float(np.interp(ms, self.weather[:, 0], self.weather[:, 1]))

    def series(self, room, cluster):
        for (node, cl), pts in self.air.items():
            if cl == cluster and self.room_of_air.get(node) == room:
                a = np.array(pts)
                return a[:, 0], a[:, 1]
        return None

    def trv_series(self, room, attr):
        for (node, at), pts in self.trv.items():
            if at == attr and self.room_of_trv.get(node) == room:
                a = np.array(pts)
                return a[:, 0], a[:, 1]
        return None

    def episode_fit(self, cfg, room, a, b):
        end = min(b, a + cfg["fit_min"] * MIN_MS)
        co2 = self.series(room, 1037)
        if co2 is not None:
            m = (co2[0] >= a) & (co2[0] <= end)
            excess = co2[1][m] - cfg["outdoor_co2_ppm"]
            if m.sum() >= cfg["fit_points"] and excess.mean() >= cfg["min_excess_ppm"]:
                lam = decay_lambda(co2[0][m], excess, cfg["fit_points"])
                if lam is not None:
                    return "co2", lam
        rh, temp = self.series(room, 1029), self.series(room, 1026)
        if rh is not None and temp is not None:
            m = (rh[0] >= a - 6 * MIN_MS) & (rh[0] <= end)
            tt = np.interp(rh[0][m], temp[0], temp[1]) / 100.0
            ah = absolute_humidity(rh[1][m] / 100.0, tt)
            out = absolute_humidity(cfg["outdoor_rh_pct"], np.array([self.outdoor(x) for x in rh[0][m]]))
            excess = ah - out
            if m.sum() >= cfg["fit_points"] and excess.mean() >= cfg["min_excess_ah"]:
                lam = decay_lambda(rh[0][m], excess, cfg["fit_points"])
                if lam is not None:
                    return "ah", lam
        return None

    def coupling(self, cfg, room):
        temp, valve, loc = self.series(room, 1026), self.trv_series(room, 8), self.trv_series(room, 0)
        if temp is None or valve is None or loc is None:
            return None
        grid = np.arange(max(temp[0][0], loc[0][0], valve[0][0]), min(temp[0][-1], loc[0][-1], valve[0][-1]), 60_000)
        air = np.interp(grid, temp[0], temp[1]) / 100.0
        trv = np.interp(grid, loc[0], loc[1]) / 100.0
        v = np.interp(grid, valve[0], valve[1])
        diff = trv - air
        hi, lo = v >= cfg["valve_open_pct"], v <= 1.0
        if hi.sum() < 120 or lo.sum() < 120:
            return None
        k = 10
        d_air = (air[k:] - air[:-k]) / k
        beta = float(np.clip((np.median(diff[hi]) - np.median(diff[lo])) / cfg["flow_k"], 0.0, 1.0))
        x = np.c_[np.ones(len(d_air)), v[:-k] / 100.0]
        coef = np.linalg.lstsq(x, d_air, rcond=None)[0]
        return beta, float(max(coef[1], 0.0))


def closed_noise(cfg, trace, dump):
    d = DumpAir(dump)
    skip = [(utc_ms(f"2026-{day}T00:00"), utc_ms(f"2026-{day}T00:00") + 86_400_000) for day in cfg["noise_exclude_days"]]
    lam = cfg["noise_lambda_h"]
    res = {"co2": [], "ah": [], "temp": []}
    sd_model = {"co2": [], "ah": [], "temp": []}
    times = {"co2": [], "ah": [], "temp": []}
    for room, t in trace.items():
        if "o_co2_rate" not in t:
            return None
        keep = np.ones(len(t["ms"]), bool)
        for a, b in skip:
            keep &= ~((t["ms"] >= a) & (t["ms"] < b))
        out_c = np.array([d.outdoor(ms) for ms in t["ms"]])
        out_ah = absolute_humidity(cfg["outdoor_rh_pct"], out_c)
        pairs = {"co2": (t["o_co2_rate"], t["o_co2_level"], t["o_co2_sd"], cfg["outdoor_co2_ppm"], lam), "ah": (t["o_ah_rate"], t["o_ah_level"], t["o_ah_sd"], out_ah, lam), "temp": (t["o_t_rate"], t["o_t_level"], t["o_t_sd"], out_c, 0.0)}
        for name, (rate, level, sd, out, l) in pairs.items():
            ok = keep & np.isfinite(rate) & np.isfinite(level)
            if ok.sum() < 200:
                continue
            res[name].append(rate[ok] + l * (level[ok] - (out[ok] if hasattr(out, "__len__") else out)) / 60.0)
            sd_model[name].append(sd[ok])
    out = {}
    for name in res:
        if not res[name]:
            continue
        r = np.concatenate(res[name])
        lower = float(np.median(r) - np.quantile(r, 0.1587))
        model = float(np.median(np.concatenate(sd_model[name])))
        out[name] = {"lower_sd": lower, "model_sd": model, "scale": lower / model, "source": float(np.mean(np.maximum(r - np.median(r), 0.0))), "act": float(np.median([act([x - np.mean(x)], cfg["noise_lag_max"]) for x in [np.clip(v, *np.quantile(v, [0.02, 0.98])) for v in res[name]]]))}
    return out


def noise_priors(cfg, noise, defaults):
    if not noise:
        return None
    out = {"weight": [round(1.0 / noise[c]["act"], 3) if c in noise else w for c, w in zip(("co2", "ah", "temp"), defaults)] + [defaults[3]]}
    out["co2_sigma_ppm"] = round(defaults_sigma("co2", noise, cfg), 2) if "co2" in noise else None
    out["rh_sigma_pct"] = round(defaults_sigma("ah", noise, cfg), 3) if "ah" in noise else None
    out["temp_sigma_k"] = round(defaults_sigma("temp", noise, cfg), 4) if "temp" in noise else None
    out["co2_source_ppm_min"] = round(noise["co2"]["source"], 2) if "co2" in noise else None
    out["ah_source_g_min"] = round(noise["ah"]["source"], 4) if "ah" in noise else None
    return out


def defaults_sigma(name, noise, cfg):
    base = {"co2": cfg["sigma_co2_ppm"], "ah": cfg["sigma_rh_pct"], "temp": cfg["sigma_temp_k"]}[name]
    return base * noise[name]["scale"]


def tempering(cfg, trace, pos, base):
    cols = cfg["raw_columns"]
    if not all(c in next(iter(trace.values())) for c in cols):
        return list(base), None
    x = np.concatenate([np.clip(np.nan_to_num(np.c_[[t[c] for c in cols]].T), -cfg["raw_cap"], cfg["raw_cap"]) for t in trace.values()])
    y = np.concatenate([pos[r].astype(float) for r in trace])
    prior = np.r_[0.0, np.ones(len(cols))]
    beta = prior.copy()
    reg = np.r_[0.0, np.full(len(cols), cfg["shrink"])]
    z = np.c_[np.ones(len(y)), x]
    for _ in range(40):
        p = 1 / (1 + np.exp(-np.clip(z @ beta, -30, 30)))
        w = np.maximum(p * (1 - p), 1e-6)
        grad = z.T @ (y - p) - reg * (beta - prior)
        hess = (z * w[:, None]).T @ z + np.diag(reg)
        step = np.linalg.solve(hess, grad)
        beta = beta + step
        beta[1:] = np.clip(beta[1:], cfg["scale_lo"], cfg["scale_hi"])
        if np.abs(step).max() < 1e-6:
            break
    scale = [round(float(v), 4) for v in beta[1:]]
    return [round(w * s, 5) for w, s in zip(base, scale)], scale


def physics_fit(config, dump, spans, trace=None, pos=None):
    cfg = config["airwin"]["physics"]
    d = DumpAir(dump)
    episodes = []
    for room, a, b in spans:
        fit = d.episode_fit(cfg, room, a, b)
        if fit:
            episodes.append({"room": room, "from_ms": a, "to_ms": b, "signal": fit[0], "lambda_h": round(fit[1], 3)})
    out = {"episodes": episodes, "lambda_open": {}, "lambda_kipp": {}}
    for room in sorted({e["room"] for e in episodes}):
        lam = [e["lambda_h"] for e in episodes if e["room"] == room]
        hi = [x for x in lam if x >= cfg["open_lambda_h"]]
        lo = [x for x in lam if x < cfg["open_lambda_h"]]
        if hi:
            out["lambda_open"][room] = round(float(np.median(hi)), 3)
        if lo:
            out["lambda_kipp"][room] = round(float(np.median(lo)), 3)
    coupled = [c for c in (d.coupling(cfg, r) for r in sorted(set(d.room_of_air.values()))) if c]
    if coupled:
        out["trv_beta"] = round(float(np.mean([c[0] for c in coupled])), 4)
        out["valve_gain_k_min"] = round(float(np.mean([c[1] for c in coupled])), 5)
    if trace is not None:
        noise = closed_noise(cfg, trace, dump)
        out["noise"] = noise_priors(cfg, noise, cfg["default_weight"])
        out["noise_stats"] = noise
    if trace is not None and pos is not None:
        out["weight"], out["scale"] = tempering(cfg, trace, pos, cfg["default_weight"])
    return out


def run_airwin(trace_path, dump, out_path, exclude=""):
    tables = learn(CONFIG, trace_path, dump, out_path, [d for d in exclude.split(",") if d])
    print(json.dumps({k: tables[k] for k in ("temper", "alarm_llr", "trv", "physics", "stats")}, indent=1))
