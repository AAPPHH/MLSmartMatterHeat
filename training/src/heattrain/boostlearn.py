import json
from datetime import datetime, timedelta, timezone
from pathlib import Path
import numpy as np
import pyarrow.parquet as pq
from scipy.optimize import least_squares
from scipy.stats import norm
from scipy.signal import lfilter
from numpy.lib.stride_tricks import sliding_window_view
from heattrain.thermal import read_arrows
from heattrain.windowml import Grid, runs, utc

FEATURES = ["out", "wind", "rad", "start_temp", "gap", "supply_ok", "since_resume", "vm60", "vm180", "since_open", "hour_sin", "hour_cos"]


def kernels(m, length, shape):
    k = np.arange(1, length + 1, dtype=float)
    theta = np.asarray(m, float)[:, None] / shape
    h = k ** (shape - 1) * np.exp(-k / theta)
    return h / h.sum(axis=1, keepdims=True)


def hold_closed(valve, max_min, closed_pct):
    out = valve.copy()
    a, b = runs(~np.isfinite(valve))
    for x, y in zip(a, b):
        if 0 < x and y - x <= max_min and valve[x - 1] <= closed_pct:
            out[x:y] = valve[x - 1]
    return out


def ramp(minutes, tau):
    return 1.0 - np.exp(-np.asarray(minutes, float) / tau)


def lagmatrix(u_ext, horizon, length):
    return sliding_window_view(u_ext, length)[:horizon, ::-1]


def response(theta, U, ref, a, t0, length, shape):
    theta = np.atleast_2d(theta)
    h = kernels(np.exp(theta[:, 1]), length, shape)
    heat = np.exp(theta[:, 0])[:, None] * (h @ U.T)
    x = np.zeros_like(heat)
    x[:, 1:] = a * ref[:-1] + heat[:, 1:]
    return lfilter([1.0], [1.0, -(1.0 - a)], x, axis=1) + t0 * (1.0 - a) ** np.arange(U.shape[0])


def rollout(theta, a, delta, t0, tout, u_ext, length, shape):
    horizon = len(u_ext) - length
    ref = np.broadcast_to(np.asarray(tout, float), (horizon,)) + delta
    return response(theta, lagmatrix(np.asarray(u_ext, float), horizon, length), ref, a, t0, length, shape)


def autocorr_factor(res_list):
    num = sum(float(np.sum(r[1:] * r[:-1])) for r in res_list if len(r) > 2)
    den = sum(float(np.sum(r * r)) for r in res_list if len(r) > 2)
    rho = np.clip(num / den, 0.0, 0.97) if den > 0 else 0.0
    return float(np.sqrt((1 + rho) / (1 - rho)))


class Series:
    def __init__(self, grid, room, cfg):
        c = cfg
        d = grid.room[room]
        self.room, self.cfg, self.t = room, cfg, grid.t
        self.temp, self.setpoint = d["temp"], d["setpoint"]
        valve = hold_closed(d["valve"], c["closed_hold_min"], 100.0 * c["dead_zone"])
        self.valid_valve = np.isfinite(valve)
        self.outage = d["supply_event"]
        raw = np.where(self.outage | ~self.valid_valve, 0.0, np.nan_to_num(valve) / 100.0)
        self.u = np.clip((raw - c["dead_zone"]) / (1.0 - c["dead_zone"]), 0.0, 1.0)
        self.d = d
        n = len(grid.t)
        sp = self.setpoint
        drop = np.zeros(n, bool)
        dsp = np.nan_to_num(np.diff(sp, prepend=sp[0]))
        for i in np.flatnonzero(dsp <= -c["setpoint_drop_k"]):
            drop[i:i + c["cooldown_min"]] = True
        self.drop = drop
        self.bad = d["v1_open"] | d["frost_write"] | drop
        out = grid.shared["out"]
        self.out = np.where(np.isfinite(out), out, np.nanmedian(out) if np.isfinite(out).any() else c["out_default"])
        self.wind = np.nan_to_num(grid.shared["wind"], nan=c["wind_default"])
        self.rad = np.nan_to_num(grid.shared["rad"], nan=0.0)
        self.hour = grid.local_hour
        self.resume = self.since_resume()
        self.u_cmd = self.u
        self.u = self.u * ramp(self.resume, c["ramp_tau_min"])
        self.scenario = self.scenarios(dsp, sp)

    def since_resume(self):
        o = self.outage
        idx = np.arange(len(o), dtype=float)
        last = np.maximum.accumulate(np.where(np.concatenate([[False], o[:-1] & ~o[1:]]), idx, -1.0))
        cap = self.cfg["resume_cap_min"]
        out = np.where(last >= 0, np.minimum(idx - last, cap), cap)
        return np.where(o, 0.0, out)

    def scenarios(self, dsp, sp):
        c = self.cfg
        out = np.full(len(dsp), "other", dtype=object)
        prev = np.roll(sp, 1)
        for i in np.flatnonzero(dsp >= c["morning_jump_k"]):
            h = self.hour[i]
            kind = "morning" if c["morning_hours"][0] <= h < c["morning_hours"][1] else ("homecoming" if h >= c["home_hours"][0] and h < c["home_hours"][1] and dsp[i] >= c["home_jump_k"] and prev[i] <= c["away_max_c"] else None)
            if kind:
                seg = out[max(i - c["scenario_pre_min"], 0):i + c["scenario_post_min"]]
                seg[seg == "other"] = kind
        return out

    def merged_open(self):
        c = self.cfg
        m = self.u > 0.0
        a, b = runs(~m)
        for x, y in zip(a, b):
            if y - x <= c["merge_gap_min"] and x > 0 and y < len(m):
                m[x:y] = True
        return runs(m)

    def vector(self, s, setpoint=None):
        c = self.cfg
        sp = self.setpoint[s] if setpoint is None else setpoint
        t0 = self.temp[s]
        return {"out": float(self.out[s]), "wind": float(self.wind[s]), "rad": float(self.rad[s]), "start_temp": float(t0), "gap": float(sp - t0), "supply_ok": float(not self.outage[s]),
                "since_resume": float(self.resume[s]), "vm60": float(self.u[max(s - 60, 0):s].sum()), "vm180": float(self.u[max(s - 180, 0):s].sum()), "hour": float(self.hour[s])}


