use crate::config::Config;
use crate::context::*;
use crate::lake::{kind, LakeConfig, Sink, Table};
use crate::status::{Status, SubState};
use anyhow::{anyhow, Result};
use arrow::array::Array;
use serde_json::Value;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::Arc;

const FIXTURE: &str = include_str!("../tests/fixtures/open_meteo_forecast.json");
const T0: i64 = 1_793_145_600_000_000;

fn fixture_fetch() -> (Fetcher, Arc<AtomicUsize>) {
    let calls = Arc::new(AtomicUsize::new(0));
    let counter = calls.clone();
    (
        Arc::new(move |url: &str| {
            assert!(url.contains("latitude=52.5") && url.contains("past_days=2") && url.contains("shortwave_radiation") && url.contains("wind_speed_unit=ms"), "{url}");
            counter.fetch_add(1, Ordering::SeqCst);
            Ok(FIXTURE.to_string())
        }),
        calls,
    )
}

fn clocked_sink(dir: &std::path::Path, now: i64) -> Sink {
    Sink::open_with_clock(LakeConfig::new(dir), Arc::new(move || now)).unwrap()
}

fn config() -> Config {
    Config::parse(
        "latitude = 52.5\nlongitude = 13.4\nneighbours = [['b','a'],['a','b']]\n[heat_source]\nkind = 'gas'\nnight_shutdown = '22:00-05:00'\n[[rooms]]\nname = 'a'\nnodes = [1]\n[[rooms]]\nname = 'b'\nnodes = [2, 3]\n",
    )
    .unwrap()
}

fn raw_strings(root: &std::path::Path, table: Table, column: &str) -> Vec<String> {
    read_raw(root, table, None)
        .unwrap()
        .iter()
        .flat_map(|b| {
            let col = arrow::compute::cast(b.column_by_name(column).unwrap(), &arrow::datatypes::DataType::Utf8).unwrap();
            let col = col.as_any().downcast_ref::<arrow::array::StringArray>().unwrap().clone();
            (0..col.len()).map(|i| col.value(i).to_string()).collect::<Vec<_>>()
        })
        .collect()
}

#[test]
fn parses_recorded_open_meteo_response() {
    let hours = parse_hourly(FIXTURE).unwrap();
    assert_eq!(hours.len(), 48);
    assert_eq!(hours[0], Hourly { ts: T0, temp_c: Some(9.4), wind_ms: Some(1.91), radiation_wm2: Some(0.0), cloud_pct: Some(52.0), precip_mm: Some(0.0) });
    assert_eq!(hours[9].precip_mm, Some(0.1));
    assert_eq!(hours[33].radiation_wm2, Some(208.8));
    assert!(hours.windows(2).all(|w| w[1].ts - w[0].ts == HOUR_US));
}

#[test]
fn rejects_broken_responses() {
    assert!(parse_hourly("{\"error\":true,\"reason\":\"Latitude must be in range\"}").unwrap_err().to_string().contains("Latitude"));
    assert!(parse_hourly("{\"hourly\":{\"time\":[1,2],\"temperature_2m\":[1],\"wind_speed_10m\":[1,2],\"shortwave_radiation\":[1,2],\"cloud_cover\":[1,2],\"precipitation\":[1,2]}}").is_err());
    assert!(parse_hourly("not json").is_err());
    assert!(parse_hourly("{}").is_err());
}

#[test]
fn splits_observations_and_forecast_with_issue_time() {
    let hours = parse_hourly(FIXTURE).unwrap();
    let now = T0 + 30 * HOUR_US + 600_000_000;
    let rows = weather_rows(&hours, now, None);
    let obs: Vec<_> = rows.iter().filter(|r| r.kind == "obs").collect();
    let fc: Vec<_> = rows.iter().filter(|r| r.kind == "fc").collect();
    assert_eq!((obs.len(), fc.len()), (31, 17));
    assert!(obs.iter().all(|r| r.issued_ts.is_none() && r.ts <= T0 + 30 * HOUR_US && r.fetched_ts == now));
    assert!(fc.iter().all(|r| r.issued_ts == Some(T0 + 30 * HOUR_US) && r.ts > T0 + 30 * HOUR_US));
    assert_eq!(obs[30].temp_c, Some(4.0));
}

