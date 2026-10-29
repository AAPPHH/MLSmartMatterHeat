use crate::config::{WatchMatchSection, WatchState};
use crate::lake::{self, kind, Event, Sink, WatchInference};
use crate::report::open_lake;
use crate::status::Status;
use anyhow::{anyhow, bail, Context, Result};
use chrono::{NaiveDate, Timelike};
use ort::session::Session;
use ort::value::Tensor;
use serde::Deserialize;
use std::collections::{BTreeMap, HashMap, VecDeque};
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};
use std::time::Duration;

pub const MATCHER_ROOM: &str = "room";
pub const NO_MODEL: &str = "none";
const SIGNAL_KEEP_MS: i64 = 1_800_000;
const NO_SCAN_AFTER_MS: i64 = 120_000;
const HISTORY_KEEP: usize = 32;
const RELOAD_EVERY_TICKS: u64 = 5;
const MAX_STEPS: f64 = 60.0;
const BELIEF_FLOOR: f64 = 1e-4;
const DRIFT_RISE: f64 = 1.5;
const DRIFT_MIN_ABS: f64 = 0.05;
const DRIFT_MIN_ROWS: i64 = 20;

#[derive(Debug, Clone, Deserialize)]
pub struct Scale {
    pub floor: f32,
    pub span: f32,
}

#[derive(Debug, Clone, Deserialize)]
pub struct Smoothing {
    pub kind: String,
    pub stay_prob: f64,
    #[serde(default)]
    pub transition: BTreeMap<String, BTreeMap<String, f64>>,
}

#[derive(Debug, Clone, Deserialize)]
pub struct Spec {
    pub model_version: String,
    pub matcher: String,
    pub rooms: Vec<String>,
    pub senders: Vec<String>,
    pub sentinel_dbm: f32,
    pub scale: Scale,
    pub states: BTreeMap<String, String>,
    pub tau_room: f64,
    pub delta_room: f64,
    pub t_hold_min: i64,
    pub smoothing: Smoothing,
    #[serde(default)]
    pub state_rule: Option<RuleSpec>,
    #[serde(default = "one")]
    pub feature_version: u32,
    #[serde(default)]
    pub sender_bands: Vec<Option<String>>,
    #[serde(default = "default_history_s")]
    pub history_s: f64,
    #[serde(default = "default_history_scans")]
    pub history_scans: usize,
}

pub type Past = (i64, Vec<(String, i32)>);

fn one() -> u32 {
    1
}

fn default_history_s() -> f64 {
    300.0
}

fn default_history_scans() -> usize {
    5
}

fn strongest(aps: &[(String, i32)]) -> HashMap<String, i32> {
    let mut out: HashMap<String, i32> = HashMap::new();
    for (sender, rssi) in aps {
        let e = out.entry(sender.to_lowercase()).or_insert(*rssi);
        *e = (*e).max(*rssi);
    }
    out
}

impl Spec {
    pub fn feature_len(&self) -> usize {
        let n = self.senders.len();
        if self.feature_version >= 2 {
            3 * n + 9
        } else {
            n
        }
    }