def closure_points(s, cycles, cfg):
    c = cfg
    gap = s.setpoint - s.temp
    ok = np.isfinite(gap) & ~s.bad & ~s.outage & s.valid_valve & (s.resume >= c["resume_cap_min"])
    for cy in cycles:
        ok[cy["start_idx"]:cy["start_idx"] + c["closure_excl_min"]] = False
    ok &= (gap > -c["closure_gap"][0]) & (gap < c["closure_gap"][1])
    return gap[ok], s.u_cmd[ok]


def fit_closure(g, u, cfg):
    c = cfg
    kp = np.geomspace(*c["closure_kp"], 40)
    best = (np.inf, None, None)
    for g0 in np.linspace(0.0, c["closure_gap0_max"], 31):
        err = ((np.clip(kp[:, None] * (g[None, :] - g0), 0.0, 1.0) - u[None, :]) ** 2).mean(axis=1)
        i = int(np.argmin(err))
        best = min(best, (float(err[i]), float(kp[i]), float(g0)))
    return {"kp": round(best[1], 5), "gap0": round(best[2], 4), "n": int(len(g)), "rmse": round(float(np.sqrt(best[0])), 4)}


def closed_loop(theta, a, delta, t0, tout, hist, doses, setpoint, pi, ramp_factor, length, shape):
    th = np.atleast_2d(theta)
    horizon = len(ramp_factor)
    doses = np.asarray(doses, float)
    h = kernels(np.exp(th[:, 1]), length, shape)[:, ::-1]
    g = np.exp(th[:, 0])
    ring = np.zeros((len(doses), len(th), length + horizon))
    ring[:, :, :length] = hist
    ref = np.broadcast_to(np.asarray(tout, float), (horizon,)) + delta
    boost = np.clip(doses[:, None] - np.arange(horizon)[None, :], 0.0, 1.0)
    cur = np.full((len(doses), len(th)), float(t0))
    peak = cur.copy()
    for t in range(horizon):
        if t:
            cur = (1.0 - a) * cur + a * ref[t - 1] + g * np.einsum("dsl,sl->ds", ring[:, :, t:t + length], h)
            peak = np.maximum(peak, cur)
        thermostat = np.clip(pi["kp"] * (setpoint - cur - pi["gap0"]), 0.0, 1.0)
        ring[:, :, length + t] = np.where(boost[:, t, None] > 0, boost[:, t, None], thermostat) * ramp_factor[t]
    return peak


def feature_row(ctx):
    h = 2 * np.pi * ctx["hour"] / 24.0
    return [ctx["out"], ctx["wind"], ctx["rad"], ctx["start_temp"], ctx["gap"], ctx["supply_ok"], ctx["since_resume"], ctx["vm60"], ctx["vm180"], ctx["since_open"], float(np.sin(h)), float(np.cos(h))]