#[test]
fn backfills_only_hours_after_the_last_observation() {
    let hours = parse_hourly(FIXTURE).unwrap();
    let now = T0 + 30 * HOUR_US;
    let rows = weather_rows(&hours, now, Some(T0 + 20 * HOUR_US));
    let obs: Vec<i64> = rows.iter().filter(|r| r.kind == "obs").map(|r| r.ts).collect();
    assert_eq!(obs, (21..=30).map(|h| T0 + h * HOUR_US).collect::<Vec<_>>());
    assert!(weather_rows(&hours, now, Some(now)).iter().all(|r| r.kind == "fc"));
}

#[test]
fn weather_is_disabled_without_coordinates() {
    let (fetch, calls) = fixture_fetch();
    assert!(WeatherJob::new(&Config::default(), fetch.clone()).is_none());
    assert!(WeatherJob::new(&Config::parse("latitude = 52.5").unwrap(), fetch.clone()).is_none());
    assert!(WeatherJob::new(&Config::parse("latitude = 152.5\nlongitude = 1").unwrap(), fetch.clone()).is_none());
    assert!(WeatherJob::new(&config(), fetch).is_some());
    assert_eq!(calls.load(Ordering::SeqCst), 0);
}

#[test]
fn weather_job_writes_rows_dedups_and_backfills_after_outage() {
    let dir = tempfile::tempdir().unwrap();
    let status = Status::new(0);
    let (fetch, calls) = fixture_fetch();
    let t_a = T0 + 20 * HOUR_US + 300_000_000;
    let sink = clocked_sink(dir.path(), t_a);
    let mut job = WeatherJob::new(&config(), fetch.clone()).unwrap();
    assert_eq!(job.run(&sink, &status, t_a).unwrap(), 21 + 27);
    assert_eq!(job.run(&sink, &status, t_a + HOUR_US / 2).unwrap(), 27);
    sink.close().unwrap();
    assert_eq!(last_obs_in_lake(dir.path(), crate::lake::date_of_us(t_a)).unwrap(), Some(T0 + 20 * HOUR_US));
    let t_b = T0 + 30 * HOUR_US + 300_000_000;
    let sink = clocked_sink(dir.path(), t_b);
    let mut restarted = WeatherJob::new(&config(), fetch).unwrap();
    assert_eq!(restarted.run(&sink, &status, t_b).unwrap(), 10 + 17);
    sink.close().unwrap();
    assert_eq!(calls.load(Ordering::SeqCst), 3);
    let kinds = raw_strings(dir.path(), Table::Weather, "kind");
    assert_eq!(kinds.iter().filter(|k| *k == "obs").count(), 31);
    assert_eq!(status.health(t_b)["weather_age_s"], 0.0);
}

#[test]
fn failed_fetch_leaves_state_untouched() {
    let dir = tempfile::tempdir().unwrap();
    let sink = clocked_sink(dir.path(), T0);
    let fail: Fetcher = Arc::new(|_| Err(anyhow!("offline")));
    let mut job = WeatherJob::new(&config(), fail).unwrap();
    assert!(job.run(&sink, &Status::new(0), T0 + 5 * HOUR_US).is_err());
    sink.close().unwrap();
    assert!(read_raw(dir.path(), Table::Weather, None).unwrap().is_empty());
}

#[test]
fn meta_is_versioned_and_written_only_on_change() {
    let dir = tempfile::tempdir().unwrap();
    let sink = clocked_sink(dir.path(), T0);
    let mut cfg = config();
    assert_eq!(write_meta(&sink, &cfg, T0).unwrap(), 4);
    sink.flush().unwrap();
    assert_eq!(write_meta(&sink, &cfg, T0 + 1).unwrap(), 0);
    let known = meta_in_lake(dir.path()).unwrap();
    assert_eq!(known["adjacency"], "[[\"a\",\"b\"]]");
    assert_eq!(known["rooms"], "[{\"name\":\"a\",\"nodes\":[1]},{\"name\":\"b\",\"nodes\":[2,3]}]");
    assert_eq!(known["coordinates"], "{\"lat\":52.5,\"lon\":13.4}");
    let heat: Value = serde_json::from_str(&known["heat_source"]).unwrap();
    assert_eq!((heat["type"].as_str(), heat["night_shutdown"].as_str(), heat["operating_hours"].is_null()), (Some("gas"), Some("22:00-05:00"), true));
    cfg.heat_source.night_shutdown = Some("23:00-05:00".into());
    cfg.neighbours.push(crate::config::Neighbour::pair("a", "b"));
    assert_eq!(write_meta(&sink, &cfg, T0 + 2).unwrap(), 1);
    sink.close().unwrap();
    assert!(meta_in_lake(dir.path()).unwrap()["heat_source"].contains("23:00-05:00"));
    let keys = raw_strings(dir.path(), Table::Meta, "key");
    assert_eq!(keys.iter().filter(|k| *k == "heat_source").count(), 2);
    assert_eq!(keys.len(), 5);
}

