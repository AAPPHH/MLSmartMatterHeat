use crate::config::StaleSection;
use crate::http::AppState;
use crate::lake::{self, kind, Event};
use crate::rules::Check;
use crate::status::{NodeStatus, SubState};
use serde::Deserialize;
use serde_json::{json, Value};
use std::fmt::Write;
use std::time::Duration;
use tokio::task::JoinSet;

const SOURCE: &str = "heatd";
const MAX_DOUBLINGS: u32 = 16;

#[derive(Debug, Clone, Deserialize)]
#[serde(default)]
pub struct NodeHealthSection {
    pub enabled: bool,
    pub tick_s: f64,
    pub silent_min: f64,
    pub unreachable_min: f64,
    pub min_gap_s: f64,
    pub backoff_start_s: f64,
    pub backoff_max_min: f64,
    pub reset_gap_min: f64,
    pub max_restarts: u32,
    pub restart_grace_s: f64,
    pub unreachable_backoff_max_min: f64,
    pub verify_min: f64,
    pub tolerance_c: f64,
    pub alert_retries: u32,
}

impl Default for NodeHealthSection {
    fn default() -> Self {
        Self { enabled: true, tick_s: 30.0, silent_min: 10.0, unreachable_min: 30.0, min_gap_s: 60.0, backoff_start_s: 120.0, backoff_max_min: 10.0, reset_gap_min: 30.0, max_restarts: 1, restart_grace_s: 180.0, unreachable_backoff_max_min: 240.0, verify_min: 10.0, tolerance_c: 0.3, alert_retries: 3 }
    }
}

fn min_us(minutes: f64) -> i64 {
    (minutes * 60e6) as i64
}

fn s_us(seconds: f64) -> i64 {
    (seconds * 1e6) as i64
}

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, PartialOrd, Ord)]
pub enum Health {
    #[default]
    Ok,
    Degraded,
    Silent,
    Unreachable,
}

impl Health {
    pub fn name(self) -> &'static str {
        match self {
            Health::Ok => "ok",
            Health::Degraded => "degraded",
            Health::Silent => "silent",
            Health::Unreachable => "unreachable",
        }
    }

    pub fn code(self) -> u8 {
        self as u8
    }
}

#[derive(Clone, Debug, Default)]
pub struct SetpointCheck {
    pub target: Option<f64>,
    pub device: Option<f64>,
    pub mismatch_since_us: Option<i64>,
    pub rewrites: u32,
    pub failed_rewrites: u32,
    pub rewrites_total: u64,
    pub failures_total: u64,
    pub consecutive_failures: u32,
    pub next_us: i64,
    pub last_error: Option<String>,
    pub alert_since_us: Option<i64>,
}

#[derive(Clone, Debug, Default)]
pub struct Record {
    pub state: Health,
    pub since_us: i64,
    pub episode_us: i64,
    pub attempts: u32,
    pub attempts_total: u64,
    pub transitions: u64,
    pub last_attempt_us: Option<i64>,
    pub restarts: u32,
    pub last_action: Option<&'static str>,
    pub last_error: Option<String>,
    pub set: SetpointCheck,
}

fn age(now_us: i64, t: Option<i64>) -> Option<f64> {
    t.map(|t| ((now_us - t) as f64 / 1e6).max(0.0))
}

impl Record {
    pub fn at(started_us: i64) -> Self {
        Self { since_us: started_us, episode_us: started_us, ..Default::default() }
    }

    pub fn setpoint_alert(&self) -> bool {
        self.set.alert_since_us.is_some()
    }

    pub fn view(&self, now_us: i64) -> Value {
        let s = &self.set;
        json!({
            "state": self.state.name(),
            "since_ts": self.since_us / 1000,
            "since_age_s": age(now_us, Some(self.since_us)),
            "attempts": self.attempts,
            "attempts_total": self.attempts_total,
            "last_attempt_age_s": age(now_us, self.last_attempt_us),
            "last_action": self.last_action,
            "last_error": self.last_error,
            "transitions": self.transitions,
            "setpoint": {
                "mismatch": s.mismatch_since_us.is_some(),
                "mismatch_age_s": age(now_us, s.mismatch_since_us),
                "target": s.target,
                "device": s.device,
                "rewrites": s.rewrites,
                "rewrites_total": s.rewrites_total,
                "failures_total": s.failures_total,
                "consecutive_failures": s.consecutive_failures,
                "last_error": s.last_error,
                "alert": self.setpoint_alert(),
            },
        })
    }
}

