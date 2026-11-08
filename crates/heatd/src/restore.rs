use crate::context::{read_raw, ts_column};
use crate::http::AppState;
use crate::lake::{self, kind, Table};
use crate::rules::current_presence;
use crate::watchmatch::{Signal, MATCHER_ROOM, SIGNAL_KEEP_MS};
use anyhow::Result;
use arrow::array::{Array, BooleanArray, Float64Array, StringArray};
use arrow::compute::cast;
use arrow::datatypes::DataType;
use serde_json::Value;
use std::collections::{BTreeMap, HashSet};
use std::path::Path;

const MIN_MS: i64 = 60_000;
const LABEL_LOOKBACK_MS: i64 = 24 * 3_600_000;

struct Rec {
    ts: i64,
    to: Option<i64>,
    text: Vec<Option<String>>,
    num: Vec<Option<f64>>,
    flag: Vec<Option<bool>>,
}

fn records(root: &Path, table: Table, from_ms: i64, now_ms: i64, text: &[&str], num: &[&str], flag: &[&str]) -> Result<Vec<Rec>> {
    let mut dates = vec![lake::date_of_us(from_ms * 1000), lake::date_of_us(now_ms * 1000)];
    dates.dedup();
    let mut out = Vec::new();
    for batch in read_raw(root, table, Some(&dates))? {
        let ts = ts_column(&batch, "ts")?;
        let to = ts_column(&batch, "to_ts").ok();
        let pick = |name: &str, to: DataType| batch.column_by_name(name).and_then(|c| cast(c, &to).ok());
        let texts: Vec<Option<StringArray>> = text.iter().map(|n| pick(n, DataType::Utf8).and_then(|c| c.as_any().downcast_ref::<StringArray>().cloned())).collect();
        let nums: Vec<Option<Float64Array>> = num.iter().map(|n| pick(n, DataType::Float64).and_then(|c| c.as_any().downcast_ref::<Float64Array>().cloned())).collect();
        let flags: Vec<Option<BooleanArray>> = flag.iter().map(|n| pick(n, DataType::Boolean).and_then(|c| c.as_any().downcast_ref::<BooleanArray>().cloned())).collect();
        for i in (0..batch.num_rows()).filter(|&i| !ts.is_null(i)) {
            let ms = ts.value(i) / 1000;
            if ms < from_ms || ms > now_ms {
                continue;
            }
            out.push(Rec {
                ts: ms,
                to: to.as_ref().filter(|t| !t.is_null(i)).map(|t| t.value(i) / 1000),
                text: texts.iter().map(|c| c.as_ref().filter(|c| !c.is_null(i)).map(|c| c.value(i).to_string())).collect(),
                num: nums.iter().map(|c| c.as_ref().filter(|c| !c.is_null(i)).map(|c| c.value(i))).collect(),
                flag: flags.iter().map(|c| c.as_ref().filter(|c| !c.is_null(i)).map(|c| c.value(i))).collect(),
            });
        }
    }
    out.sort_by_key(|r| r.ts);
    Ok(out)
}

enum Stay {
    Label { room: String, id: Option<String> },
    Scan,
}

fn stays(root: &Path, from_ms: i64, now_ms: i64) -> Result<Vec<(i64, String, Stay)>> {
    let mut out = Vec::new();
    let mut voided = HashSet::new();
    let rows = records(root, Table::Labels, now_ms - LABEL_LOOKBACK_MS.max(now_ms - from_ms), now_ms, &["source", "device", "room", "label_id", "voided_label_id"], &[], &["heat"])?;
    voided.extend(rows.iter().filter_map(|r| r.text[1].clone().zip(r.text[4].clone())));
    for r in rows {
        let [source, Some(device), room, id, None] = &r.text[..] else { continue };
        let at = r.to.unwrap_or(r.ts).min(r.ts);
        if source.as_deref() == Some("watch") && r.flag[0] != Some(false) && at >= from_ms && !id.as_ref().is_some_and(|i| voided.contains(&(device.clone(), i.clone()))) {
            out.push((at, device.clone(), Stay::Label { room: room.clone().unwrap_or_default(), id: id.clone() }));
        }
    }
    for r in records(root, Table::Events, from_ms, now_ms, &["kind", "detail"], &[], &[])? {
        let [Some(k), Some(detail)] = &r.text[..] else { continue };
        let Ok(d) = serde_json::from_str::<Value>(detail) else { continue };
        if k == kind::WATCH_STATE && d["n_aps"].as_u64().unwrap_or(0) > 0 && d["heat"].as_bool().unwrap_or(true) {
            if let Some(device) = d["device"].as_str() {
                out.push((r.ts, device.to_string(), Stay::Scan));
            }
        }
    }
    out.sort_by_key(|(ts, _, s)| (*ts, !matches!(s, Stay::Label { .. })));
    Ok(out)
}

