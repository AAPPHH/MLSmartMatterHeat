use crate::chip_tool::{clean, payload, rx_peer};
use crate::inventory::{parse_attributes, parse_events, EventRecord};
use crate::{AttributeSpec, NodeId, EXTRA_SUBSCRIPTIONS, THERMOSTAT_CLUSTER, WATCHED};
use serde_json::Value;

pub const SOURCE_SUBSCRIPTION: &str = "subscription";
pub const SOURCE_POLL: &str = "poll";
const LOST_MARKERS: [&str; 2] = ["subscription liveness timeout", "will try to resubscribe"];
const UP_MARKERS: [&str; 2] = ["subscriberesponse is received", "subscription established"];

#[derive(Debug, Clone, Default, PartialEq)]
pub struct TypedValue {
    pub dtype: &'static str,
    pub v_i64: Option<i64>,
    pub v_f64: Option<f64>,
    pub v_bool: Option<bool>,
    pub v_str: Option<String>,
}

impl TypedValue {
    pub fn unknown() -> Self {
        Self { dtype: "unknown", ..Default::default() }
    }

    pub fn from_json(value: &Value) -> Self {
        let mut out = Self { dtype: "null", ..Default::default() };
        match value {
            Value::Null => {}
            Value::Bool(b) => (out.dtype, out.v_bool) = ("bool", Some(*b)),
            Value::Number(n) => match (n.as_i64(), n.as_u64()) {
                (Some(i), _) => (out.dtype, out.v_i64) = ("int", Some(i)),
                (None, Some(u)) => (out.dtype, out.v_str) = ("uint", Some(u.to_string())),
                _ => (out.dtype, out.v_f64) = ("float", n.as_f64()),
            },
            Value::String(s) => (out.dtype, out.v_str) = ("string", Some(s.clone())),
            Value::Array(_) => (out.dtype, out.v_str) = ("list", Some(value.to_string())),
            Value::Object(_) => (out.dtype, out.v_str) = ("struct", Some(value.to_string())),
        }
        out
    }

    pub fn number(&self) -> Option<f64> {
        self.v_i64.map(|v| v as f64).or(self.v_f64).or(self.v_bool.map(|b| f64::from(u8::from(b))))
    }

    pub fn json(&self) -> Option<Value> {
        self.v_str.as_deref().and_then(|s| serde_json::from_str(s).ok())
    }
}

#[derive(Debug, Clone, PartialEq)]
pub struct RawAttribute {
    pub node: NodeId,
    pub endpoint: u16,
    pub cluster: u32,
    pub attribute: u32,
    pub name: String,
    pub value: TypedValue,
    pub raw: Vec<u8>,
    pub source: &'static str,
}

impl RawAttribute {
    pub fn core(&self) -> Option<(&'static str, f64)> {
        let spec = core_spec(self.cluster, self.attribute)?;
        Some((spec.snake, self.value.number()? / spec.scale))
    }
}

fn core_spec(cluster: u32, attribute: u32) -> Option<&'static AttributeSpec> {
    let thermostat = WATCHED.iter().copied().filter(|_| cluster == THERMOSTAT_CLUSTER);
    let groups = EXTRA_SUBSCRIPTIONS.iter().filter(|g| g.cluster_id == cluster).flat_map(|g| g.attributes.iter().copied());
    thermostat.chain(groups).find(|s| s.id == attribute)
}

#[derive(Debug, Clone, PartialEq)]
pub struct RawEvent {
    pub node: NodeId,
    pub endpoint: u16,
    pub cluster: u32,
    pub event_id: u32,
    pub name: String,
    pub number: Option<u64>,
    pub priority: Option<u8>,
    pub device_ts_us: Option<i64>,
    pub timestamp: String,
    pub value: TypedValue,
    pub raw: Vec<u8>,
    pub source: &'static str,
}

#[derive(Debug, Clone, PartialEq)]
pub enum CaptureItem {
    Attribute(RawAttribute),
    Event(RawEvent),
    SubscriptionLost { node: Option<NodeId>, reason: String, backoff_s: Option<u64> },
    Subscribed { node: Option<NodeId> },
}

impl CaptureItem {
    pub fn node(&self) -> Option<NodeId> {
        match self {
            Self::Attribute(a) => Some(a.node),
            Self::Event(e) => Some(e.node),
            Self::SubscriptionLost { node, .. } | Self::Subscribed { node } => *node,
        }
    }
}

struct Open {
    node: Option<NodeId>,
    lines: Vec<String>,
}

