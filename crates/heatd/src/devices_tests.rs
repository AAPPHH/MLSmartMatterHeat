use super::*;
use crate::capture::tests::{lake_rows, state_with, string_column, until};
use crate::lake::{DeviceReading, LakeConfig};
use crate::storage::ScanBody;
use axum::body::Body;
use axum::http::Request;
use http_body_util::BodyExt;
use matter_controller::{CaptureParser, MockController};
use tower::ServiceExt;

const PLUG: &str = include_str!("../../matter-controller/tests/fixtures/device_plug_stream.txt");
const PLUG_EVENT: &str = include_str!("../../matter-controller/tests/fixtures/device_plug_event.txt");
const CLIMATE: &str = include_str!("../../matter-controller/tests/fixtures/device_climate_stream.txt");
const AIR: &str = include_str!("../../matter-controller/tests/fixtures/device_air_stream.txt");
const DEVICES: &str = "[[devices]]\nnode = 9\nkind = 'plug'\nrole = 'coffee'\nroom = 'a'\nwake_on = true\n[[devices]]\nnode = 11\nkind = 'climate'\nrole = 'sink'\n[[devices]]\nnode = 12\nkind = 'air'\nrole = 'air'\n";
const ROWS: usize = 27;
const FAST: LoopTiming = LoopTiming { reconnect_start: Duration::from_millis(5), reconnect_max: Duration::from_millis(10), health_tick: Duration::from_secs(3600) };
const QUICK: WakeTiming = WakeTiming { retry: Duration::from_millis(20), deadline: Duration::from_millis(100) };

fn config(dir: &Path, devices: &str) -> Config {
    Config::parse(&format!("data_dir = '{}'\n[[rooms]]\nname = 'a'\nnodes = [5]\n{devices}", dir.to_str().unwrap().replace('\\', "/"))).unwrap()
}

fn never() -> Arc<dyn Fn() -> bool + Send + Sync> {
    Arc::new(|| false)
}

fn mock() -> Arc<MockController> {
    let mock = Arc::new(MockController::new(1));
    mock.add_node(5, 19.0, 19.0);
    mock.add_device(9, &format!("{PLUG}{PLUG_EVENT}"));
    mock.add_device(11, CLIMATE);
    mock.add_device(12, AIR);
    mock
}

fn devices_of(dir: &Path, text: &str, mock: &Arc<MockController>) -> Devices {
    Devices::new(&config(dir, text), mock.clone(), mock.clone()).with_wake_timing(QUICK)
}

fn setup(dir: &Path, text: &str) -> (AppState, Arc<MockController>) {
    let mock = mock();
    let state = state_with(dir, mock.clone(), "");
    (AppState { devices: devices_of(dir, text, &mock), ..state }, mock)
}

fn local(text: &str) -> NaiveDateTime {
    NaiveDateTime::parse_from_str(text, "%Y-%m-%d %H:%M").unwrap()
}

fn step(d: &Devices, at: &str, sleep: bool, away: bool) -> Vec<WakeStep> {
    d.inner.lock().unwrap().wake_step(local(at), true, sleep, away)
}

fn attempts(steps: &[WakeStep]) -> usize {
    steps.iter().filter(|s| matches!(s, WakeStep::Attempt { .. })).count()
}

fn flushed(state: &AppState) {
    state.sink.flush().unwrap();
}

fn on_offs(mock: &MockController) -> usize {
    mock.write_log().iter().filter(|w| w.0 == "on_off_on").count()
}

#[test]
fn shipped_configs_list_the_new_devices() {
    for (file, count) in [("heatd.example.toml", 4), ("deploy/docker/heatd.docker.toml", 4)] {
        let config = Config::parse(&std::fs::read_to_string(format!("{}/../../{file}", env!("CARGO_MANIFEST_DIR"))).unwrap()).unwrap();
        assert_eq!(config.devices.len(), count, "{file}");
        assert_eq!(config.all_nodes(), vec![5, 6, 7, 8]);
        let coffee: Vec<&DeviceConfig> = config.devices.iter().filter(|d| d.wake_on).collect();
        assert_eq!((coffee.len(), coffee[0].node, coffee[0].role.as_str(), coffee[0].wake_window.as_str(), coffee[0].onoff_endpoint), (1, 9, "coffee", "06:00-09:00", 1));
        assert_eq!(config.devices.iter().find(|d| d.node == 12).unwrap().room.as_deref(), Some("schlafzimmer"));
        let brush = config.devices.iter().find(|d| d.node == 10).unwrap();
        assert_eq!((brush.role.as_str(), brush.room.as_deref(), brush.subscription().fast.unwrap().min_s), ("toothbrush", Some("bad"), 10));
    }
    assert!(Config::default().devices.is_empty());
}

