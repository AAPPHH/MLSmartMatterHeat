use anyhow::{anyhow, Context, Result};
use chrono::Weekday;
use serde::Deserialize;
use std::path::Path;

#[derive(Debug, Clone, Copy, Deserialize, PartialEq, Eq)]
pub enum ControllerKind {
    #[serde(rename = "chip-tool")]
    ChipTool,
    #[serde(rename = "mock")]
    Mock,
}

#[derive(Debug, Clone, Deserialize)]
#[serde(default)]
pub struct ChipToolSection {
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

impl Default for ChipToolSection {
    fn default() -> Self {
        let base = matter_controller::ChipToolConfig::default();
        Self {
            binary: base.binary,
            storage_dir: base.storage_dir,
            paa_trust_store: base.paa_trust_store,
            bypass_attestation: base.bypass_attestation,
            endpoint: base.endpoint,
            subscribe_min_s: base.subscribe_min_s,
            subscribe_max_s: base.subscribe_max_s,
            timeout_s: base.timeout_s,
            commission_timeout_s: base.commission_timeout_s,
        }
    }
}

impl ChipToolSection {
    pub fn to_controller_config(&self) -> matter_controller::ChipToolConfig {
        matter_controller::ChipToolConfig {
            binary: self.binary.clone(),
            storage_dir: self.storage_dir.clone().filter(|s| !s.is_empty()),
            paa_trust_store: self.paa_trust_store.clone().filter(|s| !s.is_empty()),
            bypass_attestation: self.bypass_attestation,
            endpoint: self.endpoint,
            subscribe_min_s: self.subscribe_min_s,
            subscribe_max_s: self.subscribe_max_s,
            timeout_s: self.timeout_s,
            commission_timeout_s: self.commission_timeout_s,
        }
    }
}

#[derive(Debug, Clone, Deserialize)]
pub struct Schedule {
    pub days: Vec<String>,
    pub from: String,
    pub to: String,
    pub setpoint: f64,
}

#[derive(Debug, Clone, Deserialize)]
pub struct Room {
    pub name: String,
    pub nodes: Vec<u64>,
    #[serde(default)]
    pub schedules: Vec<Schedule>,
}

#[derive(Debug, Clone, Deserialize)]
#[serde(default)]
pub struct HeatSource {
    pub kind: String,
    pub operating_hours: Option<String>,
    pub night_shutdown: Option<String>,
}

impl Default for HeatSource {
    fn default() -> Self {
        Self { kind: "unknown".into(), operating_hours: None, night_shutdown: None }
    }
}

#[derive(Debug, Clone, Deserialize)]
#[serde(default)]
pub struct Config {
    pub listen: String,
    pub db_path: String,
    pub data_dir: String,
    pub snapshot_path: String,
    pub dataset_path: String,
    pub model_path: String,
    pub controller: ControllerKind,
    pub chip_tool: ChipToolSection,
    pub thread_cli_prefix: String,
    pub default_setpoint: f64,
    pub eco_setpoint: f64,
    pub presence_boost: f64,
    pub presence_timeout_minutes: f64,
    pub still_contact_timeout_minutes: f64,
    pub frost_setpoint: f64,
    pub rooms: Vec<Room>,
    pub latitude: Option<f64>,
    pub longitude: Option<f64>,
    pub weather_url: String,
    pub ntp_server: String,
    pub neighbours: Vec<[String; 2]>,
    pub heat_source: HeatSource,
}

impl Default for Config {
    fn default() -> Self {
        Self {
            listen: "0.0.0.0:8080".into(),
            db_path: "data/heatd.duckdb".into(),
            data_dir: "data".into(),
            snapshot_path: "data/snapshot.duckdb".into(),
            dataset_path: "secrets/thread_dataset.hex".into(),
            model_path: "models/room_model.onnx".into(),
            controller: ControllerKind::ChipTool,
            chip_tool: ChipToolSection::default(),
            thread_cli_prefix: "ot ".to_string(),
            default_setpoint: 18.0,
            eco_setpoint: 16.0,
            presence_boost: 1.0,
            presence_timeout_minutes: 30.0,
            still_contact_timeout_minutes: 180.0,
            frost_setpoint: 7.0,
            rooms: Vec::new(),
            latitude: None,
            longitude: None,
            weather_url: "https://api.open-meteo.com/v1/forecast".into(),
            ntp_server: "pool.ntp.org".into(),
            neighbours: Vec::new(),
            heat_source: HeatSource::default(),
        }
    }
}

impl Config {
    pub fn load(path: &Path) -> Result<Self> {
        if !path.exists() {
            return Ok(Self::default());
        }
        let text = std::fs::read_to_string(path).with_context(|| format!("read {}", path.display()))?;
        Self::parse(&text)
    }

