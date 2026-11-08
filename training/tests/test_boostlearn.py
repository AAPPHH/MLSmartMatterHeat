import copy
import json
from datetime import datetime, timezone
import numpy as np
import pyarrow as pa
import pyarrow.ipc as ipc
import pytest
from heattrain.__main__ import CONFIG
from datetime import timedelta
from heattrain.boostlearn import Dosing, Posterior, Trust, boostcheck, boostlearn, boostmark, closed_loop, kernels, needs_retrain, ramp, rollout

T0 = datetime(2026, 11, 1, 0, 0, tzinfo=timezone.utc).timestamp()
DAYS = 6
TRUE = {"G": 0.4, "m": 15.0, "a": 0.003, "delta": 5.0}
CFG = copy.deepcopy(CONFIG)
CFG["window"]["rooms"] = ["wohnzimmer", "bad"]
CFG["window"]["nodes"] = {"wohnzimmer": 5, "bad": 7}
CFG["boost"]["n_samples"] = 24
L, SHAPE = CFG["boost"]["kernel_len_min"], CFG["boost"]["kernel_shape"]


def write(root, table, data, schema, name="a"):
    d = root / "raw" / table
    d.mkdir(parents=True, exist_ok=True)
    t = pa.table(data, schema=schema)
    with open(d / f"{name}.arrows", "wb") as f, ipc.new_stream(f, t.schema) as w:
        w.write_table(t)


def stamps(minutes):
    return [datetime.fromtimestamp(T0 + 60 * m, timezone.utc) for m in minutes]


def simulate(seed, g):
    rng = np.random.default_rng(seed)
    n = DAYS * 1440
    valve = np.full(n, 12.0)
    for s in range(300, n - 200, 260):
        valve[s:s + int(rng.integers(6, 16))] = rng.choice([60.0, 100.0])
    u = np.clip((valve / 100 - CFG["boost"]["dead_zone"]) / (1 - CFG["boost"]["dead_zone"]), 0, 1)
    ext = np.concatenate([np.zeros(L), u])
    T = rollout(np.log([[g, TRUE["m"]]]), TRUE["a"], TRUE["delta"], 19.5, 5.0, ext, L, SHAPE)[0]
    return valve, np.round(T * 2) / 2


@pytest.fixture
def lake(tmp_path):
    rows = []
    for room, node, g in (("wohnzimmer", 5, TRUE["G"]), ("bad", 7, 0.15)):
        valve, temp = simulate(node, g)
        for m in range(len(valve)):
            rows += [(m, node, 0, int(temp[m] * 100)), (m, node, 8, int(valve[m]))] + ([(m, node, 18, 2000)] if m % 300 == 0 else [])
    schema = pa.schema([("ts", pa.timestamp("us", "UTC")), ("node", pa.uint64()), ("cluster", pa.uint32()), ("attribute", pa.uint32()), ("v_i64", pa.int64())])
    write(tmp_path, "attribute_reports", [stamps([r[0] for r in rows]), [r[1] for r in rows], [513] * len(rows), [r[2] for r in rows], [r[3] for r in rows]], schema)
    write(tmp_path, "events", [stamps([]), [], [], []], pa.schema([("ts", pa.timestamp("us", "UTC")), ("kind", pa.string()), ("node", pa.uint64()), ("detail", pa.string())]))
    write(tmp_path, "decisions", [stamps([]), [], [], [], []], pa.schema([("ts", pa.timestamp("us", "UTC")), ("room", pa.string()), ("rule", pa.string()), ("setpoint_new", pa.float64()), ("applied", pa.bool_())]))
    write(tmp_path, "weather", [stamps([0, 60]), ["obs", "obs"], [5.0, 5.0], [2.0, 2.0], [0.0, 0.0]], pa.schema([("ts", pa.timestamp("us", "UTC")), ("kind", pa.string()), ("temp_c", pa.float32()), ("wind_ms", pa.float32()), ("radiation_wm2", pa.float32())]))
    return tmp_path