#[test]
fn subscription_plans_follow_the_kind_and_the_overrides() {
    let config = config(Path::new("x"), &format!("{DEVICES}[[devices]]\nnode = 13\nkind = 'plug'\nrole = 'charger'\nmin_s = 2\nmax_s = 30\nrest_min_s = 120\nrest_max_s = 600\nsilent_after_min = 5\n"));
    let plans: Vec<DeviceSubscription> = config.devices.iter().map(DeviceConfig::subscription).collect();
    assert_eq!(plans[0], DeviceSubscription { min_s: 300, max_s: 900, fast: Some(FastSubscription { clusters: vec![0x0006, 0x0090, 0x0091], min_s: 1, max_s: 60 }), events: true });
    assert_eq!(plans[1], DeviceSubscription { min_s: 60, max_s: 1800, fast: None, events: false });
    assert_eq!(plans[2].fast.as_ref().unwrap().clusters, vec![0x005B, 0x0402, 0x0405, 0x040D, 0x042A]);
    assert_eq!(plans[3], DeviceSubscription { min_s: 120, max_s: 600, fast: Some(FastSubscription { clusters: vec![0x0006, 0x0090, 0x0091], min_s: 2, max_s: 30 }), events: true });
    let silent: Vec<u64> = config.devices.iter().map(|d| d.silent_after().as_secs()).collect();
    assert_eq!(silent, vec![1800, 28800, 3600, 300]);
    assert_eq!(config.devices[0].label(), "plug 9");
}

#[test]
fn bad_device_sections_are_rejected() {
    let ok = "[[devices]]\nnode = 9\nkind = 'plug'\nrole = 'coffee'\n";
    assert!(Config::parse(&format!("[[rooms]]\nname = 'a'\nnodes = [5]\n{ok}")).is_ok());
    let cases = [
        ("duplicate node", format!("{ok}{ok}")),
        ("thermostat node", "[[devices]]\nnode = 5\nkind = 'plug'\nrole = 'coffee'\n".into()),
        ("unknown room", format!("{ok}room = 'nowhere'\n")),
        ("wake_on on a climate sensor", "[[devices]]\nnode = 9\nkind = 'climate'\nrole = 'coffee'\nwake_on = true\n".into()),
        ("wake_on on another role", "[[devices]]\nnode = 9\nkind = 'plug'\nrole = 'charger'\nwake_on = true\n".into()),
        ("bad window", format!("{ok}wake_on = true\nwake_window = '6-9'\n")),
        ("empty window", format!("{ok}wake_on = true\nwake_window = '09:00-06:00'\n")),
        ("zero interval", format!("{ok}min_s = 0\n")),
        ("min above max", format!("{ok}min_s = 10\nmax_s = 5\n")),
        ("unknown kind", "[[devices]]\nnode = 9\nkind = 'toaster'\nrole = 'x'\n".into()),
        ("empty role", "[[devices]]\nnode = 9\nkind = 'plug'\nrole = ' '\n".into()),
    ];
    for (what, devices) in cases {
        assert!(Config::parse(&format!("[[rooms]]\nname = 'a'\nnodes = [5]\n{devices}")).is_err(), "{what}");
    }
}

#[test]
fn every_report_becomes_one_raw_row_with_value_and_text() {
    let parse = |text: &str| CaptureParser::new().feed_all(text, None).iter().filter_map(|i| reading_of(i, 7)).collect::<Vec<_>>();
    let plug = parse(PLUG);
    assert_eq!(plug.len(), 12);
    assert!(plug.iter().all(|r| r.node == 9 && r.kind == "attribute" && r.ts == 7 && r.source == "subscription"));
    let power: Vec<Option<f64>> = plug.iter().filter(|r| r.cluster == 0x90 && r.attribute == 8).map(|r| r.value).collect();
    assert_eq!(power, vec![Some(1243100.0), Some(1241900.0), Some(18200.0)]);
    let on_off: Vec<(Option<f64>, &str)> = plug.iter().filter(|r| r.cluster == 6).map(|r| (r.value, r.raw.as_str())).collect();
    assert_eq!(on_off, vec![(Some(1.0), "  OnOff: TRUE"), (Some(0.0), "  OnOff: FALSE")]);
    let energy = plug.iter().find(|r| r.cluster == 0x91).unwrap();
    assert_eq!((energy.value, energy.name.as_str(), energy.endpoint), (None, "CumulativeEnergyImported", 2));
    assert!(energy.raw.contains("Energy: 12000") && energy.raw.contains("EndSystime: 17900"));
    let event = parse(PLUG_EVENT);
    assert_eq!((event.len(), event[0].kind.as_str(), event[0].name.as_str(), event[0].attribute, event[0].value), (1, "event", "CumulativeEnergyMeasured", 0, None));
    assert!(event[0].raw.contains("Event number: 4") && event[0].raw.contains("EnergyImported"));
    let climate: Vec<(u16, u32, Option<f64>)> = parse(CLIMATE).iter().filter(|r| r.node == 11).map(|r| (r.endpoint, r.cluster, r.value)).collect();
    assert_eq!(climate[..2], [(1, 0x402, Some(2156.0)), (2, 0x405, Some(8855.0))]);
    let air = parse(AIR);
    let co2 = air.iter().find(|r| r.cluster == 0x40D && r.attribute == 0).unwrap();
    assert_eq!((co2.value, co2.raw.as_str()), (Some(396.0), "  MeasuredValue: 396.000000"));
    assert_eq!(air.iter().filter(|r| r.node == 12).count(), 8);
}

