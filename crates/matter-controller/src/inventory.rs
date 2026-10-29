use crate::chip_tool::{clean, is_failure, payload};
use crate::NodeId;
use serde::Serialize;
use serde_json::{Map, Number, Value};

pub const VENDOR_CLUSTER_MIN: u32 = 0xFC00;
pub const GENERATED_COMMAND_LIST: u32 = 0xFFF8;
pub const ACCEPTED_COMMAND_LIST: u32 = 0xFFF9;
pub const EVENT_LIST: u32 = 0xFFFA;
pub const ATTRIBUTE_LIST: u32 = 0xFFFB;
pub const FEATURE_MAP: u32 = 0xFFFC;
pub const CLUSTER_REVISION: u32 = 0xFFFD;
pub const THREAD_DIAGNOSTICS_CLUSTER: u32 = 0x0035;
pub const POWER_SOURCE_CLUSTER: u32 = 0x002F;
pub const NEIGHBOR_TABLE: u32 = 0x0007;
pub const ROUTE_TABLE: u32 = 0x0008;

const GLOBALS: [(u32, &str); 6] = [
    (GENERATED_COMMAND_LIST, "GeneratedCommandList"),
    (ACCEPTED_COMMAND_LIST, "AcceptedCommandList"),
    (EVENT_LIST, "EventList"),
    (ATTRIBUTE_LIST, "AttributeList"),
    (FEATURE_MAP, "FeatureMap"),
    (CLUSTER_REVISION, "ClusterRevision"),
];

type Names = &'static [(u32, &'static str)];

struct Cluster {
    id: u32,
    name: &'static str,
    attributes: Names,
    writable: &'static [u32],
    events: Names,
    commands: Names,
}