    pub fn features(&self, past: &[Past], ts_ms: i64, aps: &[(String, i32)]) -> Vec<f32> {
        let cur = strongest(aps);
        if self.feature_version < 2 {
            return self.senders.iter().map(|s| ((cur.get(&s.to_lowercase()).map_or(self.sentinel_dbm, |r| *r as f32) - self.scale.floor) / self.scale.span).clamp(0.0, 1.0)).collect();
        }
        let norm = |r: f64| ((r - f64::from(self.scale.floor)) / f64::from(self.scale.span)).clamp(0.0, 1.0);
        let sentinel = norm(f64::from(self.sentinel_dbm));
        let recent = &past[past.len().saturating_sub(self.history_scans.saturating_sub(1))..];
        let mut hist: Vec<HashMap<String, i32>> = recent.iter().filter(|(t, _)| (ts_ms - t) as f64 / 1000.0 <= self.history_s).map(|(_, a)| strongest(a)).collect();
        hist.push(cur);
        let keys: Vec<String> = self.senders.iter().map(|s| s.to_lowercase()).collect();
        let now: Vec<f64> = keys.iter().map(|k| hist[hist.len() - 1].get(k).map_or(sentinel, |r| norm(f64::from(*r)))).collect();
        let seen: Vec<Vec<f64>> = keys.iter().map(|k| hist.iter().filter_map(|h| h.get(k)).map(|r| norm(f64::from(*r))).collect()).collect();
        let mut out: Vec<f64> = now.clone();
        out.extend(seen.iter().map(|v| if v.is_empty() { sentinel } else { v.iter().sum::<f64>() / v.len() as f64 }));
        out.extend(seen.iter().map(|v| v.len() as f64 / hist.len() as f64));
        for band in ["2.4", "5", "6"] {
            let idx: Vec<usize> = (0..keys.len()).filter(|i| self.sender_bands.get(*i).and_then(|b| b.as_deref()) == Some(band)).collect();
            let mut vis: Vec<f64> = idx.iter().filter(|i| !seen[**i].is_empty() && hist[hist.len() - 1].contains_key(&keys[**i])).map(|i| now[*i]).collect();
            let count = vis.len();
            vis.sort_by(|a, b| b.total_cmp(a));
            vis.truncate(3);
            out.push(vis.first().copied().unwrap_or(sentinel));
            out.push(if vis.is_empty() { sentinel } else { vis.iter().sum::<f64>() / vis.len() as f64 });
            out.push(if idx.is_empty() { 0.0 } else { count as f64 / idx.len() as f64 });
        }
        out.into_iter().map(|v| v as f32).collect()
    }
}

fn ten_minutes() -> f64 {
    600.0
}

#[derive(Debug, Clone, Deserialize)]
pub struct RuleSpec {
    pub hr_rest_max: f64,
    pub hr_active_min: f64,
    pub motion_active_min: f64,
    pub sleep_hours: [u32; 2],
    #[serde(default = "ten_minutes")]
    pub window_s: f64,
    #[serde(default = "ten_minutes")]
    pub stale_s: f64,
}

#[derive(Debug, Clone, Copy)]
pub struct Rule {
    pub hr_rest_max: f64,
    pub hr_active_min: f64,
    pub motion_active_min: f64,
    pub sleep: (u32, u32),
    pub window_ms: i64,
    pub stale_ms: i64,
}

impl Rule {
    pub fn of(cfg: &WatchState, sleep: (u32, u32)) -> Self {
        Self { hr_rest_max: cfg.hr_rest_max, hr_active_min: cfg.hr_active_min, motion_active_min: cfg.motion_active_min, sleep, window_ms: 600_000, stale_ms: 600_000 }
    }

    pub fn from_spec(s: &RuleSpec) -> Self {
        Self {
            hr_rest_max: s.hr_rest_max,
            hr_active_min: s.hr_active_min,
            motion_active_min: s.motion_active_min,
            sleep: (s.sleep_hours[0] * 60, s.sleep_hours[1] * 60),
            window_ms: (s.window_s * 1000.0) as i64,
            stale_ms: (s.stale_s * 1000.0) as i64,
        }
    }
}

pub struct Model {
    pub spec: Spec,
    sessions: HashMap<String, Mutex<Session>>,
    transition: Vec<Vec<f64>>,
}

fn matrix(spec: &Spec) -> Result<Vec<Vec<f64>>> {
    let n = spec.rooms.len();
    let stay = spec.smoothing.stay_prob;
    if !(stay > 0.0 && stay <= 1.0) {
        bail!("stay_prob {stay} outside (0, 1]");
    }
    let mut out = Vec::with_capacity(n);
    for (i, from) in spec.rooms.iter().enumerate() {
        let weights: Vec<f64> = spec.rooms.iter().enumerate().map(|(j, to)| if i == j { 0.0 } else { spec.smoothing.transition.get(from).and_then(|r| r.get(to)).copied().unwrap_or(0.0) }).collect();
        if weights.iter().any(|w| *w < 0.0 || !w.is_finite()) {
            bail!("negative transition weight from {from}");
        }
        let total: f64 = weights.iter().sum();
        out.push(
            (0..n)
                .map(|j| match (i == j, total > 0.0, n > 1) {
                    (true, ..) if n == 1 => 1.0,
                    (true, ..) => stay,
                    (false, true, _) => (1.0 - stay) * weights[j] / total,
                    (false, false, _) => (1.0 - stay) / (n - 1) as f64,
                })
                .collect(),
        );
    }
    Ok(out)
}

