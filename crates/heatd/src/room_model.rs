use anyhow::{bail, Context, Result};
use serde::Deserialize;
use crate::room_obs::{abs_hum, check_air, check_effort, check_gas, check_house, gas_coeffs, gas_update, lam_update, lg_pdf, obs_update, trv_update, AirDoc, EffortDoc, GasDoc, Mat};
use std::collections::{BTreeMap, VecDeque};
use std::path::Path;

pub const KIND: &str = "room_model";

#[derive(Debug, Clone, Deserialize)]
pub struct Params {
    pub a1: f64,
    pub a2: f64,
    pub b1: f64,
    pub b2: f64,
    pub b3: f64,
    pub b4: f64,
    pub w: f64,
    pub ts: f64,
    pub ts_night: f64,
    #[serde(default)]
    pub tau_v: f64,
}

#[derive(Debug, Clone, Deserialize)]
pub struct RoomDoc {
    pub node: u64,
    pub params: Params,
    pub c: BTreeMap<String, f64>,
    pub neighbours: BTreeMap<String, Vec<u64>>,
    pub valve_gain: f64,
    pub noise_r: f64,
    pub p0: f64,
    pub q_r: f64,
    pub q_a: f64,
    #[serde(default)]
    pub q_um: f64,
    #[serde(default)]
    pub q_ua: f64,
    #[serde(default)]
    pub air: Vec<AirDoc>,
    #[serde(default)]
    pub effort: Option<EffortDoc>,
    #[serde(default)]
    pub co2: Option<GasDoc>,
    #[serde(default)]
    pub hum: Option<GasDoc>,
}

#[derive(Debug, Clone, Deserialize)]
pub struct Shock {
    pub tau_l: f64,
    pub q_calm: f64,
    pub q_jump: f64,
    pub p_jump: f64,
    pub hold_min: f64,
    pub lambda_jump: f64,
    pub lambda_w: f64,
    pub p_l0: f64,
}

#[derive(Debug, Clone, Deserialize)]
pub struct HouseCfg {
    pub p_up: f64,
    pub p_down: f64,
    pub p_supply0: f64,
    pub r_grid: f64,
    pub reset_gap_min: f64,
    pub reset_sigma: f64,
    pub reset_n: u32,
    pub boost_c: f64,
    pub balance_seed: f64,
    pub night_hours: [f64; 2],
    #[serde(default)]
    pub trv_grid: f64,
    #[serde(default)]
    pub trv_floor: f64,
}

#[derive(Debug, Clone, Deserialize)]
pub struct Bundle {
    pub version: String,
    pub dt_s: f64,
    pub rooms: BTreeMap<String, RoomDoc>,
    pub shock: Shock,
    pub house: HouseCfg,
}

impl Bundle {
    pub fn parse(text: &str) -> Result<Self> {
        let raw: serde_json::Value = serde_json::from_str(text).context("room model bundle")?;
        let kind = raw.get("kind").and_then(|k| k.as_str()).unwrap_or("none");
        if kind != KIND {
            bail!("room model bundle {} has kind {kind}, expected {KIND}", raw.get("version").and_then(|v| v.as_str()).unwrap_or("?"));
        }
        let b: Self = serde_json::from_value(raw).context("room model bundle")?;
        b.validate()?;
        Ok(b)
    }

    pub fn load(dir: &Path) -> Result<Self> {
        Self::parse(&std::fs::read_to_string(dir.join("bundle.json")).with_context(|| format!("read {}", dir.join("bundle.json").display()))?)
    }

