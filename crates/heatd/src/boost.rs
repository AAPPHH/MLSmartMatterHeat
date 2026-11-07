use crate::boostlearn::{ramp, BoostModel, Context, Decision};
use crate::config::{BoostSection, Config};
use crate::explore::Rng;
use crate::lake::{kind, Event};
use anyhow::{anyhow, Result};
use serde_json::{json, Value};
use std::collections::{HashMap, HashSet, VecDeque};
use std::path::Path;
use std::sync::{Arc, Mutex};

pub const MORNING: &str = "morning";
pub const HOMECOMING: &str = "homecoming";
pub const MANUAL: &str = "manual";
const MIN_MS: i64 = 60_000;
const KEEP_MS: i64 = 8 * 3_600_000;
const INPUT_FRESH_MS: i64 = 150_000;
const ENV_FRESH_MS: i64 = 600_000;
const TARGET_EPS_C: f64 = 0.05;
const KNOWN_OVERSHOOT_K: f64 = 0.5;
const MIN_HEADROOM_K: f64 = 1.0;
const MIN_MANUAL_MIN: f64 = 0.5;
const DEFAULT_MANUAL_MIN: f64 = 5.0;
const MAX_MANUAL_MIN: f64 = 20.0;

fn value_at(points: &VecDeque<(i64, f64)>, t_ms: i64) -> Option<f64> {
    let i = points.partition_point(|p| p.0 <= t_ms);
    i.checked_sub(1).map(|j| points[j].1).or_else(|| points.front().map(|p| p.1))
}

type Series<'a> = (&'a str, &'a str, &'a str, &'a dyn Fn(&RoomState) -> f64);

#[derive(Clone, Debug)]
pub struct RoomIn {
    pub room: String,
    pub setpoint: f64,
    pub blocked: Option<&'static str>,
    pub no_raise: bool,
    pub no_supply: bool,
}

pub struct StepIn<'a> {
    pub now_ms: i64,
    pub hour: f64,
    pub outdoor: Option<(f64, Option<f64>)>,
    pub radiation: Option<f64>,
    pub presence_known: bool,
    pub sleep: bool,
    pub away: bool,
    pub rooms: &'a [RoomIn],
}

#[derive(Debug, PartialEq)]
pub enum Directive {
    Boost { experiment_id: String, setpoint: f64 },
    Switch,
}

#[derive(Default)]
pub struct StepOut {
    pub directives: HashMap<String, Directive>,
    pub events: Vec<Event>,
}

#[derive(Clone, Debug)]
struct Last {
    ts_ms: i64,
    scenario: String,
    source: &'static str,
    boosted: bool,
    reason: String,
    decision: Option<Decision>,
}

struct Cycle {
    id: String,
    scenario: String,
    source: &'static str,
    decided_ms: i64,
    written_ms: Option<i64>,
    switch_ms: Option<i64>,
    target: f64,
    old: f64,
    start_temp: f64,
    minutes: f64,
    setpoint: f64,
    decision: Decision,
    ctx: Context,
    version: String,
    radiation: f64,
    peak: f64,
    reached_ms: Option<i64>,
}

struct RoomState {
    name: String,
    nodes: Vec<u64>,
    sensor: VecDeque<(i64, f64)>,
    valve: VecDeque<(i64, f64)>,
    last_desired: Option<f64>,
    last_in: Option<(i64, RoomIn)>,
    was_no_supply: bool,
    resume_ms: Option<i64>,
    cycle: Option<Cycle>,
    last: Option<Last>,
    cycles_total: u64,
    refused_total: u64,
}

struct Trigger {
    scenario: &'static str,
    until_ms: i64,
    base: HashMap<String, f64>,
    done: HashSet<String>,
}

#[derive(Default)]
struct Env {
    out: Option<f64>,
    wind: f64,
    rad: f64,
    hour: f64,
    ms: i64,
}

