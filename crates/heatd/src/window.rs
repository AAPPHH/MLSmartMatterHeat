use crate::config::{Config, WindowSection};
use crate::lake::{self, Table};
use arrow::array::{Array, Float64Array, Int64Array, TimestampMicrosecondArray, UInt32Array, UInt64Array};
use serde_json::{json, Value};
use std::collections::{BTreeMap, HashMap, HashSet, VecDeque};
use std::path::Path;

const MIN_MS: i64 = 60_000;
const KEEP_MS: i64 = 3 * 3_600_000;
const SUPPRESS_LOG_MS: i64 = 15 * MIN_MS;
const MIN_SAMPLES: usize = 3;
const BIG_DROP_PCT: f64 = 30.0;
const THERMOSTAT_CLUSTER: u32 = 513;

pub const LOCAL_TEMPERATURE: &str = "local_temperature";
pub const PI_HEATING_DEMAND: &str = "pi_heating_demand";

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Phase {
    Open,
    Probe,
}

#[derive(Debug, Clone)]
struct Active {
    phase: Phase,
    source: &'static str,
    first_ms: i64,
    since_ms: i64,
    timeout_min: f64,
    rate: f64,
}

#[derive(Debug, Default)]
struct NodeWin {
    temp: Option<f64>,
    valve: Option<f64>,
    closed_ms: Option<i64>,
    valve_before: Option<f64>,
    cool_start: Option<(i64, f64, f64)>,
    samples: VecDeque<(i64, f64)>,
}

#[derive(Debug, Default)]
struct RoomWin {
    active: Option<Active>,
    series: VecDeque<(i64, f64)>,
    floor_ms: i64,
    slope_since: Option<i64>,
    opens: VecDeque<i64>,
    decrease_ms: Option<i64>,
    logged: HashMap<&'static str, i64>,
    last_close: Option<(&'static str, i64)>,
    min_temp: Option<f64>,
    auto_opens: u64,
    reopens: u64,
    suppressed: u64,
    nodes_seen: usize,
    learned: Vec<f64>,
}

#[derive(Debug, Clone, PartialEq)]
pub struct WinEvent {
    pub kind: &'static str,
    pub room: String,
    pub detail: Value,
}

#[derive(Debug, Default)]
pub struct Windows {
    rooms: BTreeMap<String, RoomWin>,
    nodes: HashMap<u64, NodeWin>,
}

struct Reading {
    drop_k: f64,
    slope: f64,
    temp: f64,
    hit_drop: bool,
    hit_slope: bool,
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
        _ => None,
    }
}

fn parquet_history(root: &Path, from_ms: i64, to_ms: i64) -> anyhow::Result<Vec<(i64, u64, &'static str, f64)>> {
    let day = |ms: i64| lake::date_of_us(ms * 1000);
    let db = crate::report::open_lake(root, day(from_ms), day(to_ms))?;
    let mut stmt = db.conn.prepare("SELECT CAST(epoch_us(ts) / 1000 AS BIGINT), CAST(node AS UBIGINT), CAST(attribute AS UINTEGER), CAST(coalesce(v_f64, CAST(v_i64 AS DOUBLE)) AS DOUBLE) FROM attribute_reports WHERE cluster = 513 AND attribute IN (0, 8) AND coalesce(v_f64, CAST(v_i64 AS DOUBLE)) IS NOT NULL AND epoch_us(ts) >= ?")?;
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

    fn reading(&self, cfg: &WindowSection, now: i64, factor: f64) -> Option<Reading> {
        let temp = self.samples().last()?.1;
        let min = |m: f64| now - (m * MIN_MS as f64) as i64;
        let age = (cfg.open_window_min * MIN_MS as f64) as i64;
        let peak = self.within(min(cfg.open_window_min), now, age).iter().map(|s| s.1).fold(temp, f64::max);
        let mut points = self.within(min(cfg.window_slope_min), now, age);
        points.push((now, temp));
        let slope = slope_per_h(&points);
        Some(Reading { drop_k: peak - temp, slope, temp, hit_drop: peak - temp >= cfg.open_drop_k * factor, hit_slope: points.len() >= 3 && slope <= cfg.open_slope_k_per_h * factor })
    }

    fn close(&mut self, reason: &'static str, now: i64) {
        self.active = None;
        self.floor_ms = now;
        self.slope_since = None;
        self.last_close = Some((reason, now));
    }
}

