import itertools
import json
import math
from datetime import datetime, timezone
from pathlib import Path
import duckdb
import numpy as np
from heattrain.thermal import RoomPar, build_rooms, epoch, fit, fit_room, load_events, load_lake, norm_cdf, raw_batches

ROOM_KEYS = ("a1", "a2", "b1", "b2", "b3", "b4", "w", "ts", "ts_night")


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


REC_KEYS = ("m", "a", "lam", "lam_std", "p_window", "p_supply")


def erf_np(x):
    t = 1.0 / (1.0 + 0.3275911 * np.abs(x))
    y = 1.0 - (((((1.061405429 * t - 1.453152027) * t) + 1.421413741) * t - 0.284496736) * t + 0.254829592) * t * np.exp(-x * x)
    return np.where(x >= 0, y, -y)


def norm_cdf_np(z):
    return 0.5 * (1.0 + erf_np(z / math.sqrt(2.0)))


def lg_pdf_np(v, s):
    with np.errstate(invalid="ignore", divide="ignore"):
        return np.where(s > 0, -0.5 * (v * v / np.where(s > 0, s, 1.0) + np.log(2.0 * math.pi * np.where(s > 0, s, 1.0))), np.nan)


class Inputs:
    def __init__(self, grid, docs, drop=None):
        names = grid.names
        col = lambda k: np.array([grid.cols[n][k] for n in names], float)
        self.y, self.yv, self.u, self.sp, self.tout, self.wind, self.rad = (col(k) for k in ("y", "yv", "u", "sp", "tout", "wind", "rad"))
        if drop is not None:
            hide = np.array([drop[n] for n in names], bool)
            self.y, self.yv = np.where(hide, np.nan, self.y), np.where(hide, np.nan, self.yv)
        self.night = col("night") > 0.5
        self.cn, self.cs = np.zeros_like(self.u), np.zeros_like(self.u)
        nsum_ok = np.ones(self.u.shape, bool)
        for i, n in enumerate(names):
            c = grid.cols[n]
            for ck, a, s in zip([docs[n]["c"][k] for k in sorted(docs[n]["c"])], c["ncnt"], c["nsum"]):
                self.cn[i] = self.cn[i] + ck * np.asarray(a, float)
                self.cs[i] = self.cs[i] + ck * np.asarray(s, float)
                nsum_ok[i] &= np.isfinite(np.asarray(s, float))
        self.dyn = np.isfinite(self.u) & np.isfinite(self.tout) & np.isfinite(self.wind) & np.isfinite(self.rad) & nsum_ok
        self.n = self.u.shape[1]


