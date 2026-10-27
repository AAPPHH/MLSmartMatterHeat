use crate::{
    centi_degrees, AttributeReport, AttributeSpec, Error, MatterController, NodeId, Result, ThermostatState, OCCUPIED_HEATING_SETPOINT, WATCHED,
};
use async_trait::async_trait;
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

fn clean(line: &str) -> String {
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
            subscribe_min_s: 5,
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

    fn write_setpoint_parts(&self, node: NodeId, celsius: f64) -> Vec<String> {
        strings(&[
            "thermostat",
            "write",
            OCCUPIED_HEATING_SETPOINT.kebab,
            &centi_degrees(celsius).to_string(),
            &node.to_string(),
            &self.endpoint.to_string(),
        ])
    }

    pub fn write_setpoint_args(&self, node: NodeId, celsius: f64) -> Vec<String> {
        self.args(&self.write_setpoint_parts(node, celsius))
    }

    pub fn interactive_args(&self) -> Vec<String> {
        self.args(&strings(&["interactive", "start"]))
    }

    pub fn subscribe_line(&self, node: NodeId) -> String {
        let ids: Vec<String> = WATCHED.iter().map(|s| format!("0x{:04X}", s.id)).collect();
        format!(
            "thermostat subscribe-by-id {} {} {} {} {} --keepSubscriptions 1 --auto-resubscribe 1",
            ids.join(","),
            self.subscribe_min_s,
            self.subscribe_max_s,
            node,
            self.endpoint
        )
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

fn payload(line: &str) -> Option<&str> {
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
        "true" => Some(1.0),
        "false" => Some(0.0),
        t => t.strip_prefix("0x").map_or_else(|| t.parse::<f64>().ok(), |h| i64::from_str_radix(h, 16).ok().map(|v| v as f64)),
    }
}

fn status_code(line: &str, marker: &str) -> Option<u32> {
    let rest = line.split_once(marker)?.1;
    let hex: String = rest.chars().take_while(char::is_ascii_hexdigit).collect();
    u32::from_str_radix(&hex, 16).ok()
}

fn is_failure(line: &str) -> bool {
    let body = payload(line).unwrap_or(line).trim();
    body.starts_with("Run command failure")
        || body.starts_with("Response Failure")
        || body.starts_with("Error:")
        || body.contains("commissioning Failure")
        || body.contains("Init Storage failure")
        || status_code(line, "Status=0x").is_some_and(|c| c != 0)
        || status_code(line, "status = 0x").is_some_and(|c| c != 0)
}

fn rx_peer(line: &str) -> Option<NodeId> {
    let token = line.split_once("Msg RX from ")?.1.split_whitespace().next()?;
    NodeId::from_str_radix(token.split_once(':')?.1, 16).ok()
}

pub struct ReportParser {
    node: NodeId,
    endpoint: u16,
}

impl ReportParser {
    pub fn new(node: NodeId, default_endpoint: u16) -> Self {
        Self { node, endpoint: default_endpoint }
    }

    pub fn feed(&mut self, raw: &str) -> Option<AttributeReport> {
        let line = clean(raw);
        let body = payload(&line)?;
        if let Some(rest) = body.trim_start().strip_prefix("Endpoint:") {
            if let Some(ep) = rest.split_whitespace().next().and_then(|t| t.parse().ok()) {
                self.endpoint = ep;
            }
            return None;
        }
        if !body.starts_with("  ") {
            return None;
        }
        let (name, value) = body.trim().split_once(": ")?;
        if name.is_empty() || !name.chars().all(|c| c.is_ascii_alphanumeric()) {
            return None;
        }
        let raw = parse_value(value)?;
        let known = WATCHED.iter().find(|s| s.pascal == name);
        Some(AttributeReport {
            node: self.node,
            endpoint: self.endpoint,
            attribute: known.map_or_else(|| snake_case(name), |s| s.snake.to_string()),
            value: raw / known.map_or(1.0, |s| s.scale),
        })
    }
}

pub fn parse_reports(output: &str, node: NodeId, default_endpoint: u16) -> Vec<AttributeReport> {
    let mut parser = ReportParser::new(node, default_endpoint);
    output.lines().filter_map(|l| parser.feed(l)).collect()
}

#[derive(Debug, PartialEq)]
pub enum StreamEvent {
    Report(AttributeReport),
    Write(std::result::Result<(), String>),
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

    pub fn feed(&mut self, raw: &str, sole_node: Option<NodeId>) -> Option<StreamEvent> {
        let line = clean(raw);
        if let Some(peer) = rx_peer(&line) {
            self.peer = Some(peer);
            return None;
        }
        if line.contains("WriteResponseMessage") {
            self.in_write = true;
            return None;
        }
        if self.in_write {
            if let Some(code) = status_code(&line, "status = 0x") {
                self.in_write = false;
                return Some(StreamEvent::Write(if code == 0 { Ok(()) } else { Err(line.trim().to_string()) }));
            }
        }
        if is_failure(&line) {
            return Some(StreamEvent::Write(Err(line.trim().to_string())));
        }
        let node = self.peer.or(sole_node);
        self.reports.node = node.unwrap_or(0);
        self.reports.feed(&line).filter(|_| node.is_some()).map(StreamEvent::Report)
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

#[derive(Default)]
struct Hub {
    subs: Mutex<Vec<(NodeId, Sender<AttributeReport>)>>,
    pending: Mutex<Option<oneshot::Sender<std::result::Result<(), String>>>>,
}

impl Hub {
    fn sole_node(&self) -> Option<NodeId> {
        match self.subs.lock().unwrap().as_slice() {
            [(node, _)] => Some(*node),
            _ => None,
        }
    }

    fn nodes(&self) -> Vec<NodeId> {
        self.subs.lock().unwrap().iter().map(|(n, _)| *n).collect()
    }

    fn finish_write(&self, result: std::result::Result<(), String>) {
        if let Some(tx) = self.pending.lock().unwrap().take() {
            let _ = tx.send(result);
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
}

struct Session {
    config: ChipToolConfig,
    hub: Hub,
    stdin: tokio::sync::Mutex<Option<ChildStdin>>,
    write_gate: tokio::sync::Mutex<()>,
    ready: watch::Sender<bool>,
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
            for node in self.hub.nodes() {
                Self::send(stdin, &self.config.subscribe_line(node)).await?;
            }
        }
        self.ready.send_replace(true);
        let mut parser = StreamParser::new(self.config.endpoint);
        let mut reader = BufReader::new(stdout);
        let mut buf = Vec::new();
        let result = loop {
            buf.clear();
            match reader.read_until(b'\n', &mut buf).await {
                Ok(0) => break Ok(()),
                Err(e) => break Err(Error::from(e)),
                Ok(_) => match parser.feed(&String::from_utf8_lossy(&buf), self.hub.sole_node()) {
                    Some(StreamEvent::Report(report)) => self.hub.dispatch(report).await,
                    Some(StreamEvent::Write(result)) => self.hub.finish_write(result),
                    None => {}
                },
            }
        };
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
                eprintln!("{e}");
            }
            if started.elapsed() > HEALTHY_AFTER {
                backoff = BACKOFF_START;
            }
            sleep(backoff).await;
            backoff = (backoff * 2).min(BACKOFF_MAX);
        }
    }

    async fn subscribe(&self, node: NodeId) -> Result<Receiver<AttributeReport>> {
        let (tx, rx) = channel(64);
        let mut guard = self.stdin.lock().await;
        let known = {
            let mut subs = self.hub.subs.lock().unwrap();
            let known = subs.iter().any(|(n, _)| *n == node);
            subs.retain(|(n, _)| *n != node);
            subs.push((node, tx));
            known
        };
        if let (Some(stdin), false) = (guard.as_mut(), known) {
            Self::send(stdin, &self.config.subscribe_line(node)).await?;
        }
        Ok(rx)
    }

    async fn write(&self, node: NodeId, celsius: f64) -> Result<()> {
        let limit = Duration::from_secs(self.config.timeout_s);
        let line = self.config.write_setpoint_parts(node, celsius).join(" ");
        let failed = |what: &str| Error::Command(format!("{what}: {line}"));
        let _gate = self.write_gate.lock().await;
        let mut ready = self.ready.subscribe();
        timeout(limit, ready.wait_for(|r| *r)).await.map_err(|_| failed("chip-tool interactive session not ready"))?.map_err(|_| failed("session closed"))?;
        let (tx, rx) = oneshot::channel();
        *self.hub.pending.lock().unwrap() = Some(tx);
        let sent = match self.stdin.lock().await.as_mut() {
            Some(stdin) => Self::send(stdin, &line).await,
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
            ready: watch::channel(false).0,
        });
        tokio::spawn(session.clone().supervise());
        Self { config, session: Some(session) }
    }

    async fn run(&self, args: Vec<String>, limit_s: u64) -> Result<String> {
        let child = Command::new(&self.config.binary).args(&args).stdin(Stdio::null()).kill_on_drop(true).output();
        let output = timeout(Duration::from_secs(limit_s), child)
            .await
            .map_err(|_| Error::Command(format!("timeout after {limit_s}s: {}", args.join(" "))))??;
        let text = format!("{}\n{}", String::from_utf8_lossy(&output.stdout), String::from_utf8_lossy(&output.stderr));
        if !output.status.success() {
            return Err(Error::Command(format!("chip-tool exited with {}: {}", output.status, output_tail(&text, 10))));
        }
        Ok(text)
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
            Some(session) => session.write(node, celsius).await,
            None => check_output(&self.run(self.config.write_setpoint_args(node, celsius), self.config.timeout_s).await?),
        }
    }

    async fn subscribe(&self, node: NodeId) -> Result<Receiver<AttributeReport>> {
        match &self.session {
            Some(session) => session.subscribe(node).await,
            None => Err(Error::Command("subscribe needs a serving controller".into())),
        }
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
        text.lines().filter_map(|l| parser.feed(l, sole)).collect()
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
        assert_eq!(c.interactive_args(), expect(&["interactive", "start"]));
        let plain = ChipToolConfig { paa_trust_store: None, endpoint: 2, ..Default::default() };
        assert_eq!(plain.read_args(&SYSTEM_MODE, 9), ["thermostat", "read", "system-mode", "9", "2"]);
        assert_eq!(
            ChipToolConfig::default().paa_trust_store.as_deref(),
            Some("/home/aapphh/snap/chip-tool/common/paa-root-certs")
        );
    }

    #[test]
    fn builds_subscribe_line() {
        assert_eq!(
            config().subscribe_line(5),
            "thermostat subscribe-by-id 0x0000,0x0012,0x001C 5 300 5 1 --keepSubscriptions 1 --auto-resubscribe 1"
        );
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
        assert!(failed.iter().all(|e| matches!(e, StreamEvent::Write(Err(_)))));
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
