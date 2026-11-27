use crate::config::Config;
use crate::http::AppState;
use crate::lake::{self, kind};
use crate::manual::{celsius_text, DEFAULT_HOLD_MIN};
use crate::plan::{self, Action, NewPlan, When};
use chrono::NaiveDateTime;
use serde::Deserialize;
use serde_json::{json, Value};
use std::collections::HashMap;
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

pub const MAX_TEXT: usize = 500;
pub const MAX_REPLY: usize = 300;
pub const MAX_SUMMARY: usize = 200;
const MAX_PENDING: usize = 8;
const PREVIEW_HOURS: u32 = 72;
const HOLD_SLACK_S: u64 = 3;

#[derive(Debug, Clone, Deserialize)]
#[serde(default)]
pub struct AssistantSection {
    pub enabled: bool,
    pub key_file: String,
    pub model: String,
    pub endpoint: String,
    pub timeout_s: u64,
    pub max_turns: u32,
    pub pending_ttl_s: u64,
}

impl Default for AssistantSection {
    fn default() -> Self {
        Self { enabled: false, key_file: "secrets/gemini_api_key".into(), model: "gemini-flash-latest".into(), endpoint: "https://generativelanguage.googleapis.com/v1beta".into(), timeout_s: 20, max_turns: 6, pending_ttl_s: 300 }
    }
}

#[derive(Debug)]
pub enum ModelError {
    Timeout,
    Failed(String),
}

pub trait Model: Send + Sync {
    fn generate(&self, body: &Value, timeout: Duration) -> Result<Value, ModelError>;
}

pub struct Gemini {
    key: String,
    url: String,
}

impl Gemini {
    pub fn load(cfg: &AssistantSection) -> anyhow::Result<Self> {
        let key = std::fs::read_to_string(&cfg.key_file).map_err(|e| anyhow::anyhow!("assistant key_file: {e}"))?.trim().to_string();
        if key.is_empty() {
            anyhow::bail!("assistant key_file is empty");
        }
        Ok(Self { key, url: format!("{}/models/{}:generateContent", cfg.endpoint.trim_end_matches('/'), cfg.model) })
    }
}

impl Model for Gemini {
    fn generate(&self, body: &Value, timeout: Duration) -> Result<Value, ModelError> {
        let agent: ureq::Agent = ureq::Agent::config_builder().timeout_global(Some(timeout)).build().into();
        let sent = agent.post(&self.url).header("x-goog-api-key", &self.key).header("Content-Type", "application/json").send(body.to_string().as_str());
        let mut response = sent.map_err(|e| match e {
            ureq::Error::Timeout(_) => ModelError::Timeout,
            ureq::Error::StatusCode(code) => ModelError::Failed(format!("HTTP {code}")),
            other => ModelError::Failed(other.to_string().replace(&self.key, "***")),
        })?;
        let text = response.body_mut().read_to_string().map_err(|e| match e {
            ureq::Error::Timeout(_) => ModelError::Timeout,
            other => ModelError::Failed(other.to_string()),
        })?;
        serde_json::from_str(&text).map_err(|e| ModelError::Failed(format!("keine gültige Antwort: {e}")))
    }
}

#[derive(Debug)]
pub enum Failure {
    Disabled,
    Timeout,
    Model(String),
    Unknown,
}

#[derive(Debug, Clone)]
enum Call {
    Create(NewPlan),
    Delete(String),
    SetHold { room: String, celsius: f64, hold_min: f64},
    ClearHold { room: String},
}

struct Pending {
    calls: Vec<Call>,
    expires: Instant,
}

#[derive(Clone, Default)]
pub struct Assistant {
    model: Option<Arc<dyn Model>>,
    pending: Arc<Mutex<HashMap<String, Pending>>>,
}

fn chars(text: &str, max: usize) -> String {
    text.chars().take(max).collect()
}

