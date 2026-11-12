use anyhow::{bail, Result};
use serde::Deserialize;

pub type Mat = [f64; 9];

const SQRT2: f64 = std::f64::consts::SQRT_2;
const P_FLOOR: f64 = 1e-300;
const Z_MIN: f64 = 1e-6;
pub const D_MIN: f64 = 1.0;

#[derive(Debug, Clone, Deserialize)]
pub struct AirDoc {
    #[serde(default)]
    pub node: Option<u64>,
    pub noise: f64,
    #[serde(default)]
    pub offset: f64,
}

#[derive(Debug, Clone, Deserialize)]
pub struct EffortDoc {
    pub sigma: f64,
    pub steady_u: f64,
}

#[derive(Debug, Clone, Deserialize)]
pub struct GasDoc {
    #[serde(default)]
    pub node: Option<u64>,
    pub k: f64,
    pub gain: f64,
    pub sigma: f64,
    pub sigma_up: f64,
    pub min_excess: f64,
    pub win_min: f64,
    #[serde(default)]
    pub out: Option<f64>,
    #[serde(default)]
    pub rh_out: Option<f64>,
}

impl GasDoc {
    pub fn window(&self) -> usize {
        self.win_min as usize
    }
}

fn positive(xs: &[f64]) -> bool {
    xs.iter().all(|v| v.is_finite() && *v > 0.0)
}

fn nonneg(xs: &[f64]) -> bool {
    xs.iter().all(|v| v.is_finite() && *v >= 0.0)
}

pub fn check_air(a: &AirDoc) -> bool {
    positive(&[a.noise]) && a.offset.is_finite()
}

pub fn check_effort(e: &EffortDoc) -> bool {
    positive(&[e.sigma, e.steady_u])
}

pub fn check_gas(g: &GasDoc, hum: bool) -> bool {
    let base = if hum { g.rh_out } else { g.out };
    let Some(base) = base else { return false };
    positive(&[g.sigma, g.sigma_up, g.win_min]) && nonneg(&[g.k, g.gain, g.min_excess, base]) && g.win_min.fract() == 0.0 && (!hum || base <= 100.0)
}

pub fn check_house(trv_grid: f64, trv_floor: f64) -> Result<()> {
    if !nonneg(&[trv_grid, trv_floor]) {
        bail!("invalid trv_grid or trv_floor");
    }
    Ok(())
}

pub fn lg_pdf(v: f64, s: f64) -> f64 {
    if s > 0.0 { -0.5 * (v * v / s + (2.0 * std::f64::consts::PI * s).ln()) } else { f64::NAN }
}

pub fn obs_update(x: &mut [f64; 3], p: &mut Mat, hrow: [f64; 3], v: f64, r: f64) -> f64 {
    let mut ph = [0.0; 3];
    for (i, q) in ph.iter_mut().enumerate() {
        *q = p[3 * i] * hrow[0] + p[3 * i + 1] * hrow[1] + p[3 * i + 2] * hrow[2];
    }
    let s = hrow[0] * ph[0] + hrow[1] * ph[1] + hrow[2] * ph[2] + r;
    if s.is_nan() || s <= 0.0 {
        return s;
    }
    let k = [ph[0] / s, ph[1] / s, ph[2] / s];
    for i in 0..3 {
        x[i] += k[i] * v;
    }
    let mut q = [0.0; 9];
    for i in 0..3 {
        for j in 0..3 {
            q[3 * i + j] = p[3 * i + j] - k[i] * ph[j];
        }
    }
    for i in 0..3 {
        for j in 0..3 {
            p[3 * i + j] = 0.5 * (q[3 * i + j] + q[3 * j + i]);
        }
    }
    s
}

pub fn erfc_nr(x: f64) -> f64 {
    let z = x.abs();
    let t = 1.0 / (1.0 + 0.5 * z);
    let r = t * (-z * z - 1.26551223 + t * (1.00002368 + t * (0.37409196 + t * (0.09678418 + t * (-0.18628806 + t * (0.27886807 + t * (-1.13520398 + t * (1.48851587 + t * (-0.82215223 + t * 0.17087277))))))))).exp();
    if x >= 0.0 { r } else { 2.0 - r }
}