class Episodes:
    def __init__(self, grid, cfg, boost_cycles):
        self.cfg, self.rooms = cfg, grid.rooms
        self.series = {r: Series(grid, r, cfg) for r in grid.rooms}
        self.pulses, self.windows, self.cycles, self.pas = [], {r: [] for r in grid.rooms}, boost_cycles, {}
        for r in grid.rooms:
            self.extract(r, boost_cycles.get(r, []))
        pts = {r: closure_points(self.series[r], boost_cycles.get(r, []), cfg) for r in grid.rooms}
        pooled = fit_closure(np.concatenate([p[0] for p in pts.values()]), np.concatenate([p[1] for p in pts.values()]), cfg) if sum(len(p[0]) for p in pts.values()) >= cfg["closure_min_points"] else dict(cfg["closure_default"], n=0, rmse=None)
        self.closure = {r: fit_closure(*pts[r], cfg) if len(pts[r][0]) >= cfg["closure_min_points"] and (pts[r][0] > cfg["closure_active_gap"]).sum() >= cfg["closure_min_active"] else dict(pooled, pooled=True) for r in grid.rooms}

    def extract(self, room, cycles):
        c, s = self.cfg, self.series[room]
        L, n = c["kernel_len_min"], len(s.t)
        a, b = s.merged_open()
        prev_end = -10 ** 9
        spans = []
        for x, y in zip(a, b):
            ok = x >= 1 and np.isfinite(s.temp[x]) and np.isfinite(s.setpoint[x]) and not s.bad[x]
            dose = float(s.u[x:min(y, x + c["pulse_cap_min"])].sum())
            measured = dose
            boost = next((cy for cy in cycles if abs(cy["start_idx"] - x) <= c["boost_match_min"]), None)
            if ok and dose >= c["min_dose"]:
                ctx = s.vector(x, (boost or {}).get("target"))
                ctx["since_open"] = float(min(x - prev_end, c["since_open_cap_min"]))
                if boost and boost["dose"] is not None:
                    dose = min(float(boost["dose"]), measured)
                p = {"room": room, "start": int(x), "end": int(y), "dose": dose, "ctx": ctx, "scenario": (boost or {}).get("scenario") or str(s.scenario[x]), "boost": boost is not None, "overshoot_c": (boost or {}).get("overshoot_c")}
                self.pulses.append(p)
                spans.append(p)
            prev_end = y
        mask = np.zeros(n, bool)
        for p in spans:
            mask[max(p["start"] - c["pre_min"], L):min(min(p["end"], p["start"] + c["pulse_cap_min"]) + c["post_min"], n - 1) + 1] = True
        cum = np.concatenate([[0], np.cumsum(~s.valid_valve)])
        idx = np.arange(n)
        hist_ok = (idx >= L) & ((cum[idx + 1] - cum[np.maximum(idx - L, 0)]) == 0)
        nan = ~np.isfinite(s.temp)
        a0, b0 = runs(nan)
        long_gap = np.zeros(n, bool)
        for x, y in zip(a0, b0):
            if y - x > c["max_temp_gap_min"]:
                long_gap[x:y] = True
        good = mask & ~s.bad & hist_ok & ~long_gap
        for x, y in zip(*runs(good)):
            fin = np.flatnonzero(np.isfinite(s.temp[x:y]))
            if not len(fin):
                continue
            x += fin[0]
            if y - x < c["min_window_min"] or np.isfinite(s.temp[x:y]).mean() < c["min_temp_frac"]:
                continue
            w = {"ws": int(x), "we": int(y - 1), "pulses": [], "boost": False, "y": s.temp[x:y], "t0": float(np.nanmedian(s.temp[x:y][:3])), "tout": s.out[x:y], "u_ext": s.u[x - L:y]}
            w["U"] = lagmatrix(w["u_ext"], y - x, L)
            for p in spans:
                if x <= p["start"] < y - c["min_pulse_tail_min"]:
                    p.update(clean=True, win=w, hi=min(p["end"] + c["post_min"], y - 1))
                    seg = s.temp[p["start"]:p["hi"] + 1]
                    p["rise"] = float(np.nanmax(seg) - s.temp[p["start"]]) if np.isfinite(seg).any() else np.nan
                    w["pulses"].append(p)
                    w["boost"] |= p["boost"]
            if w["pulses"]:
                self.windows[room].append(w)
        for p in spans:
            p.setdefault("clean", False)
            p["known_ok"] = self.did_not_overshoot(p)

    def did_not_overshoot(self, p):
        c = self.cfg
        if p["scenario"] == "manual" or not p["clean"] or not np.isfinite(p["rise"]) or p["hi"] - p["end"] < c["known_post_min"]:
            return False
        if p["boost"] and (p["overshoot_c"] is None or p["overshoot_c"] > c["known_overshoot_k"]):
            return False
        return p["rise"] - p["ctx"]["gap"] <= c["known_overshoot_k"]

    def passive(self, room):
        if room not in self.pas:
            self.pas[room] = self.fit_passive(room)
        return self.pas[room]

    def fit_passive(self, room):
        c, s = self.cfg, self.series[room]
        step, L = c["passive_step_min"], c["kernel_len_min"]
        cum = np.concatenate([[0.0], np.cumsum(s.u)])
        n = len(s.t)
        i = np.arange(L, n - step)
        quiet = (cum[i + step] - cum[i - L]) == 0
        ok = quiet & np.isfinite(s.temp[i]) & np.isfinite(s.temp[i + step]) & ~s.bad[i] & ~s.bad[i + step]
        ok &= s.valid_valve[i - L] & ~s.outage[i]
        i = i[ok]
        if len(i) < c["passive_min_points"]:
            return c["passive_default"]["a"], c["passive_default"]["delta"], 0
        dT = s.temp[i + step] - s.temp[i]
        x = s.out[i] - s.temp[i]
        A = np.stack([x, np.ones(len(i))], axis=1)
        (beta, alpha), *_ = np.linalg.lstsq(A, dT, rcond=None)
        beta = float(np.clip(beta, c["passive_a_bounds"][0] * step, c["passive_a_bounds"][1] * step))
        return beta / step, float(np.clip(alpha / beta, *c["passive_delta_bounds"])), int(len(i))


