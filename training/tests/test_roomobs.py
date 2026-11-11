import math
import numpy as np
import pytest
from heattrain.roombatch import run_batch
from heattrain.roomimm import House, RoomIn
from heattrain.roomobs import abs_hum, bin_prob, check_house, check_room_doc, obs_update, trv_update
from room_sim import CO2, EFFORT, HOUSE, HUM, NAMES, SHOCK, simulate_house
from test_roommodel import run, sim_grid


def without(docs, *keys):
    return {n: {k: v for k, v in d.items() if k not in keys} for n, d in docs.items()}


def first(x, t0, thr=0.5):
    hit = np.nonzero(x[t0:] > thr)[0]
    return int(hit[0]) if len(hit) else None


def test_interval_probability_and_tails_are_accurate():
    assert abs(bin_prob(-1.0, 1.0) - 0.6826894921) < 2e-7
    assert abs(bin_prob(2.0, 3.0) - 0.0214002) < 1e-7
    assert abs(bin_prob(-3.0, -2.0) - 0.0214002) < 1e-7
    assert bin_prob(40.0, 41.0) == 0.0 and bin_prob(8.0, 9.0) > 0.0
    assert abs(abs_hum(20.0, 50.0) - 8.6) < 0.1 and math.isnan(abs_hum(math.nan, 50.0))


def test_repeated_identical_readings_do_not_make_the_sensor_overconfident():
    w, grid, noise = 0.3, 0.5, 0.15
    h = (w, 1 - w, 0.0)
    x, p = [20.0, 20.0, 1.0], [1.0, 0, 0, 0, 1.0, 0, 0, 0, 0.01]
    xg, pg = list(x), list(p)
    sd = lambda p: math.sqrt(w * w * p[0] + 2 * w * (1 - w) * p[1] + (1 - w) ** 2 * p[4])
    for _ in range(60):
        ll, _ = trv_update(x, p, h, 20.0, w * x[0] + (1 - w) * x[1], noise ** 2, grid, HOUSE["trv_floor"], grid ** 2 / 12 + noise ** 2)
        obs_update(xg, pg, h, 20.0 - (w * xg[0] + (1 - w) * xg[1]), grid ** 2 / 12 + noise ** 2)
    floor = math.sqrt(HOUSE["trv_floor"] * grid ** 2 / 12)
    assert floor * 0.95 < sd(p) < floor * 1.3
    assert sd(pg) < 0.5 * sd(p)
    assert -0.2 < ll <= 0.0 and abs(x[0] - 20.0) < 1e-6


def test_quantized_staircase_gives_no_false_jump_and_fewer_alarms_than_the_gaussian_grid_noise():
    for seed in (1, 2):
        docs, inputs, _ = simulate_house(700, seed=seed, sp_fn=lambda k: 12.0, report_p=1.0)
        peak = {}
        for key, house in (("interval", HOUSE), ("gauss", dict(HOUSE, trv_grid=0.0))):
            h, rec = run(docs, inputs, house=house)
            assert not h.resets
            peak[key] = max(rec[n]["p_window"][60:].max() for n in NAMES)
        assert peak["interval"] < 0.1 and peak["interval"] < peak["gauss"]


def test_window_while_heating_is_found_through_the_valve_effort_the_closed_loop_hides():
    for seed in (0, 1):
        docs, inputs, _ = simulate_house(900, seed=seed, window={"kueche": (500, 700, 2.0)}, gain=1.0, pi=10.0, w=0.8)
        for d in docs.values():
            d["effort"] = dict(EFFORT, sigma=0.04)
        _, on = run(docs, inputs)
        _, off = run(without(docs, "effort"), inputs)
        assert on["kueche"]["p_window"][500:700].max() > 0.5 > off["kueche"]["p_window"][500:700].max()
        assert max(on[n]["p_window"][100:480].max() for n in NAMES) < 0.5
        assert max(on[n]["p_window"][700:].max() for n in NAMES if n != "kueche") < 0.5


