use crate::config::{Config, WindowSection, WindowV2Section};
use crate::window::{Windows, LOCAL_TEMPERATURE, PI_HEATING_DEMAND};
use serde_json::{json, Value};
use std::collections::{BTreeMap, VecDeque};

const MIN_MS: i64 = 60_000;
const LEAD_MS: i64 = 15 * MIN_MS;
const MAX_TICK_MIN: f64 = 5.0;
const CENTER_C: f64 = 20.0;
const RIDGE_A: f64 = 60.0;
const RIDGE_B: f64 = 500.0;

pub const V1: &str = "v1";
pub const V2: &str = "v2";

#[derive(Debug, Clone, Default)]
pub struct RoomCtx {
    pub target: Option<f64>,
    pub hold: bool,
    pub boost: bool,
    pub away: bool,
}

#[derive(Debug, Clone, Default)]
pub struct Flags {
    pub heating: bool,
    pub no_supply: bool,
    pub rad_age_min: Option<f64>,
    pub typical_k_per_h: f64,
    pub decrease_recent: bool,
    pub outdoor_blocked: bool,
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
pub struct Tally {
    pub start_ms: i64,
    last_ms: i64,
    pub probes: u32,
    pub probe_valve_min: f64,
    pub below_min: f64,
}

impl Tally {
    pub fn new(now: i64) -> Self {
        Self { start_ms: now, last_ms: now, probes: 0, probe_valve_min: 0.0, below_min: 0.0 }
    }

    pub fn tick(&mut self, now: i64, below: bool, probe_valve: Option<f64>) {
        let dt = ((now - self.last_ms) as f64 / MIN_MS as f64).clamp(0.0, MAX_TICK_MIN);
        self.last_ms = now;
        if below {
            self.below_min += dt;
        }
        if let Some(v) = probe_valve {
            self.probe_valve_min += dt * v;
        }
    }

    pub fn probe(&mut self) {
        self.probes += 1;
    }
}

#[derive(Debug, Clone)]
pub struct Episode {
    pub detector: &'static str,
    pub room: String,
    pub end_ms: i64,
    pub reason: String,
    pub source: &'static str,
    pub onset_ms: Option<i64>,
    pub tally: Tally,
}

impl Episode {
    pub fn detail(&self, labels: &[LabelRec], grace_ms: i64) -> Value {
        let start = self.tally.start_ms;
        let mine = |l: &&LabelRec| l.room == self.room && l.ts >= start - LEAD_MS && l.ts <= self.end_ms + grace_ms;
        let seen = |l: &&LabelRec| !(l.question == Some("closed") && !l.open);
        let confirm = labels.iter().filter(mine).filter(seen).max_by_key(|l| l.ts).map_or("none", |l| if l.open { "yes" } else { "no" });
        let latency = labels.iter().filter(mine).filter(|l| l.manual && l.open && l.ts <= self.end_ms).min_by_key(|l| l.ts).map(|l| (start - l.ts) as f64 / MIN_MS as f64);
        let close = labels.iter().filter(mine).filter(|l| l.question == Some("closed") && l.ts >= self.end_ms).max_by_key(|l| l.ts).map_or("none", |l| if l.open { "wrong" } else { "correct" });
        json!({
            "detector": self.detector,
            "room": self.room,
            "start_ms": start,
            "end_ms": self.end_ms,
            "duration_min": (self.end_ms - start) as f64 / MIN_MS as f64,
            "end_reason": self.reason,
            "source": self.source,
            "onset_ms": self.onset_ms,
            "probe_count": self.tally.probes,
            "probe_valve_min": self.tally.probe_valve_min,
            "below_min": self.tally.below_min,
            "label": confirm,
            "close_label": close,
            "latency_min": latency,
        })
    }
}

pub trait Baseline: std::fmt::Debug + Send {
    fn predict(&self, prev: f64, dt_min: f64, heating: bool) -> f64;
    fn update(&mut self, prev: f64, dt_min: f64, heating: bool, delta: f64);
}

#[derive(Debug)]
pub struct Ar1 {
    sw: f64,
    sx: f64,
    sy: f64,
    sxx: f64,
    sxy: f64,
    a0: f64,
    b0: f64,
    gain: f64,
    forget: f64,
}

impl Ar1 {
    pub fn new(c: &WindowV2Section) -> Self {
        Self { sw: 0.0, sx: 0.0, sy: 0.0, sxx: 0.0, sxy: 0.0, a0: -c.prior_cool_per_min * (CENTER_C - c.prior_floor_c), b0: -c.prior_cool_per_min, gain: c.prior_heat_k_per_min, forget: c.forget }
    }

