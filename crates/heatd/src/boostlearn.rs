use anyhow::{anyhow, bail, Context as _, Result};
use serde::{Deserialize, Serialize};
use std::collections::BTreeMap;
use std::path::Path;

const KIND: &str = "boost_learn";
const FEATURES: usize = 12;
const ROUND: f64 = 1000.0;
const MAX_SETPOINT_C: f64 = 24.0;
const MAX_ABORT_C: f64 = 24.0;
const MIN_DOSE_MIN: f64 = 5.0;
const MAX_DOSE_MIN: f64 = 20.0;
const MAX_GROWTH: f64 = 1.1;
const MAX_P_ATTRIBUTABLE: f64 = 0.10;
const MAX_MARGIN_K: f64 = 0.5;
const SCENARIOS: [&str; 2] = ["morning", "homecoming"];
const EPS: f64 = 1e-9;

#[derive(Debug, Clone, Copy, Default, Serialize, Deserialize)]
pub struct Context {
    pub out: f64,
    pub wind: f64,
    pub rad: f64,
    pub start_temp: f64,
    pub gap: f64,
    pub supply_ok: f64,
    pub since_resume: f64,
    pub vm60: f64,
    pub vm180: f64,
    #[serde(default)]
    pub since_open: Option<f64>,
    pub hour: f64,
}

impl Context {
    fn features(&self, since_open_cap: f64) -> [f64; FEATURES] {
        let h = 2.0 * std::f64::consts::PI * self.hour / 24.0;
        [self.out, self.wind, self.rad, self.start_temp, self.gap, self.supply_ok, self.since_resume, self.vm60, self.vm180, self.since_open.unwrap_or(since_open_cap), h.sin(), h.cos()]
    }
}

#[derive(Debug, Clone, Copy, Deserialize)]
pub struct Pi {
    pub kp: f64,
    pub gap0: f64,
}

#[derive(Debug, Clone, Deserialize)]
struct RoomSpec {
    a: f64,
    delta: f64,
    pi: Pi,
    #[serde(default)]
    peak_sigma_k: Option<f64>,
    samples: Vec<[f64; 2]>,
}

#[derive(Debug, Clone, Deserialize)]
struct Kernel {
    shape: f64,
    len_min: usize,
}

#[derive(Debug, Clone, Deserialize)]
struct DoseSpec {
    min_min: f64,
    max_min: f64,
    step_min: f64,
}

#[derive(Debug, Clone, Deserialize)]
struct Risk {
    p_max: f64,
    margin_k: f64,
    peak_sigma_k: f64,
    thompson_margin_k: f64,
}

#[derive(Debug, Clone, Deserialize)]
struct Episode {
    room: String,
    scenario: String,
    x: Vec<f64>,
    dose: f64,
}

#[derive(Debug, Clone, Deserialize)]
struct TrustSpec {
    mean: Vec<f64>,
    std: Vec<f64>,
    weights: Vec<f64>,
    scenario_penalty: f64,
    k: usize,
    radius: f64,
    growth: f64,
    episodes: Vec<Episode>,
}

#[derive(Debug, Clone, Deserialize)]
struct Raw {
    version: String,
    kind: String,
    horizon_min: usize,
    ramp_tau_min: f64,
    dead_zone: f64,
    kernel: Kernel,
    dose: DoseSpec,
    boost_setpoint_c: f64,
    abort_c: f64,
    risk: Risk,
    scenarios: Vec<String>,
    since_open_cap_min: f64,
    rooms: BTreeMap<String, RoomSpec>,
    trust: TrustSpec,
}

#[derive(Debug, Clone)]
struct RoomModel {
    spec: RoomSpec,
    gain: Vec<f64>,
    hrev: Vec<Vec<f64>>,
}

#[derive(Debug, Clone, Serialize, PartialEq)]
pub struct Neighbour {
    pub i: usize,
    pub dist: f64,
    pub dose: f64,
}

#[derive(Debug, Clone, Serialize, PartialEq)]
pub struct Decision {
    pub nearest_dist: Option<f64>,
    pub nearest_dose: Option<f64>,
    pub d_safe: f64,
    pub known: Option<f64>,
    pub n_similar: usize,
    pub allowed: f64,
    pub chosen: f64,
    pub p_overshoot: f64,
    pub p_at_allowed: f64,
    pub p_no_boost: f64,
    pub j: Option<usize>,
    pub neighbours: Vec<Neighbour>,
}

struct Known {
    dose: Option<f64>,
    n_similar: usize,
    neighbours: Vec<Neighbour>,
    nearest: Option<(f64, f64)>,
}

#[derive(Debug, Clone)]
pub struct BoostModel {
    raw: Raw,
    cand: Vec<f64>,
    rooms: BTreeMap<String, RoomModel>,
    z: Vec<[f64; FEATURES]>,
}