class Posterior:
    def __init__(self, cfg):
        self.cfg = cfg
        self.prior_mu = np.array([np.log(cfg["gain_prior"][0]), np.log(cfg["delay_prior"][0])])
        self.prior_sd = np.array([cfg["gain_prior"][1], cfg["delay_prior"][1]])
        self.mu, self.tau = self.prior_mu.copy(), self.prior_sd.copy()
        self.lo = np.array([np.log(cfg["gain_bounds"][0]), np.log(cfg["delay_bounds"][0])])
        self.hi = np.array([np.log(cfg["gain_bounds"][1]), np.log(cfg["delay_bounds"][1])])
        self.room = {}

    def resid_fn(self, wins, a, delta, sigma, mu, tau):
        c, lo, hi = self.cfg, self.lo, self.hi

        def parts(x):
            out = []
            for w in wins:
                T = response(x, w["U"], w["tout"] + delta, a, w["t0"], c["kernel_len_min"], c["kernel_shape"])[0]
                r = (w["y"] - T) * (c["boost_weight"] if w["boost"] else 1.0)
                out.append(r[np.isfinite(r)])
            return out

        def f(x):
            return np.concatenate([np.concatenate(parts(x)) / sigma if wins else np.zeros(0), (x - mu) / tau])
        return f, parts, lo, hi

    def solve(self, wins, a, delta, sigma, mu, tau):
        f, parts, lo, hi = self.resid_fn(wins, a, delta, sigma, mu, tau)
        x0 = np.clip(mu, lo + 1e-6, hi - 1e-6)
        sol = least_squares(f, x0, bounds=(lo, hi), x_scale=1.0, loss="soft_l1", f_scale=self.cfg["loss_scale"])
        cov = np.linalg.inv(sol.jac.T @ sol.jac + 1e-9 * np.eye(2))
        return sol.x, cov, parts(sol.x)

    def fit_room(self, wins, a, delta, mu, tau):
        c = self.cfg
        sigma = c["sigma0"]
        for _ in range(2):
            x, cov, res = self.solve(wins, a, delta, sigma, mu, tau) if wins else (mu.copy(), np.diag(tau ** 2), [])
            if not wins:
                break
            flat = np.concatenate(res)
            mad = 1.4826 * float(np.median(np.abs(flat - np.median(flat))))
            lim = 3 * max(c["sigma_floor"], mad)
            sigma = max(c["sigma_floor"], mad) * min(autocorr_factor([np.clip(r, -lim, lim) for r in res]), c["kappa_max"])
        return {"theta": x, "cov": cov, "sigma": float(sigma), "a": a, "delta": delta, "n_win": len(wins), "n_pulse": int(sum(len(w["pulses"]) for w in wins))}

    def fit(self, eps, drop=None):
        c = self.cfg
        pas = {r: eps.passive(r) for r in eps.rooms}
        wins = {r: [w for w in eps.windows[r] if w is not drop] for r in eps.rooms}
        for _ in range(c["eb_iter"]):
            fits = {r: self.fit_room(wins[r], pas[r][0], pas[r][1], self.mu, self.tau) for r in eps.rooms}
            used = [r for r in eps.rooms if fits[r]["n_win"] >= c["hyper_min_windows"]]
            if used:
                th = np.array([fits[r]["theta"] for r in used])
                var = np.array([np.diag(fits[r]["cov"]) for r in used])
                wt = 1.0 / (self.tau ** 2 + var)
                self.mu = (self.prior_mu * c["hyper_shrink"] / self.prior_sd ** 2 + (wt * th).sum(0)) / (c["hyper_shrink"] / self.prior_sd ** 2 + wt.sum(0))
                spread = ((th - self.mu) ** 2 - var).sum(0)
                self.tau = np.sqrt(np.maximum(c["tau_floor"] ** 2, (spread + c["hyper_shrink"] * self.prior_sd ** 2) / (len(used) + c["hyper_shrink"])))
        self.room = fits
        for r, f in fits.items():
            f["passive_points"] = pas[r][2]
        return self

    def samples(self, fit, n, rng):
        x = rng.multivariate_normal(fit["theta"], fit["cov"], size=4 * n)
        inside = x[np.all((x >= self.lo) & (x <= self.hi), axis=1)]
        return np.vstack([inside, np.clip(x, self.lo, self.hi)])[:n]


class Trust:
    def __init__(self, table, cfg):
        self.cfg = cfg
        self.table = table
        x = np.array([feature_row(e["ctx"]) for e in table], float).reshape(-1, len(FEATURES))
        self.mean = x.mean(0) if len(x) else np.zeros(len(FEATURES))
        self.std = np.where(x.std(0) > 1e-6, x.std(0), 1.0) if len(x) else np.ones(len(FEATURES))
        self.w = np.array([cfg["feature_weight"][f] for f in FEATURES])
        self.z = (x - self.mean) / self.std
        self.rooms = np.array([e["room"] for e in table])
        self.scn = np.array([e["scenario"] for e in table])
        self.dose = np.array([e["dose"] for e in table], float)
        self.radius = self.pair_radius()

    def dist(self, z, scenario):
        return np.sqrt(((self.z - z) ** 2 * self.w).sum(1) + np.where(self.scn != scenario, self.cfg["scenario_penalty"] ** 2, 0.0))

    def pair_radius(self):
        n = len(self.z)
        c = self.cfg
        if n < 3:
            return c["radius_default"]
        d = np.array([self.dist(self.z[i], self.scn[i]) for i in range(n)])
        iu = np.triu_indices(n, 1)
        v = d[iu][(self.rooms[:, None] == self.rooms[None, :])[iu]]
        return max(float(np.quantile(v, c["radius_quantile"])), c["radius_floor"]) if len(v) >= 10 else c["radius_default"]

    def known(self, ctx, room, scenario, radius=None):
        own = self.rooms == room
        if not own.any():
            return {"dose": None, "n_similar": 0, "neighbors": []}
        z = (np.array(feature_row(ctx), float) - self.mean) / self.std
        d = np.where(own, self.dist(z, scenario), np.inf)
        idx = np.flatnonzero(d <= (self.radius if radius is None else radius))
        idx = idx[np.argsort(d[idx], kind="stable")]
        if not len(idx):
            near = int(np.argmin(d))
            return {"dose": None, "n_similar": 0, "neighbors": [], "nearest_dist": round(float(d[near]), 3), "nearest_dose": round(float(self.dose[near]), 3)}
        return {"dose": float(self.dose[idx].max()), "n_similar": int(len(idx)), "neighbors": [{"i": int(i), "dist": round(float(d[i]), 3), "dose": round(float(self.dose[i]), 3)} for i in idx[:self.cfg["knn_k"]]]}

    def export(self):
        c = self.cfg
        return {"features": FEATURES, "mean": self.mean.round(6).tolist(), "std": self.std.round(6).tolist(), "weights": self.w.tolist(), "scenario_penalty": c["scenario_penalty"],
                "k": c["knn_k"], "radius": round(self.radius, 4), "growth": c["growth"],
                "episodes": [{"room": e["room"], "scenario": e["scenario"], "x": [round(v, 4) for v in feature_row(e["ctx"])], "dose": round(e["dose"], 4), "boost": e["boost"]} for e in self.table]}