impl NodeWin {
    fn set_valve(&mut self, cfg: &WindowSection, ts_ms: i64, value: f64) {
        if !(0.0..=100.0).contains(&value) {
            self.valve = None;
            self.closed_ms = None;
            self.cool_start = None;
            return;
        }
        let prev = self.valve;
        let low = value <= cfg.valve_low_pct;
        if low {
            if !prev.is_some_and(|p| p <= cfg.valve_low_pct) {
                self.closed_ms = Some(ts_ms);
            }
        } else {
            self.closed_ms = None;
        }
        if let Some(p) = prev.filter(|p| *p > value) {
            if (low && p > cfg.valve_low_pct) || p - value >= BIG_DROP_PCT {
                self.valve_before = Some(p);
                self.cool_start = self.temp.map(|t| (ts_ms, t, 0.0));
            }
        } else if prev.is_some_and(|p| value > p) {
            self.cool_start = None;
        }
        self.valve = Some(value);
    }

    fn track_cooldown(&mut self, cfg: &WindowSection, ts_ms: i64, temp: f64) {
        let Some((t0, temp0, best)) = self.cool_start else { return };
        let span = (cfg.cooldown_sample_min * MIN_MS as f64) as i64;
        let dt = ts_ms - t0;
        if dt > span {
            self.cool_start = None;
            if best > 0.0 {
                self.push_sample(cfg, ts_ms, best);
            }
        } else if dt >= 0 {
            let rate = (temp0 - temp) / (dt.max(2 * MIN_MS) as f64 / 3_600_000.0);
            self.cool_start = Some((t0, temp0, best.max(rate)));
        }
    }

    fn push_sample(&mut self, cfg: &WindowSection, ts_ms: i64, rate: f64) {
        let horizon = ts_ms - (cfg.cooldown_days * 86_400_000.0) as i64;
        while self.samples.front().is_some_and(|s| s.0 < horizon) {
            self.samples.pop_front();
        }
        self.samples.push_back((ts_ms, rate));
    }

    fn cold(&self, cfg: &WindowSection, now: i64) -> bool {
        self.valve.is_some_and(|v| v <= cfg.valve_low_pct) && self.closed_ms.is_some_and(|c| now - c >= (cfg.radiator_cold_min * MIN_MS as f64) as i64)
    }
}