#[derive(Default)]
pub struct CaptureParser {
    peer: Option<NodeId>,
    open: Option<Open>,
}

fn is_header(body: &str) -> bool {
    body.trim_start().strip_prefix("Endpoint:").is_some_and(|r| r.contains(" Attribute ") || r.contains(" Event "))
}

fn printed(lines: &[String]) -> Vec<u8> {
    let body: Vec<&str> = lines.iter().skip(1).map(|l| payload(l).unwrap_or(l)).map(|b| b.strip_prefix(' ').unwrap_or(b)).collect();
    body.join("\n").into_bytes()
}

fn priority(text: &str) -> Option<u8> {
    match text.trim().to_ascii_lowercase().as_str() {
        "debug" => Some(0),
        "info" => Some(1),
        "critical" => Some(2),
        other => other.parse().ok(),
    }
}

fn epoch_us(timestamp: &str) -> Option<i64> {
    let rest = timestamp.trim().strip_prefix("Epoch")?;
    let digits: String = rest.chars().filter(char::is_ascii_digit).collect();
    digits.parse::<i64>().ok().map(|ms| ms * 1000)
}

fn peer_of(line: &str) -> Option<NodeId> {
    let token = line.split_once("Peer = ")?.1.split_whitespace().next()?;
    NodeId::from_str_radix(token.split_once(':')?.1.trim_end_matches(','), 16).ok()
}

impl CaptureParser {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn feed(&mut self, raw: &str, sole: Option<NodeId>) -> Vec<CaptureItem> {
        let line = clean(raw);
        let mut out = Vec::new();
        match payload(&line) {
            Some(body) if is_header(body) => {
                out.extend(self.finish());
                self.open = Some(Open { node: self.peer.or(sole), lines: vec![line] });
            }
            Some(_) => {
                if let Some(open) = self.open.as_mut() {
                    open.lines.push(line);
                }
            }
            None => {
                out.extend(self.finish());
                if let Some(peer) = rx_peer(&line) {
                    self.peer = Some(peer);
                }
                out.extend(lifecycle(&line, peer_of(&line).or(self.peer).or(sole)));
            }
        }
        out
    }

    pub fn feed_all(&mut self, text: &str, sole: Option<NodeId>) -> Vec<CaptureItem> {
        let mut out: Vec<CaptureItem> = text.lines().flat_map(|l| self.feed(l, sole)).collect();
        out.extend(self.finish());
        out
    }

    pub fn finish(&mut self) -> Vec<CaptureItem> {
        let Some(open) = self.open.take() else { return Vec::new() };
        let Some(node) = open.node else { return Vec::new() };
        let text = open.lines.join("\n");
        let raw = printed(&open.lines);
        let attributes = parse_attributes(&text).0.into_iter().map(|a| {
            let value = if a.vendor || a.attribute_name.starts_with("Attribute0x") { TypedValue::unknown() } else { TypedValue::from_json(&a.value) };
            CaptureItem::Attribute(RawAttribute {
                node,
                endpoint: a.endpoint,
                cluster: a.cluster_id,
                attribute: a.attribute_id,
                name: a.attribute_name,
                value,
                raw: raw.clone(),
                source: SOURCE_SUBSCRIPTION,
            })
        });
        let events = parse_events(&text).0.into_iter().map(|e| CaptureItem::Event(event(node, e, raw.clone())));
        attributes.chain(events).collect()
    }
}

fn event(node: NodeId, e: EventRecord, raw: Vec<u8>) -> RawEvent {
    let value = if e.vendor || e.event_name.starts_with("Event0x") { TypedValue::unknown() } else { TypedValue::from_json(&e.data) };
    RawEvent {
        node,
        endpoint: e.endpoint,
        cluster: e.cluster_id,
        event_id: e.event_id,
        name: e.event_name,
        number: e.number,
        priority: priority(&e.priority),
        device_ts_us: epoch_us(&e.timestamp),
        timestamp: e.timestamp,
        value,
        raw,
        source: SOURCE_SUBSCRIPTION,
    }
}

