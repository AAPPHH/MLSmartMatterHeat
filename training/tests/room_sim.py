import math
import numpy as np
from heattrain.roomimm import RoomIn, RoomPar
from heattrain.roomobs import abs_hum

TRUE = {"a1": 6.0, "a2": 2.0, "b1": 1.0, "b2": 0.15, "b3": 0.03, "b4": 0.01, "w": 0.7, "ts": 50.0, "ts_night": 50.0}
SHOCK = {"tau_l": 90.0, "q_calm": 0.01, "q_jump": 0.3, "p_jump": 1e-3, "hold_min": 20.0, "lambda_jump": 3.0, "lambda_w": 2.0, "p_l0": 0.01}
HOUSE = {"p_up": 0.02, "p_down": 0.001, "p_supply0": 0.95, "r_grid": 0.25 / 12.0, "reset_gap_min": 180.0, "reset_sigma": 8.0, "reset_n": 3, "boost_c": 24.0, "balance_seed": 1, "trv_grid": 0.5, "trv_floor": 0.25}
NAMES = ("wohnzimmer", "kueche", "schlafzimmer")
EFFORT = {"sigma": 0.06, "steady_u": 0.05}
CO2 = {"out": 420.0, "min_excess": 150.0, "k": 0.004, "gain": 4.0, "sigma": 4.0, "sigma_up": 12.0, "win_min": 10}
HUM = {"rh_out": 85.0, "min_excess": 0.8, "k": 0.004, "gain": 4.0, "sigma": 0.03, "sigma_up": 0.3, "win_min": 10}
AIR = {"noise": 0.1, "offset": 0.0}


def room_doc(coupling=True, gain=0.5, **params):
    return {"params": {**TRUE, **params}, "c": {"wall": 0.2} if coupling else {}, "valve_gain": gain, "sigma_u": 0.25, "noise_r": 0.15, "p0": 1.0, "q_r": 0.5, "q_a": 0.05, "effort": dict(EFFORT)}


def rh_of(ah, t):
    return ah * (273.15 + t) / (216.7 * 6.112 * math.exp(17.62 * t / (243.12 + t))) * 100.0


def simulate_house(n, seed=0, window=None, stop=None, gap=None, corrupt=None, coupling=True, report_p=0.3, grid=0.5, gain=0.5, room_over=None, pi=None, air=None, gas=None, sp_fn=None, **params):
    rng = np.random.default_rng(seed)
    rng2 = np.random.default_rng(seed + 1000)
    air, gas = air or {}, gas or {}
    integ = dict.fromkeys(NAMES, 0.0)
    held = {name: [math.nan] * 3 for name in NAMES}
    level = {name: [g.get("c0", 420.0), g.get("ah0", 7.0)] for name, g in gas.items()}
    docs = {name: room_doc(coupling, gain, **params) for name in NAMES}
    for name, over in (room_over or {}).items():
        docs[name]["params"].update(over.get("params", {}))
        docs[name].update({k: v for k, v in over.items() if k != "params"})
    for name in air:
        docs[name]["air"] = [dict(AIR, **air[name].get("sensor", {}))]
    for name, g in gas.items():
        docs[name].update(co2=dict(CO2), hum=dict(HUM))
    lag = dict.fromkeys(NAMES)
    pars = {name: RoomPar(docs[name]) for name in NAMES}
    h = 1.0 / 60.0
    m = {name: 22.5 - i for i, name in enumerate(NAMES)}
    a = {name: 19.0 + 0.0 * i for i, name in enumerate(NAMES)}
    inputs, truth = [], {name: {"m": [], "a": [], "lam": [], "s": []} for name in NAMES}
    sensor = {name: pars[name].sensor(m[name], a[name]) for name in NAMES}
    for k in range(n):
        tout = 5.0 + 2.0 * math.sin(k / 300.0)
        s = 0 if stop and stop[0] <= k < stop[1] else 1
        row = {}
        new_sensor = {}
        for i, name in enumerate(NAMES):
            par = pars[name]
            lam = window[name][2] if window and name in window and window[name][0] <= k < window[name][1] else 1.0
            sp = sp_fn(k) if sp_fn else 21.0 if (k // 240) % 2 == 0 else 19.0
            u = float(np.clip(par.g * (sp - sensor[name]) + integ[name] + rng.normal(0, 0.03), 0.0, 1.0))
            if pi:
                integ[name] = float(np.clip(integ[name] + pi * (sp - sensor[name]) / 60.0, 0.0, 1.0))
            nb = sensor[NAMES[(i + 1) % 3]]
            ncnt, nsum = ((1.0,), (nb,)) if coupling else ((), ())
            cn, cs = par.coupling(ncnt, nsum)
            truth[name]["m"].append(m[name])
            truth[name]["a"].append(a[name])
            truth[name]["lam"].append(lam)
            truth[name]["s"].append(s)
            y = yv = math.nan
            if rng.random() < report_p:
                y = round((sensor[name] + rng.normal(0, 0.05)) / grid) * grid if grid else sensor[name] + rng.normal(0, 0.05)
            if 0.0 < u < 1.0 and rng.random() < report_p:
                yv = u
            if gap and gap[0] <= k < gap[1]:
                y = yv = math.nan
            if corrupt and name == NAMES[0] and corrupt[0] <= k < corrupt[0] + corrupt[1] and math.isfinite(y):
                y += corrupt[2]
            ya = ()
            if name in air:
                ya = (round(a[name] + docs[name]["air"][0]["offset"] + rng2.normal(0, docs[name]["air"][0]["noise"]), 2) if rng2.random() < air[name].get("p", 0.5) else math.nan,)
            if name in gas:
                c, ah = level[name]
                g = gas[name]
                src = lambda key: sum(r for a0, a1, r in g.get(key, ()) if a0 <= k < a1)
                c += src("co2_src") - CO2["k"] * (1 + CO2["gain"] * (lam - 1)) * (c - CO2["out"]) + rng2.normal(0, 0.5)
                ah += src("ah_src") - HUM["k"] * (1 + HUM["gain"] * (lam - 1)) * (ah - abs_hum(tout, HUM["rh_out"])) + rng2.normal(0, 0.003)
                level[name] = [c, ah]
                if k % 2 == 0:
                    held[name] = [round(c + rng2.normal(0, 1.0)), round(rh_of(ah, a[name]) + rng2.normal(0, 0.3), 1), round(a[name], 2)]
            row[name] = RoomIn(y, yv, u, sp, tout, 3.0, 0.0, False, ncnt, nsum, ya, *held[name])
            lag[name] = par.lag(lag[name], u)
            f11, f12, f21, f22, g1, g2, _ = par.terms(lam, lag[name][1] * s, tout, 3.0, 0.0, False, cn, cs, h)
            m[name], a[name] = f11 * m[name] + f12 * a[name] + g1 + rng.normal(0, math.sqrt(par.q_r * h)), f21 * m[name] + f22 * a[name] + g2 + rng.normal(0, math.sqrt(par.q_a * h))
            new_sensor[name] = par.sensor(m[name], a[name])
        sensor = new_sensor
        inputs.append(row)
    return docs, inputs, truth