    fn validate(&self) -> Result<()> {
        let (s, h) = (&self.shock, &self.house);
        let prob = |p: f64| p.is_finite() && p > 0.0 && p < 1.0;
        let positive = [self.dt_s, s.tau_l, s.hold_min, h.reset_gap_min, h.reset_sigma, h.r_grid + 1.0, h.boost_c];
        let nonneg = [s.q_calm, s.q_jump, s.p_l0];
        if self.rooms.is_empty()
            || !(prob(s.p_jump) && prob(h.p_up) && prob(h.p_down) && prob(h.p_supply0))
            || positive.iter().any(|v| !v.is_finite() || *v <= 0.0)
            || nonneg.iter().any(|v| !v.is_finite() || *v < 0.0)
            || !s.lambda_w.is_finite()
            || s.lambda_w <= 1.0
            || !s.lambda_jump.is_finite()
            || s.lambda_jump <= 1.0
            || s.hold_min < 1.0
            || h.reset_n == 0
            || !h.balance_seed.is_finite()
            || h.night_hours.iter().any(|v| !v.is_finite())
            || check_house(h.trv_grid, h.trv_floor).is_err()
        {
            bail!("room model bundle {}: invalid parameters", self.version);
        }
        for (name, r) in &self.rooms {
            let p = &r.params;
            let values = [p.a1, p.a2, p.b1, p.b2, p.b3, p.b4, p.w, p.ts, p.ts_night, p.tau_v, r.valve_gain, r.noise_r, r.p0, r.q_r, r.q_a, r.q_um, r.q_ua];
            if values.iter().any(|v| !v.is_finite()) || !(p.w >= 0.0 && p.w < 1.0) || p.tau_v < 0.0 || r.valve_gain <= 0.0 || r.noise_r <= 0.0 || r.p0 <= 0.0 || r.q_r < 0.0 || r.q_a < 0.0 || r.q_um < 0.0 || r.q_ua < 0.0 || r.c.values().any(|v| !v.is_finite()) {
                bail!("room model bundle {}: invalid model for {name}", self.version);
            }
            if !r.air.iter().all(check_air) || !r.effort.as_ref().is_none_or(check_effort) || !r.co2.as_ref().is_none_or(|g| check_gas(g, false)) || !r.hum.as_ref().is_none_or(|g| check_gas(g, true)) {
                bail!("room model bundle {}: invalid observation parameters for {name}", self.version);
            }
            if r.neighbours.keys().any(|k| !r.c.contains_key(k)) {
                bail!("room model bundle {}: neighbours of {name} without coupling", self.version);
            }
        }
        Ok(())
    }

}

pub fn bundle_candidates(base: &Path) -> Vec<(String, std::path::PathBuf)> {
    crate::window_model::bundle_candidates(base)
}

pub fn try_load(base: &str) -> Option<Bundle> {
    bundle_candidates(Path::new(base)).into_iter().find_map(|(_, path)| Bundle::load(&path).ok())
}

pub fn erf_as(x: f64) -> f64 {
    let t = 1.0 / (1.0 + 0.3275911 * x.abs());
    let y = 1.0 - (((((1.061405429 * t - 1.453152027) * t) + 1.421413741) * t - 0.284496736) * t + 0.254829592) * t * (-x * x).exp();
    if x >= 0.0 { y } else { -y }
}

pub fn norm_cdf(z: f64) -> f64 {
    0.5 * (1.0 + erf_as(z / std::f64::consts::SQRT_2))
}

fn all_finite(xs: &[f64]) -> bool {
    xs.iter().all(|v| v.is_finite())
}

fn mat_mul(a: &Mat, b: &Mat) -> Mat {
    let mut c = [0.0; 9];
    for i in 0..3 {
        for j in 0..3 {
            c[3 * i + j] = (a[3 * i] * b[j] + a[3 * i + 1] * b[3 + j]) + a[3 * i + 2] * b[6 + j];
        }
    }
    c
}

fn mat_t(a: &Mat) -> Mat {
    let mut c = [0.0; 9];
    for i in 0..3 {
        for j in 0..3 {
            c[3 * i + j] = a[3 * j + i];
        }
    }
    c
}

fn psd_ok(p: &Mat) -> bool {
    if (0..3).any(|i| p[4 * i] < -1e-12) {
        return false;
    }
    (0..3).all(|i| (i + 1..3).all(|j| p[3 * i + j].powi(2) <= p[4 * i] * p[4 * j] * (1.0 + 1e-6) + 1e-12))
}

#[derive(Debug, Clone)]
pub struct RoomPar {
    pub p: Params,
    pub ck: Vec<f64>,
    pub g: f64,
    pub noise_r: f64,
    pub p0: f64,
    pub q_r: f64,
    pub q_a: f64,
    pub alpha: f64,
    pub q_um: f64,
    pub q_ua: f64,
    pub air: Vec<(f64, f64)>,
    pub effort: Option<EffortDoc>,
    pub co2: Option<GasDoc>,
    pub hum: Option<GasDoc>,
}