#[test]
fn meta_without_coordinates_omits_the_key() {
    let mut cfg = config();
    cfg.latitude = None;
    assert!(meta_values(&cfg).iter().all(|(k, _)| *k != "coordinates"));
}

#[test]
fn config_rejects_unknown_neighbour_rooms() {
    assert!(Config::parse("neighbours = [['a','zz']]\n[[rooms]]\nname = 'a'\nnodes = [1]").is_err());
}

fn ntp_packet(t2: i64, t3: i64) -> [u8; 48] {
    let mut p = [0u8; 48];
    p[0] = 0x24;
    p[1] = 2;
    for (range, t) in [(32..40, t2), (40..48, t3)] {
        let secs = (t.div_euclid(1_000_000) + 2_208_988_800) as u32;
        let frac = ((t.rem_euclid(1_000_000) << 32) / 1_000_000) as u32;
        p[range.start..range.start + 4].copy_from_slice(&secs.to_be_bytes());
        p[range.start + 4..range.end].copy_from_slice(&frac.to_be_bytes());
    }
    p
}

#[test]
fn sntp_math_matches_the_rfc_formula() {
    let t1 = 1_800_000_000_000_000;
    let ahead = sntp_offset(t1, &ntp_packet(t1 + 2_050_000, t1 + 2_050_000), t1 + 100_000).unwrap();
    assert!((ahead - 2.0).abs() < 1e-5, "{ahead}");
    let behind = sntp_offset(t1, &ntp_packet(t1 - 1_450_000, t1 - 1_450_000), t1 + 100_000).unwrap();
    assert!((behind + 1.5).abs() < 1e-5, "{behind}");
    let request = sntp_request(t1 + 123_456);
    assert_eq!(request[0], 0x1b);
    let echoed = ntp_packet(0, 0);
    assert_eq!(echoed[0], 0x24);
    let secs = u32::from_be_bytes(request[40..44].try_into().unwrap()) as i64 - 2_208_988_800;
    assert_eq!(secs, (t1 + 123_456) / 1_000_000);
}

#[test]
fn sntp_rejects_bad_responses() {
    let t1 = 1_800_000_000_000_000;
    assert!(sntp_offset(t1, &[0u8; 20], t1).is_err());
    let mut kiss = ntp_packet(t1, t1);
    kiss[1] = 0;
    assert!(sntp_offset(t1, &kiss, t1).is_err());
    let mut unsynced = ntp_packet(t1, t1);
    unsynced[0] |= 0xc0;
    assert!(sntp_offset(t1, &unsynced, t1).is_err());
    let mut request_mode = ntp_packet(t1, t1);
    request_mode[0] = 0x23;
    assert!(sntp_offset(t1, &request_mode, t1).is_err());
    let mut empty = ntp_packet(t1, t1);
    empty[40..48].fill(0);
    assert!(sntp_offset(t1, &empty, t1).is_err());
}

fn offset_of(value: Result<f64, &'static str>) -> OffsetFn {
    Arc::new(move || value.map_err(|e| anyhow!(e)))
}

