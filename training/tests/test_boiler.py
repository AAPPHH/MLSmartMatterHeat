import copy
import json
from datetime import datetime
from zoneinfo import ZoneInfo
import numpy as np
import pyarrow as pa
import pyarrow.parquet as pq
import pytest
from heattrain.__main__ import CONFIG
from heattrain.boiler import Boiler, boiler, load_events

CFG = copy.deepcopy(CONFIG["boiler"])
BERLIN = ZoneInfo("Europe/Berlin")
T0 = datetime(2026, 10, 1, tzinfo=BERLIN).timestamp()
ROOMS = [("wohnzimmer", 5, 0), ("schlafzimmer", 6, 0), ("bad", 7, 10), ("kueche", 8, 25)]
META = [("rooms", json.dumps([{"name": n, "nodes": [k]} for n, k, _ in ROOMS]), 0.0)]
JITTER = [-10, 5, 15, -5, 0]


def simulate(days=4, flow_offset=0.0, coupling=0.6, cycle=False):
    rng = np.random.default_rng(0)
    reports, weather = [], []
    for h in range(days * 24 + 1):
        m = h * 60
        weather.append((T0 + m * 60.0, "obs", T0, 8.0 - 2.0 * (m // 1440) + 4.0 * np.sin(2 * np.pi * (m % 1440 - 660) / 1440), 2.0, 0.0))
    air = {n: 19.0 for n, _, _ in ROOMS}
    x = dict(air)
    for m in range(days * 1440):
        day, tod = divmod(m, 1440)
        tout = 8.0 - 2.0 * day + 4.0 * np.sin(2 * np.pi * (tod - 660) / 1440)
        start = 330 + JITTER[day]
        flow = 55.0 - 1.2 * tout + flow_offset
        for name, node, delay in ROOMS:
            arrive = start + delay
            u = 0.6 if (tod >= 1410 or tod < arrive) else 1.0 if tod < arrive + 90 else 0.0
            supplied = arrive <= tod < 1320
            target = 17.0 if (tod >= 1320 or tod < start) else 21.0
            air[name] += (target - air[name]) / 240.0
            goal = air[name] + (u * coupling * (flow - air[name]) if supplied else 0.0)
            x[name] += (goal - x[name]) / (10.0 if goal > x[name] else 20.0)
            y = x[name]
            if cycle and name == "bad":
                u, y = 0.8, 30.0 + np.sin(2 * np.pi * m / 12.0)
            v = 134 if name == "kueche" and rng.random() < 0.7 else int(round(u * 100))
            ts = T0 + m * 60.0
            reports += [(ts, node, 0, float(round(y * 2) / 2 * 100)), (ts + 1, node, 8, float(v)), (ts + 2, node, 18, 2000.0)]
    return reports, weather


@pytest.fixture(scope="module")
def doc():
    reports, weather = simulate()
    return Boiler(CFG, reports, weather, META).run()


def clock(hhmm):
    h, m = hhmm.split(":")
    return int(h) * 60 + int(m)


def test_night_setback_and_supply_start(doc):
    nb = doc["nachtabsenkung"]
    assert nb["belastbar"]
    starts = [clock(n["beginn_bestaetigt"]) for n in nb["naechte"] if n["beginn_bestaetigt"]]
    assert all(abs(s - 1410) <= 35 or s <= 35 for s in starts)
    ends = [clock(n["ende"]) for n in nb["naechte"] if n["ende"]]
    assert len(ends) >= 3
    expected = [330 + j for j in JITTER[1:len(ends) + 1]]
    assert np.mean(np.abs(np.array(ends[-len(expected):]) - np.array(expected))) < 25
    assert 2 < nb["ende"]["std_min"] < 20
    assert nb["ende"]["n"] == len(ends)


def test_morning_recovery_listed(doc):
    rows = [r for n in doc["nachtabsenkung"]["naechte"] for r in n["raeume"]]
    assert {r["raum"] for r in rows} == {n for n, _, _ in ROOMS}
    assert all(r["sensor_max_c"] > 25 for r in rows)


def test_no_supply_episodes_detected(doc):
    ns = [w for v in doc["lieferung"].values() for w in v["ohne_lieferung"]]
    assert ns and {w["raum"] for w in ns} <= {"wohnzimmer", "schlafzimmer", "bad"}
    assert all(w["minuten"] >= CFG["nosupply_min"] for w in ns)
    assert any("keine Waermelieferung" in f["text"] for f in doc["befunde"])


def test_flow_temperature_and_heating_curve(doc):
    v = doc["vorlauf"]
    full = [e for e in v["ereignisse"] if "vorlauf_c" in e]
    assert len(full) >= 6
    for e in full:
        truth = 55.0 - 1.2 * e["aussen_c"]
        assert abs(e["vorlauf_c"] - truth) < 4.0 and e["vorlauf_von_c"] < truth < e["vorlauf_bis_c"] + 2
        assert 7 < e["tau_min"] < 14
    assert v["kurve"]["wetterfuehrung"] and abs(v["kurve"]["steigung_k_pro_k"] + 1.2) < 0.5
    assert v["bewertung"] == "plausibel"


def test_flow_too_high_is_flagged():
    reports, weather = simulate(flow_offset=22.0)
    d = Boiler(CFG, reports, weather, META).run()
    assert d["vorlauf"]["bewertung"] == "zu hoch"
    assert any(f["thema"] == "vorlauf" for f in d["befunde"])


def test_hydraulic_delay_and_invalid_share(doc):
    r = doc["hydraulik"]["raeume"]
    assert r["wohnzimmer"]["verzoegerung_min"] < 6
    assert 4 < r["bad"]["verzoegerung_min"] < 18
    assert 15 < r["kueche"]["verzoegerung_min"] < 35
    assert 0.6 < r["kueche"]["ventil_ungueltig_anteil"] < 0.8 and r["wohnzimmer"]["ventil_ungueltig_anteil"] == 0
    assert any(f["thema"] == "geraet" and "kueche" in f["text"] and f["stufe"] == "hoch" for f in doc["befunde"])
    assert any(f["thema"] == "hydraulik" and "kueche" in f["text"] for f in doc["befunde"])
    assert all(r[n]["leistung"]["kph_pro_ventil"] > 0 for n in ("wohnzimmer", "schlafzimmer", "bad"))


def test_no_cycling_in_smooth_data(doc):
    assert all(t["mit_schwingung"] == 0 for t in doc["takten"]["raeume"].values())


def test_cycling_detected():
    reports, weather = simulate(days=2, cycle=True)
    d = Boiler(CFG, reports, weather, META).run()
    t = d["takten"]["raeume"]["bad"]
    assert t["mit_schwingung"] >= 3 and abs(t["periode_median_min"] - 12) < 2
    assert any(f["thema"] == "takten" and "bad" in f["text"] for f in d["befunde"])


def test_daily_report_hook(doc):
    day = sorted(doc["tagesbericht"])[1]
    text = doc["tagesbericht"][day]
    assert text.startswith("\n## Kesselversorgung (Analyse)\n") and "Waermelieferung erkannt" in text


def test_short_data_is_marked_unreliable():
    reports, weather = simulate(days=1)
    d = Boiler(CFG, reports, weather, META).run()
    assert not any(d["belastbar"].values())
    assert d["vorlauf"]["kurve"] is None


def lake(root, reports, weather, supply):
    def tbl(name, schema, rows):
        d = root / "parquet" / name / "date=2026-10-01"
        d.mkdir(parents=True, exist_ok=True)
        pq.write_table(pa.Table.from_pylist(rows, schema=schema), d / "part.parquet")

    ts = lambda s: datetime.fromtimestamp(s, ZoneInfo("UTC"))
    UTC = pa.timestamp("us", tz="UTC")
    tbl("attribute_reports", pa.schema([("ts", UTC), ("node", pa.int64()), ("cluster", pa.uint32()), ("attribute", pa.uint32()), ("v_f64", pa.float64()), ("v_i64", pa.int64()), ("source", pa.string())]),
        [{"ts": ts(t), "node": n, "cluster": 513, "attribute": a, "v_f64": None, "v_i64": int(v), "source": "live"} for t, n, a, v in reports])
    tbl("weather", pa.schema([("ts", UTC), ("kind", pa.string()), ("issued_ts", UTC), ("temp_c", pa.float32()), ("wind_ms", pa.float32()), ("radiation_wm2", pa.float32())]),
        [{"ts": ts(t), "kind": k, "issued_ts": ts(i), "temp_c": a, "wind_ms": b, "radiation_wm2": c} for t, k, i, a, b, c in weather])
    tbl("meta", pa.schema([("key", pa.string()), ("value", pa.string()), ("valid_from", UTC)]), [{"key": k, "value": v, "valid_from": ts(f)} for k, v, f in META])
    tbl("events", pa.schema([("ts", UTC), ("kind", pa.string()), ("detail", pa.string())]), [{"ts": ts(t), "kind": "supply", "detail": json.dumps(d)} for t, d in supply])
    return root


def test_cli_end_to_end(tmp_path):
    reports, weather = simulate(days=3)
    src = lake(tmp_path / "lake", reports, weather, [(T0 + 3600, {"room": "bad", "state": "end", "since_ms": 1})])
    assert len(load_events(src, "supply")) == 1
    out, doc = tmp_path / "out", None
    doc, path = boiler(CONFIG, src, out)
    assert (path / "boiler.json").exists() and json.loads((path / "boiler.json").read_text(encoding="utf-8"))["daten"]["tage"] > 2.9
    md = (path / "boiler.md").read_text(encoding="utf-8")
    for h in ("## Datenlage", "## Wann liefert der Kessel", "## Nachtabsenkung", "## Vorlauftemperatur", "## Heizkoerper und Hydraulik", "## Brenner-Takten", "## Was wir zusaetzlich braeuchten"):
        assert h in md
    assert "Plotly.newPlot" in (path / "boiler.html").read_text(encoding="utf-8")
    assert doc["heatd_abgleich"]["heatd_ereignisse"] == 1