impl RoomPar {
    pub fn new(doc: &RoomDoc) -> Self {
        let tau = doc.params.tau_v;
        let alpha = if tau > 0.0 { 1.0 - (-1.0 / tau).exp() } else { 1.0 };
        Self { p: doc.params.clone(), ck: doc.c.values().copied().collect(), g: doc.valve_gain,
            noise_r: doc.noise_r,
            p0: doc.p0,
            q_r: doc.q_r,
            q_a: doc.q_a,
            alpha,
            q_um: doc.q_um,
            q_ua: doc.q_ua,
            air: doc.air.iter().map(|a| (a.noise * a.noise, a.offset)).collect(),
            effort: doc.effort.clone(),
            co2: doc.co2.clone(),
            hum: doc.hum.clone(),
        }
    }

    pub fn lag(&self, state: Option<[f64; 2]>, u: f64) -> [f64; 2] {
        match state {
            Some([s1, s2]) if self.alpha != 1.0 => {
                let v1 = s1 + self.alpha * (u - s1);
                [v1, s2 + self.alpha * (v1 - s2)]
            }
            _ => [u, u],
        }
    }

    pub fn coupling(&self, ncnt: &[f64], nsum: &[f64]) -> (f64, f64) {
        let (mut cn, mut cs) = (0.0, 0.0);
        for ((c, n), s) in self.ck.iter().zip(ncnt).zip(nsum) {
            cn += c * n;
            cs += c * s;
        }
        (cn, cs)
    }

    #[allow(clippy::too_many_arguments)]
    pub(crate) fn terms(&self, lam: f64, ueff: f64, tout: f64, wind: f64, rad: f64, night: bool, cn: f64, cs: f64, h: f64) -> [f64; 7] {
        let p = &self.p;
        let au = p.a1 * ueff;
        let ts = if night { p.ts_night } else { p.ts };
        let amb = p.b2 + p.b3 * wind;
        [1.0 + h * (-(au + p.a2)), h * p.a2, h * p.b1, 1.0 + h * (-(p.b1 + (lam * amb + cn))), h * (au * ts), h * (lam * amb * tout + p.b4 * rad / 100.0 + cs), amb]
    }

    #[allow(clippy::too_many_arguments)]
    pub fn effort_coeffs(&self, m: f64, a: f64, ts: f64, wind: f64, tout: f64, rad: f64, cn: f64, cs: f64) -> (f64, f64) {
        let p = &self.p;
        let amb = p.b2 + p.b3 * wind;
        let k = p.a2 / (p.b1 * p.a1 * (ts - m).max(crate::room_obs::D_MIN));
        (k * (cn * a - cs - p.b4 * rad / 100.0), k * amb * (a - tout).max(0.0))
    }

    pub fn sensor(&self, m: f64, a: f64) -> f64 {
        self.p.w * m + (1.0 - self.p.w) * a
    }
}

#[derive(Debug, Clone)]
pub struct RoomIn {
    pub y: f64,
    pub yv: f64,
    pub u: f64,
    pub sp: f64,
    pub tout: f64,
    pub wind: f64,
    pub rad: f64,
    pub night: bool,
    pub ncnt: Vec<f64>,
    pub nsum: Vec<f64>,
    pub ya: Vec<f64>,
    pub co2: f64,
    pub rh: f64,
    pub th: f64,
}

impl RoomIn {
    pub fn blank(kinds: usize) -> Self {
        Self { y: f64::NAN, yv: f64::NAN, u: f64::NAN, sp: f64::NAN, tout: 0.0, wind: 0.0, rad: 0.0, night: false, ncnt: vec![0.0; kinds], nsum: vec![0.0; kinds], ya: Vec::new(), co2: f64::NAN, rh: f64::NAN, th: f64::NAN }
    }

    fn dynamic_ok(&self) -> bool {
        all_finite(&[self.u, self.tout, self.wind, self.rad]) && all_finite(&self.nsum) && all_finite(&self.ncnt)
    }
}

#[derive(Debug, Clone, Copy, Default, PartialEq)]
pub struct Out {
    pub ok: bool,
    pub m: f64,
    pub a: f64,
    pub sensor: f64,
    pub lam: f64,
    pub lam_std: f64,
    pub p_window: f64,
    pub p_supply: f64,
}

#[derive(Debug, Clone, PartialEq)]
pub struct ResetRec {
    pub room: String,
    pub cause: &'static str,
    pub step: u64,
    pub prior: [f64; 3],
}

#[derive(Debug, Clone)]
pub struct Comp {
    pub mu: f64,
    pub x: [f64; 3],
    pub p: Mat,
}

