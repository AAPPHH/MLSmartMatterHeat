import json
import os
from datetime import datetime, timedelta, timezone
from pathlib import Path
import numpy as np
import pyarrow as pa
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


def room_scans(rooms, n=30):
    from heattrain.watchroom import Scan
    rng = np.random.default_rng(2)
    return [Scan(1e9 + i * 60 + j * 5000, "d", {"aa": b + rng.normal(0, 2), "bb": -60.0 + rng.normal(0, 2)}, r, "rest")
            for j, (r, b) in enumerate(zip(rooms, (-40.0, -80.0, -65.0))) for i in range(n)]


def small_cfg(config):
    return {**config, "min_sender_scans": 1, "min_sender_days": 1, "min_room_scans": 5, "min_state_scans": 10 ** 6}


@pytest.mark.parametrize("rooms", [["a", "b"], ["a", "b", "c"]])
def test_xgb_onnx_parity_for_room_counts(config, tmp_path, rooms):
    from heattrain.watchroom import Trainer, export, full_proba
    cfg = small_cfg(config)
    bundle = Trainer(cfg).fit(room_scans(rooms), [])
    out = export(bundle, cfg, tmp_path, "p", {})
    meta = json.loads((out / "model.json").read_text())
    assert meta["classifier"] == "xgb" and meta["rooms"] == rooms
    got = ort.InferenceSession(str(out / meta["states"]["all"])).run(None, {"x": bundle["X"]})[0]
    assert np.abs(got - full_proba(bundle["models"]["all"], bundle["X"], len(rooms))).max() < 1e-5


def test_room_without_rows_after_filtering_is_dropped(config, tmp_path):
    from heattrain.watchroom import Trainer, export
    scans = room_scans(["a", "b", "c"])
    for s in scans:
        if s.room == "c":
            s.state = "offbody"
    cfg = small_cfg(config)
    bundle = Trainer(cfg).fit(scans, [])
    assert bundle["rooms"] == ["a", "b"]
    export(bundle, cfg, tmp_path, "d", {})


def test_single_room_raises_value_error(config):
    from heattrain.watchroom import Trainer
    with pytest.raises(ValueError, match="at least 2 rooms"):
        Trainer(small_cfg(config)).fit(room_scans(["a"]), [])


@pytest.mark.parametrize("rooms", [["a", "b"], ["a", "b", "c"]])
def test_train_produces_model_dir(config, tmp_path, monkeypatch, rooms):
    scans = room_scans(rooms)
    for i, s in enumerate(scans):
        s.ts += (i % 3) * 86400
    monkeypatch.setattr("heattrain.watchroom.load_scans", lambda *a: (scans, []))
    meta = json.loads((train(small_cfg(config), tmp_path, tmp_path / "m", version="t") / "model.json").read_text())
    assert meta["classifier"] == "xgb" and meta["feature_version"] == 2 and meta["rooms"] == rooms


def label_lake(root, rows):
    from conftest import LABELS, write_table
    ts = lambda t: datetime.fromtimestamp(t, timezone.utc) if t else None
    write_table(root, "labels", [{"ts": ts(t), "from_ts": ts(f), "to_ts": ts(e), "room": r, "source": "watch", "device": "pixel"} for t, f, e, r in rows], LABELS, "ts")
    return root


def test_short_interval_label_is_a_point_at_its_midpoint_and_long_ones_are_ignored(tmp_path, config):
    from heattrain.watchroom import Lake, Scan, assign_labels
    base = 1_790_000_000.0
    root = label_lake(tmp_path, [(base + 500, base - 1, base + 1, "bad"), (base + 9000, base + 4000, base + 4300, "kueche")])
    scans = [Scan(base + dt, "pixel", {}) for dt in (-20, 0, 25, 100, 4100, 4290)]
    assign_labels(Lake(root), {**config, "label_intervals": False, "label_tolerance_s": 30.0}, scans)
    assert [s.room for s in scans] == ["bad", "bad", "bad", None, None, None]
    assign_labels(Lake(root), {**config, "label_intervals": True, "label_tolerance_s": 30.0}, scans)
    assert [s.room for s in scans] == [None, "bad", None, None, "kueche", "kueche"]


