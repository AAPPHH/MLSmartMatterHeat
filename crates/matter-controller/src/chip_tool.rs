use crate::capture::{CaptureItem, CaptureParser, SOURCE_POLL};
use crate::{
    centi_degrees, parse_number, AttributeReport, AttributeSpec, Error, Inventory, MatterController, NodeId, Result, ThermostatState, EXTRA_SUBSCRIPTIONS,
    NEIGHBOR_TABLE, OCCUPIED_HEATING_SETPOINT, POLL_ATTRIBUTES, SYSTEM_MODE, THREAD_DIAGNOSTICS_CLUSTER, THREAD_PARENT_LAST_RSSI, THREAD_PARENT_LQI, THREAD_PARENT_RSSI, WATCHED,
};
use async_trait::async_trait;
use std::collections::HashSet;
use std::fs::{File, OpenOptions, TryLockError};
use std::path::PathBuf;
use std::process::Stdio;
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};
use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};
use tokio::process::{Child, ChildStdin, Command};
use tokio::sync::mpsc::{channel, Receiver, Sender};
use tokio::sync::{oneshot, watch};
use tokio::time::{sleep, timeout};

const BACKOFF_START: Duration = Duration::from_secs(1);
const BACKOFF_MAX: Duration = Duration::from_secs(60);
const HEALTHY_AFTER: Duration = Duration::from_secs(60);
const COMMISSION_MARGIN_S: u64 = 15;
const LOCK_FILE: &str = "heatd.lock";
const INVENTORY_TIMEOUT_FACTOR: u64 = 3;
const INVENTORY_MARGIN_S: u64 = 30;
const PAUSE_WAIT: Duration = Duration::from_secs(15);
const WILDCARD: &str = "0xFFFFFFFF";
const WILDCARD_ENDPOINT: &str = "0xFFFF";
const CAPTURE_QUEUE: usize = 4096;
const POLL_TIMEOUT: Duration = Duration::from_secs(30);
const POLL_GRACE: Duration = Duration::from_secs(2);
const WRITE_ATTEMPTS: u32 = 3;
const WRITE_BACKOFF: Duration = Duration::from_secs(5);

pub fn strip_ansi(text: &str) -> String {
    let mut out = String::new();
    let mut chars = text.chars();
    while let Some(c) = chars.next() {
        if c == '\x1b' {
            for n in chars.by_ref() {
                if n.is_ascii_alphabetic() {
                    break;
                }
            }
        } else {
            out.push(c);
        }
    }
    out
}

pub(crate) fn clean(line: &str) -> String {
    strip_ansi(line).trim_end().to_string()
}

#[derive(Debug, Clone)]
pub struct ChipToolConfig {
    pub binary: String,
    pub storage_dir: Option<String>,
    pub paa_trust_store: Option<String>,
    pub bypass_attestation: bool,
    pub endpoint: u16,
    pub subscribe_min_s: u32,
    pub subscribe_max_s: u32,
    pub timeout_s: u64,
    pub commission_timeout_s: u64,
}

impl Default for ChipToolConfig {
    fn default() -> Self {
        Self {
            binary: "chip-tool".into(),
            storage_dir: None,
            paa_trust_store: Some("/home/aapphh/snap/chip-tool/common/paa-root-certs".into()),
            bypass_attestation: false,
            endpoint: 1,
            subscribe_min_s: 30,
            subscribe_max_s: 300,
            timeout_s: 90,
            commission_timeout_s: 180,
        }
    }
}

#[derive(Debug)]
pub struct StorageLock {
    _file: File,
}

impl ChipToolConfig {
    pub fn args(&self, parts: &[String]) -> Vec<String> {
        let mut args = parts.to_vec();
        if let Some(dir) = &self.storage_dir {
            args.push("--storage-directory".into());
            args.push(dir.clone());
        }
        if let Some(store) = &self.paa_trust_store {
            args.push("--paa-trust-store-path".into());
            args.push(store.clone());
        }
        args
    }

    pub fn commission_args(&self, node: NodeId, pairing_code: &str, dataset_hex: &str) -> Vec<String> {
        let mut parts = strings(&["pairing", "code-thread", &node.to_string(), &format!("hex:{}", dataset_hex.trim()), pairing_code]);
        parts.extend(strings(&["--timeout", &self.commission_timeout_s.to_string()]));
        if self.bypass_attestation {
            parts.extend(strings(&["--bypass-attestation-verifier", "1"]));
        }
        self.args(&parts)
    }

    pub fn read_args(&self, spec: &AttributeSpec, node: NodeId) -> Vec<String> {
        self.args(&strings(&["thermostat", "read", spec.kebab, &node.to_string(), &self.endpoint.to_string()]))
    }

    fn write_parts(&self, spec: &AttributeSpec, value: i64, node: NodeId) -> Vec<String> {
        strings(&["thermostat", "write", spec.kebab, &value.to_string(), &node.to_string(), &self.endpoint.to_string()])
    }

    fn write_setpoint_parts(&self, node: NodeId, celsius: f64) -> Vec<String> {
        self.write_parts(&OCCUPIED_HEATING_SETPOINT, centi_degrees(celsius).into(), node)
    }

    fn write_system_mode_parts(&self, node: NodeId, mode: u8) -> Vec<String> {
        self.write_parts(&SYSTEM_MODE, mode.into(), node)
    }

    pub fn write_setpoint_args(&self, node: NodeId, celsius: f64) -> Vec<String> {
        self.args(&self.write_setpoint_parts(node, celsius))
    }

    pub fn write_system_mode_args(&self, node: NodeId, mode: u8) -> Vec<String> {
        self.args(&self.write_system_mode_parts(node, mode))
    }

    pub fn interactive_args(&self) -> Vec<String> {
        self.args(&strings(&["interactive", "start"]))
    }

    pub fn subscribe_line(&self, node: NodeId) -> String {
        self.wildcard_line("subscribe-by-id", node)
    }

    pub fn event_subscribe_line(&self, node: NodeId) -> String {
        self.wildcard_line("subscribe-event-by-id", node)
    }

    fn wildcard_line(&self, command: &str, node: NodeId) -> String {
        format!(
            "any {command} {WILDCARD} {WILDCARD} {} {} {node} {WILDCARD_ENDPOINT} --keepSubscriptions 1 --auto-resubscribe 1",
            self.subscribe_min_s, self.subscribe_max_s
        )
    }

    pub fn subscribe_lines(&self, node: NodeId) -> Vec<String> {
        vec![self.subscribe_line(node), self.event_subscribe_line(node)]
    }

