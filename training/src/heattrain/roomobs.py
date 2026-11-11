import math

SQRT2 = math.sqrt(2.0)
SQRT2PI = math.sqrt(2.0 * math.pi)
P_FLOOR = 1e-300
Z_MIN = 1e-6
D_MIN = 1.0
EFFORT_KEYS = ("sigma", "steady_u")
GAS_KEYS = ("k", "gain", "sigma", "sigma_up", "min_excess", "win_min")
LOG_SQRT_2_OVER_PI = 0.5 * math.log(2.0 / math.pi)


def lg_pdf(v, s):
    return -0.5 * (v * v / s + math.log(2.0 * math.pi * s)) if s > 0 else math.nan


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


def erfc_nr(x):
    z = abs(x)
    t = 1.0 / (1.0 + 0.5 * z)
    r = t * math.exp(-z * z - 1.26551223 + t * (1.00002368 + t * (0.37409196 + t * (0.09678418 + t * (-0.18628806 + t * (0.27886807 + t * (-1.13520398 + t * (1.48851587 + t * (-0.82215223 + t * 0.17087277)))))))))
    return r if x >= 0 else 2.0 - r


def norm_sf(z):
    return 0.5 * erfc_nr(z / SQRT2)


def norm_pdf(z):
    return math.exp(-0.5 * z * z) / SQRT2PI


def bin_prob(zlo, zhi):
    if zlo >= 0.0:
        return norm_sf(zlo) - norm_sf(zhi)
    if zhi <= 0.0:
        return norm_sf(-zhi) - norm_sf(-zlo)
    return 1.0 - norm_sf(zhi) - norm_sf(-zlo)


def abs_hum(t, rh):
    if not (math.isfinite(t) and math.isfinite(rh)) or t <= -40.0:
        return math.nan
    return 216.7 * (rh / 100.0 * 6.112 * math.exp(17.62 * t / (243.12 + t))) / (273.15 + t)


def trv_update(x, p, hrow, y, yhat, noise_var, grid, floor, fallback_var):
    ph = [p[3 * i] * hrow[0] + p[3 * i + 1] * hrow[1] + p[3 * i + 2] * hrow[2] for i in range(3)]
    s = hrow[0] * ph[0] + hrow[1] * ph[1] + hrow[2] * ph[2] + noise_var
    if not s > 0:
        return math.nan, s
    sd = math.sqrt(s)
    zlo, zhi = (y - 0.5 * grid - yhat) / sd, (y + 0.5 * grid - yhat) / sd
    z = bin_prob(zlo, zhi)
    ll = math.log(max(z, P_FLOOR))
    if z >= Z_MIN:
        pa, pb = norm_pdf(zlo), norm_pdf(zhi)
        r = (pa - pb) / z
        var_t = s * (1.0 + (zlo * pa - zhi * pb) / z - r * r)
        if math.isfinite(var_t) and var_t > 0.0:
            var_t = min(max(var_t, min(s, noise_var + floor * grid * grid / 12.0)), s)
            shift = sd * r
            for i in range(3):
                x[i] += ph[i] / s * shift
            c = (s - var_t) / (s * s)
            q = [p[3 * i + j] - c * ph[i] * ph[j] for i in range(3) for j in range(3)]
            p[:] = [0.5 * (q[3 * i + j] + q[3 * j + i]) for i in range(3) for j in range(3)]
            return ll, s
    obs_update(x, p, hrow, y - yhat, fallback_var)
    return ll, s


def effort_coeffs(par, m, a, ts, wind, tout, rad, cn, cs):
    amb = par.b2 + par.b3 * wind
    k = par.a2 / (par.b1 * par.a1 * max(ts - m, D_MIN))
    return k * (cn * a - cs - par.b4 * rad / 100.0), k * amb * max(a - tout, 0.0)


def gas_coeffs(k, gain, excess):
    return -k * (1.0 - gain) * excess, -k * gain * excess


def lam_update(x, p, c0, c1, obs, sigma):
    v = obs - (c0 + c1 * x[2])
    return lg_pdf(v, obs_update(x, p, (0.0, 0.0, c1), v, sigma * sigma))


def gas_update(x, p, c0, c1, obs, sigma, sigma_up):
    v = obs - (c0 + c1 * x[2])
    q = c1 * c1 * p[8]
    s_lo, s_hi = q + sigma * sigma, q + sigma_up * sigma_up
    obs_update(x, p, (0.0, 0.0, c1), v, (sigma if v < 0.0 else sigma_up) ** 2)
    return LOG_SQRT_2_OVER_PI - math.log(math.sqrt(s_lo) + math.sqrt(s_hi)) - 0.5 * v * v / (s_lo if v < 0.0 else s_hi)


def finite_positive(*xs):
    return all(isinstance(v, (int, float)) and math.isfinite(v) and v > 0 for v in xs)


def finite_nonneg(*xs):
    return all(isinstance(v, (int, float)) and math.isfinite(v) and v >= 0 for v in xs)


def evidence_blocks(ext, ev):
    nodes = (ext or {}).get("nodes", {})
    out = {"effort": dict(ev["effort"])} if ev.get("effort") else {}
    if nodes.get("air"):
        out["air"] = [{"node": n, **ev["air"]} for n in nodes["air"]]
    for key in ("co2", "hum"):
        if nodes.get(key) is not None:
            out[key] = {**ev[key], "node": nodes[key]}
    return out


def check_room_doc(name, doc):
    bad = lambda what: ValueError(f"room model: invalid {what} for {name}")
    for a in doc.get("air") or []:
        if not (finite_positive(a.get("noise")) and math.isfinite(a.get("offset", 0.0))):
            raise bad("air sensor")
    e = doc.get("effort")
    if e is not None and not (set(EFFORT_KEYS) <= set(e) and finite_positive(e["sigma"], e["steady_u"])):
        raise bad("effort evidence")
    for key, extra in (("co2", ("out",)), ("hum", ("rh_out",))):
        g = doc.get(key)
        if g is None:
            continue
        ok = set(GAS_KEYS + extra) <= set(g) and finite_positive(g["sigma"], g["sigma_up"], g["win_min"]) and finite_nonneg(g["k"], g["gain"], g["min_excess"], g[extra[0]]) and float(g["win_min"]).is_integer()
        if not ok or (key == "hum" and g["rh_out"] > 100.0):
            raise bad(f"{key} evidence")


def check_house(house):
    if not finite_nonneg(house.get("trv_grid", 0.0), house.get("trv_floor", 0.0)):
        raise ValueError("room model: invalid trv_grid or trv_floor")