impl Windows {
    pub fn observe(&mut self, config: &Config, node: u64, attribute: &str, value: f64, ts_ms: i64) {
        let Some(room) = config.rooms.iter().find(|r| r.nodes.contains(&node)) else { return };
        let cfg = &config.window;
        let entry = self.rooms.entry(room.name.clone()).or_default();
        let n = self.nodes.entry(node).or_default();
        match attribute {
            LOCAL_TEMPERATURE => {
                n.temp = Some(value);
                n.track_cooldown(cfg, ts_ms, value);
                let temps: Vec<f64> = room.nodes.iter().filter_map(|id| self.nodes.get(id).and_then(|n| n.temp)).collect();
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
            PI_HEATING_DEMAND => n.set_valve(cfg, ts_ms, value),
            _ => {}
        }
    }

    pub fn rebuild(&mut self, config: &Config, root: &Path, now_ms: i64) -> usize {
        let from = now_ms - (config.window.cooldown_days * 86_400_000.0) as i64;
        let mut rows = parquet_history(root, from, now_ms).unwrap_or_else(|e| {
            eprintln!("window history: {e:#}");
            Vec::new()
        });
        rows.extend(raw_history(root, from, now_ms));
        rows.sort_by(|a, b| a.0.cmp(&b.0).then(a.1.cmp(&b.1)).then(a.2.cmp(b.2)));
        rows.dedup_by(|a, b| (a.0, a.1, a.2) == (b.0, b.1, b.2));
        rows.iter().for_each(|(ts, node, attribute, v)| self.observe(config, *node, attribute, *v, *ts));
        rows.len()
    }

    pub fn note_decrease(&mut self, room: &str, ts_ms: i64) {
        self.rooms.entry(room.into()).or_default().decrease_ms = Some(ts_ms);
    }

    #[cfg(test)]
    pub fn samples(&self, node: u64) -> Vec<f64> {
        self.nodes.get(&node).map_or_else(Vec::new, |n| n.samples.iter().map(|s| s.1).collect())
    }

    pub fn open_rooms(&self) -> HashSet<String> {
        self.rooms.iter().filter(|(_, r)| r.active.as_ref().is_some_and(|a| a.phase == Phase::Open)).map(|(n, _)| n.clone()).collect()
    }

    pub fn active_rooms(&self) -> HashSet<String> {
        self.rooms.iter().filter(|(_, r)| r.active.is_some()).map(|(n, _)| n.clone()).collect()
    }

    pub fn manual(&mut self, room: &str, open: bool, now: i64) -> Option<WinEvent> {
        let r = self.rooms.entry(room.into()).or_default();
        if open {
            if r.active.as_ref().is_some_and(|a| a.phase == Phase::Open && a.source == "manual") {
                return None;
            }
            let first = r.active.as_ref().map_or(now, |a| a.first_ms);
            r.active = Some(Active { phase: Phase::Open, source: "manual", first_ms: first, since_ms: now, timeout_min: f64::MAX, rate: 0.0 });
            r.min_temp = r.series.back().map(|s| s.1);
            return Some(WinEvent { kind: "window_open", room: room.into(), detail: json!({"room": room, "source": "manual"}) });
        }
        let active = r.active.clone()?;
        let detail = json!({"room": room, "reason": "manual", "open_minutes": (now - active.first_ms) as f64 / 60_000.0, "min_temp": r.min_temp});
        r.close("manual", now);
        Some(WinEvent { kind: "window_close", room: room.into(), detail })
    }

    fn typical(&self, cfg: &WindowSection, nodes: &[u64]) -> f64 {
        nodes
            .iter()
            .map(|id| {
                let s: Vec<f64> = self.nodes.get(id).map_or_else(Vec::new, |n| n.samples.iter().map(|s| s.1).collect());
                if s.len() >= MIN_SAMPLES { median(s).max(cfg.fallback_cooldown_k_per_h) } else { cfg.fallback_cooldown_k_per_h }
            })
            .fold(cfg.fallback_cooldown_k_per_h, f64::max)
    }

    fn valve_before(&self, nodes: &[u64], now: i64, grace: i64) -> Option<f64> {
        nodes
            .iter()
            .filter_map(|id| self.nodes.get(id))
            .filter_map(|n| if n.closed_ms.is_some_and(|c| now - c <= grace) { n.valve_before } else { n.valve })
            .fold(None, |a: Option<f64>, v| Some(a.map_or(v, |a| a.max(v))))
    }

    fn heating(&self, nodes: &[u64]) -> Option<bool> {
        let known: Vec<f64> = nodes.iter().filter_map(|id| self.nodes.get(id).and_then(|n| n.valve)).collect();
        (!known.is_empty()).then(|| known.iter().any(|v| *v > 0.0))
    }

    pub fn step(&mut self, config: &Config, now: i64, outdoor: Option<(f64, Option<f64>)>) -> Vec<WinEvent> {
        let cfg = &config.window;
        if !cfg.enabled {
            return Vec::new();
        }
        let mut events = Vec::new();
        for room in config.rooms.iter().filter(|r| !r.nodes.is_empty()) {
            let Some(mut w) = self.rooms.remove(&room.name) else { continue };
            let evs = self.room_step(cfg, &room.name, &room.nodes, &mut w, now, outdoor);
            events.extend(evs);
            for rate in std::mem::take(&mut w.learned) {
                for id in &room.nodes {
                    self.nodes.entry(*id).or_default().push_sample(cfg, now, rate);
                }
            }
            self.rooms.insert(room.name.clone(), w);
        }
        events
    }

    fn room_step(&self, cfg: &WindowSection, name: &str, nodes: &[u64], w: &mut RoomWin, now: i64, outdoor: Option<(f64, Option<f64>)>) -> Vec<WinEvent> {
        let mins = |m: f64| (m * MIN_MS as f64) as i64;
        let age = mins(cfg.open_window_min);
        let event = |kind: &'static str, detail: Value| WinEvent { kind, room: name.into(), detail };
        let Some(cur) = w.series.back().map(|s| s.1) else { return Vec::new() };
        let Some(active) = w.active.clone() else {
            return self.detect(cfg, name, nodes, w, now, outdoor);
        };
        let since = active.since_ms;
        let seen: Vec<(i64, f64)> = w.samples().into_iter().filter(|s| s.0 >= since).collect();
        let low = seen.iter().map(|s| s.1).fold(cur, f64::min);
        w.min_temp = Some(w.min_temp.map_or(low, |m| m.min(low)));
        let closed = |reason: &'static str, w: &mut RoomWin| {
            let detail = json!({"room": name, "reason": reason, "open_minutes": (now - active.first_ms) as f64 / 60_000.0, "min_temp": w.min_temp});
            w.close(reason, now);
            vec![event("window_close", detail)]
        };
        if active.source == "manual" {
            return if now - since >= mins(cfg.manual_max_min) { closed("manual_timeout", w) } else { Vec::new() };
        }
        match active.phase {
            Phase::Open => {
                if now - since >= mins(cfg.min_open_min) {
                    let from = (now - mins(cfg.close_window_min)).max(since);
                    let recent: Vec<f64> = w.within(from, now, age).iter().map(|s| s.1).collect();
                    let rise = cur - recent.iter().copied().fold(cur, f64::min);
                    if rise >= cfg.close_rise_k && self.heating(nodes) != Some(true) {
                        return closed("recovery", w);
                    }
                }
                if (now - since) as f64 >= active.timeout_min * MIN_MS as f64 {
                    w.active = Some(Active { phase: Phase::Probe, since_ms: now, ..active });
                    w.slope_since = None;
                }
                Vec::new()
            }
            Phase::Probe => {
                let peak = w.within(since, now, age).iter().map(|s| s.1).fold(cur, f64::max);
                if peak - cur >= cfg.probe_drop_k && self.heating(nodes) != Some(false) {
                    let timeout = (active.timeout_min * 2.0).min(cfg.max_timeout_min);
                    w.active = Some(Active { phase: Phase::Open, since_ms: now, timeout_min: timeout, ..active });
                    w.reopens += 1;
                    return vec![event("window_reopen", json!({"room": name, "drop_k": peak - cur, "timeout_min": timeout, "open_minutes": (now - active.first_ms) as f64 / 60_000.0}))];
                }
                if now - since >= mins(cfg.probe_min) {
                    w.learned.push(active.rate);
                    return closed("timeout_probe", w);
                }
                Vec::new()
            }
        }
    }