def add_cycle(lake, minute, room="wohnzimmer", **detail):
    d = dict({"room": room, "variant": "grey_box", "mode": "live", "cycle_id": str(minute), "start_ts_ms": int((T0 + 60 * minute) * 1000), "target": 20.0}, **detail)
    schema = pa.schema([("ts", pa.timestamp("us", "UTC")), ("kind", pa.string()), ("node", pa.uint64()), ("detail", pa.string())])
    write(lake, "events", [stamps([minute]), ["boost_cycle"], [5], [json.dumps(d)]], schema, f"c{minute}")


def add_window(lake, kind, minute, room="wohnzimmer", **detail):
    schema = pa.schema([("ts", pa.timestamp("us", "UTC")), ("kind", pa.string()), ("node", pa.uint64()), ("detail", pa.string())])
    write(lake, "events", [stamps([minute]), [kind], [5], [json.dumps(dict({"room": room}, **detail))]], schema, f"w{kind}{minute}")


def bundle_from(lake, tmp_path, **boost):
    cfg = copy.deepcopy(CFG)
    cfg["boost"].update(boost)
    out, bundle, meta = boostlearn(cfg, lake, tmp_path / "models", validate=True)
    return cfg, out, bundle, meta


def synthetic(table, g=0.4):
    th = np.array([[np.log(g), np.log(15.0)]] * 16) + np.random.default_rng(0).normal(0, 0.05, (16, 2))
    return {"seed": 0, "horizon_min": 120, "ramp_tau_min": 10.0, "since_open_cap_min": 360, "abort_c": 24.0, "kernel": {"shape": SHAPE, "len_min": L}, "dose": {"min_min": 5.0, "max_min": 20.0, "step_min": 0.1}, "scenarios": ["morning", "homecoming"],
            "risk": {"p_max": 0.10, "margin_k": 0.5, "peak_sigma_k": 0.3, "thompson_margin_k": 0.25}, "rooms": {"wohnzimmer": {"a": 0.003, "delta": 5.0, "pi": {"kp": 0.3, "gap0": 0.5}, "samples": th.tolist()}},
            "trust": {"features": [], "mean": [0.0] * 12, "std": [1.0] * 12, "weights": [1.0] * 12, "scenario_penalty": 2.0, "k": 5, "radius": 2.5, "growth": 1.1, "episodes": table}}


def episode(room="wohnzimmer", scenario="morning", dose=4.0, **x):
    f = dict(out=5.0, wind=3.0, rad=0.0, start_temp=18.0, gap=2.0, supply_ok=1.0, since_resume=60.0, vm60=0.0, vm180=0.0, since_open=360.0, hs=0.0, hc=1.0)
    f.update(x)
    return {"room": room, "scenario": scenario, "x": list(f.values()), "dose": dose, "boost": False}


def ctx(**x):
    c = dict(out=5.0, wind=3.0, rad=0.0, start_temp=18.0, gap=2.0, supply_ok=1.0, since_resume=60.0, vm60=0.0, vm180=0.0, since_open=360.0, hour=0.0)
    c.update(x)
    return c


def test_response_monotone_in_valve_and_gain():
    rng = np.random.default_rng(1)
    th = np.log(np.column_stack([rng.uniform(0.01, 1.0, 16), rng.uniform(3, 60, 16)]))
    worst = 0.0
    for _ in range(200):
        u1 = rng.uniform(0, 1, L + 100) * (rng.uniform(size=L + 100) < 0.3)
        u2 = np.clip(u1 + rng.uniform(0, 0.6, L + 100) * (rng.uniform(size=L + 100) < 0.4), 0, 1)
        t0, tout = rng.uniform(0, 30), rng.uniform(-20, 35)
        a = rollout(th, 0.004, 5.0, t0, tout, u1, L, SHAPE)
        b = rollout(th, 0.004, 5.0, t0, tout, u2, L, SHAPE)
        worst = max(worst, float((a - b).max()))
        bigger = th.copy()
        bigger[:, 0] += 0.5
        worst = max(worst, float((rollout(th, 0.004, 5.0, t0, tout, u2, L, SHAPE) - rollout(bigger, 0.004, 5.0, t0, tout, u2, L, SHAPE)).max()))
    assert worst <= 1e-9
    assert np.allclose(kernels([10.0, 40.0], L, SHAPE).sum(axis=1), 1.0) and (kernels([10.0], L, SHAPE) >= 0).all()
    assert ramp(0, 10) == 0 and ramp(1000, 10) > 0.999


