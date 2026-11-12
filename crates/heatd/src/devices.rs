use crate::config::{parse_minutes, Config, Room};
use crate::http::AppState;
use crate::lake::{self, kind, DeviceReading, Event, Label, Sink};
use crate::storage::Motion;
use crate::report::{dur, hhmm, midnight_us, open_lake, rows};
use anyhow::{anyhow, bail, Result};
use chrono::{Duration as Span, NaiveDate, NaiveDateTime, Timelike};
use matter_controller::{
    CaptureItem, ANY_ATTRIBUTE, DeviceFeed, DeviceSubscription, FastSubscription, SwitchOn, TypedValue, AIR_QUALITY_CLUSTER, CO2_CLUSTER, ELECTRICAL_ENERGY_MEASUREMENT_CLUSTER, ELECTRICAL_POWER_MEASUREMENT_CLUSTER,
    HUMIDITY_MEASUREMENT_CLUSTER, ON_OFF_CLUSTER, PM25_CLUSTER, POWER_SOURCE_CLUSTER, TEMPERATURE_MEASUREMENT_CLUSTER,
};
use serde::Deserialize;
use serde_json::{json, Value};
use std::collections::{BTreeMap, VecDeque};
use std::fmt::Write;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};
use std::time::Duration;
use tokio::time::{sleep, Instant};

pub const ROLE_COFFEE: &str = "coffee";
const SOURCE_HEATD: &str = "heatd";
const DAY_FILE: &str = "devices/coffee_days.json";
const VIEW_RAW_CHARS: usize = 120;
const GLOBAL_ATTRIBUTES: u32 = 0xFFF0;
const POWER_ATTRIBUTE: u32 = 8;
const LABEL_HALF_WINDOW_US: i64 = 180_000_000;
const BASELINE_KEEP_US: i64 = 6 * 3600 * 1_000_000;
const BASELINE_MAX_SAMPLES: usize = 720;
const BASELINE_MEDIAN_FROM: usize = 30;
const EDGE_MIN_SAMPLES: usize = 5;
const EDGE_COOLDOWN_US: i64 = 600_000_000;
const DROP_MIN_BASELINE_MW: i64 = 300;
const RISE_MIN_IDLE_US: i64 = 20 * 60 * 1_000_000;
const EDGE_DAY_US: i64 = 24 * 3600 * 1_000_000;
const EDGES_PER_DAY: usize = 6;
const MAX_RESUBSCRIBE_BACKOFF: Duration = Duration::from_secs(24 * 3600);

fn default_window() -> String {
    "06:00-09:00".into()
}

fn default_endpoint() -> u16 {
    1
}

#[derive(Debug, Clone, Copy, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "lowercase")]
pub enum DeviceKind {
    Plug,
    Climate,
    Air,
}

const CALM_PLUG_MIN_S: u32 = 30;
const PLUG_PATHS: [(u32, u32); 5] = [
    (ON_OFF_CLUSTER, 0x0000),
    (ELECTRICAL_POWER_MEASUREMENT_CLUSTER, POWER_ATTRIBUTE),
    (ELECTRICAL_POWER_MEASUREMENT_CLUSTER, 0x0005),
    (ELECTRICAL_POWER_MEASUREMENT_CLUSTER, 0x0004),
    (ELECTRICAL_ENERGY_MEASUREMENT_CLUSTER, 0x0001),
];
const QUIET_BEFORE_WAKE_MS: i64 = 4 * 3600 * 1000;
const NIGHT_START_HOUR: u32 = 18;
const BEDTIME_SEARCHES: usize = 12;
const HUMIDITY_JUMP: f64 = 300.0;
const HUMIDITY_SPAN_MS: i64 = 20 * 60 * 1000;
const CO2_FALL_PPM: f64 = 60.0;
const CO2_FALL_SPAN_MS: i64 = 15 * 60 * 1000;
const CO2_NIGHT_RISE_PPM: f64 = 200.0;
const CO2_KEEP_MS: i64 = 8 * 3600 * 1000;
const WATCH_MOTION_FRESH_MS: i64 = 10 * 60 * 1000;
const COFFEE_ACTIVE_MW: i64 = 20_000;
const CO2_HOME_RISE_PPM: f64 = 50.0;
const CO2_HOME_SPAN_MS: i64 = 15 * 60 * 1000;
const READ_ALL_EVERY: Duration = Duration::from_secs(6 * 3600);

impl DeviceKind {
    fn fast_paths(self) -> Vec<(u32, u32)> {
        match self {
            Self::Plug => PLUG_PATHS.to_vec(),
            Self::Air => [AIR_QUALITY_CLUSTER, TEMPERATURE_MEASUREMENT_CLUSTER, HUMIDITY_MEASUREMENT_CLUSTER, CO2_CLUSTER, PM25_CLUSTER].map(|c| (c, ANY_ATTRIBUTE)).to_vec(),
            Self::Climate => Vec::new(),
        }
    }

    fn view_clusters(self) -> Vec<u32> {
        match self {
            Self::Climate => vec![TEMPERATURE_MEASUREMENT_CLUSTER, HUMIDITY_MEASUREMENT_CLUSTER, POWER_SOURCE_CLUSTER],
            other => other.fast_paths().into_iter().map(|p| p.0).collect(),
        }
    }

    fn primary(self) -> (u32, u32) {
        match self {
            Self::Plug => (1, 300),
            Self::Air => (10, 300),
            Self::Climate => (60, 1800),
        }
    }

    fn silent_after_min(self) -> f64 {
        match self {
            Self::Plug => 380.0,
            Self::Air => 60.0,
            Self::Climate => 480.0,
        }
    }

