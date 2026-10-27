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

    def observe(self, means, rng):
        c = self.cfg
        aps = {}
        for i, b in enumerate(self.bssids):
            rssi = means[i] + rng.normal(0, 12.0 if self.noisy[i] else self.sd[i])
            if rng.random() > (0.5 if self.noisy[i] else c["dropout"]) and rssi > c["sensitivity"]:
                aps[b] = int(round(rssi))
        if rng.random() < c["transient_prob"]:
            self.transient += 1
            aps[f"de:ad:be:ef:{self.transient // 256:02x}:{self.transient % 256:02x}"] = int(rng.integers(-90, -60))
        return aps

    def scans(self, seed):
        c = self.cfg
        rng = np.random.default_rng(seed)
        out, t, sid, self.transient = [], START_TS, int(seed) * 1_000_000, 0
        for ri, room in enumerate(c["rooms"]):
            for _ in range(c["scans_per_room"]):
                out.append({"scan_id": sid, "ts": t, "label": room, "aps": self.observe(self.means[ri], rng)})
                sid += 1
                t += c["interval_s"]
        return out

    def place_means(self):
        c = self.cfg["trajectory"]
        rng = np.random.default_rng(self.cfg["layout_seed"] + 1)
        places = c["rooms"] + c["extra"]
        means = {p: rng.uniform(-90, -40, self.cfg["n_aps"]) for p in places}
        for a, b in c["adjacent"]:
            means[b] = means[a].copy()
            idx = rng.choice(self.cfg["n_aps"], c["adjacent_aps"], replace=False)
            means[b][idx] += rng.choice([-1, 1], len(idx)) * rng.uniform(0.7, 1.3, len(idx)) * c["adjacent_offset"]
        return means

    def trajectory(self, seed):
        c = self.cfg["trajectory"]
        rng = np.random.default_rng(seed)
        means = self.place_means()
        dwell_places = [p for p in c["rooms"] if p != c["walk_place"]] + c["extra"]
        out, sid, self.transient = [], int(seed) * 1_000_000, 0

        def emit(t, place):
            nonlocal sid
            out.append({"scan_id": sid, "ts": t, "label": None, "truth": place, "aps": self.observe(means[place], rng)})
            sid += 1

        for day in range(c["days"]):
            t, prev = START_TS + day * 86400.0 + c["day_start_h"] * 3600.0, None
            for _ in range(c["stays_per_day"]):
                weights = np.array([c["extra_weight"] if p in c["extra"] else 1.0 for p in dwell_places])
                weights = np.array([0.0 if p == prev else w for p, w in zip(dwell_places, weights)])
                prev = str(rng.choice(dwell_places, p=weights / weights.sum()))
                for _ in range(int(rng.integers(*c["walk_scans"]))):
                    emit(t, c["walk_place"])
                    t += c["walk_interval_s"]
                for _ in range(max(1, int(rng.integers(*c["dwell_min"]) * 60 // c["dwell_interval_s"]))):
                    emit(t, prev)
                    t += c["dwell_interval_s"]
        for room in c["rooms"]:
            idx = [i for i, s in enumerate(out) if s["truth"] == room]
            for i in rng.choice(idx, min(c["labelled_per_room"], len(idx)), replace=False):
                out[i]["label"] = room
        return out

    def held_out(self, seed, per_place=60):
        c = self.cfg["trajectory"]
        rng = np.random.default_rng(seed)
        means = self.place_means()
        self.transient = 0
        return [{"label": p, "aps": self.observe(means[p], rng)} for p in c["rooms"] for _ in range(per_place)]

    def readings(self, seed, drop_at=None):
        c = self.cfg["temperature"]
        rng = np.random.default_rng(seed)
        ts = START_TS + np.arange(c["n"]) * c["interval_s"]
        v = c["base"] + rng.normal(0, c["noise"], c["n"])
        if drop_at is not None:
            v = v - np.clip(ts - (START_TS + drop_at), 0, None) / 60.0 * c["drop_per_min"]
        return ts, v

    def write(self, path, seed=1, drop_at=None, trajectory=False):
        path = Path(path)
        path.parent.mkdir(parents=True, exist_ok=True)
        path.unlink(missing_ok=True)
        scan_rows = [(s["scan_id"], utc(s["ts"]), utc(s["ts"]), "synth", s["label"], b, r, self.freqs.get(b, 2412))
                     for s in (self.trajectory(seed) if trajectory else self.scans(seed)) for b, r in s["aps"].items()]
        ts, v = self.readings(seed, drop_at)
        read_rows = [(utc(t), 1, 1, "local_temperature", float(x)) for t, x in zip(ts, v)]
        con = duckdb.connect(str(path))
        con.execute(SCHEMA)
        con.executemany("INSERT INTO scans VALUES (?,?,?,?,?,?,?,?)", scan_rows)
        con.executemany("INSERT INTO readings VALUES (?,?,?,?,?)", read_rows)
        con.close()
        return path