def test_safe_dose_bound_and_monotone_in_dose():
    d = Dosing(synthetic([]))
    pk = d.peaks("wohnzimmer", ctx(), 20.0)
    assert (np.diff(pk, axis=0) >= -1e-9).all() and len(d.cand) == 151
    c = ctx(start_temp=14.0, gap=6.0)
    p, _ = d.risk("wohnzimmer", c, 20.0)
    dec = d.decide("wohnzimmer", c, "homecoming", 20.0)
    assert (np.diff(p) >= -1e-12).all()
    i = int(round((dec["d_safe"] - 5) / 0.1))
    assert dec["p_no_boost"] > 0.5 and abs(dec["d_safe"] - 20.0) < 1e-6
    if dec["d_safe"]:
        assert p[i] - dec["p_no_boost"] <= 0.10
    if 0 < dec["d_safe"] and i + 1 < len(p):
        assert p[i + 1] - dec["p_no_boost"] > 0.10
    hot = d.decide("wohnzimmer", ctx(start_temp=19.4, gap=0.6), "morning", 20.0)
    j = int(round((hot["d_safe"] - 5) / 0.1))
    pk, _ = d.risk("wohnzimmer", ctx(start_temp=19.4, gap=0.6), 20.0)
    assert hot["d_safe"] == 0.0 or (pk[j] - hot["p_no_boost"] <= 0.10 and (j + 1 >= len(pk) or pk[j + 1] - hot["p_no_boost"] > 0.10))
    weak = Dosing(synthetic([], g=0.05)).decide("wohnzimmer", c, "homecoming", 20.0)
    assert weak["d_safe"] >= dec["d_safe"]
    assert d.decide("wohnzimmer", ctx(gap=0.2, start_temp=21.0), "morning", 21.2)["allowed"] == 0.0


def test_closed_loop_thermostat_continues_after_boost():
    th = np.array([[np.log(0.2), np.log(15.0)]])
    hist, ramp_f = np.zeros(L), np.ones(200)
    off = closed_loop(th, 0.003, 5.0, 18.0, 5.0, hist, [5.0], 20.0, {"kp": 0.0, "gap0": 0.0}, ramp_f, L, SHAPE)
    on = closed_loop(th, 0.003, 5.0, 18.0, 5.0, hist, [5.0], 20.0, {"kp": 1.0, "gap0": 0.0}, ramp_f, L, SHAPE)
    u = np.zeros(200)
    u[:5] = 1.0
    ref = rollout(th, 0.003, 5.0, 18.0, 5.0, np.concatenate([hist, u]), L, SHAPE)[0].max()
    assert abs(off[0, 0] - ref) < 1e-9 and on[0, 0] > off[0, 0] + 0.1
    more = closed_loop(th, 0.003, 5.0, 18.0, 5.0, hist, [5.0, 8.0], 20.0, {"kp": 1.0, "gap0": 0.0}, ramp_f, L, SHAPE)
    assert more[1, 0] >= more[0, 0]


def test_only_morning_and_homecoming_boost():
    d = Dosing(synthetic([], g=0.05))
    c = ctx(start_temp=14.0, gap=6.0)
    assert d.decide("wohnzimmer", c, "homecoming", 20.0)["allowed"] >= 5.0 and d.decide("wohnzimmer", c, "morning", 20.0)["allowed"] >= 5.0
    for scn in ("other", "room_change", "", "boost"):
        r = d.decide("wohnzimmer", c, scn, 20.0)
        assert r["allowed"] == 0.0 and r["chosen"] == 0.0
    d.b["scenarios"] = ["morning"]
    assert d.decide("wohnzimmer", c, "homecoming", 20.0)["allowed"] == 0.0


