use serde_json::{json, Value};
use std::collections::BTreeMap;
use std::fmt::Write;
use std::sync::{Arc, Mutex};

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum SubState {
    #[default]
    Unknown,
    Subscribed,
    #[allow(dead_code)]
    Resubscribing,
    Lost,
}

impl SubState {
    pub fn name(self) -> &'static str {
        match self {
            SubState::Unknown => "unknown",
            SubState::Subscribed => "subscribed",
            SubState::Resubscribing => "resubscribing",
            SubState::Lost => "lost",
        }
    }

    fn code(self) -> u8 {
        self as u8
    }
}

#[derive(Clone, Debug, Default)]
pub struct NodeStatus {
    pub state: SubState,
    pub since_us: i64,
    pub last_report_us: Option<i64>,
    pub reports: u64,
    pub last_subscription_us: Option<i64>,
    pub health: crate::node_health::Record,
}

const OUTDOOR_MAX_AGE_US: i64 = 6 * 3_600_000_000;
const STARTUP_GRACE_US: i64 = 600_000_000;
const BATTERY_KEEP_US: i64 = 86_400_000_000;
const BATTERY_MAX_STEP_US: i64 = 1_800_000_000;
const BATTERY_MIN_HOURS: f64 = 0.5;

#[derive(Debug, Default)]
struct WatchStats {
    inference: BTreeMap<(String, String), (u64, u64)>,
    samples: BTreeMap<String, u64>,
    battery: Vec<(i64, f64, bool)>,
    model_version: Option<String>,
}

impl WatchStats {
    fn battery_per_hour(&self) -> Option<f64> {
        let (mut drop, mut span) = (0.0, 0i64);
        for w in self.battery.windows(2) {
            let (dt, d) = (w[1].0 - w[0].0, w[0].1 - w[1].1);
            if !w[0].2 && !w[1].2 && dt > 0 && dt <= BATTERY_MAX_STEP_US {
                drop += d.max(0.0);
                span += dt;
            }
        }
        let hours = span as f64 / 3.6e9;
        (hours >= BATTERY_MIN_HOURS).then(|| drop / hours)
    }
}

#[derive(Debug, Default)]
struct Inner {
    watch: WatchStats,
    nodes: BTreeMap<u64, NodeStatus>,
    started_us: i64,
    ntp_offset_s: Option<f64>,
    weather_ok_us: Option<i64>,
    weather_rows: u64,
    outdoor: Option<(i64, f64, Option<f64>)>,
    radiation: Option<(i64, f64)>,
    last_roll: Option<(i64, bool)>,
    control_us: Option<i64>,
    br: Option<crate::br_watchdog::BrSnapshot>,
    stale_us: Option<i64>,
    restart_us: Option<i64>,
}

#[derive(Clone, Debug, Default)]
pub struct Status(Arc<Mutex<Inner>>);

impl Status {
    pub fn new(started_us: i64) -> Self {
        let status = Self::default();
        status.0.lock().unwrap().started_us = started_us;
        status
    }

    pub fn set_stale_after(&self, after_s: u64) {
        self.0.lock().unwrap().stale_us = Some(after_s as i64 * 1_000_000);
    }

    pub fn register_nodes(&self, nodes: impl IntoIterator<Item = u64>) {
        let mut inner = self.0.lock().unwrap();
        let started = inner.started_us;
        nodes.into_iter().for_each(|n| {
            inner.nodes.entry(n).or_insert_with(|| NodeStatus { since_us: started, health: crate::node_health::Record::at(started), ..Default::default() });
        });
    }

    pub fn started_us(&self) -> i64 {
        self.0.lock().unwrap().started_us
    }

    pub fn node_snapshot(&self, node: u64) -> Option<NodeStatus> {
        self.0.lock().unwrap().nodes.get(&node).cloned()
    }

    pub fn nodes_snapshot(&self) -> Vec<(u64, NodeStatus)> {
        self.0.lock().unwrap().nodes.iter().map(|(id, n)| (*id, n.clone())).collect()
    }

