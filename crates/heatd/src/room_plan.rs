use crate::room_model::{norm_cdf, Comp, RoomPar, Shock};

pub const HORIZON: usize = 60;
pub const PROBES: [usize; 3] = [10, 20, 30];
pub const COAST_CAP: usize = 1440;
const COAST_DROP: f64 = 0.5;
pub const BOOSTS: [usize; 4] = [5, 10, 15, 20];
pub const FORECAST_MIN: usize = 20;
pub const PEAK_MARGIN: f64 = 0.5;
const EPS: f64 = 1e-9;

#[derive(Debug, Clone, PartialEq)]
pub struct ProbeMemo {
    pub start: i64,
    pub d: usize,
    pub block_until: i64,
    pub target: f64,
    pub present: f64,
    pub ended: bool,
}

#[derive(Debug, Clone)]
pub struct Env {
    pub tout: f64,
    pub wind: f64,
    pub rad: f64,
    pub night: bool,
    pub ncnt: Vec<f64>,
    pub nsum: Vec<f64>,
    pub target: f64,
    pub present: f64,
    pub sp_cur: Option<f64>,
    pub sp_live: f64,
    pub frost: f64,
    pub boost_c: f64,
    pub boost_ok: bool,
    pub raise_cost: f64,
    pub cold_cost: f64,
    pub r_grid: f64,
    pub lag: Option<[f64; 2]>,
    pub y_last: Option<f64>,
    pub u_now: Option<f64>,
    pub now_min: i64,
    pub probe: Option<ProbeMemo>,
    pub q_net: Option<f64>,
    pub pcnn_active: bool,
}

#[derive(Debug, Clone, PartialEq)]
pub struct PlanOut {
    pub kind: &'static str,
    pub name: String,
    pub setpoint: f64,
    pub raise: bool,
    pub probe_min: Option<usize>,
    pub boost_min: Option<usize>,
    pub energy: f64,
    pub cold: f64,
    pub raise_cost: f64,
    pub cost_live: f64,
    pub total: f64,
    pub p_window: f64,
    pub lam_closed: f64,
    pub lam_open: f64,
    pub mass: f64,
    pub air: f64,
    pub sensor: f64,
    pub forecast: f64,
    pub forecast_phys: f64,
    pub forecast_pcnn: f64,
    pub pcnn_active: bool,
    pub evoi: f64,
    pub best_cost: f64,
    pub perfect_cost: f64,
    pub n_allowed: usize,
    pub coast_min: f64,
    pub stored_min: Option<f64>,
    pub energy_live: f64,
    pub target: f64,
    pub probe_event: Option<&'static str>,
    pub probed_min: usize,
    pub probe_memo: Option<ProbeMemo>,
    pub unusable: bool,
}

struct Run {
    en: Vec<f64>,
    co: Vec<f64>,
    sens: Vec<f64>,
    term: f64,
}

impl Run {
    fn cold(&self) -> f64 {
        self.co.iter().fold(0.0, |x, v| x + v) + self.term
    }
}

struct Cand {
    name: String,
    kind: &'static str,
    first: f64,
    runs: [[Run; 2]; 2],
    mode_cost: [f64; 2],
    energy: f64,
    cold: f64,
    raise: f64,
    raise_flag: bool,
    total: f64,
    peak: f64,
    boost_min: Option<usize>,
    probe_min: Option<usize>,
}

#[allow(clippy::too_many_arguments)]
fn terminal_cold(par: &RoomPar, m: f64, a: f64, lam: f64, env: &Env, cn: f64, cs: f64) -> f64 {
    let deficit = (env.target - par.sensor(m, a)).max(0.0);
    if deficit <= 0.0 || env.present <= 0.0 {
        return 0.0;
    }
    let h = 1.0 / 60.0;
    let [f11, f12, f21, f22, g1, g2, _] = par.terms(lam, 1.0, env.tout, env.wind, env.rad, env.night, cn, cs, h);
    let (mut x, mut y) = (m, a);
    for _ in 0..FORECAST_MIN {
        (x, y) = (f11 * x + f12 * y + g1, f21 * x + f22 * y + g2);
    }
    let rate = (par.sensor(x, y) - par.sensor(m, a)) / FORECAST_MIN as f64;
    0.5 * deficit * (if rate > 1e-12 { deficit / rate } else { COAST_CAP as f64 }).min(COAST_CAP as f64) * env.present
}

