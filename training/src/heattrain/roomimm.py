import math
from collections import deque
from dataclasses import dataclass
from heattrain.roompcnn import Pcnn, feature_row
from heattrain.roomobs import abs_hum, check_house, check_room_doc, effort_coeffs, gas_coeffs, gas_update, lam_update, lg_pdf, obs_update, trv_update

SQRT2 = math.sqrt(2.0)
HORIZON = 60
PROBES = (10, 20, 30)
COAST_CAP = 1440
COAST_DROP = 0.5
BOOSTS = (5, 10, 15, 20)
FORECAST_MIN = 20
PEAK_MARGIN = 0.5
LAMBDA_BOUNDS = (0.5, 8.0)
EPS = 1e-9


def erf_as(x):
    t = 1.0 / (1.0 + 0.3275911 * abs(x))
    y = 1.0 - (((((1.061405429 * t - 1.453152027) * t) + 1.421413741) * t - 0.284496736) * t + 0.254829592) * t * math.exp(-x * x)
    return y if x >= 0 else -y


def norm_cdf(z):
    return 0.5 * (1.0 + erf_as(z / SQRT2))


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
        self.g, self.noise_r, self.p0, self.q_r, self.q_a = (doc[k] for k in ("valve_gain", "noise_r", "p0", "q_r", "q_a"))
        self.q_um, self.q_ua = doc.get("q_um", 0.0), doc.get("q_ua", 0.0)
        self.unusable = bool(doc.get("unusable", False))
        self.tau_v = p.get("tau_v", 0.0)
        self.alpha = 1.0 - math.exp(-1.0 / self.tau_v) if self.tau_v > 0 else 1.0
        check_room_doc("room", doc)
        self.air = [(a["noise"] ** 2, a.get("offset", 0.0)) for a in doc.get("air") or []]
        self.effort, self.co2, self.hum = doc.get("effort"), doc.get("co2"), doc.get("hum")
        self.pcnn = Pcnn(doc["pcnn"], "room") if doc.get("pcnn") else None

    def lag(self, state, u):
        if state is None or self.alpha == 1.0:
            return [u, u]
        v1 = state[0] + self.alpha * (u - state[0])
        return [v1, state[1] + self.alpha * (v1 - state[1])]

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
    ya: tuple = ()
    co2: float = math.nan
    rh: float = math.nan
    th: float = math.nan


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
        self.lag = None
        self.hist = {k: deque(maxlen=int(g["win_min"]) + 1) for k, g in (("co2", par.co2), ("ah", par.hum)) if g}

    def rate(self, key):
        h = self.hist.get(key)
        if h is None or len(h) < h.maxlen or not (math.isfinite(h[0]) and math.isfinite(h[-1])):
            return math.nan
        return (h[-1] - h[0]) / (h.maxlen - 1)

    def gas_evidence(self, inp):
        par, out = self.par, []
        if par.co2:
            excess, r = inp.co2 - par.co2["out"], self.rate("co2")
            if finite(excess, r) and excess >= par.co2["min_excess"]:
                out.append((excess, r, par.co2))
        if par.hum:
            excess, r = abs_hum(inp.th, inp.rh) - abs_hum(inp.tout, par.hum["rh_out"]), self.rate("ah")
            if finite(excess, r) and excess >= par.hum["min_excess"]:
                out.append((excess, r, par.hum))
        return out

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
        if math.isfinite(inp.u):
            self.lag = par.lag(self.lag, inp.u)
        for key, v in (("co2", inp.co2), ("ah", abs_hum(inp.th, inp.rh))):
            if key in self.hist:
                self.hist[key].append(v)
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
        grid, noise_var = self.house.get("trv_grid", 0.0), par.noise_r ** 2
        r1 = self.house["r_grid"] + noise_var
        cn, cs = par.coupling(inp.ncnt, inp.nsum)
        mu0 = self.weights(w)
        ybar = sum(mu0[j] * par.sensor(self.x[j][0], self.x[j][1]) for j in range(4))
        eff = par.effort if par.effort and finite(inp.yv, inp.sp, ybar) and 0.0 < inp.yv < 1.0 and par.g * abs(inp.sp - ybar) <= par.effort["steady_u"] else None
        gas = self.gas_evidence(inp)
        ts = par.ts_night if inp.night else par.ts
        ll = [0.0] * 4
        yhat = [0.0] * 4
        s1 = [0.0] * 4
        for j in range(4):
            x, p = self.x[j], self.p[j]
            for (var, off), ya in zip(par.air, inp.ya):
                if math.isfinite(ya):
                    v = ya - off - x[1]
                    ll[j] += lg_pdf(v, obs_update(x, p, (0.0, 1.0, 0.0), v, var))
            if y_ok:
                yhat[j] = par.sensor(x[0], x[1])
                if grid > 0.0:
                    l, s1[j] = trv_update(x, p, (par.w, 1 - par.w, 0.0), inp.y, yhat[j], noise_var, grid, self.house.get("trv_floor", 0.0), r1)
                    ll[j] += l
                else:
                    s1[j] = obs_update(x, p, (par.w, 1 - par.w, 0.0), inp.y - yhat[j], r1)
                    ll[j] += lg_pdf(inp.y - yhat[j], s1[j])
            if eff:
                if j >= 2:
                    c0, c1 = effort_coeffs(par, x[0], x[1], ts, inp.wind, inp.tout, inp.rad, cn, cs)
                    ll[j] += lam_update(x, p, c0, c1, inp.yv, eff["sigma"])
            for excess, rate, g in gas:
                c0, c1 = gas_coeffs(g["k"], g["gain"], excess)
                ll[j] += gas_update(x, p, c0, c1, rate, g["sigma"], g["sigma_up"])
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
                for j in range(4):
                    self.x[j][2] = min(max(self.x[j][2], LAMBDA_BOUNDS[0]), LAMBDA_BOUNDS[1])
                bad = "innovation" if self.hot >= self.house["reset_n"] else None
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
            f11, f12, f21, f22, g1, g2, amb = par.terms(xb[2], self.lag[1] * st, inp.tout, inp.wind, inp.rad, inp.night, cn, cs, h)
            fm = [f11, f12, 0.0, f21, f22, h * amb * (inp.tout - xb[1]), 0.0, 0.0, self.phi]
            fp = mat_mul(mat_mul(fm, pb), mat_t(fm))
            ug = self.lag[1] * st
            fp[0] += (par.q_r + par.q_um * ug) * h
            fp[4] += (par.q_a + par.q_ua * ug) * h
            fp[8] += self.q_l[mt]
            new_x.append([f11 * xb[0] + f12 * xb[1] + g1, f21 * xb[0] + f22 * xb[1] + g2, 1.0 + self.phi * (xb[2] - 1.0)])
            new_p.append([0.5 * (fp[3 * i + k] + fp[3 * k + i]) for i in range(3) for k in range(3)])
        for s in range(2):
            t = new_nu[s][0] + new_nu[s][1]
            new_nu[s] = [new_nu[s][0] / t, new_nu[s][1] / t] if t > 0 else [1.0, 0.0]
        self.x, self.p, self.nu = new_x, new_p, new_nu


