use anyhow::{Context, Result};
use matter_controller::{
    command_name, event_name, hex_id, thermostat_features, AttributeRecord, Inventory, ACCEPTED_COMMAND_LIST, EVENT_LIST, FEATURE_MAP, GENERATED_COMMAND_LIST,
    MatterController, NEIGHBOR_TABLE, POWER_SOURCE_CLUSTER, ROUTE_TABLE, THERMOSTAT_CLUSTER, THREAD_DIAGNOSTICS_CLUSTER,
};
use serde_json::Value;
use std::collections::BTreeMap;
use std::fmt::Write as _;
use std::path::{Path, PathBuf};

pub const BASIC_INFORMATION_CLUSTER: u32 = 0x0028;
pub const SOFTWARE_VERSION: u32 = 0x0009;
pub const SOFTWARE_VERSION_STRING: u32 = 0x000A;

const CELL_LIMIT: usize = 200;
const ROLES: [&str; 7] = ["Unspecified", "Unassigned", "SleepyEndDevice", "EndDevice", "REED", "Router", "Leader"];
const CHARGE_LEVELS: [&str; 3] = ["OK", "Warning", "Critical"];
const POWER_STATUS: [&str; 4] = ["Unspecified", "Active", "Standby", "Unavailable"];
const REPLACEABILITY: [&str; 4] = ["Unspecified", "NotReplaceable", "UserReplaceable", "FactoryReplaceable"];
const NEIGHBOR_COLUMNS: [&str; 10] = ["Rloc16", "IsChild", "Lqi", "AverageRssi", "LastRssi", "FrameErrorRate", "MessageErrorRate", "RxOnWhenIdle", "FullThreadDevice", "Age"];
const ROUTE_COLUMNS: [&str; 9] = ["Rloc16", "RouterId", "NextHop", "PathCost", "LQIIn", "LQIOut", "Age", "Allocated", "LinkEstablished"];

pub fn firmware(inventory: &Inventory) -> String {
    let text = |v: &Value| match v {
        Value::String(s) => Some(s.clone()),
        Value::Number(n) => Some(n.to_string()),
        _ => None,
    };
    [SOFTWARE_VERSION_STRING, SOFTWARE_VERSION].iter().find_map(|&a| inventory.value(BASIC_INFORMATION_CLUSTER, a).and_then(text)).unwrap_or_else(|| "unknown".into())
}

pub fn sanitize(fw: &str) -> String {
    fw.chars().map(|c| if c.is_ascii_alphanumeric() || matches!(c, '.' | '-' | '+') { c } else { '_' }).collect()
}

pub fn stem(node: u64, fw: &str) -> String {
    format!("{node}_{}", sanitize(fw))
}

pub fn exists(dir: &Path, node: u64, fw: &str) -> bool {
    dir.join(format!("{}.json", stem(node, fw))).exists()
}

pub fn latest_firmware(dir: &Path, node: u64) -> Option<String> {
    let prefix = format!("{node}_");
    let newest = std::fs::read_dir(dir).ok()?.filter_map(|e| e.ok()).filter_map(|e| {
        let name = e.file_name().into_string().ok()?;
        let fw = name.strip_prefix(&prefix)?.strip_suffix(".json")?.to_string();
        Some((e.metadata().ok()?.modified().ok()?, fw))
    });
    newest.max().map(|(_, fw)| fw)
}

fn write_raw(dir: &Path, stem: &str, inventory: &Inventory) -> Result<()> {
    let raw = dir.join("raw");
    std::fs::create_dir_all(&raw).with_context(|| format!("create {}", raw.display()))?;
    std::fs::write(raw.join(format!("{stem}.attributes.log")), &inventory.raw_attributes)?;
    std::fs::write(raw.join(format!("{stem}.events.log")), &inventory.raw_events)?;
    Ok(())
}

pub fn write_report(dir: &Path, inventory: &Inventory, generated: &str) -> Result<PathBuf> {
    let fw = firmware(inventory);
    let stem = stem(inventory.node, &fw);
    write_raw(dir, &stem, inventory)?;
    let mut json = serde_json::to_value(inventory)?;
    json["fw"] = Value::String(fw);
    json["generated"] = Value::String(generated.to_string());
    std::fs::write(dir.join(format!("{stem}.md")), render_markdown(inventory, generated))?;
    let path = dir.join(format!("{stem}.json"));
    std::fs::write(&path, serde_json::to_string_pretty(&json)?)?;
    Ok(path)
}