    pub fn poll_line(&self, node: NodeId) -> String {
        let hex = |f: fn(&(u32, u32)) -> u32| POLL_ATTRIBUTES.iter().map(|p| format!("0x{:04X}", f(p))).collect::<Vec<_>>().join(",");
        let endpoints = vec![WILDCARD_ENDPOINT; POLL_ATTRIBUTES.len()].join(",");
        format!("any read-by-id {} {} {node} {endpoints}", hex(|p| p.0), hex(|p| p.1))
    }

    pub fn inventory_limit_s(&self) -> u64 {
        self.timeout_s * INVENTORY_TIMEOUT_FACTOR
    }

    fn inventory_args(&self, command: &str, node: NodeId) -> Vec<String> {
        let wait = self.inventory_limit_s().saturating_sub(INVENTORY_MARGIN_S).max(INVENTORY_MARGIN_S);
        self.args(&strings(&["any", command, WILDCARD, WILDCARD, &node.to_string(), WILDCARD_ENDPOINT, "--timeout", &wait.to_string()]))
    }

    pub fn inventory_attribute_args(&self, node: NodeId) -> Vec<String> {
        self.inventory_args("read-by-id", node)
    }

    pub fn inventory_event_args(&self, node: NodeId) -> Vec<String> {
        self.inventory_args("read-event-by-id", node)
    }

    pub fn lock_path(&self) -> PathBuf {
        let dir = self.storage_dir.as_ref().map(PathBuf::from).unwrap_or_else(|| {
            std::env::var_os("SNAP_USER_COMMON")
                .map(PathBuf::from)
                .or_else(|| std::env::var_os("HOME").map(|h| PathBuf::from(h).join("snap/chip-tool/common")))
                .unwrap_or_else(std::env::temp_dir)
        });
        dir.join(LOCK_FILE)
    }

    pub fn lock_storage(&self) -> Result<StorageLock> {
        let path = self.lock_path();
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent)?;
        }
        let file = OpenOptions::new().create(true).truncate(false).write(true).open(&path)?;
        match file.try_lock() {
            Ok(()) => Ok(StorageLock { _file: file }),
            Err(TryLockError::WouldBlock) => Err(Error::Command(format!("chip-tool storage is in use (lock {}): stop heatd first", path.display()))),
            Err(TryLockError::Error(e)) => Err(e.into()),
        }
    }
}

fn strings(parts: &[&str]) -> Vec<String> {
    parts.iter().map(|s| s.to_string()).collect()
}

pub(crate) fn payload(line: &str) -> Option<&str> {
    ["[TOO]", "CHIP:TOO:"].iter().find_map(|marker| line.find(marker).map(|i| &line[i + marker.len()..]))
}

fn snake_case(pascal: &str) -> String {
    let mut out = String::new();
    for (i, c) in pascal.chars().enumerate() {
        if c.is_ascii_uppercase() {
            if i > 0 {
                out.push('_');
            }
            out.push(c.to_ascii_lowercase());
        } else {
            out.push(c);
        }
    }
    out
}

fn parse_value(text: &str) -> Option<f64> {
    match text.trim() {
        t if t.eq_ignore_ascii_case("true") => Some(1.0),
        t if t.eq_ignore_ascii_case("false") => Some(0.0),
        t => t.strip_prefix("0x").map_or_else(|| t.parse::<f64>().ok(), |h| i64::from_str_radix(h, 16).ok().map(|v| v as f64)),
    }
}

fn status_code(line: &str, marker: &str) -> Option<u32> {
    let rest = line.split_once(marker)?.1;
    let hex: String = rest.chars().take_while(char::is_ascii_hexdigit).collect();
    u32::from_str_radix(&hex, 16).ok()
}

pub(crate) fn is_failure(line: &str) -> bool {
    let body = payload(line).unwrap_or(line).trim();
    body.starts_with("Run command failure")
        || body.starts_with("Response Failure")
        || body.starts_with("Error:")
        || body.contains("commissioning Failure")
        || body.contains("Init Storage failure")
        || status_code(line, "Status=0x").is_some_and(|c| c != 0)
        || status_code(line, "status = 0x").is_some_and(|c| c != 0)
}

pub(crate) fn rx_peer(line: &str) -> Option<NodeId> {
    let token = line.split_once("Msg RX from ")?.1.split_whitespace().next()?;
    NodeId::from_str_radix(token.split_once(':')?.1, 16).ok()
}

fn spec_for(name: &str) -> Option<&'static AttributeSpec> {
    WATCHED.iter().copied().chain(EXTRA_SUBSCRIPTIONS.iter().flat_map(|g| g.attributes.iter().copied())).find(|s| s.pascal == name)
}

#[derive(Default)]
struct Neighbor {
    lqi: Option<f64>,
    average_rssi: Option<f64>,
    last_rssi: Option<f64>,
    is_child: Option<bool>,
}

pub struct ReportParser {
    node: NodeId,
    endpoint: u16,
    cluster: u32,
    attribute: u32,
    neighbor: Option<Neighbor>,
}

impl ReportParser {
    pub fn new(node: NodeId, default_endpoint: u16) -> Self {
        Self { node, endpoint: default_endpoint, cluster: 0, attribute: u32::MAX, neighbor: None }
    }

    pub fn feed(&mut self, raw: &str) -> Option<AttributeReport> {
        self.feed_all(raw).into_iter().next()
    }

    pub fn feed_all(&mut self, raw: &str) -> Vec<AttributeReport> {
        let line = clean(raw);
        let Some(body) = payload(&line) else { return Vec::new() };
        if let Some(rest) = body.trim_start().strip_prefix("Endpoint:") {
            self.header(rest);
            return Vec::new();
        }
        if self.cluster == THREAD_DIAGNOSTICS_CLUSTER && self.attribute == NEIGHBOR_TABLE {
            return self.neighbor_line(body.trim());
        }
        self.attribute_line(body).into_iter().collect()
    }

    fn header(&mut self, rest: &str) {
        let tokens: Vec<&str> = rest.split_whitespace().collect();
        if let Some(ep) = tokens.first().and_then(|t| t.parse().ok()) {
            self.endpoint = ep;
        }
        let id_after = |key: &str| {
            let at = tokens.iter().position(|t| *t == key)?;
            u32::try_from(parse_number(tokens.get(at + 1)?)?).ok()
        };
        self.cluster = id_after("Cluster:").unwrap_or(0);
        self.attribute = id_after("Attribute").unwrap_or(u32::MAX);
        self.neighbor = None;
    }

    fn attribute_line(&self, body: &str) -> Option<AttributeReport> {
        if !body.starts_with("  ") {
            return None;
        }
        let (name, value) = body.trim().split_once(": ")?;
        if name.is_empty() || !name.chars().all(|c| c.is_ascii_alphanumeric()) {
            return None;
        }
        let raw = parse_value(value)?;
        let known = spec_for(name);
        Some(AttributeReport {
            node: self.node,
            endpoint: self.endpoint,
            attribute: known.map_or_else(|| snake_case(name), |s| s.snake.to_string()),
            value: raw / known.map_or(1.0, |s| s.scale),
        })
    }

