import copy
import json
import os
from pathlib import Path
import numpy as np
import pytest
from heattrain.__main__ import CONFIG
from heattrain.thermal import RoomData, ThermalModel, build_rooms, fit_room, night_mask, thermal

CFG = copy.deepcopy(CONFIG["thermal"])
TRUE = {"a1": 8.0, "a2": 2.0, "b1": 0.6, "b2": 0.5, "b3": 0.04, "b4": 0.3, "w": 0.7, "ts": 55.0, "ts_night": 55.0}
T0 = 1_790_000_000.0
LAKE = Path(os.environ.get("HEAT_LAKE_TODAY", r"/home/user\AppData\Local\Temp\claude\c--clones-MLSmartMatterHeat\349be985-dace-4f4a-be42-5da1745ce8a6\scratchpad\lake_today"))


def simulate(hours=72, seed=0, report_p=0.25, invalid=0.03, noise=0.03):
    rng = np.random.default_rng(seed)
    n = hours * 60
    t = np.arange(n) * 60.0
    u = np.zeros(n)
    k = 0
    while k < n:
        L = int(rng.integers(30, 120))
        u[k:k + L] = rng.choice([0.0, 0.2, 0.5, 1.0])
        k += L
    tout = 8 + 5 * np.sin(2 * np.pi * (t / 86400 - 0.6)) + np.cumsum(rng.normal(0, 0.02, n))
    wind = np.clip(3 + np.cumsum(rng.normal(0, 0.1, n)) * 0.3, 0, 12)
    rad = np.clip(300 * np.sin(2 * np.pi * (t / 86400 - 0.25)), 0, None) * (0.6 + 0.4 * np.sin(t / 20000) ** 2)
    sub = 10
    h = 1 / 60 / sub
    tr, ta = 20.0, 19.0
    air, sens = np.zeros(n), np.zeros(n)
    p = TRUE
    for i in range(n):
        for _ in range(sub):
            dr = p["a1"] * u[i] * (p["ts"] - tr) - p["a2"] * (tr - ta)
            da = p["b1"] * (tr - ta) - (p["b2"] + p["b3"] * wind[i]) * (ta - tout[i]) + p["b4"] * rad[i] / 100
            tr += h * dr + rng.normal(0, 0.02) * np.sqrt(h * 60)
            ta += h * da + rng.normal(0, 0.005) * np.sqrt(h * 60)
        air[i], sens[i] = ta, p["w"] * tr + (1 - p["w"]) * ta
    y = np.full(n, np.nan)
    rep = rng.random(n) < report_p
    rep[0] = True
    y[rep] = np.round(sens[rep] + rng.normal(0, noise, rep.sum()), 2)
    d = RoomData("synth", 1, T0, 60.0, y, np.interp(t, t[rep], y[rep]), u, np.full(n, np.nan), tout, wind, rad, np.zeros(n, bool), {}, {"n_invalid_demand": int(invalid * n), "start": T0, "end": T0 + t[-1]})
    return d, air, sens


@pytest.fixture(scope="module")
def synth():
    return simulate()


@pytest.fixture(scope="module")
def doc(synth):
    return fit_room(synth[0], CFG)


def test_recovers_parameters(doc):
    p = doc["params"]
    assert doc["structure"] == "rc2"
    assert abs(p["w"] - TRUE["w"]) < 0.2
    assert 0.5 < p["b2"] / TRUE["b2"] < 2.0
    assert abs(p["ts"] - TRUE["ts"]) < 15
    tau = doc["tau_min"]
    assert tau[0] < tau[1]
    assert doc["metrics"]["rc2"]["rmse_horizon"] < doc["metrics"]["rc2"]["rmse_persistence"]


def test_virtual_thermometer_tracks_air(synth, doc):
    d, air, sens = synth
    est = ThermalModel(doc, CFG).estimate(d)
    sensor_err = np.sqrt(np.mean((sens - air) ** 2))
    model_err = np.sqrt(np.mean((est["air"][200:] - air[200:]) ** 2))
    assert model_err < 0.3 * sensor_err
    assert model_err < 2.5
    assert est["air_total_std"][200:].mean() > est["air_std"][200:].mean()


def test_predict_and_lead_time(synth, doc):
    d, air, _ = synth
    m = ThermalModel(doc, CFG)
    est = m.estimate(d)
    x0 = (est["radiator"][-1], est["air"][-1])
    tr = m.predict(x0, 120, 8.0, u=1.0)
    assert tr[-1, 1] > tr[0, 1]
    lt = m.lead_time((19.0, 19.0), 21.0, 8.0)
    assert lt is not None and 5 < lt < 240


def test_sparse_reports_and_invalid_demand():
    d, air, _ = simulate(hours=48, seed=3, report_p=0.04, invalid=0.1)
    doc = fit_room(d, CFG)
    assert np.isfinite(doc["params"]["w"])
    assert doc["structure"] in ("rc1", "rc2")
    assert any("invalid demand" in w for w in doc["warnings"])


def test_night_mask():
    m = night_mask(np.array([1_790_000_000.0 + 3600 * i for i in range(24)]), CFG)
    assert 4 <= m.sum() <= 9


def test_invalid_134_treated_as_missing():
    reports = [(T0 + 60 * i, 5, 0, 2000.0 + i) for i in range(400)]
    reports += [(T0 + 60 * i, 5, 8, 134.0 if i % 7 == 0 else 40.0) for i in range(400)]
    weather = [(T0 + 3600 * i, "obs", None, 8.0, 2.0, 0.0) for i in range(10)]
    rooms, skipped = build_rooms(reports, weather, [], CFG)
    assert not skipped
    assert rooms[0].info["n_invalid_demand"] > 0
    assert rooms[0].u.max() <= 1.0 and rooms[0].u.min() >= 0.0


@pytest.mark.skipif(not LAKE.is_dir(), reason="lake_today files absent")
def test_real_data_smoke(tmp_path):
    docs, out = thermal({"thermal": CFG}, LAKE, tmp_path)
    assert docs
    for d in docs:
        j = json.loads((out / f"{d['room']}.json").read_text())
        assert j["params"] and j["span"] and "warnings" in j and "bounds" in j
