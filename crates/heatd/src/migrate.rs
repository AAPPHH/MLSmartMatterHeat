use crate::config::Config;
use crate::context::{read_raw, text_column, ts_column};
use crate::lake::{self, date_of_us, AttributeReport, Decision, Event, Label, Sink, Table, WatchRaw};
use anyhow::{bail, Result};
use duckdb::{AccessMode, Connection};
use std::path::Path;

const MARKER: &str = "raw/.migrated_duckdb";
const LEGACY: &str = "legacy";
const UNKNOWN_ATTRIBUTE: u32 = u32::MAX;
const UNKNOWN_DEVICE: &str = "unknown";

enum Staged {
    Raw(WatchRaw),
    Label(Label),
    Report(AttributeReport),
    Decision(Decision),
    Event(Event),
}

struct Dataset {
    name: &'static str,
    rows: Vec<(i64, Staged)>,
}

fn attribute_ids(name: &str) -> (u32, u32) {
    use matter_controller as m;
    let groups: [(u32, &[m::AttributeSpec]); 2] = [
        (0x0201, &[m::LOCAL_TEMPERATURE, m::OCCUPIED_HEATING_SETPOINT, m::SYSTEM_MODE, m::PI_HEATING_DEMAND, m::LOCAL_TEMPERATURE_CALIBRATION, m::THERMOSTAT_RUNNING_STATE]),
        (0x002F, &[m::BAT_VOLTAGE, m::BAT_PERCENT_REMAINING, m::BAT_CHARGE_LEVEL]),
    ];
    groups.iter().find_map(|(cluster, specs)| specs.iter().find(|s| s.snake == name).map(|s| (*cluster, s.id))).unwrap_or((0, UNKNOWN_ATTRIBUTE))
}

fn channel_of(freq: i64) -> Option<u8> {
    let ch = match freq {
        2412..=2472 => (freq - 2407) / 5,
        2484 => 14,
        5160..=5885 => (freq - 5000) / 5,
        5955..=7115 => (freq - 5950) / 5,
        _ => return None,
    };
    u8::try_from(ch).ok()
}

pub fn cutoff_us(root: &Path) -> Result<i64> {
    let mut first_start: Option<i64> = None;
    for batch in read_raw(root, Table::Events, None)? {
        let (kinds, sources, ts) = (text_column(&batch, "kind")?, text_column(&batch, "source")?, ts_column(&batch, "ts")?);
        for i in (0..batch.num_rows()).filter(|&i| kinds.value(i) == lake::kind::STARTUP && sources.value(i) == "heatd") {
            first_start = Some(first_start.map_or(ts.value(i), |t| t.min(ts.value(i))));
        }
    }
    if let Some(ts) = first_start {
        return Ok(ts);
    }
    let mut oldest: Option<i64> = None;
    for batch in read_raw(root, Table::WatchRaw, None)? {
        let ts = ts_column(&batch, "ts")?;
        oldest = ts.iter().flatten().chain(oldest).min();
    }
    Ok(oldest.unwrap_or(i64::MAX))
}

pub fn run(config: &Config) -> Result<()> {
    let marker = Path::new(&config.data_dir).join(MARKER);
    if marker.exists() {
        bail!("already migrated ({})", marker.display());
    }
    let sink = Sink::open(lake::LakeConfig::new(&config.data_dir))?;
    let cutoff = cutoff_us(sink.root())?;
    let conn = Connection::open_with_flags(&config.db_path, duckdb::Config::default().access_mode(AccessMode::ReadOnly)?)?;
    let datasets = vec![
        scans(&conn, cutoff)?,
        scan_labels(&conn, cutoff)?,
        predictions(&conn, cutoff)?,
        readings(&conn, cutoff)?,
        setpoints(&conn, cutoff)?,
        device_state(&conn, cutoff)?,
    ];
    let mut counts = Vec::new();
    for Dataset { name, rows } in datasets {
        counts.push((name, rows.len()));
        for (ts, row) in rows {
            let date = date_of_us(ts);
            match row {
                Staged::Raw(r) => sink.append_on(date, r)?,
                Staged::Label(r) => sink.append_on(date, r)?,
                Staged::Report(r) => sink.append_on(date, r)?,
                Staged::Decision(r) => sink.append_on(date, r)?,
                Staged::Event(r) => sink.append_on(date, r)?,
            }
        }
    }
    sink.close()?;
    std::fs::write(&marker, serde_json::json!({"counts": counts, "cutoff_us": cutoff, "heatd_commit": lake::COMMIT}).to_string())?;
    println!("cutoff: legacy rows before ts {cutoff} us");
    for (table, n) in counts {
        println!("{table}: {n}");
    }
    Ok(())
}

fn rows<T>(conn: &Connection, sql: &str, cutoff: i64, map: impl Fn(&duckdb::Row) -> duckdb::Result<T>) -> Result<Vec<T>> {
    let mut stmt = conn.prepare(sql)?;
    let out = stmt.query_map([cutoff], |r| map(r))?.collect::<std::result::Result<Vec<_>, _>>()?;
    Ok(out)
}

fn device_or_unknown(device: Option<String>) -> String {
    device.unwrap_or_else(|| UNKNOWN_DEVICE.into())
}