pub async fn capture(controller: &dyn MatterController, dir: &Path, node: u64) -> Result<(PathBuf, Inventory)> {
    let inventory = controller.inventory(node).await?;
    if inventory.attributes.is_empty() {
        let stem = format!("{}_failed_{}", inventory.node, chrono::Utc::now().format("%Y%m%dT%H%M%SZ"));
        write_raw(dir, &stem, &inventory)?;
        let reason = inventory.errors.first().map_or("no error line", String::as_str);
        anyhow::bail!("no attributes parsed from chip-tool output (raw logs saved as raw/{stem}.*): {reason}");
    }
    let path = write_report(dir, &inventory, &chrono::Utc::now().to_rfc3339_opts(chrono::SecondsFormat::Secs, true))?;
    Ok((path, inventory))
}

fn plain(value: &Value) -> String {
    match value {
        Value::String(s) => s.clone(),
        other => other.to_string(),
    }
}

fn cell(value: &Value) -> String {
    let text = plain(value).replace(['\n', '\r'], " ").replace('|', "\\|");
    if text.chars().count() > CELL_LIMIT {
        format!("{}...", text.chars().take(CELL_LIMIT).collect::<String>())
    } else {
        text
    }
}

fn enum_name(names: &[&str], value: Option<&Value>) -> String {
    match value.and_then(Value::as_u64) {
        Some(i) => names.get(i as usize).map_or_else(|| i.to_string(), |n| format!("{n} ({i})")),
        None => value.map_or_else(|| "-".into(), plain),
    }
}

fn ids(value: Option<&Value>, name: impl Fn(u32) -> Option<&'static str>) -> String {
    let items: Vec<String> = value
        .and_then(Value::as_array)
        .map(|a| a.iter().filter_map(Value::as_u64).map(|id| name(id as u32).map_or_else(|| hex_id(id as u32), |n| format!("{n} ({})", hex_id(id as u32)))).collect())
        .unwrap_or_default();
    if items.is_empty() {
        "none".into()
    } else {
        items.join(", ")
    }
}

fn table(out: &mut String, columns: &[&str], rows: &[Vec<String>]) {
    let _ = writeln!(out, "| {} |", columns.join(" | "));
    let _ = writeln!(out, "|{}", "---|".repeat(columns.len()));
    for row in rows {
        let _ = writeln!(out, "| {} |", row.join(" | "));
    }
    out.push('\n');
}

fn field(entry: &Value, column: &str) -> String {
    match (column, entry.get(column)) {
        ("Rloc16", Some(Value::Number(n))) => n.as_u64().map_or_else(|| n.to_string(), |r| format!("0x{r:04X}")),
        (_, Some(v)) => cell(v),
        _ => "-".into(),
    }
}

fn object_rows(value: Option<&Value>, columns: &[&str]) -> Vec<Vec<String>> {
    value
        .and_then(Value::as_array)
        .map(|entries| entries.iter().map(|e| columns.iter().map(|c| field(e, c)).collect()).collect())
        .unwrap_or_default()
}

fn cluster_extras(out: &mut String, inventory: &Inventory, endpoint: u16, cluster: u32) {
    let find = |attribute: u32| inventory.attributes.iter().find(|a| a.endpoint == endpoint && a.cluster_id == cluster && a.attribute_id == attribute).map(|a| &a.value);
    if let Some(map) = find(FEATURE_MAP).and_then(Value::as_u64) {
        if cluster == THERMOSTAT_CLUSTER {
            let _ = writeln!(out, "- FeatureMap 0x{map:X}: {}", thermostat_features(map).join(", "));
        } else {
            let _ = writeln!(out, "- FeatureMap 0x{map:X}");
        }
    }
    for (attribute, label) in [(ACCEPTED_COMMAND_LIST, "AcceptedCommandList"), (GENERATED_COMMAND_LIST, "GeneratedCommandList")] {
        if find(attribute).is_some() {
            let _ = writeln!(out, "- {label}: {}", ids(find(attribute), |id| command_name(cluster, id)));
        }
    }
    if find(EVENT_LIST).is_some() {
        let _ = writeln!(out, "- EventList: {}", ids(find(EVENT_LIST), |id| event_name(cluster, id)));
    }
    out.push('\n');
}

