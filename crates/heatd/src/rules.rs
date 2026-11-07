use crate::config::{Config, Room};
use crate::http::AppState;
use crate::explore::{Experiment, Inputs, Lifecycle};
use crate::lake::{self, kind, Decision, DecisionInputs, Label};
use crate::storage::{now_ms, Motion, Power, Reading};
use anyhow::Result;
use chrono::{Datelike, Local, NaiveDateTime, Timelike};
use matter_controller::{AttributeReport, OCCUPIED_HEATING_SETPOINT};
use serde_json::json;
use std::collections::{HashMap, HashSet};
use std::time::Duration;

pub const MIN_CHANGE_C: f64 = 0.5;
pub const MANUAL_TOLERANCE_C: f64 = 0.05;
const DESIRED_EPS_C: f64 = 0.01;
const GRACE_FACTOR: i64 = 2;
const BUDGET_WINDOW_MS: i64 = 3_600_000;

type Series<'a> = (&'a str, &'a str, &'a str, &'a dyn Fn(&Room) -> f64);

#[derive(Debug, Default)]
pub struct RaiseBudget {
    raises: HashMap<String, Vec<i64>>,
    held: HashMap<u64, f64>,
    deferred_total: HashMap<String, u64>,
}

impl RaiseBudget {
    fn live(&self, room: &str, now_ms: i64) -> Vec<i64> {
        self.raises.get(room).into_iter().flatten().copied().filter(|t| now_ms - t < BUDGET_WINDOW_MS).collect()
    }

    pub fn allows(&self, room: &str, now_ms: i64, limit: u32) -> bool {
        let live = self.live(room, now_ms);
        limit == 0 || live.contains(&now_ms) || live.len() < limit as usize
    }

    fn record(&mut self, room: &str, now_ms: i64) {
        let entries = self.raises.entry(room.into()).or_default();
        entries.retain(|t| now_ms - t < BUDGET_WINDOW_MS);
        entries.push(now_ms);
    }

    pub fn used(&self, room: &str, now_ms: i64) -> usize {
        self.live(room, now_ms).len()
    }

    pub fn next_free_ms(&self, room: &str, now_ms: i64, limit: u32) -> Option<i64> {
        let live = self.live(room, now_ms);
        (limit > 0 && live.len() >= limit as usize).then(|| live[live.len() - limit as usize] + BUDGET_WINDOW_MS)
    }

    fn defer(&mut self, room: &str, node: u64, setpoint: f64) -> bool {
        let fresh = self.held.insert(node, setpoint).is_none_or(|d| (d - setpoint).abs() >= DESIRED_EPS_C);
        if fresh {
            *self.deferred_total.entry(room.into()).or_default() += 1;
        }
        fresh
    }

    pub fn deferred(&self, nodes: &[u64]) -> Option<f64> {
        nodes.iter().find_map(|n| self.held.get(n).copied())
    }

    pub fn view(&self, room: &Room, now_ms: i64, limit: u32) -> serde_json::Value {
        json!({"limit": limit, "used": self.used(&room.name, now_ms), "next_free_ts": self.next_free_ms(&room.name, now_ms, limit), "deferred_setpoint": self.deferred(&room.nodes)})
    }

