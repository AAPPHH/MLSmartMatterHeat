use crate::config::{parse_minutes, parse_weekdays, window_start, Config};
use crate::http::AppState;
use crate::lake::{self, kind};
use crate::manual::{celsius_text, MAX_CELSIUS};
use chrono::{Datelike, Duration, NaiveDate, NaiveDateTime, Timelike, Weekday};
use serde::{Deserialize, Serialize};
use serde_json::{json, Value};
use std::collections::BTreeMap;
use std::io::Write;
use std::path::PathBuf;
use std::sync::{Arc, Mutex};

pub const MAX_PLANS: usize = 64;
pub const MAX_LABEL: usize = 80;
pub const MAX_ONCE_DAYS: i64 = 60;
pub const MAX_OFFSET_K: f64 = 3.0;
pub const STEP_MIN: i64 = 15;
pub const MAX_PREVIEW_HOURS: u32 = 336;
pub const DEFAULT_PREVIEW_HOURS: u32 = 168;
const DT_FORMATS: [&str; 3] = ["%Y-%m-%dT%H:%M:%S", "%Y-%m-%dT%H:%M", "%Y-%m-%d %H:%M"];
const DT_PLAN: &str = "%Y-%m-%dT%H:%M";
const DAY_NAMES: [&str; 7] = ["Mo", "Di", "Mi", "Do", "Fr", "Sa", "So"];
const LONG_NAMES: [&str; 7] = ["montags", "dienstags", "mittwochs", "donnerstags", "freitags", "samstags", "sonntags"];

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum Action {
    Setpoint { celsius: f64 },
    Offset { k: f64 },
    Away,
    Frost,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum When {
    Once { from: String, to: String },
    Weekly {
        days: Vec<String>,
        from: String,
        to: String,
        #[serde(default)]
        valid_from: Option<String>,
        #[serde(default)]
        valid_until: Option<String>,
    },
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct NewPlan {
    pub rooms: Vec<String>,
    pub action: Action,
    pub when: When,
    #[serde(default)]
    pub label: String,
    #[serde(default = "api_source")]
    pub source: String,
}

fn api_source() -> String {
    "api".into()
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Plan {
    pub id: String,
    pub rooms: Vec<String>,
    pub action: Action,
    pub when: When,
    pub label: String,
    pub source: String,
    pub created_ms: i64,
}

#[derive(Debug, Clone, PartialEq)]
pub struct Hit {
    pub id: String,
    pub label: String,
    pub off: bool,
    pub once: bool,
}

impl Hit {
    pub fn ceiling(&self, max_c: f64, boost_max_c: f64) -> f64 {
        if self.once { boost_max_c } else { max_c }
    }
}

pub struct Resolved<'a> {
    pub frost: Option<&'a Plan>,
    pub away: Option<&'a Plan>,
    pub value: Option<&'a Plan>,
}

fn parse_dt(text: &str) -> Option<NaiveDateTime> {
    DT_FORMATS.iter().find_map(|f| NaiveDateTime::parse_from_str(text.trim(), f).ok())
}

fn parse_date(text: &str) -> Option<NaiveDate> {
    NaiveDate::parse_from_str(text.trim(), "%Y-%m-%d").ok()
}

fn chars(text: &str, max: usize) -> String {
    text.chars().take(max).collect()
}

fn short_date(date: NaiveDate) -> String {
    date.format("%d.%m.").to_string()
}

fn capital(text: &str) -> String {
    let mut it = text.chars();
    it.next().map_or_else(String::new, |c| c.to_uppercase().chain(it).collect())
}

impl Plan {
    pub fn applies(&self, room: &str) -> bool {
        self.rooms.iter().any(|r| r == "all" || r == room)
    }

    pub fn active(&self, now: NaiveDateTime) -> bool {
        match &self.when {
            When::Once { from, to } => parse_dt(from).zip(parse_dt(to)).is_some_and(|(f, t)| f <= now && now < t),
            When::Weekly { days, from, to, valid_from, valid_until } => {
                let (Ok(days), Ok(from), Ok(to)) = (parse_weekdays(days), parse_minutes(from), parse_minutes(to)) else { return false };
                let Some(start) = window_start(&days, from, to, now) else { return false };
                valid_from.as_deref().and_then(parse_date).is_none_or(|d| start >= d) && valid_until.as_deref().and_then(parse_date).is_none_or(|d| start <= d)
            }
        }
    }

    fn expired(&self, now: NaiveDateTime) -> bool {
        match &self.when {
            When::Once { to, .. } => parse_dt(to).is_none_or(|t| t <= now),
            When::Weekly { valid_until, .. } => valid_until.as_deref().and_then(parse_date).is_some_and(|d| d + Duration::days(1) < now.date()),
        }
    }

    pub fn hit(&self) -> Hit {
        Hit { id: self.id.clone(), label: self.label.clone(), off: matches!(self.action, Action::Away | Action::Frost), once: matches!(self.when, When::Once { .. }) }
    }

    pub fn value(&self, base: f64, frost: f64) -> f64 {
        match self.action {
            Action::Setpoint { celsius } => celsius,
            Action::Offset { k } => (base + k).clamp(frost, MAX_CELSIUS),
            Action::Away | Action::Frost => base,
        }
    }

    pub fn view(&self) -> Value {
        json!({"id": self.id, "label": self.label, "action": self.action, "summary": summary(self)})
    }
}

pub fn resolve<'a>(plans: &'a [Plan], room: &str, now: NaiveDateTime) -> Resolved<'a> {
    let newest = |pick: fn(&Action) -> bool| plans.iter().filter(|p| pick(&p.action) && p.applies(room) && p.active(now)).max_by_key(|p| p.created_ms);
    Resolved {
        frost: newest(|a| matches!(a, Action::Frost)),
        away: newest(|a| matches!(a, Action::Away)),
        value: newest(|a| matches!(a, Action::Setpoint { .. } | Action::Offset { .. })),
    }
}

