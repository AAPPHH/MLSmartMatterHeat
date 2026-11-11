import json
import math
from pathlib import Path
from heattrain.roomimm import House, RoomIn, RoomPar, plan
from heattrain.roomobs import abs_hum, bin_prob, obs_update, trv_update
from room_sim import HOUSE, NAMES, SHOCK, simulate_house

OUT = Path(__file__).resolve().parents[2] / "crates" / "heatd" / "testdata" / "room_model_parity.json"
STEPS = 1500
PLAN_FROM, PLAN_EVERY = 60, 10
GAS = {"schlafzimmer": {"c0": 1100.0, "co2_src": [(0, 2000, 3.0)], "ah0": 8.0, "ah_src": [(250, 262, 0.25)]}, "kueche": {"c0": 600.0, "ah0": 8.0, "ah_src": [(1100, 1112, 0.25)]}}
SCENARIO = {"window": {"kueche": (400, 520, 4.0), "schlafzimmer": (1000, 1080, 4.0)}, "stop": (700, 820), "gap": (950, 1250), "corrupt": (1350, 40, 12.0), "gain": 0.3, "w": 0.3, "b1": 2.0, "a1": 3.0, "ts": 35.0, "ts_night": 35.0,
            "air": {"kueche": {"p": 0.5}}, "gas": GAS, "pi": 3.0,
            "room_over": {"wohnzimmer": {"params": {"tau_v": 8.0}, "q_um": 4.0, "q_ua": 0.5}, "schlafzimmer": {"params": {"tau_v": 15.0, "w": 0.0}}}}
SATURATED = {"kueche": [(800, 840, 1.0), (860, 880, 0.0)]}
AIR_DROPOUT = ("kueche", 300, 450)
OUTPUT_KEYS = ("m", "a", "lam", "lam_std", "p_window", "p_supply")
PLAN_KEYS = ("name", "kind", "setpoint", "raise", "probe_min", "boost_min", "energy", "cold", "raise_cost", "cost_live", "total", "p_window", "lam_closed", "lam_open", "mass", "air", "sensor", "forecast", "evoi", "n_allowed")


def clean(x):
    if isinstance(x, float):
        return None if math.isnan(x) else round(x, 6)
    if isinstance(x, (list, tuple)):
        return [clean(v) for v in x]
    return float(x) if hasattr(x, "item") else x


def rounded(row):
    r6 = lambda v: round(float(v), 6)
    return {n: RoomIn(*(r6(getattr(r, f)) for f in ("y", "yv", "u", "sp", "tout", "wind", "rad")), r.night, tuple(r6(v) for v in r.ncnt), tuple(r6(v) for v in r.nsum), tuple(r6(v) for v in r.ya), r6(r.co2), r6(r.rh), r6(r.th)) for n, r in row.items()}


def edit(k, row):
    for room, spans in SATURATED.items():
        for a, b, value in spans:
            if a <= k < b and math.isfinite(row[room].yv):
                row[room].yv = value
    room, a, b = AIR_DROPOUT
    if a <= k < b:
        row[room].ya = (math.nan,)
    return row


def checks():
    h, noise = (0.3, 0.7, 0.0), 0.15 ** 2
    cases = []
    for y, yhat, p0 in ((20.0, 20.1, 1.0), (20.0, 19.0, 0.05), (20.0, 14.0, 0.05), (20.5, 20.0, 0.01), (20.0, 20.0, 0.0004)):
        x, p = [yhat, yhat, 1.0], [p0, 0.01, 0.0, 0.01, p0, 0.0, 0.0, 0.0, 0.02]
        ll, s = trv_update(x, p, h, y, yhat, noise, 0.5, 0.25, 0.5 ** 2 / 12 + noise)
        cases.append({"y": y, "yhat": yhat, "p0": p0, "ll": ll, "s": s, "x": x, "p": p})
    return {"trv": cases, "bin": [[a, b, bin_prob(a, b)] for a, b in ((-1.0, 1.0), (2.0, 3.0), (-3.0, -2.0), (8.0, 9.0), (40.0, 41.0), (-0.3, 2.5))], "abs_hum": [[t, rh, abs_hum(t, rh)] for t, rh in ((20.0, 50.0), (-5.0, 85.0), (35.0, 10.0))]}