pub fn classify(cfg: &NodeHealthSection, n: &NodeStatus, started_us: i64, now_us: i64) -> Health {
    let silent_for = now_us - n.last_report_us.unwrap_or(started_us);
    if silent_for >= min_us(cfg.unreachable_min) {
        Health::Unreachable
    } else if silent_for >= min_us(cfg.silent_min) {
        Health::Silent
    } else if n.state == SubState::Lost {
        Health::Degraded
    } else {
        Health::Ok
    }
}

pub fn gap_us(cfg: &NodeHealthSection, stale: &StaleSection, health: Health, attempts: u32) -> i64 {
    match (health, attempts) {
        (Health::Degraded, 0) => s_us(stale.resubscribe_after_s as f64),
        (Health::Degraded, _) => s_us(stale.resubscribe_gap_s as f64),
        (_, 0) => s_us(cfg.min_gap_s),
        (Health::Unreachable, _) => backoff_us(cfg, attempts, cfg.unreachable_backoff_max_min),
        _ => backoff_us(cfg, attempts, cfg.backoff_max_min),
    }
}

fn backoff_us(cfg: &NodeHealthSection, count: u32, max_min: f64) -> i64 {
    let grown = cfg.backoff_start_s * 2f64.powi(count.saturating_sub(1).min(MAX_DOUBLINGS) as i32);
    s_us(grown.clamp(cfg.min_gap_s, (max_min * 60.0).max(cfg.min_gap_s)))
}

fn emit(state: &AppState, node: u64, detail: Value) {
    if let Err(e) = state.sink.append(Event::now(kind::NODE_HEALTH, SOURCE).with_node(node).with_detail(detail)) {
        eprintln!("node health event: {e:#}");
    }
}

fn transition(state: &AppState, node: u64, snap: &NodeStatus, next: Health, started_us: i64, now_us: i64) {
    let silent_s = (now_us - snap.last_report_us.unwrap_or(started_us)) / 1_000_000;
    let change = state.status.update_health(node, |r| {
        if r.state == next {
            return None;
        }
        let from = r.state;
        if from == Health::Ok {
            r.episode_us = now_us;
        }
        r.state = next;
        r.since_us = now_us;
        r.transitions += 1;
        let episode_s = (now_us - r.episode_us) / 1_000_000;
        let attempts = r.attempts;
        if next == Health::Ok {
            (r.attempts, r.restarts, r.last_attempt_us) = (0, 0, None);
        }
        Some(json!({"aspect": "link", "from": from.name(), "to": next.name(), "silent_s": silent_s, "episode_s": episode_s, "attempts": attempts, "subscription": snap.state.name(), "last_action": r.last_action, "last_error": r.last_error}))
    });
    if let Some(detail) = change {
        eprintln!("node {node}: {}", detail["from"].as_str().zip(detail["to"].as_str()).map(|(a, b)| format!("{a} -> {b}")).unwrap_or_default());
        emit(state, node, detail);
    }
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum Rung {
    Resubscribe,
    Restart,
}

impl Rung {
    fn name(self) -> &'static str {
        match self {
            Rung::Resubscribe => "resubscribe",
            Rung::Restart => "restart_session",
        }
    }
}

async fn recover(state: &AppState, node: u64, health: Health, now_us: i64) {
    if health == Health::Ok {
        return;
    }
    let (cfg, stale) = (&state.config.node_health, &state.config.stale);
    if state.status.restart_grace(now_us, s_us(cfg.restart_grace_s)) {
        return;
    }
    let (due, may_restart) = state.status.update_health(node, |r| (now_us >= r.last_attempt_us.unwrap_or(r.since_us) + gap_us(cfg, stale, health, r.attempts), health == Health::Unreachable && r.restarts < cfg.max_restarts));
    if !due {
        return;
    }
    let rung = if may_restart && state.status.claim_restart(now_us, min_us(cfg.reset_gap_min)) { Rung::Restart } else { Rung::Resubscribe };
    let outcome = match rung {
        Rung::Resubscribe => state.controller.resubscribe(node).await,
        Rung::Restart => state.controller.restart_session().await,
    };
    let error = outcome.err().map(|e| e.to_string());
    state.status.update_health(node, |r| {
        r.attempts += 1;
        r.attempts_total += 1;
        r.last_attempt_us = Some(now_us);
        r.restarts += u32::from(rung == Rung::Restart);
        r.last_action = Some(rung.name());
        r.last_error = error.clone();
    });
    match &error {
        Some(e) => eprintln!("node {node} {health:?}: {} failed: {e}", rung.name()),
        None => eprintln!("node {node} {health:?}: {}", rung.name()),
    }
}

fn alerting(s: &SetpointCheck, retries: u32) -> bool {
    s.consecutive_failures >= retries || s.failed_rewrites >= retries
}