def _from_bundle(cls, b):
    t = Trust.__new__(Trust)
    tr = b["trust"]
    t.cfg = {"scenario_penalty": tr["scenario_penalty"], "knn_k": tr["k"]}
    t.table = tr["episodes"]
    t.mean, t.std, t.w = np.array(tr["mean"]), np.array(tr["std"]), np.array(tr["weights"])
    x = np.array([e["x"] for e in tr["episodes"]], float).reshape(-1, len(FEATURES))
    t.z = (x - t.mean) / t.std
    t.rooms = np.array([e["room"] for e in tr["episodes"]])
    t.scn = np.array([e["scenario"] for e in tr["episodes"]])
    t.dose = np.array([e["dose"] for e in tr["episodes"]], float)
    t.radius = tr["radius"]
    return t


Trust.from_bundle = classmethod(_from_bundle)


class Dosing:
    def __init__(self, bundle):
        self.b = bundle
        c = bundle["dose"]
        self.cand = np.arange(c["min_min"], c["max_min"] + 1e-9, c["step_min"])
        self.trust = None

    def samples(self, room):
        return np.array(self.b["rooms"][room]["samples"], float)

    def peaks(self, room, ctx, target, valve_hist=None, doses=None):
        b = self.b
        r, L, H = b["rooms"][room], b["kernel"]["len_min"], b["horizon_min"]
        hist = np.zeros(L) if valve_hist is None else np.asarray(valve_hist, float)[-L:]
        if valve_hist is None and ctx.get("vm60", 0) > 0:
            hist[-60:] = ctx["vm60"] / 60.0
        hist = np.concatenate([np.zeros(L - len(hist)), hist])
        doses = self.cand if doses is None else np.sort(np.asarray(doses, float))
        pk = closed_loop(self.samples(room), r["a"], r["delta"], ctx["start_temp"], ctx["out"], hist, doses, target, r["pi"], ramp(ctx["since_resume"] + np.arange(H), b["ramp_tau_min"]), L, b["kernel"]["shape"])
        return np.maximum.accumulate(pk, axis=0)

    def exceed(self, room, pk, target):
        limit = min(target + self.b["risk"]["margin_k"], self.b["abort_c"])
        return norm.sf((limit - pk) / self.b["rooms"][room].get("peak_sigma_k", self.b["risk"]["peak_sigma_k"])).mean(axis=1)

    def risk(self, room, ctx, target, valve_hist=None):
        pk = self.peaks(room, ctx, target, valve_hist)
        return self.exceed(room, pk, target), pk

    def decide(self, room, ctx, scenario, target, valve_hist=None, draw=0):
        b = self.b
        dmin = b["dose"]["min_min"]
        p, pk = self.risk(room, ctx, target, valve_hist)
        p0 = float(self.exceed(room, self.peaks(room, ctx, target, valve_hist, [0.0]), target)[0])
        feasible = np.flatnonzero(p - p0 <= b["risk"]["p_max"])
        d_safe = float(self.cand[feasible.max()]) if len(feasible) else 0.0
        t = self.trust or Trust.from_bundle(b)
        self.trust = t
        full = dict(ctx)
        full.setdefault("since_open", b["since_open_cap_min"])
        kn = t.known(full, room, scenario)
        cap = min(d_safe, dmin if kn["dose"] is None else max(dmin, b["trust"]["growth"] * kn["dose"]))
        allowed = cap if scenario in b["scenarios"] and ctx["supply_ok"] > 0 and cap >= dmin - 1e-9 else 0.0
        chosen, p_chosen, p_allowed, j = 0.0, float(p[0]), float(p[0]), None
        if allowed > 0:
            n = int(np.searchsorted(self.cand, allowed + 1e-9))
            top = np.maximum(self.peaks(room, ctx, target, valve_hist, [allowed]), pk[n - 1:n])
            doses, grid = np.append(self.cand[:n], allowed), np.vstack([pk[:n], top])
            j = int(np.random.default_rng(b["seed"] + 7919 * draw + len(b["trust"]["episodes"])).integers(grid.shape[1]))
            ok = np.flatnonzero(grid[:, j] <= target + b["risk"]["thompson_margin_k"])
            k = int(ok.max()) if len(ok) else 0
            chosen = float(doses[k])
            p_chosen, p_allowed = (float(v) for v in self.exceed(room, grid[[k, -1]], target))
        return {"nearest_dist": kn.get("nearest_dist"), "nearest_dose": kn.get("nearest_dose"), "d_safe": d_safe, "known": kn["dose"], "n_similar": kn["n_similar"], "allowed": allowed, "chosen": chosen, "p_overshoot": p_chosen, "p_at_allowed": p_allowed, "j": j, "p_no_boost": p0}