const CLUSTERS: &[Cluster] = &[
    Cluster { id: 0x0003, name: "Identify", attributes: &[(0, "IdentifyTime"), (1, "IdentifyType")], writable: &[0], events: &[], commands: &[(0, "Identify"), (0x40, "TriggerEffect")] },
    Cluster { id: 0x0004, name: "Groups", attributes: &[(0, "NameSupport")], writable: &[], events: &[], commands: &[(0, "AddGroup"), (1, "ViewGroup"), (2, "GetGroupMembership"), (3, "RemoveGroup"), (4, "RemoveAllGroups"), (5, "AddGroupIfIdentifying")] },
    Cluster { id: 0x001D, name: "Descriptor", attributes: &[(0, "DeviceTypeList"), (1, "ServerList"), (2, "ClientList"), (3, "PartsList"), (4, "TagList")], writable: &[], events: &[], commands: &[] },
    Cluster {
        id: 0x001F,
        name: "Access Control",
        attributes: &[(0, "ACL"), (1, "Extension"), (2, "SubjectsPerAccessControlEntry"), (3, "TargetsPerAccessControlEntry"), (4, "AccessControlEntriesPerFabric")],
        writable: &[0, 1],
        events: &[(0, "AccessControlEntryChanged"), (1, "AccessControlExtensionChanged")],
        commands: &[],
    },
    Cluster {
        id: 0x0028,
        name: "Basic Information",
        attributes: &[
            (0, "DataModelRevision"), (1, "VendorName"), (2, "VendorID"), (3, "ProductName"), (4, "ProductID"), (5, "NodeLabel"), (6, "Location"), (7, "HardwareVersion"),
            (8, "HardwareVersionString"), (9, "SoftwareVersion"), (10, "SoftwareVersionString"), (11, "ManufacturingDate"), (12, "PartNumber"), (13, "ProductURL"),
            (14, "ProductLabel"), (15, "SerialNumber"), (16, "LocalConfigDisabled"), (17, "Reachable"), (18, "UniqueID"), (19, "CapabilityMinima"), (20, "ProductAppearance"),
            (21, "SpecificationVersion"), (22, "MaxPathsPerInvoke"),
        ],
        writable: &[5, 6, 16],
        events: &[(0, "StartUp"), (1, "ShutDown"), (2, "Leave"), (3, "ReachableChanged")],
        commands: &[],
    },
    Cluster {
        id: 0x002A,
        name: "OTA Software Update Requestor",
        attributes: &[(0, "DefaultOTAProviders"), (1, "UpdatePossible"), (2, "UpdateState"), (3, "UpdateStateProgress")],
        writable: &[0],
        events: &[(0, "StateTransition"), (1, "VersionApplied"), (2, "DownloadError")],
        commands: &[(0, "AnnounceOTAProvider")],
    },
    Cluster { id: 0x002B, name: "Localization Configuration", attributes: &[(0, "ActiveLocale"), (1, "SupportedLocales")], writable: &[0], events: &[], commands: &[] },
    Cluster { id: 0x002D, name: "Unit Localization", attributes: &[(0, "TemperatureUnit"), (1, "SupportedTemperatureUnits")], writable: &[0], events: &[], commands: &[] },
    Cluster {
        id: 0x002F,
        name: "Power Source",
        attributes: &[
            (0, "Status"), (1, "Order"), (2, "Description"), (3, "WiredAssessedInputVoltage"), (4, "WiredAssessedInputFrequency"), (5, "WiredCurrentType"),
            (6, "WiredAssessedCurrent"), (7, "WiredNominalVoltage"), (8, "WiredMaximumCurrent"), (9, "WiredPresent"), (10, "ActiveWiredFaults"), (11, "BatVoltage"),
            (12, "BatPercentRemaining"), (13, "BatTimeRemaining"), (14, "BatChargeLevel"), (15, "BatReplacementNeeded"), (16, "BatReplaceability"), (17, "BatPresent"),
            (18, "ActiveBatFaults"), (19, "BatReplacementDescription"), (20, "BatCommonDesignation"), (21, "BatANSIDesignation"), (22, "BatIECDesignation"),
            (23, "BatApprovedChemistry"), (24, "BatCapacity"), (25, "BatQuantity"), (26, "BatChargeState"), (27, "BatTimeToFullCharge"), (28, "BatFunctionalWhileCharging"),
            (29, "BatChargingCurrent"), (30, "ActiveBatChargeFaults"), (31, "EndpointList"),
        ],
        writable: &[],
        events: &[(0, "WiredFaultChange"), (1, "BatFaultChange"), (2, "BatChargeFaultChange")],
        commands: &[],
    },
    Cluster {
        id: 0x0030,
        name: "General Commissioning",
        attributes: &[(0, "Breadcrumb"), (1, "BasicCommissioningInfo"), (2, "RegulatoryConfig"), (3, "LocationCapability"), (4, "SupportsConcurrentConnection"), (5, "TCAcceptedVersion"), (6, "TCMinRequiredVersion"), (7, "TCAcknowledgements"), (8, "TCAcknowledgementsRequired"), (9, "TCUpdateDeadline")],
        writable: &[0],
        events: &[],
        commands: &[(0, "ArmFailSafe"), (1, "ArmFailSafeResponse"), (2, "SetRegulatoryConfig"), (3, "SetRegulatoryConfigResponse"), (4, "CommissioningComplete"), (5, "CommissioningCompleteResponse")],
    },
    Cluster {
        id: 0x0031,
        name: "Network Commissioning",
        attributes: &[(0, "MaxNetworks"), (1, "Networks"), (2, "ScanMaxTimeSeconds"), (3, "ConnectMaxTimeSeconds"), (4, "InterfaceEnabled"), (5, "LastNetworkingStatus"), (6, "LastNetworkID"), (7, "LastConnectErrorValue"), (8, "SupportedWiFiBands"), (9, "SupportedThreadFeatures"), (10, "ThreadVersion")],
        writable: &[4],
        events: &[],
        commands: &[(0, "ScanNetworks"), (2, "AddOrUpdateWiFiNetwork"), (3, "AddOrUpdateThreadNetwork"), (4, "RemoveNetwork"), (6, "ConnectNetwork"), (8, "ReorderNetwork")],
    },
    Cluster {
        id: 0x0033,
        name: "General Diagnostics",
        attributes: &[(0, "NetworkInterfaces"), (1, "RebootCount"), (2, "UpTime"), (3, "TotalOperationalHours"), (4, "BootReason"), (5, "ActiveHardwareFaults"), (6, "ActiveRadioFaults"), (7, "ActiveNetworkFaults"), (8, "TestEventTriggersEnabled")],
        writable: &[],
        events: &[(0, "HardwareFaultChange"), (1, "RadioFaultChange"), (2, "NetworkFaultChange"), (3, "BootReason")],
        commands: &[(0, "TestEventTrigger"), (1, "TimeSnapshot"), (2, "TimeSnapshotResponse")],
    },
    Cluster {
        id: 0x0034,
        name: "Software Diagnostics",
        attributes: &[(0, "ThreadMetrics"), (1, "CurrentHeapFree"), (2, "CurrentHeapUsed"), (3, "CurrentHeapHighWatermark")],
        writable: &[],
        events: &[(0, "SoftwareFault")],
        commands: &[(0, "ResetWatermarks")],
    },
    Cluster {
        id: 0x0035,
        name: "Thread Network Diagnostics",
        attributes: &[
            (0, "Channel"), (1, "RoutingRole"), (2, "NetworkName"), (3, "PanId"), (4, "ExtendedPanId"), (5, "MeshLocalPrefix"), (6, "OverrunCount"), (7, "NeighborTable"),
            (8, "RouteTable"), (9, "PartitionId"), (10, "Weighting"), (11, "DataVersion"), (12, "StableDataVersion"), (13, "LeaderRouterId"), (14, "DetachedRoleCount"),
            (15, "ChildRoleCount"), (16, "RouterRoleCount"), (17, "LeaderRoleCount"), (18, "AttachAttemptCount"), (19, "PartitionIdChangeCount"),
            (20, "BetterPartitionAttachAttemptCount"), (21, "ParentChangeCount"), (22, "TxTotalCount"), (23, "TxUnicastCount"), (24, "TxBroadcastCount"),
            (25, "TxAckRequestedCount"), (26, "TxAckedCount"), (27, "TxNoAckRequestedCount"), (28, "TxDataCount"), (29, "TxDataPollCount"), (30, "TxBeaconCount"),
            (31, "TxBeaconRequestCount"), (32, "TxOtherCount"), (33, "TxRetryCount"), (34, "TxDirectMaxRetryExpiryCount"), (35, "TxIndirectMaxRetryExpiryCount"),
            (36, "TxErrCcaCount"), (37, "TxErrAbortCount"), (38, "TxErrBusyChannelCount"), (39, "RxTotalCount"), (40, "RxUnicastCount"), (41, "RxBroadcastCount"),
            (42, "RxDataCount"), (43, "RxDataPollCount"), (44, "RxBeaconCount"), (45, "RxBeaconRequestCount"), (46, "RxOtherCount"), (47, "RxAddressFilteredCount"),
            (48, "RxDestAddrFilteredCount"), (49, "RxDuplicatedCount"), (50, "RxErrNoFrameCount"), (51, "RxErrUnknownNeighborCount"), (52, "RxErrInvalidSrcAddrCount"),
            (53, "RxErrSecCount"), (54, "RxErrFcsCount"), (55, "RxErrOtherCount"), (56, "ActiveTimestamp"), (57, "PendingTimestamp"), (58, "Delay"), (59, "SecurityPolicy"),
            (60, "ChannelPage0Mask"), (61, "OperationalDatasetComponents"), (62, "ActiveNetworkFaultsList"),
        ],
        writable: &[],
        events: &[(0, "ConnectionStatus"), (1, "NetworkFaultChange")],
        commands: &[(0, "ResetCounts")],
    },
    Cluster {
        id: 0x0038,
        name: "Time Synchronization",
        attributes: &[(0, "UTCTime"), (1, "Granularity"), (2, "TimeSource"), (3, "TrustedTimeSource"), (4, "DefaultNTP"), (5, "TimeZone"), (6, "DSTOffset"), (7, "LocalTime"), (8, "TimeZoneDatabase"), (9, "NTPServerAvailable"), (10, "TimeZoneListMaxSize"), (11, "DSTOffsetListMaxSize"), (12, "SupportsDNSResolve")],
        writable: &[],
        events: &[(0, "DSTTableEmpty"), (1, "DSTStatus"), (2, "TimeZoneStatus"), (3, "TimeFailure"), (4, "MissingTrustedTimeSource")],
        commands: &[(0, "SetUTCTime"), (1, "SetTrustedTimeSource"), (2, "SetTimeZone"), (3, "SetTimeZoneResponse"), (4, "SetDSTOffset"), (5, "SetDefaultNTP")],
    },
    Cluster {
        id: 0x003E,
        name: "Operational Credentials",
        attributes: &[(0, "NOCs"), (1, "Fabrics"), (2, "SupportedFabrics"), (3, "CommissionedFabrics"), (4, "TrustedRootCertificates"), (5, "CurrentFabricIndex")],
        writable: &[],
        events: &[],
        commands: &[(0, "AttestationRequest"), (2, "CertificateChainRequest"), (4, "CSRRequest"), (6, "AddNOC"), (7, "UpdateNOC"), (9, "UpdateFabricLabel"), (10, "RemoveFabric"), (11, "AddTrustedRootCertificate")],
    },
    Cluster {
        id: 0x003F,
        name: "Group Key Management",
        attributes: &[(0, "GroupKeyMap"), (1, "GroupTable"), (2, "MaxGroupsPerFabric"), (3, "MaxGroupKeysPerFabric")],
        writable: &[0],
        events: &[],
        commands: &[(0, "KeySetWrite"), (1, "KeySetRead"), (3, "KeySetRemove"), (4, "KeySetReadAllIndices")],
    },
    Cluster { id: 0x0040, name: "Fixed Label", attributes: &[(0, "LabelList")], writable: &[], events: &[], commands: &[] },
    Cluster { id: 0x0041, name: "User Label", attributes: &[(0, "LabelList")], writable: &[0], events: &[], commands: &[] },
    Cluster {
        id: 0x0046,
        name: "ICD Management",
        attributes: &[(0, "IdleModeDuration"), (1, "ActiveModeDuration"), (2, "ActiveModeThreshold"), (3, "RegisteredClients"), (4, "ICDCounter"), (5, "ClientsSupportedPerFabric"), (6, "UserActiveModeTriggerHint"), (7, "UserActiveModeTriggerInstruction"), (8, "OperatingMode"), (9, "MaximumCheckInBackoff")],
        writable: &[],
        events: &[],
        commands: &[(0, "RegisterClient"), (1, "RegisterClientResponse"), (2, "UnregisterClient"), (3, "StayActiveRequest"), (4, "StayActiveResponse")],
    },
    Cluster {
        id: 0x0201,
        name: "Thermostat",
        attributes: &[
            (0x00, "LocalTemperature"), (0x01, "OutdoorTemperature"), (0x02, "Occupancy"), (0x03, "AbsMinHeatSetpointLimit"), (0x04, "AbsMaxHeatSetpointLimit"),
            (0x05, "AbsMinCoolSetpointLimit"), (0x06, "AbsMaxCoolSetpointLimit"), (0x07, "PICoolingDemand"), (0x08, "PIHeatingDemand"), (0x09, "HVACSystemTypeConfiguration"),
            (0x10, "LocalTemperatureCalibration"), (0x11, "OccupiedCoolingSetpoint"), (0x12, "OccupiedHeatingSetpoint"), (0x13, "UnoccupiedCoolingSetpoint"),
            (0x14, "UnoccupiedHeatingSetpoint"), (0x15, "MinHeatSetpointLimit"), (0x16, "MaxHeatSetpointLimit"), (0x17, "MinCoolSetpointLimit"), (0x18, "MaxCoolSetpointLimit"),
            (0x19, "MinSetpointDeadBand"), (0x1A, "RemoteSensing"), (0x1B, "ControlSequenceOfOperation"), (0x1C, "SystemMode"), (0x1D, "AlarmMask"),
            (0x1E, "ThermostatRunningMode"), (0x20, "StartOfWeek"), (0x21, "NumberOfWeeklyTransitions"), (0x22, "NumberOfDailyTransitions"), (0x23, "TemperatureSetpointHold"),
            (0x24, "TemperatureSetpointHoldDuration"), (0x25, "ThermostatProgrammingOperationMode"), (0x29, "ThermostatRunningState"), (0x30, "SetpointChangeSource"),
            (0x31, "SetpointChangeAmount"), (0x32, "SetpointChangeSourceTimestamp"), (0x34, "OccupiedSetback"), (0x35, "OccupiedSetbackMin"), (0x36, "OccupiedSetbackMax"),
            (0x37, "UnoccupiedSetback"), (0x38, "UnoccupiedSetbackMin"), (0x39, "UnoccupiedSetbackMax"), (0x3A, "EmergencyHeatDelta"), (0x40, "ACType"), (0x41, "ACCapacity"),
            (0x42, "ACRefrigerantType"), (0x43, "ACCompressorType"), (0x44, "ACErrorCode"), (0x45, "ACLouverPosition"), (0x46, "ACCoilTemperature"), (0x47, "ACCapacityFormat"),
            (0x48, "PresetTypes"), (0x49, "ScheduleTypes"), (0x4A, "NumberOfPresets"), (0x4B, "NumberOfSchedules"), (0x4C, "NumberOfScheduleTransitions"),
            (0x4D, "NumberOfScheduleTransitionPerDay"), (0x4E, "ActivePresetHandle"), (0x4F, "ActiveScheduleHandle"), (0x50, "Presets"), (0x51, "Schedules"),
            (0x52, "SetpointHoldExpiryTimestamp"),
        ],
        writable: &[0x10, 0x11, 0x12, 0x13, 0x14, 0x15, 0x16, 0x17, 0x18, 0x19, 0x1A, 0x1B, 0x1C, 0x23, 0x24, 0x25, 0x34, 0x37, 0x3A, 0x40, 0x50, 0x51],
        events: &[],
        commands: &[(0x00, "SetpointRaiseLower"), (0x01, "SetWeeklySchedule"), (0x02, "GetWeeklySchedule"), (0x03, "ClearWeeklySchedule"), (0x05, "SetActiveScheduleRequest"), (0x06, "SetActivePresetRequest"), (0xFD, "AtomicResponse"), (0xFE, "AtomicRequest")],
    },
    Cluster { id: 0x0204, name: "Thermostat User Interface Configuration", attributes: &[(0, "TemperatureDisplayMode"), (1, "KeypadLockout"), (2, "ScheduleProgrammingVisibility")], writable: &[0, 1, 2], events: &[], commands: &[] },
];

