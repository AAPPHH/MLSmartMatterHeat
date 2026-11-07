mod capture;
mod chip_tool;
mod inventory;
mod mock;

use async_trait::async_trait;
use std::collections::BTreeMap;
use tokio::sync::mpsc::Receiver;

pub use capture::{CaptureItem, CaptureParser, RawAttribute, RawEvent, TypedValue, SOURCE_POLL, SOURCE_SUBSCRIPTION};
pub use chip_tool::{strip_ansi, ChipToolConfig, ChipToolController, ReportParser, StorageLock};
pub use inventory::{
    attribute_name, attribute_writable, cluster_name, command_name, event_name, hex_id, is_vendor_id, parse_attributes, parse_events, parse_number, parse_scalar,
    thermostat_features, AttributeRecord, EventRecord, Inventory, ACCEPTED_COMMAND_LIST, ATTRIBUTE_LIST, CLUSTER_REVISION, EVENT_LIST, FEATURE_MAP,
    GENERATED_COMMAND_LIST, NEIGHBOR_TABLE, POWER_SOURCE_CLUSTER, ROUTE_TABLE, THREAD_DIAGNOSTICS_CLUSTER,
};
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
pub const MAX_HEAT_SETPOINT_LIMIT: AttributeSpec = AttributeSpec { id: 0x0016, pascal: "MaxHeatSetpointLimit", snake: "max_heat_setpoint_limit", kebab: "max-heat-setpoint-limit", scale: 100.0 };
pub const SYSTEM_MODE: AttributeSpec = AttributeSpec { id: 0x001C, pascal: "SystemMode", snake: "system_mode", kebab: "system-mode", scale: 1.0 };
pub const WATCHED: [&AttributeSpec; 3] = [&LOCAL_TEMPERATURE, &OCCUPIED_HEATING_SETPOINT, &SYSTEM_MODE];
pub const PI_HEATING_DEMAND: AttributeSpec = AttributeSpec { id: 0x0008, pascal: "PIHeatingDemand", snake: "pi_heating_demand", kebab: "pi-heating-demand", scale: 1.0 };
pub const LOCAL_TEMPERATURE_CALIBRATION: AttributeSpec = AttributeSpec { id: 0x0010, pascal: "LocalTemperatureCalibration", snake: "local_temperature_calibration", kebab: "local-temperature-calibration", scale: 10.0 };
pub const THERMOSTAT_RUNNING_STATE: AttributeSpec = AttributeSpec { id: 0x0029, pascal: "ThermostatRunningState", snake: "thermostat_running_state", kebab: "thermostat-running-state", scale: 1.0 };
pub const BAT_VOLTAGE: AttributeSpec = AttributeSpec { id: 0x000B, pascal: "BatVoltage", snake: "bat_voltage", kebab: "bat-voltage", scale: 1.0 };
pub const BAT_PERCENT_REMAINING: AttributeSpec = AttributeSpec { id: 0x000C, pascal: "BatPercentRemaining", snake: "bat_percent_remaining", kebab: "bat-percent-remaining", scale: 2.0 };
pub const BAT_CHARGE_LEVEL: AttributeSpec = AttributeSpec { id: 0x000E, pascal: "BatChargeLevel", snake: "bat_charge_level", kebab: "bat-charge-level", scale: 1.0 };
pub const ROUTING_ROLE: AttributeSpec = AttributeSpec { id: 0x0001, pascal: "RoutingRole", snake: "routing_role", kebab: "routing-role", scale: 1.0 };
pub const NEIGHBOR_TABLE_SPEC: AttributeSpec = AttributeSpec { id: 0x0007, pascal: "NeighborTable", snake: "neighbor_table", kebab: "neighbor-table", scale: 1.0 };

pub const POLL_ATTRIBUTES: [(u32, u32); 5] = [
    (THERMOSTAT_CLUSTER, LOCAL_TEMPERATURE.id),
    (THERMOSTAT_CLUSTER, OCCUPIED_HEATING_SETPOINT.id),
    (THERMOSTAT_CLUSTER, PI_HEATING_DEMAND.id),
    (THERMOSTAT_CLUSTER, SYSTEM_MODE.id),
    (POWER_SOURCE_CLUSTER, BAT_PERCENT_REMAINING.id),
];

pub struct SubscriptionGroup {
    pub cluster: &'static str,
    pub cluster_id: u32,
    pub attributes: &'static [&'static AttributeSpec],
}

pub const EXTRA_SUBSCRIPTIONS: [SubscriptionGroup; 3] = [
    SubscriptionGroup { cluster: "thermostat", cluster_id: THERMOSTAT_CLUSTER, attributes: &[&PI_HEATING_DEMAND, &LOCAL_TEMPERATURE_CALIBRATION, &THERMOSTAT_RUNNING_STATE] },
    SubscriptionGroup { cluster: "powersource", cluster_id: 0x002F, attributes: &[&BAT_VOLTAGE, &BAT_PERCENT_REMAINING, &BAT_CHARGE_LEVEL] },
    SubscriptionGroup { cluster: "threadnetworkdiagnostics", cluster_id: 0x0035, attributes: &[&ROUTING_ROLE, &NEIGHBOR_TABLE_SPEC] },
];

pub const THREAD_PARENT_RSSI: &str = "thread_parent_rssi";
pub const THREAD_PARENT_LAST_RSSI: &str = "thread_parent_last_rssi";
pub const THREAD_PARENT_LQI: &str = "thread_parent_lqi";

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
    async fn write_max_heat_limit(&self, node: NodeId, celsius: f64) -> Result<()>;
    async fn write_system_mode(&self, node: NodeId, mode: u8) -> Result<()>;
    async fn subscribe(&self, node: NodeId) -> Result<Receiver<AttributeReport>>;
    async fn capture(&self, node: NodeId) -> Result<Receiver<CaptureItem>>;
    async fn poll(&self, node: NodeId) -> Result<usize>;
    async fn resubscribe(&self, node: NodeId) -> Result<()>;
    async fn restart_session(&self) -> Result<()>;
    async fn inventory(&self, node: NodeId) -> Result<Inventory>;
}
