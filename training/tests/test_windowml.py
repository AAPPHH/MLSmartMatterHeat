import copy
import json
from datetime import datetime, timezone
import numpy as np
import pyarrow as pa
import pyarrow.ipc as ipc
import pytest
from heattrain.__main__ import CONFIG
from heattrain.windowml import Approach, Evaluator, Grid, PhysicsResidual, StreamScorer, V1Like, UnheatedFloor, budget_threshold, dilate, episodes, evaluate, parity_csv, since_open, stream_run, window_bundle

CFG = copy.deepcopy(CONFIG["window"])
CFG["rooms"] = ["wohnzimmer", "bad"]
CFG["nodes"] = {"wohnzimmer": 5, "bad": 7}
T0 = datetime(2026, 11, 6, 12, 0, tzinfo=timezone.utc).timestamp()


def write(root, table, data, schema):
    d = root / "raw" / table
    d.mkdir(parents=True)
    t = pa.table(data, schema=schema)
    with open(d / "a.arrows", "wb") as f, ipc.new_stream(f, t.schema) as w:
        w.write_table(t)


def ts(minutes):
    return [datetime.fromtimestamp(T0 + 60 * m, timezone.utc) for m in minutes]


@pytest.fixture
def lake(tmp_path):
    rep = [(0, 5, 0, 2000), (0, 5, 0, 2000), (1, 5, 8, 134), (2, 5, 8, 50), (0, 7, 0, 2100), (30, 7, 0, 2100), (30, 5, 0, 1950), (30, 5, 8, 0), (5, 5, 18, 2200)]
    schema = pa.schema([("ts", pa.timestamp("us", "UTC")), ("node", pa.uint64()), ("cluster", pa.uint32()), ("attribute", pa.uint32()), ("v_i64", pa.int64())])
    write(tmp_path, "attribute_reports", [ts([r[0] for r in rep]), [r[1] for r in rep], [513] * len(rep), [r[2] for r in rep], [r[3] for r in rep]], schema)
    det = [json.dumps({"room": "bad", "state": "start", "since_ms": (T0 + 600) * 1000}), json.dumps({"room": "bad", "state": "end", "duration_min": 10.0})]
    write(tmp_path, "events", [ts([20, 25]), ["supply", "supply"], [7, 7], det], pa.schema([("ts", pa.timestamp("us", "UTC")), ("kind", pa.string()), ("node", pa.uint64()), ("detail", pa.string())]))
    write(tmp_path, "decisions", [ts([3]), ["wohnzimmer"], ["window"], [8.0], [True]], pa.schema([("ts", pa.timestamp("us", "UTC")), ("room", pa.string()), ("rule", pa.string()), ("setpoint_new", pa.float64()), ("applied", pa.bool_())]))
    write(tmp_path, "weather", [ts([0, 60]), ["obs", "obs"], [4.0, 6.0], [1.0, 3.0], [0.0, 0.0]], pa.schema([("ts", pa.timestamp("us", "UTC")), ("kind", pa.string()), ("temp_c", pa.float32()), ("wind_ms", pa.float32()), ("radiation_wm2", pa.float32())]))
    return tmp_path


def test_loader(lake):
    g = Grid.load(lake, CFG)
    w, b = g.room["wohnzimmer"], g.room["bad"]
    assert g.t[1] - g.t[0] == 60 and len(g.t) == 31
    assert w["temp"][0] == 20.0 and w["temp"][10] == 20.0 and np.isnan(w["temp"][11]) and w["temp"][30] == 19.5
    assert np.isnan(w["valve"][1]) or w["valve"][1] != 134
    assert w["valve"][2] == 50.0 and w["valve"][30] == 0.0
    assert w["setpoint"][5] == 22.0 and np.isnan(w["setpoint"][4])
    assert w["since_open"][2] == 0 and w["since_open"][30] == 1
    assert g.shared["out"][30] == 4.0 and g.shared["out"][0] == 4.0 and g.shared["wind"][30] == 1.0
    assert b["supply_event"][20] and b["supply_event"][25] and not b["supply_event"][26] and not b["supply_event"][19]
    assert b["supply_onset"][10] and not b["supply_onset"][9]
    assert g.shared["supply_event"].sum() == 6
    assert w["frost_write"][3] and w["frost_write"][30] and not w["frost_write"][2]
    assert g.summary()["supply_episodes"] == 1


def test_since_open():
    v = np.array([np.nan, 0.0, 20.0, 5.0, 5.0, np.nan, 5.0])
    out = since_open(v, 10.0)
    assert np.isnan(out[0]) and np.isnan(out[1]) and out[2] == 0 and out[4] == 2 and np.isnan(out[5]) and out[6] == 4


def test_episodes_merge():
    m = np.zeros(200)
    m[[10, 11, 50, 51, 150]] = 5.0
    assert [tuple(e) for e in episodes(m, 1.0, 40)] == [(10, 52), (150, 151)]
    assert [tuple(e) for e in episodes(m, 1.0, 30)] == [(10, 12), (50, 52), (150, 151)]


