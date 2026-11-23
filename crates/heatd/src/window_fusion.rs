use crate::config::WindowFusion;
use anyhow::{Context, Result};
use matter_controller::{CO2_CLUSTER, HUMIDITY_MEASUREMENT_CLUSTER, TEMPERATURE_MEASUREMENT_CLUSTER};
use serde::Deserialize;
use serde_json::{json, Value};
use std::collections::{BTreeMap, VecDeque};

const MIN_MS: i64 = 60_000;
const KEEP_MS: i64 = 40 * MIN_MS;
const MIN_SPAN_MS: i64 = 90_000;
const MEMORY: f64 = 0.85;
const SHOWN: f64 = 0.4;
const TAIL: f64 = 1e-6;
const RECOVERY_DAMP: f64 = 0.3;
const CO2_DEPRESSED_FRAC: f64 = 0.6;
const CO2_REF_MIN_EXCESS: f64 = 150.0;
const BUILTIN: &str = include_str!("window_fusion.json");

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Chan {
    Trv,
    Co2,
    Rh,
    Temp,
    House,
    Couple,
    Hard,
    Level,
}

pub const CHANNELS: usize = 8;

impl Chan {
    pub const ALL: [Chan; CHANNELS] = [Chan::Trv, Chan::Co2, Chan::Rh, Chan::Temp, Chan::House, Chan::Couple, Chan::Hard, Chan::Level];