fn scans(conn: &Connection, cutoff: i64) -> Result<Dataset> {
    let data = rows(conn, "SELECT epoch_us(ts), epoch_us(received_at), device, bssid, rssi, freq, scan_id FROM scans WHERE epoch_us(ts) < ? ORDER BY ts", cutoff, |r| {
        Ok((r.get::<_, i64>(0)?, r.get::<_, Option<i64>>(1)?, r.get::<_, Option<String>>(2)?, r.get::<_, String>(3)?, r.get::<_, Option<i64>>(4)?, r.get::<_, Option<i64>>(5)?, r.get::<_, Option<i64>>(6)?))
    })?;
    let rows = data
        .into_iter()
        .map(|(ts, received, device, bssid, rssi, freq, scan_id)| {
            let row = WatchRaw {
                ts,
                device_ts: None,
                sender: bssid,
                rssi: rssi.unwrap_or(-128).clamp(-128, 127) as i8,
                channel: freq.and_then(channel_of),
                batch_id: scan_id.unwrap_or(0) as u64,
                device: device_or_unknown(device),
                freq: freq.and_then(|f| u16::try_from(f).ok()),
                received_ts: received,
            };
            (ts, Staged::Raw(row))
        })
        .collect();
    Ok(Dataset { name: "watch_raw", rows })
}

fn scan_labels(conn: &Connection, cutoff: i64) -> Result<Dataset> {
    let data = rows(conn, "SELECT DISTINCT scan_id, epoch_us(ts), device, label FROM scans WHERE label IS NOT NULL AND epoch_us(ts) < ? ORDER BY 2", cutoff, |r| {
        Ok((r.get::<_, i64>(1)?, r.get::<_, Option<String>>(2)?, r.get::<_, String>(3)?))
    })?;
    let rows = data.into_iter().map(|(ts, device, room)| (ts, Staged::Label(Label { ts, room, source: "watch".into(), device: Some(device_or_unknown(device)), ..Default::default() }))).collect();
    Ok(Dataset { name: "labels(watch)", rows })
}

fn predictions(conn: &Connection, cutoff: i64) -> Result<Dataset> {
    let data = rows(conn, "SELECT epoch_us(ts), device, room, probability FROM predictions WHERE epoch_us(ts) < ? ORDER BY ts", cutoff, |r| {
        Ok((r.get::<_, i64>(0)?, r.get::<_, Option<String>>(1)?, r.get::<_, String>(2)?, r.get::<_, Option<f64>>(3)?))
    })?;
    let rows = data
        .into_iter()
        .map(|(ts, device, room, p)| {
            let label = Label { ts, room, source: "model".into(), device: Some(device_or_unknown(device)), confidence: p.map(|p| p as f32), model_id: Some(LEGACY.into()), ..Default::default() };
            (ts, Staged::Label(label))
        })
        .collect();
    Ok(Dataset { name: "labels(model)", rows })
}

fn readings(conn: &Connection, cutoff: i64) -> Result<Dataset> {
    let data = rows(conn, "SELECT epoch_us(ts), node, endpoint, attribute, value FROM readings WHERE epoch_us(ts) < ? ORDER BY ts", cutoff, |r| {
        Ok((r.get::<_, i64>(0)?, r.get::<_, u64>(1)?, r.get::<_, u16>(2)?, r.get::<_, String>(3)?, r.get::<_, Option<f64>>(4)?))
    })?;
    let rows = data
        .into_iter()
        .map(|(ts, node, endpoint, name, value)| {
            let (cluster, attribute) = attribute_ids(&name);
            let row = AttributeReport {
                ts,
                node,
                endpoint,
                cluster,
                attribute,
                dtype: "float".into(),
                v_f64: value,
                v_str: Some(name),
                source: LEGACY.into(),
                fw: LEGACY.into(),
                ..Default::default()
            };
            (ts, Staged::Report(row))
        })
        .collect();
    Ok(Dataset { name: "attribute_reports", rows })
}

fn setpoints(conn: &Connection, cutoff: i64) -> Result<Dataset> {
    let data = rows(conn, "SELECT epoch_us(ts), node, room, value, reason FROM setpoints WHERE epoch_us(ts) < ? ORDER BY ts", cutoff, |r| {
        Ok((r.get::<_, i64>(0)?, r.get::<_, u64>(1)?, r.get::<_, Option<String>>(2)?, r.get::<_, Option<f64>>(3)?, r.get::<_, Option<String>>(4)?))
    })?;
    let rows = data
        .into_iter()
        .map(|(ts, node, room, value, reason)| {
            let mut d = Decision { ts, room: room.unwrap_or_default(), node, rule: LEGACY.into(), setpoint_new: value, proposed: true, applied: true, commit: LEGACY.into(), ..Default::default() };
            d.inputs.extra = Some(serde_json::json!({"reason": reason}).to_string());
            (ts, Staged::Decision(d))
        })
        .collect();
    Ok(Dataset { name: "decisions", rows })
}

fn device_state(conn: &Connection, cutoff: i64) -> Result<Dataset> {
    let data = rows(conn, "SELECT epoch_us(ts), epoch_us(received_at), device, motion, power, queued, n_aps, away, id FROM device_state WHERE epoch_us(ts) < ? ORDER BY ts", cutoff, |r| {
        Ok((
            r.get::<_, i64>(0)?,
            serde_json::json!({
                "device": device_or_unknown(r.get::<_, Option<String>>(2)?),
                "received_ts": r.get::<_, Option<i64>>(1)?,
                "motion": r.get::<_, Option<String>>(3)?,
                "power": r.get::<_, Option<String>>(4)?,
                "queued": r.get::<_, Option<bool>>(5)?,
                "n_aps": r.get::<_, Option<i64>>(6)?,
                "away": r.get::<_, Option<bool>>(7)?,
                "id": r.get::<_, Option<String>>(8)?,
            }),
        ))
    })?;
    let rows = data
        .into_iter()
        .map(|(ts, detail)| {
            let mut e = Event::now(lake::kind::WATCH_STATE, LEGACY).with_detail(detail);
            e.ts = ts;
            e.mono_ns = 0;
            (ts, Staged::Event(e))
        })
        .collect();
    Ok(Dataset { name: "events(watch_state)", rows })
}