fn endpoints_section(out: &mut String, inventory: &Inventory) {
    let mut tree: BTreeMap<u16, BTreeMap<u32, Vec<&AttributeRecord>>> = BTreeMap::new();
    for a in &inventory.attributes {
        tree.entry(a.endpoint).or_default().entry(a.cluster_id).or_default().push(a);
    }
    for (endpoint, clusters) in tree {
        let _ = writeln!(out, "## Endpoint {endpoint}\n");
        for (cluster, mut attributes) in clusters {
            attributes.sort_by_key(|a| a.attribute_id);
            let vendor = if attributes.iter().any(|a| a.vendor) { " - vendor" } else { "" };
            let _ = writeln!(out, "### {} ({}){vendor}\n", attributes[0].cluster_name, hex_id(cluster));
            let rows: Vec<Vec<String>> = attributes
                .iter()
                .map(|a| {
                    let writable = a.writable.map_or("?", |w| if w { "yes" } else { "no" });
                    vec![a.attribute_name.clone(), hex_id(a.attribute_id), cell(&a.value), writable.into()]
                })
                .collect();
            table(out, &["Attribute", "Id", "Value", "Writable"], &rows);
            cluster_extras(out, inventory, endpoint, cluster);
        }
    }
}

fn battery_section(out: &mut String, inventory: &Inventory) {
    let value = |name: &str| inventory.attributes.iter().find(|a| a.cluster_id == POWER_SOURCE_CLUSTER && a.attribute_name == name).map(|a| &a.value);
    if inventory.attributes.iter().all(|a| a.cluster_id != POWER_SOURCE_CLUSTER) {
        return;
    }
    let show = |name: &str| value(name).map_or_else(|| "-".into(), cell);
    let percent = value("BatPercentRemaining").and_then(Value::as_f64).map_or_else(|| "-".into(), |v| format!("{} % (raw {v})", v / 2.0));
    let voltage = value("BatVoltage").map_or_else(|| "-".into(), |v| format!("{} mV", plain(v)));
    let rows = vec![
        vec!["Status".into(), enum_name(&POWER_STATUS, value("Status"))],
        vec!["Description".into(), show("Description")],
        vec!["BatVoltage".into(), voltage],
        vec!["BatPercentRemaining".into(), percent],
        vec!["BatChargeLevel".into(), enum_name(&CHARGE_LEVELS, value("BatChargeLevel"))],
        vec!["BatReplacementNeeded".into(), show("BatReplacementNeeded")],
        vec!["BatReplaceability".into(), enum_name(&REPLACEABILITY, value("BatReplaceability"))],
        vec!["BatPresent".into(), show("BatPresent")],
        vec!["BatCapacity".into(), show("BatCapacity")],
        vec!["BatQuantity".into(), show("BatQuantity")],
    ];
    out.push_str("## Batterie\n\n");
    table(out, &["Feld", "Wert"], &rows);
}