def test_saturated_or_missing_valve_demand_is_dropped():
    docs, inputs, _ = simulate_house(500, seed=0, window={"kueche": (300, 400, 3.0)}, gain=1.0, pi=10.0)
    _, base = run(docs, inputs)
    for value in (1.0, 0.0):
        sat = [{n: RoomIn(*[value if f == "yv" and math.isfinite(r.yv) else getattr(r, f) for f in ("y", "yv", "u", "sp", "tout", "wind", "rad", "night", "ncnt", "nsum")]) for n, r in row.items()} for row in inputs]
        none = [{n: RoomIn(*[math.nan if f == "yv" else getattr(r, f) for f in ("y", "yv", "u", "sp", "tout", "wind", "rad", "night", "ncnt", "nsum")]) for n, r in row.items()} for row in inputs]
        _, a = run(docs, sat)
        _, b = run(docs, none)
        for n in NAMES:
            np.testing.assert_allclose(a[n]["p_window"], b[n]["p_window"], atol=1e-12, equal_nan=True)
    assert any(not np.allclose(base[n]["p_window"], b[n]["p_window"], equal_nan=True) for n in NAMES)


def test_a_fast_co2_fall_from_an_elevated_level_is_a_window():
    gas = {"schlafzimmer": {"c0": 1100.0, "co2_src": [(0, 2000, 3.0)], "ah0": 8.0}}
    for seed in (0, 1):
        docs, inputs, _ = simulate_house(900, seed=seed, window={"schlafzimmer": (600, 700, 3.0)}, gas=gas)
        _, on = run(docs, inputs)
        _, off = run(without(docs, "co2", "hum"), inputs)
        s = "schlafzimmer"
        t_on, t_off = first(on[s]["p_window"], 600), first(off[s]["p_window"], 600)
        assert t_on is not None and t_on <= 15 and (t_off is None or t_off >= t_on + 15)
        assert on[s]["p_window"][100:590].max() < 0.2


def test_a_humidity_rise_while_showering_is_not_a_window():
    for seed in (0, 1, 2):
        gas = {"kueche": {"c0": 600.0, "ah0": 8.0, "ah_src": [(600, 612, 0.25)]}}
        docs, inputs, _ = simulate_house(900, seed=seed, gas=gas)
        h, rec = run(docs, inputs)
        assert not h.resets and np.isfinite(rec["kueche"]["p_window"][100:]).all()
        assert rec["kueche"]["p_window"][100:].max() < 0.2


def test_missing_and_dropped_extra_sensors_leave_every_room_working():
    docs, inputs, truth = simulate_house(700, seed=2, air={"kueche": {"p": 0.5}})
    drop = [{n: (RoomIn(r.y, r.yv, r.u, r.sp, r.tout, r.wind, r.rad, r.night, r.ncnt, r.nsum, (math.nan,)) if n == "kueche" and 300 <= k < 450 else r) for n, r in row.items()} for k, row in enumerate(inputs)]
    short = [{n: (RoomIn(r.y, r.yv, r.u, r.sp, r.tout, r.wind, r.rad, r.night, r.ncnt, r.nsum, ()) if n == "kueche" else r) for n, r in row.items()} for row in inputs]
    for rows in (drop, short):
        h, rec = run(docs, rows)
        assert not h.resets
        for n in NAMES:
            assert np.isfinite(rec[n]["a"][60:]).all() and rec[n]["p_window"][60:].max() < 0.5
    assert all(r.ya == () for row in inputs for n, r in row.items() if n != "kueche")
    _, with_air = run(docs, inputs)
    _, no_air = run(without(docs, "air"), inputs)
    err = lambda rec: np.sqrt(np.mean((rec["kueche"]["a"][100:] - np.array(truth["kueche"]["a"])[100:]) ** 2))
    assert err(with_air) < 0.8 * err(no_air)