pub fn hash64(text: &str) -> u64 {
    text.bytes().fold(0xcbf29ce484222325, |h, b| (h ^ u64::from(b)).wrapping_mul(0x100000001b3))
}

pub fn features_hash(aps: &[(String, i32)]) -> String {
    let mut pairs: Vec<(String, i32)> = aps.iter().map(|(s, r)| (s.to_lowercase(), *r)).collect();
    pairs.sort();
    format!("{:016x}", hash64(&pairs.iter().map(|(s, r)| format!("{s},{r};")).collect::<String>()))
}

impl Model {
    pub fn load(dir: &Path) -> Result<Self> {
        let path = dir.join("model.json");
        let spec: Spec = serde_json::from_str(&std::fs::read_to_string(&path).with_context(|| format!("read {}", path.display()))?).with_context(|| format!("parse {}", path.display()))?;
        if spec.matcher != MATCHER_ROOM || spec.rooms.is_empty() || spec.senders.is_empty() || spec.states.is_empty() || spec.scale.span.partial_cmp(&0.0) != Some(std::cmp::Ordering::Greater) || spec.smoothing.kind != "hmm" || !(1..=2).contains(&spec.feature_version) || (spec.feature_version == 2 && (spec.sender_bands.len() != spec.senders.len() || spec.history_scans == 0)) {
            bail!("{}: not a valid room model description", path.display());
        }
        let mut sessions = HashMap::new();
        for (state, file) in &spec.states {
            let session = Session::builder()?.commit_from_file(dir.join(file)).with_context(|| format!("load {file}"))?;
            sessions.insert(state.clone(), Mutex::new(session));
        }
        let transition = matrix(&spec)?;
        let model = Self { spec, sessions, transition };
        for state in model.sessions.keys() {
            model.probabilities(state, &vec![0.0; model.spec.feature_len()]).with_context(|| format!("probe template {state}"))?;
        }
        Ok(model)
    }

    pub fn rule(&self) -> Option<Rule> {
        self.spec.state_rule.as_ref().map(Rule::from_spec)
    }

    pub fn knows(&self, aps: &[(String, i32)]) -> bool {
        aps.iter().any(|(a, _)| self.spec.senders.iter().any(|s| s.eq_ignore_ascii_case(a)))
    }

    #[cfg(test)]
    pub fn features(&self, aps: &[(String, i32)]) -> Vec<f32> {
        self.spec.features(&[], 0, aps)
    }

    pub fn template_for(&self, state: &str) -> Option<&str> {
        if matches!(state, "offbody" | "charging") {
            return None;
        }
        self.spec.states.get_key_value(state).or_else(|| self.spec.states.get_key_value("all")).map(|(k, _)| k.as_str())
    }

    pub fn probabilities(&self, template: &str, x: &[f32]) -> Result<Vec<f64>> {
        let mut session = self.sessions.get(template).ok_or_else(|| anyhow!("no template {template}"))?.lock().unwrap();
        let outputs = session.run(ort::inputs!["x" => Tensor::from_array(([1usize, x.len()], x.to_vec()))?])?;
        let (_, p) = outputs["probabilities"].try_extract_tensor::<f32>()?;
        let sum: f64 = p.iter().map(|v| f64::from(*v)).sum();
        if p.len() != self.spec.rooms.len() || sum.partial_cmp(&0.0) != Some(std::cmp::Ordering::Greater) || p.iter().any(|v| !v.is_finite()) {
            bail!("template {template} returned {} values (sum {sum}), expected {} probabilities", p.len(), self.spec.rooms.len());
        }
        Ok(p.iter().map(|v| f64::from(*v) / sum).collect())
    }
}