    pub fn coefficients(&self) -> (f64, f64) {
        let (a00, a11) = (self.sw + RIDGE_A, self.sxx + RIDGE_B);
        let (r0, r1) = (self.sy + RIDGE_A * self.a0, self.sxy + RIDGE_B * self.b0);
        let det = a00 * a11 - self.sx * self.sx;
        ((r0 * a11 - self.sx * r1) / det, (a00 * r1 - self.sx * r0) / det)
    }
}

impl Baseline for Ar1 {
    fn predict(&self, prev: f64, dt_min: f64, heating: bool) -> f64 {
        if heating {
            return self.gain * dt_min;
        }
        let (a, b) = self.coefficients();
        (a + b * (prev - CENTER_C)) * dt_min
    }

    fn update(&mut self, prev: f64, dt_min: f64, heating: bool, delta: f64) {
        let rate = delta / dt_min;
        if heating {
            self.gain += (0.02 * dt_min).min(1.0) * (rate - self.gain);
            return;
        }
        let decay = self.forget.powf(dt_min);
        let (x, w) = (prev - CENTER_C, dt_min);
        self.sw = self.sw * decay + w;
        self.sx = self.sx * decay + w * x;
        self.sy = self.sy * decay + w * rate;
        self.sxx = self.sxx * decay + w * x * x;
        self.sxy = self.sxy * decay + w * x * rate;
    }
}

#[derive(Debug)]
struct Open2 {
    tally: Tally,
    onset_ms: i64,
    last_probe: Option<i64>,
}

#[derive(Debug)]
struct Room2 {
    base: Box<dyn Baseline>,
    last: Option<(i64, f64)>,
    down: f64,
    down_since: Option<i64>,
    up: f64,
    open: Option<Open2>,
    lengths: VecDeque<f64>,
    closed_at: Option<i64>,
}

#[derive(Debug, Clone, PartialEq)]
pub struct Transition {
    pub room: String,
    pub open: bool,
    pub ts: i64,
}

#[derive(Debug, Default)]
pub struct Step2 {
    pub done: Vec<Episode>,
    pub transitions: Vec<Transition>,
}

#[derive(Debug, Default)]
pub struct Shadow {
    rooms: BTreeMap<String, Room2>,
}

fn survival(samples: &[f64], t: f64) -> f64 {
    (samples.iter().filter(|l| **l > t).count() as f64 + 1.0) / (samples.len() as f64 + 1.0)
}

pub fn probe_costs(c: &WindowV2Section, probe_min: f64, samples: &[f64], elapsed_min: f64, below: bool) -> (f64, f64) {
    let s0 = survival(samples, elapsed_min);
    let closed_minutes: f64 = (1..=c.probe_horizon_min.max(1.0) as usize).map(|m| 1.0 - survival(samples, elapsed_min + m as f64) / s0).sum();
    let wait = if below { c.comfort_cost_per_min_below * closed_minutes } else { 0.0 };
    let probe = c.energy_cost_per_valve_min * probe_min * c.probe_valve_pct / 100.0 * survival(samples, elapsed_min + probe_min) / s0;
    (wait, probe)
}

impl Room2 {
    fn new(c: &WindowV2Section) -> Self {
        Self { base: Box::new(Ar1::new(c)), last: None, down: 0.0, down_since: None, up: 0.0, open: None, lengths: VecDeque::new(), closed_at: None }
    }

    fn samples(&self, c: &WindowV2Section) -> Vec<f64> {
        if self.lengths.len() >= c.prior_min_n { self.lengths.iter().copied().collect() } else { c.default_episode_min.clone() }
    }

