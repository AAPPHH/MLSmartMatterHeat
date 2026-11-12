import copy
import inspect
import math
from datetime import datetime
from zoneinfo import ZoneInfo
import numpy as np
import pytest
from heattrain.__main__ import CONFIG
from heattrain.roomfit import Exog, PcnnFit, RoomFit
from heattrain.roomimm import RoomPar, plan, rollout
from heattrain.roompcnn import FEATURES, Pcnn, check_pcnn_doc, feature_matrix, feature_row, pcnn_gate
from room_sim import SHOCK, TRUE, room_doc

PC = CONFIG["roommodel"]["pcnn"]
PCT = dict(PC, q0=8.0, q_sigma=1.0)
TRUE_P = dict(TRUE, tau_v=6.0, w=0.3)
F = len(FEATURES)


def random_doc(seed=0, hidden=6, qmax=1.5, scale=1.0):
    rng = np.random.default_rng(seed)
    return {"qmax": qmax, "features": list(FEATURES), "mu": rng.normal(size=F).tolist(), "sd": (0.5 + rng.random(F)).tolist(), "w1": (scale * rng.normal(size=(hidden, F))).tolist(), "b1": rng.normal(size=hidden).tolist(), "w2": (scale * rng.normal(size=hidden)).tolist(), "b2": float(rng.normal())}


def test_feature_row_matches_the_matrix_and_missing_gases_are_zero_with_a_flag():
    cases = [(7.25, 0.0, 120.0, 3.5, 1.0, 0.4, -0.02, 0.0), (23.9, 1.0, 0.0, -4.0, 0.0, math.nan, math.nan, 1.0), (0.0, 0.0, 40.0, 10.0, math.nan, 2.0, math.nan, math.nan)]
    mat = feature_matrix(*(np.array([c[i] for c in cases]) for i in range(8)))
    for row, c in zip(mat, cases):
        np.testing.assert_allclose(row, feature_row(*c), rtol=0, atol=1e-15)
    gases = dict(zip(FEATURES, feature_row(*cases[1])))
    assert (gases["co2_rate"], gases["co2_avail"], gases["hum_rate"], gases["hum_avail"]) == (0.0, 0.0, 0.0, 0.0)
    assert dict(zip(FEATURES, feature_row(*cases[0])))["co2_avail"] == 1.0
    assert dict(zip(FEATURES, feature_row(*cases[2])))["present"] == 0.0 and dict(zip(FEATURES, feature_row(*cases[2])))["coffee"] == 0.0


def test_forward_is_bounded_scalar_equals_batch_and_non_finite_inputs_give_zero():
    net = Pcnn(random_doc(scale=3.0))
    rng = np.random.default_rng(1)
    x = rng.normal(0.0, 30.0, (400, F))
    q = net.q_batch(x)
    assert np.all(np.abs(q) <= net.qmax) and np.abs(q).max() > 0.5 * net.qmax
    np.testing.assert_allclose([net.q(r.tolist()) for r in x[:50]], q[:50], rtol=0, atol=1e-12)
    for bad in (math.nan, math.inf, -math.inf):
        for i in range(F):
            row = x[0].copy()
            row[i] = bad
            assert net.q(row.tolist()) == 0.0 and net.q_batch(row[None])[0] == 0.0
    huge = np.full(F, 1e300)
    assert abs(net.q(huge.tolist())) <= net.qmax


def test_the_net_cannot_see_valve_setpoint_or_supply():
    banned = ("valve", "setpoint", "supply", "lag", "demand", "u", "sp", "yv", "p_supply")
    assert not [n for n in FEATURES if any(b == n or n.startswith(b + "_") for b in banned)]
    for fn in (feature_row, feature_matrix):
        assert not [n for n in inspect.signature(fn).parameters if n in banned]
    assert list(inspect.signature(feature_row).parameters) == ["hour", "weekend", "rad", "tout", "present", "co2_rate", "hum_rate", "coffee"]


def planner_env(**over):
    env = {"tout": 0.0, "wind": 3.0, "rad": 0.0, "night": False, "cn": 0.0, "cs": 0.0, "target": 21.0, "present": 1.0, "lag": [0.3, 0.3], "u_now": 0.3, "sp_cur": 21.0, "sp_live": 21.0}
    return {**env, **over}