fn valve_demand(par: &RoomPar, env: &Env, sens0: f64, setpoint: f64, sens: f64) -> f64 {
    match env.u_now.filter(|u| u.is_finite()) {
        None => (par.g * (setpoint - sens)).clamp(0.0, 1.0),
        Some(u0) => (u0 + par.g * ((setpoint - sens) - (env.sp_cur.unwrap_or(env.sp_live) - sens0))).clamp(0.0, 1.0),
    }
}

#[allow(clippy::too_many_arguments)]
fn rollout(par: &RoomPar, mut m: f64, mut a: f64, lam: f64, s: f64, sp: &[f64], env: &Env, cn: f64, cs: f64, q: f64) -> Run {
    let h = 1.0 / 60.0;
    let hq = h * q;
    let mut run = Run { en: Vec::new(), co: Vec::new(), sens: Vec::new(), term: 0.0 };
    let mut lag = env.lag;
    let sens0 = par.sensor(m, a);
    for &set in sp.iter().take(HORIZON) {
        let u = valve_demand(par, env, sens0, set, par.sensor(m, a));
        let l = par.lag(lag, u);
        lag = Some(l);
        let [f11, f12, f21, f22, g1, g2, _] = par.terms(lam, l[1] * s, env.tout, env.wind, env.rad, env.night, cn, cs, h);
        (m, a) = (f11 * m + f12 * a + g1, f21 * m + f22 * a + g2 + hq);
        let sens = par.sensor(m, a);
        run.sens.push(sens);
        run.en.push(u * s);
        run.co.push((env.target - sens).max(0.0) * env.present);
    }
    run.term = terminal_cold(par, m, a, lam, env, cn, cs);
    run
}

fn profile(first: f64, d: usize, then: f64) -> Vec<f64> {
    (0..HORIZON).map(|k| if k < d { first } else { then }).collect()
}

fn storage(par: &RoomPar, m: f64, a: f64, lam: f64, env: &Env, cn: f64, cs: f64) -> (f64, Option<f64>) {
    let h = 1.0 / 60.0;
    let amb = par.p.b2 + par.p.b3 * env.wind;
    let loss = lam * amb + cn;
    let drive = lam * amb * env.tout + par.p.b4 * env.rad / 100.0 + cs;
    let w = par.p.w;
    let m_eq = (env.target - (1.0 - w) * drive / (par.p.b1 + loss)) / (w + (1.0 - w) * par.p.b1 / (par.p.b1 + loss));
    let a_eq = (par.p.b1 * m_eq + drive) / (par.p.b1 + loss);
    let ts = if env.night { par.p.ts_night } else { par.p.ts };
    let stored = (ts - m_eq > 0.5 && par.p.a1 > 0.0).then(|| ((m - m_eq) + par.p.a2 / par.p.b1 * (a - a_eq)) / (par.p.a1 * (ts - m_eq) / 60.0));
    let floor = env.target - COAST_DROP;
    let [f11, f12, f21, f22, g1, g2, _] = par.terms(lam, 0.0, env.tout, env.wind, env.rad, env.night, cn, cs, h);
    let (mut x, mut y) = (m, a);
    for k in 0..COAST_CAP {
        if y < floor {
            return (k as f64, stored);
        }
        (x, y) = (f11 * x + f12 * y + g1, f21 * x + f22 * y + g2);
    }
    (COAST_CAP as f64, stored)
}

fn separation_minutes(pr: &Cand, sigma_obs: f64) -> usize {
    let (closed, opened) = (&pr.runs[0][0].sens, &pr.runs[1][0].sens);
    (0..HORIZON).find(|&k| (opened[k] - closed[k]).abs() >= 2.0 * sigma_obs).map_or(HORIZON, |k| k + 1)
}