fn plan_schema() -> Value {
    json!({
        "type": "OBJECT",
        "description": "Ein Plan. Bei when_type=once sind from und to lokale Zeitpunkte YYYY-MM-DDTHH:MM, bei weekly sind from und to Uhrzeiten HH:MM (über Mitternacht erlaubt) und days nötig.",
        "properties": {
            "rooms": {"type": "ARRAY", "items": {"type": "STRING"}, "description": "Raumnamen oder [\"all\"]"},
            "action": {"type": "STRING", "enum": ["setpoint", "offset", "away", "frost"], "description": "setpoint = feste Temperatur, offset = Abweichung vom normalen Sollwert, away = Abwesenheitstemperatur, frost = Frostschutz"},
            "celsius": {"type": "NUMBER", "description": "nur bei setpoint, in 0,5er Schritten"},
            "k": {"type": "NUMBER", "description": "nur bei offset, -3 bis 3 in 0,5er Schritten"},
            "when_type": {"type": "STRING", "enum": ["once", "weekly"]},
            "from": {"type": "STRING"},
            "to": {"type": "STRING"},
            "days": {"type": "ARRAY", "items": {"type": "STRING"}, "description": "nur weekly: monday..sunday, weekdays, weekend"},
            "valid_from": {"type": "STRING", "description": "nur weekly, optional YYYY-MM-DD"},
            "valid_until": {"type": "STRING", "description": "nur weekly, optional YYYY-MM-DD"},
            "label": {"type": "STRING", "description": "kurzer Name, höchstens 80 Zeichen"}
       },
        "required": ["rooms", "action", "when_type", "from", "to"]
    })
}

fn declarations() -> Value {
    json!([{"functionDeclarations": [
        {"name": "get_state", "description": "Aktueller Zustand aller Räume: Temperatur, Sollwert, Fenster, aktive Pläne, Halten."},
        {"name": "list_plans", "description": "Alle gespeicherten Pläne mit id."},
        {"name": "preview_plans", "description": "Sollwerte je Raum für die nächsten Stunden, optional mit zusätzlichen noch nicht gespeicherten Plänen. Vor jedem Vorschlag benutzen.", "parameters": {"type": "OBJECT", "properties": {"plans": {"type": "ARRAY", "items": plan_schema()}, "hours": {"type": "INTEGER", "description": "1 bis 336, Standard 72"}}}},
        {"name": "create_plan", "description": "Plan anlegen. Wird erst nach Bestätigung durch den Nutzer gespeichert.", "parameters": {"type": "OBJECT", "properties": {"plan": plan_schema()}, "required": ["plan"]}},
        {"name": "delete_plan", "description": "Plan löschen. Wird erst nach Bestätigung ausgeführt.", "parameters": {"type": "OBJECT", "properties": {"id": {"type": "STRING"}}, "required": ["id"]}},
        {"name": "set_hold", "description": "Raum für hold_min Minuten auf eine feste Temperatur halten. Wird erst nach Bestätigung ausgeführt.", "parameters": {"type": "OBJECT", "properties": {"room": {"type": "STRING"}, "celsius": {"type": "NUMBER"}, "hold_min": {"type": "NUMBER", "description": "15 bis 720, Standard 120"}}, "required": ["room", "celsius"]}},
        {"name": "clear_hold", "description": "Halten in einem Raum beenden. Wird erst nach Bestätigung ausgeführt.", "parameters": {"type": "OBJECT", "properties": {"room": {"type": "STRING"}}, "required": ["room"]}}
    ]}])
}

