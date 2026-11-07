use crate::config::{Config, SupplySection, WindowSection};
use crate::lake::{self, Table};
use crate::window_model::{Channel, Detector, Scored};
use arrow::array::{Array, Float64Array, Int64Array, TimestampMicrosecondArray, UInt32Array, UInt64Array};
use chrono::{Local, TimeZone, Timelike};
use serde_json::{json, Value};
use std::collections::{BTreeMap, HashMap, HashSet, VecDeque};
use std::path::Path;

const MIN_MS: i64 = 60_000;
const LATE_LABEL_MS: i64 = 300_000;
const KEEP_MS: i64 = 3 * 3_600_000;
const SUPPRESS_LOG_MS: i64 = 15 * MIN_MS;
const LEAD_MS: i64 = 15 * MIN_MS;
const THERMOSTAT_CLUSTER: u32 = 513;
pub const DETECTOR: &str = "v3";

pub const LOCAL_TEMPERATURE: &str = "local_temperature";
pub const PI_HEATING_DEMAND: &str = "pi_heating_demand";
pub const OCCUPIED_SETPOINT: &str = matter_controller::OCCUPIED_HEATING_SETPOINT.snake;

#[derive(Debug, Clone, Default)]
pub struct RoomCtx {
    pub hold: bool,
    pub away: bool,
}

#[derive(Debug, Clone)]
pub struct LabelRec {
    pub label_id: Option<String>,
    pub room: String,
    pub ts: i64,
    pub open: bool,
    pub manual: bool,
    pub question: Option<&'static str>,
}

#[derive(Debug, Clone)]
pub struct Episode {
    pub room: String,
    pub start_ms: i64,
    pub end_ms: i64,
    pub reason: &'static str,
    pub source: &'static str,
    pub score: Option<(f64, f64)>,
}

impl Episode {
    fn detail(&self, labels: &[LabelRec], grace_ms: i64) -> Value {
        let mine = |l: &&LabelRec| l.room == self.room && l.ts >= self.start_ms - LEAD_MS && l.ts <= self.end_ms + grace_ms;
        let seen = |l: &&LabelRec| if l.manual { l.open || l.ts <= self.start_ms } else { !(l.question == Some("closed") && !l.open) };
        let confirm = labels.iter().filter(mine).filter(seen).max_by_key(|l| l.ts).map_or("none", |l| if l.open { "yes" } else { "no" });
        let latency = labels.iter().filter(mine).filter(|l| l.manual && l.open && l.ts <= self.end_ms).min_by_key(|l| l.ts).map(|l| (self.start_ms - l.ts) as f64 / MIN_MS as f64);
        json!({
            "detector": DETECTOR,
            "room": self.room,
            "start_ms": self.start_ms,
            "end_ms": self.end_ms,
            "duration_min": (self.end_ms - self.start_ms) as f64 / MIN_MS as f64,
            "end_reason": self.reason,
            "source": self.source,
            "house": self.score.map(|s| s.0),
            "z": self.score.map(|s| s.1),
            "label": confirm,
            "latency_min": latency,
        })
    }
}

#[derive(Debug, Clone)]
struct Active {
    source: &'static str,
    first_ms: i64,
    since_ms: i64,
    score: Option<(f64, f64)>,
}

#[derive(Debug, Default)]
struct NodeWin {
    seen_ms: Option<i64>,
    gap_ms: Option<i64>,
    temp: Option<f64>,
    valve: Option<f64>,
    high_since: Option<i64>,
    over_since: Option<i64>,
}

