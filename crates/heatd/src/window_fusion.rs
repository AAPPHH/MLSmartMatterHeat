use crate::config::{Physics, WindowFusion};
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
    Ah,
}

const SIGNALS: usize = 4;
const AH_PER_RH_PCT: f64 = 0.154;
const AH_PAIR_MS: i64 = 3 * MIN_MS;

pub fn absolute_humidity(rh_pct: f64, temp_c: f64) -> f64 {
    216.7 * rh_pct / 100.0 * 6.112 * (17.62 * temp_c / (243.12 + temp_c)).exp() / (273.15 + temp_c)
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

fn log_phi(z: f64) -> f64 {
    if z > -4.0 {
        return phi(z).max(1e-300).ln();
    }
    let z2 = z * z;
    -z2 / 2.0 - 0.5 * (2.0 * std::f64::consts::PI).ln() - (-z).ln() + (1.0 - 1.0 / z2 + 3.0 / (z2 * z2)).ln()
}

pub fn onesided(x: f64, scale: f64, sd: f64) -> f64 {
    -scale.ln() + sd * sd / (2.0 * scale * scale) - x / scale + log_phi((x - sd * sd / scale) / sd)
}

fn log_add(a: f64, b: f64) -> f64 {
    let m = a.max(b);
    m + ((a - m).exp() + (b - m).exp()).ln()
}

pub const DOORS: usize = 3;

#[derive(Debug, Clone, Copy, Default, PartialEq)]
pub struct Lik {
    pub modes: [f64; 3],
    pub door: Option<[f64; DOORS]>,
}

impl Lik {
    pub fn flat(modes: [f64; 3]) -> Self {
        Self { modes, door: None }
    }

    pub fn scaled(self, k: f64) -> Self {
        Self { modes: self.modes.map(|x| x * k), door: self.door.map(|d| d.map(|x| x * k)) }
    }

    pub fn marginal(&self, ph: &Physics) -> [f64; 3] {
        let mut m = self.modes;
        m[0] = Lik::closed(std::slice::from_ref(self), ph);
        m
    }

    pub fn closed(liks: &[Lik], ph: &Physics) -> f64 {
        let plain: f64 = liks.iter().map(|l| l.modes[0]).sum();
        let share = ph.door_share.clamp(0.0, 0.99);
        if share <= 0.0 || liks.iter().all(|l| l.door.is_none()) {
            return plain;
        }
        (0..DOORS).fold((1.0 - share).ln() + plain, |acc, g| {
            let joint: f64 = liks.iter().map(|l| l.door.map_or(l.modes[0], |d| d[g])).sum();
            log_add(acc, (share / DOORS as f64).ln() + joint)
        })
    }
}

pub fn gas_modes(o: Obs, excess: f64, ph: &Physics, lambda_h: [f64; 3], source: f64, door_frac: f64) -> Lik {
    let f = |l: f64| {
        let loss = l * excess / 60.0;
        onesided(o.rate + loss, source, (o.sd.powi(2) + (ph.model_floor * loss).powi(2)).sqrt())
    };
    Lik { modes: [f(lambda_h[0]), f(lambda_h[1]), f(lambda_h[2])], door: Some(ph.door_lambda_h.map(|l| f(lambda_h[0] + l * door_frac))) }
}

pub fn temp_modes(o: Obs, delta_k: f64, ph: &Physics, lambda_h: [f64; 3], valve_gain: f64, valve: f64) -> Lik {
    let heat = if valve.is_finite() { valve_gain * valve } else { 0.0 };
    let sd = (o.sd.powi(2) + ph.temp_model_k_min.powi(2)).sqrt();
    let scale = ph.temp_source_k_min + if valve.is_finite() { ph.temp_valve_k_min * valve } else { ph.temp_valve_k_min };
    let f = |l: f64| onesided(o.rate + (ph.temp_base_loss_h + ph.temp_kappa * l) * delta_k / 60.0 - heat, scale, sd);
    Lik::flat([f(lambda_h[0]), f(lambda_h[1]), f(lambda_h[2])])
}

pub fn trv_modes(z: f64, sigma: f64, valve_open: bool, ph: &Physics, beta: f64) -> Lik {
    if !z.is_finite() || !sigma.is_finite() || sigma <= 0.0 {
        return Lik::default();
    }
    let (h, sd) = (ph.trv_censor_k / sigma, ph.trv_sd);
    let open = ph.trv_open_z * if valve_open { 1.0 - beta } else { 1.0 };
    let mean = [0.0, ph.trv_kipp_frac * open, open];
    let cell = |m: f64| phi((z + h - m) / sd) - phi((z - h - m) / sd);
    let base = cell(0.0);
    let diff = |m: f64| {
        let c = cell(m);
        let l = if c < TAIL || base < TAIL { (m * z - m * m / 2.0) / (sd * sd) } else { (c / base).ln() };
        l.clamp(-ph.trv_cap, ph.trv_cap)
    };
    Lik::flat([0.0, diff(mean[1]), diff(mean[2])])
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
    #[serde(default)]
    pub physics: Option<PhysFit>,
}

#[derive(Debug, Clone, Default, Deserialize)]
pub struct PhysFit {
    #[serde(default)]
    pub lambda_open: BTreeMap<String, f64>,
    #[serde(default)]
    pub weight: Option<[f64; 4]>,
    #[serde(default)]
    pub trv_beta: Option<f64>,
    #[serde(default)]
    pub valve_gain_k_min: Option<f64>,
}

#[derive(Debug, Clone, Copy)]
pub struct PhysRoom {
    pub lambda_h: [f64; 3],
    pub weight: [f64; 4],
    pub trv_beta: f64,
    pub valve_gain: f64,
}

impl Tables {
    pub fn phys_room(&self, ph: &Physics, room: &str) -> PhysRoom {
        let fit = self.physics.as_ref();
        let mut lambda_h = ph.lambda_h;
        if let Some(l) = fit.and_then(|f| f.lambda_open.get(room)) {
            lambda_h[2] = *l;
        }
        PhysRoom { lambda_h, weight: fit.and_then(|f| f.weight).unwrap_or(ph.weight), trv_beta: fit.and_then(|f| f.trv_beta).unwrap_or(ph.trv_beta), valve_gain: fit.and_then(|f| f.valve_gain_k_min).unwrap_or(ph.valve_gain_k_min) }
    }
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
    nodes: BTreeMap<u64, [VecDeque<(i64, f64)>; SIGNALS]>,
    floor_ms: i64,
    skip_rh: bool,
}

fn fit_per_min(points: &[(i64, f64)]) -> (f64, f64, f64) {
    let n = points.len() as f64;
    let (mx, my) = (points.iter().map(|p| p.0 as f64).sum::<f64>() / n, points.iter().map(|p| p.1).sum::<f64>() / n);
    let var: f64 = points.iter().map(|p| (p.0 as f64 - mx).powi(2)).sum();
    let slope = if var > 0.0 { points.iter().map(|p| (p.0 as f64 - mx) * (p.1 - my)).sum::<f64>() / var * MIN_MS as f64 } else { 0.0 };
    (slope, my, var / (MIN_MS as f64).powi(2))
}

fn slope_per_min(points: &[(i64, f64)]) -> (f64, f64) {
    let (slope, mean, _) = fit_per_min(points);
    (slope, mean)
}

#[derive(Debug, Clone, Copy)]
pub struct Obs {
    pub rate: f64,
    pub level: f64,
    pub sd: f64,
}

#[derive(Debug, Clone, Copy, Default)]
pub struct Balance {
    pub co2: Option<Obs>,
    pub ah: Option<Obs>,
    pub temp: Option<Obs>,
}

impl Balance {
    pub fn any(&self) -> bool {
        self.co2.is_some() || self.ah.is_some() || self.temp.is_some()
    }
}

impl AirRoom {
    pub fn skip_humidity(&mut self, skip: bool) {
        self.skip_rh = skip;
    }

    fn store(series: &mut VecDeque<(i64, f64)>, ts_ms: i64, value: f64) {
        if series.back().is_some_and(|b| b.0 > ts_ms) {
            return;
        }
        if series.back().is_some_and(|b| b.0 == ts_ms) {
            series.pop_back();
        }
        series.push_back((ts_ms, value));
        while series.front().is_some_and(|s| s.0 < ts_ms - KEEP_MS) {
            series.pop_front();
        }
    }

    pub fn push(&mut self, node: u64, signal: Signal, ts_ms: i64, value: f64) {
        let entry = self.nodes.entry(node).or_default();
        Self::store(&mut entry[signal as usize], ts_ms, value);
        let other = match signal {
            Signal::Rh => Signal::Temp,
            Signal::Temp => Signal::Rh,
            _ => return,
        };
        let Some(&(ots, ov)) = entry[other as usize].back() else { return };
        if (ts_ms - ots).abs() > AH_PAIR_MS {
            return;
        }
        let (rh, t) = if signal == Signal::Rh { (value, ov) } else { (ov, value) };
        Self::store(&mut entry[Signal::Ah as usize], ts_ms.max(ots), absolute_humidity(rh, t));
    }

    pub fn reset(&mut self, now_ms: i64) {
        self.floor_ms = now_ms;
    }

    fn newest(&self, signal: Signal, now: i64) -> Option<&VecDeque<(i64, f64)>> {
        self.nodes.values().map(|n| &n[signal as usize]).filter(|s| s.back().is_some_and(|b| b.0 <= now)).max_by_key(|s| s.back().map(|b| b.0))
    }

    fn points(&self, cfg: &WindowFusion, signal: Signal, now: i64) -> Option<Vec<(i64, f64)>> {
        self.points_in(cfg, signal, now, cfg.window_min)
    }

    fn points_in(&self, cfg: &WindowFusion, signal: Signal, now: i64, window_min: f64) -> Option<Vec<(i64, f64)>> {
        let usable: Vec<(i64, f64)> = self.newest(signal, now)?.iter().copied().filter(|s| s.0 <= now && s.0 > self.floor_ms).collect();
        if usable.last().is_none_or(|l| now - l.0 > (cfg.fresh_min * MIN_MS as f64) as i64) {
            return None;
        }
        let from = now - (window_min * MIN_MS as f64) as i64;
        let inside: Vec<(i64, f64)> = usable.iter().copied().filter(|s| s.0 > from).collect();
        let picked = if inside.len() >= cfg.min_samples { inside } else if usable.len() >= cfg.min_samples { usable[usable.len() - cfg.min_samples..].to_vec() } else { return None };
        let span = picked.last()?.0 - picked[0].0;
        (span >= MIN_SPAN_MS && span <= (cfg.max_window_min.max(window_min) * MIN_MS as f64) as i64).then_some(picked)
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

    pub fn balance(&self, cfg: &WindowFusion, now: i64) -> Balance {
        let ph = &cfg.physics;
        let obs = |signal: Signal, window: f64, sigma: f64| {
            let (rate, level, sxx) = fit_per_min(&self.points_in(cfg, signal, now, window)?);
            (sxx > 0.0).then(|| Obs { rate, level, sd: sigma / sxx.sqrt() })
        };
        Balance { co2: obs(Signal::Co2, cfg.window_min, ph.co2_sigma_ppm), ah: obs(Signal::Ah, cfg.window_min, ph.rh_sigma_pct * AH_PER_RH_PCT).filter(|_| !self.skip_rh), temp: obs(Signal::Temp, ph.temp_window_min, ph.temp_sigma_k) }
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
    pub phys: Option<PhysRoom>,
    pub balance: Balance,
    pub out_c: f64,
    pub valve: f64,
    pub trv_obs: Option<(f64, f64, bool)>,
}

#[derive(Debug, Clone)]
pub struct Posterior {
    pub mu: [f64; 3],
    pub own: f64,
    pub couple: f64,
    pub contrib: [f64; CHANNELS],
    pub added: [f64; CHANNELS],
    pub independent: bool,
}

impl Default for Posterior {
    fn default() -> Self {
        Self { mu: [1.0, 0.0, 0.0], own: f64::NAN, couple: 0.0, contrib: [0.0; CHANNELS], added: [0.0; CHANNELS], independent: false }
    }
}

impl Posterior {
    fn rates_of(cfg: &WindowFusion) -> (f64, f64) {
        (1.0 / cfg.open_every_min.max(1.0), 1.0 / cfg.stay_min.max(1.0))
    }

    pub fn prior(cfg: &WindowFusion) -> f64 {
        if cfg.engine == "physics" {
            return logit(1.0 - Self::stationary(&Self::chain(cfg))[0]);
        }
        let (open, close) = Self::rates_of(cfg);
        logit(open / (open + close))
    }

    pub fn chain(cfg: &WindowFusion) -> [[f64; 3]; 3] {
        let ph = &cfg.physics;
        let r = 1.0 / cfg.open_every_min.max(1.0);
        let (to_kipp, to_open, k_close, o_close) = (r * ph.kipp_share, r * (1.0 - ph.kipp_share), 1.0 / ph.stay_kipp_min.max(1.0), 1.0 / ph.stay_open_min.max(1.0));
        let o_kipp = 0.5 * o_close;
        [[1.0 - to_kipp - to_open, to_kipp, to_open], [k_close, 1.0 - k_close - to_open, to_open], [o_close - o_kipp, o_kipp, 1.0 - o_close]]
    }

    fn stationary(chain: &[[f64; 3]; 3]) -> [f64; 3] {
        let mut mu = [1.0, 0.0, 0.0];
        for _ in 0..400 {
            mu = Self::forward(&mu, chain);
        }
        mu
    }

    fn forward(mu: &[f64; 3], chain: &[[f64; 3]; 3]) -> [f64; 3] {
        let mut out = [0.0; 3];
        for (i, row) in chain.iter().enumerate() {
            for (j, t) in row.iter().enumerate() {
                out[j] += mu[i] * t;
            }
        }
        out
    }

    fn open_p(mu: &[f64; 3]) -> f64 {
        (mu[1] + mu[2]).clamp(1e-12, 1.0 - 1e-12)
    }

    pub fn set_own(&mut self, cfg: &WindowFusion, own: f64) {
        self.own = own;
        if cfg.engine != "physics" {
            return;
        }
        let p = sigmoid(own);
        let share = if self.mu[1] + self.mu[2] > 1e-12 { self.mu[2] / (self.mu[1] + self.mu[2]) } else { 1.0 - cfg.physics.kipp_share };
        self.mu = [1.0 - p, p * (1.0 - share), p * share];
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

    fn update_physics(&mut self, cfg: &WindowFusion, inp: &Inputs, room: &PhysRoom) {
        let ph = &cfg.physics;
        let chain = Self::chain(cfg);
        if !self.own.is_finite() {
            self.mu = Self::stationary(&chain);
            self.own = logit(Self::open_p(&self.mu));
        }
        let pred = Self::forward(&self.mu, &chain);
        let mut ch = [Lik::default(); CHANNELS];
        let w = room.weight;
        let b = &inp.balance;
        if let Some(o) = b.co2 {
            ch[Chan::Co2 as usize] = gas_modes(o, (o.level - cfg.outdoor_co2_ppm).max(0.0), ph, room.lambda_h, ph.co2_source_ppm_min, 1.0).scaled(w[0]);
        }
        if inp.out_c.is_finite() {
            let ah_out = absolute_humidity(ph.outdoor_rh_pct, inp.out_c);
            if let Some(o) = b.ah.filter(|o| (o.level - ah_out).abs() >= ph.ah_gate_g) {
                ch[Chan::Rh as usize] = gas_modes(o, o.level - ah_out, ph, room.lambda_h, ph.ah_source_g_min, ph.door_ah_frac).scaled(w[1]);
            }
            if let Some(o) = b.temp.filter(|o| o.level - inp.out_c >= cfg.dt_floor_k) {
                ch[Chan::Temp as usize] = temp_modes(o, o.level - inp.out_c, ph, room.lambda_h, room.valve_gain, inp.valve).scaled(w[2]);
            }
        }
        if let Some((z, sigma, valve_open)) = inp.trv_obs {
            ch[Chan::Trv as usize] = trv_modes(z, sigma, valve_open, ph, room.trv_beta).scaled(w[3]);
        }
        if inp.holding {
            for c in [Chan::Trv, Chan::Temp] {
                let l = &mut ch[c as usize];
                if l.modes[1].max(l.modes[2]) > l.modes[0] {
                    *l = Lik::flat([l.modes[0]; 3]);
                }
            }
        }
        if let Some((chan, llr)) = inp.pulse {
            ch[chan as usize] = Lik::flat([0.0, llr, llr]);
        }
        if inp.holding && inp.depressed {
            ch[Chan::Level as usize] = Lik::flat([0.0, ph.level_llr, ph.level_llr]);
        }
        let apply = |extra: [f64; 3]| {
            let top = extra.iter().copied().fold(f64::NEG_INFINITY, f64::max);
            let mut post = [0.0; 3];
            for (m, slot) in post.iter_mut().enumerate() {
                *slot = pred[m] * (extra[m] - top).exp();
            }
            let total: f64 = post.iter().sum();
            post.map(|x| x / total)
        };
        let base = logit(Self::open_p(&pred));
        let mut add = [0.0; CHANNELS];
        for c in Chan::ALL {
            if ch[c as usize] != Lik::default() {
                add[c as usize] = logit(Self::open_p(&apply(ch[c as usize].marginal(ph)))) - base;
            }
        }
        let active: Vec<Lik> = ch.iter().copied().filter(|l| *l != Lik::default()).collect();
        let total = [Lik::closed(&active, ph), active.iter().map(|l| l.modes[1]).sum(), active.iter().map(|l| l.modes[2]).sum()];
        let mut post = apply(total);
        let (lo, hi) = (Self::prior(cfg) - cfg.floor_margin, cfg.logit_max);
        let p = Self::open_p(&post);
        let clamped = sigmoid(logit(p).clamp(lo, hi));
        if (clamped - p).abs() > 1e-15 {
            let k = clamped / p;
            post = [1.0 - clamped, post[1] * k, post[2] * k];
        }
        self.mu = post;
        self.own = logit(Self::open_p(&post));
        self.independent = b.any();
        for c in Chan::ALL {
            self.contrib[c as usize] = self.contrib[c as usize] * MEMORY + add[c as usize];
        }
        self.added = add;
    }

    pub fn update(&mut self, cfg: &WindowFusion, t: &Tables, inp: &Inputs) {
        if let (true, Some(room)) = (cfg.engine == "physics", inp.phys.as_ref()) {
            return self.update_physics(cfg, inp, room);
        }
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
        self.set_own(cfg, if open { cfg.logit_max * 0.6 } else { Self::prior(cfg) - 2.0 });
        self.couple = 0.0;
        self.contrib = [0.0; CHANNELS];
        self.contrib[Chan::Hard as usize] = if open { 1.0 } else { -1.0 };
    }

    pub fn calm(&mut self, cfg: &WindowFusion) {
        self.set_own(cfg, Self::prior(cfg));
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