#[derive(Debug, Clone)]
pub struct RoomFilter {
    name: String,
    pub par: RoomPar,
    shock: Shock,
    house: HouseCfg,
    ts_tr: [[f64; 2]; 2],
    phi: f64,
    tm: [[f64; 2]; 2],
    q_l: [f64; 2],
    x: [[f64; 3]; 4],
    p: [Mat; 4],
    nu: [[f64; 2]; 2],
    init: bool,
    since_obs: u64,
    hot: u32,
    flag: Option<&'static str>,
    skipped: bool,
    pending: Option<[f64; 4]>,
    events: Vec<ResetRec>,
    pub comps: Vec<Comp>,
    pub last: Out,
    pub lag: Option<[f64; 2]>,
    hist: [VecDeque<f64>; 2],
}

impl RoomFilter {
    pub fn new(name: &str, doc: &RoomDoc, shock: &Shock, house: &HouseCfg) -> Self {
        let hold = 1.0 / shock.hold_min;
        Self {
            name: name.to_string(),
            par: RoomPar::new(doc),
            shock: shock.clone(),
            house: house.clone(),
            ts_tr: [[1.0 - house.p_up, house.p_up], [house.p_down, 1.0 - house.p_down]],
            phi: (-1.0 / shock.tau_l).exp(),
            tm: [[1.0 - shock.p_jump, shock.p_jump], [hold, 1.0 - hold]],
            q_l: [shock.q_calm.powi(2), shock.q_jump.powi(2)],
            x: [[0.0, 0.0, 1.0]; 4],
            p: [[0.0; 9]; 4],
            nu: [[1.0, 0.0], [1.0, 0.0]],
            init: false,
            since_obs: 0,
            hot: 0,
            flag: None,
            skipped: true,
            pending: None,
            events: Vec::new(),
            comps: Vec::new(),
            last: Out::default(),
            lag: None,
            hist: [VecDeque::new(), VecDeque::new()],
        }
    }

    fn rate(&self, i: usize, win: usize) -> f64 {
        let h = &self.hist[i];
        match (h.front(), h.back()) {
            (Some(a), Some(b)) if h.len() == win + 1 && a.is_finite() && b.is_finite() => (b - a) / win as f64,
            _ => f64::NAN,
        }
    }

    fn gas_evidence(&self, inp: &RoomIn) -> Vec<(f64, f64, GasDoc)> {
        let par = &self.par;
        let mut out = Vec::new();
        if let Some(g) = &par.co2 {
            let (excess, r) = (inp.co2 - g.out.unwrap_or(f64::NAN), self.rate(0, g.window()));
            if excess.is_finite() && r.is_finite() && excess >= g.min_excess {
                out.push((excess, r, g.clone()));
            }
        }
        if let Some(g) = &par.hum {
            let (excess, r) = (abs_hum(inp.th, inp.rh) - abs_hum(inp.tout, g.rh_out.unwrap_or(f64::NAN)), self.rate(1, g.window()));
            if excess.is_finite() && r.is_finite() && excess >= g.min_excess {
                out.push((excess, r, g.clone()));
            }
        }
        out
    }

    #[cfg(test)]
    pub fn corrupt_for_test(&mut self) {
        self.x[0][1] = f64::NAN;
    }

    fn weights(&self, w: [f64; 2]) -> [f64; 4] {
        [w[0] * self.nu[0][0], w[0] * self.nu[0][1], w[1] * self.nu[1][0], w[1] * self.nu[1][1]]
    }

    fn seed(&mut self, y: f64, inp: &RoomIn) {
        let (mut m, mut a) = (y, y);
        if self.house.balance_seed != 0.0 && inp.dynamic_ok() {
            let par = &self.par;
            let (cn, cs) = par.coupling(&inp.ncnt, &inp.nsum);
            let amb = par.p.b2 + par.p.b3 * inp.wind;
            let d = ((amb + cn) * y - amb * inp.tout - cs - par.p.b4 * inp.rad / 100.0) / (par.p.b1 + (amb + cn) * par.p.w);
            a = y - par.p.w * d;
            m = a + d;
        }
        for j in 0..4 {
            self.x[j] = [m, a, 1.0];
            self.p[j] = [self.par.p0, 0.0, 0.0, 0.0, self.par.p0, 0.0, 0.0, 0.0, self.shock.p_l0];
        }
        self.nu = [[1.0, 0.0], [1.0, 0.0]];
        self.init = true;
        self.since_obs = 0;
        self.hot = 0;
    }

