use crate::config::{Config, Room};
use crate::http::AppState;
use crate::storage::now_ms;
use anyhow::Result;
use chrono::{Datelike, Local, NaiveDateTime, Timelike};
use std::collections::HashSet;
use std::time::Duration;

pub const MIN_CHANGE_C: f64 = 0.5;

#[derive(Debug, Clone, PartialEq)]
pub struct Presence {
    pub room: String,
    pub age_minutes: f64,
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
    config
        .rooms
        .iter()
        .flat_map(|room| {
            let (base, base_reason) = scheduled(room, now).map_or((config.default_setpoint, "default"), |s| (s, "schedule"));
            let (setpoint, reason) = if window_open.contains(&room.name) {
                (config.frost_setpoint, "window_open")
            } else {
                match presence {
                    None => (base, base_reason),
                    Some(p) if p.age_minutes <= config.presence_timeout_minutes => {
                        if p.room == room.name {
                            (base + config.presence_boost, "presence_boost")
                        } else {
                            (base, base_reason)
                        }
                    }
                    Some(_) if config.eco_setpoint < base => (config.eco_setpoint, "away_eco"),
                    Some(_) => (base, base_reason),
                }
            };
            room.nodes.iter().map(move |&node| Desired { node, room: room.name.clone(), setpoint, reason })
        })
        .collect()
}

pub async fn control_step(state: &AppState, now: NaiveDateTime, now_ms: i64) -> Result<Vec<Desired>> {
    let presence = state.storage.latest_presence()?.map(|(room, ts)| Presence { room, age_minutes: (now_ms - ts) as f64 / 60_000.0 });
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
    for node in state.config.all_nodes() {
        let state = state.clone();
        tokio::spawn(async move {
            loop {
                match state.controller.subscribe(node).await {
                    Ok(mut rx) => {
                        while let Some(report) = rx.recv().await {
                            if let Err(e) = state.storage.insert_reading(now_ms(), report.node, report.endpoint, &report.attribute, report.value) {
                                eprintln!("store reading: {e}");
                            }
                        }
                    }
                    Err(e) => eprintln!("subscribe node {node}: {e}"),
                }
                tokio::time::sleep(Duration::from_secs(10)).await;
            }
        });
    }
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
        Some(Presence { room: room.into(), age_minutes: age })
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
            ("stale presence goes eco", at(27, 7, 0), presence("wohnzimmer", 30.1), vec![], 5, (16.0, "away_eco")),
            ("eco never raises", at(27, 12, 0), presence("bad", 99.0), vec![], 5, (16.0, "away_eco")),
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
        let d = desired(at(27, 12, 0), &config, presence("a", 500.0).as_ref(), &HashSet::new());
        assert_eq!((d[0].setpoint, d[0].reason), (18.0, "default"));
    }
}