fn kernel_rev(mean_delay: f64, len: usize, shape: f64) -> Vec<f64> {
    let theta = mean_delay / shape;
    let h: Vec<f64> = (1..=len).map(|k| (k as f64).powf(shape - 1.0) * (-(k as f64) / theta).exp()).collect();
    let sum: f64 = h.iter().sum();
    h.iter().rev().map(|v| v / sum).collect()
}

fn normal_sf(x: f64) -> f64 {
    0.5 * libm::erfc(x / std::f64::consts::SQRT_2)
}

fn round3(v: f64) -> f64 {
    (v * ROUND).round() / ROUND
}

pub fn ramp(minutes: f64, tau: f64) -> f64 {
    1.0 - (-minutes / tau).exp()
}

impl BoostModel {
    pub fn parse(text: &str) -> Result<Self> {
        let raw: Raw = serde_json::from_str(text).context("boost bundle")?;
        if raw.kind != KIND {
            bail!("boost bundle kind {} is not {KIND}", raw.kind);
        }
        let (d, k, t) = (&raw.dose, &raw.kernel, &raw.trust);
        let finite = [d.min_min, d.max_min, d.step_min, k.shape, raw.ramp_tau_min, raw.boost_setpoint_c, raw.abort_c, raw.dead_zone, raw.since_open_cap_min, raw.risk.p_max, raw.risk.margin_k, raw.risk.peak_sigma_k, raw.risk.thompson_margin_k, t.scenario_penalty, t.radius, t.growth];
        if finite.iter().any(|v| !v.is_finite()) || d.step_min <= 0.0 || d.max_min < d.min_min || k.len_min == 0 || k.shape <= 0.0 || raw.horizon_min == 0 || raw.ramp_tau_min <= 0.0 || raw.risk.peak_sigma_k <= 0.0 || raw.rooms.is_empty() {
            bail!("boost bundle {}: invalid parameters", raw.version);
        }
        let outside = raw.boost_setpoint_c > MAX_SETPOINT_C + EPS
            || raw.abort_c > MAX_ABORT_C + EPS
            || d.min_min < MIN_DOSE_MIN - EPS
            || d.max_min > MAX_DOSE_MIN + EPS
            || t.growth > MAX_GROWTH + EPS
            || raw.risk.p_max > MAX_P_ATTRIBUTABLE + EPS
            || raw.risk.margin_k > MAX_MARGIN_K + EPS
            || raw.scenarios.iter().any(|s| !SCENARIOS.contains(&s.as_str()));
        if outside {
            bail!("boost bundle {}: outside the safety envelope", raw.version);
        }
        let bad_vec = [&t.mean, &t.std, &t.weights].iter().any(|v| v.len() != FEATURES || v.iter().any(|x| !x.is_finite()));
        if bad_vec || t.std.iter().any(|s| *s <= 0.0) || t.episodes.iter().any(|e| e.x.len() != FEATURES || !e.dose.is_finite() || e.x.iter().any(|v| !v.is_finite())) {
            bail!("boost bundle {}: invalid trust table", raw.version);
        }
        let mut rooms = BTreeMap::new();
        for (name, spec) in &raw.rooms {
            let fits = spec.samples.iter().all(|s| s.iter().all(|v| v.is_finite())) && [spec.a, spec.delta, spec.pi.kp, spec.pi.gap0].iter().all(|v| v.is_finite()) && spec.peak_sigma_k.is_none_or(|s| s.is_finite() && s > 0.0);
            if spec.samples.is_empty() || !fits {
                bail!("boost bundle {}: invalid model for {name}", raw.version);
            }
            let gain = spec.samples.iter().map(|s| s[0].exp()).collect();
            let hrev = spec.samples.iter().map(|s| kernel_rev(s[1].exp(), k.len_min, k.shape)).collect();
            rooms.insert(name.clone(), RoomModel { spec: spec.clone(), gain, hrev });
        }
        let steps = ((d.max_min - d.min_min) / d.step_min + 1e-9).floor() as usize;
        let cand = (0..=steps).map(|i| d.min_min + i as f64 * d.step_min).collect();
        let z = t.episodes.iter().map(|e| std::array::from_fn(|i| (e.x[i] - t.mean[i]) / t.std[i])).collect();
        Ok(Self { raw, cand, rooms, z })
    }

    pub fn load(dir: &Path) -> Result<Self> {
        let path = dir.join("bundle.json");
        Self::parse(&std::fs::read_to_string(&path).with_context(|| format!("read {}", path.display()))?)
    }

    pub fn version(&self) -> &str {
        &self.raw.version
    }

    pub fn boost_setpoint(&self) -> f64 {
        self.raw.boost_setpoint_c
    }

