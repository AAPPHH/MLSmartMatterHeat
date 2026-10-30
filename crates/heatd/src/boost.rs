use crate::config::{BoostSection, Config};
use crate::explore::Rng;
use crate::lake::{kind, Event};
use crate::thermal::*;
use anyhow::{anyhow, Result};
use serde_json::{json, Value};
use std::collections::{BTreeMap, HashMap, VecDeque};
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};

const HISTORY_MIN: usize = 60;
const KEEP_MS: i64 = 3 * 3_600_000;
const STALE_MS: i64 = 30 * 60_000;
const GAP_RESET_MIN: i64 = 360;
const OFF_EPS_MIN: f64 = 1.0;
const GATE_REFRESH_MIN: u64 = 60;

fn value_at(points: &VecDeque<(i64, f64)>, t_ms: i64, interpolate: bool) -> Option<f64> {
    let i = points.partition_point(|p| p.0 <= t_ms);
    let before = i.checked_sub(1).map(|j| points[j]);
    match (before, points.get(i)) {
        (Some((t0, v0)), Some(&(t1, v1))) if interpolate && t1 > t0 => Some(v0 + (v1 - v0) * (t_ms - t0) as f64 / (t1 - t0) as f64),
        (Some((_, v)), _) => Some(v),
        (None, Some(&(_, v))) => Some(v),
        _ => None,
    }
}

fn grid(points: &VecDeque<(i64, f64)>, now_min: i64, interpolate: bool) -> Vec<f64> {
    (0..=HISTORY_MIN as i64).filter_map(|i| value_at(points, (now_min - HISTORY_MIN as i64 + i) * 60_000, interpolate)).collect()
}

#[derive(Clone, Debug)]
struct Rec {
    planned_from_start: Option<f64>,
    peak: Option<f64>,
    ttt: Option<f64>,
    valve50: Option<f64>,
    air: Option<f64>,
    frozen: bool,
    error: Option<String>,
}

#[derive(Clone, Copy, Debug, PartialEq)]
enum Phase {
    Boosting,
    Hold,
}

struct Cycle {
    id: String,
    start_ms: i64,
    target: f64,
    old: f64,
    start_temp: f64,
    live: Option<&'static str>,
    offset_min: f64,
    applied: Option<f64>,
    phase: Phase,
    plans: BTreeMap<&'static str, Rec>,
    samples: Vec<(f64, f64)>,
    last_min: i64,
    reached_min: Option<f64>,
    version: String,
    outdoor: f64,
    radiation: f64,
    others: i64,
    switched: bool,
}

struct Est {
    k: Kalman,
    min: i64,
}

struct RoomState {
    name: String,
    nodes: Vec<u64>,
    sensor: VecDeque<(i64, f64)>,
    valve: VecDeque<(i64, f64)>,
    est: Option<Est>,
    cycle: Option<Cycle>,
    last_desired: Option<f64>,
    dirty: bool,
}

pub struct RoomIn {
    pub room: String,
    pub setpoint: f64,
    pub blocked: Option<&'static str>,
}

pub struct StepIn<'a> {
    pub now_ms: i64,
    pub hour: f64,
    pub outdoor: Option<(f64, Option<f64>)>,
    pub radiation: Option<f64>,
    pub rooms: &'a [RoomIn],
}

#[derive(Debug, PartialEq)]
pub enum Directive {
    Boost { experiment_id: String },
    Switch,
}

#[derive(Default)]
pub struct StepOut {
    pub directives: HashMap<String, Directive>,
    pub events: Vec<Event>,
}

struct Inner {
    cfg: BoostSection,
    rooms: Vec<RoomState>,
    links: Vec<(String, String, String)>,
    bundle: Option<Arc<Bundle>>,
    dir_name: Option<String>,
    ambient: HashMap<String, f64>,
    rng: Rng,
    live: HashMap<String, (String, String)>,
    gate_days: HashMap<String, usize>,
    env: Env,
    hour: Option<f64>,
}

#[derive(Clone)]
pub struct Boost(Arc<Mutex<Inner>>);

impl Default for Boost {
    fn default() -> Self {
        Self::new(&Config::default())
    }
}

fn room_key(inner: &Inner, name: &str) -> Option<String> {
    let bundle = inner.bundle.as_ref()?;
    let rs = inner.rooms.iter().find(|r| r.name == name)?;
    [name.to_string(), format!("{name}_{}", rs.nodes.first()?)].into_iter().find(|k| bundle.rooms.contains_key(k))
}