pub fn norm_sf(z: f64) -> f64 {
    0.5 * erfc_nr(z / SQRT2)
}

fn norm_pdf(z: f64) -> f64 {
    (-0.5 * z * z).exp() / (2.0 * std::f64::consts::PI).sqrt()
}

pub fn bin_prob(zlo: f64, zhi: f64) -> f64 {
    if zlo >= 0.0 {
        norm_sf(zlo) - norm_sf(zhi)
    } else if zhi <= 0.0 {
        norm_sf(-zhi) - norm_sf(-zlo)
    } else {
        1.0 - norm_sf(zhi) - norm_sf(-zlo)
    }
}

pub fn abs_hum(t: f64, rh: f64) -> f64 {
    if !(t.is_finite() && rh.is_finite()) || t <= -40.0 {
        return f64::NAN;
    }
    216.7 * (rh / 100.0 * 6.112 * (17.62 * t / (243.12 + t)).exp()) / (273.15 + t)
}

#[allow(clippy::too_many_arguments)]
pub fn trv_update(x: &mut [f64; 3], p: &mut Mat, hrow: [f64; 3], y: f64, yhat: f64, noise_var: f64, grid: f64, floor: f64, fallback_var: f64) -> (f64, f64) {
    let mut ph = [0.0; 3];
    for (i, q) in ph.iter_mut().enumerate() {
        *q = p[3 * i] * hrow[0] + p[3 * i + 1] * hrow[1] + p[3 * i + 2] * hrow[2];
    }
    let s = hrow[0] * ph[0] + hrow[1] * ph[1] + hrow[2] * ph[2] + noise_var;
    if s.is_nan() || s <= 0.0 {
        return (f64::NAN, s);
    }
    let sd = s.sqrt();
    let (zlo, zhi) = ((y - 0.5 * grid - yhat) / sd, (y + 0.5 * grid - yhat) / sd);
    let z = bin_prob(zlo, zhi);
    let ll = (if z >= P_FLOOR { z } else { P_FLOOR }).ln();
    if z >= Z_MIN {
        let (pa, pb) = (norm_pdf(zlo), norm_pdf(zhi));
        let r = (pa - pb) / z;
        let var_t = s * (1.0 + (zlo * pa - zhi * pb) / z - r * r);
        if var_t.is_finite() && var_t > 0.0 {
            let var_t = var_t.max(s.min(noise_var + floor * grid * grid / 12.0)).min(s);
            let shift = sd * r;
            for i in 0..3 {
                x[i] += ph[i] / s * shift;
            }
            let c = (s - var_t) / (s * s);
            let mut q = [0.0; 9];
            for i in 0..3 {
                for j in 0..3 {
                    q[3 * i + j] = p[3 * i + j] - c * ph[i] * ph[j];
                }
            }
            for i in 0..3 {
                for j in 0..3 {
                    p[3 * i + j] = 0.5 * (q[3 * i + j] + q[3 * j + i]);
                }
            }
            return (ll, s);
        }
    }
    obs_update(x, p, hrow, y - yhat, fallback_var);
    (ll, s)
}

pub fn gas_coeffs(k: f64, gain: f64, excess: f64) -> (f64, f64) {
    (-k * (1.0 - gain) * excess, -k * gain * excess)
}

pub fn lam_update(x: &mut [f64; 3], p: &mut Mat, c0: f64, c1: f64, obs: f64, sigma: f64) -> f64 {
    let v = obs - (c0 + c1 * x[2]);
    lg_pdf(v, obs_update(x, p, [0.0, 0.0, c1], v, sigma * sigma))
}

