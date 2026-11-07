use crate::config::Config;
use anyhow::{bail, Context, Result};
use serde::Deserialize;
use std::collections::{BTreeMap, HashMap, VecDeque};
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};

const MIN_MS: i64 = 60_000;
const FEATURES: usize = 16;
const TAUS: usize = 3;
const MAX_GAP_MIN: i64 = 2880;
const KIND: &str = "window_physics_residual";
const TEMP_RANGE: [f64; 2] = [0.0, 45.0];
const VALVE_RANGE: [f64; 2] = [0.0, 100.0];

#[derive(Debug, Clone, Deserialize)]
pub struct RoomModel {
    pub node: u64,
    pub weight: f64,
    pub coef: Vec<f64>,
    pub intercept: f64,
    pub sigma: f64,
}

#[derive(Debug, Clone, Deserialize)]
struct Ffill {
    temp: f64,
    valve: f64,
    setpoint: f64,
}

#[derive(Debug, Clone, Deserialize)]
struct Valid {
    temp: [f64; 2],
    valve: [f64; 2],
}

#[derive(Debug, Clone, Deserialize)]
pub struct Bundle {
    pub version: String,
    kind: String,
    pub horizon_min: usize,
    pub taus: Vec<f64>,
    pub since_open_cap: f64,
    pub open_valve_pct: f64,
    pub valve_guard_min: usize,
    pub clip: f64,
    pub veto_pad_min: i64,
    pub threshold: f64,
    ffill_min: Ffill,
    valid: Valid,
    pub weather_max_age_min: f64,
    pub rooms: BTreeMap<String, RoomModel>,
}

impl Bundle {
    pub fn parse(text: &str) -> Result<Self> {
        let b: Self = serde_json::from_str(text).context("window bundle")?;
        if b.kind != KIND {
            bail!("window bundle kind {} is not {KIND}", b.kind);
        }
        let positive = [b.since_open_cap, b.clip, b.threshold, b.weather_max_age_min, b.ffill_min.temp, b.ffill_min.valve, b.ffill_min.setpoint, b.open_valve_pct + 1.0];
        if b.taus.len() != TAUS || b.taus.iter().chain(&positive).any(|v| !v.is_finite() || *v <= 0.0) || b.horizon_min == 0 || b.rooms.is_empty() || b.veto_pad_min < 0 {
            bail!("window bundle {}: invalid parameters", b.version);
        }
        for (name, r) in &b.rooms {
            if r.coef.len() != FEATURES || r.coef.iter().chain([&r.intercept, &r.weight]).any(|v| !v.is_finite()) || !r.sigma.is_finite() || r.sigma <= 0.0 {
                bail!("window bundle {}: invalid model for {name}", b.version);
            }
        }
        Ok(b)
    }

    pub fn load(dir: &Path) -> Result<Self> {
        Self::parse(&std::fs::read_to_string(dir.join("bundle.json")).with_context(|| format!("read {}", dir.join("bundle.json").display()))?)
    }

    fn same_shape(&self, o: &Self) -> bool {
        (self.horizon_min, &self.taus, self.since_open_cap, self.open_valve_pct, self.valve_guard_min) == (o.horizon_min, &o.taus, o.since_open_cap, o.open_valve_pct, o.valve_guard_min)
    }
}

pub fn bundle_candidates(base: &Path) -> Vec<(String, PathBuf)> {
    let Ok(dir) = std::fs::read_dir(base) else { return Vec::new() };
    let mut found: Vec<(String, PathBuf)> = dir.flatten().map(|e| (e.file_name().to_string_lossy().into_owned(), e.path())).filter(|(name, path)| chrono::NaiveDateTime::parse_from_str(name, "%Y%m%dT%H%M%SZ").is_ok() && path.join("bundle.json").is_file()).collect();
    found.sort_by(|a, b| b.0.cmp(&a.0));
    found
}

pub fn try_load(base: &str) -> Option<Bundle> {
    bundle_candidates(Path::new(base)).into_iter().find_map(|(_, path)| Bundle::load(&path).ok())
}

#[derive(Debug, Clone, Copy)]
pub struct Shared {
    pub supply: bool,
    pub out: f64,
    pub wind: f64,
    pub rad: f64,
}

#[derive(Debug, Clone)]
pub struct Scored {
    pub ts_ms: i64,
    pub z: BTreeMap<String, f64>,
    pub house: f64,
    pub veto: bool,
}

impl Scored {
    pub fn alarm(&self, threshold: f64) -> bool {
        !self.veto && self.house > threshold
    }
}

#[derive(Debug, Clone, Copy)]
struct Slot {
    temp: f64,
    v: f64,
    sp: f64,
    out: f64,
    so: f64,
    f: [f64; TAUS],
}

#[derive(Debug, Default)]
struct RoomRun {
    hist: VecDeque<Slot>,
    win: VecDeque<f64>,
    run: usize,
    last_open: Option<i64>,
    f: [f64; TAUS],
}

