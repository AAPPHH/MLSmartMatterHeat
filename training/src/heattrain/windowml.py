import copy
import importlib
import json
from datetime import datetime, timedelta, timezone
from pathlib import Path
import numpy as np
from scipy.signal import lfilter
from sklearn.linear_model import Ridge
from xgboost import XGBRegressor
import pyarrow as pa
import pyarrow.parquet as pq
import pyarrow.ipc as ipc
from numpy.lib.stride_tricks import sliding_window_view
from heattrain.thermal import read_arrows

ATTRS = {0: "temp", 8: "valve", 18: "setpoint"}


def fill(t, columns):
    return pa.table({c: t.column(c) if c in t.column_names else pa.nulls(t.num_rows) for c in columns})


def raw_tables(root, table):
    out = []
    for f in sorted((root / "raw" / table).glob("*.arrows")):
        try:
            with open(f, "rb") as fh:
                out.append(ipc.open_stream(fh).read_all())
        except (pa.ArrowInvalid, OSError):
            continue
    return out


def tables(lake, table, columns):
    root = Path(lake)
    out = [fill(pq.read_table(f), columns) for f in sorted((root / "parquet" / table).glob("**/part.parquet"))]
    out += [fill(t, columns) for t in raw_tables(root, table)]
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
        self.cache = {}
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
        shared = cls.weather(lake, t, cfg["weather_max_age_min"])
        ev = rows(lake, "events", ["ts", "kind", "node", "detail"], lambda d: (d["ts"], d["kind"], d["node"], d["detail"]))
        cls.mark_events(ev, t, rooms, room, shared, cfg)
        dec = rows(lake, "decisions", ["ts", "room", "rule", "setpoint_new", "applied"], lambda d: (d["ts"], d["room"], d["rule"], d["setpoint_new"], d["applied"]))
        cls.mark_decisions(dec, t, rooms, room, cfg)
        cls.presence(lake, t, rooms, room, shared, cfg)
        return cls(t, rooms, room, shared, cfg)

    @staticmethod
    def ages(ts, t, cap):
        if len(ts) == 0:
            return np.full(len(t), float(cap))
        i = np.searchsorted(ts, t, side="right") - 1
        return np.where(i >= 0, np.minimum((t - ts[np.maximum(i, 0)]) / 60.0, cap), float(cap))

    @classmethod
    def presence(cls, lake, t, rooms, room, shared, cfg):
        c = cfg["presence"]
        cap = c["age_cap_min"]
        wi = rows(lake, "watch_inference", ["ts", "result_room", "abstained", "state"], lambda d: (d["ts"], d["result_room"], d["state"]))
        lb = rows(lake, "labels", ["ts", "room", "source"], lambda d: (d["ts"], d["room"], d["source"]))
        ev = [(epoch(d["ts"]), d["result_room"], d["state"]) for d in wi if not d["abstained"] and d["result_room"]]
        ev += [(epoch(d["ts"]), d["room"], None) for d in lb if d["source"] == "watch" and d["room"]]
        ev.sort(key=lambda e: e[0])
        pick = lambda f: np.array([e[0] for e in ev if f(e)], float)
        for r in rooms:
            room[r]["in_room_age"] = cls.ages(pick(lambda e: e[1] == r), t, cap)
        any_room = cls.ages(pick(lambda e: e[1] != "keiner"), t, cap)
        away = cls.ages(pick(lambda e: e[1] == "keiner"), t, cap)
        sleep = cls.ages(pick(lambda e: e[1] == c["sleep_room"] and e[2] == "rest"), t, cap)
        other = cls.ages(pick(lambda e: not (e[1] == c["sleep_room"] and e[2] == "rest")), t, cap)
        shared["home_age"] = np.where(away < any_room, float(cap), any_room)
        shared["asleep"] = ((sleep <= c["asleep_age_min"]) & (sleep <= other)).astype(float)

    @staticmethod
    def weather(lake, t, max_age_min):
        w = rows(lake, "weather", ["ts", "kind", "temp_c", "wind_ms", "radiation_wm2"], lambda d: (d["ts"], d["kind"]))
        w = sorted((epoch(d["ts"]), d) for d in w if d["kind"] == "obs")
        ts = np.array([x[0] for x in w])
        out = {}
        for name, col in {"out": "temp_c", "wind": "wind_ms", "rad": "radiation_wm2"}.items():
            v = np.array([np.nan if x[1][col] is None else x[1][col] for x in w], dtype=float)
            ok = np.isfinite(v)
            out[name] = ffill(ts[ok], v[ok], t, max_age_min * 60.0) if ok.any() else np.full(len(t), np.nan)
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