class BoostLearn:
    def __init__(self, config, lake):
        wcfg = dict(config["window"])
        self.cfg = dict(config["boost"], nodes=wcfg["nodes"])
        wcfg["lake"] = lake
        self.grid = Grid.load(lake, wcfg)
        self.cycles = boost_cycles(lake, self.grid, self.cfg)
        self.eps = Episodes(self.grid, self.cfg, self.cycles)
        self.post = Posterior(self.cfg).fit(self.eps)
        self.tune_closure()
        self.table = [p for p in self.eps.pulses if p["known_ok"]]
        self.trust = Trust(self.table, self.cfg)
        self.cal = None

    def bundle(self, version):
        c, rng = self.cfg, np.random.default_rng(self.cfg["seed"])
        cal = self.calibration()
        rooms = {}
        for r, f in self.post.room.items():
            s = self.post.samples(f, c["n_samples"], rng)
            rooms[r] = {"node": c["nodes"][r], "a": round(f["a"], 6), "delta": round(f["delta"], 4), "sigma": round(f["sigma"], 4), "windows": f["n_win"], "pulses": f["n_pulse"], "pi": self.eps.closure[r], "peak_sigma_k": cal["sigma"][r],
                        "theta": f["theta"].round(5).tolist(), "cov": f["cov"].round(6).tolist(), "samples": s.round(5).tolist()}
        return {"version": version, "kind": "boost_learn", "seed": c["seed"], "horizon_min": c["horizon_min"], "ramp_tau_min": c["ramp_tau_min"], "dead_zone": c["dead_zone"], "pulse_cap_min": c["pulse_cap_min"], "open_gap_merge_min": c["merge_gap_min"], "kernel": {"shape": c["kernel_shape"], "len_min": c["kernel_len_min"]},
                "dose": {"min_min": c["dose_min"], "max_min": c["dose_max"], "step_min": c["dose_step"]}, "boost_setpoint_c": c["boost_setpoint"], "abort_c": c["abort_c"], "raise_budget_per_room_hour": 1,
                "risk": {"p_max": c["p_max"], "margin_k": c["margin_k"], "peak_sigma_k": cal["sigma"]["all"], "thompson_margin_k": c["thompson_margin_k"]}, "scenarios": ["morning", "homecoming"],
                "since_open_cap_min": c["since_open_cap_min"], "hyper": {"mu": self.post.mu.round(4).tolist(), "tau": self.post.tau.round(4).tolist()}, "rooms": rooms, "trust": self.trust.export()}

    def calibration(self):
        if self.cal is None:
            self.cal = self.calibrate()
        return self.cal

    def tune_closure(self):
        c, rooms = self.cfg, self.eps.rooms
        err = {r: np.zeros((len(c["closure_scales"]), 0)) for r in rooms}
        for r in rooms:
            f, base = self.post.room[r], self.eps.closure[r]
            rows = []
            for p in self.eps.pulses:
                y = self.eps.series[r].temp[p["start"]:p["hi"] + 1] if p["room"] == r and p["clean"] else []
                if np.isfinite(y).any():
                    rows.append([abs(float(np.nanmax(y)) - float(self.pulse_forecast(r, f, f["theta"], p, dict(base, kp=base["kp"] * k)).max())) for k in c["closure_scales"]])
            err[r] = np.array(rows, float).reshape(-1, len(c["closure_scales"]))
        pooled = np.concatenate(list(err.values()))
        for r in rooms:
            e = err[r] if len(err[r]) >= c["closure_min_pulses"] else pooled
            loss = e.mean(axis=0) if len(e) else np.zeros(len(c["closure_scales"]))
            k = int(np.flatnonzero(loss <= loss.min() * (1.0 + c["closure_tol"]) + 1e-9).max())
            base = self.eps.closure[r]
            self.eps.closure[r] = dict(base, kp=round(base["kp"] * c["closure_scales"][k], 5), scale=c["closure_scales"][k], forecast_mae_k=round(float(loss[k]), 3), forecast_mae_untuned_k=round(float(loss[-1]), 3), tuned_on=int(len(e)))

    def pulse_forecast(self, room, f, th, p, pi=None):
        c, s = self.cfg, self.eps.series[room]
        L, x = c["kernel_len_min"], p["start"]
        n = p["hi"] - x + 1
        hist = np.zeros(L)
        h = s.u[max(x - L, 0):x]
        hist[L - len(h):] = h
        tout = s.out[x:x + n]
        tout = np.concatenate([tout, np.full(n - len(tout), tout[-1])])
        target = p["ctx"]["start_temp"] + p["ctx"]["gap"]
        return closed_loop(th, f["a"], f["delta"], p["ctx"]["start_temp"], tout, hist, [p["dose"]], target, pi or self.eps.closure[room], ramp(p["ctx"]["since_resume"] + np.arange(n), c["ramp_tau_min"]), L, c["kernel_shape"])[0]

    def calibrate(self):
        c = self.cfg
        rng = np.random.default_rng(c["seed"] + 2)
        recs = {r: [] for r in self.eps.rooms}
        for r in self.eps.rooms:
            s = self.eps.series[r]
            for w in self.eps.windows[r]:
                post = Posterior(c).fit(self.eps, drop=w)
                f = post.room[r]
                th = post.samples(f, c["n_samples"], rng)
                for p in w["pulses"]:
                    y = s.temp[p["start"]:p["hi"] + 1]
                    if np.isfinite(y).any():
                        recs[r].append({"w": id(w), "obs": float(np.nanmax(y)), "rise": p["rise"], "pk": self.pulse_forecast(r, f, th, p), "z": rng.standard_normal(len(th)), "strong": p["dose"] >= c["strong_dose"]})
        pool = [x for v in recs.values() for x in v]
        pooled = self.sigma([x for x in pool if x["strong"]] or pool)
        sigma = {"all": pooled}
        for r, v in recs.items():
            strong = [x for x in v if x["strong"]]
            sigma[r] = self.sigma(strong) if len(strong) >= c["loeo_min_strong"] else pooled
        return {"sigma": sigma, "report": {r: self.report(r, v, sigma[r]) for r, v in recs.items()}}

    def interval(self, x, sigma):
        return np.quantile(x["pk"] + sigma * x["z"], [0.05, 0.5, 0.95])

    def sigma(self, recs):
        c = self.cfg
        for sg in np.arange(c["peak_sigma_floor"], c["sigma_max"] + 1e-9, c["sigma_step"]):
            if not recs or np.mean([self.interval(x, sg)[0] <= x["obs"] <= self.interval(x, sg)[2] for x in recs]) >= c["coverage_target"]:
                return round(float(sg), 4)
        return round(float(c["sigma_max"]), 4)

    def report(self, room, recs, sigma):
        if not recs:
            return {"n": 0, "sigma_k": sigma}
        q = [self.interval(x, sigma) for x in recs]
        stats = lambda idx: {"n": len(idx), "mae_k": round(float(np.mean([abs(recs[i]["obs"] - q[i][1]) for i in idx])), 3), "baseline_mae_k": round(float(np.mean([abs(recs[i]["rise"] - np.mean([x["rise"] for x in recs if x["w"] != recs[i]["w"]] or [0.0])) for i in idx])), 3),
                             "coverage90": round(float(np.mean([q[i][0] <= recs[i]["obs"] <= q[i][2] for i in idx])), 3), "interval_width_k": round(float(np.mean([q[i][2] - q[i][0] for i in idx])), 3), "bias_k": round(float(np.mean([recs[i]["obs"] - q[i][1] for i in idx])), 3)}
        strong = [i for i, x in enumerate(recs) if x["strong"]]
        return dict(stats(range(len(recs))), sigma_k=sigma, strong=stats(strong) if strong else None)

    def contexts(self, room):
        c = self.cfg
        out = {}
        for name, k in c["typical"].items():
            ctx = {"out": k["out"], "wind": k["wind"], "rad": k["rad"], "start_temp": k["start"], "gap": k["target"] - k["start"], "supply_ok": 1.0, "since_resume": k["resume"], "vm60": 0.0, "vm180": 0.0, "since_open": c["since_open_cap_min"], "hour": k["hour"]}
            out[name] = (ctx, k["scenario"], k["target"])
        return out

    def justification(self, bundle):
        dosing = Dosing(bundle)
        res = {}
        for r, f in self.post.room.items():
            s = self.post.samples(f, 2000, np.random.default_rng(self.cfg["seed"] + 1))
            g, m = np.exp(s[:, 0]), np.exp(s[:, 1])
            res[r] = {"windows": f["n_win"], "pulses": f["n_pulse"], "gain_k_per_valve_min": [round(float(np.median(g)), 4), round(float(np.quantile(g, 0.05)), 4), round(float(np.quantile(g, 0.95)), 4)],
                      "delay_mean_min": [round(float(np.median(m)), 1), round(float(np.quantile(m, 0.05)), 1), round(float(np.quantile(m, 0.95)), 1)], "cooling_per_min": round(f["a"], 5), "noise_sigma_k": round(f["sigma"], 3), "thermostat": self.eps.closure[r], "peak_sigma_k": bundle["rooms"][r]["peak_sigma_k"]}
            for name, (ctx, scn, target) in self.contexts(r).items():
                dec = dosing.decide(r, ctx, scn, target)
                res[r][name] = {k: (None if v is None else round(v, 3) if isinstance(v, float) else v) for k, v in dec.items()}
        return res

    def validate(self, bundle):
        return {"loeo": self.calibration()["report"], "monotone": self.monotone(bundle), "bounds": self.bounds(bundle)}

    def monotone(self, bundle):
        c, rng = self.cfg, np.random.default_rng(self.cfg["seed"] + 3)
        L, H, worst, worst_dose = c["kernel_len_min"], c["horizon_min"], 0.0, 0.0
        dosing = Dosing(bundle)
        for _ in range(c["monotone_trials"]):
            r = rng.choice(self.eps.rooms)
            f = self.post.room[r]
            s = self.post.samples(f, 8, rng)
            t0, tout = rng.uniform(5, 26), rng.uniform(-15, 30)
            u1 = rng.uniform(0, 1, L + H) * (rng.uniform(size=L + H) < rng.uniform(0.05, 0.7))
            u2 = np.clip(u1 + rng.uniform(0, 0.5, L + H) * (rng.uniform(size=L + H) < 0.3), 0, 1)
            a = rollout(s, f["a"], f["delta"], t0, tout, u1, L, c["kernel_shape"])
            b = rollout(s, f["a"], f["delta"], t0, tout, u2, L, c["kernel_shape"])
            worst = max(worst, float((a - b).max()))
            ctx = {"out": tout, "wind": 3.0, "rad": 0.0, "start_temp": t0, "gap": 2.0, "supply_ok": 1.0, "since_resume": 60.0, "vm60": 0.0, "vm180": 0.0, "since_open": 360.0, "hour": 7.0}
            pk = dosing.peaks(r, ctx, t0 + 2.0)
            worst_dose = max(worst_dose, float((pk[:-1] - pk[1:]).max()))
        return {"trials": c["monotone_trials"], "max_violation_state_k": round(worst, 9), "max_violation_dose_k": round(worst_dose, 9)}

    def bounds(self, bundle):
        c, rng = self.cfg, np.random.default_rng(self.cfg["seed"] + 4)
        dosing = Dosing(bundle)
        t = Trust.from_bundle(bundle)
        bad, zero, growth, dmin = 0, 0, bundle["trust"]["growth"], bundle["dose"]["min_min"]
        for _ in range(c["bounds_trials"]):
            e = t.table[rng.integers(len(t.table))] if len(t.table) else None
            room = str(rng.choice(self.eps.rooms))
            x = np.array(e["x"], float) if e else np.array([5, 3, 0, 18, 2, 1, 60, 0, 0, 360, 0, 1], float)
            x = x + rng.normal(0, 0.3, len(x)) * t.std
            ctx = dict(zip(FEATURES[:10], x[:10]))
            ctx["hour"] = float(rng.uniform(0, 24))
            ctx["supply_ok"], ctx["start_temp"] = float(rng.integers(2)), float(np.clip(ctx["start_temp"], 8, 22))
            target = ctx["start_temp"] + float(rng.uniform(0.5, 7))
            scenario = str(rng.choice(["morning", "homecoming", "room_change", "other"]))
            d = dosing.decide(room, ctx, scenario, target)
            cap = min(d["d_safe"], dmin) if d["known"] is None else min(d["d_safe"], max(dmin, growth * d["known"]))
            own = t.dose[t.rooms == room]
            bad += d["allowed"] > cap + 1e-9 or d["chosen"] > d["allowed"] + 1e-9 or (0 < d["allowed"] < dmin) or (d["allowed"] > 0 and scenario not in bundle["scenarios"]) or (d["known"] is not None and d["known"] not in own)
            zero += d["allowed"] == 0
        return {"trials": c["bounds_trials"], "violations": int(bad), "no_boost_share": round(zero / c["bounds_trials"], 3), "episodes": len(t.table), "radius": t.radius, "k": t.cfg["knn_k"]}


