import copy
import json
from pathlib import Path
from dataclasses import replace
from types import SimpleNamespace
import numpy as np
import onnxruntime as ort
import pytest
from heattrain.__main__ import CONFIG
from heattrain.thermalml import build_bundle, choose_t_off, decay_labels, env_from, feature_matrix, feature_names, load_ambient, optimum_start, origin_state, since_boost
from test_thermal import simulate

CONF = copy.deepcopy(CONFIG)
CONF["thermalml"].update(xgb_latent={"n_estimators": 40, "max_depth": 3, "learning_rate": 0.1}, xgb_residual={"n_estimators": 30, "max_depth": 3, "learning_rate": 0.1}, min_residual_rows=300, min_latent_labels=100)


def make_rooms(hours=144, names=("a", "b"), short=None):
    out = []
    for i, n in enumerate(names):
        d, _, _ = simulate(hours=hours, seed=i, report_p=0.3)
        out.append(replace(d, room=n, node=i + 1, sp=np.full(d.n, 21.0)))
    if short:
        d, _, _ = simulate(hours=short, seed=9)
        out.append(replace(d, room="tiny", node=9, sp=np.full(d.n, 21.0)))
    return out


@pytest.fixture(scope="module")
def built(tmp_path_factory):
    return build_bundle(CONF, make_rooms(short=6), {}, tmp_path_factory.mktemp("m"), version="20261030T000000Z")


def test_bundle_contract(built):
    out, b, ctx = built
    assert b["dt_s"] == 60 and b["history_min"] == 60 and b["lag_step_min"] == 5
    assert set(b["rooms"]) == {"a", "b", "tiny"} and b["room_index"] == ["a", "b", "tiny"]
    assert b["rooms"]["a"]["valid"] and not b["rooms"]["tiny"]["valid"]
    assert b["rooms"]["a"]["physics"]["params"] and "valve_gain" in b["rooms"]["a"]["physics"]
    assert b["latent"]["file"] == "latent.onnx" and b["residual"]["features"][0] == "sensor_lag_0"
    assert "room_a" in b["latent"]["features"] and "phys_air" not in b["latent"]["features"] and "phys_air" in b["residual"]["features"]
    bt = b["metrics"]["backtest"]["a"]
    assert {"grey_box", "residual", "cycles", "days"} <= set(bt) and bt["cycles"] > 0
    assert max(b["metrics"]["onnx_parity_max_abs"].values()) < 1e-4
    assert json.loads((out / "bundle.json").read_text())["version"] == "20261030T000000Z"


def test_onnx_matches_python_with_nan(built):
    out, b, ctx = built
    R = next(r for r in ctx["rooms"] if r.valid)
    X = R.F[:50].copy()
    X[:, 5:9] = np.nan
    for key, net in (("latent", ctx["nets"]["latent"]), ("residual", ctx["nets"]["residual"])):
        sess = ort.InferenceSession(str(out / f"{key}.onnx"))
        assert sess.get_outputs()[0].name == ("room_temp" if key == "latent" else "delta")
        got = sess.run(None, {"x": X[:, net.cols]})[0]
        assert got.shape == (50, 1) and np.abs(got[:, 0] - net(X)).max() < 1e-4


def test_residual_metrics_and_latent_agreement(built):
    m = built[1]["metrics"]
    assert m["split"] in ("days", "blocks")
    hz = m["residual"]["horizons"]
    assert set(hz) == {"30", "60", "120"} and all(np.isfinite(v["physics_rmse"]) and np.isfinite(v["residual_rmse"]) for v in hz.values())
    assert np.isfinite(m["latent"]["kalman_agreement_rmse_test"]) and np.isfinite(m["latent"]["label_rmse_test"])
    assert m["residual"]["feedback"] in CONF["thermalml"]["feedback_grid"]
    best = np.mean([v["residual_rmse"] for v in hz.values()])
    assert best <= 1.2 * np.mean([v["physics_rmse"] for v in hz.values()])
    assert m["offsets"]["a"]["cold_minutes"] > 0