FEATURE_NAMES = ["valve_mean_h", "valve_f4_lag", "valve_f4", "valve_f12_lag", "valve_f12", "valve_f40_lag", "valve_f40", "valve_lag", "valve", "setpoint_lag_minus_s0", "out_minus_s0", "out_lag_minus_s0", "rad_100", "wind_times_out_minus_s0", "since_open_lag", "since_open"]


def dilate(mask, pad):
    out = mask.copy()
    for s, e in zip(*runs(mask)):
        out[e:e + pad] = True
    return out


class PhysicsResidual(Approach):
    refit = True

    def __init__(self, cfg):
        super().__init__(cfg)
        self.c = cfg["phys"]
        self.model, self.rows = {}, {}

    def features(self, grid, r):
        c, d = self.c, grid.room[r]
        H, n = c["horizon"], len(grid.t)
        s, v, sp = d["temp"], d["valve"] / 100.0, d["setpoint"]
        out, rad, wind = grid.shared["out"], grid.shared["rad"], grid.shared["wind"]
        good = np.isfinite(v)
        vz = np.where(good, v, 0.0)
        cs = np.concatenate([[0.0], np.cumsum(vz)])
        win = np.full(n, np.nan)
        win[H:] = (cs[H + 1:] - cs[1:n - H + 1]) / H
        f = [win]
        for tau in c["taus"]:
            a = np.exp(-1.0 / tau)
            st = lfilter([1 - a], [1, -a], vz)
            f += [lag(st, H), st]
        s0 = lag(s, H)
        so = np.where(good & np.isnan(d["since_open"]), c["since_open_cap"], d["since_open"])
        cap = c["since_open_cap"]
        f += [lag(v, H), v, lag(sp, H) - s0, out - s0, lag(out, H) - s0, rad / 100.0, (out - s0) * wind, np.minimum(lag(so, H), cap) / cap, np.minimum(so, cap) / cap]
        X = np.column_stack(f)
        y = s - s0
        idx = np.arange(n)
        run = idx - np.maximum.accumulate(np.where(good, -1, idx))
        ok = np.isfinite(X).all(axis=1) & np.isfinite(y) & (run >= H + c["valve_guard_min"] + 1)
        return X, y, ok

    def fit(self, grid, train_days):
        c = self.c
        tr = np.zeros(len(grid.t), bool)
        for dd in train_days:
            tr[grid.day_idx[dd]] = True
        for r in grid.rooms:
            X, y, ok = self.features(grid, r)
            m = ok & tr
            if m.sum() < c["min_rows"]:
                continue
            Xt, yt = X[m], y[m]
            keep = np.ones(len(yt), bool)
            for _ in range(2):
                mdl = Ridge(alpha=c["ridge"]).fit(Xt[keep], yt[keep])
                res = yt - mdl.predict(Xt)
                keep = np.abs(res) <= np.quantile(np.abs(res), 1 - c["trim"])
            sig = float(np.sqrt(np.mean(res[keep] ** 2)))
            self.rows[r] = int(m.sum())
            self.model[r] = (np.array(mdl.coef_, float), float(mdl.intercept_), max(sig, c["sigma_floor"]))
        return self

    def evidence(self, grid):
        c = self.c
        z = {}
        for r in grid.rooms:
            if r not in self.model:
                z[r] = np.full(len(grid.t), np.nan)
                continue
            coef, icpt, sig = self.model[r]
            X, y, ok = self.features(grid, r)
            e = np.full(len(y), np.nan)
            e[ok] = -(y[ok] - (X[ok] @ coef + icpt)) / sig
            z[r] = np.minimum(e, c["clip"])
        return z

    def house(self, z, grid):
        w = self.c["weights"]
        zs = np.array([np.maximum(z[r], 0.0) * w[r] for r in grid.rooms])
        return np.where(np.isfinite(zs).any(axis=0), np.nansum(zs, axis=0), np.nan)

    def series(self, grid):
        z = self.evidence(grid)
        z["house"] = self.house(z, grid)
        veto = dilate(np.nan_to_num(grid.shared["supply_event"]).astype(bool), self.c["veto_pad_min"])
        return {k: np.where(veto, np.nan, v) for k, v in z.items()}

    def score(self, grid, day):
        s = self.series(grid)
        i = grid.day_idx[day]
        return {k: v[i] for k, v in s.items()}