struct Inner {
    cfg: BoostSection,
    rooms: Vec<RoomState>,
    model: Option<Arc<BoostModel>>,
    dir_name: Option<String>,
    rng: Rng,
    env: Env,
    prev_sleep: Option<bool>,
    prev_away: Option<bool>,
    trigger: Option<Trigger>,
    pending: Vec<Event>,
    stale_ms: i64,
}

#[derive(Clone)]
pub struct Boost {
    inner: Arc<Mutex<Inner>>,
    wake: Arc<tokio::sync::Notify>,
}

impl Default for Boost {
    fn default() -> Self {
        Self::new(&Config::default())
    }
}

impl Boost {
    pub fn new(config: &Config) -> Self {
        let cfg = config.boost.clone();
        let rooms = config.rooms.iter().map(|r| RoomState { name: r.name.clone(), nodes: r.nodes.clone(), sensor: VecDeque::new(), valve: VecDeque::new(), last_desired: None, last_in: None, was_no_supply: false, resume_ms: None, cycle: None, last: None, cycles_total: 0, refused_total: 0 }).collect();
        let seed = cfg.seed.or(config.exploration_seed).unwrap_or_else(|| crate::lake::now_us() as u64);
        let inner = Inner { cfg, rooms, model: None, dir_name: None, rng: Rng::new(seed ^ 0xB005), env: Env::default(), prev_sleep: None, prev_away: None, trigger: None, pending: Vec::new(), stale_ms: config.stale.after_ms() };
        let boost = Self { inner: Arc::new(Mutex::new(inner)), wake: Arc::new(tokio::sync::Notify::new()) };
        if config.boost.enabled {
            if let Err(e) = boost.reload() {
                eprintln!("boost: no boost bundle loaded, no boosts until one appears: {e:#}");
            }
        }
        boost
    }

    pub fn enabled(&self) -> bool {
        self.inner.lock().unwrap().cfg.enabled
    }

    pub fn waker(&self) -> Arc<tokio::sync::Notify> {
        self.wake.clone()
    }

    pub fn reload(&self) -> Result<bool> {
        let dir = self.inner.lock().unwrap().cfg.model_dir.clone();
        let current = self.inner.lock().unwrap().dir_name.clone();
        for (name, path) in crate::window_model::bundle_candidates(Path::new(&dir)) {
            if current.as_deref() == Some(name.as_str()) {
                return Ok(false);
            }
            match BoostModel::load(&path) {
                Ok(model) => {
                    eprintln!("boost: loaded boost bundle {}", model.version());
                    let mut inner = self.inner.lock().unwrap();
                    inner.model = Some(Arc::new(model));
                    inner.dir_name = Some(name);
                    return Ok(true);
                }
                Err(e) => eprintln!("boost: bundle {name} rejected: {e:#}"),
            }
        }
        Err(anyhow!("no valid boost bundle in {dir}"))
    }

    #[cfg(test)]
    pub fn set_model(&self, model: BoostModel) {
        self.inner.lock().unwrap().model = Some(Arc::new(model));
    }

    pub fn observe(&self, node: u64, attribute: &str, value: f64, ts_ms: i64) {
        let mut inner = self.inner.lock().unwrap();
        let Some(rs) = inner.rooms.iter_mut().find(|r| r.nodes.first() == Some(&node)) else { return };
        match attribute {
            crate::window::LOCAL_TEMPERATURE if value.is_finite() => {
                rs.sensor.push_back((ts_ms, value));
                if let Some(c) = rs.cycle.as_mut().filter(|c| c.written_ms.is_some_and(|w| ts_ms >= w)) {
                    c.peak = c.peak.max(value);
                    if c.reached_ms.is_none() && value >= c.target - REACH_C {
                        c.reached_ms = Some(ts_ms);
                    }
                }
            }
            crate::window::PI_HEATING_DEMAND if (0.0..=100.0).contains(&value) => rs.valve.push_back((ts_ms, value)),
            _ => return,
        }
        for q in [&mut rs.sensor, &mut rs.valve] {
            while q.len() > 1 && q.front().is_some_and(|p| p.0 < ts_ms - KEEP_MS) {
                q.pop_front();
            }
        }
    }

