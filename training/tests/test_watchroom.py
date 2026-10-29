import json
import numpy as np
import onnxruntime as ort
import pytest
from conftest import SYNTH, TRANSIENT, make_lake
from heattrain.eval import Matcher
from heattrain.watchroom import classify_state, eval_model, load_scans, train, transitions
from zoneinfo import ZoneInfo

CONTRACT = ["model_version", "created", "matcher", "rooms", "senders", "sentinel_dbm", "scale", "states", "tau_room", "delta_room", "t_hold_min",
            "smoothing", "metrics", "train_window", "n_labels"]


@pytest.fixture(scope="module")
def trained(lake, config, tmp_path_factory):
    out = train(config, lake, tmp_path_factory.mktemp("models"), version="v1")
    return out, json.loads((out / "model.json").read_text())


def test_contract_fields(trained):
    out, meta = trained
    assert all(k in meta for k in CONTRACT)
    assert meta["matcher"] == "room" and meta["model_version"] == "v1" and out.name == "v1"
    assert meta["rooms"] == sorted(SYNTH["rooms"]) and meta["scale"] == {"floor": -100, "span": 70} and meta["sentinel_dbm"] == -100
    assert "all" in meta["states"] and all((out / f).is_file() for f in meta["states"].values())
    assert TRANSIENT not in meta["senders"] and len(meta["senders"]) == SYNTH["n_aps"]
    assert set(meta["n_labels"]) == set(meta["rooms"]) and 0 < meta["tau_room"] <= 1 and meta["delta_room"] >= 0


def test_transition_rows_sum_to_one(trained):
    meta = trained[1]
    tr = meta["smoothing"]["transition"]
    for a in meta["rooms"]:
        assert sum(tr[a].values()) == pytest.approx(1.0, abs=1e-9)
        assert tr[a][a] == pytest.approx(meta["smoothing"]["stay_prob"])
    assert tr["wohnzimmer"]["kueche"] > tr["wohnzimmer"]["bad"]
    assert tr["schlafzimmer"]["bad"] > tr["schlafzimmer"]["kueche"]


def test_onnx_contract_and_parity(trained, lake, config):
    out, meta = trained
    assert all(v <= 1e-5 for v in meta["metrics"]["onnx_parity_max_abs"].values())
    for f in meta["states"].values():
        s = ort.InferenceSession(str(out / f))
        assert [i.name for i in s.get_inputs()] == ["x"] and [o.name for o in s.get_outputs()] == ["probabilities"]
        p = s.run(None, {"x": np.random.default_rng(0).random((5, len(meta["senders"]))).astype(np.float32)})[0]
        assert p.shape == (5, len(meta["rooms"])) and p.dtype == np.float32
        np.testing.assert_allclose(p.sum(axis=1), 1.0, atol=1e-5)


def test_holdout_quality(trained):
    h = trained[1]["metrics"]["holdout"]["smoothed"]
    assert h["coverage"] >= 0.8 and h["macro_f1"] >= 0.85, (h["coverage"], h["macro_f1"])
    assert h["baselines"]["no_matcher"]["coverage"] == 0.0 and len(h["confusion"]) == len(SYNTH["rooms"])


def test_eval_on_new_days(trained, config, tmp_path):
    out, meta = trained
    lake = make_lake(tmp_path / "lake", 13)
    scans, _ = load_scans(lake, config)
    assert any(s.state in ("sleep", "rest", "active") for s in scans) and any(s.state is None for s in scans)
    m = eval_model(config, lake, out)
    assert m["note"].startswith("Tage nach") and m["smoothed"]["n"] > 100
    assert m["smoothed"]["coverage"] >= 0.8 and m["smoothed"]["macro_f1"] >= 0.85
    assert json.loads((out / "model.json").read_text())["metrics"]["eval"]["smoothed"]["n"] == m["smoothed"]["n"]
    assert list(config["eval_dir"].glob("watchroom_v1.md"))


def test_matcher_abstains_without_data(trained):
    from heattrain.watchroom import Scan
    m = Matcher(trained[0])
    out, held = m.run([Scan(1e9, "pixel", {"zz:zz": -50}, None, "all"), Scan(1e9 + 60, "pixel", {}, None, "all")])
    assert out[0] == (None, "no_scan") and out[1] == (None, "no_scan") and held == [None, None]
    m.sessions.pop("all")
    out, _ = m.run([Scan(1e9, "pixel", {m.senders[0]: -50}, None, "weird")])
    assert out[0] == (None, "unknown_state")


def test_state_rule():
    cfg = {"hr_rest_max": 70, "hr_active_min": 90, "motion_active_min": 0.5, "sleep_hours": [23, 7], "window_s": 600}
    tz, t = ZoneInfo("Europe/Berlin"), 1790000000.0
    mk = lambda **kw: {("d", k): (np.array([t - 60]), np.array([v])) for k, v in kw.items()}
    assert classify_state(cfg, tz, t, "d", {}) is None
    assert classify_state(cfg, tz, t, "d", mk(hr=100.0, motion=0.1)) == "active"
    assert classify_state(cfg, tz, t, "d", mk(hr=60.0, motion=0.1)) in ("rest", "sleep")
    assert classify_state(cfg, tz, t, "d", mk(hr=60.0, charging=1.0)) == "charging"
    assert classify_state(cfg, tz, t, "d", mk(hr=60.0, offbody=0.0)) == "offbody"


def test_transitions_fallback_uniform():
    tr = transitions(["a", "b", "c"], [], {"stay_prob": 0.9, "weight_link": {}, "weight_other": 1.0})
    assert tr["a"]["b"] == pytest.approx(tr["a"]["c"]) and sum(tr["a"].values()) == pytest.approx(1.0)


def test_train_without_meta(tmp_path, config):
    lake = make_lake(tmp_path / "lake", 8, with_meta=False)
    meta = json.loads((train(config, lake, tmp_path / "m", version="v2") / "model.json").read_text())
    row = meta["smoothing"]["transition"]["bad"]
    assert row["wohnzimmer"] == pytest.approx(row["kueche"])