class StreamScorer:
    def __init__(self, bundle):
        self.b = bundle
        self.rooms = list(bundle["rooms"])
        H = bundle["horizon_min"]
        self.hist = {r: [] for r in self.rooms}
        self.st = {r: [0.0] * len(bundle["taus"]) for r in self.rooms}
        self.run = dict.fromkeys(self.rooms, 0)
        self.last_open = dict.fromkeys(self.rooms)
        self.win = {r: [] for r in self.rooms}
        self.t = -1
        self.since_supply = None
        self.H = H

    def step(self, shared, inputs):
        b, H = self.b, self.H
        self.t += 1
        sup = bool(shared["supply"])
        self.since_supply = 0 if sup else (None if self.since_supply is None else self.since_supply + 1)
        veto = sup or (self.since_supply is not None and self.since_supply <= b["veto_pad_min"])
        z = {}
        for r in self.rooms:
            temp, valve, sp = inputs[r]
            good = np.isfinite(valve)
            v = valve / 100.0 if good else np.nan
            vz = v if good else 0.0
            self.run[r] = self.run[r] + 1 if good else 0
            if good and valve > b["open_valve_pct"]:
                self.last_open[r] = self.t
            so = b["since_open_cap"] if self.last_open[r] is None else min(self.t - self.last_open[r], b["since_open_cap"])
            self.st[r] = [np.exp(-1.0 / tau) * x + (1 - np.exp(-1.0 / tau)) * vz for tau, x in zip(b["taus"], self.st[r])]
            self.win[r].append(vz)
            h = self.hist[r]
            h.append({"temp": temp, "v": v, "sp": sp, "out": shared["out"], "so": so, "st": list(self.st[r])})
            z[r] = np.nan
            if len(h) <= H:
                continue
            old = h[-1 - H]
            del h[0]
            wm = sum(self.win[r][-H:]) / H
            del self.win[r][:-H]
            m = b["rooms"][r]
            s0 = old["temp"]
            f = [wm]
            for a, c in zip(old["st"], self.st[r]):
                f += [a, c]
            cap = b["since_open_cap"]
            f += [old["v"], v, old["sp"] - s0, shared["out"] - s0, old["out"] - s0, shared["rad"] / 100.0, (shared["out"] - s0) * shared["wind"], old["so"] / cap, so / cap]
            x = np.array(f, float)
            if self.run[r] < H + b["valve_guard_min"] + 1 or not np.isfinite(x).all() or not np.isfinite(temp):
                continue
            z[r] = min(-((temp - s0) - (float(np.dot(x, m["coef"])) + m["intercept"])) / m["sigma"], b["clip"])
        zs = [max(z[r], 0.0) * b["rooms"][r]["weight"] for r in self.rooms if np.isfinite(z[r])]
        house = sum(zs) if zs else np.nan
        return z, house, veto


