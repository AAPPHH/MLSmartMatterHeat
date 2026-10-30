use crate::config::FailsafeSection;
use crate::http::AppState;
use crate::lake::{kind, Event};
use crate::storage::now_ms;
use anyhow::Result;
use serde::Serialize;
use serde_json::{json, Value};
use std::collections::BTreeMap;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

const MARGIN_C: f64 = 0.05;
const CAPS_FILE: &str = "caps.json";
const WATCHDOG_FILE: &str = "watchdog.json";

#[derive(Clone, Debug, Default, Serialize)]
pub struct Cap {
    pub value: Option<f64>,
    pub confirmed_ms: Option<i64>,
    pub attempt_ms: Option<i64>,
    pub error: Option<String>,
}

#[derive(Default)]
struct Inner {
    section: FailsafeSection,
    dir: Option<PathBuf>,
    caps: Mutex<BTreeMap<u64, Cap>>,
    shutdown: Mutex<Option<Value>>,
    stopping: AtomicBool,
    gate: tokio::sync::Mutex<()>,
}

#[derive(Clone, Default)]
pub struct Failsafe(Arc<Inner>);

impl Failsafe {
    pub fn new(section: &FailsafeSection, dir: Option<PathBuf>) -> Self {
        Self(Arc::new(Inner { section: section.clone(), dir, ..Default::default() }))
    }

    pub fn section(&self) -> &FailsafeSection {
        &self.0.section
    }

    pub fn max_c(&self) -> f64 {
        self.0.section.max_c
    }

    pub fn boost_max_c(&self) -> f64 {
        self.0.section.boost_max_c
    }

    pub fn stopping(&self) -> bool {
        self.0.stopping.load(Ordering::SeqCst)
    }

    pub fn cap_for(&self, target: Option<f64>) -> f64 {
        match target {
            Some(t) if t > self.0.section.max_c + MARGIN_C => self.0.section.boost_max_c,
            _ => self.0.section.max_c,
        }
    }

    pub fn cap(&self, node: u64) -> Option<Cap> {
        self.0.caps.lock().unwrap().get(&node).cloned()
    }

    pub fn view(&self) -> Value {
        let watchdog = self.0.dir.as_ref().and_then(|d| read_json(&d.join(WATCHDOG_FILE)));
        let s = &self.0.section;
        json!({"setpoint": s.setpoint, "max_c": s.max_c, "boost_max_c": s.boost_max_c, "caps": &*self.0.caps.lock().unwrap(), "last_shutdown": &*self.0.shutdown.lock().unwrap(), "watchdog": watchdog})
    }

    fn persist(&self) {
        let Some(dir) = &self.0.dir else { return };
        let write = || -> Result<()> {
            std::fs::create_dir_all(dir)?;
            let tmp = dir.join(format!("{CAPS_FILE}.tmp"));
            std::fs::write(&tmp, serde_json::to_vec_pretty(&self.view())?)?;
            Ok(std::fs::rename(&tmp, dir.join(CAPS_FILE))?)
        };
        if let Err(e) = write() {
            eprintln!("failsafe state file: {e}");
        }
    }

    fn record(&self, node: u64, value: f64, error: Option<String>) -> bool {
        let now = now_ms();
        let mut caps = self.0.caps.lock().unwrap();
        let cap = caps.entry(node).or_default();
        let changed = (error.is_none() && cap.value != Some(value)) || cap.error.is_some() != error.is_some();
        cap.attempt_ms = Some(now);
        if error.is_none() {
            cap.value = Some(value);
            cap.confirmed_ms = Some(now);
        }
        cap.error = error;
        changed
    }
}

fn read_json(path: &Path) -> Option<Value> {
    std::fs::read_to_string(path).ok().and_then(|t| serde_json::from_str(&t).ok())
}

fn log_event(state: &AppState, event: Event) {
    if let Err(e) = state.sink.append(event) {
        eprintln!("failsafe event: {e}");
    }
}

async fn write_cap(state: &AppState, node: u64, value: f64) {
    let error = state.controller.write_max_heat_limit(node, value).await.err().map(|e| e.to_string());
    let changed = state.failsafe.record(node, value, error.clone());
    state.failsafe.persist();
    if let Some(e) = &error {
        eprintln!("failsafe cap node {node} -> {value}: {e}");
    }
    if changed {
        log_event(state, Event::now(kind::FAILSAFE, "heatd").with_node(node).with_detail(json!({"action": "cap", "max_heat_setpoint_limit": value, "ok": error.is_none(), "error": error})));
    }
}

pub async fn ensure(state: &AppState, node: u64, target: &(dyn Fn() -> Option<f64> + Sync), force: bool) {
    let _gate = state.failsafe.0.gate.lock().await;
    let want = state.failsafe.cap_for(target());
    let known = state.failsafe.cap(node).and_then(|c| c.value);
    if force || known.is_none_or(|k| (k - want).abs() > MARGIN_C) {
        write_cap(state, node, want).await;
    }
}

