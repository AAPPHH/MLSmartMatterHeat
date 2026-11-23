import csv
import json
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
        cols[room] = {"ms": np.array([int(r["ms"]) for r in rs]), **{k: np.array([float(r[k]) for r in rs]) for k in ("z", "valve", "co2", "rh", "temp")}}
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
    tables["stats"] = {"positive_minutes": {r: int(p.sum()) for r, p in pos.items()}, "strong_minutes": {r: int(p.sum()) for r, p in strong.items()}, "channels": stats, "alarm": alarm_stats, "excluded_days": list(exclude_days), "spans": len(spans)}
    Path(out_path).write_text(json.dumps(tables, indent=1))
    return tables


def run_airwin(trace_path, dump, out_path, exclude=""):
    tables = learn(CONFIG, trace_path, dump, out_path, [d for d in exclude.split(",") if d])
    print(json.dumps({k: tables[k] for k in ("temper", "alarm_llr", "trv", "stats")}, indent=1))