pub fn plan(par: &RoomPar, shock: &Shock, comps: &[Comp], p_supply: f64, env: &Env) -> PlanOut {
    let lam_w = shock.lambda_w;
    let n = comps.len();
    let sum = |f: &dyn Fn(usize) -> f64| (0..n).fold(0.0, |acc, j| acc + f(j));
    let (m, a) = (sum(&|j| comps[j].mu * comps[j].x[0]), sum(&|j| comps[j].mu * comps[j].x[1]));
    let q: Vec<f64> = comps.iter().map(|c| 1.0 - norm_cdf((lam_w - c.x[2]) / c.p[8].max(1e-18).sqrt())).collect();
    let pw = sum(&|j| comps[j].mu * q[j]);
    let wt: Vec<f64> = (0..n).map(|j| comps[j].mu * (1.0 - q[j])).collect();
    let wsum = wt.iter().fold(0.0, |acc, v| acc + v);
    let lam_c = if wsum > 0.0 { sum(&|j| wt[j] * comps[j].x[2].min(lam_w)) / wsum } else { 1.0 };
    let lam_o = if pw > 0.0 { shock.lambda_jump.max(sum(&|j| comps[j].mu * q[j] * comps[j].x[2]) / pw) } else { shock.lambda_jump };
    let (cn, cs) = par.coupling(&env.ncnt, &env.nsum);
    let modes = [(1.0 - pw, lam_c), (pw, lam_o)];
    let supplies = [(1.0, p_supply), (0.0, 1.0 - p_supply)];
    let calm = [1.0, 0.0];
    let (target, frost, cold_w) = (env.target, env.frost, env.cold_cost);
    let cur = env.sp_cur;
    let sensor0 = par.sensor(m, a);
    let limit = (target + PEAK_MARGIN).max(sensor0) + EPS;
    let now = env.now_min;
    let q_net = env.q_net.filter(|q| q.is_finite());
    let net_on = env.pcnn_active && q_net.is_some();
    let q_use = if net_on { q_net.unwrap_or(0.0) } else { 0.0 };
    let unusable = par.unusable || (if env.night { par.p.ts_night } else { par.p.ts }) < target + PEAK_MARGIN;
    let evaluate = |name: &str, kind: &'static str, sp: Vec<f64>, first: f64, boost_min: Option<usize>, probe_min: Option<usize>| -> Cand {
        let run = |mi: usize, si: usize| rollout(par, m, a, modes[mi].1, supplies[si].0, &sp, env, cn, cs, q_use);
        let runs = [[run(0, 0), run(0, 1)], [run(1, 0), run(1, 1)]];
        let mut mode_cost = [0.0; 2];
        for (mi, mc) in mode_cost.iter_mut().enumerate() {
            *mc = (0..2).fold(0.0, |acc, si| acc + supplies[si].1 * (runs[mi][si].en.iter().fold(0.0, |x, v| x + v) + calm[mi] * cold_w * runs[mi][si].cold()));
        }
        let raise_flag = cur.is_some_and(|c| first > c + EPS);
        let raise = if raise_flag { env.raise_cost } else { 0.0 };
        let both = |f: &dyn Fn(&Run) -> f64| (0..2).fold(0.0, |acc, mi| (0..2).fold(acc, |acc2, si| acc2 + modes[mi].0 * supplies[si].1 * f(&runs[mi][si])));
        let energy = both(&|r| r.en.iter().fold(0.0, |x, v| x + v));
        let cold = (0..2).fold(0.0, |acc, si| acc + modes[0].0 * supplies[si].1 * runs[0][si].cold());
        let total = (0..2).fold(0.0, |acc, mi| acc + modes[mi].0 * mode_cost[mi]) + raise;
        let peak = runs[0][0].sens.iter().copied().fold(f64::NEG_INFINITY, f64::max);
        Cand { name: name.to_string(), kind, first, mode_cost: [mode_cost[0] + raise, mode_cost[1] + raise], energy, cold: cold_w * cold, raise, raise_flag, total, peak, boost_min, probe_min, runs }
    };
    let base: [(&'static str, Option<f64>); 6] = [("live", Some(env.sp_live)), ("current", cur), ("target", Some(target)), ("frost", Some(frost)), ("minus", Some(target - 0.5)), ("plus", Some(target + 0.5))];
    let mut cands: Vec<Cand> = Vec::new();
    let mut seen: Vec<f64> = Vec::new();
    for (name, v) in base {
        let Some(v) = v else { continue };
        if seen.iter().any(|s| (v - s).abs() < EPS) {
            continue;
        }
        seen.push(v);
        cands.push(evaluate(name, name, profile(v, HORIZON, v), v, None, None));
    }
    if env.boost_ok {
        for d in BOOSTS {
            cands.push(evaluate(&format!("boost{d}"), "boost", profile(env.boost_c, d, target), env.boost_c, Some(d), None));
        }
    }
    let mut allowed: Vec<usize> = (0..cands.len()).filter(|&i| cands[i].kind == "frost" || cands[i].peak <= limit).collect();
    if allowed.is_empty() {
        let mut best = 0;
        for i in 1..cands.len() {
            if cands[i].peak < cands[best].peak {
                best = i;
            }
        }
        allowed.push(best);
    }
    let mut best = allowed[0];
    for &i in &allowed[1..] {
        if cands[i].total < cands[best].total - 1e-12 {
            best = i;
        }
    }
    let perf = (0..2).fold(0.0, |acc, mi| acc + modes[mi].0 * allowed.iter().map(|&i| cands[i].mode_cost[mi]).fold(f64::INFINITY, f64::min));
    let gain = (cands[best].total - perf).max(0.0);
    let sigma_obs = (env.r_grid + par.noise_r.powi(2)).sqrt();
    let back = if cands[best].kind == "boost" { target } else { cands[best].first };
    let full = |c: &Cand| c.total - c.raise;
    let memo = env.probe.clone();
    let mut running = memo.as_ref().is_some_and(|mm| !mm.ended && now < mm.start + mm.d as i64);
    let (mut event, mut probed, mut new_memo): (Option<&'static str>, usize, Option<ProbeMemo>) = (None, 0, memo.clone());
    if let Some(mm) = memo.as_ref().filter(|mm| !mm.ended) {
        if running && ((mm.target - target).abs() > EPS || (mm.present - env.present).abs() > EPS || gain <= env.raise_cost || unusable) {
            let spent = (now - mm.start) as usize;
            (event, probed, new_memo, running) = (Some("abort"), spent, Some(ProbeMemo { d: spent, ended: true, ..mm.clone() }), false);
        } else if !running {
            (event, probed, new_memo) = (Some("end"), mm.d, Some(ProbeMemo { ended: true, ..mm.clone() }));
        }
    }
    let mut probe: Option<Cand> = None;
    let mut evoi_best: f64 = 0.0;
    if running {
        let left = (memo.as_ref().map_or(0, |mm| mm.start + mm.d as i64) - now) as usize;
        probe = Some(evaluate(&format!("close{left}"), "probe", profile(frost, left, back), frost, None, Some(left)));
    } else if !unusable && new_memo.as_ref().is_none_or(|mm| now >= mm.block_until) {
        for d in PROBES {
            if cands[best].kind == "frost" || back <= frost + EPS {
                break;
            }
            let pr = evaluate(&format!("close{d}"), "probe", profile(frost, d, back), frost, None, Some(d));
            if pr.peak > limit {
                continue;
            }
            let sep = (pr.runs[1][0].sens[d - 1] - pr.runs[0][0].sens[d - 1]).abs() / sigma_obs;
            let rho = 1.0 - 2.0 * norm_cdf(-sep / 2.0);
            let evoi = rho * gain - (full(&pr) - full(&cands[best])).max(0.0);
            if evoi > evoi_best.max(env.raise_cost) + 1e-12 {
                evoi_best = evoi;
                new_memo = Some(ProbeMemo { start: now, d, block_until: now + 2 * d as i64 + separation_minutes(&pr, sigma_obs) as i64, target, present: env.present, ended: false });
                event = Some("start");
                probe = Some(pr);
            }
        }
    }
    let live = &cands[0];
    let anchor = env.y_last.filter(|y| y.is_finite()).map_or(0.0, |y| y - sensor0);
    let live_sp = profile(live.first, HORIZON, live.first);
    let forecast_with = |q: f64| {
        let mut total = 0.0;
        for mode in &modes {
            for supply in &supplies {
                total += mode.0 * supply.1 * rollout(par, m, a, mode.1, supply.0, &live_sp, env, cn, cs, q).sens[FORECAST_MIN - 1];
            }
        }
        anchor + total
    };
    let (forecast_phys, forecast_pcnn) = (forecast_with(0.0), q_net.map_or(f64::NAN, forecast_with));
    let forecast = if net_on { forecast_pcnn } else { forecast_phys };
    let (cost_live, best_cost, n_allowed, energy_live) = (live.total, cands[best].total, allowed.len(), live.energy);
    let chosen = if unusable { live } else { probe.as_ref().unwrap_or(&cands[best]) };
    let (coast_min, stored_min) = storage(par, m, a, lam_c, env, cn, cs);
    PlanOut {
        kind: if unusable { "fit_unusable" } else { chosen.kind },
        name: if unusable { "fit_unusable".to_string() } else { chosen.name.clone() },
        setpoint: chosen.first,
        raise: !unusable && chosen.raise_flag,
        probe_min: chosen.probe_min,
        boost_min: chosen.boost_min,
        energy: chosen.energy,
        cold: chosen.cold,
        raise_cost: if unusable { 0.0 } else { chosen.raise },
        cost_live,
        total: chosen.total,
        p_window: pw,
        lam_closed: lam_c,
        lam_open: lam_o,
        mass: m,
        air: a,
        sensor: sensor0,
        forecast,
        forecast_phys,
        forecast_pcnn,
        pcnn_active: net_on,
        evoi: evoi_best,
        best_cost,
        perfect_cost: perf,
        n_allowed,
        coast_min,
        stored_min,
        energy_live,
        target,
        probe_event: event,
        probed_min: probed,
        probe_memo: new_memo,
        unusable,
    }
}
