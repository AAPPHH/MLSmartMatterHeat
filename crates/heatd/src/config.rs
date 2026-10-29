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
#[serde(default)]
pub struct Exploration {
    pub enabled: bool,
    pub p_per_day: f64,
    pub comfort_min: f64,
    pub comfort_max: f64,
    pub blackout: Vec<Blackout>,
}

impl Default for Exploration {
    fn default() -> Self {
        Self { enabled: false, p_per_day: 1.0, comfort_min: 16.0, comfort_max: 23.0, blackout: Vec::new() }
    }
}

#[derive(Debug, Clone, Deserialize)]
pub struct Blackout {
    pub from: String,
    pub to: String,
}

impl Blackout {
    pub fn minutes(&self) -> Result<(u32, u32)> {
        Ok((parse_minutes(&self.from)?, parse_minutes(&self.to)?))
    }

    pub fn contains(&self, minute: u32) -> bool {
        self.minutes().is_ok_and(|(from, to)| if from == to { false } else if from < to { (from..to).contains(&minute) } else { minute >= from || minute < to })
    }
}

#[derive(Debug, Clone, Deserialize)]
pub struct Room {
    pub name: String,
    #[serde(default)]
    pub nodes: Vec<u64>,
    #[serde(default)]
    pub schedules: Vec<Schedule>,
    #[serde(default)]
    pub exploration: Exploration,
    #[serde(default)]
    pub exterior: Vec<String>,
}

pub const NEIGHBOUR_KINDS: [&str; 4] = ["unknown", "wall", "ceiling", "open"];

#[derive(Deserialize)]
#[serde(untagged)]
enum RawNeighbour {
    Pair([String; 2]),
    Link {
        a: String,
        b: String,
        #[serde(default = "unknown_kind")]
        kind: String,
        #[serde(default)]
        partial: bool,
    },
}

fn unknown_kind() -> String {
    "unknown".into()
}

#[derive(Debug, Clone, Deserialize, PartialEq)]
#[serde(from = "RawNeighbour")]
pub struct Neighbour {
    pub a: String,
    pub b: String,
    pub kind: String,
    pub partial: bool,
}

impl From<RawNeighbour> for Neighbour {
    fn from(raw: RawNeighbour) -> Self {
        match raw {
            RawNeighbour::Pair([a, b]) => Self { a, b, kind: unknown_kind(), partial: false },
            RawNeighbour::Link { a, b, kind, partial } => Self { a, b, kind, partial },
        }
    }
}

impl Neighbour {
    #[cfg(test)]
    pub fn pair(a: &str, b: &str) -> Self {
        Self { a: a.into(), b: b.into(), kind: unknown_kind(), partial: false }
    }
}

#[derive(Debug, Clone, Default, Deserialize)]
#[serde(default)]
pub struct Building {
    pub floor: Option<i32>,
    pub roof_rooms: Vec<String>,
}

#[derive(Debug, Clone, Deserialize)]
#[serde(default)]
pub struct HeatSource {
    pub kind: String,
    pub operating_hours: Option<String>,
    pub night_shutdown: Option<String>,
    pub scope: Option<String>,
    pub efficiency_class: Option<String>,
    pub flow_temp_controllable: Option<bool>,
    pub night_setback_controllable: Option<bool>,
}

impl Default for HeatSource {
    fn default() -> Self {
        Self { kind: "unknown".into(), operating_hours: None, night_shutdown: None, scope: None, efficiency_class: None, flow_temp_controllable: None, night_setback_controllable: None }
    }
}

#[derive(Debug, Clone, Deserialize)]
#[serde(default)]
pub struct WatchState {
    pub hr_rest_max: f64,
    pub hr_active_min: f64,
    pub motion_active_min: f64,
    pub sleep_hours: String,
}

impl Default for WatchState {
    fn default() -> Self {
        Self { hr_rest_max: 70.0, hr_active_min: 90.0, motion_active_min: 0.5, sleep_hours: "23-7".into() }
    }
}