fn system_text(config: &Config, now: NaiveDateTime) -> String {
    let rooms: Vec<&str> = config.rooms.iter().filter(|r| !r.nodes.is_empty()).map(|r| r.name.as_str()).collect();
    format!(
        "Du steuerst die Heizung eines Haushalts über die Uhr. Jetzt ist {}. Räume: {} (oder all für alle). Antworte immer auf Deutsch, sehr kurz (höchstens zwei Sätze, Uhr-Display). Zeiten sind lokale Zeit. \
Rufe vor jedem Vorschlag preview_plans mit dem geplanten Plan auf und prüfe das Ergebnis. Frage kurz nach, wenn Raum, Zeit oder Temperatur unklar sind. \
Setze nie mehr als 24 °C, außer der Nutzer nennt die Zahl selbst. Schreibende Aufrufe (create_plan, delete_plan, set_hold, clear_hold) werden nicht sofort ausgeführt, der Nutzer bestätigt sie danach; \
formuliere deshalb nie, etwas sei schon erledigt. Fehlermeldungen von Werkzeugen korrigierst du und rufst erneut auf. Wiederkehrende Wünsche sind weekly, einmalige once; Abwesenheit tagsüber ist action away.",
        plan::today_text(now),
        rooms.join(", ")
    )
}

fn plan_from_args(args: &Value) -> Result<NewPlan, String> {
    let text = |k: &str| args[k].as_str().map(str::to_string);
    let list = |k: &str| args[k].as_array().map(|a| a.iter().filter_map(|v| v.as_str().map(str::to_string)).collect::<Vec<_>>());
    let action = match args["action"].as_str() {
        Some("setpoint") => Action::Setpoint { celsius: args["celsius"].as_f64().ok_or("celsius fehlt")?},
        Some("offset") => Action::Offset { k: args["k"].as_f64().ok_or("k fehlt")?},
        Some("away") => Action::Away,
        Some("frost") => Action::Frost,
        _ => return Err("action muss setpoint, offset, away oder frost sein".into()),
    };
    let (from, to) = (text("from").ok_or("from fehlt")?, text("to").ok_or("to fehlt")?);
    let when = match args["when_type"].as_str() {
        Some("once") => When::Once { from, to},
        Some("weekly") => When::Weekly { days: list("days").ok_or("days fehlt")?, from, to, valid_from: text("valid_from"), valid_until: text("valid_until")},
        _ => return Err("when_type muss once oder weekly sein".into()),
    };
    Ok(NewPlan { rooms: list("rooms").ok_or("rooms fehlt")?, action, when, label: text("label").unwrap_or_default(), source: "assistant".into() })
}

fn state_view(state: &AppState, now: NaiveDateTime) -> Value {
    let readings = state.latest.all();
    let setpoints = state.storage.last_setpoints().unwrap_or_default();
    let windows = state.windows.lock().unwrap().open_rooms();
    let commands = state.commanded.lock().unwrap();
    let plans = state.plans.list();
    let now_ms = crate::storage::now_ms();
    let rooms: Vec<Value> = state
        .config
        .rooms
        .iter()
        .filter(|r| !r.nodes.is_empty())
        .map(|room| {
            let temp = room.nodes.iter().find_map(|n| readings.iter().find(|r| r.node == *n && r.attribute == "local_temperature")).map(|r| r.value);
            let setpoint = room.nodes.iter().find_map(|n| setpoints.iter().find(|s| s.node == *n));
            let hold = room.nodes.iter().find_map(|n| commands.hold(*n)).map(|h| json!({"celsius": h.value, "minutes_left": ((h.until_ms - now_ms) as f64 / 60_000.0).max(0.0).round()}));
            json!({"room": room.name, "temperature": temp, "setpoint": setpoint.map(|s| s.value), "window_open": windows.contains(&room.name), "plans": plan::active_in(&plans, &room.name, now).iter().map(|p| p.view()).collect::<Vec<_>>(), "hold": hold})
        })
        .collect();
    json!({"rooms": rooms})
}

fn hold_args(state: &AppState, args: &Value) -> Result<(String, f64, f64), String> {
    let room = args["room"].as_str().ok_or("room fehlt")?.to_string();
    let celsius = args["celsius"].as_f64().ok_or("celsius fehlt")?;
    let hold_min = args["hold_min"].as_f64().unwrap_or(DEFAULT_HOLD_MIN);
    crate::manual::check(state, &room, Some(celsius), Some(hold_min))?;
    Ok((room, celsius, hold_min))
}