#[derive(Debug)]
pub struct Scorer {
    bundle: Arc<Bundle>,
    rooms: BTreeMap<String, RoomRun>,
    t: i64,
    since_supply: Option<i64>,
}

impl Scorer {
    pub fn new(bundle: Arc<Bundle>) -> Self {
        Self { bundle, rooms: BTreeMap::new(), t: -1, since_supply: None }
    }

    pub fn bundle(&self) -> &Bundle {
        &self.bundle
    }

    pub fn step(&mut self, ts_ms: i64, shared: &Shared, inputs: &BTreeMap<String, [f64; 3]>) -> Scored {
        let b = self.bundle.clone();
        self.t += 1;
        self.since_supply = if shared.supply { Some(0) } else { self.since_supply.map(|s| s + 1) };
        let veto = shared.supply || self.since_supply.is_some_and(|s| s <= b.veto_pad_min);
        let mut z = BTreeMap::new();
        let mut house: Option<f64> = None;
        for (name, model) in &b.rooms {
            let [temp, valve, sp] = inputs.get(name).copied().unwrap_or([f64::NAN; 3]);
            let value = self.room(&b, name, model, self.t, [temp, valve, sp], shared);
            if value.is_finite() {
                house = Some(house.unwrap_or(0.0) + value.max(0.0) * model.weight);
            }
            z.insert(name.clone(), value);
        }
        Scored { ts_ms, z, house: house.unwrap_or(f64::NAN), veto }
    }

    fn room(&mut self, b: &Bundle, name: &str, model: &RoomModel, t: i64, [temp, valve, sp]: [f64; 3], shared: &Shared) -> f64 {
        let h = b.horizon_min;
        let st = self.rooms.entry(name.to_string()).or_default();
        let good = valve.is_finite();
        let v = if good { valve / 100.0 } else { f64::NAN };
        let vz = if good { v } else { 0.0 };
        st.run = if good { st.run + 1 } else { 0 };
        if good && valve > b.open_valve_pct {
            st.last_open = Some(t);
        }
        let so = st.last_open.map_or(b.since_open_cap, |l| ((t - l) as f64).min(b.since_open_cap));
        for (f, tau) in st.f.iter_mut().zip(&b.taus) {
            let a = (-1.0 / tau).exp();
            *f = a * *f + (1.0 - a) * vz;
        }
        st.win.push_back(vz);
        st.hist.push_back(Slot { temp, v, sp, out: shared.out, so, f: st.f });
        if st.hist.len() <= h {
            return f64::NAN;
        }
        let old = st.hist[st.hist.len() - 1 - h];
        st.hist.pop_front();
        while st.win.len() > h {
            st.win.pop_front();
        }
        let vm = st.win.iter().sum::<f64>() / h as f64;
        let s0 = old.temp;
        let mut x = vec![vm];
        for (a, c) in old.f.iter().zip(&st.f) {
            x.extend([*a, *c]);
        }
        x.extend([old.v, v, old.sp - s0, shared.out - s0, old.out - s0, shared.rad / 100.0, (shared.out - s0) * shared.wind, old.so / b.since_open_cap, so / b.since_open_cap]);
        if st.run < h + b.valve_guard_min + 1 || !x.iter().all(|v| v.is_finite()) || !temp.is_finite() {
            return f64::NAN;
        }
        let predicted: f64 = x.iter().zip(&model.coef).map(|(a, c)| a * c).sum::<f64>() + model.intercept;
        (-((temp - s0) - predicted) / model.sigma).min(b.clip)
    }
}

#[derive(Debug, Default)]
struct NodeFeed {
    temp: VecDeque<(i64, f64)>,
    valve: VecDeque<(i64, f64)>,
    setpoint: VecDeque<(i64, f64)>,
}

fn last_at(q: &VecDeque<(i64, f64)>, t_ms: i64, max_age_min: f64) -> f64 {
    q.iter().rev().find(|p| p.0 <= t_ms).filter(|p| (t_ms - p.0) as f64 <= max_age_min * MIN_MS as f64).map_or(f64::NAN, |p| p.1)
}

fn prune(q: &mut VecDeque<(i64, f64)>, t_ms: i64) {
    while q.len() >= 2 && q[1].0 <= t_ms {
        q.pop_front();
    }
}

#[derive(Debug, Default)]
pub struct Detector {
    scorer: Option<Scorer>,
    feeds: HashMap<u64, NodeFeed>,
    cursor: Option<i64>,
    valid: Option<([f64; 2], [f64; 2])>,
}

impl Detector {
    pub fn version(&self) -> Option<&str> {
        self.scorer.as_ref().map(|s| s.bundle.version.as_str())
    }

    pub fn bundle(&self) -> Option<&Bundle> {
        self.scorer.as_ref().map(Scorer::bundle)
    }

    pub fn set_bundle(&mut self, bundle: Arc<Bundle>) {
        self.valid = Some((bundle.valid.temp, bundle.valid.valve));
        match self.scorer.as_mut() {
            Some(s) if s.bundle.same_shape(&bundle) => s.bundle = bundle,
            _ => {
                self.scorer = Some(Scorer::new(bundle));
                self.cursor = None;
            }
        }
    }

