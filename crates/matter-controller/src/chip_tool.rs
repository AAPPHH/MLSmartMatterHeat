use crate::{
    centi_degrees, AttributeReport, AttributeSpec, Error, MatterController, NodeId, Result, ThermostatState, WATCHED,
    OCCUPIED_HEATING_SETPOINT,
};
use async_trait::async_trait;
use std::process::Stdio;
use std::time::Duration;
use tokio::io::{AsyncBufReadExt, BufReader};
use tokio::process::Command;
use tokio::sync::mpsc::{channel, Receiver};
use tokio::time::timeout;

#[derive(Debug, Clone)]
pub struct ChipToolConfig {
    pub binary: String,
    pub storage_dir: String,
    pub paa_trust_store: Option<String>,
    pub endpoint: u16,
    pub subscribe_min_s: u32,
    pub subscribe_max_s: u32,
    pub timeout_s: u64,
}

impl Default for ChipToolConfig {
    fn default() -> Self {
        Self {
            binary: "chip-tool".into(),
            storage_dir: "data/chip-tool".into(),
            paa_trust_store: None,
            endpoint: 1,
            subscribe_min_s: 5,
            subscribe_max_s: 300,
            timeout_s: 90,
        }
    }
}

impl ChipToolConfig {
    pub fn args(&self, parts: &[String]) -> Vec<String> {
        let mut args = parts.to_vec();
        args.push("--storage-directory".into());
        args.push(self.storage_dir.clone());
        if let Some(store) = &self.paa_trust_store {
            args.push("--paa-trust-store-path".into());
            args.push(store.clone());
        }
        args
    }

    pub fn commission_args(&self, node: NodeId, pairing_code: &str, dataset_hex: &str) -> Vec<String> {
        self.args(&strings(&["pairing", "code-thread", &node.to_string(), &format!("hex:{}", dataset_hex.trim()), pairing_code]))
    }

    pub fn read_args(&self, spec: &AttributeSpec, node: NodeId) -> Vec<String> {
        self.args(&strings(&["thermostat", "read", spec.kebab, &node.to_string(), &self.endpoint.to_string()]))
    }

    pub fn write_setpoint_args(&self, node: NodeId, celsius: f64) -> Vec<String> {
        self.args(&strings(&[
            "thermostat",
            "write",
            OCCUPIED_HEATING_SETPOINT.kebab,
            &centi_degrees(celsius).to_string(),
            &node.to_string(),
            &self.endpoint.to_string(),
        ]))
    }