pub fn forward(prior: Option<(&[f64], f64)>, transition: &[Vec<f64>], emission: &[f64]) -> Vec<f64> {
    let n = emission.len();
    let mut belief = match prior {
        Some((b, dt_min)) if dt_min <= MAX_STEPS => {
            let mut b = b.to_vec();
            for _ in 0..dt_min.round().clamp(1.0, MAX_STEPS) as usize {
                b = (0..n).map(|j| (0..n).map(|i| b[i] * transition[i][j]).sum()).collect();
            }
            b
        }
        _ => vec![1.0 / n as f64; n],
    };
    belief.iter_mut().zip(emission).for_each(|(b, e)| *b *= e);
    let total: f64 = belief.iter().sum();
    if total.partial_cmp(&0.0) != Some(std::cmp::Ordering::Greater) {
        belief = emission.to_vec();
    }
    let total: f64 = belief.iter().sum();
    belief.iter_mut().for_each(|b| *b = (*b / total).max(BELIEF_FLOOR));
    let total: f64 = belief.iter().sum();
    belief.iter().map(|b| b / total).collect()
}

pub fn decide(p: &[f64], tau: f64, delta: f64) -> (usize, f64, Option<&'static str>) {
    let top = p.iter().enumerate().max_by(|a, b| a.1.total_cmp(b.1)).map_or(0, |(i, _)| i);
    let second = p.iter().enumerate().filter(|(i, _)| *i != top).map(|(_, v)| *v).fold(0.0, f64::max);
    let reason = if p[top] < tau {
        Some("low_confidence")
    } else if p[top] - second < delta {
        Some("low_margin")
    } else {
        None
    };
    (top, p[top], reason)
}

#[derive(Debug, Clone, Copy, PartialEq)]
pub enum Signal {
    Hr(f64),
    Motion(f64),
    OnBody(bool),
    Charging(bool),
}

#[derive(Debug, Default)]
pub struct Signals {
    hr: VecDeque<(i64, f64)>,
    motion: VecDeque<(i64, f64)>,
    on_body: Option<(i64, bool)>,
    charging: Option<(i64, bool)>,
}

impl Signals {
    pub fn push(&mut self, ts_ms: i64, signal: Signal) {
        let newest = |q: &VecDeque<(i64, f64)>| q.iter().map(|s| s.0).max().unwrap_or(ts_ms);
        match signal {
            Signal::Hr(v) => self.hr.push_back((ts_ms, v)),
            Signal::Motion(v) => self.motion.push_back((ts_ms, v)),
            Signal::OnBody(v) if self.on_body.is_none_or(|(t, _)| ts_ms >= t) => self.on_body = Some((ts_ms, v)),
            Signal::Charging(v) if self.charging.is_none_or(|(t, _)| ts_ms >= t) => self.charging = Some((ts_ms, v)),
            _ => {}
        }
        for q in [&mut self.hr, &mut self.motion] {
            let cut = newest(q) - SIGNAL_KEEP_MS;
            q.retain(|s| s.0 >= cut);
        }
    }

    fn mean(q: &VecDeque<(i64, f64)>, at_ms: i64, window_ms: i64) -> Option<f64> {
        let v: Vec<f64> = q.iter().filter(|s| s.0 > at_ms - window_ms && s.0 <= at_ms).map(|s| s.1).collect();
        (!v.is_empty()).then(|| v.iter().sum::<f64>() / v.len() as f64)
    }