class BatchHouse:
    def __init__(self, docs, names, shocks, house):
        self.names, self.house = names, house
        G, R = len(shocks), len(names)
        self.G, self.R = G, R
        par = [RoomPar(docs[n]) for n in names]
        vec = lambda f: np.array([f(p) for p in par], float)
        self.a1, self.a2, self.b1, self.b2, self.b3, self.b4, self.w, self.ts, self.ts_night = (vec(lambda p, k=k: getattr(p, k)) for k in ("a1", "a2", "b1", "b2", "b3", "b4", "w", "ts", "ts_night"))
        self.g, self.sigma_u, self.noise_r, self.p0, self.q_r, self.q_a = (vec(lambda p, k=k: getattr(p, k)) for k in ("g", "sigma_u", "noise_r", "p0", "q_r", "q_a"))
        sv = lambda k: np.array([s[k] for s in shocks], float)
        self.phi = np.exp(-1.0 / sv("tau_l"))
        pj, hold = sv("p_jump"), sv("hold_min")
        self.tm = np.stack([np.stack([1.0 - pj, pj], -1), np.stack([1.0 / hold, 1.0 - 1.0 / hold], -1)], 1)
        self.q_l = np.stack([sv("q_calm") ** 2, sv("q_jump") ** 2], -1)
        self.p_l0, self.lam_w = sv("p_l0"), sv("lambda_w")
        pu, pd = house["p_up"], house["p_down"]
        self.tr = np.array([[1.0 - pu, pu], [pd, 1.0 - pd]])
        jj = np.arange(4)
        self.coef = self.tr[jj[:, None] // 2, jj[None, :] // 2][None] * self.tm[:, jj[:, None] % 2, jj[None, :] % 2]
        self.x = np.zeros((G, R, 4, 3))
        self.x[..., 2] = 1.0
        self.p = np.zeros((G, R, 4, 3, 3))
        self.nu = np.zeros((G, R, 2, 2))
        self.nu[..., 0] = 1.0
        self.init = np.zeros((G, R), bool)
        self.since = np.zeros((G, R))
        self.hot = np.zeros((G, R))
        self.w_prior = np.tile([1.0 - house["p_supply0"], house["p_supply0"]], (G, 1))
        self.wpost = self.w_prior.copy()
        self.loglik = np.zeros(G)
        self.events = []
        self.st = np.array([0.0, 0.0, 1.0, 1.0])
        self.zero = np.zeros((G, R, 4))
        self.qdiag = np.zeros((G, R, 4, 3, 3))
        self.qdiag[..., 0, 0] = (self.q_r / 60.0)[None, :, None]
        self.qdiag[..., 1, 1] = (self.q_a / 60.0)[None, :, None]
        self.qdiag[..., 2, 2] = np.tile(self.q_l, 2)[:, None, :]

    def mu(self, w):
        return (w[:, None, :, None] * self.nu).reshape(self.G, self.R, 4)

    def seed(self, mask, inp, k):
        if not mask.any():
            return
        y = inp.y[:, k]
        m = a = np.broadcast_to(y, mask.shape)
        if self.house["balance_seed"]:
            amb = self.b2 + self.b3 * inp.wind[:, k]
            cn, cs = inp.cn[:, k], inp.cs[:, k]
            d = ((amb + cn) * y - amb * inp.tout[:, k] - cs - self.b4 * inp.rad[:, k] / 100.0) / (self.b1 + (amb + cn) * self.w)
            a_b = y - self.w * d
            bal = inp.dyn[:, k]
            a = np.where(bal, a_b, y) * np.ones(mask.shape)
            m = np.where(bal, a_b + d, y) * np.ones(mask.shape)
        x0 = np.stack([m, a, np.ones(mask.shape)], -1)[:, :, None, :]
        p0 = np.zeros((self.G, self.R, 3, 3))
        p0[..., 0, 0] = self.p0
        p0[..., 1, 1] = self.p0
        p0[..., 2, 2] = self.p_l0[:, None]
        mk = mask[:, :, None, None]
        self.x = np.where(mk, x0, self.x)
        self.p = np.where(mk[..., None], p0[:, :, None], self.p)
        self.nu = np.where(mk, np.array([[1.0, 0.0], [1.0, 0.0]]), self.nu)
        self.init |= mask
        self.since = np.where(mask, 0.0, self.since)
        self.hot = np.where(mask, 0.0, self.hot)

    def log(self, mask, cause, k, phase):
        for g, r in zip(*np.nonzero(mask)):
            self.events.append((k, int(r), phase, int(g), cause))

    def obs(self, x, p, h, v, r, mask):
        ph = np.einsum("...ij,...j->...i", p, h)
        s = np.einsum("...i,...i->...", h, ph) + r
        ok = mask & (s > 0)
        kg = ph / np.where(s > 0, s, 1.0)[..., None]
        xn = x + kg * v[..., None]
        q = p - kg[..., :, None] * ph[..., None, :]
        pn = 0.5 * (q + np.swapaxes(q, -1, -2))
        return np.where(ok[..., None], xn, x), np.where(ok[..., None, None], pn, p), s

    def step(self, inp, k, record=False):
        hs = self.house
        dyn = np.broadcast_to(inp.dyn[:, k], (self.G, self.R))
        y = inp.y[:, k]
        y_ok = np.broadcast_to(np.isfinite(y), (self.G, self.R))
        new = dyn & ~self.init & y_ok
        gap = dyn & self.init & y_ok & (self.since > hs["reset_gap_min"])
        self.log(gap, "gap", k, 0)
        self.seed(new | gap, inp, k)
        run = dyn & self.init
        v_ok = np.isfinite(inp.yv[:, k]) & (inp.yv[:, k] > 0.0) & (inp.yv[:, k] < 1.0) & np.isfinite(inp.sp[:, k])
        r1 = hs["r_grid"] + self.noise_r ** 2
        x, p = self.x, self.p
        wv = self.w[None, :, None]
        yhat = wv * x[..., 0] + (1 - wv) * x[..., 1]
        hy = np.stack([self.w, 1 - self.w, np.zeros(self.R)], -1)[None, :, None, :]
        ym = (run & y_ok)[..., None] & np.ones(4, bool)
        x, p, s1 = self.obs(x, p, np.broadcast_to(hy, x.shape), np.where(ym, y[None, :, None] - yhat, 0.0), r1[None, :, None], ym)
        ll = np.where(ym, lg_pdf_np(y[None, :, None] - yhat, s1), 0.0)
        vm = (run & v_ok[None, :])[..., None] & np.ones(4, bool)
        sens = wv * x[..., 0] + (1 - wv) * x[..., 1]
        g = self.g[None, :, None]
        v = inp.yv[:, k][None, :, None] - g * (inp.sp[:, k][None, :, None] - sens)
        hv = np.stack([-self.g * self.w, -self.g * (1 - self.w), np.zeros(self.R)], -1)[None, :, None, :]
        x, p, sv = self.obs(x, p, np.broadcast_to(hv, x.shape), np.where(vm, v, 0.0), (self.sigma_u ** 2)[None, :, None], vm)
        ll = ll + np.where(vm, lg_pdf_np(v, sv), 0.0)
        self.x, self.p = x, p
        mu0 = self.mu(self.w_prior)
        mean = (mu0 * yhat).sum(-1)
        var = (mu0 * (s1 + (yhat - mean[..., None]) ** 2)).sum(-1)
        z = np.where(np.isfinite(var) & (var > 0), np.abs(y[None, :] - mean) / np.sqrt(np.where(var > 0, var, 1.0)), np.inf)
        yo = run & y_ok
        self.hot = np.where(yo, np.where(z > hs["reset_sigma"], self.hot + 1, 0.0), self.hot)
        self.since = np.where(yo, 0.0, self.since + 1)
        fin = np.isfinite(ll).all(-1)
        flag = run & ~fin
        pend = run & fin
        top = np.where(pend, ll.max(-1), 0.0)
        e = np.where(pend[..., None], np.exp(ll - top[..., None]), 0.0)
        lik = (self.nu * e.reshape(self.G, self.R, 2, 2)).sum(-1)
        wp = self.w_prior.copy()
        scale = np.zeros(self.G)
        for r in range(self.R):
            wp = np.where(pend[:, r, None], wp * lik[:, r], wp)
            scale = scale + np.where(pend[:, r], top[:, r], 0.0)
        tot = wp[:, 0] + wp[:, 1]
        good = (tot > 0) & np.isfinite(tot)
        self.loglik = np.where(good, self.loglik + scale + np.log(np.where(good, tot, 1.0)), self.loglik)
        w = np.where(good[:, None], wp / np.where(good, tot, 1.0)[:, None], self.w_prior)
        flag = np.where(good[:, None], flag, run)
        pend &= good[:, None]
        t = self.nu * e.reshape(self.G, self.R, 2, 2)
        zt = t.sum(-1, keepdims=True)
        nun = np.where(zt > 0, t / np.where(zt > 0, zt, 1.0), np.array([1.0, 0.0]))
        self.nu = np.where(pend[..., None, None], nun, self.nu)
        mu = self.mu(w)
        allfin = np.isfinite(self.x).all((-1, -2)) & np.isfinite(self.p).all((-1, -2, -3)) & np.isfinite(self.nu).all((-1, -2)) & np.isfinite(w).all(-1)[:, None]
        pd = self.p
        dg = np.diagonal(pd, axis1=-2, axis2=-1)
        psd = (dg >= -1e-12).all(-1)
        for i, j in ((0, 1), (0, 2), (1, 2)):
            psd &= pd[..., i, j] ** 2 <= pd[..., i, i] * pd[..., j, j] * (1.0 + 1e-6) + 1e-12
        psd = psd.all(-1)
        lam = (mu * self.x[..., 2]).sum(-1)
        bad_nan = run & (flag | ~allfin)
        bad_psd = run & ~bad_nan & ~psd
        bad_lam = run & ~bad_nan & ~bad_psd & ((lam < 0.5) | (lam > 8.0))
        bad_hot = run & ~bad_nan & ~bad_psd & ~bad_lam & (self.hot >= hs["reset_n"])
        bad = bad_nan | bad_psd | bad_lam | bad_hot
        if bad.any():
            for mask, cause in ((bad_nan, "nan"), (bad_psd, "psd"), (bad_lam, "lambda"), (bad_hot, "innovation")):
                self.log(mask, cause, k, 1)
            self.seed(bad & y_ok, inp, k)
            self.init &= ~(bad & ~y_ok)
        ok = run & self.init
        self.wpost = w
        out = self.report(w) if record else None
        self.advance(inp, k, w, ok)
        self.w_prior = w @ self.tr
        return ok, out

    def report(self, w):
        mu = self.mu(w)
        x2, p22 = self.x[..., 2], self.p[..., 2, 2]
        m, a, lam = ((mu * self.x[..., i]).sum(-1) for i in range(3))
        var = (mu * (p22 + (x2 - lam[..., None]) ** 2)).sum(-1)
        pw = (mu * (1.0 - norm_cdf_np((self.lam_w[:, None, None] - x2) / np.sqrt(np.maximum(p22, 1e-18))))).sum(-1)
        return {"m": m, "a": a, "lam": lam, "lam_std": np.sqrt(var), "p_window": pw, "p_supply": np.broadcast_to(w[:, 1:2], m.shape)}

    def advance(self, inp, k, w, ok):
        h = 1.0 / 60.0
        mu = self.mu(w)
        om = self.coef[:, None] * mu[..., :, None]
        c = om.sum(-2)
        cs = np.where(c > 0, c, 1.0)
        x, p = self.x, self.p
        omt = np.swapaxes(om, -1, -2)
        xb = (omt @ x) / cs[..., None]
        dx = np.swapaxes(x[:, :, :, None, :] - xb[:, :, None, :, :], 2, 3)
        pb = ((omt @ p.reshape(self.G, self.R, 4, 9)).reshape(self.G, self.R, 4, 3, 3) + np.swapaxes(omt[..., None] * dx, -1, -2) @ dx) / cs[..., None, None]
        pos = c > 0
        xb = np.where(pos[..., None], xb, x)
        pb = np.where(pos[..., None, None], pb, p)
        t = c.reshape(self.G, self.R, 2, 2)
        tt = t.sum(-1, keepdims=True)
        nu = np.where(tt > 0, t / np.where(tt > 0, tt, 1.0), np.array([1.0, 0.0]))
        lam = xb[..., 2]
        u, tout, wind, rad, cn, cs_ = (v[:, k][None, :, None] for v in (inp.u, inp.tout, inp.wind, inp.rad, inp.cn, inp.cs))
        a1, a2, b1, b2, b3, b4 = (v[None, :, None] for v in (self.a1, self.a2, self.b1, self.b2, self.b3, self.b4))
        ts = np.where(inp.night[:, k], self.ts_night, self.ts)[None, :, None]
        au = a1 * (u * self.st)
        amb = b2 + b3 * wind
        f11, f12, f21, f22, g1, g2, phi, j23 = (np.broadcast_to(v, lam.shape) for v in (1 + h * (-(au + a2)), h * a2, h * b1, 1 + h * (-(b1 + (lam * amb + cn))), h * (au * ts), h * (lam * amb * tout + b4 * rad / 100.0 + cs_),
                                                                               self.phi[:, None, None], h * amb * (tout - xb[..., 1])))
        zero = self.zero
        F = np.stack([np.stack([f11, f12, zero], -1), np.stack([f21, f22, j23], -1), np.stack([zero, zero, phi], -1)], -2)
        fp = F @ pb @ np.swapaxes(F, -1, -2)
        fp += self.qdiag
        xn = np.stack([f11 * xb[..., 0] + f12 * xb[..., 1] + g1, f21 * xb[..., 0] + f22 * xb[..., 1] + g2, 1.0 + phi * (lam - 1.0)], -1)
        pn = 0.5 * (fp + np.swapaxes(fp, -1, -2))
        self.x = np.where(ok[..., None, None], xn, self.x)
        self.p = np.where(ok[..., None, None, None], pn, self.p)
        self.nu = np.where(ok[..., None, None], nu, self.nu)

    def resets(self, g):
        return [{"room": self.names[r], "cause": cause, "step": k} for k, r, _, gg, cause in sorted(self.events) if gg == g]


def run_batch(grid, docs, shocks, house, drop=None, record=False, inp=None):
    inp = inp or Inputs(grid, docs, drop)
    bh = BatchHouse(docs, grid.names, shocks, house)
    arr = {key: np.full((len(shocks), len(grid.names), inp.n), np.nan) for key in REC_KEYS} if record else None
    with np.errstate(invalid="ignore", divide="ignore", over="ignore"):
        for k in range(inp.n):
            ok, out = bh.step(inp, k, record)
            if record:
                for key in REC_KEYS:
                    arr[key][:, :, k] = np.where(ok, out[key], np.nan)
    rec = [{n: {key: arr[key][g, r] for key in REC_KEYS} for r, n in enumerate(grid.names)} for g in range(len(shocks))] if record else None
    return bh, rec


def shock_defaults(rc, **over):
    s = rc["shock"]
    return {"tau_l": s["tau_l"], "hold_min": s["hold_min"], "p_l0": s["p_l0"], "lambda_jump": s["lambda_jump"], "lambda_w": s["lambda_w"], **s["start"], **over}


class ShockScores:
    def __init__(self, grid, docs, rc, drop):
        self.grid, self.docs, self.rc = grid, docs, rc
        self.inp = Inputs(grid, docs, drop)
        self.cache = {}

    def __call__(self, trials):
        key = lambda t: tuple(sorted(t.items()))
        todo = list({key(t): t for t in trials if key(t) not in self.cache}.values())
        if todo:
            bh, _ = run_batch(self.grid, self.docs, [shock_defaults(self.rc, **t) for t in todo], self.rc["house"], inp=self.inp)
            self.cache.update({key(t): float(-ll) for t, ll in zip(todo, bh.loglik)})
        return [self.cache[key(t)] for t in trials]


def fit_shock(grid, docs, rc, drop):
    s = rc["shock"]
    best = dict(s["start"])
    nll = ShockScores(grid, docs, rc, drop)
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
    run, recs = run_batch(grid, docs, [shock], rc["house"], record=True)
    rec, resets = recs[0], run.resets(0)
    lw, lj, tuning = tune_windows(grid, rec, rc, gt, labels)
    shock = dict(shock, lambda_w=lw, lambda_jump=lj)
    version = version or datetime.now(timezone.utc).strftime("%Y%m%dT%H%M%SZ")
    house = dict(rc["house"], night_hours=cfg["night_hours"], timezone=cfg["timezone"])
    bundle = {"kind": "room_model", "version": version, "dt_s": grid.dt, "rooms": {n: {k: v for k, v in d.items() if k != "note"} for n, d in docs.items()},
              "shock": {k: shock[k] for k in ("tau_l", "q_calm", "q_jump", "p_jump", "hold_min", "lambda_jump", "lambda_w", "p_l0")}, "house": house}
    report = {"version": version, "nll": nll, "trace": [{"shock": t, "nll": s} for t, s in trace], "tuning": tuning, "forecast_20min": forecast_error(grid, docs, rec, rc), "resets": len(resets),
              "reset_causes": [r["cause"] for r in resets], "notes": {n: d["note"] for n, d in docs.items() if d["note"]}, "ground_truth_events": len(gt), "labels": len(labels), "loglik": float(run.loglik[0]),
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