    pub fn wrote(&self, room: &str, reason: &str, now_ms: i64) {
        if reason != "boost" {
            return;
        }
        let mut inner = self.inner.lock().unwrap();
        if let Some(c) = inner.rooms.iter_mut().find(|r| r.name == room).and_then(|r| r.cycle.as_mut()).filter(|c| c.written_ms.is_none()) {
            c.written_ms = Some(now_ms);
        }
    }

    pub fn settle(&self, finals: &[(&str, &str)], now_ms: i64) {
        let mut inner = self.inner.lock().unwrap();
        for (room, _) in finals.iter().filter(|(_, r)| *r != "boost") {
            let Some(idx) = inner.rooms.iter().position(|r| r.name == *room) else { continue };
            if inner.rooms[idx].cycle.as_ref().is_some_and(|c| c.switch_ms.is_none()) {
                let mut out = StepOut::default();
                inner.finish(idx, now_ms, "overridden", &mut out);
                inner.pending.extend(out.events);
            }
        }
    }

    pub fn next_deadline_ms(&self) -> Option<i64> {
        let inner = self.inner.lock().unwrap();
        inner.rooms.iter().filter_map(|r| r.cycle.as_ref()).filter_map(|c| c.written_ms.filter(|_| c.switch_ms.is_none()).map(|w| w + (c.minutes * MIN_MS as f64) as i64)).min()
    }

    pub fn step(&self, input: &StepIn) -> StepOut {
        let mut inner = self.inner.lock().unwrap();
        let mut out = StepOut { events: std::mem::take(&mut inner.pending), ..Default::default() };
        if !inner.cfg.enabled {
            return out;
        }
        inner.env = Env { out: input.outdoor.map(|o| o.0).or(inner.env.out.filter(|_| input.now_ms - inner.env.ms <= ENV_FRESH_MS)), wind: input.outdoor.and_then(|o| o.1).unwrap_or(inner.cfg.wind_default), rad: input.radiation.unwrap_or(0.0), hour: input.hour, ms: input.now_ms };
        inner.detect_trigger(input);
        for want in input.rooms {
            if let Some(idx) = inner.rooms.iter().position(|r| r.name == want.room) {
                inner.step_room(idx, want, input.now_ms, &mut out);
            }
        }
        if inner.trigger.as_ref().is_some_and(|t| input.now_ms > t.until_ms) {
            inner.trigger = None;
        }
        out
    }

    pub fn request(&self, room: &str, minutes: Option<f64>, now_ms: i64) -> Value {
        let mut inner = self.inner.lock().unwrap();
        let scenario = MANUAL.to_string();
        let Some(idx) = inner.rooms.iter().position(|r| r.name == room) else {
            return json!({"ok": false, "room": room, "scenario": scenario, "reason": format!("unbekannter Raum {room}")});
        };
        let result = if minutes.is_some_and(|m| !m.is_finite() || m <= 0.0) { Err("Minuten muessen groesser als 0 sein".to_string()) } else { inner.open(idx, MANUAL, "manual", minutes, now_ms, true) };
        if let Err(reason) = &result {
            let event = inner.refusal_event(idx, now_ms, reason);
            inner.pending.push(event);
        }
        let mut view = inner.view(idx);
        view["ok"] = json!(result.is_ok());
        view["room"] = json!(room);
        if let Err(reason) = result {
            view["reason"] = json!(reason);
        } else {
            self.wake.notify_one();
        }
        view
    }

    pub fn view(&self, room: &str) -> Value {
        let inner = self.inner.lock().unwrap();
        inner.rooms.iter().position(|r| r.name == room).map_or(Value::Null, |idx| inner.view(idx))
    }