    pub fn classify(&self, rule: &Rule, at_ms: i64, minute_of_day: u32) -> &'static str {
        let (hr, motion) = (Self::mean(&self.hr, at_ms, rule.window_ms), Self::mean(&self.motion, at_ms, rule.window_ms));
        let fresh = |v: Option<(i64, bool)>| v.filter(|(t, _)| at_ms - t <= rule.stale_ms).map(|(_, b)| b);
        if fresh(self.charging) == Some(true) {
            return "charging";
        }
        if fresh(self.on_body) == Some(false) {
            return "offbody";
        }
        if hr.is_none() && motion.is_none() {
            return "rest";
        }
        let (from, to) = rule.sleep;
        let in_sleep = if from == to { false } else if from < to { (from..to).contains(&minute_of_day) } else { minute_of_day >= from || minute_of_day < to };
        if in_sleep && hr.is_none_or(|h| h <= rule.hr_rest_max) && motion.is_none_or(|m| m < rule.motion_active_min) {
            "sleep"
        } else if hr.is_some_and(|h| h >= rule.hr_active_min) || motion.is_some_and(|m| m >= rule.motion_active_min) {
            "active"
        } else {
            "rest"
        }
    }
}

pub fn local_minute(ts_ms: i64) -> u32 {
    chrono::DateTime::from_timestamp_millis(ts_ms).map_or(0, |t| {
        let local = t.with_timezone(&chrono::Local);
        local.hour() * 60 + local.minute()
    })
}

#[derive(Default)]
struct Device {
    signals: Signals,
    belief: Option<(Vec<f64>, i64)>,
    hold: Option<(String, i64)>,
    history: VecDeque<Past>,
    last_scan_ms: Option<i64>,
    gap_logged: bool,
}

#[derive(Default)]
struct Inner {
    model: Option<Arc<Model>>,
    dir_name: Option<String>,
    devices: HashMap<String, Device>,
    stays: HashMap<String, (String, i64)>,
}

#[derive(Clone)]
pub struct WatchMatch {
    cfg: Arc<WatchMatchSection>,
    rule: Rule,
    sink: Sink,
    status: Status,
    inner: Arc<Mutex<Inner>>,
}

fn match_value(row: &WatchInference, top: &[(String, f64)]) -> serde_json::Value {
    let top_json: Vec<serde_json::Value> = top.iter().map(|(room, p)| serde_json::json!({"room": room, "p": p})).collect();
    serde_json::json!({
        "room": top.first().map(|t| t.0.clone()),
        "p": top.first().map_or(0.0, |t| t.1),
        "abstained": row.abstained,
        "reason": row.reason,
        "top": top_json,
        "model": (row.model_version != NO_MODEL).then(|| row.model_version.clone()),
    })
}

fn newest_dir(base: &Path) -> Option<(String, PathBuf)> {
    let mut found: Vec<(String, String, PathBuf)> = std::fs::read_dir(base)
        .ok()?
        .flatten()
        .filter_map(|e| {
            let text = std::fs::read_to_string(e.path().join("model.json")).ok()?;
            let created = serde_json::from_str::<serde_json::Value>(&text).ok()?["created"].as_str().unwrap_or_default().to_string();
            Some((created, e.file_name().to_string_lossy().into_owned(), e.path()))
        })
        .collect();
    found.sort();
    found.pop().map(|(_, name, path)| (name, path))
}

impl WatchMatch {
    pub fn new(cfg: &WatchMatchSection, sink: Sink, status: Status) -> Self {
        let wm = Self { cfg: Arc::new(cfg.clone()), rule: Rule::of(&cfg.state, cfg.state.sleep_minutes().unwrap_or((1380, 420))), sink, status, inner: Default::default() };
        if cfg.enabled {
            if let Err(e) = wm.reload() {
                eprintln!("watchmatch: no model loaded, matcher abstains: {e:#}");
            }
        }
        wm
    }

    #[cfg(test)]
    pub fn sink(&self) -> &Sink {
        &self.sink
    }

    #[cfg(test)]
    pub fn enabled(&self) -> bool {
        self.cfg.enabled
    }

    pub fn model_version(&self) -> Option<String> {
        self.inner.lock().unwrap().model.as_ref().map(|m| m.spec.model_version.clone())
    }

    pub fn model_rooms(&self) -> Vec<String> {
        self.inner.lock().unwrap().model.as_ref().map(|m| m.spec.rooms.clone()).unwrap_or_default()
    }