def window_bundle(config, lake, models_dir, version=None):
    cfg = config["window"]
    grid = Grid.load(lake, cfg)
    oof = loo_scores(grid, PhysicsResidual, cfg)
    res = Evaluator(grid, cfg).run({"house": oof["house"]})
    ap = PhysicsResidual(cfg).fit(grid, grid.days)
    c = ap.c
    ground = [r for r in cfg["floor"]["ground"] if r in ap.model]
    if not res["pass"] or len(ground) < 2:
        raise SystemExit(f"window bundle not validated: pass={res['pass']} detected={res['detected']} alarms_in_supply={res['alarms_in_supply']} ground_rooms={ground}")
    version = version or datetime.now(timezone.utc).strftime("%Y%m%dT%H%M%SZ")
    bundle = {"version": version, "kind": "window_physics_residual", "horizon_min": c["horizon"], "taus": c["taus"], "feature_names": FEATURE_NAMES, "since_open_cap": c["since_open_cap"], "open_valve_pct": cfg["open_valve_pct"],
              "valve_guard_min": c["valve_guard_min"], "clip": c["clip"], "veto_pad_min": c["veto_pad_min"], "threshold": res["threshold"], "merge_min": cfg["criterion"]["merge_min"],
              "ffill_min": cfg["ffill_min"], "valid": cfg["valid"], "weather_max_age_min": cfg["weather_max_age_min"],
              "rooms": {r: {"node": cfg["nodes"][r], "weight": c["weights"][r], "coef": coef.tolist(), "intercept": icpt, "sigma": sig} for r, (coef, icpt, sig) in ap.model.items()}}
    meta = {"version": version, "created": datetime.now(timezone.utc).isoformat(timespec="seconds"), "train_from": utc(grid.t[0]).isoformat(), "train_to": utc(grid.t[-1]).isoformat(), "days": len(grid.days), "minutes": len(grid.t), "rows": ap.rows,
            "eval": {k: res[k] for k in ("pass", "detected", "latency_min", "threshold", "alarms", "alarms_per_day", "gt_rank", "alarms_in_supply")}, "alarms": [{k: e[k] for k in ("start_utc", "minutes", "peak", "rooms", "gt")} for e in res["episodes"]]}
    out = Path(models_dir) / "window" / version
    out.mkdir(parents=True, exist_ok=True)
    (out / "bundle.json").write_text(json.dumps(bundle, indent=1))
    (out / "meta.json").write_text(json.dumps(meta, indent=1))
    return out, bundle, meta, ap, grid


def slice_grid(grid, sl):
    room = {r: {k: v[sl] for k, v in d.items() if k != "since_open"} for r, d in grid.room.items()}
    for d in room.values():
        d["since_open"] = since_open(d["valve"], grid.cfg["open_valve_pct"])
    shared = {k: v[sl] for k, v in grid.shared.items()}
    return Grid(grid.t[sl], grid.rooms, room, shared, grid.cfg)


def stream_run(bundle, grid, sl):
    sc = StreamScorer(bundle)
    sub = slice_grid(grid, sl)
    rec = []
    for i in range(len(sub.t)):
        shared = {"supply": bool(sub.shared["supply_event"][i]), "out": sub.shared["out"][i], "wind": sub.shared["wind"][i], "rad": sub.shared["rad"][i]}
        z, house, veto = sc.step(shared, {r: (sub.room[r]["temp"][i], sub.room[r]["valve"][i], sub.room[r]["setpoint"][i]) for r in sc.rooms})
        rec.append((shared, z, house, veto))
    return sub, rec


def parity_csv(bundle, grid, sl):
    sub, rec = stream_run(bundle, grid, sl)
    rooms = list(bundle["rooms"])
    f = lambda x: "" if not np.isfinite(x) else repr(float(x))
    head = ["ts", "supply", "out", "wind", "rad"] + [f"{r}_{k}" for r in rooms for k in ("temp", "valve", "setpoint")] + [f"z_{r}" for r in rooms] + ["house", "veto"]
    lines = [",".join(head)]
    for i, (shared, z, house, veto) in enumerate(rec):
        cells = [str(int(sub.t[i])), str(int(shared["supply"])), f(shared["out"]), f(shared["wind"]), f(shared["rad"])]
        cells += [f(sub.room[r][k][i]) for r in rooms for k in ("temp", "valve", "setpoint")] + [f(z[r]) for r in rooms] + [f(house), str(int(veto))]
        lines.append(",".join(cells))
    return "\n".join(lines) + "\n"