fn pick_live(inner: &Inner, room: &str, forced: bool) -> Option<(String, String)> {
    let bundle = inner.bundle.as_ref()?;
    let key = room_key(inner, room)?;
    let cycles = bundle.backtest(&key, GREY).1.max(bundle.backtest(&key, RESIDUAL).1);
    let days = inner.gate_days.get(room).copied().unwrap_or(0);
    if !forced && (days < inner.cfg.live_gate_days || cycles < inner.cfg.live_min_backtest_cycles) {
        return None;
    }
    let grey = bundle.backtest(&key, GREY).0;
    let resid = bundle.backtest(&key, RESIDUAL).0.filter(|_| bundle.has_residual());
    let (variant, why) = match (grey, resid) {
        (Some(g), Some(r)) if r < g => (RESIDUAL, format!("backtest rmse residual {r:.3} < grey_box {g:.3}")),
        (Some(g), Some(r)) => (GREY, format!("backtest rmse grey_box {g:.3} <= residual {r:.3}")),
        (Some(g), None) => (GREY, format!("backtest rmse grey_box {g:.3}, residual unavailable")),
        (None, Some(r)) => (RESIDUAL, format!("backtest rmse residual {r:.3}, grey_box without rmse")),
        (None, None) if forced && bundle.rooms[&key].valid => (GREY, "forced by live_rooms_override, no backtest rmse".to_string()),
        (None, None) => return None,
    };
    Some((variant.to_string(), format!("{why}; {days} shadow days, {cycles} backtest cycles")))
}

impl Boost {
    pub fn new(config: &Config) -> Self {
        let cfg = config.boost.clone();
        let rooms = config.rooms.iter().map(|r| RoomState { name: r.name.clone(), nodes: r.nodes.clone(), sensor: VecDeque::new(), valve: VecDeque::new(), est: None, cycle: None, last_desired: None, dirty: false }).collect();
        let links = config.neighbours.iter().map(|n| (n.a.clone(), n.b.clone(), n.kind.clone())).collect();
        let seed = cfg.seed.or(config.exploration_seed).unwrap_or_else(|| crate::lake::now_us() as u64);
        let boost = Self(Arc::new(Mutex::new(Inner { cfg, rooms, links, bundle: None, dir_name: None, ambient: HashMap::new(), rng: Rng::new(seed ^ 0xB005), live: HashMap::new(), gate_days: HashMap::new(), env: Env::default(), hour: None })));
        if config.boost.enabled {
            if let Err(e) = boost.reload() {
                eprintln!("boost: no thermal bundle loaded, shadow off until one appears: {e:#}");
            }
        }
        boost
    }

    pub fn enabled(&self) -> bool {
        self.0.lock().unwrap().cfg.enabled
    }

    pub fn reload(&self) -> Result<bool> {
        let dir = self.0.lock().unwrap().cfg.model_dir.clone();
        let (name, path) = newest_bundle_dir(Path::new(&dir)).ok_or_else(|| anyhow!("no bundle in {dir}"))?;
        if self.0.lock().unwrap().dir_name.as_deref() == Some(name.as_str()) {
            return Ok(false);
        }
        let bundle = Arc::new(Bundle::load(&path)?);
        eprintln!("boost: loaded thermal bundle {} (residual {})", bundle.version, if bundle.has_residual() { "available" } else { "unavailable" });
        let mut inner = self.0.lock().unwrap();
        inner.bundle = Some(bundle);
        inner.dir_name = Some(name);
        inner.refresh_live();
        Ok(true)
    }

    #[allow(dead_code)]
    pub fn note_ambient(&self, sensor: &str, value: f64) {
        self.0.lock().unwrap().ambient.insert(sensor.to_string(), value);
    }

    pub fn set_gate_days(&self, days: HashMap<String, usize>) {
        let mut inner = self.0.lock().unwrap();
        inner.gate_days = days;
        inner.refresh_live();
    }