#[derive(Debug, Default)]
struct RoomWin {
    active: Option<Active>,
    series: VecDeque<(i64, f64)>,
    floor_ms: i64,
    decrease_ms: Option<i64>,
    logged: HashMap<&'static str, i64>,
    last_close: Option<(&'static str, i64)>,
    auto_opens: u64,
    suppressed: u64,
    nodes_seen: usize,
    supply: Option<i64>,
    manual_label: Option<String>,
    z: Option<f64>,
}

#[derive(Debug, Clone, PartialEq)]
pub struct WinEvent {
    pub kind: &'static str,
    pub room: String,
    pub detail: Value,
}

#[derive(Debug, Clone)]
struct Prompt {
    id: String,
    room: String,
    question: &'static str,
    reason: Option<String>,
    created_ms: i64,
}

#[derive(Debug, Default)]
pub struct Windows {
    rooms: BTreeMap<String, RoomWin>,
    nodes: HashMap<u64, NodeWin>,
    episodes: Vec<(String, i64, i64)>,
    ctx: HashMap<String, RoomCtx>,
    finished: Vec<Episode>,
    labels: VecDeque<LabelRec>,
    prompt: Option<Prompt>,
    prompts: VecDeque<Prompt>,
    prompt_seq: u64,
    prompt_at: HashMap<String, i64>,
    prompt_times: VecDeque<i64>,
    det: Detector,
    last: Option<Scored>,
}

fn mins(m: f64) -> i64 {
    (m * MIN_MS as f64) as i64
}

fn raw_history(root: &Path, from_ms: i64, to_ms: i64) -> Vec<(i64, u64, &'static str, f64)> {
    let mut out = Vec::new();
    let day = |ms: i64| lake::date_of_us(ms * 1000);
    let mut date = day(from_ms);
    while date <= day(to_ms) {
        for (path, _) in lake::raw_files(root, Table::AttributeReports, Some(date)).unwrap_or_default() {
            let Ok(loaded) = lake::load_stream(&path) else { continue };
            for batch in loaded.batches {
                let (Some(ts), Some(node), Some(cluster), Some(attribute), Some(vi), Some(vf)) = (
                    batch.column_by_name("ts").and_then(|c| c.as_any().downcast_ref::<TimestampMicrosecondArray>()),
                    batch.column_by_name("node").and_then(|c| c.as_any().downcast_ref::<UInt64Array>()),
                    batch.column_by_name("cluster").and_then(|c| c.as_any().downcast_ref::<UInt32Array>()),
                    batch.column_by_name("attribute").and_then(|c| c.as_any().downcast_ref::<UInt32Array>()),
                    batch.column_by_name("v_i64").and_then(|c| c.as_any().downcast_ref::<Int64Array>()),
                    batch.column_by_name("v_f64").and_then(|c| c.as_any().downcast_ref::<Float64Array>()),
                ) else {
                    continue;
                };
                for i in 0..batch.num_rows() {
                    let value = if vf.is_valid(i) { vf.value(i) } else if vi.is_valid(i) { vi.value(i) as f64 } else { continue };
                    if let Some((name, v)) = history_attribute(cluster.value(i), attribute.value(i), value) {
                        out.push((ts.value(i) / 1000, node.value(i), name, v));
                    }
                }
            }
        }
        date += chrono::Duration::days(1);
    }
    out
}

fn history_attribute(cluster: u32, attribute: u32, value: f64) -> Option<(&'static str, f64)> {
    match (cluster, attribute) {
        (THERMOSTAT_CLUSTER, 0) => Some((LOCAL_TEMPERATURE, value / 100.0)),
        (THERMOSTAT_CLUSTER, 8) => Some((PI_HEATING_DEMAND, value)),
        (THERMOSTAT_CLUSTER, 18) => Some((OCCUPIED_SETPOINT, value / 100.0)),
        _ => None,
    }
}

fn parquet_history(root: &Path, from_ms: i64, to_ms: i64) -> anyhow::Result<Vec<(i64, u64, &'static str, f64)>> {
    let day = |ms: i64| lake::date_of_us(ms * 1000);
    let db = crate::report::open_lake(root, day(from_ms), day(to_ms))?;
    let mut stmt = db.conn.prepare("SELECT CAST(epoch_us(ts) / 1000 AS BIGINT), CAST(node AS UBIGINT), CAST(attribute AS UINTEGER), CAST(coalesce(v_f64, CAST(v_i64 AS DOUBLE)) AS DOUBLE) FROM attribute_reports WHERE cluster = 513 AND attribute IN (0, 8, 18) AND coalesce(v_f64, CAST(v_i64 AS DOUBLE)) IS NOT NULL AND epoch_us(ts) >= ?")?;
    let rows = stmt.query_map([from_ms * 1000], |r| Ok((r.get::<_, i64>(0)?, r.get::<_, u64>(1)?, r.get::<_, u32>(2)?, r.get::<_, f64>(3)?)))?;
    Ok(rows.flatten().filter_map(|(ts, node, attribute, v)| history_attribute(THERMOSTAT_CLUSTER, attribute, v).map(|(n, v)| (ts, node, n, v))).collect())
}

fn median(mut v: Vec<f64>) -> f64 {
    v.sort_by(|a, b| a.total_cmp(b));
    v[v.len() / 2]
}

fn slope_per_h(points: &[(i64, f64)]) -> f64 {
    let n = points.len() as f64;
    let (mx, my) = (points.iter().map(|p| p.0 as f64).sum::<f64>() / n, points.iter().map(|p| p.1).sum::<f64>() / n);
    let var: f64 = points.iter().map(|p| (p.0 as f64 - mx).powi(2)).sum();
    if var <= 0.0 {
        return 0.0;
    }
    points.iter().map(|p| (p.0 as f64 - mx) * (p.1 - my)).sum::<f64>() / var * 3_600_000.0
}

impl RoomWin {
    fn samples(&self) -> Vec<(i64, f64)> {
        self.series.iter().copied().filter(|s| s.0 > self.floor_ms).collect()
    }

