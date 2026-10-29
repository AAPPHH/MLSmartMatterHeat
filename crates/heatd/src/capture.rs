use crate::http::AppState;
use crate::inventory;
use crate::lake::{self, kind, AttributeReport, Event};
use crate::status::SubState;
use crate::storage::Reading;
use matter_controller::{CaptureItem, RawAttribute, RawEvent, SOURCE_POLL, SOURCE_SUBSCRIPTION};
use serde_json::json;
use std::collections::HashMap;
use std::path::PathBuf;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;
use tokio::time::sleep;

const SOURCE_HEATD: &str = "heatd";
const STARTUP_EVENT: u32 = 0;
const SHUTDOWN_EVENT: u32 = 1;
const MAX_POLL_FACTOR_LOG2: u32 = 4;
const INVENTORY_ATTEMPTS: usize = 3;

#[derive(Clone, Copy)]
pub struct Timing {
    pub poll: Duration,
    pub settle: Duration,
    pub retry: Duration,
    pub reconnect_start: Duration,
    pub reconnect_max: Duration,
}

impl Default for Timing {
    fn default() -> Self {
        Self {
            poll: Duration::from_secs(60),
            settle: Duration::from_secs(30),
            retry: Duration::from_secs(300),
            reconnect_start: Duration::from_secs(1),
            reconnect_max: Duration::from_secs(60),
        }
    }
}

#[derive(Clone, Default)]
pub struct Latest(Arc<Mutex<HashMap<(u64, String), (f64, i64)>>>);

impl Latest {
    pub fn update(&self, node: u64, attribute: &str, value: f64, ts_ms: i64) {
        self.0.lock().unwrap().insert((node, attribute.to_string()), (value, ts_ms));
    }

    #[cfg(test)]
    pub fn get(&self, node: u64, attribute: &str) -> Option<f64> {
        self.0.lock().unwrap().get(&(node, attribute.to_string())).map(|v| v.0)
    }

    pub fn all(&self) -> Vec<Reading> {
        let mut readings: Vec<Reading> = self.0.lock().unwrap().iter().map(|((node, attribute), (value, ts))| Reading { node: *node, attribute: attribute.clone(), value: *value, ts: *ts }).collect();
        readings.sort_by(|a, b| (a.node, &a.attribute).cmp(&(b.node, &b.attribute)));
        readings
    }
}

#[derive(Debug, Default, PartialEq)]
enum Link {
    #[default]
    Unknown,
    Up,
    Lost { since_us: i64, attempts: u32 },
}

#[derive(Debug, PartialEq)]
pub enum Out {
    Attribute(AttributeReport),
    Event(Event),
    Firmware { label: String, complete: bool, changed: bool },
    Core { endpoint: u16, name: &'static str, value: f64, poll: bool },
}

pub struct NodeCapture {
    node: u64,
    latest: Latest,
    fw_str: Option<String>,
    fw_int: Option<u64>,
    link: Link,
    last_us: i64,
}

fn differs(a: &str, b: &str) -> bool {
    inventory::sanitize(a) != inventory::sanitize(b)
}

impl NodeCapture {
    pub fn new(node: u64, latest: Latest, known_fw: Option<String>) -> Self {
        Self { node, latest, fw_str: known_fw, fw_int: None, link: Link::Unknown, last_us: lake::now_us() }
    }

    pub fn fw(&self) -> String {
        self.fw_str.clone().or(self.fw_int.map(|i| i.to_string())).unwrap_or_else(|| "unknown".into())
    }