    pub fn observe(&self, node: u64, attribute: &str, value: f64, ts_ms: i64) {
        let mut inner = self.0.lock().unwrap();
        let Some(idx) = inner.rooms.iter().position(|r| r.nodes.first() == Some(&node)) else { return };
        match attribute {
            crate::window::LOCAL_TEMPERATURE if value.is_finite() => {
                let rs = &mut inner.rooms[idx];
                rs.sensor.push_back((ts_ms, value));
                rs.dirty = true;
                inner.update_estimate(idx, value, ts_ms);
                inner.trim(idx, ts_ms);
            }
            crate::window::PI_HEATING_DEMAND if (0.0..=100.0).contains(&value) => inner.rooms[idx].valve.push_back((ts_ms, value / 100.0)),
            _ => {}
        }
    }

    pub fn step(&self, input: &StepIn) -> StepOut {
        let mut inner = self.0.lock().unwrap();
        let mut out = StepOut::default();
        if !inner.cfg.enabled {
            return out;
        }
        if let Some((t, wind)) = input.outdoor {
            inner.env.tout = t;
            inner.env.wind = wind.unwrap_or(inner.env.wind);
        }
        inner.env.rad = input.radiation.unwrap_or(inner.env.rad);
        inner.hour = Some(input.hour);
        for want in input.rooms {
            if let Some(idx) = inner.rooms.iter().position(|r| r.name == want.room) {
                inner.step_room(idx, want, input, &mut out);
            }
        }
        out
    }

    pub fn view(&self, room: &str) -> Value {
        let inner = self.0.lock().unwrap();
        let Some(rs) = inner.rooms.iter().find(|r| r.name == room) else { return Value::Null };
        let live = inner.live.get(room);
        let ctx_air = |variant: &str| -> Option<f64> {
            let (bundle, key) = (inner.bundle.as_ref()?, room_key(&inner, room)?);
            let est = rs.est.as_ref()?;
            let ctx = inner.ctx(rs, bundle, &key, est.k.x, 0.0, 0.0, inner.hour_now(), crate::lake::now_us() / 60_000_000)?;
            if variant == GREY {
                Some(est.k.x.1)
            } else {
                ctx.learned_air().ok()
            }
        };
        let mut per = serde_json::Map::new();
        for v in VARIANTS {
            let rec = rs.cycle.as_ref().and_then(|c| c.plans.get(v));
            let available = inner.available(v, room);
            let state = match (&rs.cycle, rec, available) {
                (_, _, false) => "unavailable",
                (None, ..) => "idle",
                (Some(c), ..) if c.live == Some(v) && c.phase == Phase::Boosting => "boosting",
                (Some(c), Some(r), _) if c.live != Some(v) && !r.frozen => "shadow_planning",
                (Some(c), ..) if c.live == Some(v) => "hold",
                _ => "shadow_switched",
            };
            per.insert(
                v.to_string(),
                json!({
                    "state": state,
                    "variant": v,
                    "mode": rs.cycle.as_ref().map_or_else(|| if live.is_some_and(|l| l.0 == v) { "live" } else { "shadow" }, |c| if c.live == Some(v) { "live" } else { "shadow" }),
                    "planned_t_off": rec.and_then(|r| r.planned_from_start),
                    "predicted_peak": rec.and_then(|r| r.peak),
                    "air_estimate": ctx_air(v),
                    "live_candidate": live.is_some_and(|l| l.0 == v),
                }),
            );
        }
        per.insert(BASELINE.to_string(), json!({"state": if rs.cycle.is_some() { "measuring" } else { "idle" }, "variant": BASELINE, "mode": "measured"}));
        json!({
            "cycle": rs.cycle.as_ref().map(|c| json!({"id": c.id, "target": c.target, "elapsed_min": c.samples.len(), "applied_t_off": c.applied, "explore_offset_min": c.offset_min, "live": c.live})),
            "live": live.map(|l| json!({"variant": l.0, "why": l.1})),
            "gate_days": inner.gate_days.get(room),
            "sensor_offset_c": inner.bundle.as_ref().zip(room_key(&inner, room)).map(|(b, k)| b.rooms[&k].offset),
            "model_version": inner.bundle.as_ref().map(|b| b.version.clone()),
            "variants": per,
        })
    }
}

impl Inner {
    fn hour_now(&self) -> f64 {
        use chrono::Timelike;
        if let Some(h) = self.hour {
            return h;
        }
        let now = chrono::Local::now();
        f64::from(now.hour()) + f64::from(now.minute()) / 60.0
    }