    pub fn summary(&self) -> Value {
        let inner = self.inner.lock().unwrap();
        json!({"enabled": inner.cfg.enabled, "model_version": inner.model.as_ref().map(|m| m.version().to_string()), "trigger": inner.trigger.as_ref().map(|t| json!({"scenario": t.scenario, "until_ms": t.until_ms}))})
    }

    pub fn metrics(&self) -> String {
        let inner = self.inner.lock().unwrap();
        let mut md = format!("# HELP heatd_boost_model_loaded 1 while a boost bundle is loaded\n# TYPE heatd_boost_model_loaded gauge\nheatd_boost_model_loaded {}\n", u8::from(inner.model.is_some()));
        let series: [Series; 5] = [
            ("heatd_boost_active", "gauge", "1 while a boost cycle of the room is armed, boosting or measuring", &|r| f64::from(u8::from(r.cycle.is_some()))),
            ("heatd_boost_allowed_minutes", "gauge", "allowed boost dose of the last decision", &|r| r.last.as_ref().and_then(|l| l.decision.as_ref()).map_or(0.0, |d| d.allowed)),
            ("heatd_boost_chosen_minutes", "gauge", "chosen boost dose of the last decision", &|r| r.last.as_ref().and_then(|l| l.decision.as_ref()).map_or(0.0, |d| d.chosen)),
            ("heatd_boost_cycles_total", "counter", "boost cycles started since start", &|r| r.cycles_total as f64),
            ("heatd_boost_refused_total", "counter", "boost requests and triggers refused since start", &|r| r.refused_total as f64),
        ];
        for (name, kind, help, value) in series {
            md.push_str(&format!("# HELP {name} {help}\n# TYPE {name} {kind}\n"));
            inner.rooms.iter().for_each(|r| md.push_str(&format!("{name}{{room=\"{}\"}} {}\n", r.name, value(r))));
        }
        md
    }
}

const REACH_C: f64 = 0.3;

impl Inner {
    fn in_morning(&self, hour: f64) -> bool {
        (self.cfg.morning_hours[0]..self.cfg.morning_hours[1]).contains(&hour)
    }

    fn detect_trigger(&mut self, input: &StepIn) {
        if !input.presence_known {
            return;
        }
        let found = if self.prev_away == Some(true) && !input.away {
            Some(HOMECOMING)
        } else if self.prev_sleep == Some(true) && !input.sleep && !input.away && self.in_morning(input.hour) {
            Some(MORNING)
        } else {
            None
        };
        (self.prev_sleep, self.prev_away) = (Some(input.sleep), Some(input.away));
        if let Some(scenario) = found {
            let base = self.rooms.iter().filter_map(|r| r.last_desired.map(|d| (r.name.clone(), d))).collect();
            self.trigger = Some(Trigger { scenario, until_ms: input.now_ms + (self.cfg.trigger_window_min * MIN_MS as f64) as i64, base, done: HashSet::new() });
        }
    }

    fn u_at(&self, idx: usize, model: &BoostModel, t_ms: i64) -> f64 {
        let rs = &self.rooms[idx];
        let dz = model.dead_zone();
        let u = ((value_at(&rs.valve, t_ms).unwrap_or(0.0) / 100.0 - dz) / (1.0 - dz)).clamp(0.0, 1.0);
        match rs.resume_ms.filter(|r| t_ms >= *r) {
            Some(r) => u * ramp(((t_ms - r) as f64 / MIN_MS as f64).min(self.cfg.resume_cap_min), model.ramp_tau()),
            None => u,
        }
    }