    pub fn handle(&mut self, item: CaptureItem) -> Vec<Out> {
        let now = lake::now_us();
        let mut out = Vec::new();
        match item {
            CaptureItem::Attribute(a) => {
                if a.source == SOURCE_SUBSCRIPTION {
                    self.up(&mut out, now, "report");
                }
                self.attribute(a, now, &mut out);
                self.last_us = now;
            }
            CaptureItem::Event(e) => {
                if e.source == SOURCE_SUBSCRIPTION {
                    self.up(&mut out, now, "report");
                }
                self.event(e, now, &mut out);
                self.last_us = now;
            }
            CaptureItem::SubscriptionLost { reason, backoff_s, .. } => match &mut self.link {
                Link::Lost { attempts, .. } => *attempts += 1,
                Link::Unknown => {}
                link => {
                    let silent_s = (now - self.last_us) / 1_000_000;
                    *link = Link::Lost { since_us: now, attempts: 1 };
                    out.push(Out::Event(self.lifecycle(kind::SUBSCRIPTION_LOST, now, json!({"reason": reason, "silent_s": silent_s, "backoff_s": backoff_s}))));
                }
            },
            CaptureItem::Subscribed { .. } => self.up(&mut out, now, "marker"),
        }
        out
    }

    fn lifecycle(&self, kind: &str, ts: i64, detail: serde_json::Value) -> Event {
        Event { ts, fw: Some(self.fw()), ..Event::now(kind, SOURCE_HEATD).with_node(self.node).with_detail(detail) }
    }

    fn up(&mut self, out: &mut Vec<Out>, now: i64, via: &str) {
        match std::mem::replace(&mut self.link, Link::Up) {
            Link::Lost { since_us, attempts } => {
                out.push(Out::Event(self.lifecycle(kind::RESUBSCRIBED, now, json!({"attempt": attempts, "downtime_s": (now - since_us) / 1_000_000, "via": via}))));
            }
            _ => {}
        }
    }

    fn firmware(&mut self, string: Option<String>, int: Option<u64>, now: i64, out: &mut Vec<Out>) {
        let (old_str, old_int, old_label) = (self.fw_str.clone(), self.fw_int, self.fw());
        let changed = string.as_ref().zip(old_str.as_ref()).is_some_and(|(n, o)| differs(n, o)) || int.zip(old_int).is_some_and(|(n, o)| n != o);
        self.fw_str = string.or(old_str.clone());
        self.fw_int = int.or(old_int);
        let known_same = self.fw_str.as_deref().zip(old_str.as_deref()).is_some_and(|(n, o)| !differs(n, o)) || (self.fw_str.is_none() && old_str.is_none());
        if known_same && self.fw_int == old_int {
            return;
        }
        if changed {
            let detail = json!({"old": old_label, "new": self.fw(), "old_version": old_int, "new_version": self.fw_int});
            out.push(Out::Event(self.lifecycle(kind::FW_CHANGE, now, detail)));
        }
        out.push(Out::Firmware { label: self.fw(), complete: self.fw_str.is_some(), changed });
    }

    fn attribute(&mut self, a: RawAttribute, now: i64, out: &mut Vec<Out>) {
        if a.cluster == inventory::BASIC_INFORMATION_CLUSTER && a.attribute == inventory::SOFTWARE_VERSION_STRING {
            self.firmware(a.value.v_str.clone(), None, now, out);
        } else if a.cluster == inventory::BASIC_INFORMATION_CLUSTER && a.attribute == inventory::SOFTWARE_VERSION {
            self.firmware(None, a.value.v_i64.and_then(|v| u64::try_from(v).ok()), now, out);
        }
        if let Some((name, value)) = a.core() {
            self.latest.update(self.node, name, value, now / 1000);
            out.push(Out::Core { endpoint: a.endpoint, name, value, poll: a.source == SOURCE_POLL });
        }
        out.push(Out::Attribute(AttributeReport {
            ts: now,
            mono_ns: lake::mono_ns(),
            node: self.node,
            endpoint: a.endpoint,
            cluster: a.cluster,
            attribute: a.attribute,
            dtype: a.value.dtype.into(),
            v_i64: a.value.v_i64,
            v_f64: a.value.v_f64,
            v_bool: a.value.v_bool,
            v_str: a.value.v_str,
            v_bin: None,
            value_raw: a.raw,
            source: a.source.into(),
            fw: self.fw(),
        }));
    }