    fn available(&self, variant: &str, room: &str) -> bool {
        let Some(bundle) = &self.bundle else { return false };
        room_key(self, room).is_some_and(|k| bundle.rooms[&k].valid) && (variant == GREY || bundle.has_residual())
    }

    fn trim(&mut self, idx: usize, now_ms: i64) {
        let rs = &mut self.rooms[idx];
        for q in [&mut rs.sensor, &mut rs.valve] {
            while q.len() > 1 && q.front().is_some_and(|p| p.0 < now_ms - KEEP_MS) {
                q.pop_front();
            }
        }
    }

    fn refresh_live(&mut self) {
        let forced = !self.cfg.live_rooms_override.is_empty();
        let names: Vec<String> = if forced { self.cfg.live_rooms_override.clone() } else if self.cfg.auto_live { self.rooms.iter().map(|r| r.name.clone()).collect() } else { Vec::new() };
        let mut live = HashMap::new();
        for room in names {
            if let Some(choice) = pick_live(self, &room, forced) {
                if self.live.get(&room) != Some(&choice) {
                    eprintln!("boost: {room} live with {} ({})", choice.0, choice.1);
                }
                live.insert(room, choice);
            }
        }
        self.live = live;
    }

    fn neighbours(&self, room: &str) -> BTreeMap<String, (f64, f64)> {
        let mut nb: BTreeMap<String, (f64, f64)> = BTreeMap::new();
        for (a, b, kind) in &self.links {
            for (here, there) in [(a, b), (b, a)] {
                if here != room {
                    continue;
                }
                if let Some(t) = self.rooms.iter().find(|r| &r.name == there).and_then(|r| r.sensor.back()).filter(|_| there != room) {
                    let e = nb.entry(kind.clone()).or_default();
                    e.0 += 1.0;
                    e.1 += t.1;
                }
            }
        }
        nb
    }

    fn env_for(&self, idx: usize, hour: f64) -> Env {
        let name = &self.rooms[idx].name;
        let night = room_key(self, name).and_then(|k| self.bundle.as_ref().map(|b| b.rooms[&k].phys.is_night(hour))).unwrap_or(false);
        Env { night, nb: self.neighbours(name), ..self.env.clone() }
    }

    fn update_estimate(&mut self, idx: usize, value: f64, ts_ms: i64) {
        self.advance_estimate(idx, value, ts_ms, true);
    }

    fn advance_estimate(&mut self, idx: usize, value: f64, ts_ms: i64, measured: bool) {
        let name = self.rooms[idx].name.clone();
        let Some(bundle) = self.bundle.clone() else { return };
        let Some(key) = room_key(self, &name) else { return };
        let room = &bundle.rooms[&key];
        if !room.valid {
            return;
        }
        let minute = ts_ms.div_euclid(60_000);
        let (cfg, hour) = (self.cfg.clone(), self.hour_now());
        let env = self.env_for(idx, hour);
        let rs = &mut self.rooms[idx];
        if rs.est.as_ref().is_some_and(|e| minute - e.min > GAP_RESET_MIN) {
            rs.est = None;
        }
        match rs.est.as_mut() {
            None => rs.est = Some(Est { k: Kalman::new(value, cfg.p0), min: minute }),
            Some(est) => {
                let h = 1.0 / 60.0;
                while est.min < minute {
                    let u = value_at(&rs.valve, est.min * 60_000, false).unwrap_or(0.0);
                    est.k.predict(&room.phys.terms(u, &env, h), cfg.q_r * h, cfg.q_a * h);
                    est.min += 1;
                }
                if measured {
                    est.k.update(value, room.phys.w, cfg.noise_r * cfg.noise_r);
                }
            }
        }
    }

    #[allow(clippy::too_many_arguments)]
    fn ctx<'a>(&'a self, rs: &'a RoomState, bundle: &'a Bundle, key: &'a str, x: (f64, f64), target: f64, since: f64, hour: f64, now_min: i64) -> Option<Ctx<'a>> {
        let idx = self.rooms.iter().position(|r| r.name == rs.name)?;
        let others = self.rooms.iter().filter(|o| o.name != rs.name).filter(|o| o.valve.back().is_some_and(|v| v.1 > 0.0)).count() as f64;
        Some(Ctx {
            phys: &bundle.rooms.get(key)?.phys,
            bundle,
            room: key,
            x,
            env: self.env_for(idx, hour),
            hour,
            target,
            boost_sp: self.cfg.boost_setpoint,
            sensor: grid(&rs.sensor, now_min, true),
            valve: grid(&rs.valve, now_min, false),
            others,
            since: since.min(SINCE_CAP_MIN),
            ambient: AMBIENT.map(|s| self.ambient.get(s).copied().unwrap_or(f64::NAN)),
            h: 1.0 / 60.0,
        })
    }

