import importlib
import json
from datetime import datetime, timedelta, timezone
from pathlib import Path
import numpy as np
import pyarrow as pa
import pyarrow.parquet as pq
from numpy.lib.stride_tricks import sliding_window_view
from heattrain.thermal import read_arrows

ATTRS = {0: "temp", 8: "valve", 18: "setpoint"}


def tables(lake, table, columns):
    root = Path(lake)
    out = [pq.read_table(f, columns=columns) for f in sorted((root / "parquet" / table).glob("**/part.parquet"))]
    raw = read_arrows(root, table)
    if raw is not None:
        out.append(raw.select(columns))
    return out


def column(t, c):
    col = t.column(c)
    return col.cast(pa.string()).to_pylist() if pa.types.is_dictionary(col.type) else col.to_pylist()


def rows(lake, table, columns, key):
    seen, out = set(), []
    for t in tables(lake, table, columns):
        for r in zip(*[column(t, c) for c in columns]):
            d = dict(zip(columns, r))
            k = key(d)
            if k not in seen:
                seen.add(k)
                out.append(d)
    return out


def epoch(ts):
    return ts.timestamp()


def utc(sec):
    return datetime.fromtimestamp(sec, timezone.utc)


def ffill(ts, val, t, max_age):
    i = np.searchsorted(ts, t, side="right") - 1
    ok = i >= 0
    j = np.where(ok, i, 0)
    ok &= (t - ts[j]) <= max_age
    return np.where(ok, val[j], np.nan)


def runs(mask):
    d = np.diff(np.concatenate([[0], mask.astype(np.int8), [0]]))
    return np.flatnonzero(d == 1), np.flatnonzero(d == -1)


def since_open(valve, pct):
    idx = np.arange(len(valve), dtype=float)
    last = np.maximum.accumulate(np.where(valve > pct, idx, -1.0))
    return np.where(np.isfinite(valve) & (last >= 0), idx - last, np.nan)


def lag(x, k):
    out = np.full(len(x), np.nan)
    out[k:] = x[:-k]
    return out