    pub fn abort_c(&self) -> f64 {
        self.raw.abort_c
    }

    pub fn dead_zone(&self) -> f64 {
        self.raw.dead_zone
    }

    pub fn ramp_tau(&self) -> f64 {
        self.raw.ramp_tau_min
    }

    pub fn kernel_len(&self) -> usize {
        self.raw.kernel.len_min
    }

    pub fn dose_min(&self) -> f64 {
        self.raw.dose.min_min
    }

    pub fn since_open_cap(&self) -> f64 {
        self.raw.since_open_cap_min
    }

    pub fn has_room(&self, room: &str) -> bool {
        self.rooms.contains_key(room)
    }

    pub fn scenario_allowed(&self, scenario: &str) -> bool {
        self.raw.scenarios.iter().any(|s| s == scenario)
    }

    #[cfg(test)]
    pub fn doses(&self) -> &[f64] {
        &self.cand
    }

    fn peaks(&self, room: &RoomModel, ctx: &Context, target: f64, hist: Option<&[f64]>, doses: &[f64]) -> Vec<Vec<f64>> {
        let (len, horizon) = (self.raw.kernel.len_min, self.raw.horizon_min);
        let mut start = vec![0.0; len];
        match hist {
            Some(h) => {
                let h = &h[h.len().saturating_sub(len)..];
                start[len - h.len()..].copy_from_slice(h);
            }
            None if ctx.vm60 > 0.0 => start[len.saturating_sub(60)..].fill(ctx.vm60 / 60.0),
            None => {}
        }
        let ramps: Vec<f64> = (0..horizon).map(|t| ramp(ctx.since_resume + t as f64, self.raw.ramp_tau_min)).collect();
        let spec = &room.spec;
        let reference = ctx.out + spec.delta;
        let mut out = vec![vec![0.0; room.gain.len()]; doses.len()];
        let mut ring = vec![0.0; len + horizon];
        for (di, dose) in doses.iter().enumerate() {
            for (si, slot) in out[di].iter_mut().enumerate() {
                ring[..len].copy_from_slice(&start);
                let (g, h) = (room.gain[si], &room.hrev[si]);
                let mut cur = ctx.start_temp;
                let mut peak = cur;
                for t in 0..horizon {
                    if t > 0 {
                        let drive: f64 = ring[t..t + len].iter().zip(h).map(|(a, b)| a * b).sum();
                        cur = (1.0 - spec.a) * cur + spec.a * reference + g * drive;
                        peak = peak.max(cur);
                    }
                    let boost = (dose - t as f64).clamp(0.0, 1.0);
                    let thermostat = (spec.pi.kp * (target - cur - spec.pi.gap0)).clamp(0.0, 1.0);
                    ring[len + t] = if boost > 0.0 { boost } else { thermostat } * ramps[t];
                }
                *slot = peak;
            }
        }
        for d in 1..out.len() {
            for s in 0..out[d].len() {
                out[d][s] = out[d][s].max(out[d - 1][s]);
            }
        }
        out
    }

    fn exceed(&self, room: &RoomModel, rows: &[&Vec<f64>], target: f64) -> Vec<f64> {
        let limit = (target + self.raw.risk.margin_k).min(self.raw.abort_c);
        let sigma = room.spec.peak_sigma_k.unwrap_or(self.raw.risk.peak_sigma_k);
        rows.iter().map(|row| row.iter().map(|p| normal_sf((limit - p) / sigma)).sum::<f64>() / row.len() as f64).collect()
    }

    fn room(&self, room: &str) -> Result<&RoomModel> {
        self.rooms.get(room).ok_or_else(|| anyhow!("room {room} not in boost bundle"))
    }

    pub fn risk(&self, room: &str, ctx: &Context, target: f64, hist: Option<&[f64]>) -> Result<(Vec<f64>, Vec<Vec<f64>>)> {
        let model = self.room(room)?;
        let pk = self.peaks(model, ctx, target, hist, &self.cand);
        Ok((self.exceed(model, &pk.iter().collect::<Vec<_>>(), target), pk))
    }