impl WatchState {
    pub fn sleep_minutes(&self) -> Result<(u32, u32)> {
        let (from, to) = self.sleep_hours.split_once('-').ok_or_else(|| anyhow!("bad sleep_hours {}", self.sleep_hours))?;
        let minutes = |t: &str| if t.contains(':') { parse_minutes(t) } else { parse_minutes(&format!("{}:00", t.trim())) };
        Ok((minutes(from)?, minutes(to)?))
    }
}

#[derive(Debug, Clone, Deserialize)]
#[serde(default)]
pub struct WatchMatchSection {
    pub enabled: bool,
    pub model_dir: String,
    pub model_version: String,
    pub scan_fresh_s: i64,
    pub labels_since: Option<String>,
    pub label_target: usize,
    pub state: WatchState,
}

impl Default for WatchMatchSection {
    fn default() -> Self {
        Self { enabled: false, model_dir: "/models/watchroom".into(), model_version: "latest".into(), scan_fresh_s: 180, labels_since: None, label_target: 10, state: WatchState::default() }
    }
}

#[derive(Debug, Clone, Deserialize)]
#[serde(default)]
pub struct WindowSection {
    pub enabled: bool,
    pub window_slope_min: f64,
    pub open_drop_k: f64,
    pub open_window_min: f64,
    pub open_slope_k_per_h: f64,
    pub open_sustain_min: f64,
    pub setpoint_guard_min: f64,
    pub min_outdoor_delta_k: f64,
    pub cooldown_grace_min: f64,
    pub expected_cooldown_factor: f64,
    pub fallback_cooldown_k_per_h: f64,
    pub cooldown_sample_min: f64,
    pub cooldown_days: f64,
    pub wind_ms: f64,
    pub wind_relax: f64,
    pub min_open_min: f64,
    pub close_rise_k: f64,
    pub close_window_min: f64,
    pub max_open_min: f64,
    pub max_timeout_min: f64,
    pub probe_min: f64,
    pub probe_drop_k: f64,
    pub max_events_per_hour: usize,
    pub valve_low_pct: f64,
    pub radiator_cold_min: f64,
    pub manual_max_min: f64,
}

impl Default for WindowSection {
    fn default() -> Self {
        Self {
            enabled: true,
            window_slope_min: 6.0,
            open_drop_k: 1.0,
            open_window_min: 10.0,
            open_slope_k_per_h: -4.0,
            open_sustain_min: 4.0,
            setpoint_guard_min: 20.0,
            min_outdoor_delta_k: 5.0,
            cooldown_grace_min: 30.0,
            expected_cooldown_factor: 1.5,
            fallback_cooldown_k_per_h: 20.0,
            cooldown_sample_min: 15.0,
            cooldown_days: 14.0,
            wind_ms: 6.0,
            wind_relax: 0.2,
            min_open_min: 5.0,
            close_rise_k: 0.3,
            close_window_min: 10.0,
            max_open_min: 20.0,
            max_timeout_min: 60.0,
            probe_min: 10.0,
            probe_drop_k: 0.5,
            max_events_per_hour: 4,
            valve_low_pct: 10.0,
            radiator_cold_min: 45.0,
            manual_max_min: 720.0,
        }
    }
}