    pub fn metrics(&self, rooms: &[Room], now_ms: i64, limit: u32) -> String {
        let series: [Series; 5] = [
            ("heatd_raise_budget_limit", "gauge", "setpoint raises allowed per room and hour (0 = unlimited)", &|_| f64::from(limit)),
            ("heatd_raise_budget_used", "gauge", "setpoint raises of the room in the last 60 minutes", &|r| self.used(&r.name, now_ms) as f64),
            ("heatd_raise_next_free_seconds", "gauge", "seconds until the room may raise again, 0 when it may now", &|r| self.next_free_ms(&r.name, now_ms, limit).map_or(0.0, |t| ((t - now_ms) as f64 / 1000.0).max(0.0))),
            ("heatd_raise_deferred", "gauge", "1 while a raise of the room waits for budget", &|r| f64::from(u8::from(self.deferred(&r.nodes).is_some()))),
            ("heatd_raises_deferred_total", "counter", "raises deferred by the budget since start", &|r| self.deferred_total.get(&r.name).copied().unwrap_or(0) as f64),
        ];
        series
            .iter()
            .map(|(name, kind, help, value)| format!("# HELP {name} {help}
# TYPE {name} {kind}
{}", rooms.iter().map(|r| format!("{name}{{room=\"{}\"}} {}
", r.name, value(r))).collect::<String>()))
            .collect()
    }
}

#[derive(Debug, Clone)]
pub struct Sent {
    pub target: f64,
    pub previous: Option<f64>,
    pub written_ms: i64,
    pub acked: Option<f64>,
    pub stale: Option<f64>,
}

#[derive(Debug, Clone, Copy, PartialEq)]
pub enum Check {
    Unknown,
    Pending,
    Match,
    Mismatch(f64, f64, i64),
}

#[derive(Debug, Clone)]
pub struct Hold {
    pub value: f64,
    pub since_ms: i64,
}

#[derive(Debug, Default)]
pub struct Occ {
    candidate: Option<(String, i64)>,
    seen: HashMap<String, (i64, i64)>,
    logged: Option<(String, i64)>,
}

#[derive(Debug, Default)]
pub struct OccStep {
    pub held: HashSet<String>,
    pub ended: Vec<(String, i64, i64)>,
    pub pending: Option<(String, i64)>,
}

impl Occ {
    pub fn step(&mut self, detected: Option<&str>, tap: bool, away: bool, now: i64, enter_ms: i64, exit_ms: i64) -> OccStep {
        let mut ended: Vec<(String, i64, i64)> = Vec::new();
        if away {
            ended.extend(self.seen.drain().map(|(r, (s, _))| (r, s, now)));
        }
        match detected.filter(|_| !away) {
            Some(room) => {
                if self.candidate.as_ref().is_none_or(|c| c.0 != room) {
                    self.candidate = Some((room.into(), now));
                }
                let since = self.candidate.as_ref().map_or(now, |c| c.1);
                match self.seen.get_mut(room) {
                    Some(e) => e.1 = now,
                    None if tap || now - since >= enter_ms => {
                        self.seen.insert(room.into(), (now, now));
                    }
                    None => {}
                }
            }
            None => self.candidate = None,
        }
        let expired: Vec<String> = self.seen.iter().filter(|(_, (_, last))| now - last >= exit_ms).map(|(r, _)| r.clone()).collect();
        for room in expired {
            if let Some((start, last)) = self.seen.remove(&room) {
                ended.push((room, start, last + exit_ms));
            }
        }
        let pending = self.candidate.clone().filter(|(r, _)| !self.seen.contains_key(r));
        OccStep { held: self.seen.keys().cloned().collect(), ended, pending }
    }

    fn restore(&mut self, room: &str, since: i64, now: i64, enter_ms: i64) {
        self.candidate = Some((room.into(), since));
        if now - since >= enter_ms {
            self.seen.insert(room.into(), (since, now));
        }
    }

    fn fresh_pending(&mut self, pending: &Option<(String, i64)>) -> bool {
        match pending {
            Some(p) if self.logged.as_ref() != Some(p) => {
                self.logged = Some(p.clone());
                true
            }
            _ => false,
        }
    }
}

#[derive(Debug, Default)]
pub struct Commands {
    sent: HashMap<u64, Sent>,
    device: HashMap<u64, f64>,
    device_ms: HashMap<u64, i64>,
    holds: HashMap<u64, Hold>,
    last_write: HashMap<u64, (i64, &'static str)>,
    deferred: HashMap<u64, f64>,
    pub budget: RaiseBudget,
    restored: HashSet<u64>,
    resets: HashMap<u64, i64>,
    last_reset: HashMap<u64, i64>,
    occ: Occ,
}

impl Commands {
    #[cfg(test)]
    pub fn occ_probe(&mut self, detected: Option<&str>, now: i64, enter_ms: i64) -> OccStep {
        self.occ.step(detected, false, false, now, enter_ms, 3_600_000)
    }

    pub fn restore_occupancy(&mut self, room: &str, since: i64, now: i64, enter_ms: i64) {
        self.occ.restore(room, since, now, enter_ms);
    }

    pub fn target(&self, node: u64) -> Option<f64> {
        self.sent.get(&node).map(|s| s.target)
    }

    pub fn check(&self, node: u64, tolerance: f64, now_ms: i64, pending_ms: i64) -> Check {
        let (Some(sent), Some(device), Some(at)) = (self.sent.get(&node), self.device.get(&node), self.device_ms.get(&node)) else { return Check::Unknown };
        if self.holds.contains_key(&node) || sent.written_ms <= 0 {
            return Check::Unknown;
        }
        if *at < sent.written_ms {
            return if sent.stale.is_some() && now_ms - sent.written_ms >= pending_ms { Check::Mismatch(sent.target, *device, sent.written_ms) } else { Check::Pending };
        }
        let near = |a: f64| (device - a).abs() < tolerance;
        if near(sent.target) || sent.acked.is_some_and(near) {
            Check::Match
        } else {
            Check::Mismatch(sent.target, *device, sent.written_ms)
        }
    }

    pub fn claim_rewrite(&mut self, node: u64, target: f64, written_ms: i64, device: f64, now_ms: i64) -> Option<Sent> {
        let current = self.sent.get(&node).filter(|s| s.target == target && s.written_ms == written_ms)?.clone();
        self.sent.insert(node, Sent { target, previous: Some(device), written_ms: now_ms, acked: None, stale: Some(device) });
        Some(current)
    }

    pub fn release_rewrite(&mut self, node: u64, claimed_ms: i64, prior: Sent) {
        if self.sent.get(&node).is_some_and(|s| s.written_ms == claimed_ms) {
            self.sent.insert(node, prior);
        }
    }

    #[cfg(test)]
    pub fn test_sent(&mut self, node: u64, target: f64, previous: f64, written_ms: i64) {
        self.sent.insert(node, Sent { target, previous: Some(previous), written_ms, acked: None, stale: None });
    }

    pub fn forget_desired_writes(&mut self) {
        self.sent.clear();
        self.device.clear();
        self.device_ms.clear();
        self.last_write.clear();
        self.deferred.clear();
        self.budget.held.clear();
        self.restored.clear();
        self.resets.clear();
    }

    #[cfg(test)]
    pub fn test_hold(&mut self, node: u64, value: f64) {
        self.holds.insert(node, Hold { value, since_ms: 0 });
    }

    pub fn holding(&self, nodes: &[u64]) -> bool {
        nodes.iter().any(|n| self.holds.contains_key(n))
    }

    fn drifted(&self, node: u64, grace_ms: i64) -> bool {
        let near = |a: f64, b: f64| (a - b).abs() < MANUAL_TOLERANCE_C;
        let (Some(device), Some(at), Some(sent)) = (self.device.get(&node), self.device_ms.get(&node), self.sent.get(&node)) else { return false };
        at - sent.written_ms >= grace_ms && !self.holds.contains_key(&node) && !near(*device, sent.target) && !sent.acked.is_some_and(|a| near(a, *device))
    }

    pub fn hold(&self, node: u64) -> Option<&Hold> {
        self.holds.get(&node)
    }

    fn release_due(&mut self, want: &Desired, now_ms: i64, hold_ms: i64) -> Option<bool> {
        let Some(hold) = self.holds.get(&want.node) else { return Some(false) };
        if now_ms - hold.since_ms < hold_ms && !matches!(want.reason, "window_open" | "away") {
            return None;
        }
        if let Some(hold) = self.holds.remove(&want.node) {
            self.sent.insert(want.node, Sent { target: hold.value, previous: None, written_ms: 0, acked: None, stale: None });
        }
        Some(true)
    }
}

#[derive(Debug, Clone, PartialEq)]
pub struct Presence {
    pub room: Option<String>,
    pub prediction_age_minutes: Option<f64>,
    pub motion: Option<Motion>,
    pub power: Option<Power>,
    pub away: bool,
    pub contact_age_minutes: Option<f64>,
    pub source: &'static str,
    pub sleep: bool,
}

#[derive(Debug, Clone, PartialEq)]
pub enum Occupancy<'a> {
    Room(&'a str),
    Away,
    Unknown,
}

impl Presence {
    pub fn detected(&self, config: &Config) -> Option<String> {
        match self.occupancy(config) {
            Occupancy::Room(r) => Some(r.to_string()),
            _ => None,
        }
    }

    pub fn occupancy(&self, config: &Config) -> Occupancy<'_> {
        if self.contact_age_minutes.is_some_and(|a| a > config.still_contact_timeout_minutes) || matches!(self.power, Some(Power::Saver | Power::Offbody)) {
            return Occupancy::Unknown;
        }
        if self.away {
            return Occupancy::Away;
        }
        let still = self.motion == Some(Motion::Still) && self.contact_age_minutes.is_some();
        let fresh = self.prediction_age_minutes.is_some_and(|a| a <= config.presence_timeout_minutes);
        match &self.room {
            Some(room) if still || fresh => Occupancy::Room(room),
            _ => Occupancy::Unknown,
        }
    }
}

#[derive(Debug, Clone, PartialEq)]
pub struct Desired {
    pub node: u64,
    pub room: String,
    pub setpoint: f64,
    pub reason: &'static str,
    pub rule: &'static str,
    pub base: f64,
    pub occupancy: &'static str,
    pub source: &'static str,
    pub sleep: bool,
    pub window_open: bool,
    pub experiment_id: Option<String>,
}

fn scheduled(room: &Room, now: NaiveDateTime) -> Option<f64> {
    let minute = now.hour() * 60 + now.minute();
    let today = now.weekday();
    let yesterday = today.pred();
    room.schedules.iter().find_map(|s| {
        let (from, to) = s.minutes().ok()?;
        let days = s.weekdays().ok()?;
        let hit = if from == to {
            days.contains(&today)
        } else if from < to {
            days.contains(&today) && (from..to).contains(&minute)
        } else {
            (days.contains(&today) && minute >= from) || (days.contains(&yesterday) && minute < to)
        };
        hit.then_some(s.setpoint)
    })
}

pub fn base_setpoint(room: &Room, config: &Config, now: NaiveDateTime) -> f64 {
    scheduled(room, now).unwrap_or(config.default_setpoint)
}

fn in_sleep_hours(config: &Config, now: NaiveDateTime) -> bool {
    let Ok((from, to)) = config.watchmatch.state.sleep_minutes() else { return false };
    let minute = now.hour() * 60 + now.minute();
    if from <= to { (from..to).contains(&minute) } else { minute >= from || minute < to }
}

pub fn desired(now: NaiveDateTime, config: &Config, presence: Option<&Presence>, held: &HashSet<String>, window_open: &HashSet<String>, exploring: &HashMap<String, Experiment>) -> Vec<Desired> {
    let occupancy = presence.map_or(Occupancy::Unknown, |p| p.occupancy(config));
    let (source, sleeping) = presence.map_or(("", false), |p| (p.source, p.sleep));
    let night = in_sleep_hours(config, now);
    config
        .rooms
        .iter()
        .flat_map(|room| {
            let sleep_here = (sleeping || night) && config.sleep_room.as_deref() == Some(room.name.as_str());
            let (base, base_reason) = match (scheduled(room, now), room.home_setpoint) {
                (Some(s), _) => (s, "schedule"),
                (None, Some(h)) if !sleep_here && occupancy != Occupancy::Away => (h, "home"),
                _ => (config.default_setpoint, "default"),
            };
            let window = window_open.contains(&room.name);
            let experiment = exploring.get(&room.name);
            let (setpoint, reason, rule) = if window {
                (config.frost_setpoint, "window_open", "window")
            } else if occupancy == Occupancy::Away {
                (config.away_setpoint, "away", "presence")
            } else if let Some(e) = experiment {
                (e.setpoint.max(config.frost_setpoint), "exploration", "exploration")
            } else {
                match held.contains(&room.name) {
                    _ if sleep_here => (base, "sleep", "presence"),
                    true => (config.occupied_setpoint, "occupied", "presence"),
                    false => (base, base_reason, "schedule"),
                }
            };
            let label = match occupancy {
                Occupancy::Room(_) => "room",
                Occupancy::Away => "away",
                Occupancy::Unknown => "unknown",
            };
            let id = experiment.filter(|_| reason == "exploration").map(|e| e.id.clone());
            room.nodes.iter().map(move |&node| Desired { node, room: room.name.clone(), setpoint, reason, rule, base, occupancy: label, source, sleep: sleeping, window_open: window, experiment_id: id.clone() })
        })
        .collect()
}

#[derive(Debug, Clone, Copy, PartialEq)]
pub enum Transition {
    Left { since: i64 },
    Arrived,
}

#[derive(Debug, Default)]
struct Track {
    last_ts: i64,
    away_since: Option<i64>,
    moved: bool,
    confirmed: bool,
}

#[derive(Debug, Default)]
pub struct Tracker {
    devices: HashMap<String, Track>,
}

impl Tracker {
    pub fn observe(&mut self, device: &str, ts: i64, away: bool, motion: Option<Motion>, power: Option<Power>) -> Option<Transition> {
        let t = self.devices.entry(device.to_string()).or_default();
        if ts < t.last_ts {
            return None;
        }
        t.last_ts = ts;
        if !away {
            t.away_since = None;
            t.moved = false;
            return std::mem::take(&mut t.confirmed).then_some(Transition::Arrived);
        }
        t.away_since.get_or_insert(ts);
        t.moved |= motion == Some(Motion::Moving) && power != Some(Power::Offbody);
        None
    }

    pub fn poll(&mut self, now_ms: i64, guard_minutes: f64) -> Vec<(String, Transition)> {
        let mut out = Vec::new();
        for (device, t) in self.devices.iter_mut().filter(|(_, t)| !t.confirmed && t.moved) {
            if let Some(since) = t.away_since.filter(|s| (now_ms - s) as f64 >= guard_minutes * 60_000.0) {
                t.confirmed = true;
                out.push((device.clone(), Transition::Left { since }));
            }
        }
        out
    }

    pub fn confirmed(&self, device: &str) -> bool {
        self.devices.get(device).is_some_and(|t| t.confirmed)
    }
}

pub fn presence_event(device: &str, transition: Transition) -> lake::Event {
    let (name, ts) = match transition {
        Transition::Left { since } => (kind::PRESENCE_LEAVE, Some(since * 1000)),
        Transition::Arrived => (kind::PRESENCE_ARRIVE, None),
    };
    let mut event = lake::Event::now(name, "heatd").with_detail(json!({"source": "watch_wifi", "device": device}));
    if let Some(ts) = ts {
        event.ts = ts;
    }
    event
}

pub(crate) fn current_presence(state: &AppState, now_ms: i64) -> Result<Option<Presence>> {
    let minutes = |ms: i64| (now_ms - ms) as f64 / 60_000.0;
    let prediction = state.storage.last_predictions()?.into_iter().max_by_key(|p| p.ts);
    let statuses = state.storage.device_statuses()?;
    let status = match &prediction {
        Some(p) => statuses.iter().find(|s| s.device == p.device),
        None => statuses.iter().max_by_key(|s| s.state_ts),
    }
    .cloned();
    let matched = state.watchmatch.effective(now_ms);
    let stay = state
        .watchmatch
        .stay()
        .filter(|(_, _, ts)| minutes(*ts) <= state.config.presence_timeout_minutes)
        .filter(|(device, _, ts)| !statuses.iter().any(|s| &s.device == device && s.motion == Some(Motion::Moving) && s.state_ts > *ts));
    if prediction.is_none() && status.is_none() && stay.is_none() && !matched.as_ref().is_some_and(Option::is_some) {
        return Ok(None);
    }
    let away = status.as_ref().is_some_and(|s| s.away && state.tracker.lock().unwrap().confirmed(&s.device));
    let (model_room, model_age) = match matched {
        Some(held) => (held.as_ref().map(|h| h.0.clone()), held.as_ref().map(|h| minutes(h.1))),
        None => (prediction.as_ref().map(|p| p.room.clone()), prediction.as_ref().map(|p| minutes(p.ts))),
    };
    let model_fresh = model_age.is_some_and(|a| a <= state.config.presence_timeout_minutes);
    let (room, prediction_age_minutes, source) = match stay {
        Some((_, label, ts)) if !model_fresh => (Some(label), Some(minutes(ts)), "stay_label"),
        _ => (model_room, model_age, "matcher"),
    };
    let sleep = state.watchmatch.sleeping(now_ms) || status.as_ref().is_some_and(|s| s.bedtime == Some(true));
    Ok(Some(Presence {
        prediction_age_minutes,
        room,
        source,
        sleep,
        motion: status.as_ref().and_then(|s| s.motion),
        power: status.as_ref().and_then(|s| s.power),
        away,
        contact_age_minutes: status.as_ref().map(|s| minutes(s.contact_ms)),
    }))
}

fn log<T>(what: &str, result: Result<T>) {
    if let Err(e) = result {
        eprintln!("{what}: {e}");
    }
}

fn lifecycle_event(state: &AppState, life: Lifecycle) {
    let (name, e, reason) = match &life {
        Lifecycle::Started(e) => (kind::EXPERIMENT_START, e, None),
        Lifecycle::Ended(e) => (kind::EXPERIMENT_END, e, None),
        Lifecycle::Aborted(e, why) => (kind::EXPERIMENT_ABORT, e, Some(*why)),
    };
    let mut detail = json!({"room": e.room, "type": e.kind.name(), "setpoint": e.setpoint, "base": e.base, "duration_min": (e.end - e.start).num_minutes()});
    if let Some(why) = reason {
        detail["reason"] = json!(why);
    }
    let mut event = lake::Event::now(name, "heatd").with_experiment(&e.id).with_detail(detail);
    event.ts = state.sink.now_us();
    event.node = state.config.rooms.iter().find(|r| r.name == e.room).and_then(|r| r.nodes.first().copied());
    log("experiment event", state.sink.append(event));
}

pub fn window_event(state: &AppState, e: crate::window::WinEvent) {
    let mut event = lake::Event::now(e.kind, "heatd").with_detail(e.detail);
    event.node = state.config.rooms.iter().find(|r| r.name == e.room).and_then(|r| r.nodes.first().copied());
    log("window event", state.sink.append(event));
}

fn mode_name(value: f64) -> String {
    match value as u8 {
        0 => "off",
        1 => "auto",
        3 => "cool",
        4 => "heat",
        5 => "emergency_heat",
        7 => "fan_only",
        _ => "other",
    }
    .into()
}

fn proposal(state: &AppState, want: &Desired, now_ms: i64, old: Option<f64>, readings: &[Reading], presence: Option<&Presence>) -> Decision {
    let reading = |name: &str| readings.iter().find(|r| r.node == want.node && r.attribute == name).map(|r| r.value);
    Decision {
        ts: now_ms * 1000,
        room: want.room.clone(),
        node: want.node,
        rule: want.rule.into(),
        inputs: DecisionInputs {
            local_temp: reading("local_temperature"),
            schedule_setpoint: Some(want.base),
            frost_setpoint: Some(state.config.frost_setpoint),
            demand_pct: reading("pi_heating_demand"),
            battery_pct: reading("bat_percent_remaining").map(|v| v / 2.0),
            prediction_age_min: presence.and_then(|p| p.prediction_age_minutes),
            contact_age_min: presence.and_then(|p| p.contact_age_minutes),
            occupancy: Some(want.occupancy.into()),
            room_estimate: presence.and_then(|p| p.room.clone()),
            window_open: Some(want.window_open),
            away: Some(want.occupancy == "away"),
            system_mode: reading("system_mode").map(mode_name),
            extra: Some(json!({"phase": "proposal", "reason": want.reason, "source": want.source, "sleep": want.sleep}).to_string()),
            ..Default::default()
        },
        setpoint_old: old,
        setpoint_new: Some(want.setpoint),
        exploration: want.experiment_id.is_some(),
        experiment_id: want.experiment_id.clone(),
        proposed: true,
        applied: false,
        commit: lake::COMMIT.into(),
        ..Default::default()
    }
}

fn deferred_row(state: &AppState, want: &Desired, now_ms: i64, old: Option<f64>, readings: &[Reading], presence: Option<&Presence>, why: &str) -> Decision {
    let mut row = proposal(state, want, now_ms, old, readings, presence);
    row.inputs.extra = Some(json!({"phase": "proposal", "reason": why, "wanted": want.reason, "source": want.source, "sleep": want.sleep, "deferred": true}).to_string());
    row
}

fn tag_budget(row: &mut Decision, budget: &str) {
    let mut extra: serde_json::Value = row.inputs.extra.as_deref().and_then(|e| serde_json::from_str(e).ok()).unwrap_or_else(|| json!({}));
    extra["budget"] = json!(budget);
    row.inputs.extra = Some(extra.to_string());
}

fn raise_verdict(state: &AppState, commands: &Commands, want: &Desired, old: Option<f64>, last_write: Option<(i64, &'static str)>, now_ms: i64) -> Verdict {
    let limit = state.config.raise_budget_per_hour;
    if limit == 0 || !old.is_some_and(|o| want.setpoint > o) {
        return Verdict::Free;
    }
    let levels = [state.config.failsafe.setpoint, state.config.br_watchdog.setpoint_c];
    if !commands.restored.contains(&want.node) && old.is_some_and(|o| levels.iter().any(|l| (o - l).abs() < MANUAL_TOLERANCE_C)) {
        return Verdict::Exempt("restore");
    }
    if want.reason == "no_supply" {
        return Verdict::Exempt("supply_cap");
    }
    if last_write.is_some_and(|(_, r)| matches!(r, "window_open" | "pi_reset")) {
        return Verdict::Exempt("window_close");
    }
    if commands.budget.allows(&want.room, now_ms, limit) {
        Verdict::Raise
    } else {
        Verdict::Deferred
    }
}

#[derive(Debug, Clone, Copy, PartialEq)]
enum Verdict {
    Free,
    Exempt(&'static str),
    Raise,
    Deferred,
}

fn result_row(proposal: &Decision, reason: &str, error: Option<String>) -> Decision {
    let mut row = proposal.clone();
    row.applied = error.is_none();
    let mut extra = json!({"phase": "result", "reason": reason, "ref_ts": proposal.ts});
    if let Some(e) = error {
        extra["error"] = json!(e);
    }
    row.inputs.extra = Some(extra.to_string());
    row
}

const SAFE_MIN_C: f64 = 7.5;
const SAFE_MAX_C: f64 = 28.5;

pub fn setpoint_limits(readings: &[Reading], node: u64) -> (f64, f64) {
    let find = |attribute: &str| readings.iter().find(|r| r.node == node && r.attribute == attribute).map(|r| r.value).filter(|v| (0.0..=50.0).contains(v));
    match (find("min_heat_setpoint_limit"), find("max_heat_setpoint_limit")) {
        (lo, hi) if lo.unwrap_or(SAFE_MIN_C) < hi.unwrap_or(SAFE_MAX_C) => (lo.unwrap_or(SAFE_MIN_C), hi.unwrap_or(SAFE_MAX_C)),
        _ => (SAFE_MIN_C, SAFE_MAX_C),
    }
}

pub fn clamp_setpoint(readings: &[Reading], node: u64, setpoint: f64, cap: f64) -> f64 {
    let (lo, hi) = setpoint_limits(readings, node);
    setpoint.clamp(lo, hi.min(cap))
}

fn clamp_boost(readings: &[Reading], node: u64, setpoint: f64, cap: f64) -> f64 {
    setpoint.clamp(setpoint_limits(readings, node).0, cap)
}

fn supply_step(state: &AppState, wants: &mut [Desired], readings: &[Reading], now_ms: i64, window_busy: &HashSet<String>, exploring: &HashMap<String, Experiment>) {
    let cfg = &state.config.supply;
    if !cfg.enabled {
        return;
    }
    let no_supply = state.windows.lock().unwrap().no_supply_rooms();
    let frost = state.config.frost_setpoint;
    let reset_event = |want: &Desired, phase: &str, detail: serde_json::Value| {
        let mut event = lake::Event::now(kind::PI_RESET, "heatd").with_detail(json!({"room": want.room, "state": phase, "detail": detail}));
        event.node = Some(want.node);
        log("pi reset event", state.sink.append(event));
    };
    for want in wants.iter_mut() {
        let blocked = window_busy.contains(&want.room) || exploring.contains_key(&want.room) || matches!(want.reason, "window_open" | "away" | "boost" | "boost_off" | "exploration") || state.commanded.lock().unwrap().holding(&[want.node]);
        let running = state.commanded.lock().unwrap().resets.get(&want.node).copied();
        if let Some(until) = running {
            if !blocked && now_ms < until {
                want.setpoint = clamp_setpoint(readings, want.node, frost, state.failsafe.max_c());
                want.reason = "pi_reset";
                want.rule = "supply";
                continue;
            }
            state.commanded.lock().unwrap().resets.remove(&want.node);
            reset_event(want, "end", json!({"aborted": blocked}));
        }
        if blocked {
            state.windows.lock().unwrap().clear_overshoot(want.node);
            continue;
        }
        if no_supply.contains(&want.room) {
            let Some(temp) = state.windows.lock().unwrap().local_temp(want.node, now_ms, state.config.stale.after_ms()) else {
                if let Some(held) = state.commanded.lock().unwrap().target(want.node).filter(|h| want.setpoint > *h) {
                    want.setpoint = held;
                    want.reason = "no_supply";
                    want.rule = "supply";
                }
                continue;
            };
            let cap = clamp_setpoint(readings, want.node, (((temp + cfg.cap_k) * 2.0).floor() / 2.0).max(frost), state.failsafe.max_c());
            if want.setpoint > cap {
                want.setpoint = cap;
                want.reason = "no_supply";
                want.rule = "supply";
            }
            continue;
        }
        let due = state.windows.lock().unwrap().overshoot_due(cfg, want.node, want.setpoint, now_ms, state.config.stale.after_ms());
        let gap = (cfg.reset_gap_min * 60_000.0) as i64;
        let Some((temp, valve)) = due.filter(|_| state.commanded.lock().unwrap().last_reset.get(&want.node).is_none_or(|t| now_ms - t >= gap)) else { continue };
        {
            let mut commands = state.commanded.lock().unwrap();
            commands.resets.insert(want.node, now_ms + (cfg.reset_min * 60_000.0) as i64);
            commands.last_reset.insert(want.node, now_ms);
        }
        state.windows.lock().unwrap().clear_overshoot(want.node);
        reset_event(want, "start", json!({"temp": temp, "setpoint": want.setpoint, "valve": valve}));
        want.setpoint = clamp_setpoint(readings, want.node, frost, state.failsafe.max_c());
        want.reason = "pi_reset";
        want.rule = "supply";
    }
}

#[allow(clippy::too_many_arguments)]
fn boost_step(state: &AppState, wants: &mut [Desired], readings: &[Reading], now: NaiveDateTime, now_ms: i64, window_busy: &HashSet<String>, away: bool, exploring: &HashMap<String, Experiment>) {
    if !state.boost.enabled() {
        return;
    }
    let held = |room: &Room| state.commanded.lock().unwrap().holding(&room.nodes);
    let rooms: Vec<crate::boost::RoomIn> = state
        .config
        .rooms
        .iter()
        .filter_map(|room| {
            let want = wants.iter().find(|w| w.room == room.name)?;
            let blocked = if window_busy.contains(&room.name) {
                Some("window_open")
            } else if away {
                Some("away")
            } else if held(room) {
                Some("manual_hold")
            } else if exploring.contains_key(&room.name) {
                Some("exploration")
            } else {
                None
            };
            Some(crate::boost::RoomIn { room: room.name.clone(), setpoint: want.setpoint, blocked, no_raise: !state.commanded.lock().unwrap().budget.allows(&room.name, now_ms, state.config.raise_budget_per_hour) })
        })
        .collect();
    let now_us = lake::now_us();
    let out = state.boost.step(&crate::boost::StepIn { now_ms, hour: f64::from(now.hour()) + f64::from(now.minute()) / 60.0, outdoor: state.status.outdoor(now_us), radiation: state.status.radiation(now_us), rooms: &rooms });
    out.events.into_iter().for_each(|e| log("boost event", state.sink.append(e)));
    for want in wants.iter_mut() {
        match out.directives.get(&want.room) {
            Some(crate::boost::Directive::Boost { experiment_id }) => {
                want.setpoint = clamp_boost(readings, want.node, state.config.boost.boost_setpoint, state.failsafe.boost_max_c());
                want.reason = "boost";
                want.rule = "boost";
                want.experiment_id = Some(experiment_id.clone());
            }
            Some(crate::boost::Directive::Switch) => {
                want.reason = "boost_off";
                want.rule = "boost";
            }
            None => {}
        }
    }
}

pub(crate) async fn send_setpoint(state: &AppState, node: u64, setpoint: f64) -> matter_controller::Result<()> {
    let target = || Some(setpoint);
    if setpoint > state.failsafe.max_c() {
        crate::failsafe::ensure(state, node, &target, false).await;
    }
    let outcome = state.controller.write_heating_setpoint(node, setpoint).await;
    if outcome.is_ok() {
        crate::failsafe::ensure(state, node, &target, false).await;
    }
    outcome
}

pub async fn control_step(state: &AppState, now: NaiveDateTime, now_ms: i64) -> Result<Vec<Desired>> {
    if state.failsafe.stopping() {
        return Ok(Vec::new());
    }
    for (device, transition) in state.tracker.lock().unwrap().poll(now_ms, state.config.away_guard_minutes) {
        log("presence event", state.sink.append(presence_event(&device, transition)));
    }
    let presence = current_presence(state, now_ms).unwrap_or_else(|e| {
        eprintln!("presence: {e}");
        None
    });
    let found = state.windows.lock().unwrap().step(&state.config, now_ms, |age_min| state.status.weather_within(lake::now_us(), (age_min * 60e6) as i64));
    found.into_iter().for_each(|e| window_event(state, e));
    let windows = state.windows.lock().unwrap().open_rooms();
    let window_busy = &windows;
    let away = presence.as_ref().is_some_and(|p| p.occupancy(&state.config) == Occupancy::Away);
    let exploring = {
        let held = |room: &str| state.config.rooms.iter().find(|r| r.name == room).is_some_and(|r| state.commanded.lock().unwrap().holding(&r.nodes));
        let blocked = |room: &str| if window_busy.contains(room) { Some("window_open") } else if away { Some("away") } else if held(room) { Some("manual_hold") } else { None };
        let base = |room: &Room| base_setpoint(room, &state.config, now);
        let mut explorer = state.explorer.lock().unwrap();
        let no_raise = |room: &str| !state.commanded.lock().unwrap().budget.allows(room, now_ms, state.config.raise_budget_per_hour);
        let lifecycle = explorer.tick(now, &Inputs { config: &state.config, blocked: &blocked, no_raise: &no_raise, base: &base });
        let active = explorer.active().clone();
        drop(explorer);
        lifecycle.into_iter().for_each(|life| lifecycle_event(state, life));
        active
    };
    let mut written = Vec::new();
    let hold_ms = (state.config.manual_hold_minutes * 60_000.0) as i64;
    let (detected, tap) = presence.as_ref().map_or((None, false), |p| (p.detected(&state.config), p.source == "stay_label"));
    let minutes = |m: f64| (m * 60_000.0) as i64;
    let step = state.commanded.lock().unwrap().occ.step(detected.as_deref(), tap, away, now_ms, minutes(state.config.occupancy_enter_min), minutes(state.config.occupancy_exit_min));
    for (room, from, to) in &step.ended {
        let event = lake::Event::now(kind::OCCUPANCY_END, "heatd").with_detail(json!({"room": room, "from_ms": from, "minutes": (to - from) as f64 / 60_000.0}));
        log("occupancy event", state.sink.append(lake::Event { ts: to * 1000, ..event }));
    }
    let readings = state.latest.all();
    let mut wants: Vec<Desired> = desired(now, &state.config, presence.as_ref(), &step.held, &windows, &exploring).into_iter().map(|w| Desired { setpoint: clamp_setpoint(&readings, w.node, w.setpoint, state.failsafe.max_c()), ..w }).collect();
    boost_step(state, &mut wants, &readings, now, now_ms, window_busy, away, &exploring);
    supply_step(state, &mut wants, &readings, now_ms, window_busy, &exploring);
    let contexts: Vec<(String, crate::window::RoomCtx)> = state.config.rooms.iter().map(|r| (r.name.clone(), crate::window::RoomCtx { hold: state.commanded.lock().unwrap().holding(&r.nodes), away })).collect();
    state.windows.lock().unwrap().set_context(contexts);
    let current = |commands: &Commands, node: u64| {
        commands.device.get(&node).copied().or(commands.target(node)).or_else(|| readings.iter().find(|r| r.node == node && r.attribute == "occupied_heating_setpoint").map(|r| r.value))
    };
    let fresh = state.commanded.lock().unwrap().occ.fresh_pending(&step.pending);
    if let Some((room, _)) = step.pending.as_ref().filter(|_| fresh) {
        for want in wants.iter().filter(|w| &w.room == room && w.reason != "window_open") {
            let old = current(&state.commanded.lock().unwrap(), want.node);
            let target = clamp_setpoint(&readings, want.node, state.config.occupied_setpoint, state.failsafe.max_c());
            if old.is_some_and(|o| (o - target).abs() < state.config.setpoint_deadband) {
                continue;
            }
            let wanted = Desired { setpoint: target, reason: "occupied", rule: "presence", ..want.clone() };
            log("decision", state.sink.append(deferred_row(state, &wanted, now_ms, old, &readings, presence.as_ref(), "debounce")));
        }
    }
    let interval_ms = minutes(state.config.min_write_interval_min);
    let grace_ms = GRACE_FACTOR * state.config.chip_tool.timeout_s as i64 * 1000;
    for want in wants {
        let (old, prior, verdict) = {
            let mut commands = state.commanded.lock().unwrap();
            let Some(hold_ended) = commands.release_due(&want, now_ms, hold_ms) else {
                continue;
            };
            let last = commands.target(want.node);
            let old = current(&commands, want.node);
            let drifted = commands.drifted(want.node, grace_ms);
            if (!drifted && last.is_some_and(|l| (l - want.setpoint).abs() < MIN_CHANGE_C)) || old.is_some_and(|o| (o - want.setpoint).abs() < state.config.setpoint_deadband) {
                commands.deferred.remove(&want.node);
                commands.budget.held.remove(&want.node);
                continue;
            }
            let last_write = commands.last_write.get(&want.node).copied();
            let urgent = |r: &str| matches!(r, "away" | "window_open" | "exploration" | "boost" | "boost_off" | "no_supply" | "pi_reset");
            let bypass = hold_ended || drifted || urgent(want.reason) || last_write.is_some_and(|(_, r)| urgent(r)) || (want.reason == "occupied" && want.source == "stay_label");
            if !bypass && last_write.is_some_and(|(t, _)| now_ms - t < interval_ms) {
                if commands.deferred.get(&want.node).is_none_or(|d| (d - want.setpoint).abs() >= DESIRED_EPS_C) {
                    commands.deferred.insert(want.node, want.setpoint);
                    drop(commands);
                    log("decision", state.sink.append(deferred_row(state, &want, now_ms, old, &readings, presence.as_ref(), "rate_limited")));
                }
                continue;
            }
            let verdict = raise_verdict(state, &commands, &want, old, last_write, now_ms);
            if verdict == Verdict::Deferred {
                if commands.budget.defer(&want.room, want.node, want.setpoint) {
                    drop(commands);
                    log("decision", state.sink.append(deferred_row(state, &want, now_ms, old, &readings, presence.as_ref(), "raise_budget")));
                }
                continue;
            }
            (old, commands.sent.get(&want.node).cloned(), verdict)
        };
        let mut decision = proposal(state, &want, now_ms, old, &readings, presence.as_ref());
        match verdict {
            Verdict::Exempt(why) => tag_budget(&mut decision, &format!("exempt_{why}")),
            Verdict::Raise => tag_budget(&mut decision, "raise"),
            _ => {}
        }
        log("decision", state.sink.append(decision.clone()));
        let sink = state.sink.clone();
        log("decision flush", tokio::task::spawn_blocking(move || sink.flush()).await.map_err(anyhow::Error::from).and_then(|r| r.map(|_| ())));
        state.commanded.lock().unwrap().sent.insert(want.node, Sent { target: want.setpoint, previous: old, written_ms: now_ms, acked: None, stale: None });
        let outcome = send_setpoint(state, want.node, want.setpoint).await;
        let error = outcome.as_ref().err().map(|e| e.to_string());
        state.status.setpoint_write(want.node, error.clone());
        log("decision result", state.sink.append(result_row(&decision, want.reason, error)));
        match outcome {
            Ok(()) => {
                {
                    let mut commands = state.commanded.lock().unwrap();
                    commands.last_write.insert(want.node, (now_ms, want.reason));
                    commands.deferred.remove(&want.node);
                    commands.budget.held.remove(&want.node);
                    commands.restored.insert(want.node);
                    if verdict == Verdict::Raise {
                        commands.budget.record(&want.room, now_ms);
                    }
                }
                if !matches!(want.reason, "window_open" | "no_supply" | "pi_reset") && old.is_some_and(|o| want.setpoint < o) {
                    state.windows.lock().unwrap().note_decrease(&want.room, now_ms);
                }
                log("setpoint row", state.storage.insert_setpoint(now_ms, want.node, &want.room, want.setpoint, want.reason));
                written.push(want);
            }
            Err(e) => {
                eprintln!("setpoint node {}: {e}", want.node);
                let mut commands = state.commanded.lock().unwrap();
                match prior {
                    Some(p) => commands.sent.insert(want.node, p),
                    None => commands.sent.remove(&want.node),
                };
            }
        }
    }
    Ok(written)
}

fn manual_change(state: &AppState, report: &AttributeReport, poll: bool, now_ms: i64, reconnected: bool) -> Option<f64> {
    let near = |a: f64, b: f64| (a - b).abs() < MANUAL_TOLERANCE_C;
    let grace_ms = GRACE_FACTOR * state.config.chip_tool.timeout_s as i64 * 1000;
    let mut commands = state.commanded.lock().unwrap();
    commands.device.insert(report.node, report.value);
    commands.device_ms.insert(report.node, now_ms);
    let Some(sent) = commands.sent.get_mut(&report.node) else {
        commands.sent.insert(report.node, Sent { target: report.value, previous: None, written_ms: 0, acked: None, stale: None });
        return None;
    };
    if near(report.value, sent.target) || [sent.acked, sent.stale].into_iter().flatten().any(|a| near(a, report.value)) {
        return None;
    }
    if now_ms - sent.written_ms < grace_ms {
        if !sent.previous.is_some_and(|p| near(p, report.value)) {
            sent.acked = Some(report.value);
        }
        return None;
    }
    if poll || (reconnected && near(report.value, state.config.br_watchdog.setpoint_c)) {
        return None;
    }
    let previous = sent.acked.unwrap_or(sent.target);
    commands.sent.insert(report.node, Sent { target: report.value, previous: None, written_ms: 0, acked: None, stale: None });
    commands.holds.insert(report.node, Hold { value: report.value, since_ms: now_ms });
    Some(previous)
}

pub fn on_report(state: &AppState, report: &AttributeReport, poll: bool, now_ms: i64) {
    state.windows.lock().unwrap().touch(&state.config, report.node, now_ms);
    if [crate::window::LOCAL_TEMPERATURE, crate::window::PI_HEATING_DEMAND, crate::window::OCCUPIED_SETPOINT].contains(&report.attribute.as_str()) {
        state.windows.lock().unwrap().observe(&state.config, report.node, &report.attribute, report.value, now_ms);
    }
    if [crate::window::LOCAL_TEMPERATURE, crate::window::PI_HEATING_DEMAND].contains(&report.attribute.as_str()) {
        state.boost.observe(report.node, &report.attribute, report.value, now_ms);
        return;
    }
    if report.attribute != OCCUPIED_HEATING_SETPOINT.snake {
        return;
    }
    let reconnected = state.windows.lock().unwrap().reconnected(report.node, now_ms, GRACE_FACTOR * state.config.chip_tool.timeout_s as i64 * 1000);
    let Some(previous) = manual_change(state, report, poll, now_ms, reconnected) else { return };
    let Some(room) = state.config.rooms.iter().find(|r| r.nodes.contains(&report.node)) else { return };
    if report.value < previous {
        state.windows.lock().unwrap().note_decrease(&room.name, now_ms);
    }
    let (aborted, running) = {
        let mut explorer = state.explorer.lock().unwrap();
        let running = explorer.active_in(&room.name).map(|e| e.id.clone());
        (explorer.abort(&room.name, "manual_override"), running)
    };
    aborted.into_iter().for_each(|life| lifecycle_event(state, life));
    log(
        "manual decision",
        state.sink.append(Decision {
            ts: now_ms * 1000,
            room: room.name.clone(),
            node: report.node,
            rule: "manual".into(),
            inputs: DecisionInputs { extra: Some(json!({"phase": "observed", "reason": "device_setpoint_change"}).to_string()), ..Default::default() },
            setpoint_old: Some(previous),
            setpoint_new: Some(report.value),
            experiment_id: running,
            applied: true,
            commit: lake::COMMIT.into(),
            ..Default::default()
        }),
    );
    log("override label", state.sink.append(Label { ts: now_ms * 1000, room: room.name.clone(), source: "override".into(), ..Default::default() }));
}

pub fn spawn_background(state: AppState) {
    let looper = state.clone();
    tokio::spawn(async move {
        let mut tick = tokio::time::interval(Duration::from_secs(60));
        loop {
            tick.tick().await;
            match control_step(&looper, Local::now().naive_local(), now_ms()).await {
                Ok(_) if !looper.failsafe.stopping() => looper.status.control_step_done(lake::now_us()),
                Ok(_) => {}
                Err(e) => eprintln!("control step: {e}"),
            }
        }
    });
}

#[cfg(test)]
mod tests {
    use super::*;
    use chrono::NaiveDate;

    const CONFIG: &str = "
default_setpoint = 18.0
away_setpoint = 18.0
occupied_setpoint = 21.0
sleep_room = 'schlafzimmer'
presence_timeout_minutes = 30
frost_setpoint = 7.0
[[rooms]]
name = 'wohnzimmer'
nodes = [5]
[[rooms.schedules]]
days = ['weekdays']
from = '06:00'
to = '08:30'
setpoint = 21.0
[[rooms.schedules]]
days = ['Sat']
from = '08:00'
to = '23:00'
setpoint = 22.0
[[rooms]]
name = 'schlafzimmer'
nodes = [6, 7]
[[rooms.schedules]]
days = ['Mon', 'Fri']
from = '22:00'
to = '06:00'
setpoint = 19.0
[watchmatch.state]
sleep_hours = '3-3'
";

    fn at(day: u32, h: u32, m: u32) -> NaiveDateTime {
        NaiveDate::from_ymd_opt(2025, 10, day).unwrap().and_hms_opt(h, m, 0).unwrap()
    }

    fn presence(room: &str, age: f64) -> Option<Presence> {
        Some(Presence { room: Some(room.into()), prediction_age_minutes: Some(age), motion: None, power: None, away: false, contact_age_minutes: None, source: "matcher", sleep: false })
    }

    fn device(room: &str, age: f64, motion: Option<Motion>, power: Option<Power>, away: bool, contact: f64) -> Option<Presence> {
        Some(Presence { room: Some(room.into()), prediction_age_minutes: Some(age), motion, power, away, contact_age_minutes: Some(contact), source: "matcher", sleep: false })
    }

    fn raw(now: NaiveDateTime, config: &Config, p: Option<&Presence>, windows: &HashSet<String>, exploring: &HashMap<String, Experiment>) -> Vec<Desired> {
        let held = p.and_then(|p| p.detected(config)).into_iter().collect();
        desired(now, config, p, &held, windows, exploring)
    }

    fn lookup(now: NaiveDateTime, p: Option<Presence>, windows: &[&str], node: u64) -> (f64, &'static str) {
        let config = Config::parse(CONFIG).unwrap();
        let windows = windows.iter().map(|s| s.to_string()).collect();
        let d = raw(now, &config, p.as_ref(), &windows, &HashMap::new()).into_iter().find(|d| d.node == node).unwrap();
        (d.setpoint, d.reason)
    }

    #[test]
    fn home_setpoint_applies_while_home_except_away_and_sleeping_bedroom() {
        let config = Config::parse("default_setpoint = 18.0
away_setpoint = 18.0
occupied_setpoint = 20.0
sleep_room = \"schlafzimmer\"
[[rooms]]
name = \"wohnzimmer\"
nodes = [5]
home_setpoint = 20.0
[[rooms]]
name = \"schlafzimmer\"
nodes = [6]
home_setpoint = 20.0
[[rooms]]
name = \"kueche\"
nodes = [8]
").unwrap();
        let get = |p: Option<Presence>, node: u64| raw(at(27, 15, 0), &config, p.as_ref(), &HashSet::new(), &HashMap::new()).into_iter().find(|d| d.node == node).map(|d| (d.setpoint, d.reason)).unwrap();
        let home = || presence("flur", 1.0);
        assert_eq!(get(home(), 5), (20.0, "home"));
        assert_eq!(get(home(), 8), (18.0, "default"));
        let away = Some(Presence { away: true, ..home().unwrap() });
        assert_eq!(get(away, 5).0, 18.0);
        let sleeping = Some(Presence { sleep: true, ..home().unwrap() });
        assert_eq!(get(sleeping.clone(), 6), (18.0, "sleep"));
        assert_eq!(get(sleeping, 5), (20.0, "home"));
    }

    #[test]
    fn sleep_hours_fallback_keeps_the_bedroom_at_default_without_a_sleep_report() {
        let config = Config::parse("default_setpoint = 18.0
occupied_setpoint = 20.0
sleep_room = \"schlafzimmer\"
[[rooms]]
name = \"wohnzimmer\"
nodes = [5]
home_setpoint = 20.0
[[rooms]]
name = \"schlafzimmer\"
nodes = [6]
home_setpoint = 20.0
").unwrap();
        let get = |now: NaiveDateTime, p: Option<Presence>, node: u64| raw(now, &config, p.as_ref(), &HashSet::new(), &HashMap::new()).into_iter().find(|d| d.node == node).map(|d| (d.setpoint, d.reason)).unwrap();
        let awake = || presence("flur", 1.0);
        assert_eq!(get(at(30, 23, 45), awake(), 6), (18.0, "sleep"));
        assert_eq!(get(at(31, 3, 0), None, 6), (18.0, "sleep"));
        assert_eq!(get(at(31, 6, 59), awake(), 6), (18.0, "sleep"));
        assert_eq!(get(at(31, 3, 0), presence("schlafzimmer", 1.0), 6), (18.0, "sleep"));
        assert_eq!(get(at(31, 3, 0), awake(), 5), (20.0, "home"));
        assert_eq!(get(at(31, 7, 0), awake(), 6), (20.0, "home"));
        assert_eq!(get(at(30, 22, 59), awake(), 6), (20.0, "home"));
        assert_eq!(get(at(30, 12, 0), presence("schlafzimmer", 1.0), 6), (20.0, "occupied"));
    }

    #[test]
    fn table() {
        let cases = vec![
            ("monday morning schedule", at(27, 7, 0), None, vec![], 5, (21.0, "schedule")),
            ("start inclusive", at(27, 6, 0), None, vec![], 5, (21.0, "schedule")),
            ("end exclusive", at(27, 8, 30), None, vec![], 5, (18.0, "default")),
            ("outside schedule", at(27, 12, 0), None, vec![], 5, (18.0, "default")),
            ("saturday only own schedule", at(25, 7, 0), None, vec![], 5, (18.0, "default")),
            ("saturday day", at(25, 9, 0), None, vec![], 5, (22.0, "schedule")),
            ("midnight range same evening", at(27, 23, 0), None, vec![], 6, (19.0, "schedule")),
            ("midnight range next morning", at(28, 5, 59), None, vec![], 6, (19.0, "schedule")),
            ("midnight range ends", at(28, 6, 0), None, vec![], 6, (18.0, "default")),
            ("tuesday evening not scheduled", at(28, 23, 0), None, vec![], 6, (18.0, "default")),
            ("friday night into saturday", at(25, 1, 0), None, vec![], 6, (19.0, "schedule")),
            ("saturday night not scheduled", at(26, 1, 0), None, vec![], 6, (18.0, "default")),
            ("fresh presence sets the occupied room to 21", at(27, 7, 0), presence("wohnzimmer", 29.9), vec![], 5, (21.0, "occupied")),
            ("fresh presence other room unchanged", at(27, 7, 0), presence("bad", 5.0), vec![], 5, (21.0, "schedule")),
            ("occupied applies to every node of the room", at(27, 23, 0), presence("schlafzimmer", 1.0), vec![], 7, (21.0, "occupied")),
            ("stale presence is unknown not eco", at(27, 7, 0), presence("wohnzimmer", 30.1), vec![], 5, (21.0, "schedule")),
            ("stale presence other room schedule", at(27, 12, 0), presence("bad", 99.0), vec![], 6, (18.0, "default")),
            ("still keeps old room", at(27, 7, 0), device("wohnzimmer", 120.0, Some(Motion::Still), None, false, 100.0), vec![], 5, (21.0, "occupied")),
            ("still at contact timeout boundary", at(27, 7, 0), device("wohnzimmer", 500.0, Some(Motion::Still), Some(Power::Doze), false, 180.0), vec![], 5, (21.0, "occupied")),
            ("still but contact stale", at(27, 7, 0), device("wohnzimmer", 500.0, Some(Motion::Still), None, false, 180.1), vec![], 5, (21.0, "schedule")),
            ("moving with stale prediction unknown", at(27, 7, 0), device("wohnzimmer", 31.0, Some(Motion::Moving), None, false, 1.0), vec![], 5, (21.0, "schedule")),
            ("moving with fresh prediction occupies", at(27, 7, 0), device("wohnzimmer", 2.0, Some(Motion::Moving), None, false, 1.0), vec![], 5, (21.0, "occupied")),
            ("saver is unknown", at(27, 7, 0), device("wohnzimmer", 1.0, Some(Motion::Still), Some(Power::Saver), false, 1.0), vec![], 5, (21.0, "schedule")),
            ("offbody is unknown", at(27, 7, 0), device("wohnzimmer", 1.0, Some(Motion::Still), Some(Power::Offbody), false, 1.0), vec![], 5, (21.0, "schedule")),
            ("away goes eco", at(27, 12, 0), device("wohnzimmer", 1.0, Some(Motion::Moving), None, true, 1.0), vec![], 5, (18.0, "away")),
            ("away with saver is unknown", at(27, 12, 0), device("wohnzimmer", 1.0, None, Some(Power::Saver), true, 1.0), vec![], 5, (18.0, "default")),
            ("away but contact stale is unknown", at(27, 12, 0), device("wohnzimmer", 1.0, None, None, true, 181.0), vec![], 5, (18.0, "default")),
            ("away without prediction", at(27, 12, 0), Some(Presence { room: None, prediction_age_minutes: None, motion: None, power: None, away: true, contact_age_minutes: Some(1.0), source: "matcher", sleep: false }), vec![], 5, (18.0, "away")),
            ("heartbeat without prediction is unknown", at(27, 12, 0), Some(Presence { room: None, prediction_age_minutes: None, motion: Some(Motion::Still), power: None, away: false, contact_age_minutes: Some(1.0), source: "matcher", sleep: false }), vec![], 5, (18.0, "default")),
            ("window open frost", at(27, 7, 0), presence("wohnzimmer", 1.0), vec!["wohnzimmer"], 5, (7.0, "window_open")),
            ("window open other room unaffected", at(27, 7, 0), None, vec!["wohnzimmer"], 6, (18.0, "default")),
        ];
        for (name, now, p, windows, node, expected) in cases {
            assert_eq!(lookup(now, p, &windows, node), expected, "{name}");
        }
    }

    #[test]
    fn occupied_room_gets_21_and_bedroom_keeps_night_value_while_asleep() {
        let config = Config::parse(CONFIG).unwrap();
        let go = |p: Option<Presence>, node: u64| {
            let d = raw(at(27, 23, 0), &config, p.as_ref(), &HashSet::new(), &HashMap::new()).into_iter().find(|d| d.node == node).unwrap();
            (d.setpoint, d.reason, d.rule, d.source, d.sleep)
        };
        let asleep = |room: &str| presence(room, 1.0).map(|p| Presence { sleep: true, ..p });
        assert_eq!(go(presence("wohnzimmer", 1.0), 5), (21.0, "occupied", "presence", "matcher", false));
        assert_eq!(go(presence("wohnzimmer", 1.0), 6), (19.0, "schedule", "schedule", "matcher", false));
        assert_eq!(go(asleep("schlafzimmer"), 6), (19.0, "sleep", "presence", "matcher", true));
        assert_eq!(go(asleep("schlafzimmer"), 5), (18.0, "default", "schedule", "matcher", true));
        assert_eq!(go(asleep("wohnzimmer"), 5), (21.0, "occupied", "presence", "matcher", true));
        assert_eq!(go(presence("schlafzimmer", 1.0), 6), (21.0, "occupied", "presence", "matcher", false));
        let label = presence("wohnzimmer", 2.0).map(|p| Presence { source: "stay_label", ..p });
        assert_eq!(go(label, 5), (21.0, "occupied", "presence", "stay_label", false));
        let away = device("schlafzimmer", 1.0, None, None, true, 1.0).map(|p| Presence { sleep: true, ..p });
        assert_eq!(go(away, 6).1, "away");
    }

    #[test]
    fn walking_through_rooms_never_occupies_but_a_ten_minute_stay_does_and_leaving_holds() {
        let (min, enter, exit) = (60_000, 10 * 60_000, 45 * 60_000);
        let mut occ = Occ::default();
        let mut now = 0;
        let mut tick = |occ: &mut Occ, room: Option<&str>, tap: bool, away: bool, minutes: i64| {
            let mut held = HashSet::new();
            for _ in 0..minutes {
                held = occ.step(room, tap, away, now, enter, exit).held;
                now += min;
            }
            held
        };
        for room in ["kueche", "bad", "flur", "kueche", "bad"] {
            assert!(tick(&mut occ, Some(room), false, false, 5).is_empty());
        }
        assert!(tick(&mut occ, Some("wohnzimmer"), false, false, 10).is_empty());
        assert_eq!(tick(&mut occ, Some("wohnzimmer"), false, false, 1), HashSet::from(["wohnzimmer".to_string()]));
        assert_eq!(tick(&mut occ, Some("kueche"), false, false, 5), HashSet::from(["wohnzimmer".to_string()]));
        assert_eq!(tick(&mut occ, None, false, false, 39), HashSet::from(["wohnzimmer".to_string()]));
        assert!(tick(&mut occ, None, false, false, 1).is_empty());
        assert_eq!(tick(&mut occ, Some("bad"), true, false, 1), HashSet::from(["bad".to_string()]));
        assert!(tick(&mut occ, Some("bad"), true, true, 1).is_empty());
    }

    #[test]
    fn away_overrides_schedule_and_can_be_configured() {
        let config = Config::parse("away_setpoint = 17.5
[[rooms]]
name='a'
nodes=[1]
[[rooms.schedules]]
days=['weekdays']
from='06:00'
to='08:30'
setpoint=21.0").unwrap();
        let p = device("a", 500.0, None, None, true, 5.0);
        let d = raw(at(27, 7, 0), &config, p.as_ref(), &HashSet::new(), &HashMap::new());
        assert_eq!((d[0].setpoint, d[0].reason, d[0].rule, d[0].occupancy), (17.5, "away", "presence", "away"));
        assert_eq!(Config::parse("").unwrap().away_setpoint, 18.0);
    }

    #[test]
    fn exploration_overrides_schedule_but_not_window_or_away() {
        let config = Config::parse(CONFIG).unwrap();
        let experiment = Experiment { id: "e1".into(), room: "wohnzimmer".into(), kind: crate::explore::Kind::Step, setpoint: 3.0, base: 21.0, start: at(27, 7, 0), end: at(27, 8, 0) };
        let exploring = HashMap::from([("wohnzimmer".to_string(), experiment)]);
        let go = |p: Option<Presence>, windows: &[&str]| {
            let windows = windows.iter().map(|s| s.to_string()).collect();
            let d = raw(at(27, 7, 0), &config, p.as_ref(), &windows, &exploring).into_iter().find(|d| d.node == 5).unwrap();
            (d.setpoint, d.reason, d.rule, d.experiment_id)
        };
        assert_eq!(go(None, &[]), (7.0, "exploration", "exploration", Some("e1".to_string())));
        assert_eq!(go(None, &["wohnzimmer"]), (7.0, "window_open", "window", None));
        assert_eq!(go(device("wohnzimmer", 1.0, Some(Motion::Moving), None, true, 1.0), &[]), (18.0, "away", "presence", None));
    }

    #[test]
    fn setpoints_are_clamped_to_device_or_safe_limits() {
        let reading = |node, attribute: &str, value| Reading { node, attribute: attribute.into(), value, ts: 0 };
        assert_eq!(setpoint_limits(&[], 5), (7.5, 28.5));
        let limits = [reading(5, "min_heat_setpoint_limit", 10.0), reading(5, "max_heat_setpoint_limit", 25.0), reading(6, "min_heat_setpoint_limit", 30.0)];
        assert_eq!(setpoint_limits(&limits, 5), (10.0, 25.0));
        assert_eq!(setpoint_limits(&limits, 6), (7.5, 28.5));
        assert_eq!(setpoint_limits(&limits, 7), (7.5, 28.5));
        assert_eq!([7.0, 8.0, 40.0].map(|v| clamp_setpoint(&[], 5, v, 28.5)), [7.5, 8.0, 28.5]);
        assert_eq!([7.0, 8.0, 40.0].map(|v| clamp_setpoint(&[], 5, v, 22.0)), [7.5, 8.0, 22.0]);
        assert_eq!(clamp_setpoint(&limits, 5, 8.0, 22.0), 10.0);
        assert_eq!(clamp_boost(&[], 5, 28.0, 28.5), 28.0);
    }

    #[test]
    fn away_needs_guard_and_carried_watch() {
        let mut t = Tracker::default();
        let min = 60_000;
        assert_eq!(t.observe("w", 0, true, Some(Motion::Still), Some(Power::Doze)), None);
        assert!(t.poll(60 * min, 20.0).is_empty());
        assert!(!t.confirmed("w"));
        t.observe("w", 61 * min, true, Some(Motion::Moving), None);
        assert!(t.poll(10 * min + 61 * min, 20.0).len() == 1 && t.confirmed("w"));
        assert_eq!(t.observe("w", 100 * min, false, None, None), Some(Transition::Arrived));
        assert!(!t.confirmed("w") && t.observe("w", 101 * min, false, None, None).is_none());
        t.observe("w", 200 * min, true, Some(Motion::Moving), None);
        assert!(t.poll(210 * min, 20.0).is_empty());
        assert_eq!(t.poll(220 * min, 20.0), vec![("w".to_string(), Transition::Left { since: 200 * min })]);
        assert_eq!(t.observe("w", 150 * min, false, None, None), None);
        assert!(t.confirmed("w"));
    }
}