    #[cfg(test)]
    pub fn feed_len(&self, node: u64) -> usize {
        self.feeds.get(&node).map_or(0, |f| f.temp.len())
    }

    pub fn push(&mut self, node: u64, attribute: &str, ts_ms: i64, value: f64) {
        let (temp, valve) = self.valid.unwrap_or((TEMP_RANGE, VALVE_RANGE));
        let in_range = |r: [f64; 2]| value.is_finite() && value >= r[0] && value <= r[1];
        let feed = self.feeds.entry(node).or_default();
        let (q, ok) = match attribute {
            crate::window::LOCAL_TEMPERATURE => (&mut feed.temp, in_range(temp)),
            crate::window::PI_HEATING_DEMAND => (&mut feed.valve, in_range(valve)),
            crate::window::OCCUPIED_SETPOINT => (&mut feed.setpoint, in_range(temp)),
            _ => return,
        };
        if ok && q.back().is_none_or(|l| l.0 <= ts_ms) {
            q.push_back((ts_ms, value));
            while q.len() >= 2 && q[1].0 < ts_ms - MAX_GAP_MIN * MIN_MS {
                q.pop_front();
            }
        }
    }

    pub fn advance(&mut self, config: &Config, now_ms: i64, weather: [f64; 3], supply: bool) -> Vec<Scored> {
        let target = now_ms.div_euclid(MIN_MS);
        if self.cursor.is_some_and(|c| target <= c) {
            return Vec::new();
        }
        let Some(scorer) = self.scorer.as_mut() else {
            self.feeds.values_mut().for_each(|f| [&mut f.temp, &mut f.valve, &mut f.setpoint].into_iter().for_each(|q| prune(q, now_ms)));
            return Vec::new();
        };
        let mut cursor = self.cursor.unwrap_or(target - 1);
        if target - cursor > MAX_GAP_MIN {
            *scorer = Scorer::new(scorer.bundle.clone());
            cursor = target - 1;
        }
        let nodes: BTreeMap<String, u64> = scorer.bundle.rooms.iter().filter_map(|(name, m)| {
            let room = config.rooms.iter().find(|r| &r.name == name)?;
            Some((name.clone(), if room.nodes.contains(&m.node) { m.node } else { *room.nodes.first()? }))
        }).collect();
        let b = scorer.bundle.clone();
        let shared = Shared { supply, out: weather[0], wind: weather[1], rad: weather[2] };
        let mut out = Vec::new();
        for minute in cursor + 1..=target {
            let t_ms = minute * MIN_MS;
            let inputs: BTreeMap<String, [f64; 3]> = nodes
                .iter()
                .map(|(name, node)| {
                    let f = self.feeds.get(node);
                    let at = |q: Option<&VecDeque<(i64, f64)>>, age: f64| q.map_or(f64::NAN, |q| last_at(q, t_ms, age));
                    (name.clone(), [at(f.map(|f| &f.temp), b.ffill_min.temp), at(f.map(|f| &f.valve), b.ffill_min.valve), at(f.map(|f| &f.setpoint), b.ffill_min.setpoint)])
                })
                .collect();
            out.push(scorer.step(t_ms, &shared, &inputs));
            self.feeds.values_mut().for_each(|f| [&mut f.temp, &mut f.valve, &mut f.setpoint].into_iter().for_each(|q| prune(q, t_ms)));
        }
        self.cursor = Some(target);
        out
    }
}

pub fn spawn_reload(windows: Arc<Mutex<crate::window::Windows>>, dir: String, every: std::time::Duration) {
    tokio::spawn(async move {
        let mut last_error = String::new();
        loop {
            let current = windows.lock().unwrap().model_version();
            let base = dir.clone();
            let loaded = tokio::task::spawn_blocking(move || -> Result<Option<Bundle>> {
                let candidates = bundle_candidates(Path::new(&base));
                if candidates.is_empty() {
                    bail!("no window bundle in {base}, detection off until one appears");
                }
                let mut failures = Vec::new();
                for (name, path) in candidates {
                    if current.as_deref() == Some(name.as_str()) {
                        return Ok(None);
                    }
                    match Bundle::load(&path) {
                        Ok(bundle) => return Ok(Some(bundle)),
                        Err(e) => failures.push(format!("{name}: {e:#}")),
                    }
                }
                bail!("no loadable window bundle in {base}, keeping the current model ({})", failures.join("; "))
            })
            .await;
            match loaded {
                Ok(Ok(Some(bundle))) => {
                    last_error.clear();
                    eprintln!("window: loaded bundle {} (threshold {:.2})", bundle.version, bundle.threshold);
                    windows.lock().unwrap().set_model(bundle);
                }
                Ok(Err(e)) if last_error != format!("{e:#}") => {
                    last_error = format!("{e:#}");
                    eprintln!("window: {last_error}");
                }
                _ => {}
            }
            tokio::time::sleep(every).await;
        }
    });
}