    pub fn subscribe_args(&self, spec: &AttributeSpec, node: NodeId) -> Vec<String> {
        self.args(&strings(&[
            "thermostat",
            "subscribe",
            spec.kebab,
            &self.subscribe_min_s.to_string(),
            &self.subscribe_max_s.to_string(),
            &node.to_string(),
            &self.endpoint.to_string(),
        ]))
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

pub struct ReportParser {
    node: NodeId,
    endpoint: u16,
}

impl ReportParser {
    pub fn new(node: NodeId, default_endpoint: u16) -> Self {
        Self { node, endpoint: default_endpoint }
    }

    pub fn feed(&mut self, line: &str) -> Option<AttributeReport> {
        let body = payload(line)?;
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

pub fn check_output(output: &str) -> Result<()> {
    for line in output.lines() {
        let body = payload(line).unwrap_or(line).trim();
        let bad_status = body
            .split_once("Status=0x")
            .is_some_and(|(_, s)| u32::from_str_radix(s.split_whitespace().next().unwrap_or("0"), 16) != Ok(0));
        if body.starts_with("Run command failure") || body.contains("commissioning Failure") || bad_status {
            return Err(Error::Command(body.to_string()));
        }
    }
    Ok(())
}

pub fn check_commissioned(output: &str) -> Result<()> {
    check_output(output)?;
    if output.contains("Device commissioning completed with success") {
        Ok(())
    } else {
        Err(Error::Command("commissioning did not report success".into()))
    }
}

pub struct ChipToolController {
    config: ChipToolConfig,
}

impl ChipToolController {
    pub fn new(config: ChipToolConfig) -> Self {
        Self { config }
    }

    async fn run(&self, args: Vec<String>) -> Result<String> {
        let child = Command::new(&self.config.binary)
            .args(&args)
            .stdin(Stdio::null())
            .kill_on_drop(true)
            .output();
        let output = timeout(Duration::from_secs(self.config.timeout_s), child)
            .await
            .map_err(|_| Error::Command(format!("timeout after {}s: {}", self.config.timeout_s, args.join(" "))))??;
        Ok(format!("{}\n{}", String::from_utf8_lossy(&output.stdout), String::from_utf8_lossy(&output.stderr)))
    }
}

#[async_trait]
impl MatterController for ChipToolController {
    async fn commission(&self, node: NodeId, pairing_code: &str, thread_dataset_hex: &str) -> Result<()> {
        check_commissioned(&self.run(self.config.commission_args(node, pairing_code, thread_dataset_hex)).await?)
    }

    async fn read_thermostat(&self, node: NodeId) -> Result<ThermostatState> {
        let mut state = ThermostatState::default();
        for spec in WATCHED {
            let out = self.run(self.config.read_args(spec, node)).await?;
            check_output(&out)?;
            parse_reports(&out, node, self.config.endpoint).iter().for_each(|r| state.apply(r));
        }
        Ok(state)
    }

    async fn write_heating_setpoint(&self, node: NodeId, celsius: f64) -> Result<()> {
        check_output(&self.run(self.config.write_setpoint_args(node, celsius)).await?)
    }

    async fn subscribe(&self, node: NodeId) -> Result<Receiver<AttributeReport>> {
        let (tx, rx) = channel(64);
        for spec in WATCHED {
            let mut child = Command::new(&self.config.binary)
                .args(self.config.subscribe_args(spec, node))
                .stdin(Stdio::null())
                .stdout(Stdio::piped())
                .stderr(Stdio::null())
                .kill_on_drop(true)
                .spawn()?;
            let stdout = child.stdout.take().ok_or_else(|| Error::Command("no stdout".into()))?;
            let tx = tx.clone();
            let mut parser = ReportParser::new(node, self.config.endpoint);
            tokio::spawn(async move {
                let _child = child;
                let mut lines = BufReader::new(stdout).lines();
                while let Ok(Some(line)) = lines.next_line().await {
                    if let Some(report) = parser.feed(&line) {
                        if tx.send(report).await.is_err() {
                            break;
                        }
                    }
                }
            });
        }
        Ok(rx)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{LOCAL_TEMPERATURE, SYSTEM_MODE};

    fn config() -> ChipToolConfig {
        ChipToolConfig { storage_dir: "/var/lib/heatd/chip".into(), paa_trust_store: Some("/opt/paa".into()), ..Default::default() }
    }

    #[test]
    fn builds_command_lines() {
        let c = config();
        let tail = ["--storage-directory", "/var/lib/heatd/chip", "--paa-trust-store-path", "/opt/paa"];
        let expect = |head: &[&str]| head.iter().chain(tail.iter()).map(|s| s.to_string()).collect::<Vec<_>>();
        assert_eq!(c.commission_args(5, "34970112332", "0e08\n"), expect(&["pairing", "code-thread", "5", "hex:0e08", "34970112332"]));
        assert_eq!(c.read_args(&LOCAL_TEMPERATURE, 5), expect(&["thermostat", "read", "local-temperature", "5", "1"]));
        assert_eq!(c.write_setpoint_args(5, 21.5), expect(&["thermostat", "write", "occupied-heating-setpoint", "2150", "5", "1"]));
        assert_eq!(c.subscribe_args(&SYSTEM_MODE, 5), expect(&["thermostat", "subscribe", "system-mode", "5", "300", "5", "1"]));
        let plain = ChipToolConfig { storage_dir: "s".into(), endpoint: 2, ..Default::default() };
        assert_eq!(plain.read_args(&SYSTEM_MODE, 9), ["thermostat", "read", "system-mode", "9", "2", "--storage-directory", "s"]);
    }

    #[test]
    fn parses_read_output() {
        let reports = parse_reports(include_str!("../tests/fixtures/read_local_temperature.txt"), 5, 9);
        assert_eq!(reports, vec![AttributeReport { node: 5, endpoint: 1, attribute: "local_temperature".into(), value: 21.5 }]);
        let legacy = parse_reports(include_str!("../tests/fixtures/read_setpoint_legacy.txt"), 5, 9);
        assert_eq!(legacy[0].attribute, "occupied_heating_setpoint");
        assert_eq!(legacy[0].value, 20.0);
        assert!(parse_reports(include_str!("../tests/fixtures/read_null.txt"), 5, 1).is_empty());
    }

    #[test]
    fn parses_subscription_stream() {
        let reports = parse_reports(include_str!("../tests/fixtures/subscribe_stream.txt"), 5, 1);
        let pairs: Vec<_> = reports.iter().map(|r| (r.attribute.as_str(), r.value)).collect();
        assert_eq!(pairs, vec![("local_temperature", 21.75), ("occupied_heating_setpoint", 21.0), ("system_mode", 4.0)]);
        let mut state = ThermostatState::default();
        reports.iter().for_each(|r| state.apply(r));
        assert_eq!(state.system_mode, Some(4));
        assert_eq!(state.local_temperature_c, Some(21.75));
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
        assert!(check_commissioned(include_str!("../tests/fixtures/pairing_ok.txt")).is_ok());
        assert!(check_commissioned(include_str!("../tests/fixtures/pairing_fail.txt")).is_err());
        assert!(check_commissioned("nothing useful").is_err());
    }
}