    fn at(&self, ms: i64, max_age: i64) -> Option<(i64, f64)> {
        self.series.iter().rev().find(|s| s.0 <= ms && s.0 > self.floor_ms && ms - s.0 <= max_age).copied()
    }

    fn within(&self, from_ms: i64, now: i64, max_age: i64) -> Vec<(i64, f64)> {
        let base = self.at(from_ms, max_age);
        let mut real: Vec<(i64, f64)> = base.into_iter().collect();
        real.extend(self.samples().into_iter().filter(|s| s.0 >= from_ms && s.0 <= now));
        let start = (1..real.len()).rev().find(|&i| real[i].0 - real[i - 1].0 > max_age).unwrap_or(0);
        let mut out = real[start..].to_vec();
        if start == 0 && base.is_some() {
            out[0].0 = from_ms;
        }
        out
    }

    fn trend(&self, from: i64, now: i64) -> Option<f64> {
        let mut points = self.within(from, now, KEEP_MS);
        points.push((now, self.samples().last()?.1));
        (points[0].0 <= from + 5 * MIN_MS).then(|| slope_per_h(&points))
    }
}

impl NodeWin {
    fn set_valve(&mut self, high_pct: f64, ts_ms: i64, value: f64) {
        if !(0.0..=100.0).contains(&value) {
            self.valve = None;
            self.high_since = None;
            self.over_since = None;
            return;
        }
        self.high_since = if value >= high_pct { self.high_since.or(Some(ts_ms)) } else { None };
        self.valve = Some(value);
    }
}

impl Windows {
    pub fn touch(&mut self, config: &Config, node: u64, ts_ms: i64) {
        let n = self.nodes.entry(node).or_default();
        let gap = n.seen_ms.is_some_and(|s| ts_ms - s > config.stale.after_ms());
        n.seen_ms = Some(n.seen_ms.map_or(ts_ms, |s| s.max(ts_ms)));
        if !gap {
            return;
        }
        n.gap_ms = Some(ts_ms);
        n.over_since = None;
        n.high_since = None;
        if let Some(room) = config.rooms.iter().find(|r| r.nodes.contains(&node)) {
            self.rooms.entry(room.name.clone()).or_default().floor_ms = ts_ms - 1;
        }
    }

    pub fn reconnected(&self, node: u64, now: i64, within_ms: i64) -> bool {
        self.nodes.get(&node).and_then(|n| n.gap_ms).is_some_and(|g| now - g <= within_ms)
    }

    fn fresh(&self, node: u64, now: i64, stale_ms: i64) -> bool {
        self.nodes.get(&node).and_then(|n| n.seen_ms).is_some_and(|s| now - s <= stale_ms)
    }

    pub fn set_model(&mut self, bundle: crate::window_model::Bundle) {
        self.det.set_bundle(std::sync::Arc::new(bundle));
    }

    pub fn model_version(&self) -> Option<String> {
        self.det.version().map(Into::into)
    }

