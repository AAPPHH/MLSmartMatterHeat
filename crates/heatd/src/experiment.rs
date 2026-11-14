use crate::config::Room;
use crate::explore::Rng;
use crate::http::AppState;
use crate::intent::Reply;
use crate::lake::{kind, Event, Table};
use crate::report::{hhmm, midnight_us, open_lake, rows};
use crate::rules::{clamp_boost, clamp_setpoint, in_sleep_hours, Desired, Occupancy, Presence};
use anyhow::Result;
use axum::http::StatusCode;
use chrono::{Datelike, NaiveDate, NaiveDateTime, Timelike};
use serde_json::{json, Map, Value};
use std::collections::{BTreeMap, HashMap, HashSet};
use std::fmt::Write;
use std::path::Path;

pub const COOLDOWN_MIN: i64 = 75;
pub const TOTAL_MIN: i64 = 225;
pub const EXPIRY_MS: i64 = 24 * 3_600_000;
pub const CAP_MARGIN_K: f64 = 2.0;
pub const HOMECOMING_MARGIN_MIN: i64 = 30;
pub const ACTIVITY_MS: i64 = 30 * 60_000;
const HOMECOMING_DAYS: i64 = 42;
const HOMECOMING_MIN_DAYS: usize = 3;
const MAIN_RETURN_FROM_MIN: u32 = 12 * 60;
const DEPARTURE_FRESH_MS: i64 = 60 * 60_000;
const DEPARTURE_MAX_MS: i64 = 180 * 60_000;
const DECLARED_SETTLE_MS: i64 = 15 * 60_000;
const HOMECOMING_REFRESH_MS: i64 = 6 * 3_600_000;
const MIN_MS: i64 = 60_000;
const REBUILD_EXTRA_MS: i64 = 20 * MIN_MS;
const LOOKBACK_MS: i64 = 48 * 3_600_000;
const PULSE_STEPS: u32 = 4;
const PULSE_GRID_MIN: i64 = 5;
const LIFECYCLE: [&str; 4] = [kind::EXPERIMENT_ARMED, kind::EXPERIMENT_STARTED, kind::EXPERIMENT_STEP, kind::EXPERIMENT_ENDED];

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Doors {
    Closed,
    Open,
}

impl Doors {
    pub fn parse(text: &str) -> Option<Self> {
        match text {
            "closed" => Some(Doors::Closed),
            "open" => Some(Doors::Open),
            _ => None,
        }
    }

