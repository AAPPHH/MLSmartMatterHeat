import copy
import json
import math
import numpy as np
import pytest
from heattrain.__main__ import CONFIG
from heattrain.roomfit import fit_bundle, fit_shock, run_batch, shock_defaults, tune_windows
from heattrain.roomimm import House, RoomIn, RoomPar, plan, storage
from heattrain.thermal import RoomData, build_rooms, kalman
from room_sim import HOUSE, NAMES, SHOCK, TRUE, room_doc, simulate_house
from test_thermal import CFG, T0, simulate


def run(docs, inputs, shock=SHOCK, house=HOUSE):
    h = House(docs, shock, house)
    rec = {n: {k: [] for k in ("p_window", "lam", "p_supply", "a")} for n in NAMES}
    for row in inputs:
        for n, o in h.step(row).items():
            for k in rec[n]:
                rec[n][k].append(o.get(k, math.nan))
    return h, {n: {k: np.array(v) for k, v in r.items()} for n, r in rec.items()}


def test_regression_matches_old_kalman():
    d, _, _ = simulate(hours=12)
    ks, v, s, xs, ps = kalman(dict(TRUE), d, "rc2", CFG, collect=True)
    doc = {"params": dict(TRUE), "c": {}, "valve_gain": 0.5, "sigma_u": 0.2, "noise_r": CFG["noise_r"], "p0": CFG["p0"], "q_r": CFG["q_r"], "q_a": CFG["q_a"]}
    shock = dict(SHOCK, q_calm=0.0, q_jump=0.0, p_jump=0.0, p_l0=0.0)
    house = dict(HOUSE, p_up=0.0, p_down=0.0, p_supply0=1.0, reset_gap_min=1e9, reset_sigma=1e9, r_grid=0.0, balance_seed=0, trv_grid=0.0)
    h = House({"r": doc}, shock, house)
    worst = 0.0
    for k in range(d.n):
        out = h.step({"r": RoomIn(d.y[k], math.nan, d.u[k], math.nan, d.tout[k], d.wind[k], d.rad[k], bool(d.night[k]), (), ())})["r"]
        if out["ok"]:
            worst = max(worst, abs(out["m"] - xs[k][0]), abs(out["a"] - xs[k][1]))
    assert worst < 1e-9
    assert not h.resets


def test_outage_mask_excludes_gap_restart_and_stale_nodes():
    minutes = 400
    reports = []
    for i in range(minutes):
        if 100 <= i < 280:
            continue
        for node in (5, 6):
            reports += [(T0 + 60 * i, node, 0, 2000.0 + i % 3), (T0 + 60 * i, node, 8, 40.0)]
    weather = [(T0 + 3600 * i, "obs", None, 8.0, 2.0, 0.0) for i in range(10)]
    events = [(T0 + 60 * 100, "shutdown"), (T0 + 60 * 280, "startup")]
    rooms, _ = build_rooms(reports, weather, [], CFG, events)
    assert rooms
    for d in rooms:
        assert d.bad[100:286].all()
        assert np.isnan(d.y[100:286]).all() and np.isnan(d.yv[100:286]).all()
        assert not d.bad[290:].any()
        assert np.isfinite(d.y[300])
        assert d.info["n_masked"] >= 186
    quiet = [r for r in reports if not (r[1] == 6 and (T0 + 60 * 300 > r[0] > T0 + 60 * 100)) and not (T0 + 60 * 100 <= r[0] < T0 + 60 * 280)] + [(T0 + 60 * i, 5, a, v) for i in range(100, 280) for a, v in ((0, 2001.0), (8, 40.0))]
    rooms, _ = build_rooms(quiet, weather, [], CFG, ())
    d5, d6 = ([d for d in rooms if d.node == n][0] for n in (5, 6))
    assert d6.bad[200] and not d6.bad[110] and not d5.bad[200]