    #[cfg(test)]
    pub fn active(&self) -> bool {
        self.cfg.enabled && self.inner.lock().unwrap().model.is_some()
    }

    pub fn reload(&self) -> Result<bool> {
        let base = Path::new(&self.cfg.model_dir);
        let (name, path) = if self.cfg.model_version == "latest" { newest_dir(base) } else { Some((self.cfg.model_version.clone(), base.join(&self.cfg.model_version))) }.ok_or_else(|| anyhow!("no model in {}", base.display()))?;
        {
            let inner = self.inner.lock().unwrap();
            if inner.model.is_some() && inner.dir_name.as_deref() == Some(name.as_str()) {
                return Ok(false);
            }
        }
        let model = Arc::new(Model::load(&path)?);
        eprintln!("watchmatch: loaded model {} from {}", model.spec.model_version, path.display());
        self.status.watch_model(Some(model.spec.model_version.clone()));
        let mut inner = self.inner.lock().unwrap();
        inner.model = Some(model);
        inner.dir_name = Some(name);
        inner.devices.values_mut().for_each(|d| d.belief = None);
        Ok(true)
    }

    pub fn is_anchor(&self, device: &str) -> bool {
        self.cfg.anchor_devices.iter().any(|a| a == device)
    }

    pub fn signals(&self, device: &str, signals: impl IntoIterator<Item = (i64, Signal)>) {
        if !self.cfg.enabled || self.is_anchor(device) {
            return;
        }
        let mut inner = self.inner.lock().unwrap();
        let dev = inner.devices.entry(device.into()).or_default();
        signals.into_iter().for_each(|(ts, s)| dev.signals.push(ts, s));
    }

    #[cfg(test)]
    pub fn scan_at(&self, device: &str, ts_ms: i64, now_ms: i64, minute: u32, aps: &[(String, i32)]) -> Option<WatchInference> {
        self.scan_ranked(device, ts_ms, now_ms, minute, aps).map(|(row, _)| row)
    }

    pub fn scan_match(&self, device: &str, ts_ms: i64, now_ms: i64, aps: &[(String, i32)]) -> Option<serde_json::Value> {
        self.scan_ranked(device, ts_ms, now_ms, local_minute(ts_ms), aps).map(|(row, top)| match_value(&row, &top))
    }

    fn scan_ranked(&self, device: &str, ts_ms: i64, now_ms: i64, minute: u32, aps: &[(String, i32)]) -> Option<(WatchInference, Vec<(String, f64)>)> {
        if !self.cfg.enabled || self.is_anchor(device) || aps.is_empty() || now_ms - ts_ms > self.cfg.scan_fresh_s * 1000 {
            return None;
        }
        let mut inner = self.inner.lock().unwrap();
        let model = inner.model.clone();
        let dev = inner.devices.entry(device.into()).or_default();
        if dev.last_scan_ms.is_some_and(|t| ts_ms <= t) {
            return None;
        }
        dev.last_scan_ms = Some(ts_ms);
        dev.gap_logged = false;
        let rule = model.as_ref().and_then(|m| m.rule()).unwrap_or(self.rule);
        let state = dev.signals.classify(&rule, ts_ms, minute);
        let mut ranked: Vec<(String, f64)> = Vec::new();
        let mut row = WatchInference {
            ts: ts_ms * 1000,
            matcher: MATCHER_ROOM.into(),
            template_id: NO_MODEL.into(),
            abstained: true,
            reason: Some("no_model".into()),
            state: state.into(),
            features_hash: features_hash(aps),
            model_version: NO_MODEL.into(),
            ..Default::default()
        };
        if let Some(model) = model {
            row.model_version = model.spec.model_version.clone();
            match model.template_for(state).filter(|_| model.knows(aps)) {
                None if !model.knows(aps) => row.reason = Some("no_known_ap".into()),
                None => row.reason = Some("unknown_state".into()),
                Some(template) => match model.probabilities(template, &model.spec.features(dev.history.make_contiguous(), ts_ms, aps)) {
                    Err(e) => eprintln!("watchmatch: inference failed: {e:#}"),
                    Ok(emission) => {
                        row.template_id = template.into();
                        let prior = dev.belief.as_ref().map(|(b, t)| (b.as_slice(), (ts_ms - t) as f64 / 60_000.0));
                        let belief = forward(prior, &model.transition, &emission);
                        let (top, confidence, reason) = decide(&belief, model.spec.tau_room, model.spec.delta_room);
                        ranked = model.spec.rooms.iter().cloned().zip(belief.iter().copied()).collect();
                        ranked.sort_by(|a, b| b.1.total_cmp(&a.1));
                        ranked.truncate(3);
                        row.confidence = confidence as f32;
                        row.reason = reason.map(Into::into);
                        row.abstained = reason.is_some();
                        if !row.abstained {
                            row.result_room = Some(model.spec.rooms[top].clone());
                            dev.hold = Some((model.spec.rooms[top].clone(), ts_ms));
                        }
                        dev.belief = Some((belief, ts_ms));
                    }
                },
            }
        }
        dev.history.push_back((ts_ms, aps.to_vec()));
        while dev.history.len() > HISTORY_KEEP {
            dev.history.pop_front();
        }
        drop(inner);
        self.record(&row);
        Some((row, ranked))
    }