    pub fn observe(&mut self, config: &Config, node: u64, attribute: &str, value: f64, ts_ms: i64) {
        let Some(room) = config.rooms.iter().find(|r| r.nodes.contains(&node)) else { return };
        self.touch(config, node, ts_ms);
        self.det.push(node, attribute, ts_ms, value);
        let entry = self.rooms.entry(room.name.clone()).or_default();
        let n = self.nodes.entry(node).or_default();
        match attribute {
            LOCAL_TEMPERATURE => {
                n.temp = Some(value);
                let stale_ms = config.stale.after_ms();
                let temps: Vec<f64> = room.nodes.iter().filter_map(|id| self.nodes.get(id)).filter(|n| n.seen_ms.is_some_and(|s| ts_ms - s <= stale_ms)).filter_map(|n| n.temp).collect();
                if entry.nodes_seen != temps.len() {
                    entry.nodes_seen = temps.len();
                    entry.series.clear();
                    entry.floor_ms = ts_ms - 1;
                }
                entry.series.push_back((ts_ms, temps.iter().sum::<f64>() / temps.len() as f64));
                while entry.series.front().is_some_and(|s| s.0 < ts_ms - KEEP_MS) {
                    entry.series.pop_front();
                }
            }
            PI_HEATING_DEMAND => n.set_valve(config.supply.valve_pct, ts_ms, value),
            _ => {}
        }
    }

    pub fn set_context(&mut self, ctx: impl IntoIterator<Item = (String, RoomCtx)>) {
        self.ctx = ctx.into_iter().collect();
    }

    fn raise(&mut self, cfg: &WindowSection, room: &str, question: &'static str, reason: Option<String>, now: i64) {
        if self.prompt_at.get(room).is_some_and(|t| now - t < mins(cfg.prompt_gap_min)) {
            return;
        }
        let day = 24 * 60 * MIN_MS;
        while self.prompt_times.front().is_some_and(|t| now - t >= day) {
            self.prompt_times.pop_front();
        }
        if self.prompt_times.len() >= cfg.prompt_max_per_day || self.prompt_times.back().is_some_and(|t| *t != now && now - t < mins(cfg.prompt_global_gap_min)) {
            return;
        }
        self.prompt_times.push_back(now);
        self.prompt_at.insert(room.into(), now);
        self.prompt_seq += 1;
        let prompt = Prompt { id: format!("w{now}-{}", self.prompt_seq), room: room.into(), question, reason, created_ms: now };
        self.prompts.push_back(prompt.clone());
        while self.prompts.len() > 20 {
            self.prompts.pop_front();
        }
        self.prompt = Some(prompt);
    }

    #[cfg(test)]
    pub fn test_prompt(&mut self, cfg: &WindowSection, room: &str, question: &'static str, now: i64) {
        self.raise(cfg, room, question, None, now);
    }

    pub fn prompt(&self, cfg: &WindowSection, now: i64) -> Option<Value> {
        self.prompt.as_ref().filter(|p| now - p.created_ms < mins(cfg.prompt_expire_min)).map(|p| json!({"id": p.id, "room": p.room, "question": p.question, "reason": p.reason}))
    }

    pub fn label(&mut self, room: &str, ts: i64, open: bool, prompt_id: Option<&str>, label_id: Option<&str>, now: i64) -> Option<WinEvent> {
        let question = prompt_id.and_then(|id| self.prompts.iter().find(|p| p.id == id)).map(|p| p.question);
        if prompt_id.is_some_and(|id| self.prompt.as_ref().is_some_and(|p| p.id == id)) {
            self.prompt = None;
        }
        self.labels.push_back(LabelRec { label_id: label_id.map(Into::into), room: room.into(), ts, open, manual: prompt_id.is_none(), question });
        while self.labels.front().is_some_and(|l| l.ts < now - 24 * 3_600_000) || self.labels.len() > 500 {
            self.labels.pop_front();
        }
        if now - ts > LATE_LABEL_MS {
            return None;
        }
        if prompt_id.is_some() {
            let auto = self.rooms.get(room).and_then(|r| r.active.as_ref()).is_some_and(|a| a.source == "auto");
            return if auto && !open { self.close(room, "label", now) } else { None };
        }
        let event = self.manual(room, open, now);
        if event.is_some() && open {
            if let Some(r) = self.rooms.get_mut(room) {
                r.manual_label = label_id.map(Into::into);
            }
        }
        event
    }

