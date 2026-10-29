import json
import os
from pathlib import Path
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
        p = s.run(None, {"x": np.random.default_rng(0).random((5, s.get_inputs()[0].shape[1])).astype(np.float32)})[0]
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
    assert any(s.state in ("sleep", "rest", "active") for s in scans) and all(s.state is not None for s in scans)
    m = eval_model(config, lake, out)
    assert m["note"].startswith("Tage nach") and m["smoothed"]["n"] > 100
    assert m["smoothed"]["coverage"] >= 0.8 and m["smoothed"]["macro_f1"] >= 0.85
    assert json.loads((out / "model.json").read_text())["metrics"]["eval"]["smoothed"]["n"] == m["smoothed"]["n"]
    assert "v1" in m and "Features v1 vs v2" in next(config["eval_dir"].glob("watchroom_v1.md")).read_text()


def test_matcher_abstains_without_data(trained):
    from heattrain.watchroom import Scan
    m = Matcher(trained[0])
    out, held = m.run([Scan(1e9, "pixel", {"zz:zz": -50}, None, "all"), Scan(1e9 + 60, "pixel", {}, None, "all")])
    assert out[0] == (None, "no_known_ap") and out[1] == (None, "no_known_ap") and held == [None, None]
    off, _ = m.run([Scan(1e9, "pixel", {m.senders[0]: -50}, None, "offbody"), Scan(1e9, "pixel", {m.senders[0]: -50}, None, "charging")])
    assert off == [(None, "unknown_state")] * 2
    m.sessions.pop("all")
    out, _ = m.run([Scan(1e9, "pixel", {m.senders[0]: -50}, None, "weird")])
    assert out[0] == (None, "unknown_state")


def test_state_rule():
    cfg = {"hr_rest_max": 70, "hr_active_min": 90, "motion_active_min": 0.5, "sleep_hours": [23, 7], "window_s": 600, "stale_s": 600}
    tz, t = ZoneInfo("Europe/Berlin"), 1790000000.0
    mk = lambda **kw: {("d", k): (np.array([t - 60]), np.array([v])) for k, v in kw.items()}
    assert classify_state(cfg, tz, t, "d", {}) == "rest"
    quiet = {("d", "hr"): (np.array([t - 60]), np.array([55.0]))}
    assert classify_state(cfg, tz, 1790000000.0 - 3 * 3600, "d", {}) == "rest"
    assert classify_state(cfg, tz, t, "d", {**mk(offbody=0.0), **quiet}) == "offbody"
    assert classify_state(cfg, tz, t + 900, "d", {**mk(offbody=0.0), **quiet}) == "rest"
    assert classify_state(cfg, tz, t, "d", mk(offbody=1.0)) == "rest"
    assert classify_state(cfg, tz, t, "d", mk(hr=100.0, motion=0.1)) == "active"
    assert classify_state(cfg, tz, t, "d", mk(hr=60.0, motion=0.1)) in ("rest", "sleep")
    assert classify_state(cfg, tz, t, "d", mk(hr=60.0, charging=1.0)) == "charging"
    assert classify_state(cfg, tz, t, "d", mk(hr=60.0, offbody=0.0)) == "offbody"


def test_smoothing_matches_heatd_forward_step():
    from heattrain.eval import forward
    trans = np.array([[0.9, 0.1], [0.1, 0.9]])
    em = np.array([0.8, 0.2])
    first = forward(None, 0.0, trans, em)
    assert first == pytest.approx(em)
    later = forward(first, 1.0, trans, np.array([0.5, 0.5]))
    assert later == pytest.approx(first @ trans)
    assert forward(first, 61.0, trans, em) == pytest.approx(em)
    assert forward(first, 60.0, trans, em)[0] > 0.8
    assert forward(np.array([1.0, 0.0]), 1.0, trans, np.array([0.0, 1.0])).min() >= 1e-4 / 2