const THERMOSTAT_FEATURES: [&str; 9] = [
    "Heating", "Cooling", "Occupancy", "ScheduleConfiguration", "Setback", "AutoMode", "LocalTemperatureNotExposed", "MatterScheduleConfiguration", "Presets",
];

fn cluster(id: u32) -> Option<&'static Cluster> {
    CLUSTERS.iter().find(|c| c.id == id)
}

fn lookup(names: Names, id: u32) -> Option<&'static str> {
    names.iter().find(|(i, _)| *i == id).map(|(_, n)| *n)
}

pub fn cluster_name(id: u32) -> Option<&'static str> {
    cluster(id).map(|c| c.name)
}

pub fn attribute_name(cluster_id: u32, attribute: u32) -> Option<&'static str> {
    lookup(&GLOBALS, attribute).or_else(|| cluster(cluster_id).and_then(|c| lookup(c.attributes, attribute)))
}

pub fn event_name(cluster_id: u32, event: u32) -> Option<&'static str> {
    cluster(cluster_id).and_then(|c| lookup(c.events, event))
}

pub fn command_name(cluster_id: u32, command: u32) -> Option<&'static str> {
    cluster(cluster_id).and_then(|c| lookup(c.commands, command))
}

pub fn attribute_writable(cluster_id: u32, attribute: u32) -> Option<bool> {
    attribute_name(cluster_id, attribute)?;
    Some(cluster(cluster_id).is_some_and(|c| c.writable.contains(&attribute)))
}