    fn event(&mut self, e: RawEvent, now: i64, out: &mut Vec<Out>) {
        let basic = e.cluster == inventory::BASIC_INFORMATION_CLUSTER;
        let kind = match (basic, e.event_id) {
            (true, STARTUP_EVENT) => kind::STARTUP,
            (true, SHUTDOWN_EVENT) => kind::SHUTDOWN,
            _ => kind::MATTER_EVENT,
        };
        if kind == kind::STARTUP {
            let version = e.value.json().and_then(|v| v["SoftwareVersion"].as_u64());
            if version.is_some() {
                self.firmware(None, version, now, out);
            }
        }
        out.push(Out::Event(Event {
            ts: now,
            mono_ns: lake::mono_ns(),
            kind: kind.into(),
            node: Some(self.node),
            endpoint: Some(e.endpoint),
            cluster: Some(e.cluster),
            event_id: Some(e.event_id),
            event_number: e.number,
            priority: e.priority,
            device_ts: e.device_ts_us,
            dtype: Some(e.value.dtype.into()),
            v_i64: e.value.v_i64,
            v_f64: e.value.v_f64,
            v_bool: e.value.v_bool,
            v_str: e.value.v_str,
            value_raw: Some(e.raw),
            source: e.source.into(),
            fw: Some(self.fw()),
            ..Event::now(kind, e.source).with_detail(json!({"name": e.name, "timestamp": e.timestamp}))
        }));
    }
}

pub fn poll_delay(base: Duration, failures: u32) -> Duration {
    base * (1 << failures.min(MAX_POLL_FACTOR_LOG2))
}

fn log_error(result: anyhow::Result<()>) {
    if let Err(e) = result {
        eprintln!("lake append: {e:#}");
    }
}

fn start_inventory(state: &AppState, node: u64, busy: &Arc<AtomicBool>, timing: Timing) {
    if busy.swap(true, Ordering::SeqCst) {
        return;
    }
    let (controller, busy, dir) = (state.controller.clone(), busy.clone(), PathBuf::from(&state.config.inventory_dir));
    tokio::spawn(async move {
        for attempt in 0..INVENTORY_ATTEMPTS {
            sleep(if attempt == 0 { timing.settle } else { timing.retry }).await;
            match inventory::capture(controller.as_ref(), &dir, node).await {
                Ok((path, _)) => {
                    eprintln!("inventory node {node}: {}", path.display());
                    break;
                }
                Err(e) => eprintln!("inventory node {node} attempt {}: {e:#}", attempt + 1),
            }
        }
        busy.store(false, Ordering::SeqCst);
    });
}

fn apply(state: &AppState, node: u64, busy: &Arc<AtomicBool>, timing: Timing, out: Out) {
    match out {
        Out::Attribute(row) => {
            state.status.node_report(node, row.ts);
            if row.source == SOURCE_SUBSCRIPTION {
                state.status.subscription(node, SubState::Subscribed, row.ts);
            }
            log_error(state.sink.append(row));
        }
        Out::Event(row) => {
            match row.kind.as_str() {
                kind::SUBSCRIPTION_LOST => state.status.subscription(node, SubState::Lost, row.ts),
                kind::RESUBSCRIBED => state.status.subscription(node, SubState::Subscribed, row.ts),
                _ => {}
            }
            log_error(state.sink.append(row));
        }
        Out::Core { endpoint, name, value, poll } => crate::rules::on_report(state, &matter_controller::AttributeReport { node, endpoint, attribute: name.into(), value }, poll, crate::storage::now_ms()),
        Out::Firmware { label, complete, changed } => {
            if changed || (complete && !inventory::exists(&PathBuf::from(&state.config.inventory_dir), node, &label)) {
                start_inventory(state, node, busy, timing);
            }
        }
    }
}

async fn capture_loop(state: AppState, node: u64, busy: Arc<AtomicBool>, timing: Timing) {
    let known = inventory::latest_firmware(&PathBuf::from(&state.config.inventory_dir), node);
    let mut capture = NodeCapture::new(node, state.latest.clone(), known);
    let mut backoff = timing.reconnect_start;
    loop {
        match state.controller.capture(node).await {
            Ok(mut rx) => {
                backoff = timing.reconnect_start;
                while let Some(item) = rx.recv().await {
                    capture.handle(item).into_iter().for_each(|out| apply(&state, node, &busy, timing, out));
                }
            }
            Err(e) => eprintln!("capture node {node}: {e}"),
        }
        sleep(backoff).await;
        backoff = (backoff * 2).min(timing.reconnect_max);
    }
}

async fn poll_loop(state: AppState, node: u64, busy: Arc<AtomicBool>, timing: Timing) {
    let mut failures = 0u32;
    let silence_us = state.config.poll_silence_s as i64 * 1_000_000;
    loop {
        sleep(poll_delay(timing.poll, failures)).await;
        let silent = state.status.last_report_us(node).is_none_or(|t| lake::now_us() - t >= silence_us);
        if busy.load(Ordering::SeqCst) || !silent {
            continue;
        }
        match state.controller.poll(node).await {
            Ok(_) => failures = 0,
            Err(e) => {
                failures += 1;
                let detail = json!({"error": e.to_string(), "consecutive": failures, "next_poll_s": poll_delay(timing.poll, failures).as_secs()});
                log_error(state.sink.append(Event::now(kind::POLL_FAILED, SOURCE_HEATD).with_node(node).with_detail(detail)));
            }
        }
    }
}

pub fn spawn(state: AppState, timing: Timing) {
    for node in state.config.all_nodes() {
        let busy = Arc::new(AtomicBool::new(false));
        tokio::spawn(capture_loop(state.clone(), node, busy.clone(), timing));
        tokio::spawn(poll_loop(state.clone(), node, busy, timing));
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::Config;
    use crate::storage::Storage;
    use arrow::array::Array;
    use matter_controller::{CaptureParser, MatterController, MockController, TypedValue, SOURCE_SUBSCRIPTION};

    fn items(text: &str, node: u64) -> Vec<CaptureItem> {
        CaptureParser::new().feed_all(text, Some(node))
    }

    fn rows(outs: &[Out]) -> (Vec<&AttributeReport>, Vec<&Event>) {
        let attributes = outs.iter().filter_map(|o| if let Out::Attribute(a) = o { Some(a) } else { None }).collect();
        let events = outs.iter().filter_map(|o| if let Out::Event(e) = o { Some(e) } else { None }).collect();
        (attributes, events)
    }

    fn version(string: &str) -> CaptureItem {
        let value = TypedValue { dtype: "string", v_str: Some(string.into()), ..Default::default() };
        CaptureItem::Attribute(RawAttribute { node: 5, endpoint: 0, cluster: 0x28, attribute: 0x0A, name: "SoftwareVersionString".into(), value, raw: vec![], source: SOURCE_SUBSCRIPTION })
    }

    fn capture() -> (NodeCapture, Latest) {
        let latest = Latest::default();
        (NodeCapture::new(5, latest.clone(), None), latest)
    }

    #[test]
    fn turns_reports_into_typed_rows_and_keeps_latest_values() {
        let (mut capture, latest) = capture();
        let outs: Vec<Out> = items(include_str!("../../matter-controller/tests/fixtures/subscribe_extras.txt"), 5).into_iter().flat_map(|i| capture.handle(i)).collect();
        let (attributes, events) = rows(&outs);
        assert_eq!((attributes.len(), events.len()), (9, 0));
        let battery = attributes.iter().find(|a| a.attribute == 0x0C).unwrap();
        assert_eq!((battery.dtype.as_str(), battery.v_i64, battery.source.as_str(), battery.fw.as_str()), ("int", Some(190), "subscription", "unknown"));
        assert_eq!(battery.value_raw, b"  BatPercentRemaining: 190");
        assert_eq!(latest.get(5, "bat_percent_remaining"), Some(95.0));
        assert_eq!(latest.get(5, "pi_heating_demand"), Some(35.0));
        assert_eq!(latest.all().iter().map(|r| r.node).collect::<Vec<_>>(), vec![5; latest.all().len()]);
    }

    #[test]
    fn firmware_change_emits_event_and_requests_inventory_once() {
        let (mut capture, _) = capture();
        let first = capture.handle(version("1.0.0"));
        assert!(matches!(first[0], Out::Firmware { complete: true, changed: false, .. }));
        assert_eq!(rows(&first).1.len(), 0);
        assert_eq!(capture.handle(version("1.0.0")).iter().filter(|o| matches!(o, Out::Firmware { .. })).count(), 0);
        let second = capture.handle(version("1.1.0"));
        let (attributes, events) = rows(&second);
        assert_eq!((events[0].kind.as_str(), events[0].node), ("fw_change", Some(5)));
        assert_eq!(events[0].detail.as_deref(), Some(r#"{"new":"1.1.0","new_version":null,"old":"1.0.0","old_version":null}"#));
        assert!(matches!(&second[1], Out::Firmware { label, changed: true, .. } if label == "1.1.0"));
        assert_eq!(attributes[0].fw, "1.1.0");
    }

    #[test]
    fn known_firmware_from_disk_detects_a_change_during_downtime() {
        let mut capture = NodeCapture::new(5, Latest::default(), Some("1.0.0".into()));
        assert_eq!(capture.fw(), "1.0.0");
        assert!(matches!(capture.handle(version("1.0.0"))[0], Out::Attribute(_)));
        let outs = capture.handle(version("2.0.0"));
        assert_eq!(rows(&outs).1[0].kind, "fw_change");
    }

    #[test]
    fn device_events_become_events_and_startup_carries_the_version() {
        let (mut capture, _) = capture();
        let outs: Vec<Out> = items(include_str!("../../matter-controller/tests/fixtures/inventory_events.txt"), 5).into_iter().flat_map(|i| capture.handle(i)).collect();
        let (_, events) = rows(&outs);
        let kinds: Vec<&str> = events.iter().map(|e| e.kind.as_str()).collect();
        assert_eq!(kinds[0], "startup");
        assert!(kinds.contains(&"matter_event") && !kinds.contains(&"fw_change"));
        let startup = events[0];
        assert_eq!((startup.event_id, startup.event_number, startup.priority, startup.source.as_str(), startup.dtype.as_deref()), (Some(0), Some(1), Some(2), "subscription", Some("struct")));
        assert_eq!(startup.v_str.as_deref(), Some(r#"{"SoftwareVersion":66}"#));
        assert!(startup.value_raw.as_ref().unwrap().starts_with(b"  Event number: 1"));
        assert!(outs.iter().any(|o| matches!(o, Out::Firmware { label, complete: false, .. } if label == "66")));
        assert_eq!(capture.fw(), "66");
    }

    #[test]
    fn lifecycle_events_pair_loss_and_recovery_without_duplicates() {
        let (mut capture, _) = capture();
        let lost = || CaptureItem::SubscriptionLost { node: None, reason: "gone".into(), backoff_s: Some(1) };
        assert!(capture.handle(lost()).is_empty());
        capture.handle(CaptureItem::Subscribed { node: Some(5) });
        let first = capture.handle(lost());
        assert_eq!((rows(&first).1[0].kind.as_str(), rows(&first).1[0].node), ("subscription_lost", Some(5)));
        assert!(capture.handle(lost()).is_empty());
        let back = capture.handle(version("1.0.0"));
        let resubscribed = rows(&back).1[0];
        assert_eq!(resubscribed.kind, "resubscribed");
        let detail: serde_json::Value = serde_json::from_str(resubscribed.detail.as_ref().unwrap()).unwrap();
        assert_eq!((detail["attempt"].clone(), detail["via"].clone()), (2.into(), "report".into()));
        assert!(rows(&capture.handle(CaptureItem::Subscribed { node: Some(5) })).1.is_empty());
    }

    #[test]
    fn poll_backoff_doubles_and_is_capped() {
        let base = Duration::from_secs(60);
        let secs: Vec<u64> = (0..7).map(|f| poll_delay(base, f).as_secs()).collect();
        assert_eq!(secs, vec![60, 120, 240, 480, 960, 960, 960]);
    }

    fn state(dir: &std::path::Path, mock: Arc<MockController>) -> AppState {
        let toml = format!("poll_silence_s = 0
inventory_dir = '{0}/inventory'\n[[rooms]]\nname = 'a'\nnodes = [5]\n", dir.to_str().unwrap().replace('\\', "/"));
        let config = Config::parse(&toml).unwrap();
        let sink = lake::Sink::open(lake::LakeConfig::new(dir.join("lake"))).unwrap();
        let status = crate::status::Status::new(lake::now_us());
        AppState {
            watchmatch: crate::watchmatch::WatchMatch::new(&config.watchmatch, sink.clone(), status.clone()),
            storage: Arc::new(Storage::open(dir.join("state.duckdb").to_str().unwrap()).unwrap()),
            sink,
            latest: Latest::default(),
            config: Arc::new(config),
            model: None,
            controller: mock,
            windows: Default::default(),
            commanded: Default::default(),
            status,
            explorer: Arc::new(Mutex::new(crate::explore::Explorer::new(1))),
            tracker: Default::default(),
            batch_seq: Arc::new(std::sync::atomic::AtomicU64::new(1)),
        }
    }

    fn lake_rows(dir: &std::path::Path, table: &str) -> usize {
        let Ok(entries) = std::fs::read_dir(dir.join("lake/raw").join(table)) else { return 0 };
        entries.filter_map(|e| e.ok()).map(|e| lake::load_stream(&e.path()).unwrap().batches.iter().map(|b| b.num_rows()).sum::<usize>()).sum()
    }

    fn string_column(dir: &std::path::Path, table: &str, column: &str) -> Vec<String> {
        let Ok(entries) = std::fs::read_dir(dir.join("lake/raw").join(table)) else { return Vec::new() };
        let mut out = Vec::new();
        for entry in entries.filter_map(|e| e.ok()) {
            for batch in lake::load_stream(&entry.path()).unwrap().batches {
                let cast = arrow::compute::cast(batch.column_by_name(column).unwrap(), &arrow::datatypes::DataType::Utf8).unwrap();
                let strings = arrow::array::AsArray::as_string::<i32>(&cast);
                out.extend((0..strings.len()).map(|i| strings.value(i).to_string()));
            }
        }
        out
    }

    async fn until(what: &str, mut cond: impl FnMut() -> bool) {
        let deadline = std::time::Instant::now() + Duration::from_secs(10);
        while !cond() {
            assert!(std::time::Instant::now() < deadline, "timeout waiting for {what}");
            sleep(Duration::from_millis(20)).await;
        }
    }

    #[tokio::test]
    async fn mock_node_flows_into_the_lake_with_poll_failures_backoff_and_inventory() {
        let dir = tempfile::tempdir().unwrap();
        let mock = Arc::new(MockController::new(1));
        mock.add_node(5, 19.0, 19.0);
        let state = state(dir.path(), mock.clone());
        let timing = Timing { poll: Duration::from_millis(20), settle: Duration::from_millis(5), retry: Duration::from_millis(5), reconnect_start: Duration::from_millis(5), reconnect_max: Duration::from_millis(10) };
        spawn(state.clone(), timing);
        let column = |table: &str, name: &str| {
            state.sink.flush().unwrap();
            string_column(dir.path(), table, name)
        };
        until("inventory file", || dir.path().join("inventory/5_mock-1.json").exists()).await;
        until("poll and subscription rows", || {
            let sources = column("attribute_reports", "source");
            sources.iter().any(|s| s == "poll") && sources.iter().any(|s| s == "subscription")
        })
        .await;
        assert_eq!(state.latest.get(5, "occupied_heating_setpoint"), Some(19.0));
        assert_eq!(state.latest.get(5, "bat_percent_remaining"), Some(45.0));
        mock.set_poll_failing(true);
        until("poll_failed event", || column("events", "kind").iter().any(|k| k == "poll_failed")).await;
        mock.push_capture(5, version("mock-2"));
        until("fw_change event", || column("events", "kind").iter().any(|k| k == "fw_change")).await;
        until("new fw on rows", || column("attribute_reports", "fw").iter().any(|f| f == "mock-2")).await;
        assert!(column("attribute_reports", "fw").iter().any(|f| f == "mock-1"));
        assert!(lake_rows(dir.path(), "attribute_reports") > 10);
        mock.write_heating_setpoint(5, 22.0).await.unwrap();
        until("setpoint report", || state.latest.get(5, "occupied_heating_setpoint") == Some(22.0)).await;
    }
}