fn settle_alert(r: &mut Record, retries: u32, now_us: i64) -> Option<Value> {
    let now = alerting(&r.set, retries);
    if now == r.set.alert_since_us.is_some() {
        return None;
    }
    r.set.alert_since_us = now.then_some(now_us);
    Some(json!({"aspect": "setpoint", "alert": now, "target": r.set.target, "device": r.set.device, "rewrites": r.set.rewrites, "consecutive_failures": r.set.consecutive_failures, "last_error": r.set.last_error}))
}

async fn verify(state: &AppState, node: u64, health: Health, now_us: i64) {
    let cfg = &state.config.node_health;
    let answering = health <= Health::Degraded && !state.failsafe.stopping();
    let tolerance = cfg.tolerance_c.max(state.config.setpoint_deadband);
    let (verify_us, retries) = (min_us(cfg.verify_min), cfg.alert_retries);
    let seen = answering.then(|| state.commanded.lock().unwrap().check(node, tolerance, now_us / 1000, verify_us / 1000));
    let (rewrite, change) = state.status.update_health(node, |r| {
        let s = &mut r.set;
        let mut mismatch = None;
        match seen {
            None | Some(Check::Pending) => {}
            Some(Check::Mismatch(target, device, written_ms)) => {
                if s.target != Some(target) {
                    (s.mismatch_since_us, s.rewrites, s.failed_rewrites) = (None, 0, 0);
                }
                (s.target, s.device) = (Some(target), Some(device));
                mismatch = Some((target, device, written_ms));
            }
            Some(check) => {
                (s.target, s.device, s.rewrites, s.failed_rewrites) = (None, None, 0, 0);
                if check == Check::Match {
                    s.consecutive_failures = 0;
                }
            }
        }
        match (mismatch, seen) {
            (Some(_), _) => {
                s.mismatch_since_us.get_or_insert(now_us);
            }
            (None, Some(Check::Pending)) => {}
            (None, _) => s.mismatch_since_us = None,
        }
        let due = mismatch.filter(|_| s.mismatch_since_us.is_some_and(|t| now_us - t >= verify_us) && now_us >= s.next_us);
        let mut rewrite = None;
        if due.is_some() {
            s.failed_rewrites = s.rewrites;
            if s.rewrites < retries {
                s.rewrites += 1;
                s.rewrites_total += 1;
                rewrite = due;
            }
        }
        let change = settle_alert(r, retries, now_us);
        (rewrite, change)
    });
    if let Some(detail) = change {
        emit(state, node, detail);
    }
    let Some((target, device, written_ms)) = rewrite else { return };
    let now_ms = now_us / 1000;
    let Some(prior) = state.commanded.lock().unwrap().claim_rewrite(node, target, written_ms, device, now_ms) else {
        state.status.update_health(node, |r| (r.set.rewrites, r.set.rewrites_total) = (r.set.rewrites.saturating_sub(1), r.set.rewrites_total.saturating_sub(1)));
        return;
    };
    let outcome = crate::rules::send_setpoint(state, node, target).await;
    let error = outcome.as_ref().err().map(|e| e.to_string());
    if error.is_some() {
        state.commanded.lock().unwrap().release_rewrite(node, now_ms, prior);
    }
    let (rewrites, change) = state.status.update_health(node, |r| {
        match &error {
            Some(e) => {
                r.set.failures_total += 1;
                r.set.consecutive_failures += 1;
                r.set.last_error = Some(e.clone());
            }
            None => r.set.consecutive_failures = 0,
        }
        r.set.next_us = now_us + backoff_us(&state.config.node_health, r.set.rewrites, state.config.node_health.backoff_max_min);
        (r.set.rewrites, settle_alert(r, retries, now_us))
    });
    eprintln!("node {node}: setpoint {target} not applied (device reports {device}), rewrite {rewrites}{}", error.as_ref().map(|e| format!(" failed: {e}")).unwrap_or_default());
    emit(state, node, json!({"aspect": "setpoint", "action": "rewrite", "target": target, "device": device, "rewrites": rewrites, "error": error, "budget": "exempt_verification"}));
    if let Some(detail) = change {
        emit(state, node, detail);
    }
}

async fn step_node(state: AppState, node: u64, now_us: i64) {
    let Some(snap) = state.status.node_snapshot(node) else { return };
    let started = state.status.started_us();
    let next = classify(&state.config.node_health, &snap, started, now_us);
    transition(&state, node, &snap, next, started, now_us);
    recover(&state, node, next, now_us).await;
    verify(&state, node, next, now_us).await;
}

pub async fn step(state: &AppState, now_us: i64) {
    let mut set = JoinSet::new();
    state.config.all_nodes().into_iter().for_each(|node| {
        set.spawn(step_node(state.clone(), node, now_us));
    });
    while set.join_next().await.is_some() {}
}

