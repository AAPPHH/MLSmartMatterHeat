import math
import numpy as np
from heattrain.thermal import RoomIn, RoomPar

TRUE = {"a1": 6.0, "a2": 2.0, "b1": 1.0, "b2": 0.15, "b3": 0.03, "b4": 0.01, "w": 0.7, "ts": 50.0, "ts_night": 50.0}
SHOCK = {"tau_l": 90.0, "q_calm": 0.01, "q_jump": 0.3, "p_jump": 1e-3, "hold_min": 20.0, "lambda_jump": 3.0, "lambda_w": 2.0, "p_l0": 0.01}
HOUSE = {"p_up": 0.02, "p_down": 0.001, "p_supply0": 0.95, "r_grid": 0.25 / 12.0, "reset_gap_min": 180.0, "reset_sigma": 8.0, "reset_n": 3, "boost_c": 24.0, "balance_seed": 1}
NAMES = ("wohnzimmer", "kueche", "schlafzimmer")


def room_doc(coupling=True, gain=0.5, **params):
    return {"params": {**TRUE, **params}, "c": {"wall": 0.2} if coupling else {}, "valve_gain": gain, "sigma_u": 0.25, "noise_r": 0.15, "p0": 1.0, "q_r": 0.5, "q_a": 0.05}


def simulate_house(n, seed=0, window=None, stop=None, gap=None, corrupt=None, coupling=True, report_p=0.3, grid=0.5, gain=0.5, **params):
    rng = np.random.default_rng(seed)
    docs = {name: room_doc(coupling, gain, **params) for name in NAMES}
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
            sp = 21.0 if (k // 240) % 2 == 0 else 19.0
            u = float(np.clip(par.g * (sp - sensor[name]) + rng.normal(0, 0.03), 0.0, 1.0))
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
            row[name] = RoomIn(y, yv, u, sp, tout, 3.0, 0.0, False, ncnt, nsum)
            f11, f12, f21, f22, g1, g2, _ = par.terms(lam, u * s, tout, 3.0, 0.0, False, cn, cs, h)
            m[name], a[name] = f11 * m[name] + f12 * a[name] + g1 + rng.normal(0, math.sqrt(par.q_r * h)), f21 * m[name] + f22 * a[name] + g2 + rng.normal(0, math.sqrt(par.q_a * h))
            new_sensor[name] = par.sensor(m[name], a[name])
        sensor = new_sensor
        inputs.append(row)
    return docs, inputs, truth
