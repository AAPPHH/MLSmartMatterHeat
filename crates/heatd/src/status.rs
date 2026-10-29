use serde_json::{json, Value};
use std::collections::BTreeMap;
use std::fmt::Write;
use std::sync::{Arc, Mutex};

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum SubState {
    #[default]
    Unknown,
    Subscribed,
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
}

#[derive(Debug, Default)]
struct Inner {
    nodes: BTreeMap<u64, NodeStatus>,
    started_us: i64,
    ntp_offset_s: Option<f64>,
    weather_ok_us: Option<i64>,
    weather_rows: u64,
    last_roll: Option<(i64, bool)>,
}

#[derive(Clone, Debug, Default)]
pub struct Status(Arc<Mutex<Inner>>);

impl Status {
    pub fn new(started_us: i64) -> Self {
        let status = Self::default();
        status.0.lock().unwrap().started_us = started_us;
        status
    }

    pub fn register_nodes(&self, nodes: impl IntoIterator<Item = u64>) {
        let mut inner = self.0.lock().unwrap();
        let started = inner.started_us;
        nodes.into_iter().for_each(|n| {
            inner.nodes.entry(n).or_insert_with(|| NodeStatus { since_us: started, ..Default::default() });
        });
    }

    pub fn node_report(&self, node: u64, ts_us: i64) {
        let mut inner = self.0.lock().unwrap();
        let n = inner.nodes.entry(node).or_default();
        n.last_report_us = Some(n.last_report_us.map_or(ts_us, |t| t.max(ts_us)));
        n.reports += 1;
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

    pub fn rolled(&self, ts_us: i64, ok: bool) {
        self.0.lock().unwrap().last_roll = Some((ts_us, ok));
    }

    #[cfg(test)]
    pub fn node(&self, node: u64) -> Option<NodeStatus> {
        self.0.lock().unwrap().nodes.get(&node).cloned()
    }

    pub fn health(&self, now_us: i64) -> Value {
        let inner = self.0.lock().unwrap();
        let age = |t: Option<i64>| t.map(|t| ((now_us - t) as f64 / 1e6).max(0.0));
        let nodes: Vec<Value> = inner
            .nodes
            .iter()
            .map(|(id, n)| json!({"node": id, "subscription": n.state.name(), "subscription_age_s": age(Some(n.since_us)), "last_report_age_s": age(n.last_report_us), "reports": n.reports}))
            .collect();
        let degraded = inner.nodes.values().any(|n| matches!(n.state, SubState::Lost | SubState::Resubscribing));
        json!({
            "status": if degraded { "degraded" } else { "ok" },
            "uptime_s": age(Some(inner.started_us)),
            "nodes": nodes,
            "ntp_offset_s": inner.ntp_offset_s,
            "weather_age_s": age(inner.weather_ok_us),
            "last_roll": inner.last_roll.map(|(t, ok)| json!({"age_s": age(Some(t)), "ok": ok})),
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
        family(&mut out, "heatd_node_reports_total", "attribute reports seen per node since start", "counter");
        inner.nodes.iter().for_each(|(id, n)| {
            let _ = writeln!(out, "heatd_node_reports_total{{node=\"{id}\"}} {}", n.reports);
        });
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
        out
    }
}
