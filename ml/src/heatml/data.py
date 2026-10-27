from dataclasses import dataclass
from pathlib import Path
import duckdb
import numpy as np

REPO_ROOT = next(p for p in Path(__file__).resolve().parents if (p / "contract").is_dir())
DEFAULT_SNAPSHOT = REPO_ROOT / "data" / "snapshot.duckdb"


@dataclass
class Scan:
    scan_id: int
    ts: float
    label: str | None
    aps: dict


class Snapshot:
    def __init__(self, path=None):
        self.path = Path(path) if path else DEFAULT_SNAPSHOT

    def query(self, sql, params=()):
        con = duckdb.connect(str(self.path), read_only=True)
        try:
            return con.execute(sql, list(params)).fetchall()
        finally:
            con.close()

    def scans(self, labelled_only=True):
        where = "WHERE label IS NOT NULL" if labelled_only else ""
        rows = self.query(f"SELECT scan_id, epoch(ts), label, bssid, rssi FROM scans {where} ORDER BY ts, scan_id")
        grouped = {}
        for scan_id, ts, label, bssid, rssi in rows:
            scan = grouped.setdefault(scan_id, Scan(scan_id, float(ts), label, {}))
            scan.aps[bssid] = max(rssi, scan.aps.get(bssid, rssi))
        return list(grouped.values())

    def readings(self, attribute="local_temperature"):
        rows = self.query("SELECT epoch(ts), node, value FROM readings WHERE attribute = ? ORDER BY ts", [attribute])
        ts, node, value = (np.array(c) for c in zip(*rows)) if rows else (np.array([]),) * 3
        return {"ts": ts.astype(float), "node": node, "value": value.astype(float)}