pub fn restore_windows(state: &AppState, now_ms: i64) -> Result<usize> {
    let from_ms = now_ms - (state.config.boost.after_window_min * MIN_MS as f64) as i64;
    let mut found = 0;
    for r in records(state.sink.root(), Table::Events, from_ms, now_ms, &["kind", "detail"], &[], &[])? {
        let [Some(k), Some(detail)] = &r.text[..] else { continue };
        if k != kind::WINDOW_OPEN && k != kind::WINDOW_CLOSE {
            continue;
        }
        if let Some(room) = serde_json::from_str::<Value>(detail).ok().and_then(|d| d["room"].as_str().map(String::from)) {
            state.windows.lock().unwrap().note_open(&room, r.ts);
            found += 1;
        }
    }
    Ok(found)
}

pub fn restore(state: &AppState, now_ms: i64) -> Result<String> {
    let cfg = &state.config;
    if !cfg.presence.restore {
        return Ok("off".into());
    }
    let root = state.sink.root();
    let timeout_ms = (cfg.presence_timeout_minutes * MIN_MS as f64) as i64;
    let from_ms = now_ms - timeout_ms;
    let history = state.storage.state_history(now_ms - (cfg.presence.tracker_lookback_h * 3_600_000.0) as i64)?;
    {
        let mut tracker = state.tracker.lock().unwrap();
        for h in &history {
            let _ = tracker.observe(&h.device, h.ts, h.away, h.motion, h.power);
        }
        drop(tracker.poll(now_ms, cfg.away_guard_minutes));
    }
    let statuses = state.storage.device_statuses()?;
    let watch = statuses.iter().filter(|s| !state.watchmatch.is_anchor(&s.device) && now_ms - s.state_ts <= timeout_ms).max_by_key(|s| s.state_ts);
    for s in statuses.iter().filter(|s| now_ms - s.state_ts <= timeout_ms) {
        if let Some(power) = s.power {
            state.watchmatch.signals(&s.device, [(s.state_ts, Signal::OnBody(power != crate::storage::Power::Offbody))]);
        }
    }
    let signal_from = now_ms - timeout_ms.max(SIGNAL_KEEP_MS);
    let mut by_device: BTreeMap<String, Vec<(i64, Signal)>> = BTreeMap::new();
    for r in records(root, Table::WatchRaw, signal_from, now_ms, &["device", "sensor_type"], &["value"], &[])? {
        let [Some(device), Some(sensor)] = &r.text[..] else { continue };
        let Some(v) = r.num[0] else { continue };
        let signal = match sensor.as_str() {
            "hr" => Signal::Hr(v),
            "motion" => Signal::Motion(v),
            "offbody" => Signal::OnBody(v >= 0.5),
            "charging" => Signal::Charging(v >= 0.5),
            _ => continue,
        };
        by_device.entry(device.clone()).or_default().push((r.ts, signal));
    }
    by_device.into_iter().for_each(|(device, signals)| state.watchmatch.signals(&device, signals));
    for (ts, device, s) in stays(root, from_ms, now_ms)? {
        match s {
            Stay::Label { room, id } => state.watchmatch.note_stay(&device, ts, Some(&room), id.as_deref()),
            Stay::Scan => state.watchmatch.note_stay(&device, ts, None, None),
        }
    }
    let rooms: Vec<(i64, Option<String>, f64)> = records(root, Table::WatchInference, from_ms, now_ms, &["matcher", "result_room"], &["confidence", "abstained"], &[])?
        .into_iter()
        .filter(|r| r.text[0].as_deref() == Some(MATCHER_ROOM) && r.num[1] != Some(1.0))
        .map(|r| (r.ts, r.text[1].clone(), r.num[0].unwrap_or(0.0)))
        .collect();
    let mut held = None;
    if let (Some(device), Some((ts, Some(room), _))) = (watch, rooms.iter().rev().find(|r| r.1.is_some())) {
        state.watchmatch.restore_hold(&device.device, room, *ts);
        let since = rooms.iter().rev().take_while(|r| r.1.is_none() || r.1.as_deref() == Some(room.as_str())).filter(|r| r.1.is_some()).last().map_or(*ts, |r| r.0);
        held = Some((room.clone(), since));
    }
    let presence = current_presence(state, now_ms)?;
    let detected = presence.as_ref().and_then(|p| p.detected(cfg));
    if let (Some(room), Some((held_room, since))) = (&detected, &held) {
        if room == held_room {
            state.commanded.lock().unwrap().restore_occupancy(room, *since, now_ms, (cfg.occupancy_enter_min * MIN_MS as f64) as i64);
        }
    }
    Ok(format!("room {}, away {}, sleep {}", detected.as_deref().unwrap_or("-"), presence.as_ref().is_some_and(|p| p.away), presence.as_ref().is_some_and(|p| p.sleep)))
}