def test_loss_jump_is_detected_in_the_right_room_only():
    docs, inputs, truth = simulate_house(900, seed=0, window={"kueche": (400, 520, 4.0)})
    h, rec = run(docs, inputs)
    assert not h.resets
    k = rec["kueche"]
    first = int(np.nonzero(k["p_window"][400:] > 0.5)[0][0])
    assert first <= 45
    assert 2.5 < np.median(k["lam"][450:520]) < 5.5
    assert k["p_window"][700:].max() < 0.5
    for other in ("wohnzimmer", "schlafzimmer"):
        assert rec[other]["p_window"].max() < 0.5
    err = np.sqrt(np.mean((k["a"][100:] - np.array(truth["kueche"]["a"])[100:]) ** 2))
    assert err < 0.8


def test_boiler_stop_is_one_supply_event_not_four_windows():
    docs, inputs, _ = simulate_house(900, seed=0, stop=(400, 560))
    h, rec = run(docs, inputs)
    for n in NAMES:
        assert rec[n]["p_window"][300:700].max() < 0.5
        assert rec[n]["p_supply"][450:560].max() < 0.5
        assert rec[n]["p_supply"][300:395].min() > 0.9
        assert rec[n]["p_supply"][650:].min() > 0.9
    assert not h.resets


def test_resets_are_logged_and_never_nan():
    docs, inputs, _ = simulate_house(500, seed=1)
    h = House(docs, SHOCK, HOUSE)
    for k, row in enumerate(inputs):
        if k == 200:
            h.rooms["kueche"].x[0][1] = math.nan
        outs = h.step(row)
        for o in outs.values():
            if o["ok"]:
                assert all(math.isfinite(o[x]) for x in ("m", "a", "lam", "lam_std", "p_window", "p_supply"))
    assert [(r["room"], r["cause"]) for r in h.resets] == [("kueche", "nan")]
    assert math.isnan(h.resets[0]["prior"][0]) or h.resets[0]["step"] == 200
    docs, inputs, _ = simulate_house(500, seed=1, corrupt=(300, 6, 6.0), report_p=1.0)
    h, _ = run(docs, inputs)
    assert any(r["cause"] == "innovation" and r["room"] == NAMES[0] for r in h.resets)
    docs, inputs, _ = simulate_house(700, seed=2, gap=(200, 500))
    h, rec = run(docs, inputs)
    assert [(r["cause"], r["step"]) for r in h.resets if r["room"] == NAMES[0]] == [("gap", 500 + next(i for i in range(200) if math.isfinite(inputs[500 + i][NAMES[0]].y)))]
    assert np.isfinite(rec[NAMES[0]]["a"][520:]).all()


def plan_env(**over):
    env = {"tout": -5.0, "wind": 3.0, "rad": 0.0, "night": False, "ncnt": (), "nsum": (), "target": 21.0, "present": 1.0, "sp_cur": 21.0, "sp_live": 21.0, "frost": 7.0, "boost_c": 24.0, "boost_ok": False,
           "raise_cost": 15.0, "cold_cost": 1.0, "r_grid": HOUSE["r_grid"]}
    env.update(over)
    return env


def light_par():
    doc = room_doc(False, 0.3)
    doc["params"].update(w=0.3, b1=2.0, a1=3.0, ts=35.0, ts_night=35.0)
    return RoomPar(doc)


def comps_at(m, a, lam, p=1e-4):
    return [(0.25, [m, a, lam], [0.01, 0, 0, 0, 0.01, 0, 0, 0, p]) for _ in range(4)]


def test_plan_with_preset_costs_drops_to_frost_for_an_open_window_and_heats_a_closed_cold_room():
    par = light_par()
    for lam in (3.0, 6.0):
        open_ = plan(par, dict(SHOCK, lambda_jump=lam), comps_at(17.0, 16.0, lam), 1.0, plan_env())
        assert open_["p_window"] > 0.99 and open_["kind"] == "frost" and open_["setpoint"] == 7.0 and not open_["raise"]
        assert open_["cold"] < 1e-6 and open_["total"] < open_["cost_live"]
    cold = plan(par, SHOCK, comps_at(17.0, 16.0, 1.0), 1.0, plan_env())
    assert cold["p_window"] < 0.01 and cold["kind"] != "frost" and cold["setpoint"] >= 21.0
    warm = plan(par, SHOCK, comps_at(23.0, 22.5, 1.0), 1.0, plan_env())
    assert warm["p_window"] < 0.01 and not warm["raise"] and warm["setpoint"] <= 21.0