fn due(state: &AppState, node: u64, want: f64, now: i64) -> bool {
    let section = state.failsafe.section();
    let Some(cap) = state.failsafe.cap(node) else { return true };
    let older = |t: Option<i64>, minutes: f64| t.is_none_or(|t| (now - t) as f64 >= minutes * 60_000.0);
    let mismatch = state.latest.all().iter().any(|r| r.node == node && r.attribute == "max_heat_setpoint_limit" && (r.value - want).abs() > MARGIN_C);
    let off = cap.value.is_none_or(|v| (v - want).abs() > MARGIN_C);
    ((off || mismatch) && older(cap.attempt_ms, section.retry_min)) || older(cap.confirmed_ms, section.reassert_h * 60.0)
}

pub async fn reconcile(state: &AppState) {
    for node in state.config.all_nodes() {
        let target = || state.commanded.lock().unwrap().target(node);
        if !state.failsafe.stopping() && due(state, node, state.failsafe.cap_for(target()), now_ms()) {
            ensure(state, node, &target, true).await;
        }
    }
}

pub fn spawn_background(state: AppState) {
    tokio::spawn(async move {
        let mut tick = tokio::time::interval(Duration::from_secs(state.failsafe.section().reconcile_s.max(1)));
        loop {
            tick.tick().await;
            reconcile(&state).await;
        }
    });
}

pub async fn shutdown(state: &AppState) {
    let section = state.failsafe.section().clone();
    state.failsafe.0.stopping.store(true, Ordering::SeqCst);
    let started = Instant::now();
    let deadline = started + Duration::from_secs(section.shutdown_timeout_s);
    let nodes = state.config.all_nodes();
    let mut results = Vec::new();
    for (i, &node) in nodes.iter().enumerate() {
        let node_deadline = Instant::now() + deadline.saturating_duration_since(Instant::now()) / (nodes.len() - i) as u32;
        let until = tokio::time::Instant::from_std(node_deadline);
        let (mut error, mut attempts) = (Some("not attempted".to_string()), 0);
        while error.is_some() && attempts < section.attempts.max(1) && Instant::now() < node_deadline {
            attempts += 1;
            error = match tokio::time::timeout_at(until, state.controller.write_heating_setpoint(node, section.setpoint)).await {
                Ok(Ok(())) => None,
                Ok(Err(e)) => Some(e.to_string()),
                Err(_) => Some("timeout".to_string()),
            };
        }
        if error.is_none() && state.failsafe.cap(node).and_then(|c| c.value).is_some_and(|v| (v - section.max_c).abs() > MARGIN_C) {
            let _ = tokio::time::timeout_at(until, write_cap(state, node, section.max_c)).await;
        }
        results.push(json!({"node": node, "ok": error.is_none(), "attempts": attempts, "error": error}));
    }
    let detail = json!({"action": "shutdown_write", "setpoint": section.setpoint, "elapsed_s": started.elapsed().as_secs_f64(), "nodes": results});
    *state.failsafe.0.shutdown.lock().unwrap() = Some(json!({"ts_ms": now_ms(), "detail": detail}));
    state.failsafe.persist();
    println!("failsafe shutdown: {detail}");
    log_event(state, Event::now(kind::FAILSAFE, "heatd").with_detail(detail));
}

pub fn report_section(dir: &Path) -> String {
    let mut md = String::from("\n## Failsafe\n\n");
    let Some(caps) = read_json(&dir.join(CAPS_FILE)) else {
        md.push_str("Kein Failsafe-Status vorhanden.\n");
        return md;
    };
    let when = |ms: &Value| ms.as_i64().and_then(chrono::DateTime::from_timestamp_millis).map_or("-".into(), |t| t.format("%Y-%m-%d %H:%M UTC").to_string());
    md.push_str(&format!("Ausfall-Sollwert {} C, Obergrenze (MaxHeatSetpointLimit) {} C.\n\n| Knoten | Obergrenze | zuletzt bestaetigt | Fehler |\n|---|---|---|---|\n", caps["setpoint"], caps["max_c"]));
    for (node, cap) in caps["caps"].as_object().into_iter().flatten() {
        md.push_str(&format!("| {node} | {} | {} | {} |\n", cap["value"], when(&cap["confirmed_ms"]), cap["error"].as_str().unwrap_or("-")));
    }
    if !caps["last_shutdown"].is_null() {
        md.push_str(&format!("\nLetzter Shutdown-Schreibvorgang: {} ({})\n", when(&caps["last_shutdown"]["ts_ms"]), caps["last_shutdown"]["detail"]["nodes"]));
    }
    match read_json(&dir.join(WATCHDOG_FILE)) {
        Some(w) => md.push_str(&format!("\nWatchdog zuletzt ausgeloest: {w}\n")),
        None => md.push_str("\nWatchdog: nie ausgeloest.\n"),
    }
    md
}