pub fn is_vendor_id(id: u32) -> bool {
    id > 0xFFFF || (VENDOR_CLUSTER_MIN..0xFFF8).contains(&id)
}

pub fn thermostat_features(map: u64) -> Vec<&'static str> {
    THERMOSTAT_FEATURES.iter().enumerate().filter(|(bit, _)| map & (1 << bit) != 0).map(|(_, n)| *n).collect()
}

pub fn hex_id(id: u32) -> String {
    if id > 0xFFFF {
        format!("0x{:04X}_{:04X}", id >> 16, id & 0xFFFF)
    } else {
        format!("0x{:04X}", id)
    }
}

fn usable_name(name: &str) -> bool {
    !name.is_empty() && name.chars().all(|c| c.is_ascii_alphanumeric()) && !["tlv", "unknown"].contains(&name.to_ascii_lowercase().as_str())
}

#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct AttributeRecord {
    pub endpoint: u16,
    pub cluster_id: u32,
    pub cluster_name: String,
    pub attribute_id: u32,
    pub attribute_name: String,
    pub value: Value,
    pub data_version: Option<u64>,
    pub writable: Option<bool>,
    pub vendor: bool,
}

#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct EventRecord {
    pub endpoint: u16,
    pub cluster_id: u32,
    pub cluster_name: String,
    pub event_id: u32,
    pub event_name: String,
    pub number: Option<u64>,
    pub priority: String,
    pub timestamp: String,
    pub data: Value,
    pub vendor: bool,
}