def test_plan_at_half_window_probability_probes_instead_of_guessing():
    par = light_par()
    shock = dict(SHOCK, lambda_jump=6.0)
    half = plan(par, shock, comps_at(17.0, 16.0, 2.0, 1e-2), 1.0, plan_env())
    assert abs(half["p_window"] - 0.5) < 1e-6
    assert half["kind"] == "probe" and half["probe_min"] in (10, 20, 30) and half["setpoint"] == 7.0 and not half["raise"]
    assert half["evoi"] > 1.0


def test_close_probe_costs_no_raise_and_is_a_decrease():
    par = light_par()
    shock = dict(SHOCK, lambda_jump=6.0)
    for cur in (21.0, 18.0):
        p = plan(par, shock, comps_at(17.0, 16.0, 2.0, 1e-2), 1.0, plan_env(sp_cur=cur, sp_live=cur))
        assert p["kind"] == "probe" and p["raise_cost"] == 0.0 and not p["raise"] and p["setpoint"] == 7.0


def test_storage_coast_and_stored_heat():
    par = light_par()
    warm = plan(par, SHOCK, comps_at(30.0, 22.0, 1.0), 1.0, plan_env())
    cold = plan(par, SHOCK, comps_at(17.0, 17.0, 1.0), 1.0, plan_env())
    assert cold["coast_min"] == 0.0 and cold["stored_min"] < 0.0
    assert warm["coast_min"] > 0.0 and warm["stored_min"] > 0.0
    hot = plan(par, SHOCK, comps_at(40.0, 25.0, 1.0), 1.0, plan_env())
    assert hot["coast_min"] > warm["coast_min"] and hot["stored_min"] > warm["stored_min"]
    mild = plan(par, SHOCK, comps_at(23.0, 21.0, 1.0), 1.0, plan_env(tout=21.0, wind=0.0))
    assert mild["coast_min"] == 1440.0


def test_stored_heat_is_zero_at_the_target_equilibrium():
    par = light_par()
    env = plan_env()
    env["cn"], env["cs"] = par.coupling((), ())
    amb = par.b2 + par.b3 * env["wind"]
    drive = amb * env["tout"] + par.b4 * env["rad"] / 100.0
    m_eq = (21.0 - (1 - par.w) * drive / (par.b1 + amb)) / (par.w + (1 - par.w) * par.b1 / (par.b1 + amb))
    a_eq = (par.b1 * m_eq + drive) / (par.b1 + amb)
    assert abs(par.sensor(m_eq, a_eq) - 21.0) < 1e-9
    _, stored = storage(par, m_eq, a_eq, 1.0, env)
    assert abs(stored) < 1e-9
    _, more = storage(par, m_eq + 1.0, a_eq, 1.0, env)
    assert more > 0.0


def test_plan_raise_cost_blocks_small_raises_and_no_supply_blocks_heating():
    par = light_par()
    comps = comps_at(19.4, 19.0, 1.0)
    cheap = plan(par, SHOCK, comps, 1.0, plan_env(sp_cur=20.0, sp_live=20.0, raise_cost=0.0))
    dear = plan(par, SHOCK, comps, 1.0, plan_env(sp_cur=20.0, sp_live=20.0, raise_cost=500.0))
    assert dear["raise"] is False and dear["setpoint"] <= 20.0
    assert cheap["best_cost"] <= dear["best_cost"] + 1e-9
    stop = plan(par, SHOCK, comps_at(14.0, 13.0, 1.0), 0.0, plan_env(raise_cost=15.0))
    assert not stop["raise"]