    fn reset(&mut self, cause: &'static str, inp: &RoomIn, w: [f64; 2], step: u64) {
        let mu = self.weights(w);
        let prior = if self.init {
            let mut s = [0.0; 3];
            for (i, v) in s.iter_mut().enumerate() {
                *v = (0..4).fold(0.0, |acc, j| acc + mu[j] * self.x[j][i]);
            }
            s
        } else {
            [f64::NAN; 3]
        };
        self.events.push(ResetRec { room: self.name.clone(), cause, step, prior });
        if inp.y.is_finite() {
            self.seed(inp.y, inp);
        } else {
            self.init = false;
        }
    }

    fn update(&mut self, inp: &RoomIn, w: [f64; 2], step: u64) -> Option<([f64; 2], f64)> {
        self.pending = None;
        self.flag = None;
        self.skipped = true;
        if inp.u.is_finite() {
            self.lag = Some(self.par.lag(self.lag, inp.u));
        }
        for (i, v) in [(0, inp.co2), (1, abs_hum(inp.th, inp.rh))] {
            if let Some(win) = [&self.par.co2, &self.par.hum][i].as_ref().map(GasDoc::window) {
                self.hist[i].push_back(v);
                while self.hist[i].len() > win + 1 {
                    self.hist[i].pop_front();
                }
            }
        }
        if !inp.dynamic_ok() {
            self.since_obs += 1;
            return None;
        }
        let y_ok = inp.y.is_finite();
        if !self.init {
            if !y_ok {
                self.since_obs += 1;
                return None;
            }
            self.seed(inp.y, inp);
        } else if y_ok && self.since_obs as f64 > self.house.reset_gap_min {
            self.reset("gap", inp, w, step);
        }
        self.skipped = false;
        let par = self.par.clone();
        let (grid, noise_var) = (self.house.trv_grid, par.noise_r.powi(2));
        let r1 = self.house.r_grid + noise_var;
        let (cn, cs) = par.coupling(&inp.ncnt, &inp.nsum);
        let mu0 = self.weights(w);
        let ybar = (0..4).fold(0.0, |acc, j| acc + mu0[j] * par.sensor(self.x[j][0], self.x[j][1]));
        let eff = par.effort.as_ref().filter(|e| all_finite(&[inp.yv, inp.sp, ybar]) && 0.0 < inp.yv && inp.yv < 1.0 && par.g * (inp.sp - ybar).abs() <= e.steady_u);
        let gas = self.gas_evidence(inp);
        let ts = if inp.night { par.p.ts_night } else { par.p.ts };
        let (mut ll, mut yhat, mut s1) = ([0.0; 4], [0.0; 4], [0.0; 4]);
        for j in 0..4 {
            let (x, p) = (&mut self.x[j], &mut self.p[j]);
            for (&(var, off), &ya) in par.air.iter().zip(&inp.ya) {
                if ya.is_finite() {
                    let v = ya - off - x[1];
                    ll[j] += lg_pdf(v, obs_update(x, p, [0.0, 1.0, 0.0], v, var));
                }
            }
            if y_ok {
                yhat[j] = par.sensor(x[0], x[1]);
                if grid > 0.0 {
                    let (l, s) = trv_update(x, p, [par.p.w, 1.0 - par.p.w, 0.0], inp.y, yhat[j], noise_var, grid, self.house.trv_floor, r1);
                    s1[j] = s;
                    ll[j] += l;
                } else {
                    s1[j] = obs_update(x, p, [par.p.w, 1.0 - par.p.w, 0.0], inp.y - yhat[j], r1);
                    ll[j] += lg_pdf(inp.y - yhat[j], s1[j]);
                }
            }
            if let (Some(e), true) = (eff, j >= 2) {
                let (c0, c1) = par.effort_coeffs(x[0], x[1], ts, inp.wind, inp.tout, inp.rad, cn, cs);
                ll[j] += lam_update(x, p, c0, c1, inp.yv, e.sigma);
            }
            for (excess, rate, g) in &gas {
                let (c0, c1) = gas_coeffs(g.k, g.gain, *excess);
                ll[j] += gas_update(x, p, c0, c1, *rate, g.sigma, g.sigma_up);
            }
        }
        if y_ok {
            let mu = self.weights(w);
            let mean = (0..4).fold(0.0, |acc, j| acc + mu[j] * yhat[j]);
            let var = (0..4).fold(0.0, |acc, j| acc + mu[j] * (s1[j] + (yhat[j] - mean).powi(2)));
            let z = if var.is_finite() && var > 0.0 { (inp.y - mean).abs() / var.sqrt() } else { f64::INFINITY };
            self.hot = if z > self.house.reset_sigma { self.hot + 1 } else { 0 };
            self.since_obs = 0;
        } else {
            self.since_obs += 1;
        }
        if !all_finite(&ll) {
            self.flag = Some("nan");
            return None;
        }
        let top = ll.iter().copied().fold(f64::NEG_INFINITY, f64::max);
        let mut e = [0.0; 4];
        for j in 0..4 {
            e[j] = (ll[j] - top).exp();
        }
        self.pending = Some(e);
        let lik = |s: usize| (0..2).fold(0.0, |acc, m| acc + self.nu[s][m] * e[2 * s + m]);
        Some(([lik(0), lik(1)], top))
    }