    fn record(&self, row: &WatchInference) {
        self.status.watch_inference(&row.matcher, &row.state, row.abstained);
        if let Err(e) = self.sink.append(row.clone()) {
            eprintln!("watch_inference: {e}");
        }
    }

    pub fn minute_tick(&self, now_ms: i64) {
        if !self.cfg.enabled {
            return;
        }
        let version = self.model_version().unwrap_or_else(|| NO_MODEL.into());
        let mut rows = Vec::new();
        {
            let mut inner = self.inner.lock().unwrap();
            let rule = inner.model.as_ref().and_then(|m| m.rule()).unwrap_or(self.rule);
            for dev in inner.devices.values_mut().filter(|d| !d.gap_logged && d.last_scan_ms.is_some_and(|t| now_ms - t >= NO_SCAN_AFTER_MS)) {
                dev.gap_logged = true;
                let state = dev.signals.classify(&rule, now_ms, local_minute(now_ms));
                rows.push(WatchInference {
                    ts: now_ms * 1000,
                    matcher: MATCHER_ROOM.into(),
                    template_id: NO_MODEL.into(),
                    abstained: true,
                    reason: Some("no_scan".into()),
                    state: state.into(),
                    model_version: version.clone(),
                    ..Default::default()
                });
            }
        }
        rows.iter().for_each(|r| self.record(r));
    }

    pub fn note_stay(&self, device: &str, ts_ms: i64, label: Option<&str>) {
        if self.is_anchor(device) {
            return;
        }
        let mut inner = self.inner.lock().unwrap();
        match label.filter(|l| !l.is_empty()) {
            Some(l) if inner.stays.get(device).is_none_or(|s| ts_ms >= s.1) => {
                inner.stays.insert(device.into(), (l.into(), ts_ms));
            }
            Some(_) => {}
            None if inner.stays.get(device).is_some_and(|s| ts_ms > s.1 + self.cfg.label_tolerance_s * 1000) => {
                inner.stays.remove(device);
            }
            None => {}
        }
    }

    pub fn stay(&self) -> Option<(String, String, i64)> {
        let inner = self.inner.lock().unwrap();
        inner.stays.iter().map(|(d, (r, t))| (d.clone(), r.clone(), *t)).max_by_key(|s| s.2)
    }

    pub fn sleeping(&self, now_ms: i64) -> bool {
        let inner = self.inner.lock().unwrap();
        let rule = inner.model.as_ref().and_then(|m| m.rule()).unwrap_or(self.rule);
        inner.devices.values().any(|d| d.signals.classify(&rule, now_ms, local_minute(now_ms)) == "sleep")
    }