    pub fn update_health<R>(&self, node: u64, f: impl FnOnce(&mut crate::node_health::Record) -> R) -> R {
        f(&mut self.0.lock().unwrap().nodes.entry(node).or_default().health)
    }

    pub fn poll_result(&self, node: u64, error: Option<String>) {
        self.update_health(node, |r| match error {
            Some(e) => r.last_error = Some(format!("poll: {e}")),
            None => r.last_error = r.last_error.take().filter(|e| !e.starts_with("poll: ")),
        });
    }

    pub fn setpoint_write(&self, node: u64, error: Option<String>) {
        self.update_health(node, |r| match error {
            Some(e) => {
                r.set.failures_total += 1;
                r.set.consecutive_failures += 1;
                r.set.last_error = Some(e);
            }
            None => r.set.consecutive_failures = 0,
        });
    }

    pub fn claim_restart(&self, now_us: i64, gap_us: i64) -> bool {
        let mut inner = self.0.lock().unwrap();
        let free = inner.restart_us.is_none_or(|t| now_us - t >= gap_us);
        if free {
            inner.restart_us = Some(now_us);
        }
        free
    }

    pub fn session_restarted(&self, now_us: i64) {
        self.0.lock().unwrap().nodes.values_mut().filter(|n| n.health.state != crate::node_health::Health::Ok).for_each(|n| n.health.last_attempt_us = Some(now_us));
    }

    pub fn node_health_view(&self, node: u64, now_us: i64) -> Option<Value> {
        self.0.lock().unwrap().nodes.get(&node).map(|n| n.health.view(now_us))
    }

    pub fn node_report(&self, node: u64, ts_us: i64) {
        let mut inner = self.0.lock().unwrap();
        let n = inner.nodes.entry(node).or_default();
        n.last_report_us = Some(n.last_report_us.map_or(ts_us, |t| t.max(ts_us)));
        n.reports += 1;
    }

    pub fn subscription_report(&self, node: u64, ts_us: i64) {
        let mut inner = self.0.lock().unwrap();
        let n = inner.nodes.entry(node).or_default();
        n.last_subscription_us = Some(n.last_subscription_us.map_or(ts_us, |t| t.max(ts_us)));
    }

    pub fn subscription(&self, node: u64, state: SubState, ts_us: i64) {
        let mut inner = self.0.lock().unwrap();
        let n = inner.nodes.entry(node).or_default();
        if n.state != state {
            n.state = state;
            n.since_us = ts_us;
        }
    }

    pub fn ntp_offset(&self, offset_s: f64) {
        self.0.lock().unwrap().ntp_offset_s = Some(offset_s);
    }

    pub fn weather_fetched(&self, ts_us: i64, rows: usize) {
        let mut inner = self.0.lock().unwrap();
        inner.weather_ok_us = Some(ts_us);
        inner.weather_rows += rows as u64;
    }

    pub fn outdoor_obs(&self, ts_us: i64, temp_c: f64, wind_ms: Option<f64>) {
        self.0.lock().unwrap().outdoor = Some((ts_us, temp_c, wind_ms));
    }

    pub fn radiation_obs(&self, ts_us: i64, wm2: Option<f64>) {
        self.0.lock().unwrap().radiation = wm2.map(|v| (ts_us, v));
    }

    pub fn radiation(&self, now_us: i64) -> Option<f64> {
        self.0.lock().unwrap().radiation.filter(|o| now_us - o.0 <= OUTDOOR_MAX_AGE_US).map(|o| o.1)
    }

    pub fn weather_within(&self, now_us: i64, max_age_us: i64) -> [f64; 3] {
        let inner = self.0.lock().unwrap();
        let fresh = |ts: i64| now_us - ts <= max_age_us;
        let out = inner.outdoor.filter(|o| fresh(o.0));
        [out.map_or(f64::NAN, |o| o.1), out.and_then(|o| o.2).unwrap_or(f64::NAN), inner.radiation.filter(|o| fresh(o.0)).map_or(f64::NAN, |o| o.1)]
    }