    fn context(&self, idx: usize, model: &BoostModel, target: f64, temp: f64, now_ms: i64) -> Context {
        let rs = &self.rooms[idx];
        let minutes = |from: i64, to: i64| (from..=to).map(|i| self.u_at(idx, model, now_ms - i * MIN_MS)).sum::<f64>();
        let cap = model.since_open_cap();
        let since_open = (1..=cap as i64).find(|i| self.u_at(idx, model, now_ms - i * MIN_MS) > 0.0).map_or(cap, |i| (i - 1) as f64);
        let since_resume = rs.resume_ms.map_or(self.cfg.resume_cap_min, |r| ((now_ms - r) as f64 / MIN_MS as f64).clamp(0.0, self.cfg.resume_cap_min));
        Context {
            out: self.env.out.unwrap_or_default(),
            wind: self.env.wind,
            rad: self.env.rad,
            start_temp: temp,
            gap: target - temp,
            supply_ok: f64::from(u8::from(!rs.was_no_supply)),
            since_resume: if rs.was_no_supply { 0.0 } else { since_resume },
            vm60: minutes(1, 60),
            vm180: minutes(1, 180),
            since_open: Some(since_open),
            hour: self.env.hour,
        }
    }

    fn open(&mut self, idx: usize, scenario: &str, source: &'static str, requested: Option<f64>, now_ms: i64, manual: bool) -> Result<(), String> {
        let result = self.try_open(idx, scenario, source, requested, now_ms, manual);
        let rs = &mut self.rooms[idx];
        match &result {
            Ok((reason, decision)) => rs.last = Some(Last { ts_ms: now_ms, scenario: scenario.into(), source, boosted: true, reason: reason.clone(), decision: Some(decision.clone()) }),
            Err((reason, decision)) => {
                rs.refused_total += 1;
                rs.last = Some(Last { ts_ms: now_ms, scenario: scenario.into(), source, boosted: false, reason: reason.clone(), decision: decision.as_deref().cloned() });
            }
        }
        result.map(|_| ()).map_err(|e| e.0)
    }

    #[allow(clippy::type_complexity)]
    fn try_open(&mut self, idx: usize, scenario: &str, source: &'static str, requested: Option<f64>, now_ms: i64, manual: bool) -> Result<(String, Decision), (String, Option<Box<Decision>>)> {
        let refuse = |s: &str| (s.to_string(), None);
        let model = self.model.clone().ok_or_else(|| refuse("kein Boost-Modell geladen"))?;
        let rs = &self.rooms[idx];
        let (seen_ms, want) = rs.last_in.clone().ok_or_else(|| refuse("noch kein Regelschritt seit dem Start"))?;
        if manual && now_ms - seen_ms > INPUT_FRESH_MS {
            return Err(refuse("kein aktueller Regelschritt, bitte spaeter erneut versuchen"));
        }
        if rs.cycle.is_some() {
            return Err(refuse("im Raum laeuft bereits ein Boost"));
        }
        if !model.has_room(&rs.name) {
            return Err(refuse("Raum ist nicht im Boost-Modell"));
        }
        if let Some(b) = want.blocked {
            return Err((format!("Raum ist blockiert ({b})"), None));
        }
        if want.no_raise {
            return Err(refuse("Anhebungs-Budget des Raums in dieser Stunde verbraucht"));
        }
        let Some(temp) = rs.sensor.back().filter(|s| now_ms - s.0 <= self.stale_ms).map(|s| s.1) else { return Err(refuse("kein frischer Temperaturwert")) };
        if self.env.out.is_none() {
            return Err(refuse("keine Aussentemperatur"));
        }
        if rs.valve.is_empty() {
            return Err(refuse("noch kein Ventilwert seit dem Start"));
        }
        let target = want.setpoint;
        if temp >= target - REACH_C {
            return Err(refuse("Raum ist schon am Ziel"));
        }
        if model.boost_setpoint() - target < MIN_HEADROOM_K || temp >= model.abort_c() {
            return Err(refuse("Ziel liegt zu nah am Boost-Sollwert"));
        }
        let ctx = self.context(idx, &model, target, temp, now_ms);
        let hist: Vec<f64> = (0..model.kernel_len() as i64).map(|i| self.u_at(idx, &model, now_ms - (model.kernel_len() as i64 - i) * MIN_MS)).collect();
        let u = self.rng.unit();
        let (decision, minutes) = if manual {
            if ctx.supply_ok <= 0.0 {
                return Err(refuse("Heizungsversorgung unterbrochen, kein Boost"));
            }
            let minutes = requested.unwrap_or(DEFAULT_MANUAL_MIN).clamp(MIN_MANUAL_MIN, MAX_MANUAL_MIN);
            (model.manual(&rs.name, &ctx, target, Some(&hist), minutes).map_err(|e| (e.to_string(), None))?, minutes)
        } else {
            let decision = model.decide(&rs.name, &ctx, scenario, target, Some(&hist), &|n| ((u * n as f64) as usize).min(n.saturating_sub(1))).map_err(|e| (e.to_string(), None))?;
            if decision.allowed <= 0.0 {
                return Err((explain(&model, &decision, scenario, &ctx), Some(Box::new(decision))));
            }
            let minutes = decision.chosen;
            (decision, minutes)
        };
        let reason = format!(
            "Boost {scenario} {minutes:.1} min auf {:.1} C (erlaubt {:.1}, sicher bis {:.1}, bekannt {}, Risiko {:.2})",
            model.boost_setpoint(),
            decision.allowed,
            decision.d_safe,
            decision.known.map_or("nichts".to_string(), |k| format!("{k:.1}")),
            decision.p_overshoot
        );
        let id = format!("boost-{}-{now_ms}", rs.name);
        let cycle = Cycle {
            id,
            scenario: scenario.into(),
            source,
            decided_ms: now_ms,
            written_ms: None,
            switch_ms: None,
            target,
            old: self.rooms[idx].last_desired.unwrap_or(target),
            start_temp: temp,
            minutes,
            setpoint: model.boost_setpoint(),
            decision: decision.clone(),
            ctx,
            version: model.version().to_string(),
            radiation: self.env.rad,
            peak: temp,
            reached_ms: None,
        };
        let rs = &mut self.rooms[idx];
        rs.cycle = Some(cycle);
        rs.cycles_total += 1;
        Ok((reason, decision))
    }

