use crate::config::{Config, WindowSection};
use serde_json::{json, Value};
use std::collections::{BTreeMap, HashMap, HashSet, VecDeque};

const MIN_MS: i64 = 60_000;
const KEEP_MS: i64 = 3 * 3_600_000;
const SUPPRESS_LOG_MS: i64 = 15 * MIN_MS;
const MIN_SAMPLES: usize = 3;

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
}

#[derive(Debug, Default)]
struct NodeWin {
    temp: Option<f64>,
    valve: Option<f64>,
    closed_ms: Option<i64>,
    valve_before: Option<f64>,
    cool_start: Option<(i64, f64)>,
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

    fn at(&self, ms: i64) -> Option<f64> {
        self.series.iter().rev().find(|s| s.0 <= ms && s.0 > self.floor_ms).map(|s| s.1)
    }

    fn within(&self, from_ms: i64, now: i64) -> Vec<(i64, f64)> {
        let mut out: Vec<(i64, f64)> = self.at(from_ms).map(|t| (from_ms, t)).into_iter().collect();
        out.extend(self.samples().into_iter().filter(|s| s.0 >= from_ms && s.0 <= now));
        out
    }

    fn reading(&self, cfg: &WindowSection, now: i64, factor: f64) -> Option<Reading> {
        let temp = self.samples().last()?.1;
        let min = |m: f64| now - (m * MIN_MS as f64) as i64;
        let peak = self.within(min(cfg.open_window_min), now).iter().map(|s| s.1).fold(temp, f64::max);
        let mut points = self.within(min(cfg.window_slope_min), now);
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

impl Windows {
    pub fn observe(&mut self, config: &Config, node: u64, attribute: &str, value: f64, ts_ms: i64) {
        let Some(room) = config.rooms.iter().find(|r| r.nodes.contains(&node)) else { return };
        let entry = self.rooms.entry(room.name.clone()).or_default();
        let busy = entry.active.is_some();
        let n = self.nodes.entry(node).or_default();
        match attribute {
            LOCAL_TEMPERATURE => {
                n.temp = Some(value);
                let temps: Vec<f64> = room.nodes.iter().filter_map(|id| self.nodes.get(id).and_then(|n| n.temp)).collect();
                entry.series.push_back((ts_ms, temps.iter().sum::<f64>() / temps.len() as f64));
                while entry.series.front().is_some_and(|s| s.0 < ts_ms - KEEP_MS) {
                    entry.series.pop_front();
                }
            }
            PI_HEATING_DEMAND => {
                let before = n.valve.filter(|v| *v > 0.0);
                if let (Some(v), true) = (before, value <= 0.0) {
                    n.closed_ms = Some(ts_ms);
                    n.valve_before = Some(v);
                    n.cool_start = n.temp.filter(|_| !busy).map(|t| (ts_ms, t));
                } else if value > 0.0 {
                    n.closed_ms = None;
                    n.cool_start = None;
                }
                n.valve = Some(value);
            }
            _ => {}
        }
    }

    pub fn note_decrease(&mut self, room: &str, ts_ms: i64) {
        self.rooms.entry(room.into()).or_default().decrease_ms = Some(ts_ms);
    }

    #[cfg(test)]
    pub fn seed_cooldown(&mut self, node: u64, ts_ms: i64, rate: f64) {
        self.nodes.entry(node).or_default().samples.push_back((ts_ms, rate));
    }

    pub fn open_rooms(&self) -> HashSet<String> {
        self.rooms.iter().filter(|(_, r)| r.active.as_ref().is_some_and(|a| a.phase == Phase::Open)).map(|(n, _)| n.clone()).collect()
    }

    pub fn manual(&mut self, room: &str, open: bool, now: i64) -> Option<WinEvent> {
        let r = self.rooms.entry(room.into()).or_default();
        if open {
            if r.active.as_ref().is_some_and(|a| a.phase == Phase::Open && a.source == "manual") {
                return None;
            }
            let first = r.active.as_ref().map_or(now, |a| a.first_ms);
            r.active = Some(Active { phase: Phase::Open, source: "manual", first_ms: first, since_ms: now, timeout_min: f64::MAX });
            r.min_temp = r.series.back().map(|s| s.1);
            return Some(WinEvent { kind: "window_open", room: room.into(), detail: json!({"room": room, "source": "manual"}) });
        }
        let active = r.active.clone()?;
        let detail = json!({"room": room, "reason": "manual", "open_minutes": (now - active.first_ms) as f64 / 60_000.0, "min_temp": r.min_temp});
        r.close("manual", now);
        Some(WinEvent { kind: "window_close", room: room.into(), detail })
    }

    fn typical(&self, cfg: &WindowSection, nodes: &[u64], now: i64) -> f64 {
        let horizon = now - (cfg.cooldown_days * 86_400_000.0) as i64;
        let rates: Vec<f64> = nodes
            .iter()
            .filter(|id| self.nodes.get(id).is_some_and(|n| n.closed_ms.is_some_and(|c| now - c <= (cfg.cooldown_grace_min * MIN_MS as f64) as i64)))
            .map(|id| {
                let s: Vec<f64> = self.nodes[id].samples.iter().filter(|s| s.0 >= horizon).map(|s| s.1).collect();
                if s.len() >= MIN_SAMPLES { median(s) } else { cfg.fallback_cooldown_k_per_h }
            })
            .collect();
        rates.into_iter().fold(0.0, f64::max)
    }

    fn learn(&mut self, cfg: &WindowSection, config: &Config, now: i64) {
        let span = (cfg.cooldown_sample_min * MIN_MS as f64) as i64;
        let horizon = now - (cfg.cooldown_days * 86_400_000.0) as i64;
        for room in &config.rooms {
            let busy = self.rooms.get(&room.name).is_some_and(|w| w.active.is_some());
            for id in &room.nodes {
                let Some(n) = self.nodes.get_mut(id) else { continue };
                if busy {
                    n.cool_start = None;
                    continue;
                }
                while n.samples.front().is_some_and(|s| s.0 < horizon) {
                    n.samples.pop_front();
                }
                if let (Some((t0, temp0)), Some(temp)) = (n.cool_start, n.temp) {
                    if now - t0 >= span && n.valve.is_some_and(|v| v <= 0.0) {
                        n.cool_start = None;
                        let rate = (temp0 - temp) / (span as f64 / 3_600_000.0);
                        if rate > 0.0 {
                            n.samples.push_back((now, rate));
                        }
                    }
                }
            }
        }
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
        self.learn(cfg, config, now);
        let mut events = Vec::new();
        for room in config.rooms.iter().filter(|r| !r.nodes.is_empty()) {
            let Some(mut w) = self.rooms.remove(&room.name) else { continue };
            let evs = self.room_step(cfg, &room.name, &room.nodes, &mut w, now, outdoor);
            events.extend(evs);
            self.rooms.insert(room.name.clone(), w);
        }
        events
    }

    fn room_step(&self, cfg: &WindowSection, name: &str, nodes: &[u64], w: &mut RoomWin, now: i64, outdoor: Option<(f64, Option<f64>)>) -> Vec<WinEvent> {
        let mins = |m: f64| (m * MIN_MS as f64) as i64;
        let event = |kind: &'static str, detail: Value| WinEvent { kind, room: name.into(), detail };
        let Some(cur) = w.series.back().map(|s| s.1) else { return Vec::new() };
        let Some(active) = w.active.clone() else {
            return self.detect(cfg, name, nodes, w, now, outdoor);
        };
        if active.source == "manual" {
            return Vec::new();
        }
        let since = active.since_ms;
        let seen: Vec<(i64, f64)> = w.samples().into_iter().filter(|s| s.0 >= since).collect();
        let low = seen.iter().map(|s| s.1).fold(cur, f64::min);
        w.min_temp = Some(w.min_temp.map_or(low, |m| m.min(low)));
        let closed = |reason: &'static str, w: &mut RoomWin| {
            let detail = json!({"room": name, "reason": reason, "open_minutes": (now - active.first_ms) as f64 / 60_000.0, "min_temp": w.min_temp});
            w.close(reason, now);
            vec![event("window_close", detail)]
        };
        match active.phase {
            Phase::Open => {
                if now - since >= mins(cfg.min_open_min) {
                    let from = (now - mins(cfg.close_window_min)).max(since);
                    let recent: Vec<f64> = w.within(from, now).iter().map(|s| s.1).collect();
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
                let peak = w.within(since, now).iter().map(|s| s.1).fold(cur, f64::max);
                if peak - cur >= cfg.probe_drop_k && self.heating(nodes) != Some(false) {
                    let timeout = (active.timeout_min * 2.0).min(cfg.max_timeout_min);
                    w.active = Some(Active { phase: Phase::Open, since_ms: now, timeout_min: timeout, ..active });
                    w.reopens += 1;
                    return vec![event("window_reopen", json!({"room": name, "drop_k": peak - cur, "timeout_min": timeout, "open_minutes": (now - active.first_ms) as f64 / 60_000.0}))];
                }
                if now - since >= mins(cfg.probe_min) {
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
        let grace = mins(cfg.cooldown_grace_min);
        let cooling = nodes.iter().any(|id| self.nodes.get(id).is_some_and(|n| n.closed_ms.is_some_and(|c| now - c <= grace)));
        let guard = self.typical(cfg, nodes, now) * cfg.expected_cooldown_factor;
        let fast = |rate: f64| !cooling || rate > guard;
        let by_drop = r.hit_drop && fast(r.drop_k / (cfg.open_window_min / 60.0));
        let by_slope = sustained && fast(-r.slope);
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
        let valve_before = self.valve_before(nodes, now, grace);
        let detail = json!({"room": name, "drop_k": r.drop_k, "slope": r.slope, "outdoor": outdoor.map(|o| o.0), "wind": wind, "valve_before": valve_before});
        let Some(why) = blocked else {
            w.active = Some(Active { phase: Phase::Open, source: "auto", first_ms: now, since_ms: now, timeout_min: cfg.max_open_min });
            w.opens.push_back(now);
            w.auto_opens += 1;
            w.slope_since = None;
            w.min_temp = Some(r.temp);
            let mut detail = detail;
            detail["source"] = json!("auto");
            return vec![WinEvent { kind: "window_open", room: name.into(), detail }];
        };
        w.suppressed += 1;
        if w.logged.get(why).is_some_and(|t| now - t < SUPPRESS_LOG_MS) {
            return Vec::new();
        }
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
        for (name, help, kind) in [("heatd_window_state", "window detection state per room (0 closed, 1 open, 2 probe)", "gauge"), ("heatd_window_auto_opens_total", "automatic window openings per room since start", "counter"), ("heatd_window_reopens_total", "window reopenings after a failed timeout probe per room since start", "counter"), ("heatd_window_suppressed_total", "suppressed window detections per room since start", "counter")] {
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