#[tokio::test]
async fn devices_record_into_their_own_table_and_leave_thermostat_state_alone() {
    let dir = tempfile::tempdir().unwrap();
    let (state, mock) = setup(dir.path(), DEVICES);
    state.status.register_nodes([5]);
    spawn(state.clone(), FAST);
    until("all device rows", || {
        flushed(&state);
        lake_rows(dir.path(), "device_readings") == ROWS
    })
    .await;
    assert_eq!(lake_rows(dir.path(), "attribute_reports"), 0);
    assert_eq!(lake_rows(dir.path(), "events"), 0);
    assert!(state.latest.all().is_empty());
    let health = state.status.health(lake::now_us());
    assert_eq!(health["nodes"].as_array().unwrap().len(), 1);
    assert!([9, 11, 12].iter().all(|n| state.status.last_report_us(*n).is_none()));
    let nodes = string_column(dir.path(), "device_readings", "node");
    assert_eq!((nodes.iter().filter(|n| *n == "9").count(), nodes.iter().filter(|n| *n == "11").count(), nodes.iter().filter(|n| *n == "12").count()), (13, 6, 8));
    mock.push_capture(11, CaptureItem::SubscriptionLost { node: Some(11), reason: "liveness".into(), backoff_s: None });
    mock.push_capture(11, CaptureItem::SubscriptionLost { node: Some(11), reason: "again".into(), backoff_s: None });
    mock.push_capture(11, CaptureItem::Subscribed { node: Some(11) });
    until("link events", || {
        flushed(&state);
        string_column(dir.path(), "events", "kind").len() == 2
    })
    .await;
    let details = string_column(dir.path(), "events", "detail");
    assert!(details[0].contains("\"state\":\"subscription_lost\"") && details[1].contains("\"state\":\"resubscribed\""));
    assert!(string_column(dir.path(), "events", "kind").iter().all(|k| k == "device_link"));
    assert_eq!(mock.resubscribes(), Vec::<u64>::new());
    assert_eq!(on_offs(&mock), 0);
    assert!(state.status.health(lake::now_us())["nodes"][0]["node"] == 5);
}

#[tokio::test]
async fn api_state_lists_the_latest_values_per_device() {
    let dir = tempfile::tempdir().unwrap();
    let (state, _) = setup(dir.path(), DEVICES);
    spawn(state.clone(), FAST);
    until("rows", || state.devices.view(lake::now_us())[2]["reports"] == 8).await;
    let response = crate::http::router(state.clone()).oneshot(Request::builder().uri("/api/state").body(Body::empty()).unwrap()).await.unwrap();
    let body: Value = serde_json::from_slice(&response.into_body().collect().await.unwrap().to_bytes()).unwrap();
    let devices = body["matter_devices"].as_array().unwrap();
    assert_eq!(devices.iter().map(|d| d["node"].as_u64().unwrap()).collect::<Vec<_>>(), vec![9, 11, 12]);
    let value = |d: &Value, name: &str| d["latest"].as_array().unwrap().iter().find(|l| l["name"] == name).map(|l| l["value"].clone());
    until("plug rows", || state.devices.view(lake::now_us())[0]["reports"] == 13).await;
    let plug = state.devices.view(lake::now_us())[0].clone();
    assert_eq!((value(&plug, "ActivePower"), value(&plug, "OnOff"), plug["wake_on"].clone(), plug["silent"].clone()), (Some(json!(18200.0)), Some(json!(0.0)), json!(true), json!(false)));
    assert!(plug["latest"].as_array().unwrap().iter().all(|l| [6, 0x90, 0x91].contains(&l["cluster"].as_u64().unwrap())));
    let climate = &state.devices.view(lake::now_us())[1];
    assert!(climate["latest"].as_array().unwrap().iter().any(|l| l["name"] == "BatPercentRemaining" && l["value"] == 200.0));
    assert!(climate["latest"].as_array().unwrap().iter().all(|l| l["cluster"] != 0x35));
    assert_eq!(body["rooms"][0]["nodes"][0]["node"], 5);
}