    fn retry_start_s(self) -> f64 {
        match self {
            Self::Climate => 300.0,
            _ => 60.0,
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
enum Edge {
    Rise,
    Drop,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Level {
    Low,
    Idle,
    High,
}

#[derive(Debug)]
pub struct PresenceRule {
    role: &'static str,
    source: &'static str,
    name: &'static str,
    rise: (&'static str, &'static str),
    drop: (&'static str, &'static str),
    rise_margin_mw: i64,
    rise_pct: i64,
    drop_pct: i64,
    exit_pct: i64,
    confirm: u32,
}

impl PresenceRule {
    fn answer(&self, edge: Edge) -> (&'static str, &'static str) {
        match edge {
            Edge::Rise => self.rise,
            Edge::Drop => self.drop,
        }
    }
}

pub const PRESENCE_RULES: [PresenceRule; 1] = [PresenceRule {
    role: "toothbrush",
    source: "toothbrush",
    name: "Zahnbuerste",
    rise: ("charge_start", "zurueckgestellt, Ladestart"),
    drop: ("removed", "vom Ladestaender genommen"),
    rise_margin_mw: 150,
    rise_pct: 0,
    drop_pct: 50,
    exit_pct: 50,
    confirm: 1,
}];

fn presence_rule(role: &str) -> Option<&'static PresenceRule> {
    PRESENCE_RULES.iter().find(|r| r.role == role)
}

#[derive(Debug)]
struct EdgeDetector {
    rule: &'static PresenceRule,
    idle: VecDeque<(i64, i64)>,
    state: Level,
    run: Option<(Level, u32, i64)>,
    last: BTreeMap<Edge, i64>,
    broken: bool,
    idle_since: Option<i64>,
    emitted: VecDeque<i64>,
}

impl Default for EdgeDetector {
    fn default() -> Self {
        Self::new(&PRESENCE_RULES[0])
    }
}

impl EdgeDetector {
    fn new(rule: &'static PresenceRule) -> Self {
        Self { rule, idle: VecDeque::new(), state: Level::Idle, run: None, last: BTreeMap::new(), broken: false, idle_since: None, emitted: VecDeque::new() }
    }

    fn break_link(&mut self) {
        self.broken = true;
    }
}

impl EdgeDetector {
    fn baseline(&self) -> i64 {
        let mut values: Vec<i64> = self.idle.iter().map(|s| s.1).collect();
        values.sort_unstable();
        if values.len() < BASELINE_MEDIAN_FROM {
            values[0]
        } else {
            values[values.len() / 2]
        }
    }

    fn remember(&mut self, ts: i64, milliwatts: i64) {
        self.idle.push_back((ts, milliwatts));
        while self.idle.len() > BASELINE_MAX_SAMPLES || self.idle.front().is_some_and(|f| ts - f.0 > BASELINE_KEEP_US) {
            self.idle.pop_front();
        }
    }

    fn classify(&self, milliwatts: i64) -> Level {
        let base = self.baseline();
        let margin = self.rule.rise_margin_mw.max(base * self.rule.rise_pct / 100);
        let high_from = if self.state == Level::High { base + margin * self.rule.exit_pct / 100 } else { base + margin };
        if milliwatts > high_from {
            Level::High
        } else if self.idle.len() >= BASELINE_MEDIAN_FROM && base >= DROP_MIN_BASELINE_MW && milliwatts * 100 < base * self.rule.drop_pct {
            Level::Low
        } else {
            Level::Idle
        }
    }

    fn allowed(&mut self, edge: Edge, at: i64) -> bool {
        while self.emitted.front().is_some_and(|t| at - t > EDGE_DAY_US) {
            self.emitted.pop_front();
        }
        let quiet = self.last.get(&edge).is_none_or(|t| at - t >= EDGE_COOLDOWN_US);
        let rested = edge != Edge::Rise || self.idle_since.is_some_and(|s| at - s >= RISE_MIN_IDLE_US);
        quiet && rested && self.emitted.len() < EDGES_PER_DAY
    }

    fn push(&mut self, ts: i64, milliwatts: i64) -> Option<(Edge, i64)> {
        let gap = std::mem::take(&mut self.broken);
        if self.idle.len() < EDGE_MIN_SAMPLES {
            self.remember(ts, milliwatts);
            self.idle_since.get_or_insert(ts);
            return None;
        }
        let class = self.classify(milliwatts);
        if class != Level::High {
            self.remember(ts, milliwatts);
        }
        if gap {
            self.run = None;
            self.state = class;
            self.idle_since = (class != Level::High).then_some(ts);
            return None;
        }
        if class == self.state {
            self.run = None;
            return None;
        }
        let (count, first) = match self.run {
            Some((level, count, first)) if level == class => (count + 1, first),
            _ => (1, ts),
        };
        if count < self.rule.confirm {
            self.run = Some((class, count, first));
            return None;
        }
        self.run = None;
        let left_high = self.state == Level::High;
        self.state = class;
        let edge = match class {
            Level::High => Edge::Rise,
            Level::Low => Edge::Drop,
            Level::Idle => {
                if left_high {
                    self.idle_since = Some(first);
                }
                return None;
            }
        };
        let allowed = self.allowed(edge, first);
        if class == Level::High {
            self.idle_since = None;
        } else if left_high {
            self.idle_since = Some(first);
        }
        if !allowed {
            return None;
        }
        self.last.insert(edge, first);
        self.emitted.push_back(first);
        Some((edge, first))
    }
}

#[derive(Debug, Clone, Deserialize)]
pub struct DeviceConfig {
    pub node: u64,
    pub kind: DeviceKind,
    pub role: String,
    #[serde(default)]
    pub room: Option<String>,
    #[serde(default)]
    pub name: Option<String>,
    #[serde(default)]
    pub min_s: Option<u32>,
    #[serde(default)]
    pub max_s: Option<u32>,
    #[serde(default)]
    pub rest_min_s: Option<u32>,
    #[serde(default)]
    pub rest_max_s: Option<u32>,
    #[serde(default)]
    pub silent_after_min: Option<f64>,
    #[serde(default)]
    pub wake_on: bool,
    #[serde(default = "default_window")]
    pub wake_window: String,
    #[serde(default = "default_endpoint")]
    pub onoff_endpoint: u16,
}

impl DeviceConfig {
    pub fn label(&self) -> String {
        self.name.clone().unwrap_or_else(|| format!("{:?} {}", self.kind, self.node).to_lowercase())
    }

    pub fn subscription(&self) -> DeviceSubscription {
        let (default_min, default_max) = self.kind.primary();
        let default_min = if self.kind == DeviceKind::Plug && self.role != ROLE_COFFEE { default_min.max(CALM_PLUG_MIN_S) } else { default_min };
        let (min_s, max_s) = (self.min_s.unwrap_or(default_min), self.max_s.unwrap_or(default_max));
        let (rest_min_s, rest_max_s) = (self.rest_min_s.unwrap_or(300), self.rest_max_s.unwrap_or(900));
        match self.kind.fast_paths() {
            paths if paths.is_empty() => DeviceSubscription { min_s, max_s, fast: None, rest: true, events: false },
            paths => DeviceSubscription { min_s: rest_min_s, max_s: rest_max_s, fast: Some(FastSubscription { paths, min_s, max_s }), rest: self.kind != DeviceKind::Plug, events: false },
        }
    }

    pub fn silent_after(&self) -> Duration {
        Duration::from_secs_f64(self.silent_after_min.unwrap_or(self.kind.silent_after_min()) * 60.0)
    }

    fn retry_start_s(&self) -> f64 {
        self.kind.retry_start_s().min(self.silent_after().as_secs_f64())
    }

    pub fn window(&self) -> Result<(u32, u32)> {
        let (from, to) = self.wake_window.split_once('-').ok_or_else(|| anyhow!("bad wake_window {}", self.wake_window))?;
        Ok((parse_minutes(from)?, parse_minutes(to)?))
    }

    fn meta(&self) -> Value {
        json!({"node": self.node, "kind": format!("{:?}", self.kind).to_lowercase(), "role": self.role, "room": self.room, "name": self.label(), "wake_on": CoffeePlug::from_config(self).is_some(), "wake_window": self.wake_window})
    }
}

pub fn validate(devices: &[DeviceConfig], rooms: &[Room]) -> Result<()> {
    let mut seen = std::collections::BTreeSet::new();
    for device in devices {
        if !seen.insert(device.node) {
            bail!("devices: node {} listed twice", device.node);
        }
        if rooms.iter().any(|r| r.nodes.contains(&device.node)) {
            bail!("devices: node {} is a thermostat of a room", device.node);
        }
        if let Some(unknown) = device.room.as_ref().filter(|name| rooms.iter().all(|r| &r.name != *name)) {
            bail!("devices: node {} has unknown room {unknown}", device.node);
        }
        if device.role.trim().is_empty() {
            bail!("devices: node {} needs a role", device.node);
        }
        if let Some(rule) = presence_rule(&device.role).filter(|_| device.kind != DeviceKind::Plug || device.room.is_none()) {
            bail!("devices: node {} with role {} needs kind plug and a room", device.node, rule.role);
        }
        if [device.min_s, device.max_s, device.rest_min_s, device.rest_max_s].into_iter().flatten().any(|s| s == 0) || device.silent_after_min.is_some_and(|m| m <= 0.0) {
            bail!("devices: node {} has a zero interval", device.node);
        }
        if device.min_s.zip(device.max_s).is_some_and(|(min, max)| min > max) || device.rest_min_s.zip(device.rest_max_s).is_some_and(|(min, max)| min > max) {
            bail!("devices: node {} has min_s above max_s", device.node);
        }
        let (from, to) = device.window()?;
        if device.wake_on {
            if device.kind != DeviceKind::Plug || device.role != ROLE_COFFEE {
                bail!("devices: wake_on is only allowed for a plug with role {ROLE_COFFEE} (node {})", device.node);
            }
            if from >= to {
                bail!("devices: node {} has an empty wake_window", device.node);
            }
        }
    }
    Ok(())
}

pub fn meta_value(devices: &[DeviceConfig]) -> Value {
    Value::Array(devices.iter().map(DeviceConfig::meta).collect())
}

#[derive(Debug, Clone, PartialEq)]
pub struct CoffeePlug {
    node: u64,
    endpoint: u16,
    window: (u32, u32),
}

impl CoffeePlug {
    pub fn from_config(device: &DeviceConfig) -> Option<Self> {
        let armed = device.wake_on && device.kind == DeviceKind::Plug && device.role == ROLE_COFFEE;
        armed.then(|| device.window().ok()).flatten().map(|window| Self { node: device.node, endpoint: device.onoff_endpoint, window })
    }

    async fn turn_on(&self, switch: &dyn SwitchOn) -> matter_controller::Result<()> {
        switch.device_switch_on(self.node, self.endpoint).await
    }

    fn in_window(&self, local: NaiveDateTime) -> bool {
        (self.window.0..self.window.1).contains(&(local.hour() * 60 + local.minute()))
    }

    fn window_label(&self) -> String {
        format!("{:02}:{:02}-{:02}:{:02}", self.window.0 / 60, self.window.0 % 60, self.window.1 / 60, self.window.1 % 60)
    }
}

#[derive(Debug, Clone, Copy)]
pub struct WakeTiming {
    pub retry: Duration,
    pub deadline: Duration,
}

impl Default for WakeTiming {
    fn default() -> Self {
        Self { retry: Duration::from_secs(120), deadline: Duration::from_secs(600) }
    }
}

#[derive(Debug, Clone, Copy)]
pub struct LoopTiming {
    pub reconnect_start: Duration,
    pub reconnect_max: Duration,
    pub health_tick: Duration,
}

impl Default for LoopTiming {
    fn default() -> Self {
        Self { reconnect_start: Duration::from_secs(1), reconnect_max: Duration::from_secs(60), health_tick: Duration::from_secs(60) }
    }
}

#[derive(Debug, Clone, PartialEq)]
pub enum WakeStep {
    Attempt { plug: CoffeePlug, day: NaiveDate, local: NaiveDateTime, trigger: String },
    Away { plug: CoffeePlug, day: NaiveDate, local: NaiveDateTime },
}

#[derive(Debug, Clone)]
struct Latest {
    name: String,
    value: Option<f64>,
    raw: String,
    ts_us: i64,
}

#[derive(Debug, Default)]
struct NodeState {
    started_us: i64,
    first_us: Option<i64>,
    last_us: Option<i64>,
    reports: u64,
    longest_gap_s: f64,
    lost: bool,
    lost_count: u32,
    silent_since_us: Option<i64>,
    next_resubscribe_us: i64,
    resubscribe_backoff_s: f64,
    resubscribes: u32,
    reported_since_issue: bool,
    read_all_us: Option<i64>,
    retry_start_s: f64,
    retry_s: f64,
    issued_us: i64,
    latest: BTreeMap<(u16, u32, u32), Latest>,
}

#[derive(Default)]
struct Inner {
    configs: Vec<DeviceConfig>,
    nodes: BTreeMap<u64, NodeState>,
    started_ms: i64,
    sleep_room: Option<String>,
    last_activity_ms: Option<i64>,
    fallback: Option<(&'static str, i64)>,
    on: BTreeMap<u64, bool>,
    humidity: BTreeMap<u64, (i64, f64)>,
    co2: BTreeMap<u64, VecDeque<(i64, f64)>>,
    done: BTreeMap<u64, NaiveDate>,
    away_logged: BTreeMap<u64, NaiveDate>,
    day_file: Option<PathBuf>,
    detectors: BTreeMap<u64, EdgeDetector>,
    home: BTreeMap<&'static str, i64>,
    coffee_power: BTreeMap<u64, i64>,
}

impl Inner {
    fn edge_label(&mut self, node: u64, row: &DeviceReading) -> Option<Label> {
        let config = self.configs.iter().find(|c| c.node == node)?;
        let rule = presence_rule(&config.role)?;
        let room = config.room.clone()?;
        if row.cluster != ELECTRICAL_POWER_MEASUREMENT_CLUSTER || row.attribute != POWER_ATTRIBUTE || row.kind != "attribute" {
            return None;
        }
        let (edge, ts) = self.detectors.entry(node).or_insert_with(|| EdgeDetector::new(rule)).push(row.ts, row.value?.round() as i64)?;
        let (answer, _) = rule.answer(edge);
        self.note_home(rule.source, ts / 1000);
        Some(Label {
            ts,
            from_ts: Some(ts - LABEL_HALF_WINDOW_US),
            to_ts: Some(ts + LABEL_HALF_WINDOW_US),
            room,
            source: rule.source.into(),
            confidence: Some(1.0),
            label_id: Some(format!("{}-{node}-{ts}", rule.source)),
            answer: Some(answer.into()),
            via: Some("plug".into()),
            ..Default::default()
        })
    }

    fn persist(&self) {
        let Some(path) = &self.day_file else { return };
        let days: BTreeMap<String, String> = self.done.iter().map(|(n, d)| (n.to_string(), d.to_string())).collect();
        let write = || -> std::io::Result<()> {
            if let Some(parent) = path.parent() {
                std::fs::create_dir_all(parent)?;
            }
            std::fs::write(path, serde_json::to_string(&days)?)
        };
        if let Err(e) = write() {
            eprintln!("coffee day marker: {e}");
        }
    }

    fn candidates(&self, local: NaiveDateTime) -> Vec<CoffeePlug> {
        let day = local.date();
        self.configs.iter().filter_map(CoffeePlug::from_config).filter(|p| p.in_window(local) && self.done.get(&p.node) != Some(&day)).collect()
    }

    fn note_activity(&mut self, source: &'static str, now_ms: i64) {
        let quiet = now_ms - self.last_activity_ms.unwrap_or(self.started_ms) >= QUIET_BEFORE_WAKE_MS;
        if quiet && self.fallback.is_none() {
            self.fallback = Some((source, now_ms));
        }
        self.last_activity_ms = Some(now_ms);
    }

    fn note_home(&mut self, source: &'static str, now_ms: i64) {
        self.home.insert(source, now_ms);
    }

    fn is_coffee(&self, node: u64) -> bool {
        self.configs.iter().any(|c| c.node == node && c.role == ROLE_COFFEE)
    }

    fn note_power(&mut self, node: u64, row: &DeviceReading, now_ms: i64) {
        if row.cluster != ELECTRICAL_POWER_MEASUREMENT_CLUSTER || row.attribute != POWER_ATTRIBUTE || row.kind != "attribute" || !self.is_coffee(node) {
            return;
        }
        let Some(milliwatts) = row.value.map(|v| v.round() as i64) else { return };
        if self.coffee_power.insert(node, milliwatts).is_some_and(|before| (before >= COFFEE_ACTIVE_MW) != (milliwatts >= COFFEE_ACTIVE_MW)) {
            self.note_home("coffee_power", now_ms);
        }
    }

    fn note_onoff(&mut self, node: u64, on: bool, local: NaiveDateTime, now_ms: i64) {
        let before = self.on.insert(node, on);
        if before.is_some_and(|b| b != on) && self.is_coffee(node) {
            self.note_home("coffee_plug", now_ms);
        }
        let Some(plug) = self.configs.iter().filter_map(CoffeePlug::from_config).find(|p| p.node == node) else { return };
        if on && before == Some(false) && plug.in_window(local) && self.done.insert(node, local.date()) != Some(local.date()) {
            self.persist();
        }
    }

    fn note_ambient(&mut self, node: u64, row: &DeviceReading, now_ms: i64) {
        let (Some(config), Some(value)) = (self.configs.iter().find(|c| c.node == node), row.value) else { return };
        if row.kind != "attribute" || row.attribute != 0 {
            return;
        }
        let (role, room) = (config.role.clone(), config.room.clone());
        if row.cluster == HUMIDITY_MEASUREMENT_CLUSTER && role == "sink" {
            let jumped = self.humidity.insert(node, (now_ms, value)).is_some_and(|(t, v)| now_ms - t <= HUMIDITY_SPAN_MS && value - v >= HUMIDITY_JUMP);
            if jumped {
                self.note_home("sink_humidity", now_ms);
                self.note_activity("sink_humidity", now_ms);
            }
        } else if row.cluster == CO2_CLUSTER && room.is_some() && room == self.sleep_room {
            let samples = self.co2.entry(node).or_default();
            samples.push_back((now_ms, value));
            while samples.front().is_some_and(|s| now_ms - s.0 > CO2_KEEP_MS) {
                samples.pop_front();
            }
            let recent = samples.iter().find(|s| now_ms - s.0 <= CO2_FALL_SPAN_MS).map_or(value, |s| s.1);
            let (high, low) = samples.iter().fold((f64::MIN, f64::MAX), |(h, l), s| (h.max(s.1), l.min(s.1)));
            let floor = samples.iter().filter(|s| now_ms - s.0 <= CO2_HOME_SPAN_MS).fold(value, |l, s| l.min(s.1));
            let risen = value - floor > CO2_HOME_RISE_PPM;
            if risen {
                self.note_home("bedroom_co2", now_ms);
            }
            if recent - value >= CO2_FALL_PPM && high - low >= CO2_NIGHT_RISE_PPM {
                self.note_activity("bedroom_co2", now_ms);
            }
        }
    }

    fn wake_step(&mut self, local: NaiveDateTime, now_ms: i64, presence_known: bool, away: bool, bedtime: &BTreeMap<u64, i64>) -> Vec<WakeStep> {
        let fallback = self.fallback.take();
        if !presence_known {
            return Vec::new();
        }
        let day = local.date();
        let mut steps = Vec::new();
        for plug in self.candidates(local) {
            let from_bed = bedtime.get(&plug.node).filter(|ts| plug.in_window(local_of(local, now_ms, **ts))).map(|_| "bedtime".to_string());
            let from_edge = fallback.filter(|(_, ts)| plug.in_window(local_of(local, now_ms, *ts))).map(|(f, _)| format!("fallback:{f}"));
            let Some(trigger) = from_bed.or(from_edge) else { continue };
            if away {
                if self.away_logged.insert(plug.node, day) != Some(day) {
                    steps.push(WakeStep::Away { plug, day, local });
                }
                continue;
            }
            self.done.insert(plug.node, day);
            steps.push(WakeStep::Attempt { plug, day, local, trigger });
        }
        if steps.iter().any(|s| matches!(s, WakeStep::Attempt { .. })) {
            self.persist();
        }
        steps
    }

    fn report(&mut self, node: u64, rows: &[DeviceReading], now_us: i64) -> Option<(i64, i64)> {
        let state = self.nodes.get_mut(&node)?;
        let recovered = state.silent_since_us.take().map(|since| (since, now_us));
        if let Some(last) = state.last_us {
            state.longest_gap_s = state.longest_gap_s.max((now_us - last) as f64 / 1e6);
        }
        state.first_us.get_or_insert(now_us);
        state.last_us = Some(now_us);
        state.reports += rows.len() as u64;
        state.resubscribe_backoff_s = 0.0;
        state.reported_since_issue = true;
        state.retry_s = state.retry_start_s;
        for row in rows {
            state.latest.insert((row.endpoint, row.cluster, row.attribute), Latest { name: row.name.clone(), value: row.value, raw: row.raw.chars().take(VIEW_RAW_CHARS).collect(), ts_us: row.ts });
        }
        recovered
    }
}

#[derive(Clone, Default)]
pub struct Devices {
    inner: Arc<Mutex<Inner>>,
    feed: Option<Arc<dyn DeviceFeed>>,
    switch: Option<Arc<dyn SwitchOn>>,
    wake_timing: Option<WakeTiming>,
}

fn number(value: &TypedValue) -> Option<f64> {
    value.number().or_else(|| (value.dtype == "uint").then(|| value.v_str.as_deref().and_then(|s| s.parse().ok())).flatten())
}

pub fn reading_of(item: &CaptureItem, ts: i64) -> Option<DeviceReading> {
    let text = |raw: &[u8]| String::from_utf8_lossy(raw).into_owned();
    match item {
        CaptureItem::Attribute(a) => Some(DeviceReading {
            ts,
            node: a.node,
            endpoint: a.endpoint,
            cluster: a.cluster,
            attribute: a.attribute,
            kind: "attribute".into(),
            name: a.name.clone(),
            value: number(&a.value),
            raw: text(&a.raw),
            source: a.source.into(),
        }),
        CaptureItem::Event(e) => Some(DeviceReading {
            ts,
            node: e.node,
            endpoint: e.endpoint,
            cluster: e.cluster,
            attribute: e.event_id,
            kind: "event".into(),
            name: e.name.clone(),
            value: number(&e.value),
            raw: text(&e.raw),
            source: e.source.into(),
        }),
        _ => None,
    }
}

fn local_of(local: NaiveDateTime, now_ms: i64, ts_ms: i64) -> NaiveDateTime {
    local - Span::milliseconds(now_ms - ts_ms)
}

fn night_start_ms(local: NaiveDateTime, now_ms: i64) -> i64 {
    let today = local.date().and_hms_opt(NIGHT_START_HOUR, 0, 0).unwrap_or(local);
    let start = if today > local { today - Span::days(1) } else { today };
    now_ms - (local - start).num_milliseconds()
}

fn log_row(what: &str, result: Result<()>) {
    if let Err(e) = result {
        eprintln!("{what}: {e:#}");
    }
}

impl Devices {
    pub fn new(config: &Config, feed: Arc<dyn DeviceFeed>, switch: Arc<dyn SwitchOn>) -> Self {
        let now_us = lake::now_us();
        let day_file = Path::new(&config.data_dir).join(DAY_FILE);
        let done = std::fs::read_to_string(&day_file)
            .ok()
            .and_then(|text| serde_json::from_str::<BTreeMap<String, String>>(&text).ok())
            .map(|days| days.into_iter().filter_map(|(n, d)| Some((n.parse().ok()?, d.parse().ok()?))).collect())
            .unwrap_or_default();
        let nodes = config.devices.iter().map(|d| (d.node, NodeState { started_us: now_us, issued_us: now_us, retry_start_s: d.retry_start_s(), retry_s: d.retry_start_s(), ..Default::default() })).collect();
        let inner = Inner { configs: config.devices.clone(), nodes, done, day_file: Some(day_file), started_ms: now_us / 1000, sleep_room: config.sleep_room.clone(), ..Default::default() };
        Self { inner: Arc::new(Mutex::new(inner)), feed: Some(feed), switch: Some(switch), wake_timing: None }
    }

    #[cfg(test)]
    pub fn with_wake_timing(mut self, timing: WakeTiming) -> Self {
        self.wake_timing = Some(timing);
        self
    }

    pub fn configs(&self) -> Vec<DeviceConfig> {
        self.inner.lock().unwrap().configs.clone()
    }

    pub fn handle(&self, sink: &Sink, node: u64, item: CaptureItem) {
        let now_us = lake::now_us();
        match &item {
            CaptureItem::Attribute(_) | CaptureItem::Event(_) => {
                let Some(row) = reading_of(&item, now_us).filter(|r| r.node == node) else { return };
                let recovered = self.inner.lock().unwrap().report(node, std::slice::from_ref(&row), now_us);
                let label = {
                    let mut inner = self.inner.lock().unwrap();
                    let now_ms = now_us / 1000;
                    if row.cluster == ON_OFF_CLUSTER && row.attribute == 0 && row.kind == "attribute" {
                        inner.note_onoff(node, row.value == Some(1.0), chrono::Local::now().naive_local(), now_ms);
                    }
                    inner.note_ambient(node, &row, now_ms);
                    inner.note_power(node, &row, now_ms);
                    let label = inner.edge_label(node, &row);
                    if label.is_some() {
                        inner.note_activity("toothbrush", now_ms);
                    }
                    label
                };
                log_row("device reading", sink.append(row));
                if let Some(label) = label {
                    log_row("presence label", sink.append(label));
                }
                if let Some((since, now)) = recovered {
                    self.link_event(sink, node, "ok", json!({"silent_s": (now - since) / 1_000_000}));
                }
            }
            CaptureItem::SubscriptionLost { reason, .. } => {
                self.break_edge_link(node);
                let first = match self.inner.lock().unwrap().nodes.get_mut(&node) {
                    Some(state) if !state.lost => {
                        state.lost = true;
                        state.lost_count += 1;
                        true
                    }
                    _ => false,
                };
                if first {
                    self.link_event(sink, node, "subscription_lost", json!({"reason": reason}));
                }
            }
            CaptureItem::Subscribed { .. } => {
                self.break_edge_link(node);
                let was_lost = self.inner.lock().unwrap().nodes.get_mut(&node).is_some_and(|s| std::mem::replace(&mut s.lost, false));
                if was_lost {
                    self.link_event(sink, node, "resubscribed", json!({}));
                }
            }
        }
    }

    pub fn home_activity(&self, now_ms: i64, within_ms: i64) -> Option<&'static str> {
        self.inner.lock().unwrap().home.iter().filter(|(_, at)| now_ms - **at <= within_ms).max_by_key(|(_, at)| **at).map(|(source, _)| *source)
    }

    fn break_edge_link(&self, node: u64) {
        if let Some(detector) = self.inner.lock().unwrap().detectors.get_mut(&node) {
            detector.break_link();
        }
    }

    fn link_event(&self, sink: &Sink, node: u64, state: &str, extra: Value) {
        let (role, device_kind, resubscribes) = {
            let inner = self.inner.lock().unwrap();
            let config = inner.configs.iter().find(|c| c.node == node);
            (config.map(|c| c.role.clone()), config.map(|c| format!("{:?}", c.kind).to_lowercase()), inner.nodes.get(&node).map_or(0, |s| s.resubscribes))
        };
        let mut detail = json!({"role": role, "kind": device_kind, "state": state, "resubscribes": resubscribes});
        if let (Some(map), Some(more)) = (detail.as_object_mut(), extra.as_object()) {
            map.extend(more.clone());
        }
        log_row("device link event", sink.append(Event::now(kind::DEVICE_LINK, SOURCE_HEATD).with_node(node).with_detail(detail)));
    }

    fn silent_nodes(&self, now_us: i64) -> Vec<(u64, bool)> {
        let mut inner = self.inner.lock().unwrap();
        let configs = inner.configs.clone();
        let mut due = Vec::new();
        for config in &configs {
            let Some(state) = inner.nodes.get_mut(&config.node) else { continue };
            let limit_s = config.silent_after().as_secs_f64();
            let reference = state.last_us.unwrap_or(state.started_us);
            if ((now_us - reference) as f64 / 1e6) < limit_s {
                if !state.reported_since_issue && now_us >= state.issued_us + (state.retry_s * 1e6) as i64 {
                    state.resubscribes += 1;
                    state.issued_us = now_us;
                    state.retry_s = (state.retry_s * 2.0).min(limit_s);
                    due.push((config.node, false));
                }
                continue;
            }
            let first = state.silent_since_us.is_none();
            if first {
                state.silent_since_us = Some(reference);
                state.resubscribe_backoff_s = limit_s;
                state.next_resubscribe_us = now_us;
            }
            if now_us < state.next_resubscribe_us {
                continue;
            }
            state.resubscribes += 1;
            state.reported_since_issue = false;
            state.issued_us = now_us;
            state.next_resubscribe_us = now_us + (state.resubscribe_backoff_s * 1e6) as i64;
            state.resubscribe_backoff_s = (state.resubscribe_backoff_s * 2.0).min(MAX_RESUBSCRIBE_BACKOFF.as_secs_f64());
            due.push((config.node, first));
        }
        due
    }

    fn read_all_due(&self, now_us: i64) -> Vec<u64> {
        let mut inner = self.inner.lock().unwrap();
        let plain: Vec<u64> = inner.configs.iter().filter(|c| !c.subscription().rest && c.subscription().fast.is_some()).map(|c| c.node).collect();
        let every = READ_ALL_EVERY.as_micros() as i64;
        plain
            .into_iter()
            .filter(|node| {
                let Some(state) = inner.nodes.get_mut(node) else { return false };
                let healthy = !state.lost && state.silent_since_us.is_none() && state.reported_since_issue;
                healthy && now_us - state.read_all_us.unwrap_or(state.started_us) >= every
            })
            .collect()
    }

    pub async fn health_tick(&self, sink: &Sink, now_us: i64) {
        if let Some(feed) = &self.feed {
            for node in self.read_all_due(now_us) {
                match feed.device_read_all(node).await {
                    Ok(()) => {
                        if let Some(state) = self.inner.lock().unwrap().nodes.get_mut(&node) {
                            state.read_all_us = Some(now_us);
                        }
                    }
                    Err(e) => eprintln!("device {node} read: {e}"),
                }
            }
        }
        for (node, first) in self.silent_nodes(now_us) {
            if let Some(feed) = &self.feed {
                if let Err(e) = feed.device_resubscribe(node).await {
                    eprintln!("device {node} resubscribe: {e}");
                }
            }
            if first {
                let since = self.inner.lock().unwrap().nodes.get(&node).and_then(|s| s.silent_since_us).unwrap_or(now_us);
                self.link_event(sink, node, "silent", json!({"silent_s": (now_us - since) / 1_000_000}));
            }
        }
    }

    fn bedtime_wakes(&self, state: &AppState, plugs: &[CoffeePlug], local: NaiveDateTime, now_ms: i64) -> BTreeMap<u64, i64> {
        let Ok(statuses) = state.storage.device_statuses() else { return BTreeMap::new() };
        let mut found = BTreeMap::new();
        for plug in plugs {
            let mut since = night_start_ms(local, now_ms);
            for _ in 0..BEDTIME_SEARCHES {
                let wakes: Vec<i64> = statuses.iter().filter_map(|s| state.storage.bedtime_wake(&s.device, since).ok().flatten()).collect();
                let Some(ts) = wakes.iter().copied().min() else { break };
                let at = local_of(local, now_ms, ts);
                if plug.in_window(at) {
                    found.insert(plug.node, ts);
                    break;
                }
                if at.date() == local.date() && at.hour() * 60 + at.minute() >= plug.window.0 {
                    break;
                }
                since = ts + 1;
            }
        }
        found
    }

    pub fn on_control(&self, state: &AppState, now: NaiveDateTime, now_ms: i64, presence_known: bool, away: bool) {
        if let Ok(statuses) = state.storage.device_statuses() {
            if statuses.iter().any(|s| s.motion == Some(Motion::Moving) && s.bedtime != Some(true) && now_ms - s.state_ts <= WATCH_MOTION_FRESH_MS) {
                self.inner.lock().unwrap().note_activity("watch_motion", now_ms);
            }
        }
        let plugs = self.inner.lock().unwrap().candidates(now);
        let bedtime = if plugs.is_empty() { BTreeMap::new() } else { self.bedtime_wakes(state, &plugs, now, now_ms) };
        let steps = self.inner.lock().unwrap().wake_step(now, now_ms, presence_known, away, &bedtime);
        for step in steps {
            match step {
                WakeStep::Away { plug, day, local } => coffee_event(&state.sink, &plug, day, local, json!({"reason": "away", "ok": false})),
                WakeStep::Attempt { plug, local, trigger, .. } => {
                    if let Some(switch) = self.switch.clone() {
                        let failsafe = state.failsafe.clone();
                        let stopping: Arc<dyn Fn() -> bool + Send + Sync> = Arc::new(move || failsafe.stopping());
                        tokio::spawn(wake_attempts(switch, state.sink.clone(), plug, local, trigger, self.wake_timing.unwrap_or_default(), stopping));
                    }
                }
            }
        }
    }

    pub fn view(&self, now_us: i64) -> Value {
        let inner = self.inner.lock().unwrap();
        let age = |us: i64| ((now_us - us) as f64 / 1e6).max(0.0);
        Value::Array(
            inner
                .configs
                .iter()
                .filter_map(|config| {
                    let state = inner.nodes.get(&config.node)?;
                    let latest: Vec<Value> = state
                        .latest
                        .iter()
                        .filter(|((_, cluster, attribute), _)| config.kind.view_clusters().contains(cluster) && *attribute < GLOBAL_ATTRIBUTES)
                        .map(|((endpoint, cluster, attribute), l)| {
                            json!({"endpoint": endpoint, "cluster": cluster, "attribute": attribute, "name": l.name, "value": l.value, "raw": l.value.is_none().then_some(&l.raw), "age_s": age(l.ts_us)})
                        })
                        .collect();
                    Some(json!({
                        "node": config.node,
                        "name": config.label(),
                        "kind": format!("{:?}", config.kind).to_lowercase(),
                        "role": config.role,
                        "room": config.room,
                        "reports": state.reports,
                        "last_report_age_s": state.last_us.map(age),
                        "longest_gap_s": state.longest_gap_s,
                        "subscription": if state.lost { "lost" } else { "ok" },
                        "silent": state.silent_since_us.is_some(),
                        "resubscribes": state.resubscribes,
                        "wake_on": CoffeePlug::from_config(config).is_some(),
                        "coffee_day": inner.done.get(&config.node).map(|d| d.to_string()),
                        "latest": latest,
                    }))
                })
                .collect(),
        )
    }
}

fn coffee_event(sink: &Sink, plug: &CoffeePlug, day: NaiveDate, local: NaiveDateTime, extra: Value) {
    let mut detail = json!({"day": day.to_string(), "local_time": local.format("%H:%M").to_string(), "window": plug.window_label()});
    if let (Some(map), Some(more)) = (detail.as_object_mut(), extra.as_object()) {
        map.extend(more.clone());
    }
    log_row("coffee event", sink.append(Event::now(kind::COFFEE_ON, SOURCE_HEATD).with_node(plug.node).with_detail(detail)));
}

pub async fn wake_attempts(switch: Arc<dyn SwitchOn>, sink: Sink, plug: CoffeePlug, local: NaiveDateTime, trigger: String, timing: WakeTiming, stopping: Arc<dyn Fn() -> bool + Send + Sync>) {
    let started = Instant::now();
    let mut attempt = 0u32;
    let outcome = loop {
        if stopping() {
            break Err("heatd stopping".to_string());
        }
        attempt += 1;
        match plug.turn_on(switch.as_ref()).await {
            Ok(()) => break Ok(()),
            Err(e) if started.elapsed() + timing.retry > timing.deadline => break Err(e.to_string()),
            Err(_) => sleep(timing.retry).await,
        }
    };
    let extra = match outcome {
        Ok(()) => json!({"reason": "wake", "trigger": trigger, "attempt": attempt, "ok": true}),
        Err(error) => json!({"reason": "gave_up", "trigger": trigger, "attempt": attempt, "ok": false, "error": error}),
    };
    coffee_event(&sink, &plug, local.date(), local, extra);
}

async fn capture_loop(state: AppState, feed: Arc<dyn DeviceFeed>, config: DeviceConfig, timing: LoopTiming) {
    let plan = config.subscription();
    let mut backoff = timing.reconnect_start;
    let mut last_error = String::new();
    loop {
        match feed.device_capture(config.node, plan.clone()).await {
            Ok(mut rx) => {
                backoff = timing.reconnect_start;
                last_error.clear();
                while let Some(item) = rx.recv().await {
                    state.devices.handle(&state.sink, config.node, item);
                }
            }
            Err(e) => {
                if last_error != e.to_string() {
                    last_error = e.to_string();
                    eprintln!("device {}: {e}", config.node);
                }
            }
        }
        sleep(backoff).await;
        backoff = (backoff * 2).min(timing.reconnect_max);
    }
}

pub fn spawn(state: AppState, timing: LoopTiming) {
    let Some(feed) = state.devices.feed.clone() else { return };
    let configs = state.devices.configs();
    if configs.is_empty() {
        return;
    }
    for config in configs {
        tokio::spawn(capture_loop(state.clone(), feed.clone(), config, timing));
    }
    tokio::spawn(async move {
        loop {
            sleep(timing.health_tick).await;
            state.devices.health_tick(&state.sink, lake::now_us()).await;
        }
    });
}

const DAY_US: i64 = 86_400_000_000;
const GAP_S: f64 = 3600.0;
const LEAD: [(u32, u32, &str, f64, &str); 8] = [
    (ON_OFF_CLUSTER, 0, "Ein/Aus", 1.0, ""),
    (ELECTRICAL_POWER_MEASUREMENT_CLUSTER, 8, "Wirkleistung", 1000.0, " W"),
    (TEMPERATURE_MEASUREMENT_CLUSTER, 0, "Temperatur", 100.0, " C"),
    (HUMIDITY_MEASUREMENT_CLUSTER, 0, "Feuchte", 100.0, " %"),
    (CO2_CLUSTER, 0, "CO2", 1.0, " ppm"),
    (PM25_CLUSTER, 0, "PM2.5", 1.0, " ug/m3"),
    (AIR_QUALITY_CLUSTER, 0, "Luftguete", 1.0, ""),
    (POWER_SOURCE_CLUSTER, 0x0C, "Batterie", 2.0, " %"),
];

fn lead_value(cluster: u32, attribute: u32, value: f64) -> Option<String> {
    let (_, _, label, scale, unit) = LEAD.iter().find(|l| l.0 == cluster && l.1 == attribute)?;
    Some(if cluster == ON_OFF_CLUSTER { format!("{label} {}", if value > 0.5 { "an" } else { "aus" }) } else { format!("{label} {:.1}{unit}", value / scale) })
}

pub fn report_section(root: &Path, date: NaiveDate) -> Result<String> {
    let db = open_lake(root, date - chrono::Duration::days(1), date + chrono::Duration::days(1))?;
    let c = &db.conn;
    let (lo, hi) = (midnight_us(date), midnight_us(date) + DAY_US);
    let day = format!("epoch_us(ts) >= {lo} AND epoch_us(ts) < {hi}");
    let meta: Vec<Value> = rows(c, "SELECT arg_max(value, ts) FROM meta WHERE key = 'devices'", |r| r.get::<_, Option<String>>(0))?
        .into_iter()
        .next()
        .flatten()
        .and_then(|text| serde_json::from_str::<Vec<Value>>(&text).ok())
        .unwrap_or_default();
    let stats = rows(
        c,
        &format!(
            "SELECT node, count(*), max(gap), count(*) FILTER (WHERE gap > {GAP_S}) FROM (SELECT node, (epoch_us(ts) - lag(epoch_us(ts)) OVER (PARTITION BY node ORDER BY ts)) / 1e6 AS gap FROM device_readings WHERE {day}) GROUP BY node ORDER BY node"
        ),
        |r| Ok((r.get::<_, u64>(0)?, r.get::<_, i64>(1)?, r.get::<_, Option<f64>>(2)?, r.get::<_, i64>(3)?)),
    )?;
    let leads = LEAD.iter().map(|l| format!("({}, {})", l.0, l.1)).collect::<Vec<_>>().join(", ");
    let last = rows(
        c,
        &format!("SELECT node, cluster, attribute, arg_max(value, ts) FROM device_readings WHERE kind = 'attribute' AND value IS NOT NULL AND (cluster, attribute) IN ({leads}) AND {day} GROUP BY node, endpoint, cluster, attribute ORDER BY node, cluster, endpoint"),
        |r| Ok((r.get::<_, u64>(0)?, r.get::<_, u32>(1)?, r.get::<_, u32>(2)?, r.get::<_, f64>(3)?)),
    )?;
    let mut md = String::from("\n## Zusatzgeraete\n\nNur Aufzeichnung (Tabelle device_readings), ohne Einfluss auf die Heizung. Luecken = Pausen ueber 1 h zwischen Berichten; ruhige Sensoren melden nur bei Aenderung.\n\n");
    let nodes: Vec<u64> = meta.iter().filter_map(|m| m["node"].as_u64()).chain(stats.iter().map(|s| s.0)).collect::<std::collections::BTreeSet<_>>().into_iter().collect();
    if nodes.is_empty() {
        md.push_str("Keine Zusatzgeraete konfiguriert.\n");
        return Ok(md);
    }
    md.push_str("| Node | Geraet | Berichte | letzter Wert | Luecken | laengste Luecke |\n|---|---|---|---|---|---|\n");
    for node in &nodes {
        let info = meta.iter().find(|m| m["node"] == *node);
        let text = |k: &str| info.and_then(|m| m[k].as_str()).unwrap_or("-").to_string();
        let name = format!("{} ({}, {}, {})", text("name"), text("kind"), text("role"), text("room"));
        let values: Vec<String> = last.iter().filter(|l| l.0 == *node).filter_map(|l| lead_value(l.1, l.2, l.3)).collect();
        let (count, gap, gaps) = stats.iter().find(|s| s.0 == *node).map_or((0, None, 0), |s| (s.1, s.2, s.3));
        let longest = if count == 0 { "ganzer Tag".to_string() } else { gap.map_or("-".into(), dur) };
        let _ = writeln!(md, "| {node} | {name} | {count} | {} | {gaps} | {longest} |", if values.is_empty() { "-".into() } else { values.join(", ") });
    }
    let coffee = rows(c, &format!("SELECT epoch_us(ts), node, detail FROM events WHERE kind = '{}' AND {day} ORDER BY ts", kind::COFFEE_ON), |r| Ok((r.get::<_, i64>(0)?, r.get::<_, Option<u64>>(1)?, r.get::<_, Option<String>>(2)?)))?;
    md.push('\n');
    for (us, node, detail) in &coffee {
        let d: Value = detail.as_deref().and_then(|t| serde_json::from_str(t).ok()).unwrap_or(Value::Null);
        let who = node.map_or(String::new(), |n| format!(" (Node {n})"));
        let line = match d["reason"].as_str() {
            Some("wake") => {
                let cause = match d["trigger"].as_str() {
                    Some("bedtime") | None => "Schlafmodus der Uhr endete".to_string(),
                    Some(other) => format!("Ausloeser {other}"),
                };
                format!("um {} UTC eingeschaltet, {cause} {} Ortszeit, Versuch {}", hhmm(*us), d["local_time"].as_str().unwrap_or("?"), d["attempt"])
            }
            Some("gave_up") => format!("Einschalten fehlgeschlagen nach {} Versuchen ({}), fuer heute aufgegeben", d["attempt"], d["error"].as_str().unwrap_or("?")),
            Some("away") => "nicht eingeschaltet, Haus war abwesend".to_string(),
            other => other.unwrap_or("?").to_string(),
        };
        let _ = writeln!(md, "- Kaffeemaschine{who}: {line}.");
    }
    if coffee.is_empty() {
        for m in meta.iter().filter(|m| m["wake_on"] == true) {
            let _ = writeln!(md, "- Kaffeemaschine (Node {}): nicht eingeschaltet, kein Ende des Schlafmodus im Fenster {} Ortszeit.", m["node"], m["wake_window"].as_str().unwrap_or("?"));
        }
    }
    let sources = PRESENCE_RULES.iter().map(|r| format!("'{}'", r.source)).collect::<Vec<_>>().join(", ");
    let presence = rows(
        c,
        &format!("SELECT epoch_us(ts), source, room, answer FROM labels WHERE source IN ({sources}) AND {day} ORDER BY ts"),
        |r| Ok((r.get::<_, i64>(0)?, r.get::<_, String>(1)?, r.get::<_, String>(2)?, r.get::<_, Option<String>>(3)?)),
    )?;
    for (us, source, room, answer) in &presence {
        let Some(rule) = PRESENCE_RULES.iter().find(|r| r.source == source) else { continue };
        let text = [rule.rise, rule.drop].into_iter().find(|a| Some(a.0) == answer.as_deref()).map_or("Kante", |a| a.1);
        let _ = writeln!(md, "- {} um {} UTC {text}: Aufenthalt im Raum {room} als Label (Quelle {source}, Scans +-3 min).", rule.name, hhmm(*us));
    }
    let links = rows(
        c,
        &format!("SELECT node, json_extract_string(detail, '$.state'), count(*) FROM events WHERE kind = '{}' AND {day} GROUP BY 1, 2 ORDER BY 1, 2", kind::DEVICE_LINK),
        |r| Ok((r.get::<_, Option<u64>>(0)?, r.get::<_, Option<String>>(1)?, r.get::<_, i64>(2)?)),
    )?;
    if !links.is_empty() {
        let _ = writeln!(md, "- Verbindung: {}.", links.iter().map(|(n, s, k)| format!("Node {} {} {k}", n.unwrap_or(0), s.as_deref().unwrap_or("?"))).collect::<Vec<_>>().join(", "));
    }
    Ok(md)
}

#[cfg(test)]
#[path = "devices_tests.rs"]
mod tests;