def test_optimum_start_peak_matches_target(built):
    out, b, ctx = built
    R = next(r for r in ctx["rooms"] if r.valid)
    cfg = ctx["cfg"]
    ks = np.array([R.d.n // 2])
    x0, S0, V0 = origin_state(cfg, R, ks)
    env = env_from(R, ks, cfg["horizon_min"])
    S0[:], x0[:], V0[:] = 17.0, 17.0, 0.0
    target = 21.0
    for kw in ({}, {"latent": ctx["nets"]["latent"], "residual": ctx["nets"]["residual"]}):
        r = optimum_start(cfg, CONF["thermal"], R.doc, R.name, ctx["names"], x0, S0, V0, env, target, **kw)
        assert r["predicted_peak_c"] <= target + cfg["overshoot_tol"]
        ok = r["peaks"] <= target + cfg["overshoot_tol"]
        assert r["t_off_min"] > 0 and np.abs(r["peaks"][ok] - target).min() == abs(r["predicted_peak_c"] - target)
        assert not ok[-1]


def test_choose_t_off_rules():
    cand = np.arange(5)
    assert choose_t_off(np.array([19.0, 19.9, 20.5, 21.2, 22.0]), cand, 21.0, 0.3) == 3
    assert choose_t_off(np.array([22.0, 22.5, 23.0, 23.5, 24.0]), cand, 21.0, 0.3) == 0


def test_since_boost_and_features():
    cfg = CONF["thermalml"]
    V = np.zeros((2, 241))
    V[0, :10] = 1.0
    V[1, 3:50] = 0.6
    assert since_boost(V, cfg).tolist() == [9.0, 49.0]
    assert since_boost(np.zeros((1, 241)), cfg)[0] == 240.0
    names = feature_names(cfg, ["a", "b"], "residual")
    ctx = {"S": np.arange(61.0)[None, :], "V": V[:1], "hour": np.array([6.0]), "outdoor_c": 5.0, "radiation_wm2": 0.0, "others_heating": 1.0, "phys_sensor_pred": 20.0, "phys_air": 19.9}
    d = dict(zip(names, feature_matrix(cfg, names, ctx, "b")[0]))
    assert d["sensor_lag_15"] == 15 and d["valve_lag_0"] == 1 and d["room_b"] == 1 and d["room_a"] == 0 and np.isnan(d["ambient_nvme_c"]) and abs(d["hour_sin"] - 1) < 1e-6


def test_little_data_never_crashes(tmp_path):
    out, b, ctx = build_bundle(CONF, make_rooms(hours=8, names=("x",)), {}, tmp_path, version="v")
    assert b["latent"] is None and b["residual"] is None and not b["rooms"]["x"]["valid"]
    assert "residual_unavailable" in b["metrics"]


def test_ambient_loader_empty(tmp_path):
    assert load_ambient(tmp_path) == {}


def test_parity_fixture_in_repo():
    root = Path(__file__).resolve().parents[2] / "crates" / "heatd" / "testdata"
    fx = json.loads((root / "thermal_ml_parity.json").read_text())
    bundle = root / fx["bundle_dir"]
    b = json.loads((bundle / "bundle.json").read_text())
    assert bundle.parent.name == "thermal_bundle" and bundle.name == b["version"]
    assert {c["structure"] for c in fx["physics"]} == {"rc1", "rc2"} and {c["night"] for c in fx["physics"]} == {True, False}
    assert fx["features"] and all(len(f["residual_vector"]) == len(b["residual"]["features"]) for f in fx["features"])
    for r in fx["rollouts"]:
        peaks = np.array(r["peaks"])
        assert r["t_off_min"] == choose_t_off(peaks, np.arange(len(peaks)), r["target"], r["overshoot_tol"]) and len(r["air"]) == r["horizon_min"]
        assert len(peaks) == r["toff_max"] + 1 and r["feedback"] == b["residual"]["feedback"]
    assert (bundle / b["latent"]["file"]).is_file()


def test_grey_box_backtest_is_held_out(built):
    b, ctx = built[1], built[2]
    R = next(r for r in ctx["rooms"] if r.name == "a")
    assert b["metrics"]["backtest"]["a"]["in_sample"] is False
    assert R.doc_tr["params"] != R.doc["params"] and R.doc_tr["structure"] == R.doc["structure"]
    assert R.test.any() and not R.test.all() and np.isfinite(R.offset_tr)


def test_decay_labels_stop_at_split_edges():
    cfg = CONF["thermalml"]
    n = 200
    u = np.zeros(n)
    u[:30] = 1.0
    t = np.arange(n, dtype=float)
    y = 20.0 + 3.0 * np.exp(-np.maximum(t - 30, 0) / 20.0)
    test = np.zeros(n, bool)
    test[70:] = True
    R = SimpleNamespace(d=SimpleNamespace(n=n, u=u, y=y), test=test)
    lab, count = decay_labels(R, cfg)
    assert count == 1 and np.isfinite(lab[30:70]).all() and np.isnan(lab[70:]).all()


def write_real_shape_lake(root, hours=21):
    import pyarrow as pa
    import pyarrow.ipc as ipc
    import pyarrow.parquet as pq
    from datetime import datetime, timezone
    rooms = {"wohnzimmer": 5, "schlafzimmer": 6}
    t0 = datetime(2026, 10, 29, 12, 41, tzinfo=timezone.utc).timestamp()
    rows = []
    for i, (name, node) in enumerate(rooms.items()):
        d, _, _ = simulate(hours=hours, seed=i, report_p=1.0, invalid=0)
        for k in range(0, d.n, 5):
            ts = datetime.fromtimestamp(t0 + k * 60, timezone.utc)
            src = "poll" if k % 10 else "subscription"
            rows.append((ts, node, 0, None, int(round(d.y[k] * 100)), src))
            rows.append((ts, node, 8, None, 134 if k % 100 == 0 else int(round(d.u[k] * 100)), src))
            rows.append((ts, node, 18, None, 1800, src))
            rows.append((ts, node, 28, None, 4, src))
        rows.append((datetime.fromtimestamp(t0 - 3600, timezone.utc), node, 0, 17.5, None, "legacy"))
    schema = pa.schema([("ts", pa.timestamp("us", tz="UTC")), ("mono_ns", pa.int64()), ("node", pa.uint64()), ("endpoint", pa.uint16()), ("cluster", pa.uint32()), ("attribute", pa.uint32()), ("v_i64", pa.int64()), ("v_f64", pa.float64()), ("source", pa.string())])
    cols = lambda rs: pa.table({"ts": [r[0] for r in rs], "mono_ns": [0] * len(rs), "node": [r[1] for r in rs], "endpoint": [1] * len(rs), "cluster": [513] * len(rs), "attribute": [r[2] for r in rs], "v_i64": [r[4] for r in rs], "v_f64": [r[3] for r in rs], "source": [r[5] for r in rs]}, schema=schema)
    rows.sort(key=lambda r: r[0])
    half = len(rows) // 2
    raw = root / "raw" / "attribute_reports"
    raw.mkdir(parents=True)
    for name, part in (("2026-10-29.arrows", rows[:half + 7]), ("2026-10-30.arrows", rows[half:])):
        with ipc.new_stream(raw / name, schema) as w:
            w.write_table(cols(part))
    pq_dir = root / "parquet" / "attribute_reports" / "date=2026-10-29"
    pq_dir.mkdir(parents=True)
    pq.write_table(cols(rows[:half]), pq_dir / "part.parquet")
    wt = pa.table({"ts": [datetime.fromtimestamp(t0 + h * 3600, timezone.utc) for h in range(hours + 2)], "issued_ts": [None] * (hours + 2), "kind": ["obs"] * (hours + 2), "temp_c": [9.0 + (h % 12) * 0.3 for h in range(hours + 2)], "wind_ms": [3.0] * (hours + 2), "radiation_wm2": [0.0] * (hours + 2)})
    (root / "raw" / "weather").mkdir()
    with ipc.new_stream(root / "raw" / "weather" / "2026-10-29.arrows", wt.schema) as w:
        w.write_table(wt)
    return root, len(rows)


def test_real_lake_shape_raw_and_parquet_merge(tmp_path):
    from heattrain.thermal import load_lake
    from heattrain.thermalml import thermalml
    root, n = write_real_shape_lake(tmp_path / "lake")
    reports, weather, meta = load_lake(root, ("legacy",))
    assert len(reports) == n - 2 and len(weather) == 23
    assert max(r[0] for r in reports) - min(r[0] for r in reports) > 20 * 3600
    out, b = thermalml(CONF, root, tmp_path / "models")
    m = b["metrics"]
    assert m["n_latent_labels"] > 0 and m["n_residual_rows"] > 0 and set(m["offsets"]) == {"node5", "node6"}
    for r in b["rooms"].values():
        assert r["trainable"] and not r["valid"] and any("required for a valid room" in x for x in r["reasons"])