def synth_grid(supply_at=None):
    n = 3 * 1440
    start = datetime(2026, 11, 5, 11, 0, tzinfo=timezone.utc).timestamp()
    t = start + 60.0 * np.arange(n)
    room = {r: {k: np.zeros(n) for k in ("temp", "valve", "setpoint", "since_open")} for r in ("wohnzimmer", "bad", "schlafzimmer")}
    sup = np.zeros(n, bool)
    g = Grid(t, list(room), room, {"supply_event": sup}, CFG)
    if supply_at is not None:
        sup[g.minute(supply_at):g.minute(supply_at) + 60] = True
    return g


def scores_with_bump(g, channel, at="2026-11-07T08:20:00+00:00", height=5.0, width=20):
    rng = np.random.default_rng(0)
    sc = {k: rng.uniform(0, 1, len(g.t)) for k in ("wohnzimmer", "bad", "schlafzimmer", "house")}
    i = g.minute(at)
    sc[channel][i:i + width] = height
    return sc


def test_criterion_pass_and_fail_modes():
    g = synth_grid()
    r = Evaluator(g, {"criterion": CFG["criterion"]}).run(scores_with_bump(g, "house"))
    assert r["pass"] and r["latency_min"] == 15 and r["gt_rank"] == 1 and r["alarms_in_supply"] == 0 and r["threshold"] < 1.0
    r = Evaluator(g, {"criterion": CFG["criterion"]}).run(scores_with_bump(g, "schlafzimmer"))
    assert not r["detected"] and not r["pass"]
    r = Evaluator(g, {"criterion": CFG["criterion"]}).run(scores_with_bump(g, "bad", at="2026-11-07T09:00:00+00:00"))
    assert not r["detected"]
    gs = synth_grid(supply_at="2026-11-07T08:10:00+00:00")
    r = Evaluator(gs, {"criterion": CFG["criterion"]}).run(scores_with_bump(gs, "house"))
    assert r["alarms_in_supply"] == 1 and not r["pass"]


def test_threshold_blind_to_positive_window():
    g = synth_grid()
    ev = Evaluator(g, {"criterion": CFG["criterion"]})
    a, b = scores_with_bump(g, "house", height=5.0), scores_with_bump(g, "house", height=500.0)
    assert ev.run(a)["threshold"] == ev.run(b)["threshold"]
    S = np.vstack(list(a.values())).max(axis=0)
    assert budget_threshold(S, ev.excl, 0.5, 30, 1500) == ev.run(a)["threshold"]


def test_threshold_descends_until_budget_breaks():
    rng = np.random.default_rng(1)
    m = rng.uniform(0, 1, 4 * 1440)
    thr = budget_threshold(m, np.zeros(len(m), bool), 0.5, 30, 1500)
    assert thr > 0.9 and len(episodes(m, thr, 30)) / 4 <= 0.5


def test_rank_requires_top_k():
    g = synth_grid()
    sc = scores_with_bump(g, "house", height=5.0)
    for k, at in enumerate(("2026-11-05T20:00:00+00:00", "2026-11-06T03:00:00+00:00", "2026-11-06T14:00:00+00:00")):
        i = g.minute(at)
        sc["bad"][i:i + 5] = 6.0 + k
    loose = {"criterion": dict(CFG["criterion"], rate=3.0)}
    r = Evaluator(g, loose).run(sc)
    assert r["detected"] and r["gt_rank"] == 4 and not r["pass"]


class Flat(Approach):
    refit = True

    def fit(self, grid, train_days):
        self.days = list(train_days)
        return self

    def score(self, grid, day):
        assert day not in self.days
        n = len(grid.day_idx[day])
        return {"wohnzimmer": np.zeros(n), "house": np.zeros(n)}


def test_loo_driver_and_baselines():
    g = synth_grid()
    assert evaluate(g, Flat, CFG)["alarms"] == 0
    for cls in (V1Like, UnheatedFloor):
        s = cls(CFG).series(g)
        assert set(s) == {"wohnzimmer", "schlafzimmer", "bad", "house"} and all(len(v) == len(g.t) for v in s.values())
    assert np.isfinite(V1Like(CFG).score(g, g.days[1])["wohnzimmer"]).all()


def test_baseline_semantics():
    g = synth_grid()
    n = len(g.t)
    x = np.full(n, 21.0)
    x[1000:] = 19.5
    for r in g.rooms:
        g.room[r]["temp"] = x.copy()
        g.room[r]["since_open"] = np.concatenate([np.full(1000, 100.0), np.arange(n - 1000.0)])
    v = V1Like(CFG).series(g)
    assert v["bad"][1005] == 1.5 and v["bad"][1015] == 0.0
    f = UnheatedFloor(CFG).series(g)
    assert f["bad"][1005] == 1.5 and f["bad"][1030] == 0.0 and f["bad"][500] == 0.0
    assert f["house"][1005] == pytest.approx(3.0)