    fn neighbor_line(&mut self, text: &str) -> Vec<AttributeReport> {
        if text.starts_with('[') && text.ends_with('{') {
            self.neighbor = Some(Neighbor::default());
            return Vec::new();
        }
        if text == "}" {
            let Some(n) = self.neighbor.take().filter(|n| n.is_child != Some(true)) else { return Vec::new() };
            let report = |attribute: &str, value: Option<f64>| value.map(|value| AttributeReport { node: self.node, endpoint: self.endpoint, attribute: attribute.to_string(), value });
            return [report(THREAD_PARENT_RSSI, n.average_rssi), report(THREAD_PARENT_LAST_RSSI, n.last_rssi), report(THREAD_PARENT_LQI, n.lqi)].into_iter().flatten().collect();
        }
        if let (Some(n), Some((key, value))) = (self.neighbor.as_mut(), text.split_once(": ")) {
            match key {
                "Lqi" => n.lqi = parse_value(value),
                "AverageRssi" => n.average_rssi = parse_value(value),
                "LastRssi" => n.last_rssi = parse_value(value),
                "IsChild" => n.is_child = parse_value(value).map(|v| v != 0.0),
                _ => {}
            }
        }
        Vec::new()
    }
}

pub fn parse_reports(output: &str, node: NodeId, default_endpoint: u16) -> Vec<AttributeReport> {
    let mut parser = ReportParser::new(node, default_endpoint);
    output.lines().flat_map(|l| parser.feed_all(l)).collect()
}

#[derive(Debug, PartialEq)]
pub enum StreamEvent {
    Report(AttributeReport),
    Write(std::result::Result<(), String>),
    Failure(String),
}

pub struct StreamParser {
    reports: ReportParser,
    peer: Option<NodeId>,
    in_write: bool,
}

impl StreamParser {
    pub fn new(default_endpoint: u16) -> Self {
        Self { reports: ReportParser::new(0, default_endpoint), peer: None, in_write: false }
    }

    pub fn feed(&mut self, raw: &str, sole_node: Option<NodeId>, writing: bool) -> Vec<StreamEvent> {
        let line = clean(raw);
        if let Some(peer) = rx_peer(&line) {
            self.peer = Some(peer);
            return Vec::new();
        }
        if line.contains("WriteResponseMessage") {
            self.in_write = true;
            return Vec::new();
        }
        if self.in_write {
            if let Some(code) = status_code(&line, "status = 0x") {
                self.in_write = false;
                return vec![StreamEvent::Write(if code == 0 { Ok(()) } else { Err(line.trim().to_string()) })];
            }
        }
        if is_failure(&line) {
            let message = line.trim().to_string();
            let ours = self.in_write || (writing && payload(&line).unwrap_or(&line).trim().starts_with("Run command failure"));
            self.in_write = false;
            return vec![if ours { StreamEvent::Write(Err(message)) } else { StreamEvent::Failure(message) }];
        }
        let node = self.peer.or(sole_node);
        self.reports.node = node.unwrap_or(0);
        let reports = self.reports.feed_all(&line);
        if node.is_some() {
            reports.into_iter().map(StreamEvent::Report).collect()
        } else {
            Vec::new()
        }
    }
}

pub fn check_output(output: &str) -> Result<()> {
    match output.lines().map(clean).find(|l| is_failure(l)) {
        Some(line) => Err(Error::Command(payload(&line).unwrap_or(&line).trim().to_string())),
        None => Ok(()),
    }
}

pub fn check_commissioned(output: &str) -> Result<()> {
    check_output(output)?;
    if output.contains("Device commissioning completed with success") {
        Ok(())
    } else {
        Err(Error::Command("commissioning did not report success".into()))
    }
}

fn output_tail(output: &str, lines: usize) -> String {
    let all: Vec<String> = output.lines().map(clean).filter(|l| !l.trim().is_empty()).collect();
    all[all.len().saturating_sub(lines)..].join("\n")
}

struct PollWindow {
    node: NodeId,
    paths: HashSet<(u32, u32)>,
    done: Sender<()>,
}

#[derive(Default)]
struct Hub {
    subs: Mutex<Vec<(NodeId, Sender<AttributeReport>)>>,
    captures: Mutex<Vec<(NodeId, Sender<CaptureItem>)>>,
    sent: Mutex<HashSet<NodeId>>,
    poll: Mutex<Option<PollWindow>>,
    pending: Mutex<Option<oneshot::Sender<std::result::Result<(), String>>>>,
    logged: Mutex<HashSet<String>>,
}

impl Hub {
    fn nodes(&self) -> Vec<NodeId> {
        let mut nodes: Vec<NodeId> = self.subs.lock().unwrap().iter().map(|(n, _)| *n).chain(self.captures.lock().unwrap().iter().map(|(n, _)| *n)).collect();
        nodes.sort_unstable();
        nodes.dedup();
        nodes
    }

    fn sole_node(&self) -> Option<NodeId> {
        match self.nodes().as_slice() {
            [node] => Some(*node),
            _ => None,
        }
    }

    fn finish_write(&self, result: std::result::Result<(), String>) -> Option<String> {
        match self.pending.lock().unwrap().take() {
            Some(tx) => {
                let _ = tx.send(result);
                None
            }
            None => result.err(),
        }
    }

    fn log_once(&self, message: &str) {
        if self.logged.lock().unwrap().insert(message.to_string()) {
            eprintln!("chip-tool: {message}");
        }
    }

    async fn dispatch(&self, report: AttributeReport) {
        let tx = self.subs.lock().unwrap().iter().find(|(n, _)| *n == report.node).map(|(_, tx)| tx.clone());
        if let Some(tx) = tx {
            let node = report.node;
            if tx.send(report).await.is_err() {
                self.subs.lock().unwrap().retain(|(n, _)| *n != node);
            }
        }
    }

    fn mark_poll(&self, item: &mut CaptureItem) {
        let CaptureItem::Attribute(a) = item else { return };
        let mut guard = self.poll.lock().unwrap();
        if let Some(window) = guard.as_mut().filter(|w| w.node == a.node) {
            if window.paths.remove(&(a.cluster, a.attribute)) {
                a.source = SOURCE_POLL;
                let _ = window.done.try_send(());
            }
        }
    }

    async fn emit(&self, mut item: CaptureItem) {
        self.mark_poll(&mut item);
        let targets: Vec<(NodeId, Sender<CaptureItem>)> = self.captures.lock().unwrap().iter().filter(|(n, _)| item.node().is_none_or(|i| i == *n)).cloned().collect();
        for (node, tx) in targets {
            if tx.send(item.clone()).await.is_err() {
                self.captures.lock().unwrap().retain(|(n, t)| *n != node || !t.same_channel(&tx));
            }
        }
    }
}