#[test]
fn wake_fires_once_per_local_day_inside_the_window() {
    let dir = tempfile::tempdir().unwrap();
    let d = devices_of(dir.path(), DEVICES, &mock());
    assert!(step(&d, "2026-11-10 05:00", true, false).is_empty());
    assert!(step(&d, "2026-11-10 05:59", false, false).is_empty());
    assert!(step(&d, "2026-11-10 05:59", true, false).is_empty());
    let fired = step(&d, "2026-11-10 09:00", false, false);
    assert!(fired.is_empty(), "window end is exclusive");
    assert!(step(&d, "2026-11-10 06:00", true, false).is_empty());
    let fired = step(&d, "2026-11-10 06:00", false, false);
    let [WakeStep::Attempt { plug, day, local: at }] = fired.as_slice() else { panic!("{fired:?}") };
    assert_eq!((plug.node, plug.endpoint, plug.window_label(), day.to_string(), at.to_string()), (9, 1, "06:00-09:00".to_string(), "2026-11-10".to_string(), "2026-11-10 06:00:00".to_string()));
    assert!(step(&d, "2026-11-10 07:00", true, false).is_empty());
    assert!(step(&d, "2026-11-10 07:30", false, false).is_empty());
    assert!(step(&d, "2026-11-11 05:00", true, false).is_empty());
    assert_eq!(attempts(&step(&d, "2026-11-11 08:59", false, false)), 1);
    assert!(step(&d, "2026-11-12 05:00", true, false).is_empty());
    assert!(step(&d, "2026-11-12 09:01", false, false).is_empty());
    let restarted = devices_of(dir.path(), DEVICES, &mock());
    assert!(step(&restarted, "2026-11-11 06:30", true, false).is_empty());
    assert!(step(&restarted, "2026-11-11 06:45", false, false).is_empty(), "the day marker survives a restart");
    assert!(step(&restarted, "2026-11-12 06:30", true, false).is_empty());
    assert_eq!(attempts(&step(&restarted, "2026-11-12 06:45", false, false)), 1);
}

#[test]
fn unknown_presence_and_missing_sleep_history_never_fire() {
    let dir = tempfile::tempdir().unwrap();
    let d = devices_of(dir.path(), DEVICES, &mock());
    let mut inner = d.inner.lock().unwrap();
    assert!(inner.wake_step(local("2026-11-10 06:30"), false, true, false).is_empty());
    assert!(inner.wake_step(local("2026-11-10 06:40"), true, false, false).is_empty());
    assert!(inner.wake_step(local("2026-11-10 06:50"), false, true, false).is_empty());
    assert!(inner.wake_step(local("2026-11-10 06:55"), false, false, false).is_empty());
    assert!(inner.done.is_empty());
}

#[test]
fn away_suppresses_the_switch_and_is_logged_once_per_day() {
    let dir = tempfile::tempdir().unwrap();
    let d = devices_of(dir.path(), DEVICES, &mock());
    assert!(step(&d, "2026-11-10 06:10", true, true).is_empty());
    let skipped = step(&d, "2026-11-10 06:40", false, true);
    assert!(matches!(skipped.as_slice(), [WakeStep::Away { plug, .. }] if plug.node == 9));
    assert!(d.inner.lock().unwrap().done.is_empty());
    assert!(step(&d, "2026-11-10 07:00", true, true).is_empty());
    assert!(step(&d, "2026-11-10 07:10", false, true).is_empty());
    assert!(step(&d, "2026-11-10 07:20", true, false).is_empty());
    assert_eq!(attempts(&step(&d, "2026-11-10 07:30", false, false)), 1);
}

#[test]
fn dst_transition_days_fire_once_at_the_local_wake() {
    for (day, next) in [("2026-03-29", "2026-03-30"), ("2026-10-25", "2026-10-26")] {
        let dir = tempfile::tempdir().unwrap();
        let d = devices_of(dir.path(), DEVICES, &mock());
        assert!(step(&d, &format!("{day} 05:50"), true, false).is_empty());
        let fired = step(&d, &format!("{day} 06:30"), false, false);
        assert!(matches!(fired.as_slice(), [WakeStep::Attempt { day: fired_day, .. }] if fired_day.to_string() == day), "{day}");
        assert!(step(&d, &format!("{day} 07:00"), true, false).is_empty());
        assert!(step(&d, &format!("{day} 07:10"), false, false).is_empty());
        assert!(step(&d, &format!("{next} 05:50"), true, false).is_empty());
        assert_eq!(attempts(&step(&d, &format!("{next} 06:30"), false, false)), 1, "{next}");
    }
}