def pcnn_q(filt, inp, hour, weekend, present, coffee):
    net = filt.par.pcnn
    return None if net is None else net.q(feature_row(hour, weekend, inp.rad, inp.tout, present, filt.rate("co2"), filt.rate("ah"), coffee))


class House:
    def __init__(self, rooms, shock, house):
        check_house(house)
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


def recovery_rate(par, m, a, lam, s, env):
    h = 1.0 / 60.0
    start = par.sensor(m, a)
    for _ in range(FORECAST_MIN):
        f11, f12, f21, f22, g1, g2, _ = par.terms(lam, s, env["tout"], env["wind"], env["rad"], env["night"], env["cn"], env["cs"], h)
        m, a = f11 * m + f12 * a + g1, f21 * m + f22 * a + g2
    return (par.sensor(m, a) - start) / FORECAST_MIN


def terminal_cold(par, m, a, lam, env):
    deficit = max(0.0, env["target"] - par.sensor(m, a))
    if deficit <= 0.0 or env["present"] <= 0.0:
        return 0.0
    rate = recovery_rate(par, m, a, lam, 1.0, env)
    return 0.5 * deficit * min(deficit / rate if rate > 1e-12 else COAST_CAP, COAST_CAP) * env["present"]


def valve_demand(par, env, sens0, setpoint, sens):
    u0 = env.get("u_now")
    if u0 is None or not math.isfinite(u0):
        return min(max(par.g * (setpoint - sens), 0.0), 1.0)
    anchor = env["sp_cur"] if env["sp_cur"] is not None else env["sp_live"]
    return min(max(u0 + par.g * ((setpoint - sens) - (anchor - sens0)), 0.0), 1.0)