struct Session {
    config: ChipToolConfig,
    hub: Hub,
    stdin: tokio::sync::Mutex<Option<ChildStdin>>,
    write_gate: tokio::sync::Mutex<()>,
    poll_gate: tokio::sync::Mutex<()>,
    ready: watch::Sender<bool>,
    pause: watch::Sender<bool>,
    exclusive_gate: tokio::sync::Mutex<()>,
}

impl Session {
    async fn send(stdin: &mut ChildStdin, line: &str) -> Result<()> {
        stdin.write_all(format!("{line}\n").as_bytes()).await?;
        stdin.flush().await?;
        Ok(())
    }

    async fn run_once(&self) -> Result<()> {
        let mut child: Child = Command::new(&self.config.binary)
            .args(self.config.interactive_args())
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::null())
            .kill_on_drop(true)
            .spawn()?;
        let stdin = child.stdin.take().ok_or_else(|| Error::Command("no stdin".into()))?;
        let stdout = child.stdout.take().ok_or_else(|| Error::Command("no stdout".into()))?;
        {
            let mut guard = self.stdin.lock().await;
            let stdin = guard.insert(stdin);
            self.hub.sent.lock().unwrap().clear();
            for node in self.hub.nodes() {
                self.start_node(stdin, node).await?;
            }
        }
        self.ready.send_replace(true);
        let mut parser = StreamParser::new(self.config.endpoint);
        let mut capture = CaptureParser::new();
        let mut reader = BufReader::new(stdout);
        let mut buf = Vec::new();
        let mut pause = self.pause.subscribe();
        let result = loop {
            buf.clear();
            let read = tokio::select! {
                read = reader.read_until(b'\n', &mut buf) => Some(read),
                _ = async { let _ = pause.wait_for(|p| *p).await; } => None,
            };
            let Some(read) = read else {
                let _ = child.kill().await;
                break Ok(());
            };
            match read {
                Ok(0) => break Ok(()),
                Err(e) => break Err(Error::from(e)),
                Ok(_) => {
                    let text = String::from_utf8_lossy(&buf);
                    for item in capture.feed(&text, self.hub.sole_node()) {
                        self.hub.emit(item).await;
                    }
                    let writing = self.hub.pending.lock().unwrap().is_some();
                    for event in parser.feed(&text, self.hub.sole_node(), writing) {
                        match event {
                            StreamEvent::Failure(message) => self.hub.log_once(&message),
                            StreamEvent::Report(report) => self.hub.dispatch(report).await,
                            StreamEvent::Write(result) => {
                                if let Some(message) = self.hub.finish_write(result) {
                                    self.hub.log_once(&message);
                                }
                            }
                        }
                    }
                }
            }
        };
        for item in capture.finish() {
            self.hub.emit(item).await;
        }
        self.ready.send_replace(false);
        *self.stdin.lock().await = None;
        self.hub.finish_write(Err("chip-tool interactive session ended".into()));
        let status = child.wait().await?;
        result.and(Err(Error::Command(format!("chip-tool interactive exited: {status}"))))
    }

    async fn supervise(self: Arc<Self>) {
        let mut backoff = BACKOFF_START;
        loop {
            let started = Instant::now();
            if let Err(e) = self.run_once().await {
                if !*self.pause.borrow() {
                    eprintln!("{e}");
                }
            }
            if started.elapsed() > HEALTHY_AFTER {
                backoff = BACKOFF_START;
            }
            self.hub.emit(CaptureItem::SubscriptionLost { node: None, reason: "chip-tool interactive session ended".into(), backoff_s: Some(backoff.as_secs()) }).await;
            let mut pause = self.pause.subscribe();
            let _ = pause.wait_for(|p| !*p).await;
            sleep(backoff).await;
            backoff = (backoff * 2).min(BACKOFF_MAX);
        }
    }

    async fn exclusive(&self, args: Vec<String>, limit_s: u64) -> Result<RunOutput> {
        let _turn = self.exclusive_gate.lock().await;
        self.pause.send_replace(true);
        let mut ready = self.ready.subscribe();
        let stopped = timeout(PAUSE_WAIT, ready.wait_for(|r| !*r)).await.is_ok();
        let result = match stopped {
            true => run_lenient(&self.config.binary, &args, limit_s).await,
            false => Err(Error::Command("chip-tool interactive session did not stop for the inventory".into())),
        };
        self.pause.send_replace(false);
        result
    }

    async fn start_node(&self, stdin: &mut ChildStdin, node: NodeId) -> Result<()> {
        if self.hub.sent.lock().unwrap().insert(node) {
            for line in self.config.subscribe_lines(node) {
                Self::send(stdin, &line).await?;
            }
        }
        Ok(())
    }

    async fn subscribe(&self, node: NodeId) -> Result<Receiver<AttributeReport>> {
        let (tx, rx) = channel(64);
        let mut guard = self.stdin.lock().await;
        {
            let mut subs = self.hub.subs.lock().unwrap();
            subs.retain(|(n, _)| *n != node);
            subs.push((node, tx));
        }
        if let Some(stdin) = guard.as_mut() {
            self.start_node(stdin, node).await?;
        }
        Ok(rx)
    }

    async fn capture(&self, node: NodeId) -> Result<Receiver<CaptureItem>> {
        let (tx, rx) = channel(CAPTURE_QUEUE);
        let mut guard = self.stdin.lock().await;
        {
            let mut captures = self.hub.captures.lock().unwrap();
            captures.retain(|(n, _)| *n != node);
            captures.push((node, tx));
        }
        if let Some(stdin) = guard.as_mut() {
            self.start_node(stdin, node).await?;
        }
        Ok(rx)
    }

    async fn poll(&self, node: NodeId) -> Result<usize> {
        let _gate = self.poll_gate.lock().await;
        let (done, mut received) = channel(POLL_ATTRIBUTES.len() + 1);
        *self.hub.poll.lock().unwrap() = Some(PollWindow { node, paths: POLL_ATTRIBUTES.iter().copied().collect(), done });
        let sent = match self.stdin.lock().await.as_mut() {
            Some(stdin) => Self::send(stdin, &self.config.poll_line(node)).await,
            None => Err(Error::Command("chip-tool interactive session not ready".into())),
        };
        let mut got = 0;
        if sent.is_ok() {
            let mut wait = POLL_TIMEOUT;
            while got < POLL_ATTRIBUTES.len() {
                if !matches!(timeout(wait, received.recv()).await, Ok(Some(()))) {
                    break;
                }
                got += 1;
                wait = POLL_GRACE;
            }
        }
        self.hub.poll.lock().unwrap().take();
        sent?;
        if got == 0 {
            return Err(Error::Command(format!("no poll response from node {node} within {}s", POLL_TIMEOUT.as_secs())));
        }
        Ok(got)
    }

    async fn write(&self, parts: Vec<String>) -> Result<()> {
        let line = parts.join(" ");
        let _gate = self.write_gate.lock().await;
        retrying(WRITE_ATTEMPTS, WRITE_BACKOFF, || self.write_once(&line)).await
    }

    async fn write_once(&self, line: &str) -> Result<()> {
        let limit = Duration::from_secs(self.config.timeout_s);
        let failed = |what: &str| Error::Command(format!("{what}: {line}"));
        let mut ready = self.ready.subscribe();
        timeout(limit, ready.wait_for(|r| *r)).await.map_err(|_| failed("chip-tool interactive session not ready"))?.map_err(|_| failed("session closed"))?;
        let (tx, rx) = oneshot::channel();
        *self.hub.pending.lock().unwrap() = Some(tx);
        let sent = match self.stdin.lock().await.as_mut() {
            Some(stdin) => Self::send(stdin, line).await,
            None => Err(failed("chip-tool interactive session not running")),
        };
        if let Err(e) = sent {
            self.hub.pending.lock().unwrap().take();
            return Err(e);
        }
        match timeout(limit, rx).await {
            Ok(Ok(Ok(()))) => Ok(()),
            Ok(Ok(Err(message))) => Err(Error::Command(message)),
            _ => {
                self.hub.pending.lock().unwrap().take();
                Err(failed(&format!("no write status after {}s", self.config.timeout_s)))
            }
        }
    }
}