def boost_cycles(lake, grid, cfg):
    t = newest_cycles(lake)
    out = {}
    for e in t:
        det = e["detail"]
        r = det.get("room")
        if r in grid.rooms and det.get("start_ts_ms"):
            idx = grid.minute(utc(det["start_ts_ms"] / 1000.0).isoformat())
            out.setdefault(r, []).append({"start_idx": idx, "target": det.get("target"), "overshoot_c": det.get("overshoot_c"), "dose": det.get("boost_min", det.get("applied_t_off_min")), "scenario": det.get("scenario") if det.get("scenario") in ("morning", "homecoming", "manual") else None})
    return out


def cycle_rows(lake):
    cols = ["ts", "kind", "detail"]
    out = []
    root = Path(lake)
    for f in sorted((root / "parquet" / "events").glob("**/part.parquet")):
        t = pq.read_table(f, columns=cols, filters=[("kind", "=", "boost_cycle")])
        out += list(zip(t.column("ts").to_pylist(), t.column("detail").to_pylist()))
    raw = read_arrows(root, "events")
    if raw is not None:
        k = raw.column("kind").to_pylist()
        ts, dt = raw.column("ts").to_pylist(), raw.column("detail").to_pylist()
        out += [(a, b) for a, b, c in zip(ts, dt, k) if c == "boost_cycle"]
    return out