fn describe(state: &AppState, call: &Call) -> String {
    match call {
        Call::Create(new) => {
            let stub = plan::Plan { id: String::new(), rooms: new.rooms.clone(), action: new.action.clone(), when: new.when.clone(), label: new.label.clone(), source: new.source.clone(), created_ms: 0 };
            format!("Plan: {}", plan::summary(&stub))
        }
        Call::Delete(id) => format!("Plan löschen: {}", state.plans.list().iter().find(|p| &p.id == id).map_or_else(|| id.clone(), |p| p.label.clone())),
        Call::SetHold { room, celsius, hold_min } => format!("{room} {} °C für {hold_min:.0} min halten", celsius_text(*celsius)),
        Call::ClearHold { room } => format!("Halten in {room} beenden"),
    }
}

type Outcome<'a> = (&'a str, Value, Option<&'a str>, Result<Result<Call, Value>, String>);

fn is_write(name: &str) -> bool {
    matches!(name, "create_plan" | "delete_plan" | "set_hold" | "clear_hold")
}

impl Assistant {
    pub fn with_model(model: Arc<dyn Model>) -> Self {
        Self { model: Some(model), pending: Default::default() }
    }

    pub fn live(cfg: &AssistantSection) -> Self {
        if !cfg.enabled {
            return Self::default();
        }
        match Gemini::load(cfg) {
            Ok(model) => Self::with_model(Arc::new(model)),
            Err(e) => {
                eprintln!("assistant disabled: {e:#}");
                Self::default()
            }
        }
    }

    pub fn enabled(&self) -> bool {
        self.model.is_some()
    }

    fn read_tool(&self, state: &AppState, name: &str, args: &Value, now: NaiveDateTime, now_ms: i64) -> Result<Value, String> {
        match name {
            "get_state" => Ok(state_view(state, now)),
            "list_plans" => Ok(json!({"plans": state.plans.list()})),
            "preview_plans" => {
                let mut plans = state.plans.list();
                for (i, raw) in args["plans"].as_array().into_iter().flatten().enumerate() {
                    let new = plan::validate(&state.config, &plan_from_args(raw)?, now)?;
                    plans.push(plan::Plan { id: "preview".into(), rooms: new.rooms, action: new.action, when: new.when, label: new.label, source: new.source, created_ms: now_ms + 1 + i as i64 });
                }
                let hours = args["hours"].as_u64().map_or(PREVIEW_HOURS, |h| h.clamp(1, u64::from(plan::MAX_PREVIEW_HOURS)) as u32);
                Ok(plan::preview(&state.config, &plans, now, hours))
            }
            other => Err(format!("Unbekanntes Werkzeug {other}")),
        }
    }

    fn write_tool(&self, state: &AppState, name: &str, args: &Value, now: NaiveDateTime) -> Result<Call, String> {
        match name {
            "create_plan" => Ok(Call::Create(plan::validate(&state.config, &plan_from_args(&args["plan"])?, now)?)),
            "delete_plan" => {
                let id = args["id"].as_str().ok_or("id fehlt")?;
                state.plans.list().iter().any(|p| p.id == id).then(|| Call::Delete(id.to_string())).ok_or_else(|| "Plan id unbekannt".to_string())
            }
            "set_hold" => hold_args(state, args).map(|(room, celsius, hold_min)| Call::SetHold { room, celsius, hold_min }),
            _ => {
                let room = args["room"].as_str().ok_or("room fehlt")?;
                state.config.rooms.iter().any(|r| r.name == room && !r.nodes.is_empty()).then(|| Call::ClearHold { room: room.to_string() }).ok_or_else(|| "Raum unbekannt".to_string())
            }
        }
    }