class Grid:
    def __init__(self, t, rooms, room, shared, cfg):
        self.t, self.rooms, self.room, self.shared, self.cfg = t, rooms, room, shared, cfg
        local = t + cfg["utc_offset_min"] * 60
        self.local_hour = (local % 86400) / 3600.0
        u, inv = np.unique(local // 86400, return_inverse=True)
        names = np.array([utc(int(x) * 86400).strftime("%Y-%m-%d") for x in u])
        self.day_of = names[inv]
        self.days = list(names)
        self.day_idx = {d: np.flatnonzero(self.day_of == d) for d in self.days}

    def minute(self, iso):
        return int((datetime.fromisoformat(iso).timestamp() - self.t[0]) // 60)

    def span(self, a, b):
        return slice(max(self.minute(a), 0), max(self.minute(b), 0) + 1)

    @classmethod
    def load(cls, lake, cfg):
        rooms = list(cfg["rooms"])
        node_room = {v: k for k, v in cfg["nodes"].items()}
        ar = rows(lake, "attribute_reports", ["ts", "node", "cluster", "attribute", "v_i64"], lambda d: (d["ts"], d["node"], d["cluster"], d["attribute"], d["v_i64"]))
        series = {}
        for d in ar:
            if d["cluster"] == 513 and d["attribute"] in ATTRS and d["v_i64"] is not None and d["node"] in node_room:
                scale = 1.0 if d["attribute"] == 8 else 0.01
                series.setdefault((node_room[d["node"]], ATTRS[d["attribute"]]), []).append((epoch(d["ts"]), d["v_i64"] * scale))
        arr = {k: np.array(sorted(v)).T for k, v in series.items()}
        t0 = np.ceil(min(arr[(r, "temp")][0][0] for r in rooms) / 60) * 60
        t1 = np.floor(max(arr[(r, "temp")][0][-1] for r in rooms) / 60) * 60
        t = np.arange(t0, t1 + 1, 60.0)
        val = cfg["valid"]
        room = {}
        for r in rooms:
            d = {}
            for name in ("temp", "valve", "setpoint"):
                lo, hi = val["valve" if name == "valve" else "temp"]
                if (r, name) in arr:
                    ts, v = arr[(r, name)]
                    ok = (v >= lo) & (v <= hi)
                    d[name] = ffill(ts[ok], v[ok], t, cfg["ffill_min"][name] * 60.0) if ok.any() else np.full(len(t), np.nan)
                else:
                    d[name] = np.full(len(t), np.nan)
            d["since_open"] = since_open(d["valve"], cfg["open_valve_pct"])
            room[r] = d
        shared = cls.weather(lake, t)
        ev = rows(lake, "events", ["ts", "kind", "node", "detail"], lambda d: (d["ts"], d["kind"], d["node"], d["detail"]))
        cls.mark_events(ev, t, rooms, room, shared, cfg)
        dec = rows(lake, "decisions", ["ts", "room", "rule", "setpoint_new", "applied"], lambda d: (d["ts"], d["room"], d["rule"], d["setpoint_new"], d["applied"]))
        cls.mark_decisions(dec, t, rooms, room, cfg)
        return cls(t, rooms, room, shared, cfg)

    @staticmethod
    def weather(lake, t):
        w = rows(lake, "weather", ["ts", "kind", "temp_c", "wind_ms", "radiation_wm2"], lambda d: (d["ts"], d["kind"]))
        w = sorted((epoch(d["ts"]), d) for d in w if d["kind"] == "obs")
        ts = np.array([x[0] for x in w])
        out = {}
        for name, col in {"out": "temp_c", "wind": "wind_ms", "rad": "radiation_wm2"}.items():
            v = np.array([np.nan if x[1][col] is None else x[1][col] for x in w], dtype=float)
            ok = np.isfinite(v)
            out[name] = np.interp(t, ts[ok], v[ok]) if ok.any() else np.full(len(t), np.nan)
        return out

    @staticmethod
    def mark_events(ev, t, rooms, room, shared, cfg):
        for r in rooms:
            for k in ("v1_open", "supply_event", "supply_onset"):
                room[r][k] = np.zeros(len(t), bool)
        opened, starts = {}, {}
        for e in sorted(ev, key=lambda d: d["ts"]):
            try:
                det = json.loads(e["detail"]) if e["detail"] else {}
            except ValueError:
                continue
            r, ts = det.get("room"), epoch(e["ts"])
            if r not in room:
                continue
            if e["kind"] == "window_open":
                opened[r] = ts
            elif e["kind"] == "window_close" and r in opened:
                room[r]["v1_open"] |= (t >= opened.pop(r)) & (t <= ts)
            elif e["kind"] == "supply" and det.get("state") == "start":
                starts[r] = (ts, min(det.get("since_ms", ts * 1000) / 1000.0, ts))
            elif e["kind"] == "supply" and det.get("state") == "end" and r in starts:
                s, o = starts.pop(r)
                room[r]["supply_event"] |= (t >= s) & (t <= ts)
                room[r]["supply_onset"] |= (t >= o) & (t <= ts)
        for r, s in opened.items():
            room[r]["v1_open"] |= (t >= s) & (t <= s + cfg["v1_open_cap_min"] * 60)
        for r, (s, o) in starts.items():
            room[r]["supply_event"] |= t >= s
            room[r]["supply_onset"] |= t >= o
        for k in ("supply_event", "supply_onset"):
            shared[k] = np.any([room[r][k] for r in rooms], axis=0)

    @staticmethod
    def mark_decisions(dec, t, rooms, room, cfg):
        by = {}
        for d in dec:
            if d["applied"] and d["room"] in room and d["setpoint_new"] is not None:
                by.setdefault(d["room"], []).append((epoch(d["ts"]), d["setpoint_new"]))
        cap = cfg["frost_cap_min"] * 60
        for r in rooms:
            room[r]["frost_write"] = np.zeros(len(t), bool)
            lst = sorted(by.get(r, []))
            for i, (ts, sp) in enumerate(lst):
                if sp <= cfg["frost_max_c"]:
                    nxt = lst[i + 1][0] if i + 1 < len(lst) else ts + cap
                    room[r]["frost_write"] |= (t >= ts) & (t < min(nxt, ts + cap))

    def summary(self):
        rm = self.rooms
        return {"minutes": len(self.t), "from": utc(self.t[0]).isoformat(), "to": utc(self.t[-1]).isoformat(), "days": {d: len(i) for d, i in self.day_idx.items()},
                "coverage": {r: {k: round(float(np.isfinite(self.room[r][k]).mean()), 3) for k in ("temp", "valve", "setpoint")} for r in rm},
                "supply_outage_min": int(self.shared["supply_event"].sum()), "supply_episodes": int(sum(len(runs(self.room[r]["supply_event"])[0]) for r in rm)),
                "v1_open_min": {r: int(self.room[r]["v1_open"].sum()) for r in rm}, "frost_write_min": {r: int(self.room[r]["frost_write"].sum()) for r in rm}}


class Approach:
    refit = False

    def __init__(self, cfg):
        self.cfg = cfg

    def fit(self, grid, train_days):
        return self

    def series(self, grid):
        raise NotImplementedError

    def score(self, grid, day):
        if getattr(self, "_key", None) != id(grid):
            self._key, self._series = id(grid), self.series(grid)
        i = grid.day_idx[day]
        return {k: v[i] for k, v in self._series.items()}


class V1Like(Approach):
    def series(self, grid):
        k = self.cfg["v1_like_min"]
        out = {r: np.maximum(lag(grid.room[r]["temp"], k) - grid.room[r]["temp"], 0.0) for r in grid.rooms}
        out["house"] = np.full(len(grid.t), np.nan)
        return out


class UnheatedFloor(Approach):
    def series(self, grid):
        c = self.cfg["floor"]
        w = c["window_min"]
        out = {}
        for r in grid.rooms:
            x = grid.room[r]["temp"]
            settled = np.where(grid.room[r]["since_open"] >= c["settled_min"], x, np.inf)
            fl = sliding_window_view(np.concatenate([np.full(w, np.inf), settled[:-1]]), w).min(axis=1)
            out[r] = np.maximum(np.where(np.isfinite(fl), fl, np.nan) - x, 0.0)
        ground = np.array([out[r] for r in c["ground"] if r in out])
        out["house"] = np.where(np.isnan(ground).all(axis=0), np.nan, np.nansum(ground, axis=0))
        return out


def episodes(m, thr, merge):
    a, b = runs(np.nan_to_num(m, nan=-np.inf) > thr)
    out = []
    for s, e in zip(a, b):
        if out and s - out[-1][1] < merge:
            out[-1][1] = e
        else:
            out.append([s, e])
    return out


def budget_threshold(m, excl, rate, merge, max_candidates):
    mm = np.where(excl, np.nan, m)
    fin = mm[np.isfinite(mm)]
    days = len(fin) / 1440.0
    u = np.unique(fin)
    if len(u) > max_candidates:
        u = np.unique(np.concatenate([np.quantile(fin, np.linspace(0, 1, max_candidates)), u[-max_candidates // 4:]]))
    best = float(u[-1]) if len(u) else 0.0
    for c in u[::-1]:
        if len(episodes(mm, c, merge)) / days > rate:
            break
        best = float(c)
    return best


class Evaluator:
    def __init__(self, grid, cfg):
        self.g, self.c = grid, cfg["criterion"]
        self.excl = np.zeros(len(grid.t), bool)
        self.excl[grid.span(*self.c["exclude_utc"])] = True
        self.hit = grid.span(*self.c["hit_utc"])

    def describe(self, scores, ep, thr):
        g = self.g
        names = list(scores)
        S = np.nan_to_num(np.vstack([scores[k] for k in names]), nan=-np.inf)
        out = []
        for s, e in ep:
            peak = S[:, s:e].max(axis=1)
            gt = self.hit.start <= s < self.hit.stop and any(n in self.c["gt_channels"] and p > thr for n, p in zip(names, peak))
            out.append({"start": int(s), "minutes": int(e - s), "start_utc": utc(g.t[s]).strftime("%m-%d %H:%M"), "start_local": (utc(g.t[s]) + timedelta(minutes=g.cfg["utc_offset_min"])).strftime("%m-%d %H:%M"),
                        "rooms": [n for n, p in zip(names, peak) if p > thr], "peak": float(peak.max()), "peak_channel": names[int(peak.argmax())], "in_supply": bool(g.shared["supply_event"][s]), "gt": bool(gt)})
        return out

    @staticmethod
    def rank(eps):
        gt = [e for e in eps if e["gt"]]
        if not gt:
            return None, None
        best = max(gt, key=lambda e: e["peak"])
        return best, 1 + sum(e["peak"] > best["peak"] for e in eps)

    def run(self, scores):
        c = self.c
        S = np.vstack(list(scores.values()))
        m = np.where(np.isfinite(S).any(axis=0), np.nanmax(np.where(np.isfinite(S), S, -np.inf), axis=0), np.nan)
        thr = budget_threshold(m, self.excl, c["rate"], c["merge_min"], c["max_candidates"])
        rep = budget_threshold(m, self.excl, c["report_rate"], c["merge_min"], c["max_candidates"])
        eps = self.describe(scores, episodes(m, thr, c["merge_min"]), thr)
        wide = self.describe(scores, episodes(m, rep, c["merge_min"]), rep)
        best, rank = self.rank(eps)
        _, wide_rank = self.rank(wide)
        days = float(np.isfinite(np.where(self.excl, np.nan, m)).sum()) / 1440.0
        n_supply = sum(e["in_supply"] for e in eps)
        return {"pass": bool(best is not None and n_supply == 0 and rank <= c["top_k"]), "detected": best is not None, "latency_min": None if best is None else best["start"] - self.hit.start,
                "threshold": thr, "report_threshold": rep, "alarms": len(eps), "alarms_per_day": round(sum(not e["gt"] for e in eps) / days, 3) if days else None,
                "gt_rank": rank, "gt_rank_wide": wide_rank, "alarms_in_supply": n_supply, "episodes": eps, "top": sorted(wide, key=lambda e: -e["peak"])[:c["list_n"]]}


def loo_scores(grid, cls, cfg):
    ap = cls(cfg)
    if not ap.refit:
        ap.fit(grid, grid.days)
    out = {}
    for d in grid.days:
        if ap.refit:
            ap = cls(cfg).fit(grid, [x for x in grid.days if x != d])
        for k, v in ap.score(grid, d).items():
            out.setdefault(k, np.full(len(grid.t), np.nan))[grid.day_idx[d]] = v
    return out


def evaluate(grid, cls, cfg):
    return Evaluator(grid, cfg).run(loo_scores(grid, cls, cfg))


def resolve(name):
    if ":" in name:
        mod, cls = name.split(":")
        return getattr(importlib.import_module(mod), cls)
    return globals()[name]


def format_report(name, r):
    lines = [f"{name}: pass={r['pass']} detected={r['detected']} latency_min={r['latency_min']} threshold={r['threshold']:.3f} alarms={r['alarms']} per_day_unlabelled={r['alarms_per_day']} gt_rank={r['gt_rank']} (wide {r['gt_rank_wide']}) in_supply={r['alarms_in_supply']}"]
    for i, e in enumerate(r["top"], 1):
        lines.append(f"  {i:2d} {e['start_local']} local {e['minutes']:4d}min peak={e['peak']:.2f} {e['peak_channel']} rooms={','.join(e['rooms'])}{' supply' if e['in_supply'] else ''}{' GT' if e['gt'] else ''}")
    return "\n".join(lines)


def run_windowml(config, name, lake=None):
    cfg = config["window"]
    grid = Grid.load(lake or cfg["lake"], cfg)
    r = evaluate(grid, resolve(name), cfg)
    print(format_report(name, r))
    return r
