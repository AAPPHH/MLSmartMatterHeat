mod chip_tool;
mod mock;

use async_trait::async_trait;
use std::collections::BTreeMap;
use tokio::sync::mpsc::Receiver;

pub use chip_tool::{strip_ansi, ChipToolConfig, ChipToolController, ReportParser, StorageLock};
pub use mock::MockController;

pub type NodeId = u64;

pub const THERMOSTAT_CLUSTER: u32 = 0x0201;

pub struct AttributeSpec {
    pub id: u32,
    pub pascal: &'static str,
    pub snake: &'static str,
    pub kebab: &'static str,
    pub scale: f64,
}

pub const LOCAL_TEMPERATURE: AttributeSpec = AttributeSpec { id: 0x0000, pascal: "LocalTemperature", snake: "local_temperature", kebab: "local-temperature", scale: 100.0 };
pub const OCCUPIED_HEATING_SETPOINT: AttributeSpec = AttributeSpec { id: 0x0012, pascal: "OccupiedHeatingSetpoint", snake: "occupied_heating_setpoint", kebab: "occupied-heating-setpoint", scale: 100.0 };
pub const SYSTEM_MODE: AttributeSpec = AttributeSpec { id: 0x001C, pascal: "SystemMode", snake: "system_mode", kebab: "system-mode", scale: 1.0 };
pub const WATCHED: [&AttributeSpec; 3] = [&LOCAL_TEMPERATURE, &OCCUPIED_HEATING_SETPOINT, &SYSTEM_MODE];

#[derive(Debug, thiserror::Error)]
pub enum Error {
    #[error("command failed: {0}")]
    Command(String),
    #[error("unknown node {0}")]
    UnknownNode(NodeId),
    #[error("io: {0}")]
    Io(#[from] std::io::Error),
}

pub type Result<T> = std::result::Result<T, Error>;

#[derive(Debug, Clone, PartialEq)]
pub struct AttributeReport {
    pub node: NodeId,
    pub endpoint: u16,
    pub attribute: String,
    pub value: f64,
}

#[derive(Debug, Clone, Default, PartialEq)]
pub struct ThermostatState {
    pub local_temperature_c: Option<f64>,
    pub occupied_heating_setpoint_c: Option<f64>,
    pub system_mode: Option<u8>,
    pub extra: BTreeMap<String, f64>,
}

impl ThermostatState {
    pub fn apply(&mut self, report: &AttributeReport) {
        match report.attribute.as_str() {
            "local_temperature" => self.local_temperature_c = Some(report.value),
            "occupied_heating_setpoint" => self.occupied_heating_setpoint_c = Some(report.value),
            "system_mode" => self.system_mode = Some(report.value as u8),
            other => {
                self.extra.insert(other.to_string(), report.value);
            }
        }
    }
}

pub fn centi_degrees(celsius: f64) -> i16 {
    (celsius * 100.0).round().clamp(i16::MIN as f64, i16::MAX as f64) as i16
}

#[async_trait]
pub trait MatterController: Send + Sync {
    async fn commission(&self, node: NodeId, pairing_code: &str, thread_dataset_hex: &str) -> Result<()>;
    async fn read_thermostat(&self, node: NodeId) -> Result<ThermostatState>;
    async fn write_heating_setpoint(&self, node: NodeId, celsius: f64) -> Result<()>;
    async fn subscribe(&self, node: NodeId) -> Result<Receiver<AttributeReport>>;
}