    fn close(&mut self, c: &WindowV2Section, room: &str, ts: i64, reason: &'static str) -> Episode {
        let o = self.open.take().expect("open episode");
        if reason == "passive" {
            self.lengths.push_back((ts - o.tally.start_ms) as f64 / MIN_MS as f64);
            while self.lengths.len() > c.prior_keep {
                self.lengths.pop_front();
            }
        }
        self.closed_at = Some(ts);
        self.down = 0.0;
        self.up = 0.0;
        self.down_since = None;
        Episode { detector: V2, room: room.into(), end_ms: ts, reason: reason.into(), source: "auto", onset_ms: Some(o.onset_ms), tally: o.tally }
    }
}

impl Shadow {
    pub fn open_rooms(&self) -> Vec<String> {
        self.rooms.iter().filter(|(_, r)| r.open.is_some()).map(|(n, _)| n.clone()).collect()
    }

    #[cfg(test)]
    pub fn lengths(&self, room: &str) -> Vec<f64> {
        self.rooms.get(room).map_or_else(Vec::new, |r| r.lengths.iter().copied().collect())
    }

    pub fn on_report(&mut self, cfg: &WindowSection, room: &str, ts: i64, x: f64, f: &Flags, ctx: &RoomCtx) -> Step2 {
        let c = &cfg.v2;
        let mins = |m: f64| (m * MIN_MS as f64) as i64;
        let r = self.rooms.entry(room.into()).or_insert_with(|| Room2::new(c));
        let mut out = Step2::default();
        let Some((pt, px)) = r.last else {
            r.last = Some((ts, x));
            return out;
        };
        if ts <= pt {
            return out;
        }
        r.last = Some((ts, x));
        let dt = (ts - pt) as f64 / MIN_MS as f64;
        if dt > c.max_gap_min {
            r.down = 0.0;
            r.up = 0.0;
            r.down_since = None;
            return out;
        }
        let guard = f.typical_k_per_h * cfg.expected_cooldown_factor / 60.0;
        let mut slack = c.slack_k_per_min;
        if f.heating && !f.no_supply {
            slack += guard;
        } else if let Some(age) = f.rad_age_min.filter(|_| !f.no_supply) {
            slack += guard * (-age / c.radiator_tau_min).exp();
        }
        let res = (x - px) - r.base.predict(px, dt, f.heating);
        let below = ctx.target.is_none_or(|t| x < t - cfg.below_k);
        let Some(o) = r.open.as_mut() else {
            let steady = f.heating || f.rad_age_min.is_none_or(|a| a >= 3.0 * c.radiator_tau_min);
            if r.down < 0.5 * c.open_h_k && steady {
                r.base.update(px, dt, f.heating, x - px);
            }
            r.down = (r.down - res - slack * dt).max(0.0);
            r.down_since = if r.down > 0.0 { r.down_since.or(Some(pt)) } else { None };
            let blocked = f.outdoor_blocked || f.decrease_recent || r.closed_at.is_some_and(|t| ts - t < mins(cfg.rearm_min));
            if r.down >= c.open_h_k {
                if blocked {
                    r.down = c.open_h_k;
                } else {
                    r.open = Some(Open2 { tally: Tally::new(ts), onset_ms: r.down_since.unwrap_or(ts), last_probe: None });
                    r.down = 0.0;
                    r.up = 0.0;
                    r.down_since = None;
                    out.transitions.push(Transition { room: room.into(), open: true, ts });
                }
            }
            return out;
        };
        o.tally.tick(ts, below, None);
        let open_min = (ts - o.tally.start_ms) as f64 / MIN_MS as f64;
        r.up = if f.heating { 0.0 } else { (r.up + res - c.close_slack_k_per_min * dt).max(0.0) };
        let held = open_min >= cfg.min_open_hold_min;
        let reason = if held && r.up >= c.close_h_k {
            Some("passive")
        } else if open_min >= c.max_episode_min {
            Some("expired")
        } else {
            None
        };
        if let Some(reason) = reason {
            let ep = r.close(c, room, ts, reason);
            out.transitions.push(Transition { room: room.into(), open: false, ts });
            out.done.push(ep);
            return out;
        }
        let allowed = !(ctx.hold || ctx.boost || ctx.away || f.no_supply);
        if held && allowed && o.last_probe.is_none_or(|t| ts - t >= mins(c.probe_gap_min)) {
            let samples = r.samples(c);
            let o = r.open.as_mut().expect("open episode");
            let (wait, probe) = probe_costs(c, cfg.probe_min, &samples, open_min, below);
            if wait > probe {
                o.last_probe = Some(ts);
                o.tally.probe();
                o.tally.probe_valve_min += cfg.probe_min * c.probe_valve_pct / 100.0;
            }
        }
        out
    }