    fn step_room(&mut self, idx: usize, want: &RoomIn, input: &StepIn, out: &mut StepOut) {
        let now_ms = input.now_ms;
        let name = want.room.clone();
        let sensor_now = self.rooms[idx].sensor.back().copied();
        if let (Some(bundle), Some(key), Some((t, v))) = (self.bundle.clone(), room_key(self, &name), sensor_now) {
            if bundle.rooms[&key].valid && now_ms - t < STALE_MS && self.rooms[idx].est.as_ref().is_some_and(|e| now_ms.div_euclid(60_000) > e.min) {
                self.advance_estimate(idx, v, now_ms, false);
            }
        }
        let rise = self.rooms[idx].last_desired.map(|d| want.setpoint - d);
        self.rooms[idx].last_desired = Some(want.setpoint);
        if let Some(c) = &self.rooms[idx].cycle {
            let abort = want.blocked.map(str::to_string).or_else(|| (want.setpoint < c.target - 0.05).then(|| "target_decrease".to_string()));
            let moved = (want.setpoint - c.target).abs() >= 0.05 && abort.is_none();
            let boosting = c.live.is_some() && c.phase == Phase::Boosting;
            if let Some(why) = abort.or_else(|| moved.then(|| "target_changed".to_string())) {
                self.finish(idx, now_ms, &why, out);
                if boosting && want.blocked.is_none() && why == "target_decrease" {
                    out.directives.insert(name.clone(), Directive::Switch);
                }
            }
        }
        if self.rooms[idx].cycle.is_none() && want.blocked.is_none() && rise.is_some_and(|r| r >= self.cfg.min_rise_k - 1e-9) {
            self.open(idx, want, input, rise.unwrap_or_default());
        }
        let Some(start_ms) = self.rooms[idx].cycle.as_ref().map(|c| c.start_ms) else { return };
        let elapsed = (now_ms - start_ms) as f64 / 60_000.0;
        let minute = now_ms.div_euclid(60_000);
        let rs = &mut self.rooms[idx];
        let (sensor, valve) = (rs.sensor.back().map_or(f64::NAN, |s| s.1), rs.valve.back().map_or(0.0, |v| v.1 * 100.0));
        if let Some(cycle) = rs.cycle.as_mut().filter(|c| c.last_min != minute) {
            cycle.last_min = minute;
            cycle.samples.push((sensor, valve));
            if cycle.reached_min.is_none() && sensor >= cycle.target - REACH_C {
                cycle.reached_min = Some(elapsed);
            }
        }
        if std::mem::take(&mut rs.dirty) {
            self.replan(idx, now_ms, input.hour);
        }
        let hold_min = self.cfg.hold_min;
        let Some(cycle) = self.rooms[idx].cycle.as_mut() else { return };
        if cycle.live.is_some() && cycle.phase == Phase::Boosting {
            if cycle.applied.is_some_and(|a| elapsed >= a) {
                cycle.phase = Phase::Hold;
                cycle.switched = true;
                if let Some(rec) = cycle.live.and_then(|v| cycle.plans.get_mut(v)) {
                    rec.frozen = true;
                }
                out.directives.insert(name.clone(), Directive::Switch);
            } else {
                out.directives.insert(name.clone(), Directive::Boost { experiment_id: cycle.id.clone() });
            }
        }
        let done = cycle.phase != Phase::Boosting && cycle.reached_min.is_some_and(|r| elapsed - r >= hold_min);
        if done || elapsed >= self.cfg.max_cycle_min {
            self.finish(idx, now_ms, if done { "held" } else { "timeout" }, out);
        }
    }