    fn detect(&self, cfg: &WindowSection, name: &str, nodes: &[u64], w: &mut RoomWin, now: i64, outdoor: Option<(f64, Option<f64>)>) -> Vec<WinEvent> {
        let mins = |m: f64| (m * MIN_MS as f64) as i64;
        let wind = outdoor.and_then(|o| o.1);
        let factor = if wind.is_some_and(|v| v > cfg.wind_ms) { 1.0 - cfg.wind_relax } else { 1.0 };
        let Some(r) = w.reading(cfg, now, factor) else { return Vec::new() };
        w.slope_since = if r.hit_slope { w.slope_since.or(Some(now)) } else { None };
        let sustained = w.slope_since.is_some_and(|s| now - s >= mins(cfg.open_sustain_min));
        if !r.hit_drop && !sustained {
            return Vec::new();
        }
        let warm = !nodes.iter().all(|id| self.nodes.get(id).is_some_and(|n| n.cold(cfg, now))) || w.decrease_ms.is_some_and(|d| now - d < mins(cfg.radiator_cold_min));
        let guard = self.typical(cfg, nodes) * cfg.expected_cooldown_factor;
        let drop_rate = r.drop_k / (cfg.open_window_min / 60.0);
        let by_drop = r.hit_drop && (!warm || drop_rate > guard);
        let by_slope = sustained && (!warm || -r.slope > guard);
        while w.opens.front().is_some_and(|t| now - t >= 3_600_000) {
            w.opens.pop_front();
        }
        let blocked = if w.decrease_ms.is_some_and(|d| now - d <= mins(cfg.setpoint_guard_min)) {
            Some("setpoint")
        } else if outdoor.is_some_and(|o| r.temp - o.0 < cfg.min_outdoor_delta_k) {
            Some("outdoor")
        } else if !by_drop && !by_slope {
            Some("cooldown")
        } else if w.opens.len() >= cfg.max_events_per_hour {
            Some("flapping")
        } else {
            None
        };
        let valve_before = self.valve_before(nodes, now, mins(cfg.cooldown_grace_min));
        let detail = json!({"room": name, "drop_k": r.drop_k, "slope": r.slope, "outdoor": outdoor.map(|o| o.0), "wind": wind, "valve_before": valve_before, "radiator_warm": warm});
        let Some(why) = blocked else {
            let rate = if by_drop { drop_rate } else { -r.slope };
            w.active = Some(Active { phase: Phase::Open, source: "auto", first_ms: now, since_ms: now, timeout_min: cfg.max_open_min, rate });
            w.opens.push_back(now);
            w.auto_opens += 1;
            w.slope_since = None;
            w.min_temp = Some(r.temp);
            let mut detail = detail;
            detail["source"] = json!("auto");
            return vec![WinEvent { kind: "window_open", room: name.into(), detail }];
        };
        if w.logged.get(why).is_some_and(|t| now - t < SUPPRESS_LOG_MS) {
            return Vec::new();
        }
        w.suppressed += 1;
        w.logged.insert(why, now);
        let mut detail = detail;
        detail["reason"] = json!(why);
        vec![WinEvent { kind: "window_suppressed", room: name.into(), detail }]
    }

