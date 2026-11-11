import math
import numpy as np
from heattrain.roomimm import RoomPar
from heattrain.roomobs import D_MIN, LOG_SQRT_2_OVER_PI, P_FLOOR, SQRT2, SQRT2PI, Z_MIN


def valve_lag(u, alpha):
    out = np.full(len(u), np.nan)
    v1 = v2 = None
    for k, x in enumerate(np.asarray(u, float).tolist()):
        if x == x:
            if v1 is None or alpha == 1.0:
                v1 = v2 = x
            else:
                v1 = v1 + alpha * (x - v1)
                v2 = v2 + alpha * (v1 - v2)
        if v2 is not None:
            out[k] = v2
    return out


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
                                 "bad": pad(d.bad.astype(float), 1.0), "ya": [pad(np.asarray(a, float)) for a in (d.ext or {}).get("ya", [])], **{k: pad(np.asarray((d.ext or {}).get(k, np.full(d.n, np.nan)), float)) for k in ("co2", "rh", "th")}, "ncnt": [nb[k][0] for k in self.kinds[d.room]], "nsum": [nb[k][1] for k in self.kinds[d.room]]}
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


def erfc_nr_np(x):
    z = np.abs(x)
    t = 1.0 / (1.0 + 0.5 * z)
    r = t * np.exp(-z * z - 1.26551223 + t * (1.00002368 + t * (0.37409196 + t * (0.09678418 + t * (-0.18628806 + t * (0.27886807 + t * (-1.13520398 + t * (1.48851587 + t * (-0.82215223 + t * 0.17087277)))))))))
    return np.where(x >= 0, r, 2.0 - r)


def norm_sf_np(z):
    return 0.5 * erfc_nr_np(z / SQRT2)


def bin_prob_np(zlo, zhi):
    return np.where(zlo >= 0.0, norm_sf_np(zlo) - norm_sf_np(zhi), np.where(zhi <= 0.0, norm_sf_np(-zhi) - norm_sf_np(-zlo), 1.0 - norm_sf_np(zhi) - norm_sf_np(-zlo)))


def abs_hum_np(t, rh):
    ok = np.isfinite(t) & np.isfinite(rh) & (t > -40.0)
    tt = np.where(ok, t, 20.0)
    return np.where(ok, 216.7 * (rh / 100.0 * 6.112 * np.exp(17.62 * tt / (243.12 + tt))) / (273.15 + tt), np.nan)


def rate_np(x, win):
    out = np.full(x.shape, np.nan)
    if win < x.shape[-1]:
        out[..., win:] = (x[..., win:] - x[..., :-win]) / win
    return out


def lg_pdf_np(v, s):
    with np.errstate(invalid="ignore", divide="ignore"):
        return np.where(s > 0, -0.5 * (v * v / np.where(s > 0, s, 1.0) + np.log(2.0 * math.pi * np.where(s > 0, s, 1.0))), np.nan)


class Inputs:
    def __init__(self, grid, docs, drop=None):
        names = grid.names
        col = lambda k: np.array([grid.cols[n][k] for n in names], float)
        self.y, self.yv, self.u, self.sp, self.tout, self.wind, self.rad = (col(k) for k in ("y", "yv", "u", "sp", "tout", "wind", "rad"))
        par = [RoomPar(docs[n]) for n in names]
        self.A = max([len(q.air) for q in par] + [0])
        ya = np.full((len(names), self.A, self.u.shape[1]), np.nan)
        for i, (n, q) in enumerate(zip(names, par)):
            for a, arr in enumerate(grid.cols[n]["ya"][:len(q.air)]):
                ya[i, a] = arr
        self.ya, self.co2, self.rh, self.th = ya, col("co2"), col("rh"), col("th")
        if drop is not None:
            hide = np.array([drop[n] for n in names], bool)
            self.y, self.yv, self.co2, self.rh = (np.where(hide, np.nan, v) for v in (self.y, self.yv, self.co2, self.rh))
            self.ya = np.where(hide[:, None, :], np.nan, self.ya)
        self.ah = abs_hum_np(self.th, self.rh)
        self.rate_co2, self.rate_ah = np.full(self.co2.shape, np.nan), np.full(self.co2.shape, np.nan)
        for i, q in enumerate(par):
            if q.co2:
                self.rate_co2[i] = rate_np(self.co2[i], int(q.co2["win_min"]))
            if q.hum:
                self.rate_ah[i] = rate_np(self.ah[i], int(q.hum["win_min"]))
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
        self.ue = np.array([valve_lag(self.u[i], RoomPar(docs[n]).alpha) for i, n in enumerate(names)]).reshape(self.u.shape)