pub fn active_in<'a>(plans: &'a [Plan], room: &str, now: NaiveDateTime) -> Vec<&'a Plan> {
    plans.iter().filter(|p| p.applies(room) && p.active(now)).collect()
}

pub fn validate(config: &Config, new: &NewPlan, now: NaiveDateTime) -> Result<NewPlan, String> {
    let mut plan = new.clone();
    let known = |room: &String| config.rooms.iter().any(|r| &r.name == room);
    if plan.rooms.is_empty() {
        return Err("Mindestens ein Raum ist nötig".into());
    }
    if let Some(unknown) = plan.rooms.iter().find(|r| *r != "all" && !known(r)) {
        return Err(format!("Raum {unknown} ist unbekannt. Bekannt: {}", config.rooms.iter().map(|r| r.name.as_str()).collect::<Vec<_>>().join(", ")));
    }
    if plan.rooms.iter().any(|r| r == "all") {
        plan.rooms = vec!["all".into()];
    } else {
        let mut seen = Vec::new();
        plan.rooms.retain(|r| !seen.contains(r) && {
            seen.push(r.clone());
            true
        });
    }
    match plan.action {
        Action::Setpoint { celsius } if !(celsius.is_finite() && (config.frost_setpoint..=MAX_CELSIUS).contains(&celsius) && (celsius * 2.0).fract() == 0.0) => {
            return Err(format!("celsius muss zwischen {} und {} in 0,5er Schritten liegen", celsius_text(config.frost_setpoint), celsius_text(MAX_CELSIUS)));
        }
        Action::Offset { k } if !(k.is_finite() && k.abs() <= MAX_OFFSET_K && (k * 2.0).fract() == 0.0) => {
            return Err(format!("k muss zwischen -{MAX_OFFSET_K} und {MAX_OFFSET_K} in 0,5er Schritten liegen"));
        }
        _ => {}
    }
    match &mut plan.when {
        When::Once { from, to } => {
            let (f, t) = parse_dt(from).zip(parse_dt(to)).ok_or("from und to müssen lokale Zeitpunkte wie 2026-12-01T15:00 sein")?;
            if t <= f {
                return Err("to muss nach from liegen".into());
            }
            if t - f > Duration::days(MAX_ONCE_DAYS) {
                return Err(format!("Ein einmaliger Plan darf höchstens {MAX_ONCE_DAYS} Tage dauern"));
            }
            if t <= now {
                return Err("Der Zeitraum liegt schon in der Vergangenheit".into());
            }
            (*from, *to) = (f.format(DT_PLAN).to_string(), t.format(DT_PLAN).to_string());
        }
        When::Weekly { days, from, to, valid_from, valid_until } => {
            if days.is_empty() || parse_weekdays(days).is_err() {
                return Err("days braucht Wochentage (monday, tuesday, ..., weekdays, weekend)".into());
            }
            if parse_minutes(from).is_err() || parse_minutes(to).is_err() {
                return Err("from und to müssen HH:MM sein".into());
            }
            let dates = [valid_from.as_deref(), valid_until.as_deref()].map(|d| d.map(|d| parse_date(d).ok_or("valid_from und valid_until müssen JJJJ-MM-TT sein")).transpose());
            let [Ok(a), Ok(b)] = dates else { return Err("valid_from und valid_until müssen JJJJ-MM-TT sein".into()) };
            if a.zip(b).is_some_and(|(a, b)| b < a) {
                return Err("valid_until liegt vor valid_from".into());
            }
            if b.is_some_and(|b| b < now.date()) {
                return Err("valid_until liegt in der Vergangenheit".into());
            }
        }
    }
    if plan.label.chars().count() > MAX_LABEL {
        return Err(format!("label darf höchstens {MAX_LABEL} Zeichen haben"));
    }
    if !["api", "assistant"].contains(&plan.source.as_str()) {
        return Err("source muss api oder assistant sein".into());
    }
    Ok(plan)
}