def test_heating_stays_monotone_in_the_setpoint_and_the_valve_with_the_net_term_on():
    par = RoomPar(room_doc(coupling=False, tau_v=6.0, w=0.3))
    for q in (-3.0, 0.0, 2.5):
        low, mid, high = (rollout(par, 20.5, 19.8, 1.0, 1, [sp] * 60, planner_env(q=q))[2] for sp in (17.0, 21.0, 26.0))
        assert all(h >= m - 1e-12 >= l - 2e-12 for h, m, l in zip(high, mid, low))
        assert high[-1] > low[-1]
    n = 300
    c = {"y": np.full(n, 20.0), "yv": np.zeros(n), "u": np.full(n, 0.2), "sp": np.full(n, 21.0), "tout": np.full(n, 3.0), "wind": np.full(n, 3.0), "rad": np.zeros(n), "night": np.zeros(n), "yi": np.full(n, 20.0), "bad": np.zeros(n), "ncnt": [], "nsum": []}
    rc = copy.deepcopy(CONFIG["roommodel"])
    rc["fit"].update(warm_min=10, stride=7)
    rf = RoomFit(c, n, [], rc, False)
    p = rf.params(np.log([TRUE_P[k] if k in TRUE_P else 1.0 for k in rf.names])[None])
    sel = rf.js > 30
    m0, a0 = np.full(sel.sum(), 20.5)[:, None], np.full(sel.sum(), 19.8)[:, None]
    doc = room_doc(coupling=False, tau_v=6.0, w=0.3)
    seen = []
    for u in (0.0, 0.2, 0.6, 1.0):
        c["u"][:] = u
        rf.lags.clear()
        out = rf.roll_causal(p, doc, m0, a0, 1.0, sel, 1.0, 1.0, np.full(sel.sum(), 1.7))
        seen.append((out[20].mean(), out[60].mean()))
    assert all(b[0] >= a[0] - 1e-12 and b[1] >= a[1] - 1e-12 for a, b in zip(seen, seen[1:])) and seen[-1][1] > seen[0][1]


def test_tangent_of_the_causal_rollout_matches_finite_differences_including_valve_clipping():
    n = 600
    rng = np.random.default_rng(5)
    c = {"y": 20.0 + 0.3 * np.sin(np.arange(n) / 40.0), "yv": np.zeros(n), "u": np.clip(0.3 + 0.4 * np.sin(np.arange(n) / 50.0), 0.0, 1.0), "sp": np.where(np.arange(n) % 200 < 100, 22.0, 17.0), "tout": 4.0 + np.sin(np.arange(n) / 90.0), "wind": np.full(n, 3.0),
         "rad": np.full(n, 40.0), "night": np.zeros(n), "yi": np.full(n, 20.0), "bad": np.zeros(n), "ncnt": [np.full(n, 1.0)], "nsum": [np.full(n, 19.0)]}
    rc = copy.deepcopy(CONFIG["roommodel"])
    rc["fit"].update(warm_min=10, stride=7)
    rf = RoomFit(c, n, ["wall"], rc, False)
    names = rf.names
    p = rf.params(np.log([dict(TRUE_P, c_wall=0.2).get(k, 1.0) for k in names])[None])
    sel = rf.js > 40
    k = int(sel.sum())
    m0, a0 = 20.4 + 0.3 * rng.random(k), 19.7 + 0.3 * rng.random(k)
    start = (np.zeros(n), np.zeros(n), np.zeros(n), np.zeros(n))
    for arr, v in zip(start, (m0, a0, np.full(k, 1.4), np.full(k, 0.8))):
        arr[rf.js[sel]] = v
    doc = room_doc(coupling=True, tau_v=6.0, w=0.3)
    q = rng.normal(0.0, 2.0, k)
    e, _, d = rf.errors(p, sel, doc, start, 0.97, True, q=q, tangent=True)
    eps = 1e-6
    e2, _ = rf.errors(p, sel, doc, start, 0.97, True, q=q + eps)
    for H in (20, 60):
        np.testing.assert_allclose(-(e2[H] - e[H])[:, 0] / eps, d[H][:, 0], rtol=1e-4, atol=1e-7)
    assert max(abs(d[H]).max() for H in (20, 60)) > 0.01