def test_plan_probe_is_chosen_only_with_value_of_information():
    par = light_par()
    shock = dict(SHOCK, lambda_jump=6.0)
    mixed = [(0.5, [17.0, 16.0, 1.0], [0.01, 0, 0, 0, 0.01, 0, 0, 0, 1e-4]), (0.5, [17.0, 16.0, 6.0], [0.01, 0, 0, 0, 0.01, 0, 0, 0, 1e-4])]
    p = plan(par, shock, mixed, 1.0, plan_env())
    assert 0.4 < p["p_window"] < 0.6 and p["kind"] == "probe" and p["evoi"] > 1.0
    assert p["best_cost"] - p["perfect_cost"] > p["evoi"]
    for lam in (1.0, 6.0):
        sure = plan(par, shock, comps_at(17.0, 16.0, lam), 1.0, plan_env())
        assert sure["kind"] != "probe" and sure["evoi"] == 0.0
        assert sure["best_cost"] - sure["perfect_cost"] < 1e-9


def test_fit_on_synthetic_rooms_writes_a_loadable_bundle(tmp_path):
    rooms = []
    for i, name in enumerate(("wohnzimmer", "kueche")):
        d, air, sens = simulate(hours=30, seed=i, report_p=0.4)
        d.room, d.node = name, 5 + i
        d.sp = np.full(d.n, 21.0)
        d.yv = np.where(np.isfinite(d.y), d.u, np.nan)
        d.bad = np.zeros(d.n, bool)
        d.info["neighbour_nodes"] = {}
        d.info["n_masked"] = 0
        rooms.append(d)
    config = copy.deepcopy(CONFIG)
    rm = config["roommodel"]
    rm["shock"].update(days=1, passes=1, grid={"q_calm": [0.01, 0.03], "p_jump": [1e-3], "q_jump": [0.3]})
    rm["fit"].update(warm_min=120, max_nfev=40)
    bundle, report = fit_bundle(config, rooms, tmp_path, version="20261101T000000Z")
    assert bundle["kind"] == "room_model" and set(bundle["rooms"]) == {"wohnzimmer", "kueche"}
    s = bundle["shock"]
    assert 0 < s["p_jump"] < 1 and s["lambda_w"] > 1 and s["lambda_jump"] > s["lambda_w"]
    for r in bundle["rooms"].values():
        assert set(r["params"]) == {"a1", "a2", "b1", "b2", "b3", "b4", "w", "ts", "ts_night", "tau_v"}
        assert all(math.isfinite(v) for v in r["params"].values()) and r["sigma_u"] > 0 and r["valve_obs"] is False
    json.dumps(bundle)
    pooled = report["holdout"]["pooled"]
    assert pooled["H20"]["n"] > 0 and pooled["H60"]["skill"] > 0.0
    assert all(i["all"]["converged"] for i in report["fit"].values())


def test_window_threshold_is_refused_without_hits_and_ties_prefer_fewer_false_alarms():
    rc = copy.deepcopy(CONFIG["roommodel"])
    rc["shock"].update(lambda_w_grid=[1.5, 2.0, 3.0], hit_min=20, pad_min=30, fa_weight=1.0)
    g = type("G", (), {})()
    g.names, g.dt = ["r"], 60.0
    g.times = 1_790_000_000.0 + 60.0 * np.arange(600)
    lam = np.ones(600)
    rec = {"r": {"lam": lam, "lam_std": np.full(600, 0.01)}}
    gt = [(g.times[300], g.times[340], ["r"])]
    lw, lj, info = tune_windows(g, rec, rc, gt, [])
    assert info["refused"] and lw == rc["shock"]["lambda_w"] and "warning" in info
    lam[305:330] = 2.6
    lam[100:110] = 1.8
    lw, lj, info = tune_windows(g, rec, rc, gt, [])
    assert not info["refused"] and lw == 2.0


def test_parity_fixture_is_in_sync_with_the_model():
    from make_room_model_fixture import OUT, build
    assert json.loads(OUT.read_text()) == json.loads(json.dumps(build(), separators=(",", ":")))