    pub fn parse(text: &str) -> Result<Self> {
        let config: Self = toml::from_str(text)?;
        for room in &config.rooms {
            for schedule in &room.schedules {
                schedule.minutes()?;
                schedule.weekdays()?;
            }
        }
        for pair in &config.neighbours {
            if let Some(unknown) = pair.iter().find(|r| !config.rooms.iter().any(|room| &room.name == *r)) {
                return Err(anyhow!("neighbours: unknown room {unknown}"));
            }
        }
        Ok(config)
    }

    pub fn coordinates(&self) -> Option<(f64, f64)> {
        self.latitude.zip(self.longitude).filter(|(la, lo)| la.abs() <= 90.0 && lo.abs() <= 180.0)
    }

    #[cfg(test)]
    pub fn room_of_node(&self, node: u64) -> Option<&Room> {
        self.rooms.iter().find(|r| r.nodes.contains(&node))
    }

    pub fn all_nodes(&self) -> Vec<u64> {
        self.rooms.iter().flat_map(|r| r.nodes.iter().copied()).collect()
    }
}

fn parse_minutes(text: &str) -> Result<u32> {
    let (h, m) = text.split_once(':').ok_or_else(|| anyhow!("bad time {text}"))?;
    let (h, m): (u32, u32) = (h.trim().parse()?, m.trim().parse()?);
    if h > 24 || m > 59 || (h == 24 && m > 0) {
        return Err(anyhow!("bad time {text}"));
    }
    Ok(h * 60 + m)
}

impl Schedule {
    pub fn minutes(&self) -> Result<(u32, u32)> {
        Ok((parse_minutes(&self.from)?, parse_minutes(&self.to)?))
    }

    pub fn weekdays(&self) -> Result<Vec<Weekday>> {
        let mut out = Vec::new();
        for day in &self.days {
            match day.trim().to_ascii_lowercase().as_str() {
                "weekdays" => out.extend([Weekday::Mon, Weekday::Tue, Weekday::Wed, Weekday::Thu, Weekday::Fri]),
                "weekend" => out.extend([Weekday::Sat, Weekday::Sun]),
                other => out.push(other.parse::<Weekday>().map_err(|_| anyhow!("bad day {day}"))?),
            }
        }
        Ok(out)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_example_config() {
        let text = std::fs::read_to_string(concat!(env!("CARGO_MANIFEST_DIR"), "/../../heatd.example.toml")).unwrap();
        let config = Config::parse(&text).unwrap();
        assert_eq!(config.listen, "0.0.0.0:8080");
        assert!(!config.rooms.is_empty());
        assert_eq!(config.room_of_node(config.rooms[0].nodes[0]).unwrap().name, config.rooms[0].name);
    }

    #[test]
    fn defaults_and_validation() {
        let config = Config::parse("").unwrap();
        assert_eq!(config.db_path, "data/heatd.duckdb");
        assert_eq!(config.controller, ControllerKind::ChipTool);
        let chip = config.chip_tool.to_controller_config();
        assert_eq!((chip.storage_dir, chip.bypass_attestation, chip.commission_timeout_s), (None, false, 180));
        assert_eq!(chip.paa_trust_store.as_deref(), Some("/home/aapphh/snap/chip-tool/common/paa-root-certs"));
        let custom = Config::parse("[chip_tool]
storage_dir = ''
paa_trust_store = ''
bypass_attestation = true").unwrap().chip_tool.to_controller_config();
        assert_eq!((custom.storage_dir, custom.paa_trust_store, custom.bypass_attestation), (None, None, true));
        assert!(Config::parse("[[rooms]]\nname='a'\nnodes=[1]\n[[rooms.schedules]]\ndays=['Funday']\nfrom='06:00'\nto='07:00'\nsetpoint=20").is_err());
        assert!(Config::parse("[[rooms]]\nname='a'\nnodes=[1]\n[[rooms.schedules]]\ndays=['weekend']\nfrom='6am'\nto='07:00'\nsetpoint=20").is_err());
    }
}