    pub fn void_label(&mut self, label_id: &str, now: i64) -> Option<WinEvent> {
        let (voided, kept): (VecDeque<LabelRec>, VecDeque<LabelRec>) = std::mem::take(&mut self.labels).into_iter().partition(|l| l.label_id.as_deref() == Some(label_id));
        self.labels = kept;
        let rec = voided.into_iter().find(|l| l.manual && l.open)?;
        let manual_open = self.rooms.get(&rec.room).is_some_and(|r| r.manual_label.as_deref() == Some(label_id) && r.active.as_ref().is_some_and(|a| a.source == "manual"));
        if manual_open { self.manual(&rec.room, false, now) } else { None }
    }

    fn emit_finished(&mut self, cfg: &WindowSection, now: i64) -> Vec<WinEvent> {
        let grace = mins(cfg.label_grace_min);
        let (ready, waiting): (Vec<Episode>, Vec<Episode>) = std::mem::take(&mut self.finished).into_iter().partition(|e| now.saturating_sub(e.end_ms) >= grace);
        self.finished = waiting;
        let labels: Vec<LabelRec> = self.labels.iter().cloned().collect();
        ready.into_iter().map(|e| WinEvent { kind: lake::kind::WINDOW_EPISODE, room: e.room.clone(), detail: e.detail(&labels, grace) }).collect()
    }

    pub fn rebuild(&mut self, config: &Config, root: &Path, now_ms: i64) -> usize {
        let from = now_ms - (config.supply.learn_days * 86_400_000.0) as i64;
        let mut rows = parquet_history(root, from, now_ms).unwrap_or_else(|e| {
            eprintln!("window history: {e:#}");
            Vec::new()
        });
        rows.extend(raw_history(root, from, now_ms));
        rows.sort_by(|a, b| a.0.cmp(&b.0).then(a.1.cmp(&b.1)).then(a.2.cmp(b.2)));
        rows.dedup_by(|a, b| (a.0, a.1, a.2) == (b.0, b.1, b.2));
        for (ts, node, attribute, v) in &rows {
            self.observe(config, *node, attribute, *v, *ts);
            if *attribute == LOCAL_TEMPERATURE {
                if let Some(room) = config.rooms.iter().find(|r| r.nodes.contains(node)) {
                    self.supply_tick(config, &room.name, *ts);
                }
            }
            if config.window.enabled {
                self.det.advance(config, *ts, [f64::NAN; 3], self.rooms.values().any(|r| r.supply.is_some()));
            }
        }
        self.finished.clear();
        self.last = None;
        rows.len()
    }

    pub fn supply_tick(&mut self, config: &Config, room: &str, now: i64) -> Vec<WinEvent> {
        let cfg = &config.supply;
        let Some(nodes) = config.rooms.iter().find(|r| r.name == room).map(|r| r.nodes.clone()).filter(|n| cfg.enabled && !n.is_empty()) else { return Vec::new() };
        if !nodes.iter().all(|id| self.fresh(*id, now, config.stale.after_ms())) {
            return Vec::new();
        }
        let Some(mut w) = self.rooms.remove(room) else { return Vec::new() };
        let event = |state: &str, since: i64, extra: Value| {
            let mut detail = json!({"room": room, "state": state, "since_ms": since});
            detail.as_object_mut().unwrap().extend(extra.as_object().cloned().unwrap_or_default());
            WinEvent { kind: lake::kind::SUPPLY, room: room.into(), detail }
        };
        let mut out = Vec::new();
        match w.supply {
            Some(since) if w.trend(now - mins(cfg.recover_window_min), now).is_some_and(|s| s >= cfg.recover_k_per_h) => {
                w.supply = None;
                self.episodes.push((room.into(), since, now));
                nodes.iter().for_each(|id| self.nodes.entry(*id).or_default().high_since = Some(now));
                out.push(event("end", since, json!({"duration_min": (now - since) as f64 / 60_000.0})));
            }
            Some(_) => {}
            None => {
                let highs: Option<Vec<i64>> = nodes.iter().map(|id| self.nodes.get(id).and_then(|n| n.high_since)).collect();
                if let Some(since) = highs.and_then(|h| h.into_iter().max()).filter(|s| now - s >= mins(cfg.min_min)) {
                    if w.trend(now - mins(cfg.min_min), now).is_some_and(|s| s <= cfg.flat_k_per_h) {
                        w.supply = Some(since);
                        out.push(event("start", since, json!({})));
                    }
                }
            }
        }
        self.rooms.insert(room.into(), w);
        out
    }

