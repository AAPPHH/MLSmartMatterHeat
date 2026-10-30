import copy
import json
from dataclasses import replace
import numpy as np
import onnxruntime as ort
import pytest
from heattrain.__main__ import CONFIG
from heattrain.thermalml import build_bundle, choose_t_off, env_from, feature_matrix, feature_names, load_ambient, optimum_start, origin_state, since_boost
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