    pub fn effective(&self, now_ms: i64) -> Option<Option<(String, i64)>> {
        if !self.cfg.enabled {
            return None;
        }
        let inner = self.inner.lock().unwrap();
        let hold_ms = inner.model.as_ref()?.spec.t_hold_min * 60_000;
        Some(inner.devices.values().filter_map(|d| d.hold.clone()).filter(|(_, t)| now_ms - t <= hold_ms).max_by_key(|(_, t)| *t))
    }

    pub fn spawn_background(&self) {
        if !self.cfg.enabled {
            return;
        }
        let wm = self.clone();
        tokio::spawn(async move {
            let mut tick = tokio::time::interval(Duration::from_secs(60));
            let mut n = 0u64;
            loop {
                tick.tick().await;
                n += 1;
                let w = wm.clone();
                let _ = tokio::task::spawn_blocking(move || {
                    w.minute_tick(lake::now_us() / 1000);
                    if n.is_multiple_of(RELOAD_EVERY_TICKS) && w.cfg.model_version == "latest" {
                        if let Err(e) = w.reload() {
                            eprintln!("watchmatch: reload failed, keeping current model: {e:#}");
                        }
                    }
                })
                .await;
            }
        });
    }
}

#[derive(Debug, Clone, PartialEq)]
pub struct Drift {
    pub matcher: String,
    pub current: f64,
    pub previous: f64,
    pub rows_current: i64,
    pub rows_previous: i64,
}

pub fn drift_of(matcher: &str, current: (i64, i64), previous: (i64, i64)) -> Option<Drift> {
    if current.0 < DRIFT_MIN_ROWS || previous.0 < DRIFT_MIN_ROWS {
        return None;
    }
    let (c, p) = (current.1 as f64 / current.0 as f64, previous.1 as f64 / previous.0 as f64);
    (c > p * DRIFT_RISE && c - p >= DRIFT_MIN_ABS).then(|| Drift { matcher: matcher.into(), current: c, previous: p, rows_current: current.0, rows_previous: previous.0 })
}

pub fn drift_check(root: &Path, today: NaiveDate) -> Result<Vec<Drift>> {
    let midnight = |d: NaiveDate| d.and_hms_opt(0, 0, 0).unwrap().and_utc().timestamp_micros();
    let week = chrono::Duration::days(7);
    let (c0, c1, p0) = (midnight(today - week), midnight(today), midnight(today - week - week));
    let db = open_lake(root, today - week - week - chrono::Duration::days(1), today)?;
    let mut stmt = db.conn.prepare(&format!(
        "SELECT matcher, count(*) FILTER (WHERE epoch_us(ts) >= {c0} AND epoch_us(ts) < {c1}), count(*) FILTER (WHERE epoch_us(ts) >= {c0} AND epoch_us(ts) < {c1} AND abstained),
                count(*) FILTER (WHERE epoch_us(ts) >= {p0} AND epoch_us(ts) < {c0}), count(*) FILTER (WHERE epoch_us(ts) >= {p0} AND epoch_us(ts) < {c0} AND abstained)
         FROM watch_inference WHERE reason IS DISTINCT FROM 'no_scan' GROUP BY matcher ORDER BY matcher"
    ))?;
    let rows = stmt.query_map([], |r| Ok((r.get::<_, String>(0)?, r.get::<_, i64>(1)?, r.get::<_, i64>(2)?, r.get::<_, i64>(3)?, r.get::<_, i64>(4)?)))?.collect::<std::result::Result<Vec<_>, _>>()?;
    Ok(rows.into_iter().filter_map(|(m, cn, ca, pn, pa)| drift_of(&m, (cn, ca), (pn, pa))).collect())
}

pub fn drift_alarm(sink: &Sink) -> Result<usize> {
    let drifts = drift_check(sink.root(), sink.today())?;
    for d in &drifts {
        sink.append(Event::now(kind::MATCHER_DRIFT, "heatd").with_detail(serde_json::json!({"matcher": d.matcher, "current": d.current, "previous": d.previous, "rows_current": d.rows_current, "rows_previous": d.rows_previous})))?;
    }
    Ok(drifts.len())
}