def phys_grid(n=4000, seed=0, gaps=()):
    rng = np.random.default_rng(seed)
    t = 1.7e9 + 60 * np.arange(n)
    sh = {"out": 5 + np.sin(np.arange(n) / 500.0), "wind": 2 + rng.uniform(0, 1, n), "rad": np.maximum(0, 100 * np.sin(np.arange(n) / 700.0)), "supply_event": np.zeros(n, bool)}
    room = {}
    for r in ("wohnzimmer", "bad", "schlafzimmer"):
        valve = np.clip(50 + 50 * np.sin(np.arange(n) / 90.0 + len(r)), 0, 100)
        temp = 20 + np.cumsum(0.02 * (valve / 100 - 0.4)) * 0 + 0.3 * np.sin(np.arange(n) / 150.0) + 0.05 * rng.standard_normal(n)
        d = {"temp": temp, "valve": valve, "setpoint": np.full(n, 21.0)}
        for a, b in gaps:
            d["valve"][a:b] = np.nan
        d["since_open"] = since_open(d["valve"], 10.0)
        room[r] = d
    cfg = copy.deepcopy(CFG)
    cfg["rooms"] = list(room)
    cfg["nodes"] = {"wohnzimmer": 5, "bad": 7, "schlafzimmer": 6}
    return Grid(t, list(room), room, sh, cfg), cfg


def test_missing_valve_is_unscorable_not_closed():
    g, cfg = phys_grid(gaps=[(2000, 2005)])
    X, y, ok = PhysicsResidual(cfg).features(g, "bad")
    g0 = cfg["phys"]["horizon"] + cfg["phys"]["valve_guard_min"]
    assert not ok[2000:2005 + g0].any() and ok[2005 + g0] and ok[1999]
    assert not ok[:g0].any() and ok[g0]


def test_causal_supply_veto():
    g, cfg = phys_grid()
    g.shared["supply_event"][1500:1600] = True
    g.shared["supply_onset"] = np.zeros(len(g.t), bool)
    g.shared["supply_onset"][1000:1600] = True
    ap = PhysicsResidual(cfg).fit(g, g.days)
    s = ap.series(g)
    assert np.isfinite(s["house"][1499]) and np.isnan(s["house"][1500]) and np.isnan(s["house"][1659]) and np.isfinite(s["house"][1660])
    assert dilate(np.array([0, 1, 0, 0, 0], bool), 2).tolist() == [False, True, True, True, False]


def test_stream_matches_vectorized_with_gaps_and_veto():
    g, cfg = phys_grid(gaps=[(2000, 2005), (3000, 3300)])
    g.shared["supply_event"][2600:2650] = True
    ap = PhysicsResidual(cfg).fit(g, g.days)
    b = {"horizon_min": 20, "taus": cfg["phys"]["taus"], "since_open_cap": 90.0, "open_valve_pct": 10.0, "valve_guard_min": 120, "clip": 6.0, "veto_pad_min": 60,
         "rooms": {r: {"weight": cfg["phys"]["weights"][r], "coef": c.tolist(), "intercept": i, "sigma": sg} for r, (c, i, sg) in ap.model.items()}}
    sub, rec = stream_run(b, g, slice(0, len(g.t)))
    s = ap.series(g)
    n_fin = 0
    for i, (sh, z, house, veto) in enumerate(rec):
        for r in g.rooms:
            want = s[r][i]
            got = np.nan if veto else z[r]
            assert np.isnan(want) == np.isnan(got)
            if np.isfinite(want):
                n_fin += 1
                assert want == pytest.approx(got, abs=1e-9)
        want, got = s["house"][i], np.nan if veto else house
        assert np.isnan(want) == np.isnan(got) and (np.isnan(want) or want == pytest.approx(got, abs=1e-9))
    assert n_fin > 3000 and any(r[3] for r in rec)


def test_export_requires_validation_and_writes_bundle(tmp_path, monkeypatch):
    g, cfg = phys_grid()
    monkeypatch.setattr(Grid, "load", classmethod(lambda cls, lake, c: g))
    config = {"window": cfg}
    with pytest.raises(SystemExit):
        window_bundle(config, "x", tmp_path)
    ok = {"pass": True, "detected": True, "latency_min": 25, "threshold": 7.5, "alarms": 1, "alarms_per_day": 0.1, "gt_rank": 1, "alarms_in_supply": 0, "episodes": []}
    monkeypatch.setattr(Evaluator, "run", lambda self, sc: ok)
    out, bundle, meta, ap, _ = window_bundle(config, "x", tmp_path, version="v1")
    b = json.loads((out / "bundle.json").read_text())
    assert out == tmp_path / "window" / "v1" and b["threshold"] == 7.5 and set(b["rooms"]) == {"wohnzimmer", "bad", "schlafzimmer"}
    assert all(len(m["coef"]) == len(b["feature_names"]) == 16 for m in b["rooms"].values())
    assert json.loads((out / "meta.json").read_text())["eval"]["pass"]
    csv = parity_csv(b, g, slice(0, 600)).splitlines()
    assert csv[0].startswith("ts,supply,out,wind,rad,wohnzimmer_temp") and len(csv) == 601 and csv[-1].split(",")[-1] == "0"