    fn finish(&mut self, inp: &RoomIn, w: [f64; 2], step: u64) -> Out {
        if self.skipped {
            return Out::default();
        }
        if let Some(e) = self.pending {
            for s in 0..2 {
                let t = [self.nu[s][0] * e[2 * s], self.nu[s][1] * e[2 * s + 1]];
                let z = t[0] + t[1];
                self.nu[s] = if z > 0.0 { [t[0] / z, t[1] / z] } else { [1.0, 0.0] };
            }
        }
        let mut bad = self.flag;
        if bad.is_none() {
            let finite = self.x.iter().all(|x| all_finite(x)) && self.p.iter().all(|p| all_finite(p)) && self.nu.iter().all(|n| all_finite(n)) && all_finite(&w);
            if !finite {
                bad = Some("nan");
            } else if !self.p.iter().all(psd_ok) {
                bad = Some("psd");
            } else {
                let mu = self.weights(w);
                let lam = (0..4).fold(0.0, |acc, j| acc + mu[j] * self.x[j][2]);
                bad = if !(0.5..=8.0).contains(&lam) {
                    Some("lambda")
                } else if self.hot >= self.house.reset_n {
                    Some("innovation")
                } else {
                    None
                };
            }
        }
        if let Some(cause) = bad {
            self.reset(cause, inp, w, step);
            if !self.init {
                return Out::default();
            }
        }
        let out = self.report(w);
        self.advance(inp, w);
        out
    }

    fn report(&mut self, w: [f64; 2]) -> Out {
        let lam_w = self.shock.lambda_w;
        let mu = self.weights(w);
        let comp = |i: usize| (0..4).fold(0.0, |acc, j| acc + mu[j] * self.x[j][i]);
        let (m, a, lam) = (comp(0), comp(1), comp(2));
        let var = (0..4).fold(0.0, |acc, j| acc + mu[j] * (self.p[j][8] + (self.x[j][2] - lam).powi(2)));
        let pw = (0..4).fold(0.0, |acc, j| acc + mu[j] * (1.0 - norm_cdf((lam_w - self.x[j][2]) / self.p[j][8].max(1e-18).sqrt())));
        self.comps = (0..4).map(|j| Comp { mu: mu[j], x: self.x[j], p: self.p[j] }).collect();
        self.last = Out { ok: true, m, a, sensor: self.par.sensor(m, a), lam, lam_std: var.sqrt(), p_window: pw, p_supply: w[1] };
        self.last
    }

