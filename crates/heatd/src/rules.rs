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

#[derive(Debug, Clone, PartialEq)]
pub struct Presence {
    pub room: Option<String>,
    pub prediction_age_minutes: Option<f64>,
    pub motion: Option<Motion>,
    pub power: Option<Power>,
    pub away: bool,
    pub contact_age_minutes: Option<f64>,
}

#[derive(Debug, Clone, PartialEq)]
pub enum Occupancy<'a> {
    Room(&'a str),
    Away,
    Unknown,
}

impl Presence {
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

pub fn desired(now: NaiveDateTime, config: &Config, presence: Option<&Presence>, window_open: &HashSet<String>, exploring: &HashMap<String, Experiment>) -> Vec<Desired> {
    let occupancy = presence.map_or(Occupancy::Unknown, |p| p.occupancy(config));
    config
        .rooms
        .iter()
        .flat_map(|room| {
            let (base, base_reason) = scheduled(room, now).map_or((config.default_setpoint, "default"), |s| (s, "schedule"));
            let window = window_open.contains(&room.name);
            let experiment = exploring.get(&room.name);
            let (setpoint, reason, rule) = if window {
                (config.frost_setpoint, "window_open", "window")
            } else if occupancy == Occupancy::Away {
                (config.away_setpoint, "away", "presence")
            } else if let Some(e) = experiment {
                (e.setpoint.max(config.frost_setpoint), "exploration", "exploration")
            } else {
                match occupancy {
                    Occupancy::Room(r) if r == room.name => (base + config.presence_boost, "presence_boost", "presence"),
                    _ => (base, base_reason, "schedule"),
                }
            };
            let label = match occupancy {
                Occupancy::Room(_) => "room",
                Occupancy::Away => "away",
                Occupancy::Unknown => "unknown",
            };
            let id = experiment.filter(|_| reason == "exploration").map(|e| e.id.clone());
            room.nodes.iter().map(move |&node| Desired { node, room: room.name.clone(), setpoint, reason, rule, base, occupancy: label, window_open: window, experiment_id: id.clone() })
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

fn current_presence(state: &AppState, now_ms: i64) -> Result<Option<Presence>> {
    let minutes = |ms: i64| (now_ms - ms) as f64 / 60_000.0;
    let prediction = state.storage.last_predictions()?.into_iter().max_by_key(|p| p.ts);
    let statuses = state.storage.device_statuses()?;
    let status = match &prediction {
        Some(p) => statuses.into_iter().find(|s| s.device == p.device),
        None => statuses.into_iter().max_by_key(|s| s.state_ts),
    };
    if prediction.is_none() && status.is_none() {
        return Ok(None);
    }
    let away = status.as_ref().is_some_and(|s| s.away && state.tracker.lock().unwrap().confirmed(&s.device));
    Ok(Some(Presence {
        prediction_age_minutes: prediction.as_ref().map(|p| minutes(p.ts)),
        room: prediction.map(|p| p.room),
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
    event.node = state.config.rooms.iter().find(|r| r.name == e.room).and_then(|r| r.nodes.first().copied());
    log("experiment event", state.sink.append(event));
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
            extra: Some(json!({"phase": "proposal", "reason": want.reason}).to_string()),
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

pub async fn control_step(state: &AppState, now: NaiveDateTime, now_ms: i64) -> Result<Vec<Desired>> {
    for (device, transition) in state.tracker.lock().unwrap().poll(now_ms, state.config.away_guard_minutes) {
        log("presence event", state.sink.append(presence_event(&device, transition)));
    }
    let presence = current_presence(state, now_ms).unwrap_or_else(|e| {
        eprintln!("presence: {e}");
        None
    });
    let windows = state.windows.lock().unwrap().clone();
    let away = presence.as_ref().is_some_and(|p| p.occupancy(&state.config) == Occupancy::Away);
    let exploring = {
        let blocked = |room: &str| if windows.contains(room) { Some("window_open") } else if away { Some("away") } else { None };
        let base = |room: &Room| base_setpoint(room, &state.config, now);
        let mut explorer = state.explorer.lock().unwrap();
        let lifecycle = explorer.tick(now, &Inputs { config: &state.config, blocked: &blocked, base: &base });
        let active = explorer.active().clone();
        drop(explorer);
        lifecycle.into_iter().for_each(|life| lifecycle_event(state, life));
        active
    };
    let readings = state.latest.all();
    let mut written = Vec::new();
    for want in desired(now, &state.config, presence.as_ref(), &windows, &exploring) {
        let last = state.commanded.lock().unwrap().get(&want.node).copied();
        if last.is_some_and(|l| (l - want.setpoint).abs() < MIN_CHANGE_C) {
            continue;
        }
        let old = last.or_else(|| readings.iter().find(|r| r.node == want.node && r.attribute == "occupied_heating_setpoint").map(|r| r.value));
        let decision = proposal(state, &want, now_ms, old, &readings, presence.as_ref());
        log("decision", state.sink.append(decision.clone()));
        state.commanded.lock().unwrap().insert(want.node, want.setpoint);
        let outcome = state.controller.write_heating_setpoint(want.node, want.setpoint).await;
        let error = outcome.as_ref().err().map(|e| e.to_string());
        log("decision result", state.sink.append(result_row(&decision, want.reason, error)));
        match outcome {
            Ok(()) => {
                log("setpoint row", state.storage.insert_setpoint(now_ms, want.node, &want.room, want.setpoint, want.reason));
                written.push(want);
            }
            Err(e) => {
                eprintln!("setpoint node {}: {e}", want.node);
                let mut commanded = state.commanded.lock().unwrap();
                match last {
                    Some(l) => commanded.insert(want.node, l),
                    None => commanded.remove(&want.node),
                };
            }
        }
    }
    Ok(written)
}

pub fn on_report(state: &AppState, report: &AttributeReport, now_ms: i64) {
    if report.attribute != OCCUPIED_HEATING_SETPOINT.snake {
        return;
    }
    let previous = state.commanded.lock().unwrap().insert(report.node, report.value);
    let Some(previous) = previous.filter(|p| (p - report.value).abs() >= MANUAL_TOLERANCE_C) else { return };
    let Some(room) = state.config.rooms.iter().find(|r| r.nodes.contains(&report.node)) else { return };
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
            if let Err(e) = control_step(&looper, Local::now().naive_local(), now_ms()).await {
                eprintln!("control step: {e}");
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
presence_boost = 1.0
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
";

    fn at(day: u32, h: u32, m: u32) -> NaiveDateTime {
        NaiveDate::from_ymd_opt(2025, 10, day).unwrap().and_hms_opt(h, m, 0).unwrap()
    }

    fn presence(room: &str, age: f64) -> Option<Presence> {
        Some(Presence { room: Some(room.into()), prediction_age_minutes: Some(age), motion: None, power: None, away: false, contact_age_minutes: None })
    }

    fn device(room: &str, age: f64, motion: Option<Motion>, power: Option<Power>, away: bool, contact: f64) -> Option<Presence> {
        Some(Presence { room: Some(room.into()), prediction_age_minutes: Some(age), motion, power, away, contact_age_minutes: Some(contact) })
    }

    fn lookup(now: NaiveDateTime, p: Option<Presence>, windows: &[&str], node: u64) -> (f64, &'static str) {
        let config = Config::parse(CONFIG).unwrap();
        let windows = windows.iter().map(|s| s.to_string()).collect();
        let d = desired(now, &config, p.as_ref(), &windows, &HashMap::new()).into_iter().find(|d| d.node == node).unwrap();
        (d.setpoint, d.reason)
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
            ("fresh presence boosts occupied room", at(27, 7, 0), presence("wohnzimmer", 29.9), vec![], 5, (22.0, "presence_boost")),
            ("fresh presence other room unchanged", at(27, 7, 0), presence("bad", 5.0), vec![], 5, (21.0, "schedule")),
            ("boost applies to every node of the room", at(27, 23, 0), presence("schlafzimmer", 1.0), vec![], 7, (20.0, "presence_boost")),
            ("stale presence is unknown not eco", at(27, 7, 0), presence("wohnzimmer", 30.1), vec![], 5, (21.0, "schedule")),
            ("stale presence other room schedule", at(27, 12, 0), presence("bad", 99.0), vec![], 6, (18.0, "default")),
            ("still keeps old room", at(27, 7, 0), device("wohnzimmer", 120.0, Some(Motion::Still), None, false, 100.0), vec![], 5, (22.0, "presence_boost")),
            ("still at contact timeout boundary", at(27, 7, 0), device("wohnzimmer", 500.0, Some(Motion::Still), Some(Power::Doze), false, 180.0), vec![], 5, (22.0, "presence_boost")),
            ("still but contact stale", at(27, 7, 0), device("wohnzimmer", 500.0, Some(Motion::Still), None, false, 180.1), vec![], 5, (21.0, "schedule")),
            ("moving with stale prediction unknown", at(27, 7, 0), device("wohnzimmer", 31.0, Some(Motion::Moving), None, false, 1.0), vec![], 5, (21.0, "schedule")),
            ("moving with fresh prediction boosts", at(27, 7, 0), device("wohnzimmer", 2.0, Some(Motion::Moving), None, false, 1.0), vec![], 5, (22.0, "presence_boost")),
            ("saver is unknown", at(27, 7, 0), device("wohnzimmer", 1.0, Some(Motion::Still), Some(Power::Saver), false, 1.0), vec![], 5, (21.0, "schedule")),
            ("offbody is unknown", at(27, 7, 0), device("wohnzimmer", 1.0, Some(Motion::Still), Some(Power::Offbody), false, 1.0), vec![], 5, (21.0, "schedule")),
            ("away goes eco", at(27, 12, 0), device("wohnzimmer", 1.0, Some(Motion::Moving), None, true, 1.0), vec![], 5, (18.0, "away")),
            ("away with saver is unknown", at(27, 12, 0), device("wohnzimmer", 1.0, None, Some(Power::Saver), true, 1.0), vec![], 5, (18.0, "default")),
            ("away but contact stale is unknown", at(27, 12, 0), device("wohnzimmer", 1.0, None, None, true, 181.0), vec![], 5, (18.0, "default")),
            ("away without prediction", at(27, 12, 0), Some(Presence { room: None, prediction_age_minutes: None, motion: None, power: None, away: true, contact_age_minutes: Some(1.0) }), vec![], 5, (18.0, "away")),
            ("heartbeat without prediction is unknown", at(27, 12, 0), Some(Presence { room: None, prediction_age_minutes: None, motion: Some(Motion::Still), power: None, away: false, contact_age_minutes: Some(1.0) }), vec![], 5, (18.0, "default")),
            ("window open frost", at(27, 7, 0), presence("wohnzimmer", 1.0), vec!["wohnzimmer"], 5, (7.0, "window_open")),
            ("window open other room unaffected", at(27, 7, 0), None, vec!["wohnzimmer"], 6, (18.0, "default")),
        ];
        for (name, now, p, windows, node, expected) in cases {
            assert_eq!(lookup(now, p, &windows, node), expected, "{name}");
        }
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
        let d = desired(at(27, 7, 0), &config, p.as_ref(), &HashSet::new(), &HashMap::new());
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
            let d = desired(at(27, 7, 0), &config, p.as_ref(), &windows, &exploring).into_iter().find(|d| d.node == 5).unwrap();
            (d.setpoint, d.reason, d.rule, d.experiment_id)
        };
        assert_eq!(go(None, &[]), (7.0, "exploration", "exploration", Some("e1".to_string())));
        assert_eq!(go(None, &["wohnzimmer"]), (7.0, "window_open", "window", None));
        assert_eq!(go(device("wohnzimmer", 1.0, Some(Motion::Moving), None, true, 1.0), &[]), (18.0, "away", "presence", None));
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