    pub fn ask(&self, state: &AppState, text: &str, now: NaiveDateTime, now_ms: i64) -> Result<Value, Failure> {
        let model = self.model.as_ref().ok_or(Failure::Disabled)?;
        let cfg = &state.config.assistant;
        let overall = Duration::from_secs((cfg.timeout_s * u64::from(cfg.max_turns.max(1))).min(state.config.http.request_timeout_s.saturating_sub(HOLD_SLACK_S)).max(1));
        let deadline = Instant::now() + overall;
        let mut contents = vec![json!({"role": "user", "parts": [{"text": text}]})];
        let mut log: Vec<Value> = Vec::new();
        let mut queued: Vec<Call> = Vec::new();
        let mut reply = String::new();
        let result = (|| -> Result<(), Failure> {
            for _ in 0..cfg.max_turns.max(1) {
                let left = deadline.checked_duration_since(Instant::now()).filter(|d| !d.is_zero()).ok_or(Failure::Timeout)?;
                let body = json!({"systemInstruction": {"parts": [{"text": system_text(&state.config, now)}]}, "contents": contents, "tools": declarations(), "generationConfig": {"temperature": 0.2}});
                let response = model.generate(&body, left.min(Duration::from_secs(cfg.timeout_s.max(1)))).map_err(|e| match e {
                    ModelError::Timeout => Failure::Timeout,
                    ModelError::Failed(m) => Failure::Model(m),
                })?;
                let content = response["candidates"][0]["content"].clone();
                let parts = content["parts"].as_array().cloned().unwrap_or_default();
                let said: String = parts.iter().filter_map(|p| p["text"].as_str()).collect::<Vec<_>>().join(" ");
                let calls: Vec<&Value> = parts.iter().filter(|p| p["functionCall"].is_object()).collect();
                if calls.is_empty() {
                    if said.trim().is_empty() && response["candidates"][0].is_null() {
                        return Err(Failure::Model("leere Antwort".into()));
                    }
                    reply = said;
                    return Ok(());
                }
                contents.push(content.clone());
                let mut outcomes: Vec<Outcome> = Vec::new();
                for part in calls {
                    let (name, args) = (part["functionCall"]["name"].as_str().unwrap_or(""), part["functionCall"]["args"].clone());
                    let outcome = if is_write(name) { self.write_tool(state, name, &args, now).map(Ok) } else { self.read_tool(state, name, &args, now, now_ms).map(Err) };
                    log.push(json!({"tool": name, "args": args, "ok": outcome.is_ok()}));
                    outcomes.push((name, args, part["functionCall"]["id"].as_str(), outcome));
                }
                let errors = outcomes.iter().any(|o| o.3.is_err());
                let mut answers = Vec::new();
                for (name, _, id, outcome) in outcomes {
                    let response = match outcome {
                        Ok(Ok(call)) if errors => {
                            drop(call);
                            json!({"error": "nicht vorgemerkt, weil ein anderer schreibender Aufruf fehlerhaft war; alle schreibenden Aufrufe korrigiert erneut senden"})
                        }
                        Ok(Ok(call)) => {
                            queued.push(call);
                            json!({"status": "vorgemerkt, wartet auf Bestätigung des Nutzers"})
                        }
                        Ok(Err(value)) => value,
                        Err(e) => json!({"error": e}),
                    };
                    let mut answer = json!({"functionResponse": {"name": name, "response": response}});
                    if let Some(id) = id {
                        answer["functionResponse"]["id"] = json!(id);
                    }
                    answers.push(answer);
                }
                contents.push(json!({"role": "user", "parts": answers}));
                if !queued.is_empty() && !errors {
                    reply = said;
                    return Ok(());
                }
            }
            reply = "Das hat zu lange gedauert, bitte anders formulieren.".into();
            Ok(())
        })();
        let outcome = match (&result, queued.is_empty()) {
            (Err(_), _) => "error",
            (_, false) => "pending",
            _ => "reply",
        };
        let mut detail = json!({"text": text, "tools": log, "outcome": outcome});
        let body = result.map(|()| {
            let reply = chars(reply.trim(), MAX_REPLY);
            if queued.is_empty() {
                return json!({"reply": if reply.is_empty() { "Dazu habe ich keine Antwort.".to_string() } else { reply }, "pending": null});
            }
            let id = format!("a{:x}", lake::now_us());
            let summary = chars(&queued.iter().map(|c| describe(state, c)).collect::<Vec<_>>().join("; "), MAX_SUMMARY);
            let mut map = self.pending.lock().unwrap();
            map.retain(|_, p| p.expires > Instant::now());
            while map.len() >= MAX_PENDING {
                let Some(oldest) = map.iter().min_by_key(|(_, p)| p.expires).map(|(k, _)| k.clone()) else { break };
                map.remove(&oldest);
            }
            map.insert(id.clone(), Pending { calls: queued.clone(), expires: Instant::now() + Duration::from_secs(cfg.pending_ttl_s) });
            detail["pending_id"] = json!(id);
            detail["summary"] = json!(summary);
            json!({"reply": if reply.is_empty() { "Bitte bestätigen.".to_string() } else { reply }, "pending": {"id": id, "summary": summary, "expires_s": cfg.pending_ttl_s}})
        });
        detail["error"] = match &body {
            Err(e) => json!(format!("{e:?}")),
            Ok(_) => Value::Null,
        };
        event(state, detail);
        body
    }