    fn advance(&mut self, inp: &RoomIn, w: [f64; 2]) {
        let (par, h) = (&self.par, 1.0 / 60.0);
        let mu = self.weights(w);
        let (cn, cs) = par.coupling(&inp.ncnt, &inp.nsum);
        let ue = self.lag.map_or(inp.u, |l| l[1]);
        let (mut new_x, mut new_p, mut new_nu) = ([[0.0; 3]; 4], [[0.0; 9]; 4], [[0.0; 2]; 2]);
        for jt in 0..4 {
            let (st, mt) = (jt / 2, jt % 2);
            let om: Vec<f64> = (0..4).map(|j| self.ts_tr[j / 2][st] * self.tm[j % 2][mt] * mu[j]).collect();
            let c = om.iter().fold(0.0, |acc, v| acc + v);
            let (xb, pb) = if c > 0.0 {
                let mut xb = [0.0; 3];
                for (i, v) in xb.iter_mut().enumerate() {
                    *v = (0..4).fold(0.0, |acc, j| acc + om[j] * self.x[j][i]) / c;
                }
                let mut pb = [0.0; 9];
                for i in 0..3 {
                    for k in 0..3 {
                        pb[3 * i + k] = (0..4).fold(0.0, |acc, j| acc + om[j] * (self.p[j][3 * i + k] + (self.x[j][i] - xb[i]) * (self.x[j][k] - xb[k]))) / c;
                    }
                }
                (xb, pb)
            } else {
                (self.x[jt], self.p[jt])
            };
            new_nu[st][mt] = c;
            let [f11, f12, f21, f22, g1, g2, amb] = par.terms(xb[2], ue * st as f64, inp.tout, inp.wind, inp.rad, inp.night, cn, cs, h);
            let fm = [f11, f12, 0.0, f21, f22, h * amb * (inp.tout - xb[1]), 0.0, 0.0, self.phi];
            let mut fp = mat_mul(&mat_mul(&fm, &pb), &mat_t(&fm));
            let ug = ue * st as f64;
            fp[0] += (par.q_r + par.q_um * ug) * h;
            fp[4] += (par.q_a + par.q_ua * ug) * h;
            fp[8] += self.q_l[mt];
            new_x[jt] = [f11 * xb[0] + f12 * xb[1] + g1, f21 * xb[0] + f22 * xb[1] + g2, 1.0 + self.phi * (xb[2] - 1.0)];
            for i in 0..3 {
                for k in 0..3 {
                    new_p[jt][3 * i + k] = 0.5 * (fp[3 * i + k] + fp[3 * k + i]);
                }
            }
        }
        for n in new_nu.iter_mut() {
            let t = n[0] + n[1];
            *n = if t > 0.0 { [n[0] / t, n[1] / t] } else { [1.0, 0.0] };
        }
        self.x = new_x;
        self.p = new_p;
        self.nu = new_nu;
    }
}

#[derive(Debug, Clone)]
pub struct House {
    pub rooms: BTreeMap<String, RoomFilter>,
    ts_tr: [[f64; 2]; 2],
    w_prior: [f64; 2],
    pub w: [f64; 2],
    pub loglik: f64,
    step_n: u64,
    pub resets: Vec<ResetRec>,
}

impl House {
    pub fn new(bundle: &Bundle) -> Self {
        let h = &bundle.house;
        let prior = [1.0 - h.p_supply0, h.p_supply0];
        Self {
            rooms: bundle.rooms.iter().map(|(n, d)| (n.clone(), RoomFilter::new(n, d, &bundle.shock, h))).collect(),
            ts_tr: [[1.0 - h.p_up, h.p_up], [h.p_down, 1.0 - h.p_down]],
            w_prior: prior,
            w: prior,
            loglik: 0.0,
            step_n: 0,
            resets: Vec::new(),
        }
    }

    pub fn step(&mut self, inputs: &BTreeMap<String, RoomIn>) -> BTreeMap<String, Out> {
        let n = self.step_n;
        self.step_n += 1;
        let blank = RoomIn::blank(0);
        let mut wp = self.w_prior;
        let mut scale = 0.0;
        for (name, f) in self.rooms.iter_mut() {
            if let Some((lik, top)) = f.update(inputs.get(name).unwrap_or(&blank), self.w_prior, n) {
                wp = [wp[0] * lik[0], wp[1] * lik[1]];
                scale += top;
            }
        }
        let tot = wp[0] + wp[1];
        if tot > 0.0 && tot.is_finite() {
            self.loglik += scale + tot.ln();
            self.w = [wp[0] / tot, wp[1] / tot];
        } else {
            self.w = self.w_prior;
            for f in self.rooms.values_mut() {
                f.flag = if f.skipped { None } else { Some("nan") };
            }
        }
        let mut outs = BTreeMap::new();
        for (name, f) in self.rooms.iter_mut() {
            outs.insert(name.clone(), f.finish(inputs.get(name).unwrap_or(&blank), self.w, n));
            self.resets.append(&mut f.events);
        }
        let tr = self.ts_tr;
        self.w_prior = [self.w[0] * tr[0][0] + self.w[1] * tr[1][0], self.w[0] * tr[0][1] + self.w[1] * tr[1][1]];
        outs
    }
}