    pub fn no_supply_rooms(&self) -> HashSet<String> {
        self.rooms.iter().filter(|(_, r)| r.supply.is_some()).map(|(n, _)| n.clone()).collect()
    }

    pub fn local_temp(&self, node: u64, now: i64, stale_ms: i64) -> Option<f64> {
        self.nodes.get(&node).filter(|_| self.fresh(node, now, stale_ms)).and_then(|n| n.temp)
    }

    pub fn overshoot_due(&mut self, cfg: &SupplySection, node: u64, setpoint: f64, now: i64, stale_ms: i64) -> Option<(f64, f64)> {
        let fresh = self.fresh(node, now, stale_ms);
        let n = self.nodes.get_mut(&node)?;
        if !fresh {
            n.over_since = None;
            return None;
        }
        let hit = n.temp.zip(n.valve).filter(|(t, v)| *t >= setpoint + cfg.overshoot_k && *v >= cfg.overshoot_valve_pct);
        if hit.is_none() {
            n.over_since = None;
            return None;
        }
        let since = *n.over_since.get_or_insert(now);
        hit.filter(|_| now - since >= mins(cfg.overshoot_min))
    }

    pub fn clear_overshoot(&mut self, node: u64) {
        if let Some(n) = self.nodes.get_mut(&node) {
            n.over_since = None;
        }
    }

    pub fn supply_view(&self, config: &Config, now: i64) -> Value {
        let horizon = now - (config.supply.learn_days * 86_400_000.0) as i64;
        let episodes: Vec<(i64, i64)> = self.episodes.iter().filter(|e| e.2 >= horizon).map(|e| (e.1, e.2)).collect();
        json!({"active": self.rooms.iter().filter_map(|(n, r)| r.supply.map(|s| json!({"room": n, "since_ms": s}))).collect::<Vec<_>>(), "learned": supply_window(&episodes)})
    }

    pub fn note_decrease(&mut self, room: &str, ts_ms: i64) {
        self.rooms.entry(room.into()).or_default().decrease_ms = Some(ts_ms);
    }

    pub fn open_rooms(&self) -> HashSet<String> {
        self.rooms.iter().filter(|(_, r)| r.active.is_some()).map(|(n, _)| n.clone()).collect()
    }

    fn close(&mut self, room: &str, reason: &'static str, now: i64) -> Option<WinEvent> {
        let r = self.rooms.get_mut(room)?;
        let a = r.active.take()?;
        r.manual_label = None;
        r.last_close = Some((reason, now));
        self.finished.push(Episode { room: room.into(), start_ms: a.first_ms, end_ms: now, reason, source: a.source, score: a.score });
        Some(WinEvent { kind: lake::kind::WINDOW_CLOSE, room: room.into(), detail: json!({"room": room, "detector": DETECTOR, "reason": reason, "source": a.source, "open_minutes": (now - a.first_ms) as f64 / 60_000.0}) })
    }

    pub fn manual(&mut self, room: &str, open: bool, now: i64) -> Option<WinEvent> {
        if !open {
            return self.close(room, "manual", now);
        }
        let r = self.rooms.entry(room.into()).or_default();
        match r.active.as_mut() {
            Some(a) if a.source == "manual" => return None,
            Some(a) => {
                a.source = "manual";
                a.since_ms = now;
            }
            None => r.active = Some(Active { source: "manual", first_ms: now, since_ms: now, score: None }),
        }
        r.manual_label = None;
        Some(WinEvent { kind: lake::kind::WINDOW_OPEN, room: room.into(), detail: json!({"room": room, "source": "manual"}) })
    }