    pub fn finish(&mut self, cfg: &WindowSection, now: i64) -> Vec<Episode> {
        let rooms: Vec<String> = self.open_rooms();
        rooms.iter().filter_map(|n| self.rooms.get_mut(n).map(|r| r.close(&cfg.v2, n, now, "open_at_end"))).collect()
    }

    pub fn forget_open(&mut self) {
        self.rooms.values_mut().for_each(|r| {
            r.open = None;
            r.last = None;
            r.down = 0.0;
            r.up = 0.0;
            r.down_since = None;
        });
    }
}

pub fn parse_csv(text: &str) -> anyhow::Result<Vec<(i64, u64, String, f64)>> {
    text.lines()
        .skip(1)
        .filter(|l| !l.trim().is_empty())
        .map(|l| {
            let f: Vec<&str> = l.split(',').map(str::trim).collect();
            anyhow::ensure!(f.len() == 4, "bad row {l}");
            Ok((f[0].parse()?, f[1].parse()?, f[2].to_string(), f[3].parse()?))
        })
        .collect()
}

pub fn replay(text: &str, rows: &[(i64, u64, String, f64)], outdoor: Option<(f64, Option<f64>)>) -> anyhow::Result<Value> {
    let mut config = Config::parse(text)?;
    if config.rooms.is_empty() {
        let mut nodes: Vec<u64> = rows.iter().filter(|r| r.2 == LOCAL_TEMPERATURE || r.2 == PI_HEATING_DEMAND).map(|r| r.1).collect();
        nodes.sort_unstable();
        nodes.dedup();
        let generated: String = nodes.iter().map(|n| format!("
[[rooms]]
name = 'node{n}'
nodes = [{n}]
")).collect();
        config = Config::parse(&format!("{text}
{generated}"))?;
    }
    config.window.enabled = true;
    config.window.v2.enabled = true;
    let mut w = Windows::default();
    let mut rows: Vec<&(i64, u64, String, f64)> = rows.iter().collect();
    rows.sort_by_key(|r| r.0);
    let Some(first) = rows.first().map(|r| r.0) else { return Ok(json!({"episodes": [], "summary": {}})) };
    let end = rows.last().map_or(first, |r| r.0) + (config.window.label_grace_min as i64 + 1) * MIN_MS;
    let (mut t, mut i, mut outdoor) = (first - MIN_MS, 0, outdoor);
    let mut episodes: Vec<Value> = Vec::new();
    let collect = |events: Vec<crate::window::WinEvent>, episodes: &mut Vec<Value>| episodes.extend(events.into_iter().filter(|e| e.kind == crate::lake::kind::WINDOW_EPISODE).map(|e| e.detail));
    while t <= end {
        while i < rows.len() && rows[i].0 <= t {
            let (_, node, attribute, value) = rows[i];
            if attribute == "outdoor_temp" {
                outdoor = Some((*value, outdoor.and_then(|o| o.1)));
            } else {
                w.observe(&config, *node, attribute, *value, rows[i].0);
            }
            i += 1;
        }
        collect(w.step(&config, t, outdoor), &mut episodes);
        t += MIN_MS;
    }
    collect(w.finish(&config, t), &mut episodes);
    let summary: serde_json::Map<String, Value> = [V1, V2]
        .iter()
        .map(|d| {
            let mine: Vec<&Value> = episodes.iter().filter(|e| e["detector"] == *d).collect();
            let sum = |k: &str| mine.iter().filter_map(|e| e[k].as_f64()).sum::<f64>();
            let count = |l: &str| mine.iter().filter(|e| e["label"] == l).count();
            (d.to_string(), json!({"episodes": mine.len(), "open_min": sum("duration_min"), "probes": sum("probe_count"), "probe_valve_min": sum("probe_valve_min"), "below_min": sum("below_min"), "label_yes": count("yes"), "label_no": count("no"), "label_none": count("none")}))
        })
        .collect();
    Ok(json!({"episodes": episodes, "summary": summary}))
}