    pub fn confirm(&self, state: &AppState, id: &str, accept: bool, now: NaiveDateTime, now_ms: i64) -> Result<Value, Failure> {
        self.model.as_ref().ok_or(Failure::Disabled)?;
        let pending = self.pending.lock().unwrap().remove(id).filter(|p| p.expires > Instant::now()).ok_or(Failure::Unknown)?;
        if !accept {
            event(state, json!({"confirm": id, "accept": false}));
            return Ok(json!({"reply": "Verworfen."}));
        }
        let results: Vec<String> = pending.calls.iter().map(|call| execute(state, call, now, now_ms).unwrap_or_else(|e| format!("Fehlgeschlagen: {e}"))).collect();
        event(state, json!({"confirm": id, "accept": true, "results": results}));
        Ok(json!({"reply": chars(&results.join(" "), MAX_REPLY)}))
    }
}

fn execute(state: &AppState, call: &Call, now: NaiveDateTime, now_ms: i64) -> Result<String, String> {
    match call {
        Call::Create(new) => plan::create(state, new, now, now_ms).map(|(_, text)| format!("Eingetragen: {text}")),
        Call::Delete(id) => plan::delete(state, id)?.map(|p| format!("Gelöscht: {}", p.label)).ok_or_else(|| "Plan existiert nicht mehr".to_string()),
        Call::SetHold { room, celsius, hold_min } => {
            let reply = crate::manual::set(state, room, Some(*celsius), Some(*hold_min), Some("watch"), now_ms);
            reply.body["ok"].as_bool().filter(|ok| *ok).map(|_| format!("{room} hält {} °C für {hold_min:.0} min.", celsius_text(*celsius))).ok_or_else(|| reply.body["error"].as_str().unwrap_or("Fehler").to_string())
        }
        Call::ClearHold { room } => {
            let reply = crate::manual::clear(state, room, Some("watch"), now_ms);
            reply.body["ok"].as_bool().filter(|ok| *ok).map(|_| format!("Halten in {room} beendet.")).ok_or_else(|| reply.body["error"].as_str().unwrap_or("Fehler").to_string())
        }
    }
}

fn event(state: &AppState, detail: Value) {
    if let Err(e) = state.sink.append(lake::Event::now(kind::ASSISTANT, "heatd").with_detail(detail)) {
        eprintln!("assistant event: {e:#}");
    }
}