    pub fn step(&mut self, config: &Config, now: i64, weather: impl Fn(f64) -> [f64; 3]) -> Vec<WinEvent> {
        let cfg = &config.window;
        let mut events: Vec<WinEvent> = config.rooms.iter().flat_map(|r| self.supply_tick(config, &r.name, now)).collect();
        let due: Vec<(String, &'static str)> = self.rooms.iter().filter_map(|(n, r)| r.active.as_ref().and_then(|a| if a.source == "manual" { (now - a.since_ms >= mins(cfg.manual_max_min)).then_some((n.clone(), "manual_timeout")) } else { (now - a.since_ms >= mins(cfg.hold_min)).then_some((n.clone(), "timeout")) })).collect();
        events.extend(due.into_iter().filter_map(|(room, reason)| self.close(&room, reason, now)));
        if !cfg.enabled {
            return events;
        }
        let age = self.det.bundle().map_or(180.0, |b| b.weather_max_age_min);
        let supply = self.rooms.values().any(|r| r.supply.is_some());
        for minute in self.det.advance(config, now, weather(age), supply) {
            for (name, z) in &minute.z {
                self.rooms.entry(name.clone()).or_default().z = Some(*z).filter(|z| z.is_finite());
            }
            let channels = self.det.bundle().map(|b| minute.alarms(b)).unwrap_or_default();
            if !channels.is_empty() {
                events.extend(self.alarm(config, &minute, &channels, now));
            }
            self.last = Some(minute);
        }
        events.extend(self.emit_finished(cfg, now));
        events
    }

    fn alarm(&mut self, config: &Config, m: &Scored, channels: &[Channel], now: i64) -> Vec<WinEvent> {
        let cfg = &config.window;
        let Some(bundle) = self.det.bundle().cloned() else { return Vec::new() };
        let thermostat = |r: &&crate::config::Room| !r.nodes.is_empty();
        let ground: Vec<&crate::config::Room> = config.rooms.iter().filter(thermostat).filter(|r| !config.building.roof_rooms.contains(&r.name)).collect();
        let z = |r: &crate::config::Room| m.z.get(&r.name).copied().filter(|v| v.is_finite());
        let mut targets: Vec<(&crate::config::Room, &Channel)> = Vec::new();
        for channel in channels {
            let rooms: Vec<&crate::config::Room> = match channel {
                Channel::Room(name) => config.rooms.iter().filter(thermostat).filter(|r| &r.name == name).collect(),
                Channel::House => {
                    let hot: Vec<&crate::config::Room> = ground.iter().copied().filter(|r| z(r).is_some_and(|v| v >= cfg.room_z)).collect();
                    if hot.is_empty() { ground.clone() } else { hot }
                }
            };
            for r in rooms {
                if !targets.iter().any(|t| t.0.name == r.name) {
                    targets.push((r, channel));
                }
            }
        }
        let (mut free, mut events) = (Vec::new(), Vec::new());
        for (room, channel) in targets {
            let ctx = self.ctx.get(&room.name).cloned().unwrap_or_default();
            let w = self.rooms.entry(room.name.clone()).or_default();
            if w.active.is_some() {
                continue;
            }
            let why = if w.last_close.is_some_and(|(_, at)| now - at < mins(cfg.rearm_min)) {
                Some("rearm")
            } else if w.decrease_ms.is_some_and(|d| now - d <= mins(cfg.setpoint_guard_min)) {
                Some("setpoint")
            } else if ctx.hold {
                Some("manual_hold")
            } else if ctx.away {
                Some("away")
            } else {
                None
            };
            let threshold = bundle.threshold(match channel { Channel::House => "house", Channel::Room(name) => name });
            let Some(why) = why else {
                free.push((room.name.clone(), channel, threshold));
                continue;
            };
            if w.logged.get(why).is_some_and(|t| now - t < SUPPRESS_LOG_MS) {
                continue;
            }
            w.suppressed += 1;
            w.logged.insert(why, now);
            events.push(WinEvent { kind: lake::kind::WINDOW_SUPPRESSED, room: room.name.clone(), detail: json!({"room": room.name, "detector": DETECTOR, "reason": why, "house": m.house, "threshold": threshold}) });
        }
        let zs: BTreeMap<&String, Option<f64>> = m.z.iter().map(|(k, v)| (k, v.is_finite().then_some(*v))).collect();
        let names: Vec<&String> = free.iter().map(|f| &f.0).collect();
        for (name, channel, threshold) in &free {
            let w = self.rooms.entry(name.clone()).or_default();
            let own = zs.get(name).copied().flatten().unwrap_or(f64::NAN);
            w.active = Some(Active { source: "auto", first_ms: now, since_ms: now, score: Some((m.house, own)) });
            w.auto_opens += 1;
            let channel_name = match channel { Channel::House => "house", Channel::Room(room) => room };
            events.push(WinEvent { kind: lake::kind::WINDOW_OPEN, room: name.clone(), detail: json!({"room": name, "source": "auto", "detector": DETECTOR, "channel": channel_name, "reason": m.reason(&bundle, name, channel), "rooms": names, "z": zs, "house": m.house, "threshold": threshold, "bundle": bundle.version, "scored_ms": m.ts_ms}) });
        }
        let mut by_z: Vec<&(String, &Channel, f64)> = free.iter().collect();
        by_z.sort_by(|a, b| zs.get(&a.0).copied().flatten().unwrap_or(f64::MIN).total_cmp(&zs.get(&b.0).copied().flatten().unwrap_or(f64::MIN)));
        for (name, channel, _) in by_z {
            self.raise(cfg, name, "open", Some(m.reason(&bundle, name, channel)), now);
        }
        events
    }

    pub fn view(&self, room: &str, now: i64) -> Value {
        let Some(r) = self.rooms.get(room) else { return json!({"state": "closed", "reason": null}) };
        let a = r.active.as_ref();
        json!({
            "state": if a.is_some() { "open" } else { "closed" },
            "reason": if a.is_some() { a.map(|a| a.source) } else { r.last_close.map(|c| c.0) },
            "source": a.map(|a| a.source),
            "since_ts": a.map(|a| a.first_ms),
            "open_minutes": a.map(|a| (now - a.first_ms) as f64 / 60_000.0),
            "last_close": r.last_close.map(|c| json!({"reason": c.0, "ts": c.1})),
            "auto_opens": r.auto_opens,
            "suppressed": r.suppressed,
            "no_supply_since": r.supply,
            "z": r.z,
            "model": self.det.version(),
        })
    }

    pub fn metrics(&self) -> String {
        use std::fmt::Write;
        let mut out = String::new();
        for (name, help, kind) in [("heatd_window_state", "window detection state per room (0 closed, 1 open)", "gauge"), ("heatd_window_auto_opens_total", "automatic window openings per room since start", "counter"), ("heatd_window_suppressed_total", "vetoed window alarms per room since start", "counter"), ("heatd_window_z", "latest window evidence z per room (absent while unscorable)", "gauge")] {
            let _ = writeln!(out, "# HELP {name} {help}\n# TYPE {name} {kind}");
            for (room, r) in &self.rooms {
                let v = match name {
                    "heatd_window_state" => f64::from(u8::from(r.active.is_some())),
                    "heatd_window_auto_opens_total" => r.auto_opens as f64,
                    "heatd_window_suppressed_total" => r.suppressed as f64,
                    _ => match r.z {
                        Some(z) => z,
                        None => continue,
                    },
                };
                let _ = writeln!(out, "{name}{{room=\"{room}\"}} {v}");
            }
        }
        if let Some(house) = self.last.as_ref().map(|m| m.house).filter(|h| h.is_finite()) {
            let _ = writeln!(out, "# HELP heatd_window_house_score latest window house score\n# TYPE heatd_window_house_score gauge\nheatd_window_house_score {house}");
        }
        out
    }
}

fn local_minute(ms: i64) -> Option<i64> {
    Local.timestamp_millis_opt(ms).single().map(|d| i64::from(d.hour() * 60 + d.minute()))
}

pub fn supply_window(episodes: &[(i64, i64)]) -> Value {
    let long: Vec<(i64, i64)> = episodes.iter().filter_map(|(a, b)| (b - a >= 30 * MIN_MS).then_some(local_minute(*a)?).zip(local_minute(*b))).collect();
    if long.len() < 2 {
        return Value::Null;
    }
    let mid = |pick: fn(&(i64, i64)) -> i64| (median(long.iter().map(|e| ((pick(e) + 720) % 1440) as f64).collect()) as i64 + 720) % 1440;
    let (from, to) = (mid(|e| e.0), mid(|e| e.1));
    let clock = |m: i64| format!("{:02}:{:02}", m / 60, m % 60);
    json!({"from": clock(from), "to": clock(to), "episodes": long.len(), "label": format!("Kessel liefert nicht: ~{}-{}", clock(from), clock(to))})
}