#[test]
fn the_day_is_the_local_date_not_the_utc_date() {
    let dir = tempfile::tempdir().unwrap();
    let text = "[[devices]]\nnode = 9\nkind = 'plug'\nrole = 'coffee'\nwake_on = true\nwake_window = '00:00-09:00'\n";
    let d = devices_of(dir.path(), text, &mock());
    assert!(step(&d, "2026-07-01 00:10", true, false).is_empty());
    let fired = step(&d, "2026-07-01 00:30", false, false);
    assert!(matches!(fired.as_slice(), [WakeStep::Attempt { day, .. }] if day.to_string() == "2026-07-01"));
    assert_eq!(d.inner.lock().unwrap().done.get(&9).map(ToString::to_string), Some("2026-07-01".to_string()));
    assert!(step(&d, "2026-07-01 06:00", true, false).is_empty());
    assert!(step(&d, "2026-07-01 06:30", false, false).is_empty());
    assert_eq!(std::fs::read_to_string(dir.path().join(DAY_FILE)).unwrap(), r#"{"9":"2026-07-01"}"#);
}

#[test]
fn only_an_armed_coffee_plug_can_be_switched() {
    let dir = tempfile::tempdir().unwrap();
    let off = "[[devices]]\nnode = 9\nkind = 'plug'\nrole = 'coffee'\n[[devices]]\nnode = 10\nkind = 'plug'\nrole = 'charger'\n[[devices]]\nnode = 11\nkind = 'climate'\nrole = 'sink'\n";
    let cfg = config(dir.path(), off);
    assert!(cfg.devices.iter().all(|d| CoffeePlug::from_config(d).is_none()));
    let d = devices_of(dir.path(), off, &mock());
    assert!(step(&d, "2026-11-10 06:10", true, false).is_empty());
    assert!(step(&d, "2026-11-10 06:40", false, false).is_empty());
    let armed = config(dir.path(), DEVICES);
    assert_eq!(armed.devices.iter().filter_map(CoffeePlug::from_config).map(|p| p.node).collect::<Vec<_>>(), vec![9]);
}

#[tokio::test]
async fn failed_writes_are_retried_then_given_up_and_logged() {
    let dir = tempfile::tempdir().unwrap();
    let sink = Sink::open(LakeConfig::new(dir.path().join("lake"))).unwrap();
    let plug = CoffeePlug::from_config(&config(dir.path(), DEVICES).devices[0]).unwrap();
    let (day, at) = ("2026-11-10".parse().unwrap(), local("2026-11-10 06:41"));
    let ok = mock();
    ok.fail_switch_on(2);
    wake_attempts(ok.clone(), sink.clone(), plug.clone(), day, at, QUICK, never()).await;
    assert_eq!(ok.write_log(), vec![("on_off_on", 9, 1.0); 3]);
    let lost = mock();
    lost.fail_switch_on(1000);
    wake_attempts(lost.clone(), sink.clone(), plug, day, at, QUICK, never()).await;
    let tries = lost.write_log().len();
    assert!((3..=6).contains(&tries) && lost.write_log().iter().all(|w| *w == ("on_off_on", 9, 1.0)), "{tries}");
    sink.flush().unwrap();
    let details: Vec<Value> = string_column(dir.path(), "events", "detail").iter().map(|d| serde_json::from_str(d).unwrap()).collect();
    assert_eq!(string_column(dir.path(), "events", "kind"), vec!["coffee_on"; 2]);
    assert_eq!((details[0]["reason"].clone(), details[0]["ok"].clone(), details[0]["attempt"].clone(), details[0]["local_time"].clone(), details[0]["window"].clone()), (json!("wake"), json!(true), json!(3), json!("06:41"), json!("06:00-09:00")));
    assert_eq!((details[1]["reason"].clone(), details[1]["ok"].clone(), details[1]["attempt"].clone()), (json!("gave_up"), json!(false), json!(tries)));
    assert!(details[1]["error"].as_str().unwrap().contains("no write status"));
    assert_eq!(string_column(dir.path(), "events", "node"), vec!["9"; 2]);
}

#[tokio::test]
async fn a_stopping_heatd_starts_no_coffee_attempt() {
    let dir = tempfile::tempdir().unwrap();
    let sink = Sink::open(LakeConfig::new(dir.path().join("lake"))).unwrap();
    let plug = CoffeePlug::from_config(&config(dir.path(), DEVICES).devices[0]).unwrap();
    let switch = mock();
    wake_attempts(switch.clone(), sink.clone(), plug, "2026-11-10".parse().unwrap(), local("2026-11-10 06:41"), QUICK, Arc::new(|| true)).await;
    assert!(switch.write_log().is_empty());
    sink.flush().unwrap();
    assert!(string_column(dir.path(), "events", "detail")[0].contains("heatd stopping"));
}

#[test]
fn a_calm_plug_defaults_to_a_slow_power_interval_and_climate_skips_events() {
    let config = config(Path::new("x"), &format!("{DEVICES}[[devices]]
node = 13
kind = 'plug'
role = 'charger'
"));
    let plans: Vec<DeviceSubscription> = config.devices.iter().map(DeviceConfig::subscription).collect();
    assert_eq!(plans.last().unwrap().fast.as_ref().unwrap().min_s, 30);
    assert_eq!(plans[0].fast.as_ref().unwrap().min_s, 1);
    assert!(!plans[1].events);
}

#[tokio::test]
async fn the_control_step_wake_signal_switches_the_coffee_plug_once() {
    let dir = tempfile::tempdir().unwrap();
    let (state, mock) = setup(dir.path(), DEVICES);
    let now = crate::storage::now_ms();
    let scan = |ts: i64, bedtime: bool| ScanBody { ts, device: "pixel-watch".into(), id: None, label: None, aps: Vec::new(), motion: None, queued: false, power: None, away: false, bedtime: Some(bedtime), heat: None, label_id: None };
    state.storage.ingest(&scan(now - 180_000, true), now - 180_000, None).unwrap();
    crate::rules::control_step(&state, local("2026-11-10 05:30"), now - 170_000).await.unwrap();
    assert_eq!(on_offs(&mock), 0);
    state.storage.ingest(&scan(now - 60_000, false), now - 60_000, None).unwrap();
    crate::rules::control_step(&state, local("2026-11-10 06:30"), now - 50_000).await.unwrap();
    until("coffee on", || on_offs(&mock) == 1).await;
    state.storage.ingest(&scan(now - 30_000, true), now - 30_000, None).unwrap();
    crate::rules::control_step(&state, local("2026-11-10 06:50"), now - 20_000).await.unwrap();
    state.storage.ingest(&scan(now - 10_000, false), now - 10_000, None).unwrap();
    crate::rules::control_step(&state, local("2026-11-10 07:00"), now - 5_000).await.unwrap();
    sleep(Duration::from_millis(150)).await;
    assert_eq!(on_offs(&mock), 1);
    assert_eq!(mock.write_log().iter().filter(|w| w.1 != 5).collect::<Vec<_>>(), vec![&("on_off_on", 9, 1.0)]);
    flushed(&state);
    assert_eq!(string_column(dir.path(), "events", "kind").iter().filter(|k| *k == "coffee_on").count(), 1);
}

#[tokio::test]
async fn silent_devices_get_one_low_priority_line_and_backed_off_resubscribes() {
    let dir = tempfile::tempdir().unwrap();
    let (state, mock) = setup(dir.path(), DEVICES);
    let t0 = lake::now_us();
    let min = 60_000_000;
    for (node, text) in [(9, PLUG), (11, CLIMATE), (12, AIR)] {
        state.devices.handle(&state.sink, node, CaptureParser::new().feed_all(text, None).into_iter().next().unwrap());
    }
    state.devices.health_tick(&state.sink, t0 + 10 * min).await;
    assert!(mock.device_resubscribes().is_empty());
    state.devices.health_tick(&state.sink, t0 + 31 * min).await;
    state.devices.health_tick(&state.sink, t0 + 32 * min).await;
    assert_eq!(mock.device_resubscribes(), vec![9]);
    state.devices.health_tick(&state.sink, t0 + 62 * min).await;
    assert_eq!(mock.device_resubscribes(), vec![9, 9, 12]);
    flushed(&state);
    assert_eq!(string_column(dir.path(), "events", "kind"), vec!["device_link"; 2]);
    let view = state.devices.view(t0 + 62 * min);
    assert_eq!((view[0]["silent"].clone(), view[0]["resubscribes"].clone(), view[1]["silent"].clone()), (json!(true), json!(2), json!(false)));
    let item = CaptureParser::new().feed_all(PLUG, None).into_iter().next().unwrap();
    state.devices.handle(&state.sink, 9, item);
    flushed(&state);
    let details = string_column(dir.path(), "events", "detail");
    assert_eq!(details.len(), 3);
    assert!(details[2].contains("\"state\":\"ok\"") && details[2].contains("silent_s"));
    assert_eq!(state.devices.view(lake::now_us())[0]["silent"], false);
    assert!(state.status.health(lake::now_us())["nodes"].as_array().unwrap().is_empty());
    assert_eq!(on_offs(&mock), 0);
}

#[tokio::test]
async fn nothing_but_one_on_command_ever_reaches_the_devices() {
    let dir = tempfile::tempdir().unwrap();
    let (state, mock) = setup(dir.path(), DEVICES);
    spawn(state.clone(), FAST);
    until("rows", || {
        flushed(&state);
        lake_rows(dir.path(), "device_readings") == ROWS
    })
    .await;
    for (at, sleeping) in [("2026-11-10 05:00", true), ("2026-11-10 06:40", false), ("2026-11-10 06:50", true), ("2026-11-10 07:10", false), ("2026-11-10 23:00", true), ("2026-11-11 03:00", false)] {
        state.devices.on_control(&state, local(at), true, sleeping, false);
    }
    until("coffee on", || on_offs(&mock) == 1).await;
    state.devices.health_tick(&state.sink, lake::now_us() + 10 * 3600 * 1_000_000).await;
    sleep(Duration::from_millis(250)).await;
    assert_eq!(mock.write_log(), vec![("on_off_on", 9, 1.0)]);
    assert!(mock.setpoint_writes().is_empty() && mock.limit_writes().is_empty());
    assert!(mock.resubscribes().is_empty() && mock.restarts() == 0);
    assert_eq!(mock.device_resubscribes(), vec![9, 11, 12]);
    let source = include_str!("devices.rs");
    let code = &source[..source.find("#[cfg(test)]\n#[path").unwrap()];
    for forbidden in ["write_heating_setpoint", "write_max_heat_limit", "write_system_mode", "MatterController", "restart_session", "commission", "ChipToolController", "inventory"] {
        assert!(!code.contains(forbidden), "devices.rs must not use {forbidden}");
    }
    assert_eq!(code.matches("device_switch_on").count(), 1);
}

#[test]
fn the_report_section_lists_values_counts_gaps_and_the_coffee_line() {
    let dir = tempfile::tempdir().unwrap();
    let date = "2026-11-10".parse::<NaiveDate>().unwrap();
    let cfg = config(dir.path(), DEVICES);
    let base = midnight_us(date);
    let sink = dated_sink(dir.path(), date);
    crate::context::write_meta(&sink, &cfg, base).unwrap();
    let hour = 3_600_000_000;
    let reading = |ts: i64, node: u64, cluster: u32, attribute: u32, name: &str, value: Option<f64>| DeviceReading { ts, node, endpoint: 1, cluster, attribute, kind: "attribute".into(), name: name.into(), value, raw: String::new(), source: "subscription".into() };
    for row in [
        reading(base + 7 * hour, 9, 0x90, 8, "ActivePower", Some(1_200_000.0)),
        reading(base + 7 * hour + 1_000_000, 9, 0x90, 8, "ActivePower", Some(18_200.0)),
        reading(base + 7 * hour + 1_000_000, 9, 6, 0, "OnOff", Some(1.0)),
        reading(base + 10 * hour, 9, 0x90, 8, "ActivePower", Some(0.0)),
        reading(base + 8 * hour, 12, 0x40D, 0, "MeasuredValue", Some(412.0)),
        reading(base + 8 * hour, 12, 0x402, 0, "MeasuredValue", Some(1856.0)),
    ] {
        sink.append_on(date, row).unwrap();
    }
    let coffee = |ts: i64, detail: Value| Event { ts, ..Event::now(kind::COFFEE_ON, "heatd").with_node(9).with_detail(detail) };
    sink.append_on(date, coffee(base + 5 * hour + 12 * 60_000_000, json!({"reason": "wake", "local_time": "06:11", "attempt": 2, "ok": true}))).unwrap();
    sink.append_on(date, Event { ts: base + 9 * hour, ..Event::now(kind::DEVICE_LINK, "heatd").with_node(11).with_detail(json!({"state": "silent"})) }).unwrap();
    sink.roll_day(date).unwrap();
    let md = report_section(dir.path(), date).unwrap();
    assert!(md.contains("## Zusatzgeraete"), "{md}");
    assert!(md.contains("| 9 | plug 9 (plug, coffee, a) | 4 | Ein/Aus an, Wirkleistung 0.0 W | 1 | 3.0 h |"), "{md}");
    assert!(md.contains("| 11 | climate 11 (climate, sink, -) | 0 | - | 0 | ganzer Tag |"), "{md}");
    assert!(md.contains("| 12 | air 12 (air, air, -) | 2 | Temperatur 18.6 C, CO2 412.0 ppm | 0 |"), "{md}");
    assert!(md.contains("Kaffeemaschine (Node 9): um 05:12:00 UTC eingeschaltet, Schlafmodus der Uhr endete 06:11 Ortszeit, Versuch 2."), "{md}");
    assert!(md.contains("Verbindung: Node 11 silent 1."), "{md}");
    let empty = tempfile::tempdir().unwrap();
    let none = report_section(empty.path(), date).unwrap();
    assert!(none.contains("Keine Zusatzgeraete konfiguriert."));
}

fn dated_sink(root: &Path, date: NaiveDate) -> Sink {
    let base = midnight_us(date);
    Sink::open_with_clock(LakeConfig::new(root), Arc::new(move || base)).unwrap()
}

#[test]
fn a_quiet_armed_day_reports_that_the_coffee_plug_stayed_off() {
    let dir = tempfile::tempdir().unwrap();
    let date = "2026-11-10".parse::<NaiveDate>().unwrap();
    let sink = dated_sink(dir.path(), date);
    crate::context::write_meta(&sink, &config(dir.path(), DEVICES), midnight_us(date)).unwrap();
    sink.append_on(date, Event { ts: midnight_us(date) + 100, ..Event::now(kind::COFFEE_ON, "heatd").with_node(9).with_detail(json!({"reason": "away", "ok": false})) }).unwrap();
    sink.roll_day(date).unwrap();
    assert!(report_section(dir.path(), date).unwrap().contains("Kaffeemaschine (Node 9): nicht eingeschaltet, Haus war abwesend."));
    let quiet = tempfile::tempdir().unwrap();
    let sink = dated_sink(quiet.path(), date);
    crate::context::write_meta(&sink, &config(quiet.path(), DEVICES), midnight_us(date)).unwrap();
    sink.roll_day(date).unwrap();
    assert!(report_section(quiet.path(), date).unwrap().contains("Kaffeemaschine (Node 9): nicht eingeschaltet, kein Ende des Schlafmodus im Fenster 06:00-09:00 Ortszeit."));
}

#[test]
#[ignore]
fn storage_estimate_for_a_dense_power_stream() {
    let dir = tempfile::tempdir().unwrap();
    let date = "2026-11-10".parse::<NaiveDate>().unwrap();
    let sink = dated_sink(dir.path(), date);
    let base = midnight_us(date);
    let names = [(8u32, "ActivePower"), (0xA, "ApparentPower"), (5, "ActiveCurrent"), (7, "ApparentCurrent")];
    let mut seed = 12345u64;
    let mut next = move || {
        seed = seed.wrapping_mul(6364136223846793005).wrapping_add(1442695040888963407);
        (seed >> 33) as i64
    };
    let mut rows = 0u64;
    for second in 0..86_400i64 {
        for (i, (attribute, name)) in names.iter().enumerate() {
            let value = 1_200_000 + next() % 40_000 - 20_000 + (i as i64) * 1000;
            let row = DeviceReading { ts: base + second * 1_000_000 + (i as i64) * 1000, node: 9, endpoint: 2, cluster: 0x90, attribute: *attribute, kind: "attribute".into(), name: (*name).into(), value: Some(value as f64), raw: format!("  {name}: {value}"), source: "subscription".into() };
            sink.append_on(date, row).unwrap();
            rows += 1;
        }
    }
    sink.close().unwrap();
    sink.roll_day(date).unwrap();
    let size = |dir: PathBuf| std::fs::read_dir(dir).unwrap().flatten().map(|e| e.metadata().unwrap().len()).sum::<u64>();
    let raw = size(dir.path().join("raw/device_readings"));
    let parquet = size(dir.path().join("parquet/device_readings/date=2026-11-10/node=9"));
    println!("rows {rows} raw {raw} B ({:.1} B/row) parquet {parquet} B ({:.2} B/row)", raw as f64 / rows as f64, parquet as f64 / rows as f64);
}

#[test]
fn a_toothbrush_plug_is_a_calm_read_only_plug_with_the_configured_fast_interval() {
    let dir = tempfile::tempdir().unwrap();
    let cfg = "[[devices]]
node = 10
kind = 'plug'
role = 'toothbrush'
min_s = 10
";
    let config = config(dir.path(), cfg);
    assert_eq!(config.devices[0].subscription().fast.unwrap().min_s, 10);
    assert!(CoffeePlug::from_config(&config.devices[0]).is_none());
}

#[tokio::test]
async fn a_device_that_never_reported_is_retried_fast_with_doubling_up_to_the_silence_threshold() {
    let dir = tempfile::tempdir().unwrap();
    let (state, mock) = setup(dir.path(), DEVICES);
    let t0 = lake::now_us();
    let s = 1_000_000;
    let at = |secs: i64| t0 + secs * s;
    state.devices.health_tick(&state.sink, at(30)).await;
    assert!(mock.device_resubscribes().is_empty());
    state.devices.health_tick(&state.sink, at(61)).await;
    assert_eq!(mock.device_resubscribes(), vec![9, 12]);
    state.devices.health_tick(&state.sink, at(120)).await;
    assert_eq!(mock.device_resubscribes(), vec![9, 12]);
    state.devices.health_tick(&state.sink, at(182)).await;
    assert_eq!(mock.device_resubscribes(), vec![9, 12, 9, 12]);
    state.devices.health_tick(&state.sink, at(301)).await;
    assert_eq!(mock.device_resubscribes(), vec![9, 12, 9, 12, 11]);
    state.devices.health_tick(&state.sink, at(430)).await;
    assert_eq!(mock.device_resubscribes(), vec![9, 12, 9, 12, 11, 9, 12]);
    let view = state.devices.view(at(430));
    assert_eq!((view[0]["resubscribes"].clone(), view[0]["silent"].clone()), (json!(3), json!(false)));
    flushed(&state);
    assert!(string_column(dir.path(), "events", "kind").is_empty());
}

#[tokio::test]
async fn fast_retries_skip_a_reporting_device_and_keep_doubling_for_a_silent_one() {
    let dir = tempfile::tempdir().unwrap();
    let (state, mock) = setup(dir.path(), DEVICES);
    let t0 = lake::now_us();
    let s = 1_000_000;
    state.devices.handle(&state.sink, 9, CaptureParser::new().feed_all(PLUG, None).into_iter().next().unwrap());
    state.devices.health_tick(&state.sink, t0 + 600 * s).await;
    assert_eq!(mock.device_resubscribes(), vec![11, 12]);
    let gaps = state.devices.inner.lock().unwrap().nodes.get(&12).map(|n| n.retry_s).unwrap();
    assert_eq!(gaps, 120.0);
    for at in [900, 1500, 2700] {
        state.devices.health_tick(&state.sink, t0 + at * s).await;
    }
    let doubled = state.devices.inner.lock().unwrap().nodes.get(&12).map(|n| n.retry_s).unwrap();
    assert_eq!(doubled, 960.0);
}
