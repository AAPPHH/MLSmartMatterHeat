import json
import numpy as np
import onnxruntime as ort
import pytest
from heatml.__main__ import CONFIG
from heatml.data import REPO_ROOT, Snapshot
from heatml.room import RoomClassifier, vectorize
from heatml.synth import START_TS, SynthGenerator
from heatml.window import SlopeFeatures, ThresholdWindowDetector

FIXTURE = REPO_ROOT / "crates/heatd/tests/fixtures/room_model.onnx"


def load(name):
    return json.loads((REPO_ROOT / "contract" / name).read_text())


@pytest.fixture(scope="module")
def trained(tmp_path_factory):
    d = tmp_path_factory.mktemp("w")
    gen = SynthGenerator(CONFIG["synth"])
    db = gen.write(d / "snap.duckdb", seed=1)
    clf = RoomClassifier(CONFIG["room"]).fit(Snapshot(db).scans())
    onnx_path, json_path = clf.export(d / "m" / "room_model.onnx")
    return gen, clf, onnx_path, json_path


def test_vectorize_contract_example():
    scan, feat = load("scan_example.json"), load("features_example.json")
    aps = {a["bssid"]: a["rssi"] for a in scan["aps"]}
    v = vectorize(aps, feat["bssids"], feat["rssi_floor"], feat["rssi_span"])
    assert v.dtype == np.float32
    np.testing.assert_allclose(v, feat["expected_input"], atol=1e-6)


def test_snapshot_roundtrip(tmp_path):
    snap = Snapshot(SynthGenerator(CONFIG["synth"]).write(tmp_path / "s.duckdb", seed=3))
    scans = snap.scans()
    assert len(scans) == 150 * 4 and all(isinstance(s.aps, dict) for s in scans)
    assert len(snap.readings()["ts"]) == CONFIG["synth"]["temperature"]["n"]


def test_end_to_end_accuracy(trained):
    gen, clf, onnx_path, json_path = trained
    side = json.loads(json_path.read_text())
    assert side["version"] == 1 and side["input_name"] == "input"
    held = gen.scans(seed=2)
    X = np.stack([vectorize(s["aps"], side["bssids"], side["rssi_floor"], side["rssi_span"]) for s in held])
    label, prob = ort.InferenceSession(str(onnx_path)).run(["label", "probabilities"], {"input": X})
    truth = np.array([side["labels"].index(s["label"]) for s in held])
    assert (label == truth).mean() > 0.9
    assert prob.shape == (len(held), len(side["labels"]))


def test_transient_bssids_excluded(trained):
    clf = trained[1]
    assert clf.bssids
    assert not any(b.startswith("de:ad:be:ef") for b in clf.bssids)


def test_interface_matches_fixture(trained):
    a, b = ort.InferenceSession(str(trained[2])), ort.InferenceSession(str(FIXTURE))
    sig = lambda s: ([(i.name, i.type, len(i.shape)) for i in s.get_inputs()], [(o.name, o.type, len(o.shape)) for o in s.get_outputs()])
    assert sig(a) == sig(b)


def test_window_baseline_detects_drop(tmp_path):
    drop = 3600.0
    snap = Snapshot(SynthGenerator(CONFIG["synth"]).write(tmp_path / "w.duckdb", seed=4, drop_at=drop))
    feats = SlopeFeatures(CONFIG["window"])
    rows = feats.extract(snap.readings())
    X = feats.matrix(rows)
    rel = np.array([r[1] - START_TS for r in rows])
    falling = (rel >= drop + 120) & (rel <= drop + 240)
    calm = rel < drop - 300
    mask = falling | calm
    pred = ThresholdWindowDetector().fit(X[mask], falling[mask]).predict(X)
    assert pred[falling].mean() > 0.9 and pred[calm].mean() < 0.05
    with pytest.raises(NotImplementedError):
        ThresholdWindowDetector().export_onnx("x.onnx")
