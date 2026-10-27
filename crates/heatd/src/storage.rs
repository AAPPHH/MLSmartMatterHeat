use anyhow::Result;
use duckdb::{params, Connection};
use serde::{Deserialize, Serialize};
use std::path::Path;
use std::sync::Mutex;

const SCHEMA: &str = "
CREATE TABLE IF NOT EXISTS scans (scan_id BIGINT, ts TIMESTAMP, received_at TIMESTAMP, device VARCHAR, label VARCHAR, bssid VARCHAR, rssi INTEGER, freq INTEGER);
CREATE TABLE IF NOT EXISTS readings (ts TIMESTAMP, node UBIGINT, endpoint USMALLINT, attribute VARCHAR, value DOUBLE);
CREATE TABLE IF NOT EXISTS setpoints (ts TIMESTAMP, node UBIGINT, room VARCHAR, value DOUBLE, reason VARCHAR);
CREATE TABLE IF NOT EXISTS predictions (ts TIMESTAMP, device VARCHAR, room VARCHAR, probability DOUBLE);
";

#[derive(Debug, Clone, Deserialize, Serialize)]
pub struct AccessPoint {
    pub bssid: String,
    pub rssi: i32,
    pub freq: i32,
}

#[derive(Debug, Clone, Deserialize, Serialize)]
pub struct ScanBody {
    pub ts: i64,
    pub device: String,
    pub label: Option<String>,
    pub aps: Vec<AccessPoint>,
}

#[derive(Debug, Clone, Serialize, PartialEq)]
pub struct Reading {
    pub node: u64,
    pub attribute: String,
    pub value: f64,
    pub ts: i64,
}

#[derive(Debug, Clone, Serialize, PartialEq)]
pub struct SetpointRow {
    pub node: u64,
    pub room: String,
    pub value: f64,
    pub reason: String,
    pub ts: i64,
}

#[derive(Debug, Clone, Serialize, PartialEq)]
pub struct PredictionRow {
    pub device: String,
    pub room: String,
    pub probability: f64,
    pub ts: i64,
}

pub fn now_ms() -> i64 {
    std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).map_or(0, |d| d.as_millis() as i64)
}

pub struct Storage {
    conn: Mutex<Connection>,
}

impl Storage {
    pub fn open(path: &str) -> Result<Self> {
        if let Some(parent) = Path::new(path).parent().filter(|p| !p.as_os_str().is_empty()) {
            std::fs::create_dir_all(parent)?;
        }
        let conn = Connection::open(path)?;
        conn.execute_batch(SCHEMA)?;
        Ok(Self { conn: Mutex::new(conn) })
    }

    pub fn insert_scan(&self, scan: &ScanBody, received_ms: i64) -> Result<usize> {
        let conn = self.conn.lock().unwrap();
        let scan_id: i64 = conn.query_row("SELECT coalesce(max(scan_id), 0) + 1 FROM scans", [], |r| r.get(0))?;
        conn.execute_batch("BEGIN")?;
        let result = (|| -> Result<()> {
            let mut stmt = conn.prepare("INSERT INTO scans VALUES (?, epoch_ms(?), epoch_ms(?), ?, ?, ?, ?, ?)")?;
            for ap in &scan.aps {
                stmt.execute(params![scan_id, scan.ts, received_ms, scan.device, scan.label, ap.bssid.to_lowercase(), ap.rssi, ap.freq])?;
            }
            Ok(())
        })();
        conn.execute_batch(if result.is_ok() { "COMMIT" } else { "ROLLBACK" })?;
        result?;
        Ok(scan.aps.len())
    }

    pub fn insert_reading(&self, ts_ms: i64, node: u64, endpoint: u16, attribute: &str, value: f64) -> Result<()> {
        self.conn.lock().unwrap().execute("INSERT INTO readings VALUES (epoch_ms(?), ?, ?, ?, ?)", params![ts_ms, node, endpoint, attribute, value])?;
        Ok(())
    }

    pub fn insert_setpoint(&self, ts_ms: i64, node: u64, room: &str, value: f64, reason: &str) -> Result<()> {
        self.conn.lock().unwrap().execute("INSERT INTO setpoints VALUES (epoch_ms(?), ?, ?, ?, ?)", params![ts_ms, node, room, value, reason])?;
        Ok(())
    }

    pub fn insert_prediction(&self, ts_ms: i64, device: &str, room: &str, probability: f64) -> Result<()> {
        self.conn.lock().unwrap().execute("INSERT INTO predictions VALUES (epoch_ms(?), ?, ?, ?)", params![ts_ms, device, room, probability])?;
        Ok(())
    }

    pub fn last_readings(&self) -> Result<Vec<Reading>> {
        let conn = self.conn.lock().unwrap();
        let mut stmt = conn.prepare(
            "SELECT node, attribute, value, epoch_ms(ts) FROM readings QUALIFY row_number() OVER (PARTITION BY node, attribute ORDER BY ts DESC) = 1 ORDER BY node, attribute",
        )?;
        let rows = stmt.query_map([], |r| Ok(Reading { node: r.get(0)?, attribute: r.get(1)?, value: r.get(2)?, ts: r.get(3)? }))?;
        Ok(rows.collect::<std::result::Result<_, _>>()?)
    }