#[derive(Debug, Clone, Default, PartialEq, Serialize)]
pub struct Inventory {
    pub node: NodeId,
    pub attributes: Vec<AttributeRecord>,
    pub events: Vec<EventRecord>,
    pub errors: Vec<String>,
    #[serde(skip)]
    pub raw_attributes: String,
    #[serde(skip)]
    pub raw_events: String,
}

impl Inventory {
    pub fn from_raw(node: NodeId, raw_attributes: String, raw_events: String) -> Self {
        let (attributes, mut errors) = parse_attributes(&raw_attributes);
        let (events, event_errors) = parse_events(&raw_events);
        for error in event_errors {
            if !errors.contains(&error) {
                errors.push(error);
            }
        }
        Self { node, attributes, events, errors, raw_attributes, raw_events }
    }

    pub fn attribute(&self, cluster_id: u32, attribute_id: u32) -> Option<&AttributeRecord> {
        self.attributes.iter().find(|a| a.cluster_id == cluster_id && a.attribute_id == attribute_id)
    }

    pub fn value(&self, cluster_id: u32, attribute_id: u32) -> Option<&Value> {
        self.attribute(cluster_id, attribute_id).map(|a| &a.value)
    }
}

struct Frame {
    indent: usize,
    key: String,
    list: bool,
    items: Vec<Value>,
    fields: Map<String, Value>,
}

impl Frame {
    fn new(indent: usize, key: &str, list: bool) -> Self {
        Self { indent, key: key.to_string(), list, items: Vec::new(), fields: Map::new() }
    }

    fn put(&mut self, key: String, value: Value) {
        if self.list {
            self.items.push(value);
        } else {
            self.fields.insert(key, value);
        }
    }

    fn into_value(self) -> Value {
        if self.list {
            Value::Array(self.items)
        } else {
            Value::Object(self.fields)
        }
    }
}

#[derive(Default)]
struct Meta {
    number: Option<u64>,
    priority: String,
    timestamp: String,
}

enum Kind {
    Attribute { version: Option<u64> },
    Event { meta: Meta },
}

const UNDECODED: &str = "Don't know how to log";
pub const UNDECODED_VALUE: &str = "<not decoded by chip-tool, see raw log>";

struct Section {
    undecoded: bool,
    endpoint: u16,
    cluster: u32,
    id: u32,
    kind: Kind,
    stack: Vec<Frame>,
}

pub fn parse_number(token: &str) -> Option<u64> {
    let t = token.trim().trim_end_matches(',').replace('_', "");
    t.strip_prefix("0x").map_or_else(|| t.parse().ok(), |h| u64::from_str_radix(h, 16).ok())
}

fn id_with_name(text: &str) -> Option<u64> {
    let (number, rest) = text.split_once(' ')?;
    (rest.starts_with('(') && rest.ends_with(')') && number.bytes().all(|b| b.is_ascii_digit())).then(|| number.parse().ok()).flatten()
}