def plan_env(i, k):
    sp = i.sp
    v = (k // 10) % 3
    return {"tout": i.tout, "wind": i.wind, "rad": i.rad, "night": i.night, "ncnt": i.ncnt, "nsum": i.nsum, "target": 21.0 if sp > 20 else 19.0, "present": 1.0, "sp_cur": sp - (0.0, 1.0, 0.0)[v],
            "sp_live": sp if k % 20 else sp + 0.5, "frost": 7.0, "boost_c": 24.0, "boost_ok": k % 3 == 0, "raise_cost": 15.0, "cold_cost": (1.0, 0.1, 0.3)[v], "r_grid": HOUSE["r_grid"]}


def last_report(inputs, n, k):
    return next((inputs[j][n].y for j in range(k, max(k - 30, -1), -1) if math.isfinite(inputs[j][n].y)), None)


def build():
    docs, raw, _ = simulate_house(STEPS, seed=7, **SCENARIO)
    inputs = [rounded(edit(k, r)) for k, r in enumerate(raw)]
    h = House(docs, SHOCK, HOUSE)
    expected = {n: {k: [] for k in ("ok",) + OUTPUT_KEYS} for n in NAMES}
    plans = []
    for k, row in enumerate(inputs):
        outs = h.step(row)
        for n, o in outs.items():
            expected[n]["ok"].append(bool(o["ok"]))
            for key in OUTPUT_KEYS:
                expected[n][key].append(clean(o[key]) if o["ok"] else None)
            if o["ok"] and k >= PLAN_FROM and k % PLAN_EVERY == 0:
                env = clean_env({**plan_env(row[n], k), "lag": h.rooms[n].lag, "y_last": last_report(inputs, n, k) if k % 20 else None})
                p = plan(RoomPar(docs[n]), SHOCK, h.rooms[n].comps, h.w[1], env)
                plans.append({"step": k, "room": n, "env": env, "out": {key: clean(p[key]) for key in PLAN_KEYS}})
    bundle_rooms = {n: {"node": 5 + i, "struct": "rc2", **{key: docs[n][key] for key in ("params", "c", "valve_gain", "sigma_u", "noise_r", "p0", "q_r", "q_a")}, **{key: docs[n][key] for key in ("air", "effort", "co2", "hum") if key in docs[n]}, "q_um": docs[n].get("q_um", 0.0), "q_ua": docs[n].get("q_ua", 0.0), "neighbours": {"wall": [5 + (i + 1) % 3]}} for i, n in enumerate(NAMES)}
    return {
        "bundle": {"kind": "room_model", "version": "20261101T000000Z", "dt_s": 60.0, "rooms": bundle_rooms, "shock": SHOCK, "house": {**HOUSE, "night_hours": [22, 5], "timezone": "Europe/Berlin"}},
        "rooms": list(NAMES), "steps": STEPS,
        "inputs": {n: {key: clean([getattr(r[n], key) for r in inputs]) for key in ("y", "yv", "u", "sp", "tout", "wind", "rad")} | {"night": [int(r[n].night) for r in inputs], "ncnt": [list(r[n].ncnt) for r in inputs], "nsum": [clean(list(r[n].nsum)) for r in inputs], "ya": [clean(list(r[n].ya)) for r in inputs]} | {key: clean([getattr(r[n], key) for r in inputs]) for key in ("co2", "rh", "th")} for n in NAMES},
        "expected": expected, "resets": [{"room": r["room"], "cause": r["cause"], "step": r["step"]} for r in h.resets], "plans": plans, "checks": checks(),
    }


def clean_env(env):
    return {k: clean(v) if not isinstance(v, bool) else v for k, v in env.items()}


if __name__ == "__main__":
    OUT.write_text(json.dumps(build(), separators=(",", ":")))
    print(OUT, OUT.stat().st_size)