def test_offbody_and_charging_scans_never_train_templates(config):
    from heattrain.watchroom import Scan, Trainer
    rng = np.random.default_rng(0)
    scans = []
    for room, base in (("a", -40.0), ("b", -80.0)):
        for i in range(60):
            for state in ("rest", "offbody", "charging"):
                scans.append(Scan(1e9 + i * 60 + (86400 * (i % 4)), "d", {"aa": base + rng.normal(0, 2), "bb": -60.0 + rng.normal(0, 2)}, room, state))
    cfg = {**config, "min_sender_scans": 1, "min_sender_days": 1, "min_state_scans": 10, "min_state_room_scans": 3}
    fit = Trainer(cfg).fit(scans, [])
    assert set(fit["models"]) <= {"all", "rest"} and sum(fit["n_labels"].values()) == 120


def test_transitions_fallback_uniform():
    tr = transitions(["a", "b", "c"], [], {"stay_prob": 0.9, "weight_link": {}, "weight_other": 1.0})
    assert tr["a"]["b"] == pytest.approx(tr["a"]["c"]) and sum(tr["a"].values()) == pytest.approx(1.0)


def test_train_without_meta(tmp_path, config):
    lake = make_lake(tmp_path / "lake", 8, with_meta=False)
    meta = json.loads((train(config, lake, tmp_path / "m", version="v2") / "model.json").read_text())
    row = meta["smoothing"]["transition"]["bad"]
    assert row["wohnzimmer"] == pytest.approx(row["kueche"])


def test_keiner_is_regular_class_with_default_adjacency(config, tmp_path):
    from heattrain.eval import evaluate
    from heattrain.watchroom import Scan, Trainer, export
    rng = np.random.default_rng(1)
    bases = {"flur": -45.0, "wohnzimmer": -60.0, "bad": -75.0, "keiner": -90.0}
    scans = [Scan(1e9 + i * 60 + 86400 * (i % 4), "d", {"aa": b + rng.normal(0, 2), "bb": -60.0 + k * 7 + rng.normal(0, 2)}, room, "rest")
             for k, (room, b) in enumerate(bases.items()) for i in range(60)]
    cfg = {**config, "min_sender_scans": 1, "min_sender_days": 1, "min_state_scans": 10, "min_state_room_scans": 3}
    fit = Trainer(cfg).fit(scans, [])
    assert fit["rooms"] == ["bad", "flur", "keiner", "wohnzimmer"] and fit["n_labels"]["keiner"] == 60
    tr = fit["transition"]["keiner"]
    assert tr["flur"] == pytest.approx(tr["wohnzimmer"]) and tr["flur"] > 2 * tr["bad"]
    m = Matcher(export(fit, cfg, tmp_path, "k", {}))
    res = evaluate(m, scans)
    assert "keiner" in m.meta["rooms"] and res["per_room"]["keiner"]["support"] == 60 and res["per_room"]["keiner"]["recall"] > 0.5


def test_default_links_are_config_only():
    cfg = {"stay_prob": 0.9, "weight_link": {"open": 8.0}, "weight_other": 1.0, "default_links": [["keiner", "flur", "open"]]}
    tr = transitions(["flur", "keiner", "bad"], [], cfg)
    assert tr["keiner"]["flur"] > tr["keiner"]["bad"]
    plain = transitions(["flur", "keiner", "bad"], [], {**cfg, "default_links": []})["keiner"]
    assert plain["flur"] == pytest.approx(plain["bad"])


FIXTURE = Path(__file__).resolve().parents[2] / "crates" / "heatd" / "testdata" / "watch_features_v2.json"


def feature_fixture(cfg):
    from heattrain.eval import features
    from heattrain.watchroom import Scan, attach_history
    spec = {"feature_version": 2, "senders": ["aa:00", "bb:00", "cc:00", "dd:00", "ee:00", "ff:00"], "sender_bands": ["2.4", "2.4", "5", "5", "6", None],
            "scale": {"floor": -100, "span": 70}, "sentinel_dbm": -100, "history_s": 300.0, "history_scans": 5}
    steps = [(0, {"aa:00": -50, "cc:00": -70}), (60, {"aa:00": -60, "bb:00": -80, "cc:00": -65, "dd:00": -90, "zz:00": -40}), (120, {"bb:00": -55, "ee:00": -75, "ff:00": -45}),
             (180, {"aa:00": -45, "bb:00": -62, "cc:00": -72, "dd:00": -58, "ee:00": -85}), (240, {"cc:00": -30, "dd:00": -100}), (300, {"aa:00": -20, "ee:00": -60}),
             (360, {"bb:00": -130, "cc:00": -66}), (400, {"zz:00": -50}), (1000, {"aa:00": -70, "dd:00": -70, "ee:00": -70}), (1100, {"aa:00": -72, "bb:00": -52})]
    scans = [Scan(float(t), "w", dict(a)) for t, a in steps]
    attach_history(scans, {"history_s": spec["history_s"], "history_scans": spec["history_scans"]})
    return {"spec": spec, "scans": [{"ts": s.ts, "aps": s.aps} for s in scans], "expected": [[float(v) for v in features(spec, s.hist)] for s in scans]}