pub fn parse_scalar(text: &str) -> Value {
    let t = text.trim();
    match t {
        "null" => Value::Null,
        _ if t.eq_ignore_ascii_case("true") => Value::Bool(true),
        _ if t.eq_ignore_ascii_case("false") => Value::Bool(false),
        _ => {
            if let Some(n) = id_with_name(t) {
                Value::from(n)
            } else if let Some(s) = t.strip_prefix('"').and_then(|s| s.strip_suffix('"')) {
                Value::String(s.to_string())
            } else if let Ok(i) = t.parse::<i64>() {
                Value::from(i)
            } else if let Ok(u) = t.parse::<u64>() {
                Value::from(u)
            } else if let Some(n) = t.contains('.').then(|| t.parse::<f64>().ok().and_then(Number::from_f64)).flatten() {
                Value::Number(n)
            } else {
                Value::String(t.to_string())
            }
        }
    }
}

fn after<'a>(text: &'a str, key: &str) -> Option<&'a str> {
    text.find(key).map(|i| text[i + key.len()..].trim_start())
}

fn first_token(text: &str) -> &str {
    text.split_whitespace().next().unwrap_or("")
}

fn meta_key(key: &str) -> String {
    key.chars().filter(|c| c.is_ascii_alphanumeric()).collect::<String>().to_ascii_lowercase()
}

fn is_entries(text: &str) -> bool {
    let mut parts = text.split_whitespace();
    matches!((parts.next(), parts.next(), parts.next()), (Some(n), Some("entries" | "entry"), None) if n.bytes().all(|b| b.is_ascii_digit()))
}

impl Section {
    fn start(body: &str) -> Option<Self> {
        let rest = body.trim_start().strip_prefix("Endpoint:")?;
        let mut tokens = rest.split_whitespace();
        let endpoint = u16::try_from(parse_number(tokens.next()?)?).ok()?;
        if tokens.next()? != "Cluster:" {
            return None;
        }
        let cluster = u32::try_from(parse_number(tokens.next()?)?).ok()?;
        let kind = tokens.next()?;
        let id = u32::try_from(parse_number(tokens.next()?)?).ok()?;
        let kind = match kind {
            "Attribute" => Kind::Attribute { version: after(rest, "DataVersion:").and_then(|t| parse_number(first_token(t))) },
            "Event" => Kind::Event {
                meta: Meta {
                    number: after(rest, "EventNumber:").and_then(|t| parse_number(first_token(t))),
                    priority: after(rest, "Priority:").map(|t| first_token(t).to_string()).unwrap_or_default(),
                    timestamp: after(rest, "Timestamp:").map(|t| t.trim().to_string()).unwrap_or_default(),
                },
            },
            _ => return None,
        };
        Some(Self { undecoded: false, endpoint, cluster, id, kind, stack: vec![Frame::new(0, "", false)] })
    }

    fn collapse(&mut self) {
        let frame = self.stack.pop().expect("root frame");
        let key = frame.key.clone();
        self.stack.last_mut().expect("parent frame").put(key, frame.into_value());
    }

    fn feed(&mut self, body: &str) -> bool {
        let indent = body.len() - body.trim_start().len();
        let text = body.trim();
        if text.is_empty() {
            return true;
        }
        if text == "}" {
            while self.stack.len() > 1 {
                let was_struct = !self.stack.last().is_some_and(|f| f.list);
                self.collapse();
                if was_struct {
                    break;
                }
            }
            return true;
        }
        if text.starts_with(UNDECODED) {
            self.undecoded = true;
            return true;
        }
        let Some((key, value)) = text.split_once(": ").or_else(|| text.strip_suffix(':').map(|k| (k, ""))) else {
            return false;
        };
        if let (Kind::Event { meta }, 1) = (&mut self.kind, self.stack.len()) {
            match meta_key(key).as_str() {
                "eventnumber" | "number" => {
                    meta.number = parse_number(first_token(value));
                    return true;
                }
                "priority" => {
                    meta.priority = value.trim().to_string();
                    return true;
                }
                "timestamp" => {
                    meta.timestamp = value.trim().to_string();
                    return true;
                }
                _ => {}
            }
        }
        if indent < 2 {
            return false;
        }
        while self.stack.len() > 1 && self.stack.last().is_some_and(|f| f.indent >= indent) {
            self.collapse();
        }
        let value = value.trim();
        if value == "{" {
            self.stack.push(Frame::new(indent, key, false));
        } else if is_entries(value) {
            self.stack.push(Frame::new(indent, key, true));
        } else {
            self.stack.last_mut().expect("root frame").put(key.to_string(), parse_scalar(value));
        }
        true
    }

    fn finish(mut self) -> (u16, u32, u32, Kind, Map<String, Value>) {
        while self.stack.len() > 1 {
            self.collapse();
        }
        let mut fields = self.stack.pop().map(|f| f.fields).unwrap_or_default();
        if self.undecoded && fields.is_empty() {
            fields.insert(String::new(), Value::String(UNDECODED_VALUE.into()));
        }
        (self.endpoint, self.cluster, self.id, self.kind, fields)
    }
}

fn single(fields: Map<String, Value>) -> (Option<String>, Value) {
    match fields.len() {
        0 => (None, Value::Null),
        1 => fields.into_iter().next().map(|(k, v)| (Some(k), v)).unwrap_or((None, Value::Null)),
        _ => (None, Value::Object(fields)),
    }
}