def test_batched_filter_matches_the_reference_with_every_new_observation():
    gas = {"schlafzimmer": {"c0": 1100.0, "co2_src": [(0, 2000, 3.0)], "ah0": 8.0, "ah_src": [(250, 262, 0.25)]}}
    kw = {"window": {"schlafzimmer": (400, 480, 3.0), "kueche": (300, 380, 4.0)}, "air": {"kueche": {"p": 0.5}}, "gas": gas, "pi": 3.0, "gain": 1.0, "corrupt": (550, 6, 6.0), "gap": (600, 640)}
    docs, inputs, g = sim_grid(720, seed=3, **kw)
    bh, rec = run_batch(g, docs, [SHOCK], HOUSE, record=True)
    h, ref = run(docs, inputs)
    assert abs(h.loglik - bh.loglik[0]) < 1e-9 * abs(h.loglik)
    assert [(r["room"], r["cause"], r["step"]) for r in h.resets] == [(r["room"], r["cause"], r["step"]) for r in bh.resets(0)]
    for n in NAMES:
        for key in ("p_window", "lam", "p_supply", "a"):
            np.testing.assert_allclose(rec[0][n][key], ref[n][key], rtol=0, atol=1e-9, equal_nan=True)


def test_bundle_validation_rejects_bad_observation_parameters():
    base = {"air": [{"node": 11, "noise": 0.1, "offset": 0.0}], "effort": dict(EFFORT), "co2": dict(CO2), "hum": dict(HUM)}
    check_room_doc("r", base)
    check_house(dict(HOUSE))
    bad = [("air", [{"noise": 0.0}]), ("air", [{"noise": math.nan}]), ("effort", dict(EFFORT, sigma=-1.0)), ("effort", {"sigma": 0.1}), ("co2", dict(CO2, k=-0.1)), ("co2", dict(CO2, win_min=0)),
           ("co2", dict(CO2, win_min=2.5)), ("co2", {k: v for k, v in CO2.items() if k != "out"}), ("hum", dict(HUM, rh_out=120.0)), ("hum", dict(HUM, sigma_up=math.nan))]
    for key, value in bad:
        with pytest.raises(ValueError):
            check_room_doc("r", dict(base, **{key: value}))
    for bad_house in (dict(HOUSE, trv_grid=-0.5), dict(HOUSE, trv_floor=math.nan)):
        with pytest.raises(ValueError):
            check_house(bad_house)
    with pytest.raises(ValueError):
        House({"r": dict(simulate_house(5)[0]["wohnzimmer"], air=[{"noise": -1.0}])}, SHOCK, HOUSE)


def write_arrows(path, columns):
    import pyarrow as pa
    import pyarrow.ipc as ipc
    path.parent.mkdir(parents=True, exist_ok=True)
    table = pa.table(columns)
    with open(path, "wb") as fh, ipc.new_stream(fh, table.schema) as w:
        w.write_table(table)