#[derive(Debug, Clone, Deserialize)]
#[serde(default)]
pub struct Config {
    pub listen: String,
    pub db_path: String,
    pub data_dir: String,
    pub inventory_dir: String,
    pub snapshot_path: String,
    pub dataset_path: String,
    pub model_path: String,
    pub controller: ControllerKind,
    pub chip_tool: ChipToolSection,
    pub thread_cli_prefix: String,
    pub default_setpoint: f64,
    pub away_setpoint: f64,
    pub away_guard_minutes: f64,
    pub exploration_seed: Option<u64>,
    pub occupied_setpoint: f64,
    pub occupancy_enter_min: f64,
    pub occupancy_exit_min: f64,
    pub min_write_interval_min: f64,
    pub setpoint_deadband: f64,
    pub sleep_room: Option<String>,
    pub presence_timeout_minutes: f64,
    pub still_contact_timeout_minutes: f64,
    pub frost_setpoint: f64,
    pub rooms: Vec<Room>,
    pub latitude: Option<f64>,
    pub longitude: Option<f64>,
    pub weather_url: String,
    pub ntp_server: String,
    pub neighbours: Vec<Neighbour>,
    pub heat_source: HeatSource,
    pub building: Building,
    pub manual_hold_minutes: f64,
    pub poll_silence_s: u64,
    pub watchmatch: WatchMatchSection,
    pub window: WindowSection,
}

