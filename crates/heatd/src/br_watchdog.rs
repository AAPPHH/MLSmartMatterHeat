use crate::http::AppState;
use crate::lake::{self, kind};
use anyhow::{anyhow, Context, Result};
use serde::Deserialize;
use serde_json::{json, Value};
use std::sync::{Arc, Mutex};
use std::time::Duration;

#[derive(Debug, Clone, Deserialize)]
#[serde(default)]
pub struct BrWatchdogSection {
    pub enabled: bool,
    pub url: String,
    pub token_file: String,
    pub interval_s: u64,
    pub timeout_s: u64,
    pub setpoint_c: f64,
    pub http_timeout_s: u64,
    pub control_stale_s: i64,
    pub thread_silent_s: i64,
    pub down_after: u32,
}

impl Default for BrWatchdogSection {
    fn default() -> Self {
        Self {
            enabled: false,
            url: "http://192.168.178.33".into(),
            token_file: "secrets/br_watchdog_token".into(),
            interval_s: 30,
            timeout_s: 600,
            setpoint_c: 18.0,
            http_timeout_s: 5,
            control_stale_s: 120,
            thread_silent_s: 900,
            down_after: 3,
        }
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum BrState {
    Ok,
    BrHttpDown,
    ThreadDown,
    Both,
}

impl BrState {
    pub fn name(self) -> &'static str {
        match self {
            BrState::Ok => "ok",
            BrState::BrHttpDown => "br_http_down",
            BrState::ThreadDown => "thread_down",
            BrState::Both => "both",
        }
    }

    fn of(http_down: bool, thread_down: bool) -> Self {
        match (http_down, thread_down) {
            (false, false) => BrState::Ok,
            (true, false) => BrState::BrHttpDown,
            (false, true) => BrState::ThreadDown,
            (true, true) => BrState::Both,
        }
    }

    fn alert_text(self) -> &'static str {
        match self {
            BrState::Ok => "",
            BrState::BrHttpDown => "Router antwortet nicht. Ohne Herzschlag stellt er nach der Wartezeit alle Thermostate auf den Sicherheitswert.",
            BrState::ThreadDown => "Alle Thermostate sind stumm, der Router antwortet aber (Thread-Netz oder Controller pruefen).",
            BrState::Both => "Router und Thermostate sind nicht erreichbar. Heizung laeuft ohne Regelung.",
        }
    }
}

#[derive(Clone, Debug)]
pub struct BrSnapshot {
    pub state: BrState,
    pub since_us: i64,
    pub br_mode: Option<String>,
    pub br_triggered: Option<bool>,
    pub last_heartbeat_ok_us: Option<i64>,
    pub die_temperature_c: Option<f64>,
    pub heartbeats_ok: u64,
    pub heartbeats_failed: u64,
    pub alert_text: &'static str,
}

impl BrSnapshot {
    pub fn view(&self, now_us: i64) -> Value {
        let age = |t: Option<i64>| t.map(|t| ((now_us - t) as f64 / 1e6).max(0.0));
        json!({
            "state": self.state.name(),
            "since_age_s": age(Some(self.since_us)),
            "br_mode": self.br_mode,
            "br_triggered": self.br_triggered,
            "last_heartbeat_ok_age_s": age(self.last_heartbeat_ok_us),
            "die_temperature_c": self.die_temperature_c,
            "heartbeats_ok": self.heartbeats_ok,
            "heartbeats_failed": self.heartbeats_failed,
        })
    }

    pub fn alert(&self) -> Option<Value> {
        (self.state != BrState::Ok).then(|| json!({"id": format!("{}@{}", self.state.name(), self.since_us / 1000), "title": "Heizung: Router-Watchdog", "text": self.alert_text}))
    }
}

#[derive(Debug, Default)]
struct Runtime {
    status_fail: u32,
    heartbeat_fail: u32,
    heartbeats_ok: u64,
    heartbeats_failed: u64,
    last_heartbeat_ok_us: Option<i64>,
    br_mode: Option<String>,
    br_triggered: Option<bool>,
    die_temperature_c: Option<f64>,
    rewrote: bool,
    state: Option<(BrState, i64)>,
}