def newest_cycles(lake):
    seen, out = set(), []
    for ts, det in cycle_rows(lake):
        try:
            d = json.loads(det) if det else {}
        except ValueError:
            continue
        key = (ts, d.get("cycle_id"), d.get("variant"))
        if key in seen:
            continue
        seen.add(key)
        if d.get("mode", "live") == "live":
            out.append({"ts": ts, "detail": d})
    return out


def latest_bundle_time(models_dir):
    best = None
    for p in (Path(models_dir) / "boost").glob("*/bundle.json"):
        try:
            t = datetime.strptime(p.parent.name, "%Y%m%dT%H%M%SZ").replace(tzinfo=timezone.utc)
        except ValueError:
            continue
        best = t if best is None or t > best else best
    return best


def needs_retrain(lake, models_dir):
    ts = [e["ts"] for e in newest_cycles(lake)]
    if not ts:
        return False
    newest = max(ts)
    newest = newest if newest.tzinfo else newest.replace(tzinfo=timezone.utc)
    last = latest_bundle_time(models_dir)
    return last is None or newest > last


def boostlearn(config, lake, models_dir, validate=False, version=None):
    bl = BoostLearn(config, lake)
    version = version or datetime.now(timezone.utc).strftime("%Y%m%dT%H%M%SZ")
    bundle = bl.bundle(version)
    meta = {"version": version, "created": datetime.now(timezone.utc).isoformat(timespec="seconds"), "train_from": utc(bl.grid.t[0]).isoformat(), "train_to": utc(bl.grid.t[-1]).isoformat(),
            "boost_cycles": {r: len(v) for r, v in bl.cycles.items()}, "episodes": {r: sum(1 for e in bl.table if e["room"] == r) for r in bl.eps.rooms}, "radius": bundle["trust"]["radius"], "k": bundle["trust"]["k"],
            "hyper": bundle["hyper"], "peak_sigma_k": bundle["risk"]["peak_sigma_k"], "first_boost": bl.justification(bundle)}
    if validate:
        meta["validation"] = bl.validate(bundle)
    out = Path(models_dir) / "boost" / version
    out.mkdir(parents=True, exist_ok=True)
    (out / "bundle.json").write_text(json.dumps(bundle, separators=(",", ":")))
    (out / "meta.json").write_text(json.dumps(meta, indent=1))
    for old in sorted((Path(models_dir) / "boost").glob("*/"))[:-config["boost"]["keep_bundles"]]:
        for f in old.iterdir():
            f.unlink()
        old.rmdir()
    return out, bundle, meta


def retry_path(models_dir):
    return Path(models_dir) / "boost" / "retry.json"


def read_retry(models_dir):
    try:
        return json.loads(retry_path(models_dir).read_text())
    except (OSError, ValueError):
        return None


def boostmark(config, models_dir, ok, now=None):
    path, now = retry_path(models_dir), now or datetime.now(timezone.utc)
    if ok:
        path.unlink(missing_ok=True)
        return None
    c = config["boost"]["retry"]
    fails = (read_retry(models_dir) or {}).get("fails", 0) + 1
    delay = min(c["base_min"] * 2 ** (fails - 1), c["max_min"])
    state = {"fails": fails, "delay_min": delay, "next_try": (now + timedelta(minutes=delay)).isoformat(timespec="seconds")}
    path.parent.mkdir(parents=True, exist_ok=True)
    path.write_text(json.dumps(state))
    return state


def boostcheck(config, lake, models_dir, now=None):
    r = read_retry(models_dir)
    if r and (now or datetime.now(timezone.utc)) < datetime.fromisoformat(r["next_try"]):
        return 0
    return 10 if needs_retrain(lake, models_dir) else 0