pub fn spawn(state: AppState) {
    if !state.config.node_health.enabled {
        return;
    }
    let interval = Duration::from_secs_f64(state.config.node_health.tick_s.max(0.01));
    tokio::spawn(async move {
        let mut tick = tokio::time::interval(interval);
        tick.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
        loop {
            tick.tick().await;
            step(&state, lake::now_us()).await;
        }
    });
}

fn label(state: &AppState, node: u64) -> String {
    state.config.rooms.iter().find(|r| r.nodes.contains(&node)).map_or_else(|| format!("Node {node}"), |r| format!("{} (Node {node})", r.name))
}

pub fn alerts(state: &AppState) -> Vec<Value> {
    let mut out: Vec<Value> = state.status.br_alert().into_iter().collect();
    for (node, n) in state.status.nodes_snapshot() {
        let r = &n.health;
        if r.state == Health::Unreachable {
            out.push(json!({"id": format!("node_unreachable:{node}@{}", r.since_us / 1000), "title": "Heizung: Thermostat", "text": format!("{} antwortet nicht. heatd versucht automatisch, die Verbindung wiederherzustellen.", label(state, node))}));
        }
        if let Some(since) = r.set.alert_since_us {
            let (target, device) = (r.set.target.unwrap_or(f64::NAN), r.set.device);
            let reported = device.map(|d| format!(" (meldet {d:.1} Grad)")).unwrap_or_default();
            out.push(json!({"id": format!("setpoint_not_applied:{node}@{}", since / 1000), "title": "Heizung: Thermostat", "text": format!("{}: Solltemperatur {target:.1} Grad wird nicht uebernommen{reported}. heatd schreibt sie erneut.", label(state, node))}));
        }
    }
    out.sort_by_key(|a| a["id"].as_str().and_then(|id| id.rsplit_once('@')).and_then(|(_, since)| since.parse::<i64>().ok()).unwrap_or(i64::MAX));
    out
}

pub fn report_section(events: &[(i64, Option<String>, Option<String>)]) -> String {
    if events.is_empty() {
        return "Keine Stoerungen, alle Thermostate durchgehend erreichbar, Sollwerte uebernommen.\n".into();
    }
    let mut out = String::from("| Node | Wechsel | schlechtester Zustand | Stoerzeit | Neuschreiben | Sollwert-Alarme |\n|---|---|---|---|---|---|\n");
    let parsed: Vec<(i64, String, Value)> = events.iter().map(|(us, node, detail)| (*us, node.clone().unwrap_or_default(), detail.as_deref().and_then(|d| serde_json::from_str(d).ok()).unwrap_or(Value::Null))).collect();
    let mut nodes: Vec<&String> = parsed.iter().map(|e| &e.1).collect();
    nodes.sort();
    nodes.dedup();
    for node in nodes {
        let own = || parsed.iter().filter(|e| &e.1 == node).map(|e| &e.2);
        let changes = own().filter(|d| d["aspect"] == "link").count();
        let worst = own().filter_map(|d| d["to"].as_str()).filter_map(|s| [Health::Degraded, Health::Silent, Health::Unreachable].into_iter().find(|h| h.name() == s)).max().map_or("ok", Health::name);
        let down_s: i64 = own().filter(|d| d["aspect"] == "link" && d["to"] == "ok").filter_map(|d| d["episode_s"].as_i64()).sum();
        let rewrites = own().filter(|d| d["action"] == "rewrite").count();
        let alarms = own().filter(|d| d["aspect"] == "setpoint" && d["alert"] == true).count();
        let _ = writeln!(out, "| {node} | {changes} | {worst} | {} | {rewrites} | {alarms} |", crate::report::dur(down_s as f64));
    }
    out.push('\n');
    for (us, node, detail) in parsed.iter().take(50) {
        let _ = writeln!(out, "- {} Node {node}: {}", crate::report::hhmm(*us), line(detail));
    }
    out
}

fn line(v: &Value) -> String {
    let text = |k: &str| v[k].as_str().unwrap_or("?").to_string();
    match v["aspect"].as_str() {
        Some("setpoint") if v["action"].is_string() => format!("Sollwert {} nicht uebernommen (Thermostat meldet {}), Neuschreiben {}", v["target"], v["device"], v["rewrites"]),
        Some("setpoint") => format!("Sollwert-Alarm {}", if v["alert"] == true { "an" } else { "aus" }),
        _ => format!("{} -> {} (stumm seit {} s, Versuche {}, Stoerzeit {} s)", text("from"), text("to"), v["silent_s"], v["attempts"], v["episode_s"]),
    }
}