def rollout(par, m, a, lam, s, sp, env):
    h = 1.0 / 60.0
    en, co, sens = [], [], []
    lag = env["lag"]
    sens0 = par.sensor(m, a)
    hq = h * env.get("q", 0.0)
    for k in range(HORIZON):
        u = valve_demand(par, env, sens0, sp[k], par.sensor(m, a))
        lag = par.lag(lag, u)
        f11, f12, f21, f22, g1, g2, _ = par.terms(lam, lag[1] * s, env["tout"], env["wind"], env["rad"], env["night"], env["cn"], env["cs"], h)
        m, a = f11 * m + f12 * a + g1, f21 * m + f22 * a + g2 + hq
        sens.append(par.sensor(m, a))
        en.append(u * s)
        co.append(max(0.0, env["target"] - sens[-1]) * env["present"])
    return en, co, sens, terminal_cold(par, m, a, lam, env)


def profile(first, d, then):
    return [first] * d + [then] * (HORIZON - d)


def storage(par, m, a, lam, env):
    h = 1.0 / 60.0
    cn, cs = env["cn"], env["cs"]
    amb = par.b2 + par.b3 * env["wind"]
    loss = lam * amb + cn
    drive = lam * amb * env["tout"] + par.b4 * env["rad"] / 100.0 + cs
    w = par.w
    m_eq = (env["target"] - (1.0 - w) * drive / (par.b1 + loss)) / (w + (1.0 - w) * par.b1 / (par.b1 + loss))
    a_eq = (par.b1 * m_eq + drive) / (par.b1 + loss)
    ts = par.ts_night if env["night"] else par.ts
    stored = None
    if ts - m_eq > 0.5 and par.a1 > 0.0:
        stored = ((m - m_eq) + par.a2 / par.b1 * (a - a_eq)) / (par.a1 * (ts - m_eq) / 60.0)
    floor = env["target"] - COAST_DROP
    coast = COAST_CAP
    x, y = m, a
    f11, f12, f21, f22, g1, g2, _ = par.terms(lam, 0.0, env["tout"], env["wind"], env["rad"], env["night"], cn, cs, h)
    for k in range(COAST_CAP):
        if y < floor:
            coast = k
            break
        x, y = f11 * x + f12 * y + g1, f21 * x + f22 * y + g2
    return float(coast), stored