    pub fn name(self) -> &'static str {
        match self {
            Chan::Trv => "trv",
            Chan::Co2 => "co2",
            Chan::Rh => "rh",
            Chan::Temp => "air_temp",
            Chan::House => "house",
            Chan::Couple => "neighbour",
            Chan::Hard => "answer",
            Chan::Level => "level",
        }
    }

    fn text(self, open: bool) -> &'static str {
        match (self, open) {
            (Chan::Trv, true) => "Thermostat kühlt",
            (Chan::Trv, false) => "Thermostat wärmt wieder",
            (Chan::Co2, true) => "CO2 fällt schnell",
            (Chan::Co2, false) => "CO2 steigt wieder",
            (Chan::Rh, true) => "Luftfeuchte fällt",
            (Chan::Rh, false) => "Luftfeuchte erholt sich",
            (Chan::Temp, true) => "Raumluft kühlt ab",
            (Chan::Temp, false) => "Raumluft wärmt wieder",
            (Chan::House, true) => "Haus-Kanal schlägt an",
            (Chan::House, false) => "Haus-Kanal ruhig",
            (Chan::Couple, true) => "Nachbarraum offen",
            (Chan::Couple, false) => "Nachbarraum zu",
            (Chan::Hard, true) => "Antwort: offen",
            (Chan::Hard, false) => "Antwort: zu",
            (Chan::Level, true) => "Luft bleibt kalt und trocken",
            (Chan::Level, false) => "Luft erholt sich",
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub enum Signal {
    Co2,
    Rh,
    Temp,
}

impl Signal {
    pub fn of_cluster(cluster: u32, value: f64) -> Option<(Signal, f64)> {
        match cluster {
            TEMPERATURE_MEASUREMENT_CLUSTER if (-4000.0..=6000.0).contains(&value) => Some((Signal::Temp, value / 100.0)),
            HUMIDITY_MEASUREMENT_CLUSTER if (0.0..=10_000.0).contains(&value) => Some((Signal::Rh, value / 100.0)),
            CO2_CLUSTER if (0.0..=100_000.0).contains(&value) => Some((Signal::Co2, value)),
            _ => None,
        }
    }
}

#[derive(Debug, Clone, Deserialize)]
pub struct Curve {
    pub edges: Vec<f64>,
    pub llr: Vec<f64>,
}

impl Curve {
    pub fn at(&self, x: f64) -> f64 {
        let (e, v) = (&self.edges, &self.llr);
        if e.is_empty() || !x.is_finite() {
            return 0.0;
        }
        let i = e.partition_point(|b| *b <= x);
        if i == 0 {
            return v[0];
        }
        if i >= e.len() {
            return v[e.len() - 1];
        }
        let f = (x - e[i - 1]) / (e[i] - e[i - 1]);
        v[i - 1] + f * (v[i] - v[i - 1])
    }
}

#[derive(Debug, Clone, Deserialize)]
pub struct TrvPar {
    pub censor_k: f64,
    pub closed_sd: f64,
    pub open_mean: [f64; 2],
    pub cap: f64,
}

fn phi(x: f64) -> f64 {
    let t = 1.0 / (1.0 + 0.327_591_1 * (x.abs() / std::f64::consts::SQRT_2));
    let poly = t * (0.254_829_592 + t * (-0.284_496_736 + t * (1.421_413_741 + t * (-1.453_152_027 + t * 1.061_405_429))));
    let erf = 1.0 - poly * (-(x * x) / 2.0).exp();
    0.5 * (1.0 + erf.copysign(x))
}

impl TrvPar {
    pub fn llr(&self, z: f64, sigma: f64, valve_open: bool) -> f64 {
        if !z.is_finite() || !sigma.is_finite() || sigma <= 0.0 {
            return 0.0;
        }
        let (h, sd, mu) = (self.censor_k / sigma, self.closed_sd, self.open_mean[usize::from(valve_open)]);
        let cell = |m: f64| phi((z + h - m) / sd) - phi((z - h - m) / sd);
        let (open, closed) = (cell(mu), cell(0.0));
        let l = if open < TAIL || closed < TAIL { (mu * z - mu * mu / 2.0) / (sd * sd) } else { (open / closed).ln() };
        l.clamp(-self.cap, self.cap)
    }
}

#[derive(Debug, Clone, Deserialize)]
pub struct Temper {
    pub trv: f64,
    pub co2: f64,
    pub rh: f64,
    pub temp: f64,
}

#[derive(Debug, Clone, Deserialize)]
pub struct Tables {
    pub version: String,
    pub co2: Curve,
    pub rh: Curve,
    pub temp: Curve,
    pub trv: TrvPar,
    pub temper: Temper,
    pub alarm_llr: f64,
}

impl Default for Tables {
    fn default() -> Self {
        Self::builtin()
    }
}

impl Tables {
    pub fn builtin() -> Self {
        serde_json::from_str(BUILTIN).expect("built-in window fusion tables")
    }

    pub fn load(path: &str) -> Result<Self> {
        serde_json::from_str(&std::fs::read_to_string(path).with_context(|| format!("read {path}"))?).with_context(|| format!("parse {path}"))
    }

    pub fn for_config(cfg: &WindowFusion) -> Self {
        if cfg.tables.is_empty() {
            return Self::builtin();
        }
        Self::load(&cfg.tables).unwrap_or_else(|e| {
            eprintln!("window fusion tables: {e:#}, using the built-in ones");
            Self::builtin()
        })
    }
}

pub fn thresholds(cfg: &WindowFusion, raise_cost_min: f64, cold_cost_min: f64) -> (f64, f64) {
    let cold = cold_cost_min * cfg.cool_k_per_h * cfg.horizon_min * cfg.horizon_min / 120.0;
    (((raise_cost_min + cold).max(1e-9) / cfg.horizon_min).ln(), (cold.max(1e-9) / cfg.horizon_min).ln())
}

fn logit(p: f64) -> f64 {
    (p / (1.0 - p)).ln()
}

fn sigmoid(l: f64) -> f64 {
    1.0 / (1.0 + (-l).exp())
}

#[derive(Debug, Clone, Copy, Default)]
pub struct Rates {
    pub co2: Option<f64>,
    pub rh: Option<f64>,
    pub temp: Option<f64>,
}

impl Rates {
    pub fn any(&self) -> bool {
        self.co2.is_some() || self.rh.is_some() || self.temp.is_some()
    }
}

#[derive(Debug, Clone, Copy, Default)]
pub struct Reference {
    pub co2: Option<f64>,
    pub since_ms: i64,
}

#[derive(Debug, Default)]
pub struct AirRoom {
    nodes: BTreeMap<u64, [VecDeque<(i64, f64)>; 3]>,
    floor_ms: i64,
    skip_rh: bool,
}

fn slope_per_min(points: &[(i64, f64)]) -> (f64, f64) {
    let n = points.len() as f64;
    let (mx, my) = (points.iter().map(|p| p.0 as f64).sum::<f64>() / n, points.iter().map(|p| p.1).sum::<f64>() / n);
    let var: f64 = points.iter().map(|p| (p.0 as f64 - mx).powi(2)).sum();
    let slope = if var > 0.0 { points.iter().map(|p| (p.0 as f64 - mx) * (p.1 - my)).sum::<f64>() / var * MIN_MS as f64 } else { 0.0 };
    (slope, my)
}

impl AirRoom {
    pub fn skip_humidity(&mut self, skip: bool) {
        self.skip_rh = skip;
    }

    pub fn push(&mut self, node: u64, signal: Signal, ts_ms: i64, value: f64) {
        let series = &mut self.nodes.entry(node).or_default()[signal as usize];
        if series.back().is_some_and(|b| b.0 > ts_ms) {
            return;
        }
        series.push_back((ts_ms, value));
        while series.front().is_some_and(|s| s.0 < ts_ms - KEEP_MS) {
            series.pop_front();
        }
    }

    pub fn reset(&mut self, now_ms: i64) {
        self.floor_ms = now_ms;
    }

    fn newest(&self, signal: Signal, now: i64) -> Option<&VecDeque<(i64, f64)>> {
        self.nodes.values().map(|n| &n[signal as usize]).filter(|s| s.back().is_some_and(|b| b.0 <= now)).max_by_key(|s| s.back().map(|b| b.0))
    }

    fn points(&self, cfg: &WindowFusion, signal: Signal, now: i64) -> Option<Vec<(i64, f64)>> {
        let usable: Vec<(i64, f64)> = self.newest(signal, now)?.iter().copied().filter(|s| s.0 <= now && s.0 > self.floor_ms).collect();
        if usable.last().is_none_or(|l| now - l.0 > (cfg.fresh_min * MIN_MS as f64) as i64) {
            return None;
        }
        let from = now - (cfg.window_min * MIN_MS as f64) as i64;
        let inside: Vec<(i64, f64)> = usable.iter().copied().filter(|s| s.0 > from).collect();
        let picked = if inside.len() >= cfg.min_samples { inside } else if usable.len() >= cfg.min_samples { usable[usable.len() - cfg.min_samples..].to_vec() } else { return None };
        let span = picked.last()?.0 - picked[0].0;
        (span >= MIN_SPAN_MS && span <= (cfg.max_window_min.max(cfg.window_min) * MIN_MS as f64) as i64).then_some(picked)
    }

    fn level(&self, signal: Signal, now: i64) -> Option<f64> {
        self.newest(signal, now)?.back().filter(|b| now - b.0 <= 10 * MIN_MS).map(|b| b.1)
    }

    pub fn reference(&self, now: i64) -> Reference {
        Reference { co2: self.newest(Signal::Co2, now).and_then(|s| s.iter().filter(|p| p.0 <= now).map(|p| p.1).reduce(f64::max)), since_ms: now }
    }

    pub fn depressed(&self, cfg: &WindowFusion, now: i64, r: &Reference) -> bool {
        now - r.since_ms <= (cfg.level_max_min * MIN_MS as f64) as i64 && r.co2.zip(self.level(Signal::Co2, now)).is_some_and(|(top, v)| top - cfg.outdoor_co2_ppm >= CO2_REF_MIN_EXCESS && v - cfg.outdoor_co2_ppm < CO2_DEPRESSED_FRAC * (top - cfg.outdoor_co2_ppm))
    }

    pub fn latest_temp(&self, now: i64) -> Option<f64> {
        self.newest(Signal::Temp, now).and_then(|s| s.back()).map(|b| b.1)
    }

    pub fn rates(&self, cfg: &WindowFusion, now: i64, outdoor_c: f64) -> Rates {
        let co2 = self.points(cfg, Signal::Co2, now).map(|p| {
            let (s, m) = slope_per_min(&p);
            s / (m - cfg.outdoor_co2_ppm).max(cfg.co2_floor_ppm)
        });
        let rh = self.points(cfg, Signal::Rh, now).filter(|_| !self.skip_rh).map(|p| slope_per_min(&p).0);
        let temp = self.points(cfg, Signal::Temp, now).filter(|_| outdoor_c.is_finite()).map(|p| {
            let (s, m) = slope_per_min(&p);
            s / (m - outdoor_c).max(cfg.dt_floor_k)
        });
        Rates { co2, rh, temp }
    }
}

#[derive(Debug, Clone, Default)]
pub struct Inputs {
    pub trv: Option<f64>,
    pub pulse: Option<(Chan, f64)>,
    pub rates: Rates,
    pub gain: f64,
    pub holding: bool,
    pub depressed: bool,
}

#[derive(Debug, Clone)]
pub struct Posterior {
    pub own: f64,
    pub couple: f64,
    pub contrib: [f64; CHANNELS],
    pub added: [f64; CHANNELS],
    pub independent: bool,
}

impl Default for Posterior {
    fn default() -> Self {
        Self { own: f64::NAN, couple: 0.0, contrib: [0.0; CHANNELS], added: [0.0; CHANNELS], independent: false }
    }
}

impl Posterior {
    fn rates_of(cfg: &WindowFusion) -> (f64, f64) {
        (1.0 / cfg.open_every_min.max(1.0), 1.0 / cfg.stay_min.max(1.0))
    }

    pub fn prior(cfg: &WindowFusion) -> f64 {
        let (open, close) = Self::rates_of(cfg);
        logit(open / (open + close))
    }

    pub fn total(&self) -> f64 {
        self.own + self.couple
    }

    pub fn p(&self) -> f64 {
        sigmoid(self.total())
    }

    pub fn own_p(&self) -> f64 {
        sigmoid(self.own)
    }

    pub fn update(&mut self, cfg: &WindowFusion, t: &Tables, inp: &Inputs) {
        let (open, close) = Self::rates_of(cfg);
        if !self.own.is_finite() {
            self.own = Self::prior(cfg);
        }
        let p = sigmoid(self.own);
        self.own = logit(p * (1.0 - close) + (1.0 - p) * open);
        let gain = if inp.gain.is_finite() { inp.gain } else { 1.0 };
        let k = cfg.temper_scale;
        let mut add = [0.0; CHANNELS];
        add[Chan::Trv as usize] = inp.trv.map_or(0.0, |l| t.temper.trv * gain * l);
        add[Chan::Co2 as usize] = inp.rates.co2.map_or(0.0, |r| k * t.temper.co2 * t.co2.at(r));
        add[Chan::Rh as usize] = inp.rates.rh.map_or(0.0, |r| k * t.temper.rh * t.rh.at(r));
        add[Chan::Temp as usize] = inp.rates.temp.map_or(0.0, |r| k * t.temper.temp * gain * t.temp.at(r));
        if inp.holding {
            for c in [Chan::Trv, Chan::Temp] {
                add[c as usize] = add[c as usize].min(0.0);
            }
        }
        if inp.holding && inp.depressed {
            for c in [Chan::Co2, Chan::Rh] {
                add[c as usize] = if add[c as usize] < 0.0 { add[c as usize] * RECOVERY_DAMP } else { add[c as usize] };
            }
            add[Chan::Level as usize] = cfg.level_llr;
        }
        if let Some((chan, llr)) = inp.pulse {
            add[chan as usize] += llr;
        }
        self.independent = inp.rates.any();
        self.own = (self.own + add.iter().sum::<f64>()).clamp(Self::prior(cfg) - cfg.floor_margin, cfg.logit_max);
        for c in Chan::ALL {
            self.contrib[c as usize] = self.contrib[c as usize] * MEMORY + add[c as usize];
        }
        self.added = add;
    }

    pub fn set_couple(&mut self, shift: f64) {
        self.couple = shift;
        self.contrib[Chan::Couple as usize] = shift;
    }

    pub fn hard(&mut self, cfg: &WindowFusion, open: bool) {
        self.own = if open { cfg.logit_max * 0.6 } else { Self::prior(cfg) - 2.0 };
        self.couple = 0.0;
        self.contrib = [0.0; CHANNELS];
        self.contrib[Chan::Hard as usize] = if open { 1.0 } else { -1.0 };
    }

    pub fn calm(&mut self, cfg: &WindowFusion) {
        self.own = Self::prior(cfg);
        self.contrib = [0.0; CHANNELS];
    }

    fn leaders(&self, open: bool) -> Vec<Chan> {
        let sign = if open { 1.0 } else { -1.0 };
        let mut v: Vec<(Chan, f64)> = Chan::ALL.into_iter().map(|c| (c, sign * self.contrib[c as usize])).filter(|(_, x)| *x >= SHOWN).collect();
        v.sort_by(|a, b| b.1.total_cmp(&a.1));
        v.into_iter().take(4).map(|(c, _)| c).collect()
    }

    pub fn reason(&self, open: bool) -> String {
        let lead = self.leaders(open);
        let head = if lead.is_empty() { if open { "Gesamtlage spricht für ein offenes Fenster".to_string() } else { "keine Hinweise auf ein offenes Fenster".to_string() } } else { lead.iter().map(|c| c.text(open)).collect::<Vec<_>>().join(", ") };
        format!("{head} - Fenster {} (P {:.2})", if open { "offen" } else { "zu" }, self.p())
    }

    pub fn breakdown(&self) -> Value {
        let mut map = serde_json::Map::new();
        for c in Chan::ALL {
            map.insert(c.name().into(), json!((self.contrib[c as usize] * 100.0).round() / 100.0));
        }
        Value::Object(map)
    }

    pub fn view(&self, open: bool) -> Value {
        json!({"p": (self.p() * 1000.0).round() / 1000.0, "own_p": (self.own_p() * 1000.0).round() / 1000.0, "channels": self.breakdown(), "top": self.leaders(open).iter().map(|c| c.name()).collect::<Vec<_>>(), "reason": self.reason(open), "independent": self.independent})
    }
}