    fn open(&mut self, idx: usize, want: &RoomIn, input: &StepIn, rise: f64) {
        let name = want.room.clone();
        let Some(bundle) = self.bundle.clone() else { return };
        let Some(key) = room_key(self, &name) else { return };
        let rs = &self.rooms[idx];
        let Some(&(t, temp)) = rs.sensor.back() else { return };
        if !bundle.rooms[&key].valid || rs.est.is_none() || input.now_ms - t > STALE_MS || temp >= want.setpoint - REACH_C {
            return;
        }
        let others = self.rooms.iter().filter(|o| o.name != name).filter(|o| o.valve.back().is_some_and(|v| v.1 > 0.0)).count() as i64;
        let offset_min = if self.live.contains_key(&name) { (self.rng.unit() * 2.0 - 1.0) * self.cfg.explore_min } else { 0.0 };
        let live = self.live.get(&name).and_then(|l| VARIANTS.into_iter().find(|v| *v == l.0)).filter(|v| self.available(v, &name));
        self.rooms[idx].cycle = Some(Cycle {
            id: format!("boost-{name}-{}", input.now_ms),
            start_ms: input.now_ms,
            target: want.setpoint,
            old: want.setpoint - rise,
            start_temp: temp,
            live,
            offset_min,
            applied: None,
            phase: Phase::Hold,
            plans: BTreeMap::new(),
            samples: Vec::new(),
            last_min: -1,
            reached_min: None,
            version: bundle.version.clone(),
            outdoor: self.env.tout,
            radiation: self.env.rad,
            others,
            switched: false,
        });
        self.rooms[idx].dirty = true;
        self.replan(idx, input.now_ms, input.hour);
        if let Some(c) = self.rooms[idx].cycle.as_mut() {
            if c.live.is_some() && c.applied.is_some_and(|a| a > OFF_EPS_MIN) {
                c.phase = Phase::Boosting;
            }
        }
    }