def test_no_similar_episode_gives_minimum_or_none():
    d = Dosing(synthetic([]))
    c = ctx(start_temp=14.0, gap=6.0)
    dec = d.decide("wohnzimmer", c, "homecoming", 20.0)
    assert dec["known"] is None and dec["allowed"] in (0.0, 5.0)
    assert d.decide("wohnzimmer", ctx(start_temp=14.0, gap=6.0, supply_ok=0.0, since_resume=0.0), "homecoming", 20.0)["allowed"] == 0.0
    weak = Dosing(synthetic([episode(room="bad", dose=12.0)], g=0.05))
    weak.b["trust"]["radius"] = 5.0
    r = weak.decide("wohnzimmer", ctx(), "morning", 20.0)
    assert r["known"] is None and r["allowed"] == 5.0 and r["chosen"] <= 5.0


def test_trust_region_never_above_growth_times_known_own_room_only():
    table = [episode(dose=7.0), episode(dose=9.0, out=5.5), episode(room="bad", dose=40.0, scenario="morning"), episode(dose=60.0, scenario="homecoming", out=-10.0)]
    d = Dosing(synthetic(table, g=0.05))
    d.b["trust"]["radius"] = 0.9
    dec = d.decide("wohnzimmer", ctx(out=5.2), "morning", 20.0)
    assert dec["known"] == 9.0 and dec["n_similar"] == 2
    assert dec["allowed"] <= 1.1 * 9.0 + 1e-9
    other = Dosing(synthetic([episode(room="bad", dose=8.0)], g=0.05))
    other.b["trust"]["radius"] = 3.0
    o = other.decide("wohnzimmer", ctx(), "morning", 21.0)
    assert o["known"] is None and o["allowed"] == 5.0
    weak_known = Dosing(synthetic([episode(dose=3.0)], g=0.05))
    weak_known.b["trust"]["radius"] = 3.0
    assert weak_known.decide("wohnzimmer", ctx(), "morning", 21.0)["allowed"] == 5.0
    rng = np.random.default_rng(0)
    for _ in range(40):
        c = ctx(out=rng.uniform(-5, 15), start_temp=rng.uniform(12, 20), gap=rng.uniform(0.5, 6), hour=rng.uniform(0, 24))
        r = d.decide("wohnzimmer", c, str(rng.choice(["morning", "homecoming"])), c["start_temp"] + c["gap"])
        cap = 5.0 if r["known"] is None else max(5.0, 1.1 * r["known"])
        assert r["allowed"] <= cap + 1e-9 and r["chosen"] <= r["allowed"] + 1e-9 and (r["allowed"] == 0 or r["allowed"] >= 5.0)
        assert r["known"] is None or r["known"] in (7.0, 9.0, 60.0)


def test_dose_escalates_by_ten_percent_per_safe_cycle():
    d = Dosing(synthetic([], g=0.05))
    d.b["trust"]["radius"] = 0.9
    c = ctx(start_temp=18.0, gap=2.0)
    doses = []
    for _ in range(7):
        r = d.decide("wohnzimmer", c, "morning", 20.0)
        assert r["allowed"] > 0 and r["chosen"] == r["allowed"] and r["d_safe"] >= r["allowed"]
        doses.append(r["chosen"])
        d.b["trust"]["episodes"].append(episode(dose=r["chosen"]))
        d.trust = None
    assert doses[0] == 5.0 and abs(doses[1] - 5.5) < 1e-9 and abs(doses[2] - 6.05) < 1e-9
    assert abs(doses[6] - 5.0 * 1.1 ** 6) < 1e-6 * doses[6]


def test_trust_table_distance_prefers_same_scenario_and_own_room():
    base = dict(out=5.0, wind=3.0, rad=0.0, start_temp=18.0, gap=2.0, supply_ok=1.0, since_resume=60.0, vm60=0.0, vm180=0.0, since_open=360.0, hour=0.0)
    tr = Trust([{"room": "wohnzimmer", "scenario": "morning", "ctx": base, "dose": 3.0}] * 4 + [{"room": "bad", "scenario": "homecoming", "ctx": base, "dose": 30.0}], CFG["boost"])
    z = (np.array([5.0, 3.0, 0.0, 18.0, 2.0, 1.0, 60.0, 0.0, 0.0, 360.0, 0.0, 1.0]) - tr.mean) / tr.std
    d = tr.dist(z, "morning")
    assert d[0] < d[4]
    assert tr.known(base, "wohnzimmer", "morning", radius=10.0)["dose"] == 3.0
    assert tr.known(base, "bad", "morning", radius=10.0)["dose"] == 30.0
    assert tr.known(base, "kueche", "morning", radius=10.0)["dose"] is None