def test_voided_and_post_tap_point_labels_are_ignored(tmp_path, config):
    import pyarrow as pa
    from conftest import LABELS, write_table
    from heattrain.watchroom import Lake, Scan, assign_labels
    schema = pa.schema(list(LABELS) + [("label_id", pa.string()), ("voided_label_id", pa.string())])
    tap = datetime.fromisoformat(config["tap_labels_from"]).timestamp()
    ts = lambda t: datetime.fromtimestamp(t, timezone.utc) if t else None
    row = lambda t, f, e, room, lid=None, void=None, source="watch": {"ts": ts(t), "from_ts": ts(f), "to_ts": ts(e), "room": room, "source": source, "device": "pixel", "label_id": lid, "voided_label_id": void}
    rows = [row(tap - 5000, None, None, "old_point"), row(tap + 1000, None, None, "stay_point"), row(tap + 2000, tap + 1999, tap + 2001, "kept", "k"),
            row(tap + 3000, tap + 2999, tap + 3001, "wrong", "w"), row(tap + 3500, None, None, "", None, "w", "void")]
    write_table(tmp_path, "labels", rows, schema, "ts")
    scans = [Scan(t, "pixel", {}) for t in (tap - 5000, tap + 1000, tap + 2000, tap + 3000)]
    assign_labels(Lake(tmp_path), {**config, "label_intervals": False}, scans)
    assert [s.room for s in scans] == ["old_point", None, "kept", None]
    assign_labels(Lake(tmp_path), {**config, "label_intervals": False, "tap_labels_from": None}, scans)
    assert [s.room for s in scans] == ["old_point", "stay_point", "kept", None]


def test_confident_learning_drops_mislabelled_scans_only_in_rooms_with_enough_labels(config):
    from heattrain.watchroom import Scan, Trainer
    rng = np.random.default_rng(1)
    scans = []
    for room, base in (("a", -40.0), ("b", -80.0), ("c", -60.0)):
        for i in range(40):
            scans.append(Scan(1e9 + i * 60 + 86400 * (i % 4), "d", {"aa": base + rng.normal(0, 1), "bb": -60.0 + rng.normal(0, 1), "cc": -50.0 - (base + 60) / 2 + rng.normal(0, 1)}, room, "rest"))
    flipped = [s for s in scans if s.room == "a"][:3]
    for s in flipped:
        s.room = "b"
    cfg = {**small_cfg(config), "cv_folds": 4}
    fit = Trainer(cfg).fit(scans, [])
    dropped = fit["metrics"]["noise_dropped"]
    assert 1 <= dropped <= 3
    assert fit["n_labels"] == {"a": 37, "b": 43 - dropped, "c": 40}
    off = Trainer({**cfg, "noise_p_min": None}).fit(scans, [])
    assert off["metrics"]["noise_dropped"] == 0 and off["n_labels"]["b"] == 43
    small = Trainer({**cfg, "noise_min_labels": 50}).fit(scans, [])
    assert small["metrics"]["noise_dropped"] == 0


def write_stream(path, batches, chop=0):
    schema = pa.schema([("ts", pa.timestamp("us", tz="UTC")), ("room", pa.dictionary(pa.int32(), pa.string())), ("source", pa.string())])
    with pa.OSFile(str(path), "wb") as sink, pa.ipc.new_stream(sink, schema) as writer:
        for b in batches:
            writer.write_batch(pa.record_batch([pa.array([datetime.fromtimestamp(t, timezone.utc) for t in b], pa.timestamp("us", tz="UTC")), pa.array(["bad"] * len(b)).dictionary_encode(), pa.array(["watch"] * len(b))], schema=schema))
    if chop:
        data = Path(path).read_bytes()
        Path(path).write_bytes(data[:-chop])


def test_lake_reads_unrolled_raw_streams_ignores_truncated_tail_and_rolled_days(tmp_path):
    from heattrain.watchroom import Lake
    base = 1_790_000_000.0
    raw = tmp_path / "raw" / "labels"
    raw.mkdir(parents=True)
    write_stream(raw / "2026-10-29.arrows", [[base, base + 1]])
    write_stream(raw / "2026-10-29.1.arrows", [[base + 2], [base + 3, base + 4]], chop=40)
    write_stream(raw / "2026-10-28.arrows", [[base - 100000]])
    (tmp_path / "parquet" / "labels" / "date=2026-10-28").mkdir(parents=True)
    (tmp_path / "parquet" / "labels" / "date=2026-10-28" / ".rolled").write_text("{}")
    lake = Lake(tmp_path)
    got = [r[0] for r in lake.query("labels", [("epoch(ts)", None)], order="ORDER BY ts")]
    assert got == [base, base + 1, base + 2]
    assert {"ts", "room", "source"} <= lake.columns("labels")
    assert lake.query("labels", [("room", None)], "TRUE")[0] == ("bad",)