#[derive(Clone)]
pub struct BrWatchdog {
    cfg: BrWatchdogSection,
    nodes: Vec<u64>,
    endpoint: u16,
    valid: bool,
    agent: ureq::Agent,
    runtime: Arc<Mutex<Runtime>>,
}

fn call(agent: &ureq::Agent, method: &str, url: &str, token: Option<&str>, body: Option<String>) -> Result<Value> {
    let request = match method {
        "POST" => agent.post(url),
        _ => return fetch(agent.get(url).call(), url),
    };
    let request = request.header("Content-Type", "application/json");
    let request = match token {
        Some(t) => request.header("X-Heatd-Token", t),
        None => request,
    };
    fetch(request.send(body.unwrap_or_default().as_str()), url)
}

fn fetch(response: std::result::Result<ureq::http::Response<ureq::Body>, ureq::Error>, url: &str) -> Result<Value> {
    let mut response = response.map_err(|e| anyhow!("{url}: {e}"))?;
    let text = response.body_mut().read_to_string().map_err(|e| anyhow!("{url}: {e}"))?;
    serde_json::from_str(&text).with_context(|| format!("{url}: bad json"))
}

impl BrWatchdog {
    pub fn new(config: &crate::config::Config) -> Self {
        let cfg = config.br_watchdog.clone();
        let agent = ureq::Agent::config_builder().timeout_global(Some(Duration::from_secs(cfg.http_timeout_s))).build().into();
        let (nodes, endpoint) = (config.all_nodes(), config.chip_tool.endpoint);
        let valid = (1..=8).contains(&nodes.len()) && (1..=254).contains(&endpoint);
        if cfg.enabled && !valid {
            eprintln!("br watchdog: {} nodes / endpoint {endpoint} outside the router limits (1-8 nodes, endpoint 1-254), no heartbeats are sent", nodes.len());
        }
        Self { cfg, nodes, endpoint, valid, agent, runtime: Default::default() }
    }

    fn url(&self, path: &str) -> String {
        format!("{}{path}", self.cfg.url.trim_end_matches('/'))
    }

    fn heartbeat_body(&self, now_ms: i64) -> String {
        json!({"ts": now_ms, "timeout_s": self.cfg.timeout_s, "setpoint_c": self.cfg.setpoint_c, "nodes": self.nodes, "endpoint": self.endpoint}).to_string()
    }

