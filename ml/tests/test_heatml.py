import json
import numpy as np
import onnxruntime as ort
import pytest
from heatml.__main__ import CONFIG, cluster, fit_model, train
from heatml.cluster import UNKNOWN, SemiSupervisedRoom
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



@pytest.fixture(scope="module", params=[1, 2])
def semi(request, tmp_path_factory):
    d = tmp_path_factory.mktemp("c")
    gen = SynthGenerator(CONFIG["synth"])
    db = gen.write(d / "traj.duckdb", seed=request.param, trajectory=True)
    scans = Snapshot(db).scans(labelled_only=False)
    model = SemiSupervisedRoom(CONFIG).fit(scans)
    onnx_path, json_path = model.classifier.export(d / "m" / "room_model.onnx")
    truth = {s["scan_id"]: s["truth"] for s in gen.trajectory(request.param)}
    return gen, scans, model, onnx_path, json_path, db, truth


def test_semi_supervised_accuracy(semi):
    gen, scans, model, onnx_path, json_path, _, _ = semi
    assert sum(s.label is not None for s in scans) == 5 * CONFIG["synth"]["trajectory"]["labelled_per_room"]
    side = json.loads(json_path.read_text())
    assert sorted(side["labels"]) == sorted(CONFIG["synth"]["trajectory"]["rooms"])
    held = gen.held_out(seed=9)
    X = np.stack([vectorize(s["aps"], side["bssids"], side["rssi_floor"], side["rssi_span"]) for s in held])
    label, _ = ort.InferenceSession(str(onnx_path)).run(["label", "probabilities"], {"input": X})
    truth = np.array([side["labels"].index(s["label"]) for s in held])
    assert (label == truth).mean() > 0.9


def test_unlabelled_place_stays_unknown(semi):
    scans, model, truth = semi[1], semi[2], semi[6]
    keller = [s for s in scans if truth[s.scan_id] == "keller"]
    assert keller
    assert np.mean([model.clusterer.names[s.scan_id] == UNKNOWN for s in keller]) > 0.9
    assert "keller" not in model.classifier.labels
    kept = {s.scan_id for s in model.clusterer.named(scans)}
    assert all(model.clusterer.names[s.scan_id] == UNKNOWN for s in scans if s.scan_id not in kept)


def test_adjacent_rooms_not_merged(semi):
    scans, model, truth = semi[1], semi[2], semi[6]
    for a, b in CONFIG["synth"]["trajectory"]["adjacent"]:
        names_a = {model.clusterer.names[s.scan_id] for s in scans if truth[s.scan_id] == a}
        names_b = {model.clusterer.names[s.scan_id] for s in scans if truth[s.scan_id] == b}
        assert a in names_a and b in names_b and b not in names_a and a not in names_b


def test_cli_dispatch_and_fallback(semi, capsys):
    db = semi[5]
    cfg = {**CONFIG, "snapshot": db, "model_dir": db.parent / "out"}
    train(cfg)
    assert UNKNOWN in capsys.readouterr().out and (db.parent / "out" / "room_model.onnx").exists()
    cluster(cfg)
    assert "clusters" in capsys.readouterr().out
    sup = SynthGenerator(CONFIG["synth"]).write(db.parent / "sup.duckdb", seed=1)
    clf, clusterer = fit_model({**cfg, "snapshot": sup})
    assert clusterer is None and len(clf.labels) == 4


def make_viz_db(path, filled):
    import duckdb
    con = duckdb.connect(str(path))
    con.execute("CREATE TABLE scans(scan_id BIGINT, ts TIMESTAMP, received_at TIMESTAMP, device VARCHAR, label VARCHAR, bssid VARCHAR, rssi INTEGER, freq INTEGER)")
    if filled:
        for i in range(30):
            for b, base in enumerate([-40, -60, -80]):
                con.execute("INSERT INTO scans VALUES (?, ?, now(), 'w', ?, ?, ?, ?)", [i, f"2026-10-28 07:{i:02d}:00", ["bad", "kueche", None][i % 3], f"aa:bb:cc:dd:ee:0{b}", base - 15 * (i % 3), 2412])
    con.close()
    return path


@pytest.mark.parametrize("filled", [True, False])
def test_viz_builds_umap(tmp_path, filled):
    from heatml.viz import build
    html = build(make_viz_db(tmp_path / "s.duckdb", filled), tmp_path / "d.html").read_text(encoding="utf-8")
    assert "UMAP 3D" in html and ("Cluster" in html) and (("kueche" in html) == filled)


def make_usage_lake(root):
    import duckdb
    con = duckdb.connect()
    base = 1_792_000_000 // 86400 * 86400
    rows = []
    for day in range(3):
        for minute in range(0, 1440):
            ts = base + day * 86400 + minute * 60
            hour = minute // 60
            rows.append((1, ts, 60 if hour in (6, 7) else 0))
            rows.append((2, ts, 100 if hour == 18 and day == 1 else (255 if hour == 12 else 0)))
    con.execute("CREATE TABLE ar(node UBIGINT, ts TIMESTAMP, cluster UINTEGER, attribute UINTEGER, v_i64 BIGINT, v_f64 DOUBLE)")
    con.executemany("INSERT INTO ar VALUES (?, to_timestamp(?), 513, 8, ?, NULL)", rows)
    for node in (1, 2):
        part = root / f"parquet/attribute_reports/date=2026-10-20/node={node}"
        part.mkdir(parents=True)
        con.execute(f"COPY (SELECT ts, cluster, attribute, v_i64, v_f64 FROM ar WHERE node = {node}) TO '{(part / 'part.parquet').as_posix()}' (FORMAT parquet)")
    (root / "parquet/meta/date=2026-10-20").mkdir(parents=True)
    con.execute(f"COPY (SELECT now() AS ts, 'rooms' AS key, '[{{\"name\":\"a\",\"nodes\":[1]}},{{\"name\":\"b\",\"nodes\":[2]}}]' AS value) TO '{(root / 'parquet/meta/date=2026-10-20/part.parquet').as_posix()}' (FORMAT parquet)")
    con.close()


def test_usage_valve_hours_and_html(tmp_path):
    from heatml.__main__ import CONFIG, usage
    from heatml.viz import HeatingUsage
    make_usage_lake(tmp_path)
    u = HeatingUsage(tmp_path)
    days, hours, mins = u.tables()
    assert len(days) >= 3 and set(hours) == {"a", "b"}
    assert hours["a"].sum() == pytest.approx(3 * 2 * 0.6, abs=0.02)
    assert hours["b"].sum() == pytest.approx(1.0, abs=0.02)
    assert mins["a"][:, 0].sum() == pytest.approx(3 * 120, abs=3)
    assert hours["b"][:, 12].sum() == 0
    out = tmp_path / "out" / "usage.html"
    usage({**CONFIG, "lake_dir": tmp_path, "usage_out": out})
    html = out.read_text(encoding="utf-8")
    assert "Ventilstunden" in html and "Haus gesamt" in html and "Heatmap" in html.replace("heatmap", "Heatmap")


def test_usage_empty_lake(tmp_path):
    from heatml.viz import build_usage
    assert "Haus gesamt" in build_usage(tmp_path, tmp_path / "e.html").read_text(encoding="utf-8")