def test_gate_requires_both_horizons_pooled_and_no_room_losing_more_than_a_point():
    gate = CONFIG["roommodel"]["pcnn"]["gate"]
    rep = lambda h20, h60, rooms=None: {"pooled": {"H20": {"skill": 0.30, "pcnn_skill": 0.30 + h20}, "H60": {"skill": 0.30, "pcnn_skill": 0.30 + h60}},
                                       "rooms": {r: {"H20": {"skill": 0.3, "pcnn_skill": 0.3 + g}, "H60": {"skill": 0.3, "pcnn_skill": 0.3 + g}} for r, g in (rooms or {"a": 0.02, "b": 0.0}).items()}}
    assert pcnn_gate(rep(0.02, 0.02), gate)["active"]
    assert pcnn_gate(rep(0.01, 0.01), gate)["active"]
    assert not pcnn_gate(rep(0.02, 0.0095), gate)["active"]
    assert not pcnn_gate(rep(0.0095, 0.03), gate)["active"]
    assert pcnn_gate(rep(0.02, 0.02, {"a": 0.05, "b": -0.0099}), gate)["active"]
    out = pcnn_gate(rep(0.02, 0.02, {"a": 0.06, "b": -0.015}), gate)
    assert not out["active"] and any("b H20 loses 1.50" in r for r in out["reasons"])
    missing = {"pooled": {"H20": {"skill": 0.3, "pcnn_skill": None}, "H60": {"skill": 0.3, "pcnn_skill": None}}, "rooms": {}}
    assert not pcnn_gate(missing, gate)["active"]


def test_the_loader_rejects_bad_shapes_non_finite_weights_and_other_feature_lists_and_accepts_no_block():
    base = room_doc(coupling=False)
    assert RoomPar(base).pcnn is None
    assert RoomPar(dict(base, pcnn=random_doc())).pcnn is not None
    bad = []
    d = random_doc()
    d["w1"][2] = d["w1"][2][:-1]
    bad.append(d)
    d = random_doc()
    d["w2"][1] = math.nan
    bad.append(d)
    d = random_doc()
    d["features"] = list(reversed(FEATURES))
    bad.append(d)
    d = random_doc()
    d["sd"][0] = 0.0
    bad.append(d)
    d = random_doc()
    d["qmax"] = -1.0
    bad.append(d)
    d = random_doc()
    d["mu"] = d["mu"][:-1]
    bad.append(d)
    d = random_doc()
    del d["b2"]
    bad.append(d)
    for d in bad:
        with pytest.raises(ValueError, match="pcnn"):
            RoomPar(dict(base, pcnn=d))
        with pytest.raises(ValueError):
            check_pcnn_doc("room", d)


def test_planner_logs_both_forecasts_and_the_active_flag_decides_which_one_drives_the_plan():
    par = RoomPar(room_doc(coupling=False, tau_v=6.0, w=0.3))
    comps = [(1.0, [20.5, 20.0, 1.0], [0.01, 0.0, 0.0, 0.0, 0.01, 0.0, 0.0, 0.0, 1e-2])]
    env = lambda **over: {"tout": 0.0, "wind": 3.0, "rad": 0.0, "night": False, "ncnt": (), "nsum": (), "target": 21.0, "present": 1.0, "sp_cur": 21.0, "sp_live": 21.0, "frost": 7.0, "boost_c": 24.0, "boost_ok": False, "raise_cost": 15.0,
                          "cold_cost": 1.0, "r_grid": 0.02, "lag": [0.3, 0.3], "y_last": None, "u_now": 0.3, "now_min": 0, **over}
    none = plan(par, SHOCK, comps, 1.0, env())
    assert math.isnan(none["forecast_pcnn"]) and none["forecast"] == none["forecast_phys"] and not none["pcnn_active"]
    off = plan(par, SHOCK, comps, 1.0, env(q_net=-3.0, pcnn_active=False))
    on = plan(par, SHOCK, comps, 1.0, env(q_net=-3.0, pcnn_active=True))
    assert off["forecast_phys"] == none["forecast_phys"] == on["forecast_phys"] and off["forecast_pcnn"] == on["forecast_pcnn"]
    assert off["forecast"] == off["forecast_phys"] and on["forecast"] == on["forecast_pcnn"] and on["forecast_pcnn"] < on["forecast_phys"] - 0.1
    assert off["total"] == none["total"] and off["cost_live"] == none["cost_live"] and on["cost_live"] != none["cost_live"] and on["pcnn_active"]
    anchored = plan(par, SHOCK, comps, 1.0, env(q_net=-3.0, pcnn_active=True, y_last=19.0))
    assert abs((anchored["forecast_pcnn"] - on["forecast_pcnn"]) - (19.0 - on["sensor"])) < 1e-12