#[cfg(test)]
mod tests {
    use super::*;
    use matter_controller::MockController;

    fn harness(dir: &Path) -> (AppState, Arc<MockController>) {
        let mock = Arc::new(MockController::new(1));
        mock.add_node(5, 19.0, 19.0);
        let state = crate::capture::tests::state(dir, mock.clone());
        let failsafe = Failsafe::new(&state.config.failsafe, Some(dir.join("failsafe")));
        (AppState { failsafe, ..state }, mock)
    }

    fn two_nodes(state: AppState, second: u64) -> AppState {
        AppState { config: Arc::new(crate::config::Config::parse(&format!("[[rooms]]\nname = 'a'\nnodes = [5, {second}]\n")).unwrap()), ..state }
    }

    #[tokio::test]
    async fn startup_writes_the_cap_once_and_reports_it() {
        let dir = tempfile::tempdir().unwrap();
        let (state, mock) = harness(dir.path());
        reconcile(&state).await;
        reconcile(&state).await;
        assert_eq!(mock.limit_writes(), vec![(5, 22.0)]);
        assert_eq!(state.failsafe.view()["caps"]["5"]["value"], 22.0);
        assert!(std::fs::read_to_string(dir.path().join("failsafe/caps.json")).unwrap().contains("22.0"));
        assert!(report_section(&dir.path().join("failsafe")).contains("| 5 | 22.0 |"));
    }

    #[tokio::test]
    async fn boost_raises_the_cap_before_and_lowers_it_after_the_write() {
        let dir = tempfile::tempdir().unwrap();
        let (state, mock) = harness(dir.path());
        reconcile(&state).await;
        for setpoint in [20.0, 28.0, 28.0, 18.0, 8.0] {
            crate::rules::send_setpoint(&state, 5, setpoint).await.unwrap();
        }
        assert_eq!(
            mock.write_log(),
            vec![("limit", 5, 22.0), ("setpoint", 5, 20.0), ("limit", 5, 28.5), ("setpoint", 5, 28.0), ("setpoint", 5, 28.0), ("setpoint", 5, 18.0), ("limit", 5, 22.0), ("setpoint", 5, 8.0)]
        );
    }

    #[tokio::test]
    async fn rejected_cap_is_logged_and_does_not_block_setpoints() {
        let dir = tempfile::tempdir().unwrap();
        let (state, mock) = harness(dir.path());
        mock.set_limit_rejecting(true);
        reconcile(&state).await;
        crate::rules::send_setpoint(&state, 5, 8.0).await.unwrap();
        assert!(state.failsafe.cap(5).unwrap().error.unwrap().contains("0x87"));
        assert_eq!(mock.setpoint_writes(), vec![(5, 8.0)]);
        mock.set_limit_rejecting(false);
        ensure(&state, 5, &|| None, true).await;
        assert_eq!(state.failsafe.cap(5).unwrap().value, Some(22.0));
    }

    #[tokio::test]
    async fn shutdown_writes_the_failsafe_setpoint_and_stops_the_control_loop() {
        let dir = tempfile::tempdir().unwrap();
        let (state, mock) = harness(dir.path());
        mock.add_node(6, 19.0, 21.0);
        let state = two_nodes(state, 6);
        reconcile(&state).await;
        crate::rules::send_setpoint(&state, 5, 28.0).await.unwrap();
        shutdown(&state).await;
        let writes = mock.setpoint_writes();
        assert_eq!(writes[writes.len() - 2..], [(5, 18.0), (6, 18.0)]);
        assert_eq!(state.failsafe.cap(5).unwrap().value, Some(22.0));
        assert!(state.failsafe.stopping());
        let now = chrono::Local::now().naive_local();
        assert!(crate::rules::control_step(&state, now, now_ms()).await.unwrap().is_empty());
        assert_eq!(state.failsafe.view()["last_shutdown"]["detail"]["nodes"][1]["ok"], true);
        assert!(report_section(&dir.path().join("failsafe")).contains("Letzter Shutdown"));
    }

    #[tokio::test]
    async fn shutdown_retries_and_stays_bounded_when_a_node_fails() {
        let dir = tempfile::tempdir().unwrap();
        let (state, _) = harness(dir.path());
        let state = two_nodes(state, 9);
        shutdown(&state).await;
        let nodes = state.failsafe.view()["last_shutdown"]["detail"]["nodes"].clone();
        assert_eq!((nodes[0]["ok"].clone(), nodes[1]["ok"].clone(), nodes[1]["attempts"].clone()), (true.into(), false.into(), 3.into()));
    }
}