fn walk(output: &str, mut on_section: impl FnMut(Section)) -> Vec<String> {
    let mut errors: Vec<String> = Vec::new();
    let mut current: Option<Section> = None;
    for raw in output.lines() {
        let line = clean(raw);
        if is_failure(&line) {
            let text = payload(&line).unwrap_or(&line).trim().to_string();
            if !errors.contains(&text) {
                errors.push(text);
            }
        }
        let Some(body) = payload(&line) else { continue };
        if let Some(section) = Section::start(body) {
            current.take().into_iter().for_each(&mut on_section);
            current = Some(section);
        } else if let Some(section) = current.as_mut() {
            if !section.feed(body) {
                current.take().into_iter().for_each(&mut on_section);
            }
        }
    }
    current.into_iter().for_each(on_section);
    errors
}

fn display_cluster(id: u32) -> String {
    cluster_name(id).map(str::to_string).unwrap_or_else(|| format!("Cluster{}", hex_id(id)))
}

pub fn parse_attributes(output: &str) -> (Vec<AttributeRecord>, Vec<String>) {
    let mut records = Vec::new();
    let errors = walk(output, |section| {
        let (endpoint, cluster_id, attribute_id, kind, fields) = section.finish();
        let Kind::Attribute { version } = kind else { return };
        let (printed, value) = single(fields);
        let attribute_name = attribute_name(cluster_id, attribute_id)
            .map(str::to_string)
            .or(printed.filter(|p| usable_name(p)))
            .unwrap_or_else(|| format!("Attribute{}", hex_id(attribute_id)));
        records.push(AttributeRecord {
            endpoint,
            cluster_id,
            cluster_name: display_cluster(cluster_id),
            attribute_id,
            attribute_name,
            value,
            data_version: version,
            writable: attribute_writable(cluster_id, attribute_id),
            vendor: is_vendor_id(cluster_id) || is_vendor_id(attribute_id),
        });
    });
    (records, errors)
}