    pub fn outdoor(&self, now_us: i64) -> Option<(f64, Option<f64>)> {
        self.0.lock().unwrap().outdoor.filter(|o| now_us - o.0 <= OUTDOOR_MAX_AGE_US).map(|o| (o.1, o.2))
    }

    pub fn watch_inference(&self, matcher: &str, state: &str, abstained: bool) {
        let mut inner = self.0.lock().unwrap();
        let e = inner.watch.inference.entry((matcher.into(), state.into())).or_default();
        e.0 += 1;
        e.1 += u64::from(abstained);
    }

    pub fn watch_samples(&self, sensor_type: &str, n: u64) {
        *self.0.lock().unwrap().watch.samples.entry(sensor_type.into()).or_default() += n;
    }

    pub fn watch_battery(&self, ts_us: i64, pct: f64, charging: bool) {
        let mut inner = self.0.lock().unwrap();
        let battery = &mut inner.watch.battery;
        let at = battery.partition_point(|b| b.0 <= ts_us);
        battery.insert(at, (ts_us, pct, charging));
        let newest = battery.last().map_or(ts_us, |b| b.0);
        let stale = battery.partition_point(|b| b.0 < newest - BATTERY_KEEP_US);
        battery.drain(..stale);
    }

    pub fn watch_model(&self, version: Option<String>) {
        self.0.lock().unwrap().watch.model_version = version;
    }

    pub fn rolled(&self, ts_us: i64, ok: bool) {
        self.0.lock().unwrap().last_roll = Some((ts_us, ok));
    }

    pub fn control_step_done(&self, ts_us: i64) {
        self.0.lock().unwrap().control_us = Some(ts_us);
    }

    pub fn control_healthy(&self, now_us: i64, stale_us: i64, silent_us: i64) -> bool {
        let inner = self.0.lock().unwrap();
        let in_grace = now_us - inner.started_us <= STARTUP_GRACE_US;
        let reachable = inner.nodes.is_empty() || inner.nodes.values().any(|n| n.state == SubState::Subscribed || (n.state == SubState::Unknown && in_grace));
        let all_silent = !inner.nodes.is_empty() && inner.nodes.values().all(|n| now_us - n.last_report_us.unwrap_or(inner.started_us) > silent_us);
        inner.control_us.is_some_and(|t| now_us - t <= stale_us) && reachable && !all_silent
    }

    pub fn all_silent(&self, now_us: i64, silent_us: i64) -> bool {
        let inner = self.0.lock().unwrap();
        !inner.nodes.is_empty() && inner.nodes.values().all(|n| now_us - n.last_report_us.unwrap_or(inner.started_us) > silent_us)
    }

    pub fn set_br(&self, snapshot: crate::br_watchdog::BrSnapshot) {
        self.0.lock().unwrap().br = Some(snapshot);
    }

    pub fn br_view(&self, now_us: i64) -> Option<Value> {
        self.0.lock().unwrap().br.as_ref().map(|b| b.view(now_us))
    }

    pub fn br_alert(&self) -> Option<Value> {
        self.0.lock().unwrap().br.as_ref().and_then(|b| b.alert())
    }

    pub fn last_report_us(&self, node: u64) -> Option<i64> {
        self.0.lock().unwrap().nodes.get(&node).and_then(|n| n.last_report_us)
    }