pub fn gas_update(x: &mut [f64; 3], p: &mut Mat, c0: f64, c1: f64, obs: f64, sigma: f64, sigma_up: f64) -> f64 {
    let v = obs - (c0 + c1 * x[2]);
    let q = c1 * c1 * p[8];
    let (s_lo, s_hi) = (q + sigma * sigma, q + sigma_up * sigma_up);
    let low = v < 0.0;
    let side = if low { sigma } else { sigma_up };
    obs_update(x, p, [0.0, 0.0, c1], v, side * side);
    0.5 * (2.0 / std::f64::consts::PI).ln() - (s_lo.sqrt() + s_hi.sqrt()).ln() - 0.5 * v * v / if low { s_lo } else { s_hi }
}

pub const FEATURES: [&str; 11] = ["sin_hour", "cos_hour", "weekend", "radiation", "outdoor", "present", "co2_rate", "co2_avail", "hum_rate", "hum_avail", "coffee"];
const HIDDEN_MAX: usize = 64;

#[derive(Debug, Clone, Deserialize)]
pub struct PcnnDoc {
    pub qmax: f64,
    pub features: Vec<String>,
    pub mu: Vec<f64>,
    pub sd: Vec<f64>,
    pub w1: Vec<Vec<f64>>,
    pub b1: Vec<f64>,
    pub w2: Vec<f64>,
    pub b2: f64,
}

#[derive(Debug, Clone, Deserialize)]
pub struct PcnnTop {
    pub active: bool,
    pub coffee_on_mw: f64,
    pub features: Vec<String>,
}

fn same_features(names: &[String]) -> bool {
    names.len() == FEATURES.len() && names.iter().zip(FEATURES).all(|(a, b)| a == b)
}

pub fn check_pcnn(d: &PcnnDoc) -> bool {
    let (n, hidden) = (FEATURES.len(), d.w2.len());
    let finite = |xs: &[f64]| xs.iter().all(|v| v.is_finite());
    same_features(&d.features)
        && d.qmax.is_finite()
        && d.qmax > 0.0
        && (1..=HIDDEN_MAX).contains(&hidden)
        && d.b1.len() == hidden
        && d.w1.len() == hidden
        && d.w1.iter().all(|r| r.len() == n && finite(r))
        && d.mu.len() == n
        && d.sd.len() == n
        && finite(&d.mu)
        && positive(&d.sd)
        && finite(&d.b1)
        && finite(&d.w2)
        && d.b2.is_finite()
}

pub fn check_pcnn_top(t: &PcnnTop) -> bool {
    same_features(&t.features) && nonneg(&[t.coffee_on_mw])
}

#[allow(clippy::too_many_arguments)]
pub fn feature_row(hour: f64, weekend: bool, rad: f64, tout: f64, present: f64, co2_rate: f64, hum_rate: f64, coffee: f64) -> [f64; 11] {
    let ang = 2.0 * std::f64::consts::PI * hour / 24.0;
    let opt = |v: f64| if v.is_finite() { (v, 1.0) } else { (0.0, 0.0) };
    let flag = |v: f64| if v > 0.5 { 1.0 } else { 0.0 };
    let ((co2, co2_ok), (hum, hum_ok)) = (opt(co2_rate), opt(hum_rate));
    [ang.sin(), ang.cos(), if weekend { 1.0 } else { 0.0 }, rad / 100.0, tout, flag(present), co2, co2_ok, hum, hum_ok, flag(coffee)]
}

impl PcnnDoc {
    pub fn q(&self, x: &[f64; 11]) -> f64 {
        if !x.iter().all(|v| v.is_finite()) {
            return 0.0;
        }
        let z: Vec<f64> = x.iter().zip(&self.mu).zip(&self.sd).map(|((v, m), s)| (v - m) / s).collect();
        let mut out = self.b2;
        for ((row, b), w) in self.w1.iter().zip(&self.b1).zip(&self.w2) {
            let mut acc = *b;
            for (wi, zi) in row.iter().zip(&z) {
                acc += wi * zi;
            }
            out += w * acc.tanh();
        }
        self.qmax * out.tanh()
    }
}
