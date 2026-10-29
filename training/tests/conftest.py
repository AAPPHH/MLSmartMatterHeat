import json
from datetime import datetime, timedelta, timezone
from zoneinfo import ZoneInfo
import numpy as np
import pyarrow as pa
import pyarrow.parquet as pq
import pytest
from heattrain.__main__ import CONFIG

BERLIN = ZoneInfo("Europe/Berlin")
START = datetime(2026, 10, 1, tzinfo=BERLIN)
SYNTH = {
    "rooms": ["wohnzimmer", "kueche", "schlafzimmer", "bad"], "n_aps": 14, "layout_seed": 3, "room_sigma": 9.0, "noise": 3.0, "dropout": 0.06,
    "sensitivity": -92, "scan_s": 300, "night_scan_s": 600, "dwell_min": [25, 80], "label_fraction": 0.8, "legacy_days": 2,
    "links": [{"a": "wohnzimmer", "b": "kueche", "kind": "open", "partial": False}, {"a": "wohnzimmer", "b": "schlafzimmer", "kind": "wall", "partial": False},
              {"a": "schlafzimmer", "b": "bad", "kind": "wall", "partial": False}, {"a": "kueche", "b": "bad", "kind": "wall", "partial": False}],
}
TRANSIENT = "de:ad:be:ef:00:01"


def bssid(i):
    return f"aa:bb:cc:00:00:{i:02x}"


def layout():
    rng = np.random.default_rng(SYNTH["layout_seed"])
    base = rng.uniform(-82, -52, SYNTH["n_aps"])
    return {r: base + rng.normal(0, SYNTH["room_sigma"], SYNTH["n_aps"]) for r in SYNTH["rooms"]}


def next_room(rng, room):
    near = [l["b"] if l["a"] == room else l["a"] for l in SYNTH["links"] if room in (l["a"], l["b"])]
    return str(rng.choice(near))


def day_rows(day, means, rng):
    s = SYNTH
    watch, sensors, labels = [], [], []
    t0 = START + timedelta(days=day)
    t, room, batch = t0 + timedelta(hours=7), "schlafzimmer", day * 100000
    end = t0 + timedelta(hours=23)
    stays = []
    while t < end:
        room = next_room(rng, room)
        dwell = timedelta(minutes=float(rng.integers(*s["dwell_min"])))
        stays.append((room, t, min(t + dwell, end), rng.random() < 0.5))
        t += dwell
    stays.append(("schlafzimmer", end, t0 + timedelta(days=1, hours=7), None))
    for room, a, b, active in stays:
        night = active is None
        step = timedelta(seconds=s["night_scan_s"] if night else s["scan_s"])
        ts = a
        while ts < b:
            batch += 1
            for i, m in enumerate(means[room]):
                v = m + rng.normal(0, s["noise"])
                if v >= s["sensitivity"] and rng.random() > s["dropout"]:
                    watch.append((ts, bssid(i), int(round(v)), batch))
            if day == 2 and rng.random() < 0.2:
                watch.append((ts, TRANSIENT, -60, batch))
            ts += step
        m = a
        while m < b:
            hr = 52 if night else (105 if active else 66)
            sensors.append((m, "hr", hr + rng.normal(0, 2)))
            sensors.append((m, "motion", 0.05 if not active else 1.2))
            m += timedelta(minutes=1)
        if rng.random() < s["label_fraction"]:
            labels.append((b + timedelta(seconds=10), a, b, room, "watch"))
        elif not night:
            labels.append((a + timedelta(minutes=2), None, None, room, "manual"))
    return watch, sensors, labels


def write_table(root, name, rows, schema, key):
    by_date = {}
    for r in rows:
        by_date.setdefault(r[key].astimezone(timezone.utc).date().isoformat(), []).append(r)
    for date, part in by_date.items():
        d = root / "parquet" / name / f"date={date}"
        d.mkdir(parents=True, exist_ok=True)
        pq.write_table(pa.Table.from_pylist(part, schema=schema), d / f"part.parquet")


UTC = pa.timestamp("us", tz="UTC")
LEGACY = pa.schema([("ts", UTC), ("sender", pa.string()), ("rssi", pa.int8()), ("batch_id", pa.uint64()), ("device", pa.string())])
MODERN = pa.schema(list(LEGACY) + [("sensor_type", pa.string()), ("value", pa.float32())])
LABELS = pa.schema([("ts", UTC), ("from_ts", UTC), ("to_ts", UTC), ("room", pa.string()), ("source", pa.string()), ("device", pa.string())])
META = pa.schema([("ts", UTC), ("key", pa.string()), ("value", pa.string()), ("valid_from", UTC)])


def make_lake(root, days, with_meta=True):
    means = layout()
    for day in range(days):
        rng = np.random.default_rng([SYNTH["layout_seed"], day])
        watch, sensors, labels = day_rows(day, means, rng)
        legacy = day < SYNTH["legacy_days"]
        rows = [{"ts": t, "sender": s, "rssi": r, "batch_id": b, "device": "pixel"} for t, s, r, b in watch]
        if legacy:
            write_table(root, "watch_raw", rows, LEGACY, "ts")
        else:
            rows = [dict(r, sensor_type="rssi", value=float(r["rssi"])) for r in rows]
            rows += [{"ts": t, "sender": None, "rssi": None, "batch_id": 0, "device": "pixel", "sensor_type": k, "value": v} for t, k, v in sensors]
            write_table(root, "watch_raw", rows, MODERN, "ts")
        write_table(root, "labels", [{"ts": t, "from_ts": f, "to_ts": e, "room": r, "source": src, "device": "pixel"} for t, f, e, r, src in labels], LABELS, "ts")
    if with_meta:
        now = START
        write_table(root, "meta", [{"ts": now, "key": "links", "value": json.dumps(SYNTH["links"]), "valid_from": now}], META, "ts")
    return root


@pytest.fixture(scope="session")
def lake(tmp_path_factory):
    return make_lake(tmp_path_factory.mktemp("lake"), 10)


@pytest.fixture(scope="session")
def config(tmp_path_factory):
    return dict(CONFIG, eval_dir=tmp_path_factory.mktemp("eval"))