def sim_room(n, q_of, seed=0):
    rng = np.random.default_rng(seed)
    par = RoomPar(room_doc(coupling=False, tau_v=6.0, w=0.3))
    h = 1.0 / 60.0
    hour = (np.arange(n) / 60.0 + 6.0) % 24.0
    sp = np.full(n, 21.0)
    tout, rad = 4.0 + 3.0 * np.sin(np.arange(n) / 700.0), np.clip(80.0 * np.sin((hour - 6.0) / 12.0 * np.pi), 0.0, None)
    present = ((hour > 17.0) & (hour < 22.0) & ((np.arange(n) // 1440) % 7 < 5)).astype(float)
    m, a, lag = 20.0, 19.5, None
    ms, as_, us = np.zeros(n), np.zeros(n), np.zeros(n)
    for k in range(n):
        sens = par.sensor(m, a)
        u = float(np.clip(par.g * (sp[k] - sens), 0.0, 1.0))
        lag = par.lag(lag, u)
        ms[k], as_[k], us[k] = m, a, u
        f11, f12, f21, f22, g1, g2, _ = par.terms(1.0, lag[1], tout[k], 3.0, rad[k], False, 0.0, 0.0, h)
        m, a = f11 * m + f12 * a + g1 + rng.normal(0, 0.003), f21 * m + f22 * a + g2 + h * q_of(present[k], hour[k]) + rng.normal(0, 0.003)
    y = par.sensor(ms, as_) + rng.normal(0, 0.02, n)
    c = {"y": y, "yv": us, "u": us, "sp": sp, "tout": tout, "wind": np.full(n, 3.0), "rad": rad, "night": np.zeros(n), "yi": y, "bad": np.zeros(n), "ncnt": [], "nsum": []}
    return c, ms, as_, hour, present


def fitted(q_of, n=9000, seed=0, iters=None):
    c, ms, as_, hour, present = sim_room(n, q_of, seed)
    rc = copy.deepcopy(CONFIG["roommodel"])
    rc["fit"].update(warm_min=60, stride=5)
    rf = RoomFit(c, n, [], rc, False)
    p = rf.params(np.log([TRUE_P[k] if k in TRUE_P else 1.0 for k in rf.names])[None])
    start = (ms, as_, np.ones(n), np.ones(n))
    X = feature_matrix(hour, np.zeros(n), c["rad"], c["tout"], present, np.full(n, np.nan), np.full(n, np.nan), np.zeros(n))
    cut = int(n * 0.7)
    doc = room_doc(coupling=False, tau_v=6.0, w=0.3)
    net, info = PcnnFit(PCT).fit(rf, p, doc, start, X, cut, 1.0, iters)
    return rf, p, doc, start, X, cut, net, info


def holdout_rmse(rf, p, doc, start, X, cut, net, H):
    test = (rf.js >= cut) & rf.valid[H] & (rf.js + max(rf.Hs) < rf.n)
    e0, _ = rf.errors(p, test, doc, start, 1.0, True)
    q = net.q_batch(X[rf.js[test]]) if net else None
    e1, _ = rf.errors(p, test, doc, start, 1.0, True, q=q)
    return float(np.sqrt(np.mean(e0[H] ** 2))), float(np.sqrt(np.mean(e1[H] ** 2)))


def test_training_learns_an_injected_presence_gain_deterministically_and_ignores_a_pure_rc_room():
    gain = lambda present, hour: 8.0 * present
    rf, p, doc, start, X, cut, net, info = fitted(gain)
    assert net is not None and info["best_iter"] > 0 and info["val_loss"] < info["val_loss_physics"]
    for H in (20, 60):
        phys, withnet = holdout_rmse(rf, p, doc, start, X, cut, net, H)
        assert withnet < 0.8 * phys, (H, phys, withnet)
    again = fitted(gain)[6]
    assert again.doc == net.doc
    assert net.qmax > 5.0 and np.abs(net.q_batch(X[rf.js])).max() <= net.qmax
    shipped = fitted(gain, iters=info["best_iter"])[6]
    assert shipped is not None and shipped.doc == fitted(gain, iters=info["best_iter"])[6].doc
    flat = fitted(lambda present, hour: 0.0, seed=2)
    assert flat[6] is None and flat[7]["reason"] == "no validation gain"
    far = fitted(gain, n=1000)
    assert far[6] is None and "origins" in far[7]["reason"]


def test_exogenous_features_follow_the_presence_rows_the_coffee_plug_and_the_local_clock():
    grid = type("G", (), {})()
    grid.names, grid.n = ["kueche", "bad"], 600
    t0 = 1_793_000_000.0 - (1_793_000_000.0 % 60)
    grid.times = t0 + 60.0 * np.arange(600)
    rows = [(t0 + 3600.0, "kueche"), (t0 + 3600.0 + 1200.0, None), (t0 + 3600.0 * 3, "bad")]
    power = {9: (np.array([t0 + 300.0, t0 + 4000.0]), np.array([1_500_000.0, 0.0]))}
    pc = CONFIG["roommodel"]["pcnn"]
    ex = Exog(grid, "Europe/Berlin", rows, power, {"kueche": [9]}, pc, 900.0)
    assert ex.present["kueche"][59] == 0.0 and ex.present["kueche"][60] == 1.0 and ex.present["kueche"][79] == 1.0 and ex.present["kueche"][80] == 0.0
    assert ex.present["bad"][180] == 1.0 and ex.present["bad"][180 + 31] == 0.0 and ex.present["bad"][100] == 0.0
    on = ex.coffee["kueche"]
    assert on[4] == 0.0 and on[5] == 1.0 and on[20] == 1.0 and on[21] == 0.0 and on[66] == 0.0 and on[67] == 0.0 and not ex.coffee["bad"].any()
    for i in (0, 90, 599, 580 * 2 % 600):
        local = datetime.fromtimestamp(grid.times[i], ZoneInfo("Europe/Berlin"))
        assert abs(ex.hour[i] - (local.hour + local.minute / 60.0)) < 1e-9 and ex.weekend[i] == float(local.weekday() >= 5)
    summer = Exog(type("G", (), {"names": ["a"], "n": 3, "times": np.array([1_782_000_000.0, 1_782_000_000.0 + 86400 * 5, 1_793_000_000.0 + 86400 * 2])})(), "Europe/Berlin", [], {}, {}, pc, 900.0)
    for t, hh, we in zip((1_782_000_000.0, 1_782_000_000.0 + 86400 * 5, 1_793_000_000.0 + 86400 * 2), summer.hour, summer.weekend):
        local = datetime.fromtimestamp(t, ZoneInfo("Europe/Berlin"))
        assert abs(hh - (local.hour + local.minute / 60.0 + local.second / 3600.0)) < 1e-9 and we == float(local.weekday() >= 5)
    assert set(np.unique(ex.weekend)) <= {0.0, 1.0}


def house_rooms(q_of, n=7500):
    from heattrain.thermal import RoomData
    t0 = datetime(2026, 11, 10, 6, 0, tzinfo=ZoneInfo("Europe/Berlin")).timestamp()
    rooms = []
    for i, name in enumerate(("wohnzimmer", "kueche")):
        c, *_ = sim_room(n, q_of, seed=i)
        d = RoomData(name, 5 + i, t0, 60.0, c["y"], c["y"], c["u"], c["sp"], c["tout"], c["wind"], c["rad"], np.zeros(n, bool), {}, {"base_room": name, "neighbour_nodes": {}, "n_masked": 0}, np.zeros(n, bool), c["yv"], None)
        rooms.append(d)
    return rooms


def bundle_config():
    config = copy.deepcopy(CONFIG)
    rm = config["roommodel"]
    rm["shock"].update(days=1, passes=1, grid={"q_calm": [0.01, 0.03], "p_jump": [1e-3], "q_jump": [0.3]})
    rm["fit"].update(warm_min=120, max_nfev=40)
    rm["eval"].update(folds=[0.5, 0.7], warm_days=0.3)
    rm["evidence"]["select"]["enabled"] = False
    rm["pcnn"].update(q0=8.0, q_sigma=1.0)
    return config


def test_full_fit_ships_an_active_net_for_an_injected_diurnal_gain_and_an_inactive_one_for_a_pure_rc_house(tmp_path):
    from heattrain.roomeval import fit_bundle
    gained, report = fit_bundle(bundle_config(), house_rooms(lambda present, hour: 8.0 * ((8.0 < hour < 11.0) or (17.0 < hour < 22.0))), tmp_path, version="20261101T000000Z")
    assert gained["pcnn"]["features"] == list(FEATURES)
    assert report["pcnn"]["gate"]["pooled"]["H20"] >= 0.01 and report["pcnn"]["gate"]["pooled"]["H60"] >= 0.01, report["pcnn"]["gate"]
    assert gained["pcnn"]["active"] and report["pcnn"]["active"] and all("pcnn" in d for d in gained["rooms"].values())
    for d in gained["rooms"].values():
        RoomPar(d)
    flat, report = fit_bundle(bundle_config(), house_rooms(lambda present, hour: 0.0), tmp_path, version="20261101T000001Z")
    assert flat["pcnn"]["active"] is False and not report["pcnn"]["active"] and report["pcnn"]["gate"]["reasons"]