def separation_minutes(pr, sigma_obs):
    closed, opened = pr["runs"][0][0][2], pr["runs"][1][0][2]
    return next((k + 1 for k in range(HORIZON) if abs(opened[k] - closed[k]) >= 2.0 * sigma_obs), HORIZON)


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
    q_net = env.get("q_net")
    net_on = bool(env.get("pcnn_active")) and q_net is not None
    env["q"] = q_net if net_on else 0.0
    env["cn"], env["cs"] = par.coupling(env["ncnt"], env["nsum"])
    env["lag"] = list(env["lag"]) if env.get("lag") is not None else None
    modes = ((1.0 - pw, lam_c), (pw, lam_o))
    supplies = ((1, p_supply), (0, 1.0 - p_supply))
    calm = (1.0, 0.0)
    target, frost, cur, cold_w = env["target"], env["frost"], env["sp_cur"], env["cold_cost"]
    sensor0 = par.sensor(m, a)
    limit = max(target + PEAK_MARGIN, sensor0) + EPS
    memo, now = env.get("probe"), env.get("now_min", 0)

    def evaluate(name, kind, sp, first, **meta):
        runs = [[rollout(par, m, a, lam, s, sp, env) for s, _ in supplies] for _, lam in modes]
        cold_of = lambda r: sum(r[1]) + r[3]
        mode_cost = [sum(supplies[si][1] * (sum(r[si][0]) + calm[mi] * cold_w * cold_of(r[si])) for si in range(2)) for mi, r in enumerate(runs)]
        rz = cur is not None and first > cur + EPS
        raise_c = env["raise_cost"] if rz else 0.0
        energy = sum(modes[mi][0] * supplies[si][1] * sum(runs[mi][si][0]) for mi in range(2) for si in range(2))
        cold = sum(modes[0][0] * supplies[si][1] * cold_of(runs[0][si]) for si in range(2))
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
    back = target if best["kind"] == "boost" else best["first"]
    full = lambda c: c["total"] - c["raise"]
    unusable = par.unusable or (par.ts_night if env["night"] else par.ts) < target + PEAK_MARGIN
    running = memo is not None and not memo["ended"] and now < memo["start"] + memo["d"]
    event, probed, new_memo = None, 0, memo
    if memo is not None and not memo["ended"]:
        if running and (abs(memo["target"] - target) > EPS or abs(memo["present"] - env["present"]) > EPS or gain <= env["raise_cost"] or unusable):
            event, probed, new_memo, running = "abort", now - memo["start"], dict(memo, d=now - memo["start"], ended=True), False
        elif not running:
            event, probed, new_memo = "end", memo["d"], dict(memo, ended=True)
    chosen, evoi_best = best, 0.0
    if running:
        left = memo["start"] + memo["d"] - now
        chosen = evaluate(f"close{left}", "probe", profile(frost, left, back), frost, probe_min=left)
    elif not unusable and (new_memo is None or now >= new_memo["block_until"]):
        for d in PROBES if best["kind"] != "frost" and back > frost + EPS else ():
            pr = evaluate(f"close{d}", "probe", profile(frost, d, back), frost, probe_min=d)
            if pr["peak"] > limit:
                continue
            sep = abs(pr["runs"][1][0][2][d - 1] - pr["runs"][0][0][2][d - 1]) / sigma_obs
            rho = 1.0 - 2.0 * norm_cdf(-sep / 2.0)
            evoi = rho * gain - max(0.0, full(pr) - full(best))
            if evoi > max(evoi_best, env["raise_cost"]) + 1e-12:
                chosen, evoi_best = pr, evoi
                new_memo = {"start": now, "d": d, "block_until": now + 2 * d + separation_minutes(pr, sigma_obs), "target": target, "present": env["present"], "ended": False}
                event = "start"
    y_last = env.get("y_last")
    anchor = y_last - sensor0 if y_last is not None and math.isfinite(y_last) else 0.0
    live_sp = profile(live["first"], HORIZON, live["first"])

    def forecast_with(q):
        e = dict(env, q=q)
        return anchor + sum(modes[mi][0] * supplies[si][1] * rollout(par, m, a, lam, s, live_sp, e)[2][FORECAST_MIN - 1] for mi, (_, lam) in enumerate(modes) for si, (s, _) in enumerate(supplies))

    fc_phys = forecast_with(0.0)
    fc_pcnn = forecast_with(q_net) if q_net is not None else math.nan
    fc = fc_pcnn if net_on else fc_phys
    coast, stored = storage(par, m, a, lam_c, env)
    if unusable:
        chosen = live
    return {"coast_min": coast, "stored_min": stored, "kind": "fit_unusable" if unusable else chosen["kind"], "name": "fit_unusable" if unusable else chosen["name"], "setpoint": chosen["first"], "raise": False if unusable else chosen["raise_flag"],
            "probe_min": chosen["probe_min"], "boost_min": chosen["boost_min"], "energy": chosen["energy"], "cold": chosen["cold"], "raise_cost": 0.0 if unusable else chosen["raise"], "cost_live": live["total"], "total": chosen["total"],
            "p_window": pw, "lam_closed": lam_c, "lam_open": lam_o, "mass": m, "air": a, "sensor": sensor0, "forecast": fc, "forecast_phys": fc_phys, "forecast_pcnn": fc_pcnn, "pcnn_active": net_on, "evoi": evoi_best, "best_cost": best["total"], "perfect_cost": perf, "n_allowed": len(allowed),
            "probe_event": event, "probed_min": probed, "probe_memo": new_memo, "unusable": unusable}