def test_lake_merges_parquet_with_unrolled_raw(tmp_path):
    from heattrain.watchroom import Lake
    base = 1_790_000_000.0
    label_lake(tmp_path, [(base - 200000, None, None, "kueche")])
    raw = tmp_path / "raw" / "labels"
    raw.mkdir(parents=True)
    write_stream(raw / "2026-10-29.arrows", [[base]])
    assert Lake(tmp_path).query("labels", [("room", None)], order="ORDER BY ts") == [("kueche",), ("bad",)]


def test_anchor_devices_are_excluded_from_scans_and_drift_is_reported(tmp_path, config):
    from conftest import MODERN, write_table
    from heattrain.eval import anchor_drift
    from heattrain.watchroom import anchor_rssi
    base = datetime(2026, 10, 20, 12, tzinfo=timezone.utc)
    rows = []
    for day, shift in ((0, 0), (1, 0), (5, 0), (6, -8)):
        for k in range(5):
            for s, level in (("aa:00", -50), ("bb:00", -60), ("cc:00", -70)):
                if day == 6 and s == "cc:00":
                    continue
                v = level + (shift if s == "aa:00" else 0)
                rows.append({"ts": base + timedelta(days=day, minutes=k), "sender": s, "rssi": v, "batch_id": day * 10 + k, "device": "minix-anchor", "sensor_type": "rssi", "value": float(v), "freq": 2437})
    write_table(tmp_path, "watch_raw", rows, MODERN, "ts")
    cfg = {**config, "anchor_devices": ["minix-anchor"]}
    scans, _ = load_scans(tmp_path, cfg)
    assert scans == []
    drift = anchor_drift(anchor_rssi(tmp_path, cfg), ("2026-10-20", "2026-10-21"), 6.0)
    assert drift["baseline"] == {"aa:00": -50.0, "bb:00": -60.0, "cc:00": -70.0} and sorted(drift["days"]) == ["2026-10-25", "2026-10-26"]
    assert [(f["day"], f["sender"], f["kind"]) for f in drift["flags"]] == [("2026-10-26", "aa:00", "shift"), ("2026-10-26", "cc:00", "missing")]
    assert drift["flags"][0]["delta"] == -8.0


def test_toothbrush_labels_keep_their_three_minute_interval_and_match_any_device(tmp_path, config):
    import pyarrow as pa
    from conftest import LABELS, write_table
    from heattrain.watchroom import Lake, Scan, assign_labels
    base = 1_790_000_000.0
    ts = lambda t: datetime.fromtimestamp(t, timezone.utc)
    schema = pa.schema(list(LABELS) + [("label_id", pa.string()), ("voided_label_id", pa.string())])
    row = lambda t, f, e, room, source, device, lid: {"ts": ts(t), "from_ts": ts(f), "to_ts": ts(e), "room": room, "source": source, "device": device, "label_id": lid, "voided_label_id": None}
    write_table(tmp_path, "labels", [row(base, base - 180, base + 180, "bad", "toothbrush", None, "t1"), row(base + 9000, base + 8000, base + 9300, "kueche", "watch", "pixel", "w1")], schema, "ts")
    scans = [Scan(base + dt, d, {}) for dt, d in ((-170, "pixel"), (0, "other"), (170, "pixel"), (200, "pixel"), (8500, "pixel"))]
    cfg = {**config, "label_intervals": False, "label_tolerance_s": 30.0, "labels_from": None, "tap_labels_from": "2020-01-01T00:00:00+00:00"}
    assign_labels(Lake(tmp_path), {**cfg, "interval_sources": []}, scans)
    assert [s.room for s in scans] == [None] * 5
    assign_labels(Lake(tmp_path), {**cfg, "sources": ["watch", "toothbrush"], "interval_sources": ["toothbrush"]}, scans)
    assert [s.room for s in scans] == ["bad", "bad", "bad", None, None]


def test_the_shipped_config_trains_on_toothbrush_labels_as_intervals():
    from heattrain.__main__ import CONFIG
    assert "toothbrush" in CONFIG["sources"] and CONFIG["interval_sources"] == ["toothbrush"]