class BatchHouse:
    def __init__(self, docs, names, shocks, house):
        self.names, self.house = names, house
        G, R = len(shocks), len(names)
        self.G, self.R = G, R
        par = [RoomPar(docs[n]) for n in names]
        vec = lambda f: np.array([f(p) for p in par], float)
        self.a1, self.a2, self.b1, self.b2, self.b3, self.b4, self.w, self.ts, self.ts_night = (vec(lambda p, k=k: getattr(p, k)) for k in ("a1", "a2", "b1", "b2", "b3", "b4", "w", "ts", "ts_night"))
        self.g, self.noise_r, self.p0, self.q_r, self.q_a, self.q_um, self.q_ua = (vec(lambda p, k=k: getattr(p, k)) for k in ("g", "noise_r", "p0", "q_r", "q_a", "q_um", "q_ua"))
        blk = lambda key, field: np.array([(getattr(p, key) or {}).get(field, 1.0) for p in par], float)
        self.has_eff = np.array([p.effort is not None for p in par])
        self.e_sigma, self.e_steady = blk("effort", "sigma"), blk("effort", "steady_u")
        A = max([len(p.air) for p in par] + [0])
        self.air_has, self.air_var, self.air_off = np.zeros((R, A), bool), np.ones((R, A)), np.zeros((R, A))
        for r, p in enumerate(par):
            for a, (var, off) in enumerate(p.air):
                self.air_has[r, a], self.air_var[r, a], self.air_off[r, a] = True, var, off
        self.gas = []
        for key, base in (("co2", "out"), ("hum", "rh_out")):
            self.gas.append({"has": np.array([getattr(p, key) is not None for p in par]), **{f: blk(key, f) for f in ("k", "gain", "sigma", "sigma_up", "min_excess", base)}, "base": base})
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

    def lam_h(self, c1, x):
        h = np.zeros(x.shape)
        h[..., 2] = c1
        return h

    def trv(self, x, p, h, y, yhat, noise_var, r1, grid, floor, mask):
        ph = np.einsum("...ij,...j->...i", p, h)
        s = np.einsum("...i,...i->...", h, ph) + noise_var
        pos = s > 0
        sd = np.sqrt(np.where(pos, s, 1.0))
        zlo, zhi = (y - 0.5 * grid - yhat) / sd, (y + 0.5 * grid - yhat) / sd
        z = bin_prob_np(zlo, zhi)
        ll = np.where(pos, np.log(np.maximum(z, P_FLOOR)), np.nan)
        pa, pb = np.exp(-0.5 * zlo * zlo) / SQRT2PI, np.exp(-0.5 * zhi * zhi) / SQRT2PI
        zs = np.where(z >= Z_MIN, z, 1.0)
        r = (pa - pb) / zs
        var_t = s * (1.0 + (zlo * pa - zhi * pb) / zs - r * r)
        tr = mask & pos & (z >= Z_MIN) & np.isfinite(var_t) & (var_t > 0.0)
        var_t = np.minimum(np.maximum(var_t, np.minimum(s, noise_var + floor * grid * grid / 12.0)), s)
        xt = x + (ph / np.where(pos, s, 1.0)[..., None]) * (sd * r)[..., None]
        c = (s - var_t) / (s * s)
        q = p - c[..., None, None] * ph[..., :, None] * ph[..., None, :]
        pt = 0.5 * (q + np.swapaxes(q, -1, -2))
        fb = mask & ~tr & pos
        xg, pg, _ = self.obs(x, p, h, np.where(fb, y - yhat, 0.0), r1, fb)
        return np.where(tr[..., None], xt, xg), np.where(tr[..., None, None], pt, pg), s, ll

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
        G, R = self.G, self.R
        x, p = self.x, self.p
        wv = self.w[None, :, None]
        noise_var = (self.noise_r ** 2)[None, :, None]
        r1 = hs["r_grid"] + noise_var
        grid = hs.get("trv_grid", 0.0)
        ones = np.ones(4, bool)
        mu0 = self.mu(self.w_prior)
        ybar = (mu0 * (wv * x[..., 0] + (1 - wv) * x[..., 1])).sum(-1)
        run4 = run[..., None] & ones
        ll = np.zeros((G, R, 4))
        hair = np.broadcast_to(np.array([0.0, 1.0, 0.0]), x.shape)
        for a in range(inp.A):
            ya = inp.ya[:, a, k]
            ma = run4 & (self.air_has[:, a] & np.isfinite(ya))[None, :, None]
            v = ya[None, :, None] - self.air_off[None, :, a, None] - x[..., 1]
            x, p, sa = self.obs(x, p, hair, np.where(ma, v, 0.0), self.air_var[None, :, a, None], ma)
            ll = ll + np.where(ma, lg_pdf_np(v, sa), 0.0)
        yhat = wv * x[..., 0] + (1 - wv) * x[..., 1]
        hy = np.broadcast_to(np.stack([self.w, 1 - self.w, np.zeros(self.R)], -1)[None, :, None, :], x.shape)
        ym = (run & y_ok)[..., None] & ones
        yb = y[None, :, None]
        if grid > 0.0:
            x, p, s1, lt = self.trv(x, p, hy, yb, yhat, noise_var, r1, grid, hs.get("trv_floor", 0.0), ym)
            ll = ll + np.where(ym, lt, 0.0)
        else:
            x, p, s1 = self.obs(x, p, hy, np.where(ym, yb - yhat, 0.0), r1, ym)
            ll = ll + np.where(ym, lg_pdf_np(yb - yhat, s1), 0.0)
        yv, sp = inp.yv[:, k], inp.sp[:, k]
        gate = run & self.has_eff[None, :] & np.isfinite(yv + sp)[None, :] & np.isfinite(ybar) & (yv > 0.0)[None, :] & (yv < 1.0)[None, :] & (self.g[None, :] * np.abs(sp[None, :] - ybar) <= self.e_steady[None, :])
        me = gate[..., None] & (self.st[None, None, :] > 0.5)
        tout, wind, rad, cn, cs_ = (v[:, k][None, :, None] for v in (inp.tout, inp.wind, inp.rad, inp.cn, inp.cs))
        ts = np.where(inp.night[:, k], self.ts_night, self.ts)[None, :, None]
        a1, a2, b1, b2, b3, b4 = (v[None, :, None] for v in (self.a1, self.a2, self.b1, self.b2, self.b3, self.b4))
        kq = a2 / (b1 * a1 * np.maximum(ts - x[..., 0], D_MIN))
        c0, c1 = kq * (cn * x[..., 1] - cs_ - b4 * rad / 100.0), kq * (b2 + b3 * wind) * np.maximum(x[..., 1] - tout, 0.0)
        v = yv[None, :, None] - (c0 + c1 * x[..., 2])
        x, p, se = self.obs(x, p, self.lam_h(c1, x), np.where(me, v, 0.0), (self.e_sigma ** 2)[None, :, None], me)
        ll = ll + np.where(me, lg_pdf_np(v, se), 0.0)
        for g, (level, rate, excess) in zip(self.gas, ((inp.co2[:, k], inp.rate_co2[:, k], inp.co2[:, k] - self.gas[0]["out"]), (inp.ah[:, k], inp.rate_ah[:, k], inp.ah[:, k] - abs_hum_np(inp.tout[:, k], self.gas[1]["rh_out"])))):
            ok_g = g["has"] & np.isfinite(excess) & np.isfinite(rate) & (excess >= g["min_excess"])
            mg = run4 & ok_g[None, :, None]
            ex = np.where(ok_g, excess, 0.0)[None, :, None]
            c0, c1 = -g["k"][None, :, None] * (1.0 - g["gain"][None, :, None]) * ex, -g["k"][None, :, None] * g["gain"][None, :, None] * ex
            v = np.where(ok_g, rate, 0.0)[None, :, None] - (c0 + c1 * x[..., 2])
            q = c1 * c1 * p[..., 2, 2]
            sg, su = g["sigma"][None, :, None], g["sigma_up"][None, :, None]
            s_lo, s_hi = q + sg * sg, q + su * su
            low = v < 0.0
            x, p, _ = self.obs(x, p, self.lam_h(c1, x), np.where(mg, v, 0.0), np.where(low, sg, su) ** 2, mg)
            ll = ll + np.where(mg, LOG_SQRT_2_OVER_PI - np.log(np.sqrt(s_lo) + np.sqrt(s_hi)) - 0.5 * v * v / np.where(low, s_lo, s_hi), 0.0)
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
        u, tout, wind, rad, cn, cs_ = (v[:, k][None, :, None] for v in (inp.ue, inp.tout, inp.wind, inp.rad, inp.cn, inp.cs))
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
        ug = u * self.st
        fp[..., 0, 0] += self.q_um[None, :, None] * ug * h
        fp[..., 1, 1] += self.q_ua[None, :, None] * ug * h
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