pub fn summary(plan: &Plan) -> String {
    let rooms = if plan.rooms.iter().any(|r| r == "all") { "alle Räume".to_string() } else { plan.rooms.iter().map(|r| capital(r)).collect::<Vec<_>>().join(", ") };
    let what = match plan.action {
        Action::Setpoint { celsius } => format!("{} °C", celsius_text(celsius)),
        Action::Offset { k } => format!("{}{} K", if k >= 0.0 { "+" } else { "-" }, celsius_text(k.abs())),
        Action::Away => "Abwesenheits-Temperatur".into(),
        Action::Frost => "Frostschutz".into(),
    };
    let when = match &plan.when {
        When::Once { from, to } => match parse_dt(from).zip(parse_dt(to)) {
            Some((f, t)) if f.date() == t.date() => format!("am {} von {} bis {}", short_date(f.date()), f.format("%H:%M"), t.format("%H:%M")),
            Some((f, t)) => format!("vom {} {} bis {} {}", short_date(f.date()), f.format("%H:%M"), short_date(t.date()), t.format("%H:%M")),
            None => String::new(),
        },
        When::Weekly { days, from, to, valid_from, valid_until } => {
            let list = parse_weekdays(days).unwrap_or_default();
            let mut sorted: Vec<Weekday> = Vec::new();
            list.iter().for_each(|d| if !sorted.contains(d) { sorted.push(*d) });
            sorted.sort_by_key(|d| d.num_days_from_monday());
            let names = match sorted.as_slice() {
                [d] => LONG_NAMES[d.num_days_from_monday() as usize].to_string(),
                all => all.iter().map(|d| DAY_NAMES[d.num_days_from_monday() as usize]).collect::<Vec<_>>().join(","),
            };
            let bound = |d: &Option<String>, word: &str| d.as_deref().and_then(parse_date).map(|d| format!(" {word} {}", short_date(d))).unwrap_or_default();
            format!("{names} {from} bis {to}{}{}", bound(valid_from, "ab"), bound(valid_until, "bis"))
        }
    };
    chars(&format!("{rooms} {what} {when}"), 200)
}