def test_feature_v2_fixture_matches_rust_input(config):
    fx = feature_fixture(config)
    if os.environ.get("REGEN_FIXTURE"):
        FIXTURE.write_text(json.dumps(fx, indent=1))
    assert json.loads(FIXTURE.read_text()) == fx
    n = len(fx["spec"]["senders"])
    assert all(len(v) == 3 * n + 9 for v in fx["expected"])
    last = fx["expected"][-1]
    assert last[n + 0] == pytest.approx((np.mean([(-70 + 100) / 70, (-72 + 100) / 70])), abs=1e-6) and last[2 * n + 0] == 1.0 and last[2 * n + 1] == 0.5


def test_history_uses_only_past_and_current(config):
    from heattrain.watchroom import Scan, attach_history
    scans = [Scan(float(t), "d", {"a": -50 - t}) for t in (0, 100, 200, 700, 710)]
    attach_history(scans, {"history_s": 300.0, "history_scans": 3})
    assert [[h["a"] for h in s.hist] for s in scans] == [[-50], [-50, -150], [-50, -150, -250], [-750], [-750, -760]]


def test_v2_model_json_and_v1_compat(trained, lake, config, tmp_path):
    out, meta = trained
    n = len(meta["senders"])
    assert meta["feature_version"] == 2 and len(meta["sender_bands"]) == n and meta["history_s"] == 300.0 and meta["history_scans"] == 5
    assert set(meta["sender_bands"]) == {"2.4", "5", "6"}
    assert ort.InferenceSession(str(out / meta["states"]["all"])).get_inputs()[0].shape[1] == 3 * n + 9
    assert "v1" in meta["metrics"]["holdout"]
    v1 = json.loads((train({**config, "feature_version": 1}, lake, tmp_path / "m1", version="v1x") / "model.json").read_text())
    assert "feature_version" not in v1 and "sender_bands" not in v1


def test_xgb_export_parity(lake, config, tmp_path):
    from heattrain.watchroom import Trainer, export
    cfg = dict(config, candidates=["xgb"])
    scans, links = load_scans(lake, cfg)
    bundle = Trainer(cfg).fit(scans, links)
    out = export(bundle, cfg, tmp_path, "x1", {})
    meta = json.loads((out / "model.json").read_text())
    assert meta["classifier"] == "xgb"
    s = ort.InferenceSession(str(out / meta["states"]["all"]))
    assert [o.name for o in s.get_outputs()] == ["probabilities"]
    p = s.run(None, {"x": bundle["X"][:20]})[0]
    assert p.shape == (20, len(meta["rooms"])) and np.allclose(p.sum(axis=1), 1.0, atol=1e-5)


def test_xgb_trains_on_single_day(config, tmp_path):
    from heattrain.watchroom import Scan, Trainer, export
    rng = np.random.default_rng(1)
    scans = [Scan(1e9 + i * 60 + j * 3000, "d", {"aa": b + rng.normal(0, 2), "bb": -60.0 + rng.normal(0, 2)}, r, "rest")
             for j, (r, b) in enumerate((("a", -40.0), ("b", -80.0), ("c", -65.0))) for i in range(8)]
    cfg = {**config, "min_sender_scans": 1, "min_sender_days": 1, "min_room_scans": 5, "min_state_scans": 10 ** 6}
    bundle = Trainer(cfg).fit(scans, [])
    assert bundle["kind"] == "xgb" and bundle["metrics"]["cv_macro_f1"]["xgb"] > 0
    export(bundle, cfg, tmp_path, "s1", {})