    pub fn last_setpoints(&self) -> Result<Vec<SetpointRow>> {
        let conn = self.conn.lock().unwrap();
        let mut stmt = conn.prepare(
            "SELECT node, room, value, reason, epoch_ms(ts) FROM setpoints QUALIFY row_number() OVER (PARTITION BY node ORDER BY ts DESC) = 1 ORDER BY node",
        )?;
        let rows = stmt.query_map([], |r| Ok(SetpointRow { node: r.get(0)?, room: r.get(1)?, value: r.get(2)?, reason: r.get(3)?, ts: r.get(4)? }))?;
        Ok(rows.collect::<std::result::Result<_, _>>()?)
    }

    pub fn last_predictions(&self) -> Result<Vec<PredictionRow>> {
        let conn = self.conn.lock().unwrap();
        let mut stmt = conn.prepare(
            "SELECT device, room, probability, epoch_ms(ts) FROM predictions QUALIFY row_number() OVER (PARTITION BY device ORDER BY ts DESC) = 1 ORDER BY device",
        )?;
        let rows = stmt.query_map([], |r| Ok(PredictionRow { device: r.get(0)?, room: r.get(1)?, probability: r.get(2)?, ts: r.get(3)? }))?;
        Ok(rows.collect::<std::result::Result<_, _>>()?)
    }

    pub fn latest_presence(&self) -> Result<Option<(String, i64)>> {
        Ok(self.last_predictions()?.into_iter().max_by_key(|p| p.ts).map(|p| (p.room, p.ts)))
    }

    #[cfg(test)]
    pub fn count(&self, table: &str) -> Result<i64> {
        Ok(self.conn.lock().unwrap().query_row(&format!("SELECT count(*) FROM {table}"), [], |r| r.get(0))?)
    }

    pub fn snapshot(&self, path: &str) -> Result<()> {
        if let Some(parent) = Path::new(path).parent().filter(|p| !p.as_os_str().is_empty()) {
            std::fs::create_dir_all(parent)?;
        }
        for suffix in ["", ".wal"] {
            let _ = std::fs::remove_file(format!("{path}{suffix}"));
        }
        let conn = self.conn.lock().unwrap();
        let source: String = conn.query_row("SELECT current_database()", [], |r| r.get(0))?;
        conn.execute_batch(&format!("ATTACH '{}' AS snapshot_target", path.replace('\'', "''")))?;
        let copied = conn.execute_batch(&format!("COPY FROM DATABASE \"{source}\" TO snapshot_target"));
        conn.execute_batch("DETACH snapshot_target")?;
        Ok(copied?)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    pub fn sample_scan(label: Option<&str>) -> ScanBody {
        ScanBody {
            ts: 1_761_580_800_000,
            device: "pixel-watch".into(),
            label: label.map(String::from),
            aps: vec![
                AccessPoint { bssid: "AA:BB:CC:00:00:01".into(), rssi: -45, freq: 2412 },
                AccessPoint { bssid: "aa:bb:cc:00:00:02".into(), rssi: -67, freq: 5180 },
            ],
        }
    }

    #[test]
    fn stores_and_snapshots() {
        let dir = tempfile::tempdir().unwrap();
        let db = dir.path().join("live.duckdb");
        let storage = Storage::open(db.to_str().unwrap()).unwrap();
        assert_eq!(storage.insert_scan(&sample_scan(Some("bad")), 1_761_580_801_000).unwrap(), 2);
        assert_eq!(storage.insert_scan(&sample_scan(None), 1_761_580_802_000).unwrap(), 2);
        storage.insert_reading(1_000, 5, 1, "local_temperature", 21.5).unwrap();
        storage.insert_reading(2_000, 5, 1, "local_temperature", 21.7).unwrap();
        storage.insert_setpoint(3_000, 5, "wohnzimmer", 21.0, "schedule").unwrap();
        storage.insert_prediction(4_000, "pixel-watch", "bad", 0.8).unwrap();
        storage.insert_prediction(5_000, "pixel-watch", "wohnzimmer", 0.9).unwrap();
        assert_eq!(storage.count("scans").unwrap(), 4);
        let last = storage.last_readings().unwrap();
        assert_eq!(last, vec![Reading { node: 5, attribute: "local_temperature".into(), value: 21.7, ts: 2_000 }]);
        assert_eq!(storage.last_setpoints().unwrap()[0].reason, "schedule");
        assert_eq!(storage.latest_presence().unwrap(), Some(("wohnzimmer".into(), 5_000)));
        let snap = dir.path().join("snap").join("snapshot.duckdb");
        storage.snapshot(snap.to_str().unwrap()).unwrap();
        storage.snapshot(snap.to_str().unwrap()).unwrap();
        let copy = Connection::open(&snap).unwrap();
        let (n, bssid): (i64, String) = copy.query_row("SELECT count(*), min(bssid) FROM scans", [], |r| Ok((r.get(0)?, r.get(1)?))).unwrap();
        assert_eq!((n, bssid.as_str()), (4, "aa:bb:cc:00:00:01"));
        let ids: i64 = copy.query_row("SELECT count(DISTINCT scan_id) FROM scans", [], |r| r.get(0)).unwrap();
        assert_eq!(ids, 2);
    }
}