fn retryable(error: &Error) -> bool {
    !matches!(error, Error::Command(message) if message.contains("status = 0x"))
}

async fn retrying<F, Fut>(attempts: u32, backoff: Duration, mut once: F) -> Result<()>
where
    F: FnMut() -> Fut,
    Fut: std::future::Future<Output = Result<()>>,
{
    let mut wait = backoff;
    let mut attempt = 1;
    loop {
        match once().await {
            Err(e) if attempt < attempts && retryable(&e) => {
                sleep(wait).await;
                wait *= 2;
                attempt += 1;
            }
            other => return other,
        }
    }
}

#[derive(Debug)]
struct RunOutput {
    stdout: String,
    stderr: String,
    status: Option<std::process::ExitStatus>,
    limit_s: u64,
}

impl RunOutput {
    fn failure(&self) -> Option<String> {
        match &self.status {
            Some(status) if status.success() => None,
            Some(status) => Some(format!("chip-tool exited with {status}")),
            None => Some(format!("chip-tool killed after {}s timeout", self.limit_s)),
        }
    }

    fn combined(&self) -> String {
        if self.stderr.trim().is_empty() {
            self.stdout.clone()
        } else {
            format!("{}\n{}", self.stdout, self.stderr)
        }
    }
}

async fn read_all<R: tokio::io::AsyncRead + Unpin>(mut stream: R) -> Vec<u8> {
    let mut data = Vec::new();
    let _ = tokio::io::AsyncReadExt::read_to_end(&mut stream, &mut data).await;
    data
}

async fn run_lenient(binary: &str, args: &[String], limit_s: u64) -> Result<RunOutput> {
    let mut child = Command::new(binary).args(args).stdin(Stdio::null()).stdout(Stdio::piped()).stderr(Stdio::piped()).kill_on_drop(true).spawn()?;
    let stdout = tokio::spawn(read_all(child.stdout.take().ok_or_else(|| Error::Command("no stdout".into()))?));
    let stderr = tokio::spawn(read_all(child.stderr.take().ok_or_else(|| Error::Command("no stderr".into()))?));
    let status = match timeout(Duration::from_secs(limit_s), child.wait()).await {
        Ok(status) => Some(status?),
        Err(_) => {
            let _ = child.kill().await;
            None
        }
    };
    let text = |bytes: Vec<u8>| String::from_utf8_lossy(&bytes).into_owned();
    Ok(RunOutput { stdout: text(stdout.await.unwrap_or_default()), stderr: text(stderr.await.unwrap_or_default()), status, limit_s })
}

pub struct ChipToolController {
    config: ChipToolConfig,
    session: Option<Arc<Session>>,
}

impl ChipToolController {
    pub fn new(config: ChipToolConfig) -> Self {
        Self { config, session: None }
    }

    pub fn serving(config: ChipToolConfig) -> Self {
        let session = Arc::new(Session {
            config: config.clone(),
            hub: Hub::default(),
            stdin: Default::default(),
            write_gate: Default::default(),
            poll_gate: Default::default(),
            ready: watch::channel(false).0,
            pause: watch::channel(false).0,
            exclusive_gate: Default::default(),
        });
        tokio::spawn(session.clone().supervise());
        Self { config, session: Some(session) }
    }

    async fn output(&self, args: Vec<String>, limit_s: u64) -> Result<(String, String)> {
        let out = run_lenient(&self.config.binary, &args, limit_s).await?;
        match out.failure() {
            Some(why) => Err(Error::Command(format!("{why}: {}", output_tail(&format!("{}\n{}", out.stdout, out.stderr), 10)))),
            None => Ok((out.stdout, out.stderr)),
        }
    }

    async fn run(&self, args: Vec<String>, limit_s: u64) -> Result<String> {
        let (stdout, stderr) = self.output(args, limit_s).await?;
        Ok(format!("{stdout}\n{stderr}"))
    }
}

#[async_trait]
impl MatterController for ChipToolController {
    async fn commission(&self, node: NodeId, pairing_code: &str, thread_dataset_hex: &str) -> Result<()> {
        let limit = self.config.commission_timeout_s + COMMISSION_MARGIN_S;
        check_commissioned(&self.run(self.config.commission_args(node, pairing_code, thread_dataset_hex), limit).await?)
    }

    async fn read_thermostat(&self, node: NodeId) -> Result<ThermostatState> {
        let mut state = ThermostatState::default();
        for spec in WATCHED {
            let out = self.run(self.config.read_args(spec, node), self.config.timeout_s).await?;
            check_output(&out)?;
            parse_reports(&out, node, self.config.endpoint).iter().for_each(|r| state.apply(r));
        }
        Ok(state)
    }

    async fn write_heating_setpoint(&self, node: NodeId, celsius: f64) -> Result<()> {
        match &self.session {
            Some(session) => session.write(self.config.write_setpoint_parts(node, celsius)).await,
            None => check_output(&self.run(self.config.write_setpoint_args(node, celsius), self.config.timeout_s).await?),
        }
    }