fn default_label(plan: &NewPlan) -> String {
    let stub = Plan { id: String::new(), rooms: plan.rooms.clone(), action: plan.action.clone(), when: plan.when.clone(), label: String::new(), source: plan.source.clone(), created_ms: 0 };
    chars(&summary(&stub), MAX_LABEL)
}

#[derive(Clone, Default)]
pub struct Plans {
    items: Arc<Mutex<Vec<Plan>>>,
    path: Option<Arc<PathBuf>>,
}

impl Plans {
    pub fn open(path: PathBuf, now: NaiveDateTime) -> Self {
        let items = Self::restore(&path);
        let plans = Self { items: Arc::new(Mutex::new(items)), path: Some(Arc::new(path)) };
        plans.prune(now);
        plans
    }

    fn restore(path: &PathBuf) -> Vec<Plan> {
        let Ok(text) = std::fs::read_to_string(path) else { return Vec::new() };
        match serde_json::from_str::<Value>(&text) {
            Ok(v) => v["plans"]
                .as_array()
                .map(|a| a.iter().filter_map(|e| serde_json::from_value::<Plan>(e.clone()).map_err(|err| eprintln!("plans: skipped entry: {err}")).ok()).collect())
                .unwrap_or_default(),
            Err(e) => {
                let bad = path.with_extension(format!("json.bad-{}", chrono::Utc::now().timestamp_millis()));
                eprintln!("plans: {e}, moved to {}", bad.display());
                if let Err(r) = std::fs::rename(path, &bad) {
                    eprintln!("plans: rename failed: {r}");
                }
                Vec::new()
            }
        }
    }

    pub fn list(&self) -> Vec<Plan> {
        let mut items = self.items.lock().unwrap().clone();
        items.sort_by_key(|p| p.created_ms);
        items
    }

    fn store(&self, items: &[Plan]) -> Result<(), String> {
        let Some(path) = &self.path else { return Ok(()) };
        let tmp = path.with_extension("json.tmp");
        let text = serde_json::to_string_pretty(&json!({"plans": items})).map_err(|e| e.to_string())?;
        let write = || -> std::io::Result<()> {
            if let Some(dir) = path.parent() {
                std::fs::create_dir_all(dir)?;
            }
            let mut file = std::fs::File::create(&tmp)?;
            file.write_all(text.as_bytes())?;
            file.sync_all()?;
            drop(file);
            std::fs::rename(&tmp, path.as_path())
        };
        write().map_err(|e| format!("Plan konnte nicht gespeichert werden: {e}"))
    }

    pub fn insert(&self, config: &Config, new: &NewPlan, now: NaiveDateTime, now_ms: i64) -> Result<Plan, String> {
        let mut new = validate(config, new, now)?;
        let mut items = self.items.lock().unwrap();
        let mut next: Vec<Plan> = items.iter().filter(|p| !p.expired(now)).cloned().collect();
        if next.len() >= MAX_PLANS {
            return Err(format!("Es gibt schon {MAX_PLANS} Pläne, bitte erst welche löschen"));
        }
        if new.label.trim().is_empty() {
            new.label = default_label(&new);
        }
        let mut created = now_ms;
        while next.iter().any(|p| p.id == format!("p{created:x}")) {
            created += 1;
        }
        let plan = Plan { id: format!("p{created:x}"), rooms: new.rooms, action: new.action, when: new.when, label: new.label, source: new.source, created_ms: created };
        next.push(plan.clone());
        self.store(&next)?;
        *items = next;
        Ok(plan)
    }

    pub fn remove(&self, id: &str) -> Result<Option<Plan>, String> {
        let mut items = self.items.lock().unwrap();
        let Some(at) = items.iter().position(|p| p.id == id) else { return Ok(None) };
        let mut next = items.clone();
        let gone = next.remove(at);
        self.store(&next)?;
        *items = next;
        Ok(Some(gone))
    }

    pub fn prune(&self, now: NaiveDateTime) {
        let mut items = self.items.lock().unwrap();
        if !items.iter().any(|p| p.expired(now)) {
            return;
        }
        let next: Vec<Plan> = items.iter().filter(|p| !p.expired(now)).cloned().collect();
        match self.store(&next) {
            Ok(()) => *items = next,
            Err(e) => eprintln!("plans: {e}"),
        }
    }
}