def test_posterior_samples_respect_bounds():
    post = Posterior(CFG["boost"])
    fit = {"theta": np.array([np.log(4.9), np.log(78.0)]), "cov": np.diag([1.0, 0.5])}
    s = post.samples(fit, 200, np.random.default_rng(0))
    assert len(s) == 200 and (s >= post.lo - 1e-12).all() and (s <= post.hi + 1e-12).all()
    assert np.exp(s[:, 1]).max() <= CFG["boost"]["delay_bounds"][1] + 1e-9


def test_export_bundle_and_keep(lake, tmp_path):
    cfg, out, bundle, meta = bundle_from(lake, tmp_path)
    assert (out / "bundle.json").exists() and (out / "meta.json").exists()
    on_disk = json.loads((out / "bundle.json").read_text())
    assert on_disk["kind"] == "boost_learn" and set(on_disk["rooms"]) == {"wohnzimmer", "bad"} and on_disk["scenarios"] == ["morning", "homecoming"]
    assert on_disk["trust"]["episodes"] and set(on_disk["trust"]) >= {"mean", "std", "weights", "radius", "k", "growth", "episodes"}
    assert set(on_disk["rooms"]["bad"]["pi"]) >= {"kp", "gap0"} and on_disk["rooms"]["bad"]["peak_sigma_k"] >= cfg["boost"]["peak_sigma_floor"]
    g = np.exp(np.array(on_disk["rooms"]["wohnzimmer"]["samples"])[:, 0])
    assert 0.2 < np.median(g) < 0.8
    assert np.median(np.exp(np.array(on_disk["rooms"]["bad"]["samples"])[:, 0])) < np.median(g)
    assert meta["episodes"]["wohnzimmer"] > 3 and meta["first_boost"]["wohnzimmer"]["morning"]["allowed"] >= 0
    assert meta["validation"]["monotone"]["max_violation_state_k"] <= 1e-9 and meta["validation"]["monotone"]["max_violation_dose_k"] <= 1e-9
    assert meta["validation"]["bounds"]["violations"] == 0
    loeo = meta["validation"]["loeo"]["wohnzimmer"]
    assert loeo["coverage90"] > 0.6 and loeo["mae_k"] < 1.5 and loeo["sigma_k"] == on_disk["rooms"]["wohnzimmer"]["peak_sigma_k"]
    cfg["boost"].update(keep_bundles=3, eb_iter=1)
    for i in range(5):
        boostlearn(cfg, lake, tmp_path / "models", version=f"20261101T0000{i:02d}Z")
    names = sorted(p.name for p in (tmp_path / "models" / "boost").iterdir())
    assert len(names) == 3 and "20261101T000004Z" in names and "20261101T000000Z" not in names
    assert CONFIG["boost"]["keep_bundles"] == 14


def test_boost_cycle_known_only_without_overshoot_and_dose_is_boost_minutes(lake, tmp_path):
    base = bundle_from(lake, tmp_path)[2]
    add_cycle(lake, 300 + 260 * 3, overshoot_c=0.1, scenario="morning", target=30.0, boost_min=7.0)
    add_cycle(lake, 300 + 260 * 4, overshoot_c=2.0, scenario="homecoming", target=30.0, boost_min=7.0)
    add_cycle(lake, 300 + 260 * 5, scenario="morning", target=30.0, boost_min=7.0)
    cfg, out, bundle, meta = bundle_from(lake, tmp_path / "b")
    assert meta["boost_cycles"]["wohnzimmer"] == 3
    boosts = [e for e in bundle["trust"]["episodes"] if e["boost"]]
    assert len(boosts) == 1 and boosts[0]["scenario"] == "morning" and boosts[0]["dose"] == 7.0
    assert len(bundle["trust"]["episodes"]) <= len([e for e in base["trust"]["episodes"] if not e["boost"]]) + 1