def sim_grid(steps=1500, seed=7, **kw):
    from make_room_model_fixture import SCENARIO
    docs, inputs, _ = simulate_house(steps, seed=seed, **(kw or SCENARIO))
    g = type("SimGrid", (), {})()
    g.names, g.n = list(NAMES), len(inputs)
    g.cols = {n: {k: np.array([getattr(r[n], k) for r in inputs], float) for k in ("y", "yv", "u", "sp", "tout", "wind", "rad")} for n in NAMES}
    for n in NAMES:
        g.cols[n].update(night=np.array([float(r[n].night) for r in inputs]), ncnt=[np.array([r[n].ncnt[0] for r in inputs])], nsum=[np.array([r[n].nsum[0] for r in inputs])],
                         ya=[np.array([r[n].ya[i] for r in inputs], float) for i in range(len(docs[n].get("air", [])))], **{k: np.array([getattr(r[n], k) for r in inputs], float) for k in ("co2", "rh", "th")})
    return docs, inputs, g


def test_batched_filter_matches_the_reference_house_including_resets():
    shocks = [SHOCK, dict(SHOCK, q_calm=0.03), dict(SHOCK, p_jump=1e-2, q_jump=0.6, hold_min=10.0, tau_l=60.0)]
    causes = set()
    for docs, inputs, g in (sim_grid(), sim_grid(500, seed=1, corrupt=(300, 6, 6.0), report_p=1.0)):
        bh, rec = run_batch(g, docs, shocks, HOUSE, record=True)
        for gi, shock in enumerate(shocks):
            h, ref = run(docs, inputs, shock)
            assert abs(h.loglik - bh.loglik[gi]) < 1e-9 * abs(h.loglik)
            assert [(r["room"], r["cause"], r["step"]) for r in h.resets] == [(r["room"], r["cause"], r["step"]) for r in bh.resets(gi)]
            causes |= {r["cause"] for r in h.resets}
            for n in NAMES:
                for key in ("p_window", "lam", "p_supply", "a"):
                    np.testing.assert_allclose(rec[gi][n][key], ref[n][key], rtol=0, atol=1e-9, equal_nan=True)
    assert causes >= {"gap", "innovation", "lambda"}


def test_fast_shock_fit_replays_the_reference_coordinate_descent():
    docs, inputs, g = sim_grid(600)
    rc = copy.deepcopy(CONFIG["roommodel"])
    rc["house"] = dict(HOUSE)
    rc["shock"].update(passes=2, grid={"q_calm": [0.003, 0.03], "p_jump": [1e-4, 1e-2], "q_jump": [0.6]})
    drop = {n: np.arange(g.n) % 97 < 5 for n in NAMES}

    def ref_nll(vals):
        h = House(docs, shock_defaults(rc, **vals), rc["house"])
        for k, row in enumerate(inputs):
            h.step({n: RoomIn(math.nan, math.nan, r.u, r.sp, r.tout, r.wind, r.rad, r.night, r.ncnt, r.nsum, tuple(math.nan for _ in r.ya), math.nan, math.nan, r.th) if drop[n][k] else r for n, r in row.items()})
        return -h.loglik

    best = dict(rc["shock"]["start"])
    score = ref_nll(best)
    trace = [(dict(best), score)]
    for _ in range(rc["shock"]["passes"]):
        for key, values in rc["shock"]["grid"].items():
            for v in values:
                if v != best[key]:
                    trial = dict(best, **{key: v})
                    s = ref_nll(trial)
                    trace.append((trial, s))
                    best, score = (trial, s) if s < score else (best, score)
    for batch_max in (64, 1):
        rc["shock"]["batch_max"] = batch_max
        fb, fs, ft = fit_shock(g, docs, rc, drop)
        assert fb == best and abs(fs - score) < 1e-9 * abs(score)
        assert [t for t, _ in ft] == [t for t, _ in trace]
        np.testing.assert_allclose([s for _, s in ft], [s for _, s in trace], rtol=1e-12)