fn record(state: &AppState, detail: Value) {
    if let Err(e) = state.sink.append(lake::Event::now(kind::PLAN_CHANGE, "heatd").with_detail(detail)) {
        eprintln!("plan event: {e:#}");
    }
}

pub fn create(state: &AppState, new: &NewPlan, now: NaiveDateTime, now_ms: i64) -> Result<(Plan, String), String> {
    let plan = state.plans.insert(&state.config, new, now, now_ms)?;
    record(state, json!({"change": "create", "plan": plan}));
    let text = summary(&plan);
    Ok((plan, text))
}

pub fn delete(state: &AppState, id: &str) -> Result<Option<Plan>, String> {
    let gone = state.plans.remove(id)?;
    if let Some(plan) = &gone {
        record(state, json!({"change": "delete", "plan": plan}));
    }
    Ok(gone)
}

pub fn preview(config: &Config, plans: &[Plan], start: NaiveDateTime, hours: u32) -> Value {
    let hours = hours.clamp(1, MAX_PREVIEW_HOURS);
    let step = Duration::minutes(STEP_MIN);
    let start = start.date().and_hms_opt(0, 0, 0).unwrap_or(start) + Duration::minutes(i64::from(start.hour() * 60 + start.minute()) / STEP_MIN * STEP_MIN);
    let end = start + Duration::hours(i64::from(hours));
    let mut rooms: BTreeMap<String, Vec<Value>> = BTreeMap::new();
    let mut open: BTreeMap<String, (NaiveDateTime, f64, &'static str)> = BTreeMap::new();
    let mut at = start;
    let flush = |rooms: &mut BTreeMap<String, Vec<Value>>, room: &str, seg: (NaiveDateTime, f64, &'static str), to: NaiveDateTime| {
        rooms.entry(room.to_string()).or_default().push(json!({"from": seg.0.format(DT_PLAN).to_string(), "to": to.format(DT_PLAN).to_string(), "celsius": seg.1, "reason": seg.2}));
    };
    while at < end {
        for want in crate::rules::desired_planned(at, config, None, &Default::default(), &Default::default(), &Default::default(), None, plans) {
            let setpoint = want.setpoint.min(want.plan.as_ref().filter(|_| want.reason == "plan").map_or(config.failsafe.max_c, |h| h.ceiling(config.failsafe.max_c, config.failsafe.boost_max_c)));
            let want = crate::rules::Desired { setpoint, ..want };
            match open.get(&want.room).copied() {
                Some(seg) if seg.1 == want.setpoint && seg.2 == want.reason => {}
                Some(seg) => {
                    flush(&mut rooms, &want.room, seg, at);
                    open.insert(want.room.clone(), (at, want.setpoint, want.reason));
                }
                None => {
                    open.insert(want.room.clone(), (at, want.setpoint, want.reason));
                }
            }
        }
        at += step;
    }
    for (room, seg) in open {
        flush(&mut rooms, &room, seg, end);
    }
    json!({"rooms": rooms})
}

pub fn metrics(state: &AppState, now: NaiveDateTime) -> String {
    let plans = state.plans.list();
    let mut out = String::from("# HELP heatd_plan_active number of plans matching now per room\n# TYPE heatd_plan_active gauge\n");
    state.config.rooms.iter().for_each(|r| out.push_str(&format!("heatd_plan_active{{room=\"{}\"}} {}\n", r.name, active_in(&plans, &r.name, now).len())));
    out
}

pub fn weekday_name(day: Weekday) -> &'static str {
    ["Montag", "Dienstag", "Mittwoch", "Donnerstag", "Freitag", "Samstag", "Sonntag"][day.num_days_from_monday() as usize]
}

pub fn today_text(now: NaiveDateTime) -> String {
    format!("{}, {} {}", weekday_name(now.weekday()), now.format("%d.%m.%Y"), now.format("%H:%M"))
}