def test_device_readings_become_air_sensors_and_gas_series_per_room(tmp_path):
    import copy
    from datetime import datetime, timezone
    from heattrain.__main__ import CONFIG
    from heattrain.thermal import build_rooms, load_devices, load_lake
    t0, minutes = 1_790_000_000.0, 300
    stamp = lambda t: datetime.fromtimestamp(t, timezone.utc)
    rep = [(t0 + 60 * i, node, a, v) for i in range(minutes) for node in (5, 6) for a, v in ((0, 2000.0 + 50 * (i // 40 % 2)), (8, 40.0))]
    rows = {"ts": [], "node": [], "cluster": [], "attribute": [], "value": []}
    for i in range(0, minutes, 2):
        for node, cluster, attr, v in ((11, 0x0402, 0, 2105.0), (12, 0x0402, 0, 1950.0 + i), (12, 0x0405, 0, 4500.0), (12, 0x040D, 0, 800.0 + i), (12, 0x040D, 3, 1.0), (9, 0x0006, 0, 1.0)):
            for k, x in zip(rows, (stamp(t0 + 60 * i), node, cluster, attr, v)):
                rows[k].append(x)
    write_arrows(tmp_path / "raw" / "device_readings" / "a.arrows", {"ts": rows["ts"], "node": rows["node"], "cluster": rows["cluster"], "attribute": rows["attribute"], "value": rows["value"]})
    devices = load_devices(tmp_path)
    assert {r[2] for r in devices} == {0x0402, 0x0405, 0x040D} and len(devices) == 4 * 150
    cfg = copy.deepcopy(CONFIG["thermal"])
    meta = [("rooms", '[{"name":"kueche","nodes":[5]},{"name":"schlafzimmer","nodes":[6]}]', t0), ("devices", '[{"node":11,"room":"kueche"},{"node":12,"room":"schlafzimmer"},{"node":9,"room":"kueche"}]', t0)]
    weather = [(t0 + 3600 * i, "obs", None, 8.0, 2.0, 0.0) for i in range(10)]
    rooms, _ = build_rooms(rep, weather, meta, cfg, (), devices)
    by = {d.room: d for d in rooms}
    k, s = by["kueche"], by["schlafzimmer"]
    assert k.ext["nodes"] == {"air": [11], "co2": None, "hum": None} and len(k.ext["ya"]) == 1
    assert np.isfinite(k.ext["ya"][0]).sum() == 150 and abs(np.nanmean(k.ext["ya"][0]) - 21.05) < 1e-9 and np.isnan(k.ext["co2"]).all()
    assert s.ext["nodes"] == {"air": [12], "co2": 12, "hum": 12}
    assert abs(s.ext["rh"][100] - 45.0) < 1e-9 and s.ext["co2"][101] == s.ext["co2"][100] and np.isfinite(s.ext["co2"][0])
    assert abs(s.ext["th"][10] - (19.5 + 10 / 100)) < 1e-9
    part = s.slice(60, 200)
    assert len(part.ext["ya"][0]) == part.n and part.ext["nodes"]["co2"] == 12
    plain, _ = build_rooms(rep, weather, meta, cfg, ())
    assert all(d.ext is None for d in plain)


def test_fit_writes_the_observation_blocks_into_the_bundle(tmp_path):
    import copy
    from heattrain.__main__ import CONFIG
    from heattrain.roomfit import fit_bundle
    from test_thermal import simulate
    config = copy.deepcopy(CONFIG)
    rm = config["roommodel"]
    rm["shock"].update(days=1, passes=1, grid={"q_calm": [0.01], "p_jump": [1e-3], "q_jump": [0.3]})
    rm["fit"].update(warm_min=120, max_nfev=20)
    rooms = []
    for i, name in enumerate(("wohnzimmer", "kueche")):
        d, _, _ = simulate(hours=30, seed=i, report_p=0.4)
        d.room, d.node = name, 5 + i
        d.sp, d.yv, d.bad = np.full(d.n, 21.0), np.where(np.isfinite(d.y), d.u, np.nan), np.zeros(d.n, bool)
        d.info["neighbour_nodes"], d.info["n_masked"] = {}, 0
        if i:
            ya = np.full(d.n, np.nan)
            ya[::3] = 20.0
            d.ext = {"ya": [ya], "co2": np.full(d.n, 800.0), "rh": np.full(d.n, 45.0), "th": np.full(d.n, 20.0), "nodes": {"air": [11], "co2": 12, "hum": 12}}
        rooms.append(d)
    bundle, _ = fit_bundle(config, rooms, tmp_path, version="20261101T000000Z")
    plain, rich = bundle["rooms"]["wohnzimmer"], bundle["rooms"]["kueche"]
    assert plain["effort"] == rm["evidence"]["effort"] and "air" not in plain and "co2" not in plain
    assert rich["air"][0]["node"] == 11 and rich["co2"]["node"] == 12 and rich["hum"]["node"] == 12 and rich["hum"]["rh_out"] == 85.0
    assert bundle["house"]["trv_grid"] == 0.5 and bundle["house"]["trv_floor"] == 0.25
    for r in bundle["rooms"].values():
        check_room_doc("r", r)
    check_house(bundle["house"])