    pub fn health(&self, now_us: i64) -> Value {
        let inner = self.0.lock().unwrap();
        let age = |t: Option<i64>| t.map(|t| ((now_us - t) as f64 / 1e6).max(0.0));
        let nodes: Vec<Value> = inner
            .nodes
            .iter()
            .map(|(id, n)| {
                let mut node = json!({"node": id, "subscription": n.state.name(), "subscription_age_s": age(Some(n.since_us)), "last_report_age_s": age(n.last_report_us), "last_subscription_report_age_s": age(n.last_subscription_us), "polling_only": n.state == SubState::Lost && n.last_report_us.is_some_and(|t| n.last_subscription_us.is_none_or(|s| t > s)), "reports": n.reports, "health": n.health.view(now_us)});
                if let Some(limit) = inner.stale_us {
                    node["stale"] = json!(n.last_report_us.is_none_or(|t| now_us - t > limit));
                }
                node
            })
            .collect();
        let degraded = inner.nodes.values().any(|n| matches!(n.state, SubState::Lost | SubState::Resubscribing) || n.health.state != crate::node_health::Health::Ok);
        json!({
            "status": if degraded { "degraded" } else { "ok" },
            "uptime_s": age(Some(inner.started_us)),
            "nodes": nodes,
            "ntp_offset_s": inner.ntp_offset_s,
            "weather_age_s": age(inner.weather_ok_us),
            "last_roll": inner.last_roll.map(|(t, ok)| json!({"age_s": age(Some(t)), "ok": ok})),
            "watch": {
                "model_version": inner.watch.model_version,
                "abstain": inner.watch.inference.iter().map(|((m, s), (n, a))| json!({"matcher": m, "state": s, "total": n, "abstained": a, "rate": *a as f64 / (*n).max(1) as f64})).collect::<Vec<_>>(),
                "samples": inner.watch.samples,
                "battery_pct_per_h": inner.watch.battery_per_hour(),
            },
        })
    }

