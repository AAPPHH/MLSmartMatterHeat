import json
import sys
from pathlib import Path
import numpy as np
from heattrain.boostlearn import Dosing
from test_boostlearn import SHAPE, L, ctx, episode, synthetic

OUT = Path(__file__).resolve().parents[2] / "crates" / "heatd" / "testdata" / "boost_parity.json"


def bundle():
    b = synthetic([episode(dose=7.0), episode(dose=9.0, out=5.5), episode(room="bad", dose=12.0, out=2.0), episode(dose=6.0, scenario="homecoming", out=1.0, start_temp=14.0, gap=6.0), episode(dose=30.0, scenario="morning", out=-12.0)], g=0.05)
    rng = np.random.default_rng(5)
    th = np.array([[np.log(0.12), np.log(11.0)]] * 16) + rng.normal(0, 0.08, (16, 2))
    b["rooms"]["bad"] = {"a": 0.004, "delta": 4.0, "pi": {"kp": 0.6, "gap0": 0.3}, "peak_sigma_k": 0.6, "samples": th.tolist()}
    b["rooms"]["wohnzimmer"]["peak_sigma_k"] = 0.45
    b["trust"]["mean"] = [4.0, 2.0, 10.0, 17.0, 2.5, 1.0, 100.0, 5.0, 10.0, 200.0, 0.0, 0.0]
    b["trust"]["std"] = [6.0, 1.5, 100.0, 2.0, 1.5, 1.0, 80.0, 6.0, 12.0, 150.0, 0.7, 0.7]
    b["trust"]["weights"] = [1.0, 0.5, 0.5, 1.0, 1.0, 1.0, 0.5, 0.5, 0.5, 0.5, 0.5, 0.5]
    b["trust"]["radius"] = 2.2
    b["version"] = "20261101T000000Z"
    b["kind"] = "boost_learn"
    b["pulse_cap_min"] = 30
    b["dead_zone"] = 0.25
    b["boost_setpoint_c"] = 24.0
    return b


def hist(kind):
    h = np.zeros(L)
    if kind == "pulse":
        h[-70:-55] = 0.6
    if kind == "recent":
        h[-12:] = 1.0
    return h.round(6).tolist()


CASES = [
    ("wohnzimmer", dict(), "morning", 20.0, "zero"),
    ("wohnzimmer", dict(start_temp=14.0, gap=6.0, out=1.0, hour=17.5, since_resume=180.0), "homecoming", 20.0, "zero"),
    ("wohnzimmer", dict(out=5.2, hour=6.5, vm60=9.0, vm180=15.0), "morning", 20.0, "pulse"),
    ("wohnzimmer", dict(start_temp=16.0, gap=4.0, out=-3.0, hour=7.0, since_open=40.0), "room_change", 20.0, "zero"),
    ("wohnzimmer", dict(supply_ok=0.0, since_resume=0.0, start_temp=15.0, gap=5.0), "homecoming", 20.0, "zero"),
    ("bad", dict(out=2.0, start_temp=17.0, gap=3.0, hour=6.0), "morning", 20.0, "recent"),
    ("bad", dict(out=9.0, start_temp=19.0, gap=1.0, hour=8.0, wind=5.0), "homecoming", 20.0, "zero"),
    ("wohnzimmer", dict(start_temp=20.8, gap=0.2, hour=6.0), "morning", 21.0, "zero"),
    ("wohnzimmer", dict(out=-12.0, start_temp=17.0, gap=3.0, hour=6.5, rad=50.0), "morning", 20.0, "pulse"),
]


def main():
    b = bundle()
    dosing = Dosing(b)
    cases = []
    for room, kw, scenario, target, kind in CASES:
        c = ctx(**kw)
        c.pop("since_open") if "since_open" not in kw else None
        vh = hist(kind)
        dec = dosing.decide(room, c, scenario, target, vh, draw=3)
        p, pk = dosing.risk(room, c, target, vh)
        cases.append({"room": room, "ctx": c, "scenario": scenario, "target": target, "valve_hist": vh, "expected": dec, "p_sample": {str(i): float(p[i]) for i in (0, 10, 40, 80, 150)}, "peak_sample": {str(i): float(pk[i].max()) for i in (0, 40, 150)}})
    OUT.write_text(json.dumps({"bundle": b, "cases": cases}, separators=(",", ":")))
    return cases


if __name__ == "__main__":
    for c in main():
        e = c["expected"]
        print(c["room"], c["scenario"], e["d_safe"], e["known"], e["allowed"], e["chosen"], e["j"], round(e["p_overshoot"], 4))
