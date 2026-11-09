use crate::boost::WakeEdge;
use crate::config::{parse_minutes, Config, Room};
use crate::http::AppState;
use crate::lake::{self, kind, DeviceReading, Event, Sink};
use crate::report::{dur, hhmm, midnight_us, open_lake, rows};
use anyhow::{anyhow, bail, Result};
use chrono::{NaiveDate, NaiveDateTime, Timelike};
use matter_controller::{
    CaptureItem, DeviceFeed, DeviceSubscription, FastSubscription, SwitchOn, TypedValue, AIR_QUALITY_CLUSTER, CO2_CLUSTER, ELECTRICAL_ENERGY_MEASUREMENT_CLUSTER, ELECTRICAL_POWER_MEASUREMENT_CLUSTER,
    HUMIDITY_MEASUREMENT_CLUSTER, ON_OFF_CLUSTER, PM25_CLUSTER, POWER_SOURCE_CLUSTER, TEMPERATURE_MEASUREMENT_CLUSTER,
};
use serde::Deserialize;
use serde_json::{json, Value};
use std::collections::BTreeMap;
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

impl DeviceKind {
    fn fast_clusters(self) -> &'static [u32] {
        match self {
            Self::Plug => &[ON_OFF_CLUSTER, ELECTRICAL_POWER_MEASUREMENT_CLUSTER, ELECTRICAL_ENERGY_MEASUREMENT_CLUSTER],
            Self::Air => &[AIR_QUALITY_CLUSTER, TEMPERATURE_MEASUREMENT_CLUSTER, HUMIDITY_MEASUREMENT_CLUSTER, CO2_CLUSTER, PM25_CLUSTER],
            Self::Climate => &[],
        }
    }

    fn view_clusters(self) -> &'static [u32] {
        match self {
            Self::Climate => &[TEMPERATURE_MEASUREMENT_CLUSTER, HUMIDITY_MEASUREMENT_CLUSTER, POWER_SOURCE_CLUSTER],
            other => other.fast_clusters(),
        }
    }

    fn primary(self) -> (u32, u32) {
        match self {
            Self::Plug => (1, 60),
            Self::Air => (10, 300),
            Self::Climate => (60, 1800),
        }
    }

    fn silent_after_min(self) -> f64 {
        match self {
            Self::Plug => 30.0,
            Self::Air => 60.0,
            Self::Climate => 480.0,
        }
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
        let (min_s, max_s) = (self.min_s.unwrap_or(default_min), self.max_s.unwrap_or(default_max));
        match self.kind.fast_clusters() {
            [] => DeviceSubscription { min_s, max_s, fast: None },
            clusters => DeviceSubscription {
                min_s: self.rest_min_s.unwrap_or(300),
                max_s: self.rest_max_s.unwrap_or(900),
                fast: Some(FastSubscription { clusters: clusters.to_vec(), min_s, max_s }),
            },
        }
    }

    pub fn silent_after(&self) -> Duration {
        Duration::from_secs_f64(self.silent_after_min.unwrap_or(self.kind.silent_after_min()) * 60.0)
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
    Attempt { plug: CoffeePlug, day: NaiveDate, local: NaiveDateTime },
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
    latest: BTreeMap<(u16, u32, u32), Latest>,
}

#[derive(Default)]
struct Inner {
    configs: Vec<DeviceConfig>,
    nodes: BTreeMap<u64, NodeState>,
    edge: WakeEdge,
    done: BTreeMap<u64, NaiveDate>,
    away_logged: BTreeMap<u64, NaiveDate>,
    day_file: Option<PathBuf>,
}

impl Inner {
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

    fn wake_step(&mut self, local: NaiveDateTime, presence_known: bool, sleep: bool, away: bool) -> Vec<WakeStep> {
        if !presence_known || !self.edge.advance(sleep) {
            return Vec::new();
        }
        let day = local.date();
        let plugs: Vec<CoffeePlug> = self.configs.iter().filter_map(CoffeePlug::from_config).filter(|p| p.in_window(local)).collect();
        let mut steps = Vec::new();
        for plug in plugs {
            if self.done.get(&plug.node) == Some(&day) {
                continue;
            }
            if away {
                if self.away_logged.insert(plug.node, day) != Some(day) {
                    steps.push(WakeStep::Away { plug, day, local });
                }
                continue;
            }
            self.done.insert(plug.node, day);
            steps.push(WakeStep::Attempt { plug, day, local });
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
        let nodes = config.devices.iter().map(|d| (d.node, NodeState { started_us: now_us, ..Default::default() })).collect();
        let inner = Inner { configs: config.devices.clone(), nodes, done, day_file: Some(day_file), ..Default::default() };
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
                log_row("device reading", sink.append(row));
                if let Some((since, now)) = recovered {
                    self.link_event(sink, node, "ok", json!({"silent_s": (now - since) / 1_000_000}));
                }
            }
            CaptureItem::SubscriptionLost { reason, .. } => {
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
                let was_lost = self.inner.lock().unwrap().nodes.get_mut(&node).is_some_and(|s| std::mem::replace(&mut s.lost, false));
                if was_lost {
                    self.link_event(sink, node, "resubscribed", json!({}));
                }
            }
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
            state.next_resubscribe_us = now_us + (state.resubscribe_backoff_s * 1e6) as i64;
            state.resubscribe_backoff_s = (state.resubscribe_backoff_s * 2.0).min(MAX_RESUBSCRIBE_BACKOFF.as_secs_f64());
            due.push((config.node, first));
        }
        due
    }

    pub async fn health_tick(&self, sink: &Sink, now_us: i64) {
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

    pub fn on_control(&self, state: &AppState, now: NaiveDateTime, presence_known: bool, sleep: bool, away: bool) {
        let steps = self.inner.lock().unwrap().wake_step(now, presence_known, sleep, away);
        for step in steps {
            match step {
                WakeStep::Away { plug, day, local } => coffee_event(&state.sink, &plug, day, local, json!({"reason": "away", "ok": false})),
                WakeStep::Attempt { plug, day, local } => {
                    if let Some(switch) = self.switch.clone() {
                        tokio::spawn(wake_attempts(switch, state.sink.clone(), plug, day, local, self.wake_timing.unwrap_or_default()));
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

pub async fn wake_attempts(switch: Arc<dyn SwitchOn>, sink: Sink, plug: CoffeePlug, day: NaiveDate, local: NaiveDateTime, timing: WakeTiming) {
    let started = Instant::now();
    let mut attempt = 0u32;
    let outcome = loop {
        attempt += 1;
        match plug.turn_on(switch.as_ref()).await {
            Ok(()) => break Ok(()),
            Err(e) if started.elapsed() + timing.retry > timing.deadline => break Err(e.to_string()),
            Err(_) => sleep(timing.retry).await,
        }
    };
    let extra = match outcome {
        Ok(()) => json!({"reason": "wake", "attempt": attempt, "ok": true}),
        Err(error) => json!({"reason": "gave_up", "attempt": attempt, "ok": false, "error": error}),
    };
    coffee_event(&sink, &plug, day, local, extra);
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
            Some("wake") => format!("um {} UTC eingeschaltet, Schlafmodus der Uhr endete {} Ortszeit, Versuch {}", hhmm(*us), d["local_time"].as_str().unwrap_or("?"), d["attempt"]),
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