    async fn write_system_mode(&self, node: NodeId, mode: u8) -> Result<()> {
        match &self.session {
            Some(session) => session.write(self.config.write_system_mode_parts(node, mode)).await,
            None => check_output(&self.run(self.config.write_system_mode_args(node, mode), self.config.timeout_s).await?),
        }
    }

    async fn subscribe(&self, node: NodeId) -> Result<Receiver<AttributeReport>> {
        match &self.session {
            Some(session) => session.subscribe(node).await,
            None => Err(Error::Command("subscribe needs a serving controller".into())),
        }
    }

    async fn capture(&self, node: NodeId) -> Result<Receiver<CaptureItem>> {
        match &self.session {
            Some(session) => session.capture(node).await,
            None => Err(Error::Command("capture needs a serving controller".into())),
        }
    }

    async fn poll(&self, node: NodeId) -> Result<usize> {
        match &self.session {
            Some(session) => session.poll(node).await,
            None => Err(Error::Command("poll needs a serving controller".into())),
        }
    }

    async fn inventory(&self, node: NodeId) -> Result<Inventory> {
        let limit = self.config.inventory_limit_s();
        let run = |args: Vec<String>| async move {
            match &self.session {
                Some(session) => session.exclusive(args, limit).await,
                None => run_lenient(&self.config.binary, &args, limit).await,
            }
        };
        let attributes = run(self.config.inventory_attribute_args(node)).await?;
        let events = run(self.config.inventory_event_args(node)).await?;
        let mut inventory = Inventory::from_raw(node, attributes.combined(), events.combined());
        let failures = [("attribute read", &attributes), ("event read", &events)].into_iter().filter_map(|(what, out)| out.failure().map(|why| format!("{what}: {why}")));
        inventory.errors.extend(failures);
        Ok(inventory)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{LOCAL_TEMPERATURE, SYSTEM_MODE};

    fn config() -> ChipToolConfig {
        ChipToolConfig { storage_dir: Some("/var/lib/heatd/chip".into()), paa_trust_store: Some("/opt/paa".into()), ..Default::default() }
    }

    fn pairs(reports: &[AttributeReport]) -> Vec<(NodeId, &str, f64)> {
        reports.iter().map(|r| (r.node, r.attribute.as_str(), r.value)).collect()
    }

    fn stream(text: &str, sole: Option<NodeId>) -> Vec<StreamEvent> {
        let mut parser = StreamParser::new(1);
        text.lines().flat_map(|l| parser.feed(l, sole, false)).collect()
    }

    #[test]
    fn builds_command_lines() {
        let c = config();
        let tail = ["--storage-directory", "/var/lib/heatd/chip", "--paa-trust-store-path", "/opt/paa"];
        let expect = |head: &[&str]| head.iter().chain(tail.iter()).map(|s| s.to_string()).collect::<Vec<_>>();
        assert_eq!(
            c.commission_args(5, "34970112332", "0e08\n"),
            expect(&["pairing", "code-thread", "5", "hex:0e08", "34970112332", "--timeout", "180"])
        );
        let bypass = ChipToolConfig { bypass_attestation: true, commission_timeout_s: 200, ..config() };
        assert_eq!(
            bypass.commission_args(5, "1", "0e08"),
            expect(&["pairing", "code-thread", "5", "hex:0e08", "1", "--timeout", "200", "--bypass-attestation-verifier", "1"])
        );
        assert_eq!(c.read_args(&LOCAL_TEMPERATURE, 5), expect(&["thermostat", "read", "local-temperature", "5", "1"]));
        assert_eq!(c.write_setpoint_args(5, 21.5), expect(&["thermostat", "write", "occupied-heating-setpoint", "2150", "5", "1"]));
        assert_eq!(c.write_system_mode_args(5, 4), expect(&["thermostat", "write", "system-mode", "4", "5", "1"]));
        assert_eq!(c.write_system_mode_parts(5, 0).join(" "), "thermostat write system-mode 0 5 1");
        assert_eq!(c.interactive_args(), expect(&["interactive", "start"]));
        let plain = ChipToolConfig { paa_trust_store: None, endpoint: 2, ..Default::default() };
        assert_eq!(plain.read_args(&SYSTEM_MODE, 9), ["thermostat", "read", "system-mode", "9", "2"]);
        assert_eq!(
            ChipToolConfig::default().paa_trust_store.as_deref(),
            Some("/home/aapphh/snap/chip-tool/common/paa-root-certs")
        );
    }

    #[test]
    fn builds_wildcard_subscribe_and_poll_lines() {
        let c = config();
        assert_eq!(c.subscribe_line(5), "any subscribe-by-id 0xFFFFFFFF 0xFFFFFFFF 30 300 5 0xFFFF --keepSubscriptions 1 --auto-resubscribe 1");
        assert_eq!(
            c.subscribe_lines(5),
            vec![
                "any subscribe-by-id 0xFFFFFFFF 0xFFFFFFFF 30 300 5 0xFFFF --keepSubscriptions 1 --auto-resubscribe 1".to_string(),
                "any subscribe-event-by-id 0xFFFFFFFF 0xFFFFFFFF 30 300 5 0xFFFF --keepSubscriptions 1 --auto-resubscribe 1".to_string(),
            ]
        );
        assert_eq!(c.poll_line(5), "any read-by-id 0x0201,0x0201,0x0201,0x0201,0x002F 0x0000,0x0012,0x0008,0x001C,0x000C 5 0xFFFF,0xFFFF,0xFFFF,0xFFFF,0xFFFF");
        assert_eq!(ChipToolConfig::default().subscribe_min_s, 30);
    }

    #[test]
    fn builds_inventory_lines() {
        let c = config();
        let tail = ["--storage-directory", "/var/lib/heatd/chip", "--paa-trust-store-path", "/opt/paa"];
        let expect = |head: &[&str]| head.iter().chain(tail.iter()).map(|s| s.to_string()).collect::<Vec<_>>();
        assert_eq!(c.inventory_attribute_args(5), expect(&["any", "read-by-id", "0xFFFFFFFF", "0xFFFFFFFF", "5", "0xFFFF", "--timeout", "240"]));
        assert_eq!(c.inventory_event_args(5), expect(&["any", "read-event-by-id", "0xFFFFFFFF", "0xFFFFFFFF", "5", "0xFFFF", "--timeout", "240"]));
    }

    #[test]
    fn records_battery_demand_and_thread_parent_from_the_stream() {
        let events = stream(include_str!("../tests/fixtures/subscribe_extras.txt"), Some(5));
        let reports: Vec<_> = events.into_iter().filter_map(|e| if let StreamEvent::Report(r) = e { Some(r) } else { None }).collect();
        assert_eq!(
            pairs(&reports),
            vec![
                (5, "pi_heating_demand", 35.0),
                (5, "local_temperature_calibration", -0.5),
                (5, "thermostat_running_state", 1.0),
                (5, "bat_voltage", 2950.0),
                (5, "bat_percent_remaining", 95.0),
                (5, "bat_charge_level", 0.0),
                (5, "routing_role", 2.0),
                (5, "thread_parent_rssi", -61.0),
                (5, "thread_parent_last_rssi", -63.0),
                (5, "thread_parent_lqi", 3.0),
                (5, "local_temperature", 21.5),
            ]
        );
        assert_eq!(reports[7].endpoint, 0);
    }

    #[test]
    fn unsupported_subscription_paths_are_logged_once() {
        let hub = Hub::default();
        let message = "Response Failure: IM Error 0x00000581: General error: 0x81 (UNSUPPORTED_CLUSTER)";
        assert_eq!(hub.finish_write(Err(message.into())).as_deref(), Some(message));
        assert!(hub.logged.lock().unwrap().is_empty());
        hub.log_once(message);
        hub.log_once(message);
        assert_eq!(hub.logged.lock().unwrap().len(), 1);
        let (tx, mut rx) = oneshot::channel();
        *hub.pending.lock().unwrap() = Some(tx);
        assert_eq!(hub.finish_write(Err("x".into())), None);
        assert_eq!(rx.try_recv().unwrap(), Err("x".to_string()));
    }

    #[test]
    fn strips_ansi_sequences() {
        assert_eq!(strip_ansi("\x1b[0;32m[TOO]   LocalTemperature: 2150\x1b[0m"), "[TOO]   LocalTemperature: 2150");
        assert_eq!(strip_ansi(">>> \r\x1b[0J\x1b[0;34mx\x1b[0m"), ">>> \rx");
    }

    #[test]
    fn parses_read_output_with_ansi() {
        let text = include_str!("../tests/fixtures/read_local_temperature.txt");
        assert!(text.contains("\x1b[0;32m") && text.contains("2150\x1b[0m"));
        let reports = parse_reports(text, 5, 9);
        assert_eq!(reports, vec![AttributeReport { node: 5, endpoint: 1, attribute: "local_temperature".into(), value: 21.5 }]);
        let legacy = parse_reports(include_str!("../tests/fixtures/read_setpoint_legacy.txt"), 5, 9);
        assert_eq!(legacy[0].attribute, "occupied_heating_setpoint");
        assert_eq!(legacy[0].value, 20.0);
        assert!(parse_reports(include_str!("../tests/fixtures/read_null.txt"), 5, 1).is_empty());
    }

    #[test]
    fn value_with_ansi_suffix_regression() {
        let mut parser = ReportParser::new(1, 1);
        let report = parser.feed("\x1b[0;32m[1761580800.102] [4821:4825] [TOO]   SystemMode: 4\x1b[0m").unwrap();
        assert_eq!((report.attribute.as_str(), report.value), ("system_mode", 4.0));
        assert!(parser.feed("\x1b[0;32m[1.0] [1:2] [TOO]   LocalTemperature: null\x1b[0m").is_none());
    }

    #[test]
    fn parses_subscription_stream() {
        let text = include_str!("../tests/fixtures/subscribe_stream.txt");
        let events = stream(text, Some(5));
        let reports: Vec<_> = events.into_iter().map(|e| match e {
            StreamEvent::Report(r) => r,
            other => panic!("{other:?}"),
        }).collect();
        assert_eq!(
            pairs(&reports),
            vec![(5, "local_temperature", 21.75), (5, "occupied_heating_setpoint", 21.0), (5, "system_mode", 4.0)]
        );
        let mut state = ThermostatState::default();
        reports.iter().for_each(|r| state.apply(r));
        assert_eq!(state.system_mode, Some(4));
        assert_eq!(state.local_temperature_c, Some(21.75));
    }

    #[test]
    fn attributes_reports_to_the_sending_node() {
        let events = stream(include_str!("../tests/fixtures/subscribe_two_nodes.txt"), None);
        let reports: Vec<_> = events.into_iter().filter_map(|e| if let StreamEvent::Report(r) = e { Some(r) } else { None }).collect();
        assert_eq!(
            pairs(&reports),
            vec![
                (5, "local_temperature", 21.75),
                (6, "local_temperature", 19.5),
                (6, "occupied_heating_setpoint", 19.0),
                (5, "system_mode", 4.0)
            ]
        );
        assert!(stream("[1.0] [1:2] [TOO]   SystemMode: 4", None).is_empty());
    }

    #[test]
    fn unknown_attributes_become_extra() {
        let mut parser = ReportParser::new(1, 1);
        let report = parser.feed("[1.0] [1:2] [TOO]   ControlSequenceOfOperation: 4").unwrap();
        assert_eq!(report.attribute, "control_sequence_of_operation");
        let mut state = ThermostatState::default();
        state.apply(&report);
        assert_eq!(state.extra["control_sequence_of_operation"], 4.0);
    }

    #[test]
    fn detects_failures() {
        assert!(check_output(include_str!("../tests/fixtures/write_ok.txt")).is_ok());
        assert!(check_output(include_str!("../tests/fixtures/write_fail.txt")).is_err());
        assert!(check_output(include_str!("../tests/fixtures/command_status_ok.txt")).is_ok());
        assert!(check_output(include_str!("../tests/fixtures/command_status_fail.txt")).is_err());
        assert!(check_output(include_str!("../tests/fixtures/init_storage_failure.txt")).is_err());
        assert!(check_commissioned(include_str!("../tests/fixtures/pairing_ok.txt")).is_ok());
        assert!(check_commissioned(include_str!("../tests/fixtures/pairing_fail.txt")).is_err());
        assert!(check_commissioned("nothing useful").is_err());
    }

    #[test]
    fn detects_write_outcome_in_stream() {
        assert_eq!(stream(include_str!("../tests/fixtures/write_ok.txt"), Some(5)), vec![StreamEvent::Write(Ok(()))]);
        let failed = stream(include_str!("../tests/fixtures/write_fail.txt"), Some(5));
        assert!(matches!(failed.first(), Some(StreamEvent::Write(Err(m))) if m.contains("0x87")));
        assert!(matches!(failed.get(1), Some(StreamEvent::Failure(_))));
    }

    #[test]
    fn address_resolve_timeout_fails_a_pending_write_and_is_retryable() {
        let mut parser = StreamParser::new(1);
        let events: Vec<StreamEvent> = include_str!("../tests/fixtures/write_resolve_timeout.txt").lines().flat_map(|l| parser.feed(l, Some(5), true)).collect();
        let [StreamEvent::Write(Err(message))] = events.as_slice() else { panic!("{events:?}") };
        assert!(message.contains("Timeout") && retryable(&Error::Command(message.clone())));
        let rejected = stream(include_str!("../tests/fixtures/write_fail.txt"), Some(5));
        assert!(matches!(rejected.first(), Some(StreamEvent::Write(Err(m))) if !retryable(&Error::Command(m.clone()))));
    }

    #[tokio::test]
    async fn writes_retry_with_backoff_until_success_but_not_on_device_rejection() {
        let calls = std::sync::atomic::AtomicU32::new(0);
        let flaky = |fail_for: u32, error: &'static str| {
            let calls = &calls;
            move || async move {
                if calls.fetch_add(1, std::sync::atomic::Ordering::SeqCst) < fail_for { Err(Error::Command(error.into())) } else { Ok(()) }
            }
        };
        let started = Instant::now();
        assert!(retrying(3, Duration::from_millis(20), flaky(2, "Timeout")).await.is_ok());
        assert_eq!(calls.swap(0, std::sync::atomic::Ordering::SeqCst), 3);
        assert!(started.elapsed() >= Duration::from_millis(60));
        assert!(retrying(3, Duration::from_millis(1), flaky(9, "Timeout")).await.is_err());
        assert_eq!(calls.swap(0, std::sync::atomic::Ordering::SeqCst), 3);
        assert!(retrying(3, Duration::from_millis(1), flaky(9, "status = 0x87 (CONSTRAINT_ERROR)")).await.is_err());
        assert_eq!(calls.load(std::sync::atomic::Ordering::SeqCst), 1);
    }