pub fn parse_events(output: &str) -> (Vec<EventRecord>, Vec<String>) {
    let mut records = Vec::new();
    let errors = walk(output, |section| {
        let (endpoint, cluster_id, event_id, kind, fields) = section.finish();
        let Kind::Event { meta } = kind else { return };
        let (printed, data) = single(fields);
        records.push(EventRecord {
            endpoint,
            cluster_id,
            cluster_name: display_cluster(cluster_id),
            event_id,
            event_name: event_name(cluster_id, event_id).map(str::to_string).or(printed.filter(|p| usable_name(p))).unwrap_or_else(|| format!("Event{}", hex_id(event_id))),
            number: meta.number,
            priority: meta.priority,
            timestamp: meta.timestamp,
            data,
            vendor: is_vendor_id(cluster_id) || is_vendor_id(event_id),
        });
    });
    (records, errors)
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn fixture() -> Inventory {
        Inventory::from_raw(5, include_str!("../tests/fixtures/inventory_attributes.txt").into(), include_str!("../tests/fixtures/inventory_events.txt").into())
    }

    #[test]
    fn parses_scalars_lists_and_structs() {
        let inv = fixture();
        assert_eq!(inv.value(0x201, 0x00), Some(&json!(2150)));
        assert_eq!(inv.value(0x201, 0x01), Some(&Value::Null));
        assert_eq!(inv.value(0x28, 0x01), Some(&json!("Eurotronic")));
        assert_eq!(inv.value(0x28, 0x05), Some(&json!("")));
        assert_eq!(inv.value(0x2F, 0x11), Some(&json!(true)));
        assert_eq!(inv.value(0x201, 0xFFF9), Some(&json!([0, 1, 2])));
        assert_eq!(inv.value(0x201, 0xFFF8), Some(&json!([])));
        assert_eq!(inv.value(0x1D, 0x00), Some(&json!([{"DeviceType": 22, "Revision": 1}])));
        assert_eq!(inv.value(0x28, 0x13), Some(&json!({"CaseSessionsPerFabric": 3, "SubscriptionsPerFabric": 3})));
        assert_eq!(inv.value(0x35, 0x08), Some(&json!([])));
        let neighbors = inv.value(0x35, NEIGHBOR_TABLE).and_then(Value::as_array).unwrap();
        assert_eq!(neighbors.len(), 2);
        assert_eq!(neighbors[0]["Rloc16"], json!(20480));
        assert_eq!(neighbors[0]["ExtAddress"], json!(1311768467294899695u64));
        assert_eq!(neighbors[1]["AverageRssi"], Value::Null);
        assert_eq!(neighbors[1]["IsChild"], json!(true));
    }

    #[test]
    fn names_ids_and_flags_attributes() {
        let inv = fixture();
        let setpoint = inv.attribute(0x201, 0x12).unwrap();
        assert_eq!((setpoint.endpoint, setpoint.cluster_name.as_str(), setpoint.attribute_name.as_str()), (1, "Thermostat", "OccupiedHeatingSetpoint"));
        assert_eq!((setpoint.writable, setpoint.vendor, setpoint.data_version), (Some(true), false, Some(3037862385)));
        assert_eq!(inv.attribute(0x201, 0x00).unwrap().writable, Some(false));
        assert_eq!(inv.attribute(0x201, FEATURE_MAP).unwrap().attribute_name, "FeatureMap");
        assert_eq!(inv.attribute(0x201, ATTRIBUTE_LIST).unwrap().writable, Some(false));
        let vendor = inv.attribute(0xFFF1_FC01, 0xFFF1_0001).unwrap();
        assert!(vendor.vendor);
        assert_eq!((vendor.cluster_name.as_str(), vendor.attribute_name.as_str(), vendor.writable), ("Cluster0xFFF1_FC01", "Attribute0xFFF1_0001", None));
    }

    #[test]
    fn vendor_attribute_keeps_printed_name_and_every_record_is_kept() {
        let inv = fixture();
        assert_eq!(inv.attribute(0xFFF1_FC01, 0xFFF1_0002).unwrap().attribute_name, "VendorTrvMode");
        assert_eq!(inv.attributes.len(), 44);
        assert_eq!(inv.attributes.iter().filter(|a| a.vendor).count(), 2);
        assert!(inv.errors.iter().any(|e| e.contains("UNSUPPORTED_CLUSTER")));
        assert_eq!(inv.errors.len(), 1);
    }

    #[test]
    fn parses_events_with_metadata_and_data() {
        let inv = fixture();
        let names: Vec<_> = inv.events.iter().map(|e| (e.cluster_name.as_str(), e.event_name.as_str(), e.number, e.priority.as_str())).collect();
        assert_eq!(
            names,
            vec![
                ("Basic Information", "StartUp", Some(1), "Critical"),
                ("General Diagnostics", "BootReason", Some(2), "Critical"),
                ("Basic Information", "ReachableChanged", Some(3), "Info"),
                ("Thread Network Diagnostics", "NetworkFaultChange", Some(4), "Info"),
                ("Cluster0xFFF1_FC01", "Event0xFFF1_0001", Some(5), "Debug"),
            ]
        );
        assert_eq!(inv.events[0].timestamp, "5995103");
        assert_eq!(inv.events[0].data, json!({"SoftwareVersion": 66}));
        assert_eq!(inv.events[3].data, json!({"Current": [], "Previous": [2]}));
        assert!(inv.events[4].vendor && !inv.events[0].vendor);
    }

    #[test]
    fn parses_event_metadata_on_the_header_line() {
        let raw = "[1.0] [1:2] [TOO] Endpoint: 0 Cluster: 0x0000_0028 Event 0x0000_0001 EventNumber: 0x10 Priority: Info Timestamp: System Time = 28 ms\n[1.0] [1:2] [TOO]   ShutDown: {\n[1.0] [1:2] [TOO]    }\n";
        let (events, errors) = parse_events(raw);
        assert!(errors.is_empty());
        assert_eq!((events[0].number, events[0].priority.as_str(), events[0].timestamp.as_str(), events[0].event_name.as_str()), (Some(16), "Info", "System Time = 28 ms", "ShutDown"));
    }

    #[test]
    fn decodes_features_and_commands() {
        assert_eq!(thermostat_features(35), vec!["Heating", "Cooling", "AutoMode"]);
        assert_eq!(thermostat_features(0x181), vec!["Heating", "MatterScheduleConfiguration", "Presets"]);
        assert_eq!(command_name(0x201, 0), Some("SetpointRaiseLower"));
        assert_eq!(command_name(0x201, 0x99), None);
        assert!(is_vendor_id(0xFFF1_FC01) && is_vendor_id(0xFC10) && !is_vendor_id(0xFFFC) && !is_vendor_id(0x201));
    }

    #[test]
    fn real_chip_tool_formats_booleans_id_lists_and_undecoded_values() {
        assert_eq!((parse_scalar("TRUE"), parse_scalar("FALSE")), (json!(true), json!(false)));
        assert_eq!(parse_scalar("0 (SetpointRaiseLower)"), json!(0));
        assert_eq!(parse_scalar("1.10"), json!(1.1));
        let text = "[TOO] Endpoint: 1 Cluster: 0x0000_0201 Attribute 0x0000_FFF9 DataVersion: 1
[TOO]   AcceptedCommandList: 1 entries
[TOO]     [1]: 0 (SetpointRaiseLower)
[TOO] Endpoint: 1 Cluster: 0xFFF1_FC01 Attribute 0xFFF1_0001 DataVersion: 1
[TOO]   Don't know how to log attribute value
";
        let (records, _) = parse_attributes(text);
        assert_eq!(records[0].value, json!([0]));
        assert_eq!(records[1].value, json!(UNDECODED_VALUE));
    }

    #[test]
    fn empty_output_yields_nothing() {
        let inv = Inventory::from_raw(1, "[1.0] [1:2] [TOO] Sending command".into(), String::new());
        assert!(inv.attributes.is_empty() && inv.events.is_empty() && inv.errors.is_empty());
    }
}