    fn step_room(&mut self, idx: usize, want: &RoomIn, now_ms: i64, out: &mut StepOut) {
        let name = want.room.clone();
        {
            let rs = &mut self.rooms[idx];
            rs.last_in = Some((now_ms, want.clone()));
            if rs.was_no_supply && !want.no_supply {
                rs.resume_ms = Some(now_ms);
            }
            rs.was_no_supply = want.no_supply;
        }
        let qualified = self.trigger.as_ref().filter(|t| now_ms <= t.until_ms && !t.done.contains(&name)).and_then(|t| t.base.get(&name).filter(|b| want.setpoint - **b >= self.cfg.min_rise_k - 1e-9).map(|_| t.scenario));
        self.rooms[idx].last_desired = Some(want.setpoint);
        if self.rooms[idx].cycle.is_some() {
            self.progress(idx, want, now_ms, out);
        } else if let Some(scenario) = qualified {
            if let Some(t) = self.trigger.as_mut() {
                t.done.insert(name.clone());
            }
            let result = self.open(idx, scenario, "auto", None, now_ms, false);
            if let Err(reason) = result {
                out.events.push(self.refusal_event(idx, now_ms, &reason));
            }
        }
        if let Some(c) = self.rooms[idx].cycle.as_ref().filter(|c| c.switch_ms.is_none()) {
            out.directives.entry(name).or_insert(Directive::Boost { experiment_id: c.id.clone(), setpoint: c.setpoint });
        }
    }