    #[test]
    fn unrelated_failures_do_not_fail_a_pending_write() {
        let mut parser = StreamParser::new(1);
        let line = "[1.0] [1:2] [TOO] Run command failure: IM Error 0x00000581: General error: 0x81 (UNSUPPORTED_CLUSTER)";
        assert!(matches!(parser.feed(line, Some(5), false).as_slice(), [StreamEvent::Failure(_)]));
        assert!(matches!(parser.feed("[1.0] [1:2] [TOO] Response Failure: x", Some(5), true).as_slice(), [StreamEvent::Failure(_)]));
        assert!(matches!(parser.feed(line, Some(5), true).as_slice(), [StreamEvent::Write(Err(_))]));
    }

    #[test]
    fn uppercase_booleans_parse() {
        assert_eq!(parse_value("TRUE"), Some(1.0));
        assert_eq!(parse_value("FALSE"), Some(0.0));
    }

    #[tokio::test]
    async fn one_shot_run_respects_exit_status() {
        let (binary, ok, fail) = if cfg!(windows) {
            ("cmd", vec!["/C", "echo hello"], vec!["/C", "echo \x1b[0;31mboom\x1b[0m & exit 3"])
        } else {
            ("sh", vec!["-c", "echo hello"], vec!["-c", "printf '\\033[0;31mboom\\033[0m\\n'; exit 3"])
        };
        let controller = ChipToolController::new(ChipToolConfig { binary: binary.into(), ..Default::default() });
        let to_args = |v: Vec<&str>| v.into_iter().map(String::from).collect::<Vec<_>>();
        assert!(controller.run(to_args(ok), 10).await.unwrap().contains("hello"));
        let err = controller.run(to_args(fail), 10).await.unwrap_err().to_string();
        assert!(err.contains("boom") && err.contains("exited with") && !err.contains('\x1b'), "{err}");
    }