    fn replan(&mut self, idx: usize, now_ms: i64, hour: f64) {
        let name = self.rooms[idx].name.clone();
        let Some(bundle) = self.bundle.clone() else { return };
        let Some(key) = room_key(self, &name) else { return };
        let rs = &self.rooms[idx];
        let (Some(cycle), Some(est)) = (&rs.cycle, &rs.est) else { return };
        let elapsed = (now_ms - cycle.start_ms) as f64 / 60_000.0;
        let remaining = (self.cfg.max_cycle_min - elapsed).max(0.0) as usize;
        let v50_so_far = cycle.samples.iter().filter(|s| s.1 > 50.0).count() as f64;
        let Some(ctx) = self.ctx(rs, &bundle, &key, est.k.x, cycle.target, elapsed, hour, now_ms.div_euclid(60_000)) else { return };
        let mut results: Vec<(&'static str, Rec)> = Vec::new();
        for v in VARIANTS {
            if cycle.plans.get(v).is_some_and(|r| r.frozen) {
                continue;
            }
            let rec = if !self.available(v, &name) {
                Rec { planned_from_start: None, peak: None, ttt: None, valve50: None, air: None, frozen: true, error: Some("unavailable".into()) }
            } else {
                match ctx.plan(v, remaining) {
                    Ok((t_off, sim)) => Rec {
                        planned_from_start: Some(elapsed + t_off as f64),
                        peak: Some(sim.peak),
                        ttt: sim.ttt.map(|t| elapsed + t),
                        valve50: Some(v50_so_far + sim.valve50),
                        air: (v == GREY).then_some(est.k.x.1),
                        frozen: false,
                        error: None,
                    },
                    Err(e) => Rec { planned_from_start: None, peak: None, ttt: None, valve50: None, air: None, frozen: true, error: Some(format!("{e:#}")) },
                }
            };
            results.push((v, rec));
        }
        let max_cycle = self.cfg.max_cycle_min;
        let Some(cycle) = self.rooms[idx].cycle.as_mut() else { return };
        for (v, mut rec) in results {
            rec.frozen |= rec.planned_from_start.is_some_and(|p| elapsed >= p && cycle.live != Some(v));
            cycle.plans.insert(v, rec);
        }
        if cycle.phase != Phase::Hold || cycle.applied.is_none() {
            let planned = cycle.live.and_then(|v| cycle.plans.get(v)).and_then(|r| r.planned_from_start);
            if let Some(p) = planned {
                cycle.applied = Some((p + cycle.offset_min).clamp(0.0, max_cycle));
            } else if cycle.live.is_some() {
                cycle.live = None;
            }
        }
    }

    fn finish(&mut self, idx: usize, now_ms: i64, why: &str, out: &mut StepOut) {
        let Some(c) = self.rooms[idx].cycle.take() else { return };
        let rs = &self.rooms[idx];
        let node = rs.nodes.first().copied();
        let elapsed = (now_ms - c.start_ms) as f64 / 60_000.0;
        let seen: Vec<f64> = c.samples.iter().map(|s| s.0).filter(|v| v.is_finite()).collect();
        let overshoot = seen.iter().copied().fold(f64::NEG_INFINITY, f64::max) - c.target;
        let ttt = c.samples.iter().position(|s| s.0 >= c.target - REACH_C).map(|i| i as f64 + 1.0);
        let v50 = c.samples.iter().filter(|s| s.1 > 50.0).count() as f64;
        for (variant, rec) in &c.plans {
            let live = c.live == Some(*variant);
            let mut detail = json!({
                "room": rs.name, "variant": variant, "mode": if live { "live" } else { "shadow" }, "cycle_id": c.id, "start_ts_ms": c.start_ms, "target": c.target, "setpoint_old": c.old,
                "start_temp": c.start_temp, "planned_t_off_min": rec.planned_from_start, "applied_t_off_min": if live { c.applied } else { None }, "explore_offset_min": if live { Some(c.offset_min) } else { None },
                "predicted_peak_c": rec.peak, "model_version": c.version, "end": why, "elapsed_min": elapsed, "outdoor_temp": c.outdoor, "radiation": c.radiation, "others_heating": c.others,
                "exploration": live, "air_estimate_c": rec.air,
            });
            if let Some(e) = &rec.error {
                detail["status"] = json!("unavailable");
                detail["error"] = json!(e);
            } else if live {
                detail["overshoot_c"] = json!(overshoot.is_finite().then(|| overshoot.max(0.0)));
                detail["minutes_to_target_0_3"] = json!(ttt);
                detail["valve_over_50_min"] = json!(v50);
            } else {
                detail["overshoot_c"] = json!(rec.peak.map(|p| (p - c.target).max(0.0)));
                detail["minutes_to_target_0_3"] = json!(rec.ttt);
                detail["valve_over_50_min"] = json!(rec.valve50);
            }
            let mut event = Event::now(kind::BOOST_CYCLE, "heatd").with_detail(detail);
            event.ts = now_ms * 1000;
            event.node = node;
            if live {
                event.experiment_id = Some(c.id.clone());
            }
            out.events.push(event);
        }
    }
}

pub fn gate_days(root: &Path, today: chrono::NaiveDate, lookback_days: i64) -> Result<HashMap<String, usize>> {
    let db = crate::report::open_lake(root, today - chrono::Duration::days(lookback_days), today)?;
    let rows = crate::report::rows(
        &db.conn,
        "SELECT json_extract_string(detail, '$.room') AS room, count(DISTINCT CAST(ts AS DATE)) FROM events WHERE kind = 'boost_cycle' AND json_extract_string(detail, '$.mode') = 'shadow' AND coalesce(json_extract_string(detail, '$.status'), '') <> 'unavailable' GROUP BY 1",
        |r| Ok((r.get::<_, String>(0)?, r.get::<_, i64>(1)? as usize)),
    )?;
    Ok(rows.into_iter().collect())
}

pub fn spawn_background(boost: Boost, root: PathBuf) {
    tokio::spawn(async move {
        let reload = std::time::Duration::from_secs_f64(boost.0.lock().unwrap().cfg.reload_min.max(0.1) * 60.0);
        let mut ticks = 0u64;
        loop {
            let b = boost.clone();
            let r = root.clone();
            let _ = tokio::task::spawn_blocking(move || {
                if let Err(e) = b.reload() {
                    if b.0.lock().unwrap().bundle.is_none() {
                        eprintln!("boost: {e:#}");
                    }
                }
                if ticks.is_multiple_of(GATE_REFRESH_MIN.div_ceil((reload.as_secs() / 60).max(1))) {
                    match gate_days(&r, chrono::Utc::now().date_naive(), 30) {
                        Ok(days) => b.set_gate_days(days),
                        Err(e) => eprintln!("boost: gate days: {e:#}"),
                    }
                }
            })
            .await;
            ticks += 1;
            tokio::time::sleep(reload).await;
        }
    });
}

#[cfg(test)]
#[path = "boost_tests.rs"]
mod tests;