def test_pulses_within_the_window_block_are_not_known_and_counted(lake, tmp_path):
    at = 300 + 260 * 3
    add_cycle(lake, at, overshoot_c=0.1, scenario="morning", target=30.0, boost_min=7.0)
    add_cycle(lake, at + 260, overshoot_c=0.1, scenario="morning", target=30.0, boost_min=7.0)
    add_window(lake, "window_open", at - 140)
    add_window(lake, "window_close", at - 120)
    _, _, bundle, meta = bundle_from(lake, tmp_path)
    boosts = [e for e in bundle["trust"]["episodes"] if e["boost"]]
    assert len(boosts) == 1
    assert meta["after_window_excluded"]["wohnzimmer"] >= 1 and meta["after_window_excluded"]["bad"] == 0
    none = bundle_from(lake, tmp_path / "b", after_window_min=0)[3]
    assert none["after_window_excluded"]["wohnzimmer"] < meta["after_window_excluded"]["wohnzimmer"]


def test_ground_truth_windows_also_block(lake, tmp_path):
    at = 300 + 260 * 3
    add_cycle(lake, at, overshoot_c=0.1, scenario="morning", target=30.0, boost_min=7.0)
    gt = lake / "ground_truth"
    gt.mkdir()
    a = datetime.fromtimestamp(T0 + 60 * (at - 200), timezone.utc).isoformat()
    b = datetime.fromtimestamp(T0 + 60 * (at - 100), timezone.utc).isoformat()
    (gt / "windows.jsonl").write_text(json.dumps({"open_utc": a, "close_utc": b, "rooms_open": ["wohnzimmer"]}))
    _, _, bundle, meta = bundle_from(lake, tmp_path)
    assert not [e for e in bundle["trust"]["episodes"] if e["boost"]]
    assert meta["after_window_excluded"]["wohnzimmer"] >= 1


def test_measured_overshoot_excludes_episodes(lake, tmp_path):
    add_cycle(lake, 300 + 260 * 3, overshoot_c=0.0, scenario="morning", target=13.0, boost_min=7.0)
    _, _, bundle, _ = bundle_from(lake, tmp_path)
    assert not [e for e in bundle["trust"]["episodes"] if e["boost"]]


def test_retrain_trigger(lake, tmp_path):
    add_cycle(lake, 400, room="wohnzimmer", mode="shadow")
    assert not needs_retrain(lake, tmp_path / "m0")
    models = tmp_path / "models"
    assert boostcheck(CFG, lake, models) == 0
    add_cycle(lake, 500)
    assert needs_retrain(lake, models) and boostcheck(CFG, lake, models) == 10
    out = boostlearn(CFG, lake, models)[0]
    assert not needs_retrain(lake, models)
    old = models / "boost" / "20200101T000000Z"
    out.rename(old)
    assert needs_retrain(lake, models)
    (models / "boost" / "20991231T000000Z").mkdir()
    (models / "boost" / "20991231T000000Z" / "bundle.json").write_text("{}")
    assert not needs_retrain(lake, models)


def test_failed_training_backs_off_and_doubles_up_to_cap(lake, tmp_path):
    models = tmp_path / "models"
    add_cycle(lake, 500)
    now = datetime(2026, 11, 7, 12, 0, tzinfo=timezone.utc)
    assert boostcheck(CFG, lake, models, now) == 10
    delays = []
    for _ in range(9):
        delays.append(boostmark(CFG, models, False, now)["delay_min"])
        assert boostcheck(CFG, lake, models, now + timedelta(minutes=delays[-1] - 1)) == 0
        assert boostcheck(CFG, lake, models, now + timedelta(minutes=delays[-1] + 1)) == 10
    assert delays == [10, 20, 40, 80, 160, 320, 360, 360, 360]
    boostmark(CFG, models, True)
    assert boostcheck(CFG, lake, models, now) == 10 and not (models / "boost" / "retry.json").exists()
    assert boostmark(CFG, models, False, now)["delay_min"] == 10