    pub fn metrics(&self, now_us: i64) -> String {
        let inner = self.0.lock().unwrap();
        let age = |t: i64| ((now_us - t) as f64 / 1e6).max(0.0);
        let mut out = String::new();
        let family = |out: &mut String, name: &str, help: &str, kind: &str| {
            let _ = writeln!(out, "# HELP {name} {help}\n# TYPE {name} {kind}");
        };
        family(&mut out, "heatd_uptime_seconds", "seconds since heatd started", "gauge");
        let _ = writeln!(out, "heatd_uptime_seconds {}", age(inner.started_us));
        family(&mut out, "heatd_node_subscription", "subscription state per node (0 unknown, 1 subscribed, 2 resubscribing, 3 lost)", "gauge");
        inner.nodes.iter().for_each(|(id, n)| {
            let _ = writeln!(out, "heatd_node_subscription{{node=\"{id}\",state=\"{}\"}} {}", n.state.name(), n.state.code());
        });
        family(&mut out, "heatd_node_last_report_age_seconds", "age of the last attribute report per node (absent before the first report)", "gauge");
        inner.nodes.iter().for_each(|(id, n)| {
            if let Some(t) = n.last_report_us {
                let _ = writeln!(out, "heatd_node_last_report_age_seconds{{node=\"{id}\"}} {}", age(t));
            }
        });
        if let Some(limit) = inner.stale_us {
            family(&mut out, "heatd_node_stale", "1 if the node has not reported for longer than the stale threshold", "gauge");
            inner.nodes.iter().for_each(|(id, n)| {
                let _ = writeln!(out, "heatd_node_stale{{node=\"{id}\"}} {}", u8::from(n.last_report_us.is_none_or(|t| now_us - t > limit)));
            });
        }
        family(&mut out, "heatd_node_reports_total", "attribute reports seen per node since start", "counter");
        inner.nodes.iter().for_each(|(id, n)| {
            let _ = writeln!(out, "heatd_node_reports_total{{node=\"{id}\"}} {}", n.reports);
        });
        for (name, help, kind, pick) in [
            ("heatd_node_health", "link health per node (0 ok, 1 degraded, 2 silent, 3 unreachable)", "gauge", 0),
            ("heatd_node_health_since_seconds", "seconds the node has been in its current health state", "gauge", 1),
            ("heatd_node_recovery_attempts_total", "automatic recovery attempts (resubscribe, session restart) per node since start", "counter", 2),
            ("heatd_node_health_transitions_total", "health state transitions per node since start", "counter", 3),
            ("heatd_node_setpoint_mismatch", "1 if the device reports a setpoint different from the one heatd commanded", "gauge", 4),
            ("heatd_node_setpoint_rewrites_total", "setpoint rewrites by the verification per node since start", "counter", 5),
            ("heatd_node_setpoint_failures_total", "failed setpoint writes per node since start", "counter", 6),
            ("heatd_node_alert", "1 while an alert is active for the node", "gauge", 7),
        ] {
            family(&mut out, name, help, kind);
            for (id, n) in &inner.nodes {
                let r = &n.health;
                let _ = match pick {
                    0 => writeln!(out, "{name}{{node=\"{id}\",state=\"{}\"}} {}", r.state.name(), r.state.code()),
                    1 => writeln!(out, "{name}{{node=\"{id}\"}} {}", age(r.since_us)),
                    2 => writeln!(out, "{name}{{node=\"{id}\"}} {}", r.attempts_total),
                    3 => writeln!(out, "{name}{{node=\"{id}\"}} {}", r.transitions),
                    4 => writeln!(out, "{name}{{node=\"{id}\"}} {}", u8::from(r.set.mismatch_since_us.is_some())),
                    5 => writeln!(out, "{name}{{node=\"{id}\"}} {}", r.set.rewrites_total),
                    6 => writeln!(out, "{name}{{node=\"{id}\"}} {}", r.set.failures_total),
                    _ => writeln!(out, "{name}{{node=\"{id}\",kind=\"unreachable\"}} {}\n{name}{{node=\"{id}\",kind=\"setpoint\"}} {}", u8::from(r.state == crate::node_health::Health::Unreachable), u8::from(r.setpoint_alert())),
                };
            }
        }
        if let Some(offset) = inner.ntp_offset_s {
            family(&mut out, "heatd_ntp_offset_seconds", "last measured clock offset against the NTP server", "gauge");
            let _ = writeln!(out, "heatd_ntp_offset_seconds {offset}");
        }
        if let Some(t) = inner.weather_ok_us {
            family(&mut out, "heatd_weather_last_fetch_age_seconds", "seconds since the last successful weather fetch", "gauge");
            let _ = writeln!(out, "heatd_weather_last_fetch_age_seconds {}", age(t));
        }
        family(&mut out, "heatd_weather_rows_total", "weather rows written since start", "counter");
        let _ = writeln!(out, "heatd_weather_rows_total {}", inner.weather_rows);
        if let Some((t, ok)) = inner.last_roll {
            family(&mut out, "heatd_last_roll_age_seconds", "seconds since the last roll job", "gauge");
            let _ = writeln!(out, "heatd_last_roll_age_seconds {}", age(t));
            family(&mut out, "heatd_last_roll_ok", "1 if every table of the last roll job succeeded", "gauge");
            let _ = writeln!(out, "heatd_last_roll_ok {}", u8::from(ok));
        }
        family(&mut out, "heatd_watch_model_loaded", "1 if a watch matcher model is loaded", "gauge");
        let _ = writeln!(out, "heatd_watch_model_loaded{{version=\"{}\"}} {}", inner.watch.model_version.as_deref().unwrap_or("none"), u8::from(inner.watch.model_version.is_some()));
        for (name, help, kind, pick) in [
            ("heatd_watch_inference_total", "watch matcher inferences since start", "counter", 0),
            ("heatd_watch_abstained_total", "abstained watch matcher inferences since start", "counter", 1),
            ("heatd_watch_abstain_ratio", "abstained share of watch matcher inferences since start", "gauge", 2),
        ] {
            family(&mut out, name, help, kind);
            inner.watch.inference.iter().for_each(|((m, st), (n, a))| {
                let v = match pick {
                    0 => *n as f64,
                    1 => *a as f64,
                    _ => *a as f64 / (*n).max(1) as f64,
                };
                let _ = writeln!(out, "{name}{{matcher=\"{m}\",state=\"{st}\"}} {v}");
            });
        }
        family(&mut out, "heatd_watch_samples_total", "watch samples received since start by sensor type", "counter");
        inner.watch.samples.iter().for_each(|(t, n)| {
            let _ = writeln!(out, "heatd_watch_samples_total{{sensor_type=\"{t}\"}} {n}");
        });
        if let Some(rate) = inner.watch.battery_per_hour() {
            family(&mut out, "heatd_watch_battery_percent_per_hour", "watch battery drain over the last 24 h while not charging", "gauge");
            let _ = writeln!(out, "heatd_watch_battery_percent_per_hour {rate}");
        }
        out
    }
}
