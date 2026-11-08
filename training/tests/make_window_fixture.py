import copy
import shutil
import tempfile
from datetime import datetime, timezone
from pathlib import Path
import numpy as np
from heattrain import windowml
from heattrain.__main__ import CONFIG
from heattrain.windowml import Grid, parity_csv, since_open, window_bundle

OUT = Path(__file__).resolve().parents[2] / "crates" / "heatd" / "tests" / "fixtures"
SPAN = ["2026-11-07T02:00:00+00:00", "2026-11-07T11:00:00+00:00"]
START = datetime(2026, 11, 5, 11, 0, tzinfo=timezone.utc)
DAYS = 3
OPEN = ("2026-11-07T08:05:00+00:00", "bad", 25)
SUPPLY = ("2026-11-07T04:00:00+00:00", 40)
THRESHOLDS = {"house": 2.0, "wohnzimmer": 1.5, "schlafzimmer": 1.5, "bad": 1.5, "kueche": 1.5}
SIGMA = {"wohnzimmer": 0.25, "schlafzimmer": 0.3, "bad": 0.2, "kueche": 0.25}
GAIN = {"wohnzimmer": 0.012, "schlafzimmer": 0.008, "bad": 0.01, "kueche": 0.011}
LEAK = {"wohnzimmer": 0.0016, "schlafzimmer": 0.0022, "bad": 0.0018, "kueche": 0.0014}


def synth_grid(cfg, seed=7):
    rng = np.random.default_rng(seed)
    n = DAYS * 1440
    t = START.timestamp() + 60.0 * np.arange(n)
    hour = ((t + cfg["utc_offset_min"] * 60) % 86400) / 3600.0
    out = 5.0 + 3.0 * np.sin((hour - 9.0) / 24 * 2 * np.pi) + np.cumsum(0.02 * rng.standard_normal(n)) * 0.2
    shared = {"out": out, "wind": 2.5 + rng.uniform(0, 1.5, n), "rad": np.maximum(0.0, 250 * np.sin((hour - 6.0) / 12 * np.pi)) * (hour > 6) * (hour < 18), "supply_event": np.zeros(n, bool)}
    start = int((datetime.fromisoformat(SUPPLY[0]).timestamp() - t[0]) // 60)
    shared["supply_event"][start:start + SUPPLY[1]] = True
    opened = int((datetime.fromisoformat(OPEN[0]).timestamp() - t[0]) // 60)
    room = {}
    for r in cfg["rooms"]:
        setpoint = np.where((hour >= 6) & (hour < 22), 20.5, 18.0)
        temp, valve = np.zeros(n), np.zeros(n)
        x, v = 19.0, 0.0
        for i in range(n):
            v = float(np.clip(60.0 * (setpoint[i] - x) + 10.0, 0.0, 100.0)) if i % 5 == 0 else v
            if r == OPEN[1] and opened <= i < opened + OPEN[2]:
                x -= 0.11
            x += GAIN[r] * v / 10.0 - LEAK[r] * (x - shared["out"][i]) + 0.004 * rng.standard_normal()
            temp[i], valve[i] = x, v
        temp = np.round(temp + 0.02 * rng.standard_normal(n), 2)
        room[r] = {"temp": temp, "valve": np.round(valve), "setpoint": setpoint.astype(float), "since_open": since_open(np.round(valve), cfg["open_valve_pct"])}
    return Grid(t, list(cfg["rooms"]), room, shared, cfg)


def fake_result(grid, cls, cfg):
    det = {"A": {"detected": True, "latency_min": 12, "channel": "house"}}
    return {"detected_all": True, "detection": det, "latency_sum": 12, "threshold": THRESHOLDS, "extra": 0, "alarms_per_day": 0.0, "sigma": SIGMA, "alarms": []}


def main():
    cfg = copy.deepcopy(CONFIG["window"])
    grid = synth_grid(cfg)
    windowml.Grid.load = classmethod(lambda cls, lake, c: grid)
    windowml.evaluate_room = fake_result
    with tempfile.TemporaryDirectory() as tmp:
        out, bundle, _, _, grid = window_bundle({"window": cfg}, "synthetic", tmp, version="parity")
        shutil.copy(out / "bundle.json", OUT / "window_bundle.json")
        (OUT / "window_parity.csv").write_text(parity_csv(bundle, grid, grid.span(*SPAN)))


if __name__ == "__main__":
    main()