    pub fn name(self) -> &'static str {
        match self {
            Doors::Closed => "closed",
            Doors::Open => "open",
        }
    }

    pub fn label(self) -> &'static str {
        match self {
            Doors::Closed => "Türen zu",
            Doors::Open => "Türen offen",
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Phase {
    Cooldown,
    Pulses,
}

impl Phase {
    pub fn name(self) -> &'static str {
        match self {
            Phase::Cooldown => "cooldown",
            Phase::Pulses => "pulses",
        }
    }

    pub fn label(self) -> &'static str {
        match self {
            Phase::Cooldown => "Abkühlen",
            Phase::Pulses => "Takte",
        }
    }

    pub fn rule(self) -> &'static str {
        match self {
            Phase::Cooldown => "experiment_abkuehlen",
            Phase::Pulses => "experiment_takt",
        }
    }

    fn of(elapsed_min: i64) -> Self {
        if elapsed_min < COOLDOWN_MIN { Phase::Cooldown } else { Phase::Pulses }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Status {
    Armed,
    Running,
    Done,
    Aborted,
}

impl Status {
    pub fn name(self) -> &'static str {
        match self {
            Status::Armed => "armed",
            Status::Running => "running",
            Status::Done => "done",
            Status::Aborted => "aborted",
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Here {
    Away,
    Home,
    Unknown,
}

impl Here {
    pub fn of(state: &AppState, presence: Option<&Presence>) -> Self {
        let Some(p) = presence else { return Here::Unknown };
        match p.occupancy(&state.config) {
            Occupancy::Away => Here::Away,
            Occupancy::Room(_) => Here::Home,
            Occupancy::Unknown if !p.away && p.contact_age_minutes.is_some_and(|a| a <= state.config.still_contact_timeout_minutes) => Here::Home,
            Occupancy::Unknown => Here::Unknown,
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Seg {
    pub from_min: i64,
    pub to_min: i64,
    pub on: bool,
    pub phase: Phase,
}

fn fnv(text: &str) -> u64 {
    text.bytes().fold(0xcbf2_9ce4_8422_2325, |h, b| (h ^ u64::from(b)).wrapping_mul(0x0100_0000_01b3))
}

pub fn schedule(room: &str, doors: Doors, date: &str) -> Vec<Seg> {
    let mut rng = Rng::new(fnv(&format!("{room}|{}|{date}", doors.name())));
    let mut segs = vec![Seg { from_min: 0, to_min: COOLDOWN_MIN, on: false, phase: Phase::Cooldown }];
    let (mut at, mut on) = (COOLDOWN_MIN, rng.unit() < 0.5);
    while at < TOTAL_MIN {
        let length = (PULSE_GRID_MIN * i64::from(rng.range(1, PULSE_STEPS))).min(TOTAL_MIN - at);
        segs.push(Seg { from_min: at, to_min: at + length, on, phase: Phase::Pulses });
        at += length;
        on = !on;
    }
    segs
}

#[derive(Debug, Clone)]
struct RoomRun {
    room: String,
    segs: Vec<Seg>,
    end: Option<String>,
    entered: Option<usize>,
    executed: u32,
    capped: Option<usize>,
    starved: Option<usize>,
    starving: bool,
    min: Option<f64>,
    max: Option<f64>,
}

impl RoomRun {
    fn new(room: &str, doors: Doors, date: &str) -> Self {
        Self { room: room.into(), segs: schedule(room, doors, date), end: None, entered: None, executed: 0, capped: None, starved: None, starving: false, min: None, max: None }
    }

    fn seen(&mut self, temp: Option<f64>) {
        if let Some(t) = temp {
            self.min = Some(self.min.map_or(t, |m| m.min(t)));
            self.max = Some(self.max.map_or(t, |m| m.max(t)));
        }
    }

    fn index(&self, elapsed_min: i64) -> usize {
        self.segs.iter().position(|s| elapsed_min < s.to_min).unwrap_or(self.segs.len() - 1)
    }

    fn summary(&self) -> Value {
        json!({"planned": self.segs.len(), "executed": self.executed, "min": self.min, "max": self.max, "ended": self.end})
    }
}

#[derive(Debug, Clone)]
pub struct Run {
    id: String,
    doors: Doors,
    source: String,
    status: Status,
    armed_ms: i64,
    started_ms: Option<i64>,
    ended_ms: Option<i64>,
    reason: Option<String>,
    date: String,
    homecoming_ms: Option<i64>,
    rooms: Vec<RoomRun>,
    waiting: Option<String>,
    wait_key: Option<String>,
}

impl Run {
    fn expires_ms(&self) -> i64 {
        self.armed_ms + EXPIRY_MS
    }

    fn nominal_end_ms(&self) -> Option<i64> {
        self.started_ms.map(|s| s + TOTAL_MIN * MIN_MS)
    }

    fn cutoff_ms(&self) -> Option<i64> {
        self.homecoming_ms.map(|h| h - HOMECOMING_MARGIN_MIN * MIN_MS)
    }

    fn planned_end_ms(&self) -> Option<i64> {
        self.nominal_end_ms().map(|n| self.cutoff_ms().map_or(n, |c| n.min(c)))
    }

    fn event(&self, name: &str, now_ms: i64, detail: Value) -> Event {
        let mut detail = detail;
        detail["id"] = json!(self.id);
        detail["doors"] = json!(self.doors.name());
        Event { ts: now_ms * 1000, ..Event::now(name, "heatd").with_experiment(&self.id).with_detail(detail) }
    }

    fn schedule_json(&self) -> Value {
        let map: Map<String, Value> = self.rooms.iter().map(|r| (r.room.clone(), json!(r.segs.iter().map(|s| json!({"from_min": s.from_min, "to_min": s.to_min, "on": s.on, "phase": s.phase.name()})).collect::<Vec<_>>()))).collect();
        Value::Object(map)
    }

    fn finish(&mut self, status: Status, reason: &str, now_ms: i64) -> Event {
        self.finish_with(status, reason, now_ms, Value::Null)
    }

    fn finish_with(&mut self, status: Status, reason: &str, now_ms: i64, extra: Value) -> Event {
        self.status = status;
        (self.waiting, self.wait_key) = (None, None);
        self.reason = Some(reason.into());
        self.ended_ms = Some(now_ms);
        let rooms: Map<String, Value> = self.rooms.iter().map(|r| (r.room.clone(), r.summary())).collect();
        let mut detail = json!({"reason": reason, "status": status.name(), "source": self.source, "armed_ms": self.armed_ms, "started_ms": self.started_ms, "ended_ms": now_ms, "rooms": rooms});
        if let Some(extra) = extra.as_object() {
            extra.iter().for_each(|(k, v)| detail[k] = v.clone());
        }
        self.event(kind::EXPERIMENT_ENDED, now_ms, detail)
    }

    fn start(&mut self, rooms: &[&str], now_ms: i64, date: &str) -> Event {
        self.status = Status::Running;
        (self.waiting, self.wait_key) = (None, None);
        self.started_ms = Some(now_ms);
        self.date = date.into();
        self.rooms = rooms.iter().map(|r| RoomRun::new(r, self.doors, date)).collect();
        let end = self.planned_end_ms();
        self.event(kind::EXPERIMENT_STARTED, now_ms, json!({"source": self.source, "date": date, "started_ms": now_ms, "planned_end_ms": end, "homecoming_ms": self.homecoming_ms, "cooldown_min": COOLDOWN_MIN, "total_min": TOTAL_MIN, "schedule": self.schedule_json()}))
    }

    fn step_event(&self, now_ms: i64, room: &RoomRun, idx: usize, temp: Option<f64>, capped: bool, starved: bool) -> Event {
        let seg = room.segs[idx];
        let until = self.started_ms.unwrap_or(now_ms) + seg.to_min * MIN_MS;
        self.event(kind::EXPERIMENT_STEP, now_ms, json!({"room": room.room, "phase": seg.phase.name(), "step": idx, "steps": room.segs.len(), "on": seg.on && !capped && !starved, "capped": capped, "no_supply": starved, "until_ms": until, "temp": temp}))
    }

    fn note_wait(&mut self, wait: Option<&Wait>, now_ms: i64) -> Option<Event> {
        self.waiting = wait.map(Wait::text);
        let key = wait.map(Wait::key);
        if key == self.wait_key || key.is_none() {
            self.wait_key = key;
            return None;
        }
        self.wait_key = key;
        let detail = json!({"waiting": self.waiting, "key": self.wait_key, "armed_ms": self.armed_ms});
        Some(self.event(kind::EXPERIMENT_WAITING, now_ms, detail))
    }

    fn view(&self, now_ms: i64) -> Value {
        let elapsed = self.started_ms.map(|s| ((now_ms - s) / MIN_MS).max(0));
        let running = self.status == Status::Running;
        let rooms: Vec<Value> = self
            .rooms
            .iter()
            .map(|r| {
                let idx = elapsed.filter(|_| running && r.end.is_none()).map(|e| r.index(e));
                json!({"room": r.room, "step": idx, "steps": r.segs.len(), "on": idx.map(|i| r.segs[i].on && r.capped != Some(i) && !r.starving), "ended": r.end, "executed": r.executed, "min": r.min, "max": r.max, "schedule": r.segs.iter().map(|s| json!({"from_min": s.from_min, "to_min": s.to_min, "on": s.on, "phase": s.phase.name()})).collect::<Vec<_>>()})
            })
            .collect();
        let phase = elapsed.filter(|_| running).map(Phase::of);
        json!({"id": self.id, "state": self.status.name(), "doors": self.doors.name(), "doors_label": self.doors.label(), "source": self.source, "phase": phase.map(Phase::name), "phase_label": phase.map(Phase::label), "step_min": elapsed.filter(|_| running), "armed_ms": self.armed_ms, "expires_ms": (self.status == Status::Armed).then(|| self.expires_ms()), "started_ms": self.started_ms, "planned_end_ms": self.planned_end_ms(), "homecoming_ms": self.homecoming_ms, "ended_ms": self.ended_ms, "reason": self.reason, "waiting": self.waiting, "rooms": rooms})
    }
}

pub struct RoomIn<'a> {
    pub room: &'a str,
    pub held: bool,
    pub window: bool,
    pub no_supply: bool,
    pub temp: Option<f64>,
}

pub struct Tick<'a> {
    pub now_ms: i64,
    pub date: &'a str,
    pub here: Here,
    pub night: bool,
    pub cap_c: f64,
    pub rooms: &'a [RoomIn<'a>],
    pub activity: Option<(&'static str, i64)>,
    pub away_confirmed: bool,
    pub history_ready: bool,
    pub homecoming_ms: Option<i64>,
}

#[derive(Debug, Clone, PartialEq)]
enum Wait {
    Home,
    Settle(i64),
    Stale,
    Night,
    Activity(&'static str, i64),
    NoHistory,
    TooLate(i64),
}

fn local_hhmm(ms: i64) -> String {
    chrono::DateTime::from_timestamp_millis(ms).map_or("?".into(), |t| t.with_timezone(&chrono::Local).format("%H:%M").to_string())
}

impl Wait {
    fn key(&self) -> String {
        match self {
            Wait::Home => "home".into(),
            Wait::Settle(_) => "settle".into(),
            Wait::Stale => "stale".into(),
            Wait::Night => "night".into(),
            Wait::Activity(source, _) => format!("activity:{source}"),
            Wait::NoHistory => "no_history".into(),
            Wait::TooLate(_) => "too_late".into(),
        }
    }

    fn text(&self) -> String {
        match self {
            Wait::Home => "Uhr sieht dich noch zu Hause".into(),
            Wait::Settle(until) => format!("noch in der Wartezeit bis {}", local_hhmm(*until)),
            Wait::Stale => "Abmeldung älter als 3 h".into(),
            Wait::Night => "Nacht".into(),
            Wait::Activity(source, ago_min) => format!("Aktivität im Haus: {source} vor {ago_min} min"),
            Wait::NoHistory => "Heimkehr-Historie fehlt".into(),
            Wait::TooLate(at) => format!("zu nah an deiner üblichen Heimkehr ({})", local_hhmm(*at)),
        }
    }
}

#[derive(Debug, Default)]
pub struct Out {
    pub events: Vec<Event>,
    pub calls: HashMap<String, Call>,
}

#[derive(Debug, Clone, Copy, PartialEq)]
pub struct Call {
    pub on: bool,
    pub rule: &'static str,
}

#[derive(Debug, Default)]
pub struct Homecoming {
    days: Vec<(bool, u32)>,
    loaded_ms: Option<i64>,
}

fn median(mut v: Vec<u32>) -> u32 {
    v.sort_unstable();
    let mid = v.len() / 2;
    if v.len() % 2 == 1 { v[mid] } else { (v[mid - 1] + v[mid]) / 2 }
}

impl Homecoming {
    pub fn from_arrivals(local: &[NaiveDateTime]) -> Self {
        let mut first: BTreeMap<NaiveDate, u32> = BTreeMap::new();
        for at in local {
            let minute = at.hour() * 60 + at.minute();
            if minute >= MAIN_RETURN_FROM_MIN {
                let slot = first.entry(at.date()).or_insert(minute);
                *slot = (*slot).min(minute);
            }
        }
        Self { days: first.into_iter().map(|(d, m)| (d.weekday().number_from_monday() > 5, m)).collect(), loaded_ms: None }
    }

    pub fn typical(&self, weekend: bool) -> Option<u32> {
        if self.days.len() < HOMECOMING_MIN_DAYS {
            return None;
        }
        let same: Vec<u32> = self.days.iter().filter(|(w, _)| *w == weekend).map(|(_, m)| *m).collect();
        Some(median(if same.len() >= HOMECOMING_MIN_DAYS { same } else { self.days.iter().map(|(_, m)| *m).collect() }))
    }
}

#[derive(Debug, Default)]
pub struct Messfahrt {
    run: Option<Run>,
    departed_ms: Option<i64>,
    away_seen: bool,
    learned: Homecoming,
}

impl Messfahrt {
    pub fn arm(&mut self, doors: Doors, source: &str, now_ms: i64) -> Result<Event, &'static str> {
        if self.run.as_ref().is_some_and(|r| r.status == Status::Running) {
            return Err("Eine Messfahrt läuft schon – erst abbrechen");
        }
        let run = Run { id: format!("mf-{now_ms}"), doors, source: source.into(), status: Status::Armed, armed_ms: now_ms, started_ms: None, ended_ms: None, reason: None, date: String::new(), homecoming_ms: None, rooms: Vec::new(), waiting: None, wait_key: None };
        let event = run.event(kind::EXPERIMENT_ARMED, now_ms, json!({"source": source, "armed_ms": now_ms, "expires_ms": run.expires_ms()}));
        self.run = Some(run);
        Ok(event)
    }

    pub fn cancel(&mut self, now_ms: i64) -> Option<Event> {
        let run = self.run.as_mut().filter(|r| matches!(r.status, Status::Armed | Status::Running))?;
        Some(run.finish(Status::Aborted, "cancel", now_ms))
    }

    pub fn note_departure(&mut self, now_ms: i64) {
        self.departed_ms = Some(now_ms);
    }

    pub fn started_ms(&self) -> Option<i64> {
        self.run.as_ref().filter(|r| r.status == Status::Running).and_then(|r| r.started_ms)
    }

    pub fn running_in(&self, room: &str) -> bool {
        self.run.as_ref().is_some_and(|r| r.status == Status::Running && r.rooms.iter().any(|x| x.room == room && x.end.is_none()))
    }

    pub fn active(&self) -> bool {
        self.run.as_ref().is_some_and(|r| matches!(r.status, Status::Armed | Status::Running))
    }

    pub fn history_ready(&self) -> bool {
        self.learned.loaded_ms.is_some()
    }

    pub fn learn_due(&self, now_ms: i64) -> bool {
        self.active() && self.learned.loaded_ms.is_none_or(|t| now_ms - t >= HOMECOMING_REFRESH_MS)
    }

    pub fn learned(&mut self, mut learned: Homecoming, now_ms: i64) {
        learned.loaded_ms = Some(now_ms);
        self.learned = learned;
    }

    pub fn homecoming_ms(&self, now: NaiveDateTime, now_ms: i64) -> Option<i64> {
        let reference = self.started_ms().unwrap_or(now_ms);
        let local = now - chrono::Duration::milliseconds(now_ms - reference);
        let minute = local.hour() * 60 + local.minute();
        let typical = self.learned.typical(local.weekday().number_from_monday() > 5)?;
        Some(reference + (i64::from(typical) - i64::from(minute)) * MIN_MS)
    }

    pub fn view(&self, now_ms: i64) -> Value {
        self.run.as_ref().map_or_else(|| json!({"state": "idle"}), |r| r.view(now_ms))
    }

    fn blocker(run: &Run, departed_ms: Option<i64>, t: &Tick) -> Option<Wait> {
        let departed = departed_ms.is_some_and(|d| t.now_ms - d <= if d >= run.armed_ms { DEPARTURE_MAX_MS } else { DEPARTURE_FRESH_MS });
        let since_armed = t.now_ms - run.armed_ms;
        let gone = (t.here != Here::Home && (DECLARED_SETTLE_MS..=DEPARTURE_MAX_MS).contains(&since_armed)) || (t.here == Here::Away && t.away_confirmed && departed);
        if !gone {
            return Some(match (t.here, since_armed < DECLARED_SETTLE_MS) {
                (Here::Home, _) => Wait::Home,
                (_, true) => Wait::Settle(run.armed_ms + DECLARED_SETTLE_MS),
                _ => Wait::Stale,
            });
        }
        if t.night {
            return Some(Wait::Night);
        }
        if let Some((source, at)) = t.activity {
            return Some(Wait::Activity(source, (t.now_ms - at).max(0) / MIN_MS));
        }
        if !t.history_ready {
            return Some(Wait::NoHistory);
        }
        run.cutoff_ms().filter(|c| t.now_ms + TOTAL_MIN * MIN_MS > *c).map(|c| Wait::TooLate(c + HOMECOMING_MARGIN_MIN * MIN_MS))
    }

    pub fn tick(&mut self, t: &Tick) -> Out {
        let mut out = Out::default();
        let Some(run) = self.run.as_mut() else { return out };
        if matches!(run.status, Status::Armed | Status::Running) {
            run.homecoming_ms = t.homecoming_ms;
        }
        if run.status == Status::Armed {
            if t.now_ms >= run.expires_ms() {
                out.events.push(run.finish(Status::Aborted, "expired", t.now_ms));
                return out;
            }
            let wait = Self::blocker(run, self.departed_ms, t);
            out.events.extend(run.note_wait(wait.as_ref(), t.now_ms));
            if wait.is_some() {
                return out;
            }
            let rooms: Vec<&str> = t.rooms.iter().map(|r| r.room).collect();
            out.events.push(run.start(&rooms, t.now_ms, t.date));
            self.away_seen = false;
        }
        if run.status != Status::Running {
            return out;
        }
        if t.here == Here::Home {
            out.events.push(run.finish(Status::Aborted, "homecoming", t.now_ms));
            return out;
        }
        if self.away_seen && !t.away_confirmed {
            out.events.push(run.finish(Status::Aborted, "away_lost", t.now_ms));
            return out;
        }
        self.away_seen |= t.away_confirmed;
        if let Some((source, _)) = t.activity {
            out.events.push(run.finish_with(Status::Aborted, "home_activity", t.now_ms, json!({"activity": source})));
            return out;
        }
        if t.night {
            out.events.push(run.finish(Status::Aborted, "night", t.now_ms));
            return out;
        }
        if run.nominal_end_ms().is_some_and(|e| t.now_ms >= e) {
            out.events.push(run.finish(Status::Done, "completed", t.now_ms));
            return out;
        }
        if run.cutoff_ms().is_some_and(|c| t.now_ms >= c) {
            out.events.push(run.finish(Status::Aborted, "planned_homecoming", t.now_ms));
            return out;
        }
        let elapsed = (t.now_ms - run.started_ms.unwrap_or(t.now_ms)) / MIN_MS;
        for i in 0..run.rooms.len() {
            let input = t.rooms.iter().find(|r| r.room == run.rooms[i].room);
            let why = match input {
                None => Some("room_gone"),
                Some(r) if r.held => Some("manual_hold"),
                Some(r) if r.window => Some("window"),
                _ => None,
            };
            let temp = input.and_then(|r| r.temp);
            if let (Some(why), None) = (why, &run.rooms[i].end) {
                run.rooms[i].end = Some(why.into());
                out.events.push(run.event(kind::EXPERIMENT_STEP, t.now_ms, json!({"room": run.rooms[i].room, "ended": why, "temp": temp})));
            }
            if run.rooms[i].end.is_some() {
                continue;
            }
            run.rooms[i].seen(temp);
            let idx = run.rooms[i].index(elapsed);
            let seg = run.rooms[i].segs[idx];
            let over = seg.on && temp.is_some_and(|c| c > t.cap_c);
            let new_cap = over && run.rooms[i].capped != Some(idx);
            if new_cap {
                run.rooms[i].capped = Some(idx);
            }
            let starved = seg.on && input.is_some_and(|r| r.no_supply);
            run.rooms[i].starving = starved;
            let new_starved = starved && run.rooms[i].starved != Some(idx);
            if new_starved {
                run.rooms[i].starved = Some(idx);
            }
            if run.rooms[i].entered != Some(idx) || new_cap || new_starved {
                if run.rooms[i].entered != Some(idx) {
                    run.rooms[i].executed += 1;
                }
                run.rooms[i].entered = Some(idx);
                let capped = run.rooms[i].capped == Some(idx);
                out.events.push(run.step_event(t.now_ms, &run.rooms[i], idx, temp, capped, starved));
            }
            let on = seg.on && run.rooms[i].capped != Some(idx) && !starved && temp.is_some();
            out.calls.insert(run.rooms[i].room.clone(), Call { on, rule: seg.phase.rule() });
        }
        out
    }

    pub fn from_events(events: &[(i64, String, Value)], now_ms: i64, here: Here) -> (Self, Vec<Event>) {
        let mut run: Option<Run> = None;
        for (ts, k, d) in events {
            let id = d["id"].as_str().unwrap_or_default().to_string();
            let doors = d["doors"].as_str().and_then(Doors::parse);
            match (k.as_str(), doors) {
                (kind::EXPERIMENT_ARMED, Some(doors)) => run = Some(Run { id, doors, source: d["source"].as_str().unwrap_or("chat").into(), status: Status::Armed, armed_ms: d["armed_ms"].as_i64().unwrap_or(*ts), started_ms: None, ended_ms: None, reason: None, date: String::new(), homecoming_ms: None, rooms: Vec::new(), waiting: None, wait_key: None }),
                (kind::EXPERIMENT_STARTED, _) => {
                    let Some(r) = run.as_mut().filter(|r| r.id == id) else { continue };
                    let names: Vec<String> = d["schedule"].as_object().map(|m| m.keys().cloned().collect()).unwrap_or_default();
                    let names: Vec<&str> = names.iter().map(String::as_str).collect();
                    r.start(&names, d["started_ms"].as_i64().unwrap_or(*ts), d["date"].as_str().unwrap_or_default());
                    r.homecoming_ms = d["homecoming_ms"].as_i64();
                }
                (kind::EXPERIMENT_STEP, _) => {
                    let Some(r) = run.as_mut().filter(|r| r.id == id && r.status == Status::Running) else { continue };
                    let Some(room) = r.rooms.iter_mut().find(|x| Some(x.room.as_str()) == d["room"].as_str()) else { continue };
                    room.seen(d["temp"].as_f64());
                    match d["ended"].as_str() {
                        Some(why) => room.end = Some(why.into()),
                        None => {
                            let step = d["step"].as_u64().unwrap_or(0) as usize;
                            if room.entered != Some(step) {
                                room.executed += 1;
                            }
                            room.entered = Some(step);
                            if d["capped"].as_bool() == Some(true) {
                                room.capped = Some(step);
                            }
                        }
                    }
                }
                (kind::EXPERIMENT_ENDED, _) => {
                    let Some(r) = run.as_mut().filter(|r| r.id == id) else { continue };
                    r.status = if d["status"].as_str() == Some("done") { Status::Done } else { Status::Aborted };
                    r.reason = d["reason"].as_str().map(String::from);
                    r.ended_ms = d["ended_ms"].as_i64().or(Some(*ts));
                }
                _ => {}
            }
        }
        let mut events = Vec::new();
        if let Some(r) = run.as_mut() {
            let reason = match r.status {
                Status::Armed if now_ms >= r.expires_ms() => Some("expired"),
                Status::Running if here != Here::Away || r.planned_end_ms().is_none_or(|e| now_ms >= e) => Some("restart"),
                _ => None,
            };
            if let Some(reason) = reason {
                events.push(r.finish(Status::Aborted, reason, now_ms));
            }
        }
        (Self { run, ..Default::default() }, events)
    }
}

pub fn restore(state: &AppState, now_ms: i64) -> Result<String> {
    let rows = crate::restore::records(state.sink.root(), Table::Events, now_ms - LOOKBACK_MS, now_ms, &["kind", "detail"], &[], &[])?;
    let events: Vec<(i64, String, Value)> = rows
        .into_iter()
        .filter_map(|r| {
            let [Some(k), Some(detail)] = &r.text[..] else { return None };
            Some((r.ts, k.clone(), serde_json::from_str::<Value>(detail).ok()?)).filter(|(_, k, _)| LIFECYCLE.contains(&k.as_str()))
        })
        .collect();
    let presence = crate::rules::current_presence(state, now_ms)?;
    let here = Here::of(state, presence.as_ref());
    let (restored, ended) = Messfahrt::from_events(&events, now_ms, here);
    let summary = restored.view(now_ms)["state"].as_str().unwrap_or("idle").to_string();
    ended.into_iter().for_each(|e| log(state, e));
    state.commanded.lock().unwrap().messfahrt = restored;
    rebuild_home_memory(state, now_ms)?;
    Ok(summary)
}

fn rebuild_home_memory(state: &AppState, now_ms: i64) -> Result<()> {
    let from = now_ms - ACTIVITY_MS - REBUILD_EXTRA_MS;
    let root = state.sink.root();
    let readings: Vec<crate::lake::DeviceReading> = crate::restore::records(root, Table::DeviceReadings, from, now_ms, &["kind"], &["node", "endpoint", "cluster", "attribute", "value"], &[])?
        .into_iter()
        .filter_map(|r| {
            let [Some(node), Some(endpoint), Some(cluster), Some(attribute)] = r.num[..4] else { return None };
            Some(crate::lake::DeviceReading { ts: r.ts * 1000, node: node as u64, endpoint: endpoint as u16, cluster: cluster as u32, attribute: attribute as u32, kind: r.text[0].clone()?, value: r.num[4], ..Default::default() })
        })
        .collect();
    let edges: Vec<(String, i64)> = crate::restore::records(root, Table::Labels, now_ms - ACTIVITY_MS, now_ms, &["source", "via"], &[], &[])?.into_iter().filter(|r| r.text[1].as_deref() == Some("plug")).filter_map(|r| Some((r.text[0].clone()?, r.ts))).collect();
    state.devices.rebuild_home(&readings, &edges);
    let manual = crate::restore::records(root, Table::Events, now_ms - ACTIVITY_MS, now_ms, &["kind"], &[], &[])?.into_iter().filter(|r| r.text[0].as_deref() == Some(kind::MANUAL_SETPOINT)).map(|r| r.ts);
    let dialed = crate::restore::records(root, Table::Decisions, now_ms - ACTIVITY_MS, now_ms, &["rule"], &[], &[])?.into_iter().filter(|r| r.text[0].as_deref() == Some("manual")).map(|r| r.ts);
    let mut commands = state.commanded.lock().unwrap();
    manual.chain(dialed).for_each(|ts| commands.note_manual(ts));
    Ok(())
}

fn log(state: &AppState, event: Event) {
    if let Err(e) = state.sink.append(event) {
        eprintln!("experiment event: {e:#}");
    }
}

fn rooms_with_nodes(state: &AppState) -> Vec<&Room> {
    state.config.rooms.iter().filter(|r| !r.nodes.is_empty()).collect()
}

fn learn_homecoming(state: &AppState, now_ms: i64) {
    let rows = match crate::restore::records(state.sink.root(), Table::Events, now_ms - HOMECOMING_DAYS * 86_400_000, now_ms, &["kind"], &[], &[]) {
        Ok(rows) => rows,
        Err(e) => {
            eprintln!("homecoming history: {e:#}");
            return;
        }
    };
    let arrivals: Vec<NaiveDateTime> = rows.iter().filter(|r| r.text[0].as_deref() == Some(kind::PRESENCE_ARRIVE)).filter_map(|r| Some(chrono::DateTime::from_timestamp_millis(r.ts)?.with_timezone(&chrono::Local).naive_local())).collect();
    state.commanded.lock().unwrap().messfahrt.learned(Homecoming::from_arrivals(&arrivals), now_ms);
}

fn home_activity(state: &AppState, now_ms: i64) -> Option<(&'static str, i64)> {
    let recent = |ms: i64| now_ms - ms <= ACTIVITY_MS;
    if let Some(found) = state.devices.home_activity_at(now_ms, ACTIVITY_MS) {
        return Some(found);
    }
    if let Some(at) = state.commanded.lock().unwrap().last_manual_ms().filter(|ms| recent(*ms)) {
        return Some(("manual_setpoint", at));
    }
    match state.storage.device_statuses() {
        Ok(statuses) => statuses.iter().filter(|s| !s.away && recent(s.state_ts)).map(|s| s.state_ts).max().map(|at| ("watch_scan", at)),
        Err(_) => Some(("status_unreadable", now_ms)),
    }
}

fn away_confirmed(state: &AppState) -> bool {
    let tracker = state.tracker.lock().unwrap();
    state.storage.device_statuses().unwrap_or_default().iter().any(|s| s.away && tracker.confirmed(&s.device))
}

pub(crate) fn tick(state: &AppState, now: NaiveDateTime, now_ms: i64, windows: &HashSet<String>, presence: Option<&Presence>) -> Out {
    if state.commanded.lock().unwrap().messfahrt.learn_due(now_ms) {
        learn_homecoming(state, now_ms);
    }
    if !state.commanded.lock().unwrap().messfahrt.active() {
        return Out::default();
    }
    let rooms = rooms_with_nodes(state);
    let temps: Vec<Option<f64>> = {
        let w = state.windows.lock().unwrap();
        rooms.iter().map(|r| r.nodes.iter().find_map(|&n| w.local_temp(n, now_ms, state.config.stale.after_ms()))).collect()
    };
    let no_supply = state.windows.lock().unwrap().no_supply_rooms();
    let date = now.format("%Y-%m-%d").to_string();
    let (activity, away_confirmed) = (home_activity(state, now_ms), away_confirmed(state));
    let mut out = {
        let mut commands = state.commanded.lock().unwrap();
        let inputs: Vec<RoomIn> = rooms.iter().zip(&temps).map(|(r, t)| RoomIn { room: &r.name, held: commands.holding(&r.nodes), window: windows.contains(&r.name), no_supply: no_supply.contains(&r.name), temp: *t }).collect();
        let (homecoming_ms, history_ready) = (commands.messfahrt.homecoming_ms(now, now_ms), commands.messfahrt.history_ready());
        let tick = Tick { now_ms, date: &date, here: Here::of(state, presence), night: in_sleep_hours(&state.config, now), cap_c: state.config.occupied_setpoint + CAP_MARGIN_K, rooms: &inputs, activity, away_confirmed, history_ready, homecoming_ms };
        commands.messfahrt.tick(&tick)
    };
    std::mem::take(&mut out.events).into_iter().for_each(|e| log(state, e));
    out
}

pub(crate) fn apply_calls(state: &AppState, out: &Out, wants: &mut [Desired], readings: &[crate::storage::Reading]) {
    let (frost, cap) = (state.config.frost_setpoint, state.failsafe.boost_max_c());
    let high = state.boost.boost_setpoint().map_or(cap, |b| b.min(cap));
    for want in wants.iter_mut().filter(|w| w.reason != "window_open") {
        let Some(call) = out.calls.get(&want.room) else { continue };
        want.setpoint = if call.on { clamp_boost(readings, want.node, high, cap) } else { clamp_setpoint(readings, want.node, frost, state.failsafe.max_c()) };
        (want.reason, want.rule, want.experiment_id) = ("experiment", call.rule, None);
    }
}

#[derive(serde::Deserialize, Default)]
pub struct Body {
    #[serde(default)]
    pub doors: Option<String>,
    #[serde(default)]
    pub source: Option<String>,
    #[serde(default)]
    pub cancel: bool,
}

fn reply(status: StatusCode, body: Value) -> Reply {
    Reply { status, body }
}

fn fail(status: StatusCode, error: &str) -> Reply {
    reply(status, json!({"ok": false, "error": error}))
}

pub fn apply(state: &AppState, body: Body, now_ms: i64) -> Reply {
    if body.cancel {
        let event = state.commanded.lock().unwrap().messfahrt.cancel(now_ms);
        let cancelled = event.is_some();
        event.into_iter().for_each(|e| log(state, e));
        return reply(StatusCode::OK, json!({"ok": true, "cancelled": cancelled, "experiment": state.commanded.lock().unwrap().messfahrt.view(now_ms)}));
    }
    let Some(doors) = body.doors.as_deref().and_then(Doors::parse) else { return fail(StatusCode::BAD_REQUEST, "doors muss closed oder open sein") };
    let source = body.source.as_deref().unwrap_or("chat");
    if !matches!(source, "chat" | "watch") {
        return fail(StatusCode::BAD_REQUEST, "source muss chat oder watch sein");
    }
    let armed = state.commanded.lock().unwrap().messfahrt.arm(doors, source, now_ms);
    match armed {
        Ok(event) => {
            log(state, event);
            reply(StatusCode::OK, json!({"ok": true, "experiment": state.commanded.lock().unwrap().messfahrt.view(now_ms)}))
        }
        Err(why) => fail(StatusCode::CONFLICT, why),
    }
}

pub fn report_section(root: &Path, date: NaiveDate) -> Result<String> {
    let db = open_lake(root, date - chrono::Duration::days(1), date + chrono::Duration::days(1))?;
    let (lo, hi) = (midnight_us(date), midnight_us(date) + 86_400_000_000);
    let list = LIFECYCLE.iter().map(|k| format!("'{k}'")).collect::<Vec<_>>().join(", ");
    let events = rows(&db.conn, &format!("SELECT epoch_us(ts), kind, detail FROM events WHERE kind IN ({list}) AND epoch_us(ts) >= {lo} AND epoch_us(ts) < {hi} ORDER BY ts"), |r| Ok((r.get::<_, i64>(0)?, r.get::<_, String>(1)?, r.get::<_, Option<String>>(2)?)))?;
    let mut runs: BTreeMap<String, Vec<(i64, String, Value)>> = BTreeMap::new();
    for (us, k, detail) in events {
        let d: Value = detail.and_then(|t| serde_json::from_str(&t).ok()).unwrap_or(Value::Null);
        runs.entry(d["id"].as_str().unwrap_or("-").to_string()).or_default().push((us, k, d));
    }
    let mut md = String::from("\n## Messfahrten\n\n");
    if runs.is_empty() {
        md.push_str("Keine Messfahrt.\n");
    }
    for (id, list) in runs {
        let find = |name: &str| list.iter().find(|(_, k, _)| k == name);
        let doors = list.iter().find_map(|(_, _, d)| d["doors"].as_str()).and_then(Doors::parse).map_or("-", Doors::label);
        let started = find(kind::EXPERIMENT_STARTED).map(|(us, _, _)| hhmm(*us));
        let ended = find(kind::EXPERIMENT_ENDED);
        let reason = ended.and_then(|(_, _, d)| d["reason"].as_str()).unwrap_or("offen");
        let _ = writeln!(md, "- {id}: {doors}, scharf {}, Start {}, Ende {}, Grund {reason}", find(kind::EXPERIMENT_ARMED).map_or("-".into(), |(us, _, _)| hhmm(*us)), started.unwrap_or_else(|| "-".into()), ended.map_or("-".into(), |(us, _, _)| hhmm(*us)));
        let plan = find(kind::EXPERIMENT_STARTED).and_then(|(_, _, d)| d["schedule"].as_object());
        for (room, segs) in plan.into_iter().flatten() {
            let planned = segs.as_array().map_or(0, Vec::len);
            let steps = list.iter().filter(|(_, k, d)| k == kind::EXPERIMENT_STEP && d["room"] == json!(room) && d["ended"].is_null()).map(|(_, _, d)| d["step"].as_u64()).collect::<HashSet<_>>().len();
            let summary = ended.map(|(_, _, d)| &d["rooms"][room]);
            let range = match summary.map(|s| (s["min"].as_f64(), s["max"].as_f64())) {
                Some((Some(lo), Some(hi))) => format!("{} bis {} °C", crate::manual::celsius_text(lo), crate::manual::celsius_text(hi)),
                _ => "keine Messwerte".into(),
            };
            let stop = summary.and_then(|s| s["ended"].as_str()).map_or(String::new(), |why| format!(", Raum vorzeitig beendet ({why})"));
            let _ = writeln!(md, "  - {room}: {steps} von {planned} Schritten ausgeführt, Sensor {range}{stop}");
        }
    }
    Ok(md)
}
