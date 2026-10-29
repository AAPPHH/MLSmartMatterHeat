use crate::config::{Config, Room};
use crate::http::AppState;
use crate::storage::{now_ms, Motion, Power};
use anyhow::Result;
use chrono::{Datelike, Local, NaiveDateTime, Timelike};
use std::collections::HashSet;
use std::time::Duration;

pub const MIN_CHANGE_C: f64 = 0.5;

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

pub fn desired(now: NaiveDateTime, config: &Config, presence: Option<&Presence>, window_open: &HashSet<String>) -> Vec<Desired> {
    let occupancy = presence.map_or(Occupancy::Unknown, |p| p.occupancy(config));
    config
        .rooms
        .iter()
        .flat_map(|room| {
            let (base, base_reason) = scheduled(room, now).map_or((config.default_setpoint, "default"), |s| (s, "schedule"));
            let (setpoint, reason) = if window_open.contains(&room.name) {
                (config.frost_setpoint, "window_open")
            } else {
                match occupancy {
                    Occupancy::Room(r) if r == room.name => (base + config.presence_boost, "presence_boost"),
                    Occupancy::Away if config.eco_setpoint < base => (config.eco_setpoint, "away_eco"),
                    _ => (base, base_reason),
                }
            };
            room.nodes.iter().map(move |&node| Desired { node, room: room.name.clone(), setpoint, reason })
        })
        .collect()
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
    Ok(Some(Presence {
        prediction_age_minutes: prediction.as_ref().map(|p| minutes(p.ts)),
        room: prediction.map(|p| p.room),
        motion: status.as_ref().and_then(|s| s.motion),
        power: status.as_ref().and_then(|s| s.power),
        away: status.as_ref().is_some_and(|s| s.away),
        contact_age_minutes: status.as_ref().map(|s| minutes(s.contact_ms)),
    }))
}

pub async fn control_step(state: &AppState, now: NaiveDateTime, now_ms: i64) -> Result<Vec<Desired>> {
    let presence = current_presence(state, now_ms)?;
    let windows = state.windows.lock().unwrap().clone();
    let mut written = Vec::new();
    for want in desired(now, &state.config, presence.as_ref(), &windows) {
        let last = state.commanded.lock().unwrap().get(&want.node).copied();
        if last.is_some_and(|l| (l - want.setpoint).abs() < MIN_CHANGE_C) {
            continue;
        }
        match state.controller.write_heating_setpoint(want.node, want.setpoint).await {
            Ok(()) => {
                state.commanded.lock().unwrap().insert(want.node, want.setpoint);
                state.storage.insert_setpoint(now_ms, want.node, &want.room, want.setpoint, want.reason)?;
                written.push(want);
            }
            Err(e) => eprintln!("setpoint node {}: {e}", want.node),
        }
    }
    Ok(written)
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
eco_setpoint = 16.0
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
        let d = desired(now, &config, p.as_ref(), &windows).into_iter().find(|d| d.node == node).unwrap();
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
            ("away goes eco", at(27, 12, 0), device("wohnzimmer", 1.0, Some(Motion::Moving), None, true, 1.0), vec![], 5, (16.0, "away_eco")),
            ("away with saver is unknown", at(27, 12, 0), device("wohnzimmer", 1.0, None, Some(Power::Saver), true, 1.0), vec![], 5, (18.0, "default")),
            ("away but contact stale is unknown", at(27, 12, 0), device("wohnzimmer", 1.0, None, None, true, 181.0), vec![], 5, (18.0, "default")),
            ("away without prediction", at(27, 12, 0), Some(Presence { room: None, prediction_age_minutes: None, motion: None, power: None, away: true, contact_age_minutes: Some(1.0) }), vec![], 5, (16.0, "away_eco")),
            ("heartbeat without prediction is unknown", at(27, 12, 0), Some(Presence { room: None, prediction_age_minutes: None, motion: Some(Motion::Still), power: None, away: false, contact_age_minutes: Some(1.0) }), vec![], 5, (18.0, "default")),
            ("window open frost", at(27, 7, 0), presence("wohnzimmer", 1.0), vec!["wohnzimmer"], 5, (7.0, "window_open")),
            ("window open other room unaffected", at(27, 7, 0), None, vec!["wohnzimmer"], 6, (18.0, "default")),
        ];
        for (name, now, p, windows, node, expected) in cases {
            assert_eq!(lookup(now, p, &windows, node), expected, "{name}");
        }
    }

    #[test]
    fn eco_not_applied_above_base() {
        let config = Config::parse("eco_setpoint = 19.0\ndefault_setpoint = 18.0\n[[rooms]]\nname='a'\nnodes=[1]").unwrap();
        let d = desired(at(27, 12, 0), &config, device("a", 500.0, None, None, true, 5.0).as_ref(), &HashSet::new());
        assert_eq!((d[0].setpoint, d[0].reason), (18.0, "default"));
    }
}
