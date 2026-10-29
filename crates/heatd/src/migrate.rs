use crate::config::Config;
use crate::lake::{self, date_of_us, AttributeReport, Decision, Event, Label, Sink, WatchRaw};
use anyhow::{bail, Result};
use duckdb::{AccessMode, Connection};
use std::path::Path;

const MARKER: &str = "raw/.migrated_duckdb";
const LEGACY: &str = "legacy";
const UNKNOWN_ATTRIBUTE: u32 = u32::MAX;

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

pub fn run(config: &Config) -> Result<()> {
    let marker = Path::new(&config.data_dir).join(MARKER);
    if marker.exists() {
        bail!("already migrated ({})", marker.display());
    }
    let conn = Connection::open_with_flags(&config.db_path, duckdb::Config::default().access_mode(AccessMode::ReadOnly)?)?;
    let sink = Sink::open(lake::LakeConfig::new(&config.data_dir))?;
    let counts = [
        ("watch_raw", scans(&conn, &sink)?),
        ("labels(watch)", scan_labels(&conn, &sink)?),
        ("labels(model)", predictions(&conn, &sink)?),
        ("attribute_reports", readings(&conn, &sink)?),
        ("decisions", setpoints(&conn, &sink)?),
        ("events(watch_state)", device_state(&conn, &sink)?),
    ];
    sink.close()?;
    std::fs::write(&marker, serde_json::json!({"counts": counts, "heatd_commit": lake::COMMIT}).to_string())?;
    for (table, n) in counts {
        println!("{table}: {n}");
    }
    Ok(())
}

fn rows<T>(conn: &Connection, sql: &str, map: impl Fn(&duckdb::Row) -> duckdb::Result<T>) -> Result<Vec<T>> {
    let mut stmt = conn.prepare(sql)?;
    let out = stmt.query_map([], |r| map(r))?.collect::<std::result::Result<Vec<_>, _>>()?;
    Ok(out)
}

fn scans(conn: &Connection, sink: &Sink) -> Result<usize> {
    let data = rows(conn, "SELECT epoch_us(ts), epoch_us(received_at), device, bssid, rssi, freq, scan_id FROM scans ORDER BY ts", |r| {
        Ok((r.get::<_, i64>(0)?, r.get::<_, i64>(1)?, r.get::<_, String>(2)?, r.get::<_, String>(3)?, r.get::<_, i64>(4)?, r.get::<_, i64>(5)?, r.get::<_, i64>(6)?))
    })?;
    for (ts, received, device, bssid, rssi, freq, scan_id) in &data {
        let row = WatchRaw {
            ts: *ts,
            device_ts: None,
            sender: bssid.clone(),
            rssi: (*rssi).clamp(-128, 127) as i8,
            channel: channel_of(*freq),
            batch_id: *scan_id as u64,
            device: device.clone(),
            freq: u16::try_from(*freq).ok(),
            received_ts: Some(*received),
        };
        sink.append_on(date_of_us(*ts), row)?;
    }
    Ok(data.len())
}

fn scan_labels(conn: &Connection, sink: &Sink) -> Result<usize> {
    let data = rows(conn, "SELECT DISTINCT scan_id, epoch_us(ts), device, label FROM scans WHERE label IS NOT NULL ORDER BY 2", |r| {
        Ok((r.get::<_, i64>(1)?, r.get::<_, String>(2)?, r.get::<_, String>(3)?))
    })?;
    for (ts, device, room) in &data {
        sink.append_on(date_of_us(*ts), Label { ts: *ts, room: room.clone(), source: "watch".into(), device: Some(device.clone()), ..Default::default() })?;
    }
    Ok(data.len())
}

fn predictions(conn: &Connection, sink: &Sink) -> Result<usize> {
    let data = rows(conn, "SELECT epoch_us(ts), device, room, probability FROM predictions ORDER BY ts", |r| {
        Ok((r.get::<_, i64>(0)?, r.get::<_, String>(1)?, r.get::<_, String>(2)?, r.get::<_, f64>(3)?))
    })?;
    for (ts, device, room, p) in &data {
        let label = Label { ts: *ts, room: room.clone(), source: "model".into(), device: Some(device.clone()), confidence: Some(*p as f32), model_id: Some(LEGACY.into()), ..Default::default() };
        sink.append_on(date_of_us(*ts), label)?;
    }
    Ok(data.len())
}

fn readings(conn: &Connection, sink: &Sink) -> Result<usize> {
    let data = rows(conn, "SELECT epoch_us(ts), node, endpoint, attribute, value FROM readings ORDER BY ts", |r| {
        Ok((r.get::<_, i64>(0)?, r.get::<_, u64>(1)?, r.get::<_, u16>(2)?, r.get::<_, String>(3)?, r.get::<_, f64>(4)?))
    })?;
    for (ts, node, endpoint, name, value) in &data {
        let (cluster, attribute) = attribute_ids(name);
        let row = AttributeReport {
            ts: *ts,
            node: *node,
            endpoint: *endpoint,
            cluster,
            attribute,
            dtype: "float".into(),
            v_f64: Some(*value),
            v_str: Some(name.clone()),
            source: LEGACY.into(),
            fw: LEGACY.into(),
            ..Default::default()
        };
        sink.append_on(date_of_us(*ts), row)?;
    }
    Ok(data.len())
}

fn setpoints(conn: &Connection, sink: &Sink) -> Result<usize> {
    let data = rows(conn, "SELECT epoch_us(ts), node, room, value, reason FROM setpoints ORDER BY ts", |r| {
        Ok((r.get::<_, i64>(0)?, r.get::<_, u64>(1)?, r.get::<_, String>(2)?, r.get::<_, f64>(3)?, r.get::<_, String>(4)?))
    })?;
    for (ts, node, room, value, reason) in &data {
        let mut d = Decision { ts: *ts, room: room.clone(), node: *node, rule: LEGACY.into(), setpoint_new: Some(*value), proposed: true, applied: true, commit: LEGACY.into(), ..Default::default() };
        d.inputs.extra = Some(serde_json::json!({"reason": reason}).to_string());
        sink.append_on(date_of_us(*ts), d)?;
    }
    Ok(data.len())
}

fn device_state(conn: &Connection, sink: &Sink) -> Result<usize> {
    let data = rows(conn, "SELECT epoch_us(ts), epoch_us(received_at), device, motion, power, queued, n_aps, away, id FROM device_state ORDER BY ts", |r| {
        Ok((
            r.get::<_, i64>(0)?,
            serde_json::json!({
                "device": r.get::<_, String>(2)?,
                "received_ts": r.get::<_, i64>(1)?,
                "motion": r.get::<_, Option<String>>(3)?,
                "power": r.get::<_, Option<String>>(4)?,
                "queued": r.get::<_, Option<bool>>(5)?,
                "n_aps": r.get::<_, Option<i64>>(6)?,
                "away": r.get::<_, Option<bool>>(7)?,
                "id": r.get::<_, Option<String>>(8)?,
            }),
        ))
    })?;
    for (ts, detail) in &data {
        let mut e = Event::now(lake::kind::WATCH_STATE, LEGACY).with_detail(detail.clone());
        e.ts = *ts;
        e.mono_ns = 0;
        sink.append_on(date_of_us(*ts), e)?;
    }
    Ok(data.len())
}