    #[tokio::test]
    async fn inventory_keeps_raw_output_when_chip_tool_exits_non_zero() {
        let binary = if cfg!(windows) { "cmd" } else { "sh" };
        let script = if cfg!(windows) { "echo [TOO] Endpoint: 0 Cluster: 0x0000_0028 Attribute 0x0000_0001 DataVersion: 1 & echo [TOO]   VendorName: \"X\" & exit 3" } else { "printf '[TOO] Endpoint: 0 Cluster: 0x0000_0028 Attribute 0x0000_0001 DataVersion: 1\n[TOO]   VendorName: \"X\"\n'; exit 3" };
        let flag = if cfg!(windows) { "/C" } else { "-c" };
        let out = run_lenient(binary, &[flag.to_string(), script.to_string()], 10).await.unwrap();
        assert!(out.failure().unwrap().contains("exited with") && out.combined().contains("VendorName"), "{out:?}");
        let inventory = Inventory::from_raw(5, out.combined(), String::new());
        assert_eq!(inventory.attributes.len(), 1);
    }

    #[test]
    fn storage_lock_is_exclusive() {
        let dir = std::env::temp_dir().join(format!("heatd-lock-test-{}", std::process::id()));
        let config = ChipToolConfig { storage_dir: Some(dir.to_string_lossy().into_owned()), ..Default::default() };
        let first = config.lock_storage().unwrap();
        let err = config.lock_storage().unwrap_err().to_string();
        assert!(err.contains("stop heatd first"), "{err}");
        drop(first);
        assert!(config.lock_storage().is_ok());
        let _ = std::fs::remove_dir_all(dir);
    }
}

#[cfg(test)]
mod hub_tests {
    use super::*;

    #[tokio::test]
    async fn marks_poll_responses_and_broadcasts_lifecycle() {
        let hub = Hub::default();
        let (tx5, mut rx5) = channel(64);
        let (tx6, mut rx6) = channel(64);
        hub.captures.lock().unwrap().extend([(5, tx5), (6, tx6)]);
        let (done, mut received) = channel(8);
        *hub.poll.lock().unwrap() = Some(PollWindow { node: 5, paths: POLL_ATTRIBUTES.iter().copied().collect(), done });
        for item in CaptureParser::new().feed_all(include_str!("../tests/fixtures/subscribe_stream.txt"), Some(5)) {
            hub.emit(item).await;
        }
        let mut sources = Vec::new();
        while let Ok(CaptureItem::Attribute(a)) = rx5.try_recv() {
            sources.push((a.attribute, a.source));
        }
        assert!(sources.contains(&(0x0000, "poll")), "{sources:?}");
        assert!(received.try_recv().is_ok());
        assert!(rx6.try_recv().is_err());
        hub.emit(CaptureItem::SubscriptionLost { node: None, reason: "gone".into(), backoff_s: Some(2) }).await;
        assert!(matches!(rx5.try_recv(), Ok(CaptureItem::SubscriptionLost { backoff_s: Some(2), .. })));
        assert!(matches!(rx6.try_recv(), Ok(CaptureItem::SubscriptionLost { .. })));
    }

    #[test]
    fn nodes_union_covers_both_channels() {
        let hub = Hub::default();
        let (a, _) = channel::<AttributeReport>(1);
        let (b, _) = channel::<CaptureItem>(1);
        hub.subs.lock().unwrap().push((5, a));
        assert_eq!(hub.sole_node(), Some(5));
        hub.captures.lock().unwrap().push((6, b));
        assert_eq!((hub.nodes(), hub.sole_node()), (vec![5, 6], None));
    }
}
