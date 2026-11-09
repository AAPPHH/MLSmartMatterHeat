use crate::config::Room;
use crate::http::AppState;
use crate::intent::Reply;
use crate::lake::{self, kind};
use crate::report::{hhmm, midnight_us, open_lake, rows};
use crate::rules::HoldSource;
use axum::http::StatusCode;
use chrono::NaiveDate;
use serde_json::{json, Value};
use std::fmt::Write;
use std::path::Path;

pub const MAX_CELSIUS: f64 = 26.0;
pub const MIN_HOLD_MIN: f64 = 15.0;
pub const MAX_HOLD_MIN: f64 = 720.0;
pub const DEFAULT_HOLD_MIN: f64 = 120.0;
const MIN_MS: f64 = 60_000.0;

pub fn celsius_text(value: f64) -> String {
    if value.fract() == 0.0 { format!("{value:.0}") } else { format!("{value:.1}").replace('.', ",") }
}

fn fail(status: StatusCode, error: &str) -> Reply {
    Reply { status, body: json!({"ok": false, "error": error}) }
}

fn target<'a>(state: &'a AppState, room: &str, source: Option<&str>) -> Result<(&'a Room, HoldSource), Reply> {
    let room = state.config.rooms.iter().find(|r| r.name == room && !r.nodes.is_empty()).ok_or_else(|| fail(StatusCode::NOT_FOUND, "Raum unbekannt oder ohne Thermostat"))?;
    let source = match source {
        None => HoldSource::Api,
        Some(name) => HoldSource::parse(name).ok_or_else(|| fail(StatusCode::BAD_REQUEST, "source muss api oder watch sein"))?,
    };
    Ok((room, source))
}

fn valid(state: &AppState, celsius: Option<f64>, hold_min: Option<f64>) -> Result<(f64, f64), Reply> {
    let low = state.config.frost_setpoint;
    let hold = hold_min.unwrap_or(DEFAULT_HOLD_MIN);
    let celsius = celsius.filter(|c| c.is_finite() && (low..=MAX_CELSIUS).contains(c) && (c * 2.0).fract() == 0.0);
    let celsius = celsius.ok_or_else(|| fail(StatusCode::BAD_REQUEST, &format!("celsius muss zwischen {} und {} in 0,5er Schritten liegen", celsius_text(low), celsius_text(MAX_CELSIUS))))?;
    if !(MIN_HOLD_MIN..=MAX_HOLD_MIN).contains(&hold) {
        return Err(fail(StatusCode::BAD_REQUEST, &format!("hold_min muss zwischen {MIN_HOLD_MIN} und {MAX_HOLD_MIN} liegen")));
    }
    Ok((celsius, hold))
}

pub fn record(state: &AppState, room: &Room, detail: Value) {
    let mut event = lake::Event::now(kind::MANUAL_SETPOINT, "heatd").with_detail(detail);
    event.node = room.nodes.first().copied();
    if let Err(e) = state.sink.append(event) {
        eprintln!("manual setpoint event: {e:#}");
    }
}

pub fn set(state: &AppState, room: &str, celsius: Option<f64>, hold_min: Option<f64>, source: Option<&str>, now_ms: i64) -> Reply {
    let (room, source) = match target(state, room, source) {
        Ok(found) => found,
        Err(reply) => return reply,
    };
    let (celsius, hold) = match valid(state, celsius, hold_min) {
        Ok(found) => found,
        Err(reply) => return reply,
    };
    if state.failsafe.stopping() {
        return fail(StatusCode::SERVICE_UNAVAILABLE, "heatd faehrt herunter");
    }
    let until = now_ms + (hold * MIN_MS) as i64;
    state.commanded.lock().unwrap().set_manual(&room.nodes, celsius, now_ms, until, source);
    record(state, room, json!({"room": room.name, "target": celsius, "hold_min": hold, "source": source.name(), "until_ts": until}));
    Reply { status: StatusCode::OK, body: json!({"ok": true, "room": room.name, "target": celsius, "hold_min": hold, "until_ts": until, "source": source.name()}) }
}

pub fn clear(state: &AppState, room: &str, source: Option<&str>, now_ms: i64) -> Reply {
    let (room, source) = match target(state, room, source) {
        Ok(found) => found,
        Err(reply) => return reply,
    };
    let cleared = state.commanded.lock().unwrap().clear_holds(&room.nodes);
    if cleared {
        record(state, room, json!({"room": room.name, "cleared": true, "source": source.name(), "until_ts": now_ms}));
    }
    Reply { status: StatusCode::OK, body: json!({"ok": true, "room": room.name, "cleared": cleared}) }
}

pub fn view(state: &AppState, now_ms: i64) -> Value {
    let commands = state.commanded.lock().unwrap();
    let holds: Vec<Value> = state
        .config
        .rooms
        .iter()
        .filter_map(|room| {
            let hold = room.nodes.iter().find_map(|n| commands.hold(*n))?;
            Some(json!({"room": room.name, "target": hold.value, "until_ts": hold.until_ms, "minutes_left": ((hold.until_ms - now_ms) as f64 / MIN_MS).max(0.0), "source": hold.source.name()}))
        })
        .collect();
    json!({"holds": holds})
}

pub fn report_section(root: &Path, date: NaiveDate) -> anyhow::Result<String> {
    let db = open_lake(root, date - chrono::Duration::days(1), date + chrono::Duration::days(1))?;
    let (lo, hi) = (midnight_us(date), midnight_us(date) + 86_400_000_000);
    let events = rows(
        &db.conn,
        &format!("SELECT epoch_us(ts), json_extract_string(detail, '$.room'), CAST(json_extract(detail, '$.target') AS DOUBLE), CAST(json_extract(detail, '$.hold_min') AS DOUBLE), json_extract_string(detail, '$.source'), CAST(json_extract(detail, '$.cleared') AS BOOLEAN) FROM events WHERE kind = '{}' AND epoch_us(ts) >= {lo} AND epoch_us(ts) < {hi} ORDER BY ts", kind::MANUAL_SETPOINT),
        |r| Ok((r.get::<_, i64>(0)?, r.get::<_, Option<String>>(1)?, r.get::<_, Option<f64>>(2)?, r.get::<_, Option<f64>>(3)?, r.get::<_, Option<String>>(4)?, r.get::<_, Option<bool>>(5)?)),
    )?;
    let mut md = String::from("\n## Manuelle Vorgaben\n\n");
    if events.is_empty() {
        md.push_str("Keine Raumtemperatur von Hand vorgegeben.\n");
    }
    for (us, room, target, hold, source, cleared) in events {
        let (room, source) = (room.unwrap_or_else(|| "-".into()), source.unwrap_or_else(|| "-".into()));
        if cleared == Some(true) {
            let _ = writeln!(md, "- {} {room}: Vorgabe vorzeitig beendet ({source})", hhmm(us));
        } else {
            let _ = writeln!(md, "- {} {room}: {} °C fuer {} min vorgegeben ({source})", hhmm(us), celsius_text(target.unwrap_or(0.0)), hold.unwrap_or(0.0));
        }
    }
    Ok(md)
}