    pub fn view(&self, room: &str, now: i64) -> Value {
        let Some(r) = self.rooms.get(room) else { return json!({"state": "closed", "reason": null}) };
        let (state, reason) = match r.active.as_ref() {
            Some(a) if a.phase == Phase::Open => ("open", Some(a.source)),
            Some(_) => ("probe", Some("timeout_probe")),
            None => ("closed", r.last_close.map(|c| c.0)),
        };
        let a = r.active.as_ref();
        json!({
            "state": state,
            "reason": reason,
            "source": a.map(|a| a.source),
            "since_ts": a.map(|a| a.first_ms),
            "open_minutes": a.map(|a| (now - a.first_ms) as f64 / 60_000.0),
            "timeout_min": a.map(|a| a.timeout_min).filter(|t| t.is_finite() && *t < f64::MAX),
            "last_close": r.last_close.map(|c| json!({"reason": c.0, "ts": c.1})),
            "auto_opens": r.auto_opens,
            "reopens": r.reopens,
            "suppressed": r.suppressed,
        })
    }

    pub fn metrics(&self) -> String {
        use std::fmt::Write;
        let mut out = String::new();
        for (name, help, kind) in [("heatd_window_state", "window detection state per room (0 closed, 1 open, 2 probe)", "gauge"), ("heatd_window_auto_opens_total", "automatic window openings per room since start", "counter"), ("heatd_window_reopens_total", "window reopenings after a failed timeout probe per room since start", "counter"), ("heatd_window_suppressed_total", "suppressed window detection episodes per room since start", "counter")] {
            let _ = writeln!(out, "# HELP {name} {help}\n# TYPE {name} {kind}");
            for (room, r) in &self.rooms {
                let v = match name {
                    "heatd_window_state" => r.active.as_ref().map_or(0, |a| if a.phase == Phase::Open { 1 } else { 2 }),
                    "heatd_window_auto_opens_total" => r.auto_opens,
                    "heatd_window_reopens_total" => r.reopens,
                    _ => r.suppressed,
                };
                let _ = writeln!(out, "{name}{{room=\"{room}\"}} {v}");
            }
        }
        out
    }
}