    fn known(&self, ctx: &Context, room: &str, scenario: &str) -> Known {
        let t = &self.raw.trust;
        let f = ctx.features(self.raw.since_open_cap_min);
        let z: [f64; FEATURES] = std::array::from_fn(|i| (f[i] - t.mean[i]) / t.std[i]);
        let own: Vec<(usize, f64)> = t
            .episodes
            .iter()
            .enumerate()
            .filter(|(_, e)| e.room == room)
            .map(|(i, e)| {
                let sq: f64 = (0..FEATURES).map(|k| (self.z[i][k] - z[k]).powi(2) * t.weights[k]).sum();
                (i, (sq + if e.scenario == scenario { 0.0 } else { t.scenario_penalty.powi(2) }).sqrt())
            })
            .collect();
        let mut near: Vec<(usize, f64)> = own.iter().copied().filter(|(_, d)| *d <= t.radius).collect();
        if near.is_empty() {
            let nearest = own.iter().copied().fold(None, |best: Option<(usize, f64)>, c| if best.is_none_or(|b| c.1 < b.1) { Some(c) } else { best });
            return Known { dose: None, n_similar: 0, neighbours: Vec::new(), nearest: nearest.map(|(i, d)| (round3(d), round3(t.episodes[i].dose))) };
        }
        near.sort_by(|a, b| a.1.total_cmp(&b.1));
        let known = near.iter().map(|(i, _)| t.episodes[*i].dose).fold(f64::NEG_INFINITY, f64::max);
        let neighbours = near.iter().take(t.k).map(|(i, d)| Neighbour { i: *i, dist: round3(*d), dose: round3(t.episodes[*i].dose) }).collect();
        Known { dose: Some(known), n_similar: near.len(), neighbours, nearest: None }
    }

    fn safe_dose(&self, p: &[f64], p_no_boost: f64) -> f64 {
        p.iter().rposition(|v| *v - p_no_boost <= self.raw.risk.p_max).map_or(0.0, |i| self.cand[i])
    }

    pub fn manual(&self, room: &str, ctx: &Context, target: f64, hist: Option<&[f64]>, minutes: f64) -> Result<Decision> {
        let model = self.room(room)?;
        let (p, _) = self.risk(room, ctx, target, hist)?;
        let p_no_boost = self.exceed(model, &[&self.peaks(model, ctx, target, hist, &[0.0])[0]], target)[0];
        let p_dose = self.exceed(model, &[&self.peaks(model, ctx, target, hist, &[minutes])[0]], target)[0];
        Ok(Decision { nearest_dist: None, nearest_dose: None, d_safe: self.safe_dose(&p, p_no_boost), known: None, n_similar: 0, allowed: minutes, chosen: minutes, p_overshoot: p_dose, p_at_allowed: p_dose, p_no_boost, j: None, neighbours: Vec::new() })
    }

    pub fn decide(&self, room: &str, ctx: &Context, scenario: &str, target: f64, hist: Option<&[f64]>, pick: &dyn Fn(usize) -> usize) -> Result<Decision> {
        let model = self.room(room)?;
        let (p, pk) = self.risk(room, ctx, target, hist)?;
        let dmin = self.raw.dose.min_min;
        let p_no_boost = self.exceed(model, &[&self.peaks(model, ctx, target, hist, &[0.0])[0]], target)[0];
        let d_safe = self.safe_dose(&p, p_no_boost);
        let Known { dose: known, n_similar, neighbours, nearest } = self.known(ctx, room, scenario);
        let cap = d_safe.min(known.map_or(dmin, |k| dmin.max(self.raw.trust.growth * k)));
        let allowed = if self.scenario_allowed(scenario) && ctx.supply_ok > 0.0 && cap >= dmin - 1e-9 { cap } else { 0.0 };
        let (mut chosen, mut p_chosen, mut p_allowed, mut j) = (0.0, p[0], p[0], None);
        if allowed > 0.0 {
            let n = self.cand.partition_point(|c| *c < allowed + 1e-9);
            let top: Vec<f64> = self.peaks(model, ctx, target, hist, &[allowed])[0].iter().zip(&pk[n - 1]).map(|(a, b)| a.max(*b)).collect();
            let mut doses: Vec<f64> = self.cand[..n].to_vec();
            doses.push(allowed);
            let mut grid: Vec<&Vec<f64>> = pk[..n].iter().collect();
            grid.push(&top);
            let col = pick(top.len()).min(top.len() - 1);
            let k = grid.iter().rposition(|row| row[col] <= target + self.raw.risk.thompson_margin_k).unwrap_or(0);
            chosen = doses[k];
            let probs = self.exceed(model, &[grid[k], grid[grid.len() - 1]], target);
            (p_chosen, p_allowed, j) = (probs[0], probs[1], Some(col));
        }
        Ok(Decision { nearest_dist: nearest.map(|n| n.0), nearest_dose: nearest.map(|n| n.1), d_safe, known, n_similar, allowed, chosen, p_overshoot: p_chosen, p_at_allowed: p_allowed, p_no_boost, j, neighbours })
    }
}

#[cfg(test)]
pub fn try_load(base: &str) -> Option<BoostModel> {
    crate::window_model::bundle_candidates(Path::new(base)).into_iter().find_map(|(_, path)| BoostModel::load(&path).ok())
}

#[cfg(test)]
#[path = "boostlearn_tests.rs"]
pub(crate) mod tests;