fn lifecycle(line: &str, node: Option<NodeId>) -> Option<CaptureItem> {
    let lower = line.to_ascii_lowercase();
    if LOST_MARKERS.iter().any(|m| lower.contains(m)) {
        Some(CaptureItem::SubscriptionLost { node, reason: line.trim().to_string(), backoff_s: None })
    } else if UP_MARKERS.iter().any(|m| lower.contains(m)) {
        Some(CaptureItem::Subscribed { node })
    } else {
        None
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn parse(text: &str, sole: Option<NodeId>) -> Vec<CaptureItem> {
        CaptureParser::new().feed_all(text, sole)
    }

    fn attributes(items: &[CaptureItem]) -> Vec<&RawAttribute> {
        items.iter().filter_map(|i| if let CaptureItem::Attribute(a) = i { Some(a) } else { None }).collect()
    }

    #[test]
    fn typed_values_cover_every_dtype() {
        let cases = [
            (serde_json::json!(null), "null"),
            (serde_json::json!(true), "bool"),
            (serde_json::json!(-5), "int"),
            (serde_json::json!(u64::MAX), "uint"),
            (serde_json::json!(1.5), "float"),
            (serde_json::json!("x"), "string"),
            (serde_json::json!([1, 2]), "list"),
            (serde_json::json!({"a": 1}), "struct"),
        ];
        for (value, dtype) in cases {
            assert_eq!(TypedValue::from_json(&value).dtype, dtype, "{value}");
        }
        assert_eq!(TypedValue::from_json(&serde_json::json!(u64::MAX)).v_str.as_deref(), Some("18446744073709551615"));
        assert_eq!(TypedValue::from_json(&serde_json::json!({"a": 1})).json(), Some(serde_json::json!({"a": 1})));
    }

    #[test]
    fn parses_the_subscription_stream_with_the_raw_printed_text() {
        let items = parse(include_str!("../tests/fixtures/subscribe_extras.txt"), None);
        let all = attributes(&items);
        assert_eq!(all.len(), 9);
        assert!(all.iter().all(|a| a.node == 5 && a.source == "subscription"));
        let battery = all.iter().find(|a| a.attribute == 0x0C).unwrap();
        assert_eq!((battery.cluster, battery.endpoint, battery.value.dtype, battery.value.v_i64), (0x2F, 0, "int", Some(190)));
        assert_eq!(battery.raw, b"  BatPercentRemaining: 190");
        assert_eq!(battery.core(), Some(("bat_percent_remaining", 95.0)));
        let neighbors = all.iter().find(|a| a.attribute == 0x07).unwrap();
        assert_eq!(neighbors.value.dtype, "list");
        assert!(String::from_utf8_lossy(&neighbors.raw).contains("Rloc16: 20481"));
        assert_eq!(neighbors.core(), None);
    }

    #[test]
    fn attributes_follow_the_sending_peer() {
        let items = parse(include_str!("../tests/fixtures/subscribe_two_nodes.txt"), None);
        let nodes: Vec<NodeId> = attributes(&items).iter().map(|a| a.node).collect();
        assert!(nodes.contains(&5) && nodes.iter().any(|n| *n != 5), "{nodes:?}");
    }

    #[test]
    fn events_carry_number_priority_and_data() {
        let items = parse(include_str!("../tests/fixtures/inventory_events.txt"), Some(5));
        let events: Vec<&RawEvent> = items.iter().filter_map(|i| if let CaptureItem::Event(e) = i { Some(e) } else { None }).collect();
        assert_eq!(events.len(), 5);
        let start_up = events[0];
        assert_eq!((start_up.cluster, start_up.event_id, start_up.name.as_str(), start_up.number, start_up.priority), (0x28, 0, "StartUp", Some(1), Some(2)));
        assert_eq!(start_up.value.json(), Some(serde_json::json!({"SoftwareVersion": 66})));
        assert_eq!((start_up.timestamp.as_str(), start_up.device_ts_us), ("5995103", None));
    }

    #[test]
    fn vendor_and_unnamed_attributes_stay_uninterpreted() {
        let text = "[1.0] [1:2] [TOO] Endpoint: 1 Cluster: 0xFFF1_FC01 Attribute 0xFFF1_0002 DataVersion: 3\n[1.0] [1:2] [TOO]   VendorTrvMode: auto\n[1.0] [1:2] [TOO] Endpoint: 1 Cluster: 0x0201 Attribute 0x0000_4001 DataVersion: 3\n[1.0] [1:2] [TOO]   TLV: 77\n";
        let items = parse(text, Some(5));
        let all = attributes(&items);
        assert_eq!(all.len(), 2);
        for a in all {
            assert_eq!((a.value.dtype, a.value.v_i64, a.value.v_str.clone()), ("unknown", None, None));
            assert!(!a.raw.is_empty());
        }
    }

    #[test]
    fn detects_subscription_lifecycle_lines() {
        let text = "[1.0] [1:2] [DMG] Subscription Liveness timeout with SubscriptionID = 0x1234, Peer = 01:0000000000000005\n[1.0] [1:2] [DMG] SubscribeResponse is received\n[1.0] [1:2] [DMG] unrelated\n";
        let items = parse(text, Some(9));
        assert_eq!(items.len(), 2);
        assert!(matches!(&items[0], CaptureItem::SubscriptionLost { node: Some(5), .. }));
        assert_eq!(items[1], CaptureItem::Subscribed { node: Some(9) });
    }

    fn named(items: &[CaptureItem]) -> Vec<(u16, u32, u32, &str, &TypedValue)> {
        attributes(items).into_iter().map(|a| (a.endpoint, a.cluster, a.attribute, a.name.as_str(), &a.value)).collect()
    }

    #[test]
    fn plug_power_stream_keeps_every_report_with_its_peer_and_types() {
        let items = parse(include_str!("../tests/fixtures/device_plug_stream.txt"), None);
        let all = attributes(&items);
        assert_eq!(all.len(), 12);
        assert!(all.iter().all(|a| a.node == 9 && a.source == "subscription"));
        let power: Vec<Option<i64>> = all.iter().filter(|a| a.cluster == 0x90 && a.attribute == 0x08).map(|a| a.value.v_i64).collect();
        assert_eq!(power, vec![Some(1243100), Some(1241900), Some(18200)]);
        let on_off: Vec<(Option<bool>, &str)> = all.iter().filter(|a| a.cluster == 0x06).map(|a| (a.value.v_bool, a.value.dtype)).collect();
        assert_eq!(on_off, vec![(Some(true), "bool"), (Some(false), "bool")]);
        let energy = all.iter().find(|a| a.cluster == 0x91).unwrap();
        assert_eq!((energy.name.as_str(), energy.endpoint, energy.value.dtype), ("CumulativeEnergyImported", 2, "struct"));
        assert_eq!(energy.value.json(), Some(serde_json::json!({"Energy": 12000, "EndSystime": 17900})));
        assert!(String::from_utf8_lossy(&energy.raw).contains("EndSystime: 17900"));
        assert_eq!(all[2].raw, b"  Voltage: 230412");
    }

    #[test]
    fn plug_energy_event_is_a_struct_with_number_and_priority() {
        let items = parse(include_str!("../tests/fixtures/device_plug_event.txt"), None);
        let [CaptureItem::Event(e)] = items.as_slice() else { panic!("{items:?}") };
        assert_eq!((e.node, e.endpoint, e.cluster, e.name.as_str(), e.number, e.priority), (9, 2, 0x91, "CumulativeEnergyMeasured", Some(4), Some(1)));
        assert_eq!(e.value.json().unwrap()["EnergyImported"]["EndSystime"], 17345);
    }

    #[test]
    fn climate_sensor_reports_temperature_humidity_and_battery_on_their_endpoints() {
        let items = parse(include_str!("../tests/fixtures/device_climate_stream.txt"), None);
        let rows = named(&items);
        assert_eq!(rows.iter().map(|r| (r.0, r.1, r.2, r.4.v_i64)).collect::<Vec<_>>(), vec![(1, 0x402, 0, Some(2156)), (2, 0x405, 0, Some(8855)), (0, 0x2F, 0x0B, Some(3081)), (0, 0x2F, 0x0C, Some(200)), (0, 0x2F, 0x0E, Some(0)), (0, 0x35, 1, Some(2))]);
        assert!(attributes(&items).iter().all(|a| a.node == 11));
    }

    #[test]
    fn air_monitor_floats_arrive_as_float_dtype() {
        let items = parse(include_str!("../tests/fixtures/device_air_stream.txt"), None);
        let all = attributes(&items);
        let co2 = all.iter().find(|a| a.cluster == 0x40D && a.attribute == 0).unwrap();
        assert_eq!((co2.node, co2.value.dtype, co2.value.v_f64, co2.value.number()), (12, "float", Some(396.0), Some(396.0)));
        let pm = all.iter().find(|a| a.cluster == 0x42A && a.attribute == 0).unwrap();
        assert_eq!(pm.value.v_f64, Some(9.0));
        assert_eq!(all.iter().find(|a| a.cluster == 0x5B).unwrap().value.v_i64, Some(1));
        assert_eq!(all.iter().find(|a| a.cluster == 0x402 && a.attribute == 0).unwrap().value.v_i64, Some(1856));
        assert_eq!(all.len(), 8);
    }

    #[test]
    fn data_without_a_known_node_is_dropped() {
        assert!(parse(include_str!("../tests/fixtures/subscribe_extras.txt").lines().skip(1).collect::<Vec<_>>().join("\n").as_str(), None).is_empty());
    }
}