    fn progress(&mut self, idx: usize, want: &RoomIn, now_ms: i64, out: &mut StepOut) {
        let name = want.room.clone();
        let abort_c = self.model.as_ref().map_or(f64::MAX, |m| m.abort_c());
        let sensor = self.rooms[idx].sensor.back().copied();
        let stale = sensor.is_none_or(|s| now_ms - s.0 > self.stale_ms);
        let Some(c) = self.rooms[idx].cycle.as_ref() else { return };
        let boosting = c.switch_ms.is_none();
        let abort: Option<String> = if let Some(b) = want.blocked {
            Some(b.to_string())
        } else if stale {
            Some("sensor_stale".into())
        } else if want.setpoint < c.target - TARGET_EPS_C {
            Some("target_decrease".into())
        } else if (want.setpoint - c.target).abs() >= TARGET_EPS_C {
            Some("target_changed".into())
        } else if want.no_supply {
            Some("no_supply".into())
        } else if boosting && sensor.is_some_and(|s| s.1 >= abort_c) {
            Some("sensor_limit".into())
        } else {
            None
        };
        if let Some(why) = abort {
            let hand_back = boosting && c.written_ms.is_some() && want.blocked.is_none();
            self.finish(idx, now_ms, &why, out);
            if hand_back {
                out.directives.insert(name, Directive::Switch);
            }
            return;
        }
        let measure_ms = (self.cfg.measure_min * MIN_MS as f64) as i64;
        let arm_ms = (self.cfg.arm_timeout_min * MIN_MS as f64) as i64;
        let Some(c) = self.rooms[idx].cycle.as_mut() else { return };
        match (c.written_ms, c.switch_ms) {
            (None, _) if now_ms - c.decided_ms > arm_ms => self.finish(idx, now_ms, "not_written", out),
            (Some(w), None) if now_ms - w >= (c.minutes * MIN_MS as f64) as i64 => {
                c.switch_ms = Some(now_ms);
                out.directives.insert(name, Directive::Switch);
            }
            (Some(_), Some(s)) if now_ms - s >= measure_ms => self.finish(idx, now_ms, "complete", out),
            _ => {}
        }
    }

    fn finish(&mut self, idx: usize, now_ms: i64, why: &str, out: &mut StepOut) {
        let Some(c) = self.rooms[idx].cycle.take() else { return };
        let rs = &self.rooms[idx];
        let node = rs.nodes.first().copied();
        let written = c.written_ms;
        let applied = written.map_or(0.0, |w| (c.switch_ms.unwrap_or(now_ms) - w) as f64 / MIN_MS as f64);
        let overshoot = (c.peak - c.target).max(0.0);
        let invalid = matches!(why, "no_supply" | "overridden");
        let measured = written.is_some() && !invalid && (why == "complete" || overshoot > KNOWN_OVERSHOOT_K);
        let mode = if written.is_some() { "live" } else { "aborted" };
        let detail = json!({
            "room": rs.name, "variant": "boost_learn", "mode": mode, "cycle_id": c.id, "start_ts_ms": written.unwrap_or(c.decided_ms), "decided_ts_ms": c.decided_ms, "target": c.target, "setpoint_old": c.old, "boost_setpoint": c.setpoint,
            "start_temp": c.start_temp, "scenario": c.scenario, "source": c.source, "boost_min": applied, "chosen_min": c.minutes, "allowed_min": c.decision.allowed, "d_safe_min": c.decision.d_safe, "known_min": c.decision.known,
            "n_similar": c.decision.n_similar, "p_overshoot": c.decision.p_overshoot, "p_at_allowed": c.decision.p_at_allowed, "p_no_boost": c.decision.p_no_boost, "decision": c.decision, "context": c.ctx,
            "peak_c": c.peak, "overshoot_c": measured.then_some(overshoot), "minutes_to_target_0_3": written.zip(c.reached_ms).map(|(w, r)| (r - w) as f64 / MIN_MS as f64),
            "model_version": c.version, "end": why, "elapsed_min": written.map_or(0.0, |w| (now_ms - w) as f64 / MIN_MS as f64), "outdoor_temp": c.ctx.out, "radiation": c.radiation, "exploration": false,
        });
        let mut event = Event::now(kind::BOOST_CYCLE, "heatd").with_detail(detail);
        event.ts = now_ms * 1000;
        event.node = node;
        event.experiment_id = written.map(|_| c.id.clone());
        out.events.push(event);
    }

