from datetime import datetime, timezone
from pathlib import Path
import duckdb
import numpy as np

START_TS = 1_761_580_800.0
SCHEMA = """
CREATE TABLE scans (scan_id BIGINT, ts TIMESTAMP, received_at TIMESTAMP, device VARCHAR, label VARCHAR, bssid VARCHAR, rssi INTEGER, freq INTEGER);
CREATE TABLE readings (ts TIMESTAMP, node UBIGINT, endpoint USMALLINT, attribute VARCHAR, value DOUBLE);
CREATE TABLE setpoints (ts TIMESTAMP, node UBIGINT, room VARCHAR, value DOUBLE, reason VARCHAR);
CREATE TABLE predictions (ts TIMESTAMP, device VARCHAR, room VARCHAR, probability DOUBLE);
"""


def utc(x):
    return datetime.fromtimestamp(x, timezone.utc).replace(tzinfo=None)


class SynthGenerator:
    def __init__(self, cfg):
        self.cfg = cfg
        rng = np.random.default_rng(cfg["layout_seed"])
        n_ap = cfg["n_aps"]
        self.bssids = [f"aa:bb:cc:01:{i // 256:02x}:{i % 256:02x}" for i in range(n_ap)]
        self.freqs = {b: int(rng.choice([2412, 2437, 5180, 5500])) for b in self.bssids}
        self.means = rng.uniform(-90, -40, (len(cfg["rooms"]), n_ap))
        self.sd = rng.uniform(1.5, 3.5, n_ap)
        self.noisy = rng.random(n_ap) < cfg["noisy_fraction"]

    def scans(self, seed):
        c = self.cfg
        rng = np.random.default_rng(seed)
        out, t, sid, transient = [], START_TS, int(seed) * 1_000_000, 0
        for ri, room in enumerate(c["rooms"]):
            for _ in range(c["scans_per_room"]):
                aps = {}
                for i, b in enumerate(self.bssids):
                    rssi = self.means[ri, i] + rng.normal(0, 12.0 if self.noisy[i] else self.sd[i])
                    if rng.random() > (0.5 if self.noisy[i] else c["dropout"]) and rssi > c["sensitivity"]:
                        aps[b] = int(round(rssi))
                if rng.random() < c["transient_prob"]:
                    transient += 1
                    aps[f"de:ad:be:ef:{transient // 256:02x}:{transient % 256:02x}"] = int(rng.integers(-90, -60))
                out.append({"scan_id": sid, "ts": t, "label": room, "aps": aps})
                sid += 1
                t += c["interval_s"]
        return out

    def readings(self, seed, drop_at=None):
        c = self.cfg["temperature"]
        rng = np.random.default_rng(seed)
        ts = START_TS + np.arange(c["n"]) * c["interval_s"]
        v = c["base"] + rng.normal(0, c["noise"], c["n"])
        if drop_at is not None:
            v = v - np.clip(ts - (START_TS + drop_at), 0, None) / 60.0 * c["drop_per_min"]
        return ts, v

    def write(self, path, seed=1, drop_at=None):
        path = Path(path)
        path.parent.mkdir(parents=True, exist_ok=True)
        path.unlink(missing_ok=True)
        scan_rows = [(s["scan_id"], utc(s["ts"]), utc(s["ts"]), "synth", s["label"], b, r, self.freqs.get(b, 2412))
                     for s in self.scans(seed) for b, r in s["aps"].items()]
        ts, v = self.readings(seed, drop_at)
        read_rows = [(utc(t), 1, 1, "local_temperature", float(x)) for t, x in zip(ts, v)]
        con = duckdb.connect(str(path))
        con.execute(SCHEMA)
        con.executemany("INSERT INTO scans VALUES (?,?,?,?,?,?,?,?)", scan_rows)
        con.executemany("INSERT INTO readings VALUES (?,?,?,?,?)", read_rows)
        con.close()
        return path