impl Default for Config {
    fn default() -> Self {
        Self {
            listen: "0.0.0.0:8080".into(),
            db_path: "data/heatd.duckdb".into(),
            data_dir: "data".into(),
            inventory_dir: "inventory".into(),
            snapshot_path: "data/snapshot.duckdb".into(),
            dataset_path: "secrets/thread_dataset.hex".into(),
            model_path: "models/room_model.onnx".into(),
            controller: ControllerKind::ChipTool,
            chip_tool: ChipToolSection::default(),
            thread_cli_prefix: "ot ".to_string(),
            default_setpoint: 18.0,
            away_setpoint: 18.0,
            away_guard_minutes: 20.0,
            exploration_seed: None,
            occupied_setpoint: 21.0,
            occupancy_enter_min: 10.0,
            occupancy_exit_min: 45.0,
            min_write_interval_min: 15.0,
            setpoint_deadband: 0.3,
            sleep_room: None,
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
            building: Building::default(),
            manual_hold_minutes: 60.0,
            poll_silence_s: 60,
            watchmatch: WatchMatchSection::default(),
            window: WindowSection::default(),
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
            for blackout in &room.exploration.blackout {
                blackout.minutes()?;
            }
        }
        let known = |name: &String| config.rooms.iter().any(|room| &room.name == name);
        for link in &config.neighbours {
            if let Some(unknown) = [&link.a, &link.b].into_iter().find(|r| !known(r)) {
                return Err(anyhow!("neighbours: unknown room {unknown}"));
            }
            if !NEIGHBOUR_KINDS.contains(&link.kind.as_str()) {
                return Err(anyhow!("neighbours: unknown kind {} (expected {})", link.kind, NEIGHBOUR_KINDS.join(", ")));
            }
        }
        config.watchmatch.state.sleep_minutes()?;
        if let Some(since) = &config.watchmatch.labels_since {
            chrono::DateTime::parse_from_rfc3339(since).map_err(|e| anyhow!("watchmatch.labels_since: {e}"))?;
        }
        if let Some(unknown) = config.sleep_room.as_ref().filter(|r| !known(r)) {
            return Err(anyhow!("sleep_room: unknown room {unknown}"));
        }
        if let Some(unknown) = config.building.roof_rooms.iter().find(|r| !known(r)) {
            return Err(anyhow!("building.roof_rooms: unknown room {unknown}"));
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
    fn docker_config_describes_the_home() {
        let text = std::fs::read_to_string(concat!(env!("CARGO_MANIFEST_DIR"), "/../../deploy/docker/heatd.docker.toml")).unwrap();
        for config in [Config::parse(&text).unwrap(), Config::parse(&std::fs::read_to_string(concat!(env!("CARGO_MANIFEST_DIR"), "/../../heatd.example.toml")).unwrap()).unwrap()] {
            let nodes: Vec<(&str, Vec<u64>)> = config.rooms.iter().map(|r| (r.name.as_str(), r.nodes.clone())).collect();
            assert_eq!(nodes, vec![("wohnzimmer", vec![5]), ("schlafzimmer", vec![6]), ("bad", vec![7]), ("kueche", vec![8]), ("flur", vec![])]);
            assert_eq!(config.all_nodes(), vec![5, 6, 7, 8]);
            assert_eq!((config.latitude, config.longitude, config.away_setpoint), (Some(52.52), Some(13.40), 18.0));
            assert_eq!(config.sleep_room.as_deref(), Some("schlafzimmer"));
            assert_eq!((config.chip_tool.subscribe_min_s, config.poll_silence_s, config.manual_hold_minutes), (30, 60, 60.0));
            assert_eq!(config.neighbours.iter().filter(|n| n.kind == "wall").count(), 5);
            assert_eq!(config.neighbours.iter().filter(|n| n.kind == "ceiling").count(), 4);
            assert!(config.neighbours.iter().any(|n| n.kind == "ceiling" && n.partial) && config.neighbours.iter().any(|n| n.kind == "open"));
            assert_eq!((config.heat_source.kind.as_str(), config.heat_source.efficiency_class.as_deref(), config.heat_source.flow_temp_controllable), ("oil", Some("E"), Some(false)));
            assert!(config.rooms.iter().all(|r| !r.exploration.enabled));
        }
        let docker = Config::parse(&text).unwrap();
        assert_eq!((docker.occupied_setpoint, docker.frost_setpoint, docker.watchmatch.label_target), (20.0, 8.0, 10));
        assert_eq!(docker.watchmatch.labels_since.as_deref(), Some("2026-10-29T20:28:00+01:00"));
        assert!(docker.rooms.iter().all(|r| r.schedules.is_empty()));
        let example = Config::parse(&std::fs::read_to_string(concat!(env!("CARGO_MANIFEST_DIR"), "/../../heatd.example.toml")).unwrap()).unwrap();
        assert_eq!(example.occupied_setpoint, 21.0);
        let kitchen = example.rooms.iter().find(|r| r.name == "kueche").unwrap();
        assert_eq!(kitchen.schedules.len(), 2);
        assert!(kitchen.schedules.iter().all(|s| s.setpoint == 21.0));
    }

    #[test]
    fn rooms_without_nodes_and_bad_kinds() {
        let config = Config::parse("[[rooms]]
name = 'flur'").unwrap();
        assert!(config.rooms[0].nodes.is_empty() && config.all_nodes().is_empty());
        assert!(Config::parse("neighbours = [{ a = 'a', b = 'a', kind = 'door' }]
[[rooms]]
name = 'a'").is_err());
    }

    #[test]
    fn defaults_and_validation() {
        let config = Config::parse("").unwrap();
        assert_eq!(config.db_path, "data/heatd.duckdb");
        assert_eq!((config.occupied_setpoint, config.sleep_room.clone()), (21.0, None));
        assert!(Config::parse("presence_boost = 1.0
sleep_room = 'x'").is_err());
        assert_eq!(Config::parse("presence_boost = 1.0").unwrap().occupied_setpoint, 21.0);
        assert_eq!(config.controller, ControllerKind::ChipTool);
        assert_eq!((config.watchmatch.enabled, config.watchmatch.model_dir.as_str(), config.watchmatch.model_version.as_str(), config.watchmatch.state.sleep_minutes().unwrap()), (false, "/models/watchroom", "latest", (1380, 420)));
        let tuned = Config::parse("[watchmatch]
enabled = true
[watchmatch.state]
hr_rest_max = 70
sleep_hours = '22-6'").unwrap();
        assert_eq!((tuned.watchmatch.enabled, tuned.watchmatch.state.hr_rest_max, tuned.watchmatch.state.hr_active_min, tuned.watchmatch.state.sleep_minutes().unwrap()), (true, 70.0, 90.0, (1320, 360)));
        assert!(Config::parse("[watchmatch.state]
sleep_hours = 'never'").is_err());
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