#[test]
fn ntp_drift_event_from_one_second() {
    let dir = tempfile::tempdir().unwrap();
    let sink = clocked_sink(dir.path(), T0);
    let status = Status::new(0);
    assert_eq!(check_ntp(&sink, &status, &offset_of(Ok(0.4)), "pool").unwrap(), 0.4);
    assert_eq!(check_ntp(&sink, &status, &offset_of(Ok(-0.999)), "pool").unwrap(), -0.999);
    sink.flush().unwrap();
    assert!(read_raw(dir.path(), Table::Events, None).unwrap().is_empty());
    check_ntp(&sink, &status, &offset_of(Ok(1.0)), "pool").unwrap();
    check_ntp(&sink, &status, &offset_of(Ok(-3.25)), "pool").unwrap();
    assert!(check_ntp(&sink, &status, &offset_of(Err("timeout")), "pool").is_err());
    sink.close().unwrap();
    assert_eq!(raw_strings(dir.path(), Table::Events, "kind"), vec![kind::NTP_DRIFT, kind::NTP_DRIFT]);
    let details: Vec<Value> = raw_strings(dir.path(), Table::Events, "detail").iter().map(|d| serde_json::from_str(d).unwrap()).collect();
    assert_eq!((details[1]["offset_s"].as_f64(), details[1]["server"].as_str()), (Some(-3.25), Some("pool")));
    assert_eq!(status.health(0)["ntp_offset_s"], -3.25);
}

#[test]
fn status_feeds_health_and_metrics() {
    let status = Status::new(1_000_000_000);
    status.register_nodes([5, 6]);
    let now = 1_000_000_000 + 600_000_000;
    let health = status.health(now);
    assert_eq!((health["status"].as_str(), health["nodes"].as_array().unwrap().len()), (Some("ok"), 2));
    assert_eq!(health["nodes"][0]["subscription"], "unknown");
    assert!(health["nodes"][0]["last_report_age_s"].is_null());
    status.subscription(5, SubState::Subscribed, now - 60_000_000);
    status.node_report(5, now - 30_000_000);
    status.node_report(5, now - 90_000_000);
    status.subscription(6, SubState::Lost, now - 10_000_000);
    status.rolled(now - 5_000_000, true);
    let health = status.health(now);
    assert_eq!(health["status"], "degraded");
    assert_eq!((health["nodes"][0]["subscription"].as_str(), health["nodes"][0]["last_report_age_s"].as_f64(), health["nodes"][0]["reports"].as_u64()), (Some("subscribed"), Some(30.0), Some(2)));
    assert_eq!(health["nodes"][1]["subscription_age_s"], 10.0);
    assert_eq!(health["last_roll"]["ok"], true);
    status.subscription(6, SubState::Subscribed, now);
    assert_eq!(status.health(now)["status"], "ok");
    let metrics = status.metrics(now);
    for line in [
        "heatd_node_subscription{node=\"5\",state=\"subscribed\"} 1",
        "heatd_node_subscription{node=\"6\",state=\"subscribed\"} 1",
        "heatd_node_last_report_age_seconds{node=\"5\"} 30",
        "heatd_node_reports_total{node=\"5\"} 2",
        "heatd_node_reports_total{node=\"6\"} 0",
        "heatd_uptime_seconds 600",
        "heatd_last_roll_ok 1",
        "# TYPE heatd_node_subscription gauge",
    ] {
        assert!(metrics.lines().any(|l| l == line), "missing {line}\n{metrics}");
    }
    assert!(!metrics.contains("heatd_node_last_report_age_seconds{node=\"6\"}"));
}

#[test]
fn meta_carries_links_building_and_heat_details_for_the_home() {
    let text = std::fs::read_to_string(concat!(env!("CARGO_MANIFEST_DIR"), "/../../deploy/docker/heatd.docker.toml")).unwrap();
    let values: std::collections::HashMap<_, _> = meta_values(&crate::config::Config::parse(&text).unwrap()).into_iter().collect();
    let links: Value = serde_json::from_str(&values["links"]).unwrap();
    assert_eq!(links.as_array().unwrap().len(), 10);
    let building: Value = serde_json::from_str(&values["building"]).unwrap();
    assert_eq!((building["floor"].clone(), building["roof_rooms"][0].clone(), building["exterior"]["flur"][0].clone()), (3.into(), "schlafzimmer".into(), "front_door".into()));
    let heat: Value = serde_json::from_str(&values["heat_source"]).unwrap();
    assert_eq!((heat["type"].as_str(), heat["scope"].as_str(), heat["efficiency_class"].as_str(), heat["night_setback_controllable"].as_bool()), (Some("oil"), Some("central"), Some("E"), Some(false)));
    let rooms: Value = serde_json::from_str(&values["rooms"]).unwrap();
    assert_eq!(rooms[4], serde_json::json!({"name": "flur", "nodes": []}));
}