fn thread_section(out: &mut String, inventory: &Inventory) {
    let thread: Vec<&AttributeRecord> = inventory.attributes.iter().filter(|a| a.cluster_id == THREAD_DIAGNOSTICS_CLUSTER).collect();
    if thread.is_empty() {
        return;
    }
    let value = |name: &str| thread.iter().find(|a| a.attribute_name == name).map(|a| &a.value);
    let show = |name: &str| value(name).map_or_else(|| "-".into(), cell);
    let neighbors = inventory.value(THREAD_DIAGNOSTICS_CLUSTER, NEIGHBOR_TABLE);
    let parent = neighbors.and_then(Value::as_array).and_then(|n| n.iter().find(|e| e["IsChild"] == Value::Bool(false)));
    let rloc = |v: Option<&Value>| v.and_then(Value::as_u64).map_or_else(|| "-".into(), |r| format!("0x{r:04X}"));
    let rows = vec![
        vec!["RoutingRole".into(), enum_name(&ROLES, value("RoutingRole"))],
        vec!["Channel".into(), show("Channel")],
        vec!["NetworkName".into(), show("NetworkName")],
        vec!["PanId".into(), show("PanId")],
        vec!["ExtendedPanId".into(), show("ExtendedPanId")],
        vec!["PartitionId".into(), show("PartitionId")],
        vec!["LeaderRouterId".into(), show("LeaderRouterId")],
        vec!["Own RLOC16".into(), format!("{} (not a Matter attribute; only present if the device reports Rloc16)", rloc(value("Rloc16")))],
        vec!["Parent RLOC16".into(), rloc(parent.and_then(|p| p.get("Rloc16")))],
        vec!["Parent AverageRssi / LastRssi / Lqi".into(), parent.map_or_else(|| "-".into(), |p| format!("{} / {} / {}", cell(&p["AverageRssi"]), cell(&p["LastRssi"]), cell(&p["Lqi"])))],
    ];
    out.push_str("## Thread\n\n");
    table(out, &["Feld", "Wert"], &rows);
    out.push_str("### Nachbarn (NeighborTable)\n\n");
    table(out, &NEIGHBOR_COLUMNS, &object_rows(neighbors, &NEIGHBOR_COLUMNS));
    out.push_str("### Routen (RouteTable)\n\n");
    table(out, &ROUTE_COLUMNS, &object_rows(inventory.value(THREAD_DIAGNOSTICS_CLUSTER, ROUTE_TABLE), &ROUTE_COLUMNS));
    let counters: Vec<Vec<String>> = thread.iter().filter(|a| a.attribute_name.ends_with("Count") && a.value.is_number()).map(|a| vec![a.attribute_name.clone(), cell(&a.value)]).collect();
    out.push_str("### Zaehler\n\n");
    table(out, &["Zaehler", "Wert"], &counters);
}

fn events_section(out: &mut String, inventory: &Inventory) {
    out.push_str("## Ereignisse\n\n");
    if inventory.events.is_empty() {
        out.push_str("Keine Ereignisse gelesen.\n\n");
        return;
    }
    let rows: Vec<Vec<String>> = inventory
        .events
        .iter()
        .map(|e| {
            let vendor = if e.vendor { " (vendor)" } else { "" };
            vec![
                e.endpoint.to_string(),
                format!("{} ({}){vendor}", e.cluster_name, hex_id(e.cluster_id)),
                format!("{} ({})", e.event_name, hex_id(e.event_id)),
                e.number.map_or_else(|| "-".into(), |n| n.to_string()),
                e.priority.clone(),
                e.timestamp.replace('|', "\\|"),
                cell(&e.data),
            ]
        })
        .collect();
    table(out, &["Endpoint", "Cluster", "Event", "Number", "Priority", "Timestamp", "Data"], &rows);
}