    fn refusal_event(&self, idx: usize, now_ms: i64, reason: &str) -> Event {
        let rs = &self.rooms[idx];
        let last = rs.last.as_ref();
        let mut event = Event::now(kind::BOOST_CYCLE, "heatd").with_detail(json!({
            "room": rs.name, "variant": "boost_learn", "mode": "refused", "status": "refused", "scenario": last.map(|l| l.scenario.clone()), "source": last.map(|l| l.source), "reason": reason, "decision": last.and_then(|l| l.decision.clone()),
            "model_version": self.model.as_ref().map(|m| m.version().to_string()), "start_ts_ms": now_ms,
        }));
        event.ts = now_ms * 1000;
        event.node = rs.nodes.first().copied();
        event
    }

    fn view(&self, idx: usize) -> Value {
        let rs = &self.rooms[idx];
        let last = rs.last.as_ref();
        let d = last.and_then(|l| l.decision.as_ref());
        let state = match rs.cycle.as_ref().map(|c| (c.written_ms, c.switch_ms)) {
            None => "idle",
            Some((None, _)) => "armed",
            Some((Some(_), None)) => "boosting",
            Some((Some(_), Some(_))) => "measuring",
        };
        let reason = last.map_or_else(|| if self.model.is_some() { "noch keine Boost-Entscheidung".to_string() } else { "kein Boost-Modell geladen".to_string() }, |l| l.reason.clone());
        json!({
            "state": state, "scenario": last.map(|l| l.scenario.clone()), "source": last.map(|l| l.source), "boosted": last.map(|l| l.boosted), "decided_ts": last.map(|l| l.ts_ms),
            "allowed": d.map(|d| d.allowed), "chosen": d.map(|d| d.chosen), "d_safe": d.map(|d| d.d_safe), "known": d.and_then(|d| d.known), "p_overshoot": d.map(|d| d.p_overshoot), "p_no_boost": d.map(|d| d.p_no_boost),
            "reason": reason, "model_version": self.model.as_ref().map(|m| m.version().to_string()),
            "cycle": rs.cycle.as_ref().map(|c| json!({"id": c.id, "minutes": c.minutes, "target": c.target, "setpoint": c.setpoint, "written_ts": c.written_ms, "switch_ts": c.switch_ms, "peak_c": c.peak})),
        })
    }
}

fn explain(model: &BoostModel, d: &Decision, scenario: &str, ctx: &Context) -> String {
    if !model.scenario_allowed(scenario) {
        format!("Szenario {scenario} ist kein Boost-Szenario, geboostet wird nur morgens und bei Heimkehr")
    } else if ctx.supply_ok <= 0.0 {
        "Heizungsversorgung unterbrochen, kein Boost".to_string()
    } else if d.d_safe < model.dose_min() {
        format!("kein sicherer Boost, auch {:.0} min ueberschreiten das Ziel mit zu hoher Wahrscheinlichkeit (Risiko ohne Boost {:.2})", model.dose_min(), d.p_no_boost)
    } else {
        "kein erlaubter Boost bei diesem Kontext".to_string()
    }
}

pub fn spawn_background(boost: Boost) {
    tokio::spawn(async move {
        let reload = std::time::Duration::from_secs_f64(boost.inner.lock().unwrap().cfg.reload_min.max(0.1) * 60.0);
        loop {
            let b = boost.clone();
            let _ = tokio::task::spawn_blocking(move || {
                if let Err(e) = b.reload() {
                    eprintln!("boost: {e:#}");
                }
            })
            .await;
            tokio::time::sleep(reload).await;
        }
    });
}

#[cfg(test)]
#[path = "boost_tests.rs"]
mod tests;