    async fn blocking<T: Send + 'static>(f: impl FnOnce() -> Result<T> + Send + 'static) -> Result<T> {
        tokio::task::spawn_blocking(f).await.map_err(|e| anyhow!("join: {e}"))?
    }

    async fn poll_status(&self) -> Result<Value> {
        let (agent, url) = (self.agent.clone(), self.url("/watchdog/status"));
        Self::blocking(move || call(&agent, "GET", &url, None, None)).await
    }

    async fn send_heartbeat(&self, now_ms: i64) -> Result<Value> {
        let token = std::fs::read_to_string(&self.cfg.token_file).with_context(|| format!("read {}", self.cfg.token_file))?;
        let (agent, url, body) = (self.agent.clone(), self.url("/watchdog/heartbeat"), self.heartbeat_body(now_ms));
        let reply = Self::blocking(move || call(&agent, "POST", &url, Some(token.trim()), Some(body))).await?;
        reply["ok"].as_bool().filter(|ok| *ok).map(|_| reply.clone()).ok_or_else(|| anyhow!("heartbeat rejected: {reply}"))
    }

    pub async fn tick(&self, state: &AppState, now_us: i64) {
        let healthy = self.valid && state.status.control_healthy(now_us, self.cfg.control_stale_s * 1_000_000, self.cfg.thread_silent_s * 1_000_000);
        let polled = self.poll_status().await;
        let rewrite = {
            let mut rt = self.runtime.lock().unwrap();
            match &polled {
                Ok(v) => {
                    rt.status_fail = 0;
                    rt.br_mode = v["mode"].as_str().map(Into::into);
                    rt.br_triggered = v["triggered"].as_bool();
                    rt.die_temperature_c = v["die_temperature_c"].as_f64().filter(|t| t.is_finite());
                }
                Err(e) => {
                    rt.status_fail += 1;
                    if rt.status_fail == 1 {
                        eprintln!("br watchdog status: {e:#}");
                    }
                }
            }
            if rt.br_triggered == Some(true) && rt.br_mode.as_deref() == Some("armed") && polled.is_ok() {
                let first = healthy && !rt.rewrote;
                rt.rewrote |= healthy;
                first
            } else {
                rt.rewrote = false;
                false
            }
        };
        if rewrite {
            state.commanded.lock().unwrap().forget_desired_writes();
            let mode = self.runtime.lock().unwrap().br_mode.clone();
            eprintln!("br watchdog: router reports triggered, desired setpoints are written again");
            log("br event", state.sink.append(lake::Event::now(kind::BR_WATCHDOG, "heatd").with_detail(json!({"action": "rewrite", "br_mode": mode}))));
        }
        let die = self.runtime.lock().unwrap().die_temperature_c.filter(|_| polled.is_ok());
        if let Some(value) = die {
            log("br ambient", state.sink.append(lake::Ambient { ts: now_us, source: "br".into(), sensor: "die_c".into(), value, unit: "C".into() }));
        }
        if healthy {
            let sent = self.send_heartbeat(now_us / 1000).await;
            let mut rt = self.runtime.lock().unwrap();
            match sent {
                Ok(_) => {
                    rt.heartbeat_fail = 0;
                    rt.heartbeats_ok += 1;
                    rt.last_heartbeat_ok_us = Some(now_us);
                }
                Err(e) => {
                    rt.heartbeat_fail += 1;
                    rt.heartbeats_failed += 1;
                    if rt.heartbeat_fail == 1 {
                        eprintln!("br watchdog heartbeat: {e:#}");
                    }
                }
            }
        }
        self.evaluate(state, now_us);
    }

    fn evaluate(&self, state: &AppState, now_us: i64) {
        let silent = state.status.all_silent(now_us, self.cfg.thread_silent_s * 1_000_000);
        let mut rt = self.runtime.lock().unwrap();
        let http_down = rt.status_fail >= self.cfg.down_after || rt.heartbeat_fail >= self.cfg.down_after;
        let now = BrState::of(http_down, silent);
        let (previous, since_us) = match rt.state {
            Some((s, since)) if s == now => (s, since),
            Some((s, _)) => (s, now_us),
            None => (BrState::Ok, now_us),
        };
        if previous != now {
            eprintln!("br watchdog: {} -> {}", previous.name(), now.name());
            let detail = json!({"from": previous.name(), "to": now.name(), "status_fail": rt.status_fail, "heartbeat_fail": rt.heartbeat_fail, "thermostats_silent": silent, "br_mode": rt.br_mode, "br_triggered": rt.br_triggered});
            log("br event", state.sink.append(lake::Event::now(kind::BR_WATCHDOG, "heatd").with_detail(detail)));
        }
        rt.state = Some((now, since_us));
        let snapshot = BrSnapshot {
            state: now,
            since_us,
            br_mode: rt.br_mode.clone(),
            br_triggered: rt.br_triggered,
            last_heartbeat_ok_us: rt.last_heartbeat_ok_us,
            die_temperature_c: rt.die_temperature_c,
            heartbeats_ok: rt.heartbeats_ok,
            heartbeats_failed: rt.heartbeats_failed,
            alert_text: now.alert_text(),
        };
        drop(rt);
        state.status.set_br(snapshot);
    }
}

fn log<T>(what: &str, result: Result<T>) {
    if let Err(e) = result {
        eprintln!("{what}: {e:#}");
    }
}

pub fn spawn(state: AppState) {
    let watchdog = BrWatchdog::new(&state.config);
    let interval = Duration::from_secs(state.config.br_watchdog.interval_s.max(1));
    tokio::spawn(async move {
        let mut tick = tokio::time::interval(interval);
        tick.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
        loop {
            tick.tick().await;
            watchdog.tick(&state, lake::now_us()).await;
        }
    });
}