pub fn render_markdown(inventory: &Inventory, generated: &str) -> String {
    let mut out = String::new();
    let endpoints: std::collections::BTreeSet<u16> = inventory.attributes.iter().map(|a| a.endpoint).collect();
    let clusters: std::collections::BTreeSet<(u16, u32)> = inventory.attributes.iter().map(|a| (a.endpoint, a.cluster_id)).collect();
    let vendor = inventory.attributes.iter().filter(|a| a.vendor).count();
    let _ = writeln!(out, "# Inventar Node {}\n", inventory.node);
    let _ = writeln!(
        out,
        "Erzeugt {generated}. {} Attribute, {} Events, {} Endpoints, {} Cluster-Instanzen, davon {vendor} Vendor-Attribute.\n",
        inventory.attributes.len(),
        inventory.events.len(),
        endpoints.len(),
        clusters.len()
    );
    battery_section(&mut out, inventory);
    thread_section(&mut out, inventory);
    events_section(&mut out, inventory);
    endpoints_section(&mut out, inventory);
    if !inventory.errors.is_empty() {
        out.push_str("## Fehler und Warnungen\n\n");
        inventory.errors.iter().for_each(|e| {
            let _ = writeln!(out, "- {e}");
        });
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use matter_controller::MockController;

    fn fixture() -> Inventory {
        Inventory::from_raw(
            5,
            include_str!("../../matter-controller/tests/fixtures/inventory_attributes.txt").into(),
            include_str!("../../matter-controller/tests/fixtures/inventory_events.txt").into(),
        )
    }

    #[test]
    fn renders_clusters_with_writability_and_features() {
        let md = render_markdown(&fixture(), "2026-10-29T10:00:00Z");
        assert!(md.starts_with("# Inventar Node 5\n"));
        assert!(md.contains("44 Attribute, 5 Events, 2 Endpoints, 6 Cluster-Instanzen, davon 2 Vendor-Attribute."), "{md}");
        assert!(md.contains("### Thermostat (0x0201)\n"));
        assert!(md.contains("| OccupiedHeatingSetpoint | 0x0012 | 2100 | yes |"));
        assert!(md.contains("| LocalTemperature | 0x0000 | 2150 | no |"));
        assert!(md.contains("| OutdoorTemperature | 0x0001 | null | no |"));
        assert!(md.contains("- FeatureMap 0x23: Heating, Cooling, AutoMode"));
        assert!(md.contains("- AcceptedCommandList: SetpointRaiseLower (0x0000), SetWeeklySchedule (0x0001), GetWeeklySchedule (0x0002)"));
        assert!(md.contains("- GeneratedCommandList: none"));
        assert!(md.contains("### Cluster0xFFF1_FC01 (0xFFF1_FC01) - vendor"));
        assert!(md.contains("| VendorTrvMode | 0xFFF1_0002 | auto | ? |"));
    }

    #[test]
    fn renders_battery_thread_and_events() {
        let md = render_markdown(&fixture(), "now");
        assert!(md.contains("| BatVoltage | 2950 mV |"));
        assert!(md.contains("| BatPercentRemaining | 95 % (raw 190) |"));
        assert!(md.contains("| BatChargeLevel | OK (0) |"));
        assert!(md.contains("| RoutingRole | SleepyEndDevice (2) |"));
        assert!(md.contains("| Channel | 15 |"));
        assert!(md.contains("| Parent RLOC16 | 0x5000 |"));
        assert!(md.contains("| Parent AverageRssi / LastRssi / Lqi | -61 / -63 / 3 |"));
        assert!(md.contains("| 0x5001 | true | 2 | null | -80 | 3 | 1 | false | false | 40 |"));
        assert!(md.contains("| TxTotalCount | 1234 |"));
        assert!(md.contains("| 0 | Basic Information (0x0028) | StartUp (0x0000) | 1 | Critical | 5995103 | {\"SoftwareVersion\":66} |"));
        assert!(md.contains("(vendor)"));
        assert!(md.contains("## Fehler und Warnungen\n\n- Response Failure: IM Error 0x00000581"));
    }

    #[test]
    fn cells_are_escaped_and_truncated() {
        assert_eq!(cell(&Value::String("a|b\nc".into())), "a\\|b c");
        assert_eq!(cell(&Value::String("x".repeat(300))).chars().count(), CELL_LIMIT + 3);
    }

    #[tokio::test]
    async fn writes_node_and_firmware_named_files() {
        let mock = MockController::new(1);
        mock.add_node(5, 21.0, 20.0);
        let tmp = tempfile::tempdir().unwrap();
        let dir = tmp.path().join("inventory");
        let (path, inventory) = capture(&mock, &dir, 5).await.unwrap();
        assert_eq!(firmware(&inventory), "mock-1");
        assert_eq!(path, dir.join("5_mock-1.json"));
        let json: Value = serde_json::from_str(&std::fs::read_to_string(&path).unwrap()).unwrap();
        assert_eq!((json["node"].clone(), json["fw"].clone()), (5.into(), "mock-1".into()));
        assert!(json["attributes"].as_array().unwrap().len() > 5 && json["events"].as_array().unwrap().len() == 1);
        assert!(json["attributes"].as_array().unwrap().iter().any(|a| a["vendor"] == true && a["writable"].is_null()));
        assert!(std::fs::read_to_string(dir.join("raw/5_mock-1.attributes.log")).unwrap().contains("LocalTemperature: 2100"));
        assert!(std::fs::read_to_string(dir.join("raw/5_mock-1.events.log")).unwrap().contains("Event 0x0000_0000"));
        assert!(std::fs::read_to_string(dir.join("5_mock-1.md")).unwrap().contains("# Inventar Node 5"));
        assert!(exists(&dir, 5, "mock-1") && !exists(&dir, 5, "mock-2") && !exists(&dir, 6, "mock-1"));
        assert_eq!(latest_firmware(&dir, 5).as_deref(), Some("mock-1"));
        assert_eq!(latest_firmware(&dir, 6), None);
        assert_eq!(stem(7, "1.2/3 beta"), "7_1.2_3_beta");
    }
}
