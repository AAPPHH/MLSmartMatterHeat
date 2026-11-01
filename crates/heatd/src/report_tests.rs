use crate::config::Config;
use crate::context::{write_meta, HOUR_US};
use crate::lake::*;
use crate::report::*;
use chrono::NaiveDate;
use duckdb::Connection;
use std::path::Path;
use std::sync::Arc;

const MIN: i64 = 60_000_000;

fn day() -> NaiveDate {
    "2026-10-20".parse().unwrap()
}

fn start() -> i64 {
    day().and_hms_opt(0, 0, 0).unwrap().and_utc().timestamp_micros()
}

fn at(minute: i64) -> i64 {
    start() + minute * MIN + 5_000_000
}

fn config() -> Config {
    Config::parse("latitude = 52.5\nlongitude = 13.4\nneighbours = [['a','b']]\n[[rooms]]\nname = 'a'\nnodes = [1]\n[[rooms]]\nname = 'b'\nnodes = [2, 3]\n").unwrap()
}

fn attr(node: u64, minute: i64, cluster: u32, attribute: u32, v: i64) -> AttributeReport {
    AttributeReport { ts: at(minute), node, endpoint: 1, cluster, attribute, dtype: "int".into(), v_i64: Some(v), source: "subscription".into(), fw: "1".into(), ..Default::default() }
}

fn event(minute: i64, kind: &str, node: Option<u64>) -> Event {
    Event { ts: at(minute), kind: kind.into(), node, source: "heatd".into(), ..Default::default() }
}

fn sample_lake(root: &Path) {
    let clock = start() + 12 * HOUR_US;
    let sink = Sink::open_with_clock(LakeConfig::new(root), Arc::new(move || clock)).unwrap();
    write_meta(&sink, &config(), start()).unwrap();
    for m in 0..1440 {
        if !(600..=660).contains(&m) {
            sink.append(attr(1, m, 513, 0, 2000 + m / 10)).unwrap();
        }
        sink.append(attr(2, m, 513, 0, 1900)).unwrap();
        sink.append(attr(3, m, 513, 0, 2100)).unwrap();
        if m % 5 == 0 && !(600..=660).contains(&m) {
            sink.append(attr(1, m, 513, 8, 40)).unwrap();
        }
    }
    sink.append(attr(1, 0, 513, 18, 2100)).unwrap();
    sink.append(attr(1, 720, 513, 18, 1800)).unwrap();
    for (m, v) in [(0, 190), (700, 170), (800, 180)] {
        sink.append(attr(2, m, 47, 12, v)).unwrap();
    }
    let neighbours = |v: &str| AttributeReport { v_str: Some(v.into()), dtype: "list".into(), v_i64: None, ..attr(3, 0, 53, 7, 0) };
    sink.append(AttributeReport { ts: at(100), ..neighbours("[{\"AverageRssi\":-60,\"Lqi\":3},{\"averageRssi\":-70,\"lqi\":2}]") }).unwrap();
    sink.append(AttributeReport { ts: at(1000), ..neighbours("[{\"AverageRssi\":-80,\"Lqi\":1}]") }).unwrap();
    for h in (0..24).filter(|h| *h != 5) {
        sink.append(Weather { ts: start() + h * HOUR_US, kind: "obs".into(), fetched_ts: start() + h * HOUR_US + 300_000_000, temp_c: Some(h as f32), radiation_wm2: Some(100.0 + h as f32), wind_ms: Some(2.0), ..Default::default() }).unwrap();
    }
    sink.append(event(300, kind::PRESENCE_LEAVE, None)).unwrap();
    sink.append(event(600, kind::PRESENCE_ARRIVE, None)).unwrap();
    sink.append(event(200, kind::EXPERIMENT_START, Some(1)).with_experiment("x1")).unwrap();
    sink.append(event(260, kind::EXPERIMENT_END, Some(1)).with_experiment("x1")).unwrap();
    sink.append(event(400, kind::EXPERIMENT_ABORT, Some(2)).with_experiment("x2")).unwrap();
    let decision = |minute: i64, applied: bool, setpoint: Option<f64>, window: Option<bool>| Decision {
        ts: at(minute),
        room: "a".into(),
        node: 1,
        rule: "schedule".into(),
        applied,
        setpoint_new: setpoint,
        commit: "t".into(),
        inputs: DecisionInputs { window_open: window, ..Default::default() },
        ..Default::default()
    };
    sink.append(decision(150, true, Some(19.5), None)).unwrap();
    sink.append(decision(300, false, None, Some(true))).unwrap();
    sink.append(decision(330, false, None, Some(false))).unwrap();
    for (minute, applied, why) in [(400, true, "occupied"), (410, false, "rate_limited"), (420, false, "debounce"), (430, false, "debounce")] {
        sink.append(Decision { proposed: true, inputs: DecisionInputs { extra: Some(format!("{{\"reason\":\"{why}\"}}")), ..Default::default() }, ..decision(minute, applied, Some(21.0), None) }).unwrap();
    }
    for (minute, minutes) in [(500, 30), (700, 20)] {
        sink.append(Event { detail: Some(format!("{{\"room\":\"a\",\"minutes\":{minutes}.0}}")), ..event(minute, kind::OCCUPANCY_END, None) }).unwrap();
    }
    for (minute, k, detail) in [(300, kind::WINDOW_OPEN, "{\"room\":\"a\",\"source\":\"auto\"}"), (320, kind::WINDOW_CLOSE, "{\"room\":\"a\",\"reason\":\"recovery\",\"open_minutes\":18.0}"), (330, kind::WINDOW_SUPPRESSED, "{\"room\":\"a\",\"reason\":\"outdoor\"}")] {
        sink.append(Event { detail: Some(detail.into()), ..event(minute, k, None) }).unwrap();
    }
    for (minute, detail) in [(340, "{\"detector\":\"v1\",\"room\":\"a\",\"duration_min\":20.0,\"probe_count\":1,\"probe_valve_min\":5.0,\"below_min\":12.0,\"label\":\"yes\",\"latency_min\":2.0}"), (341, "{\"detector\":\"v2\",\"room\":\"a\",\"duration_min\":14.0,\"probe_count\":0,\"probe_valve_min\":0.0,\"below_min\":4.0,\"label\":\"none\",\"latency_min\":null}")] {
        sink.append(Event { detail: Some(detail.into()), ..event(minute, kind::WINDOW_EPISODE, None) }).unwrap();
    }
    for (room, source, n) in [("a", "watch", 2), ("b", "watch", 1), ("a", "override", 1)] {
        for i in 0..n {
            sink.append(Label { ts: at(10 + i), room: room.into(), source: source.into(), ..Default::default() }).unwrap();
        }
    }
    for (m, rssi) in [(10, -50), (11, -60), (900, -80)] {
        sink.append(WatchRaw { ts: at(m), sender: "aa:bb".into(), rssi, batch_id: 1, device: "watch".into(), ..Default::default() }).unwrap();
    }
    sink.roll_day(day()).unwrap();
    sink.close().unwrap();
}

#[derive(Debug, Clone)]
struct Row {
    minute: i64,
    actual: Option<f64>,
    setpoint: Option<f64>,
    cmd: Option<f64>,
    valve: Option<f64>,
    outdoor: Option<f32>,
    radiation: Option<f32>,
    presence: Option<bool>,
    nb_mean: Option<f64>,
    window: Option<bool>,
    exploration: bool,
    gap: i32,
}

fn rows_of(db: &LakeDb, room: &str) -> Vec<Row> {
    let mut stmt = db
        .conn
        .prepare(&format!("SELECT epoch_us(t) // 60000000, actual, setpoint, setpoint_cmd, valve_pct, outdoor_temp, radiation, presence, neighbour_temp_mean, window_open, exploration, gap_mask FROM room_minute WHERE room = '{room}' ORDER BY t"))
        .unwrap();
    stmt.query_map([], |r| {
        Ok(Row {
            minute: r.get(0)?,
            actual: r.get(1)?,
            setpoint: r.get(2)?,
            cmd: r.get(3)?,
            valve: r.get(4)?,
            outdoor: r.get(5)?,
            radiation: r.get(6)?,
            presence: r.get(7)?,
            nb_mean: r.get(8)?,
            window: r.get(9)?,
            exploration: r.get(10)?,
            gap: r.get(11)?,
        })
    })
    .unwrap()
    .map(Result::unwrap)
    .collect()
}

fn close(a: Option<f64>, b: f64) {
    assert!(a.is_some_and(|a| (a - b).abs() < 1e-6), "{a:?} != {b}");
}

#[test]
fn room_minute_has_a_full_one_minute_grid_per_room() {
    let dir = tempfile::tempdir().unwrap();
    sample_lake(dir.path());
    let db = open_room_minute(dir.path(), day(), day()).unwrap();
    let base = start() / MIN;
    for room in ["a", "b"] {
        let rows = rows_of(&db, room);
        assert_eq!(rows.len(), 1440, "{room}");
        assert!(rows.iter().enumerate().all(|(i, r)| r.minute == base + i as i64), "{room} grid has holes or duplicates");
    }
    let total: i64 = db.conn.query_row("SELECT count(*) FROM room_minute", [], |r| r.get(0)).unwrap();
    assert_eq!(total, 2880);
}

#[test]
fn room_minute_values_flags_and_gap_mask() {
    let dir = tempfile::tempdir().unwrap();
    sample_lake(dir.path());
    let db = open_room_minute(dir.path(), day(), day()).unwrap();
    let a = rows_of(&db, "a");
    let b = rows_of(&db, "b");
    close(a[100].actual, 20.10);
    close(a[100].setpoint, 21.0);
    close(a[800].setpoint, 18.0);
    close(a[100].valve, 40.0);
    close(a[102].valve, 40.0);
    close(b[100].actual, 20.0);
    close(a[100].nb_mean, 20.0);
    close(b[100].nb_mean, 20.10);
    assert_eq!((a[130].outdoor, a[130].radiation), (Some(2.0), Some(102.0)));
    assert_eq!(a[330].outdoor, Some(4.0), "hour 5 is missing, hour 4 carried forward");
    assert_eq!((a[100].presence, a[400].presence, a[700].presence), (None, Some(false), Some(true)));
    assert_eq!((a[149].cmd, a[150].cmd), (None, Some(19.5)));
    assert_eq!((a[310].window, a[340].window, a[100].window, a[400].window), (Some(true), Some(false), None, None));
    let exploring: Vec<i64> = a.iter().filter(|r| r.exploration).map(|r| r.minute - start() / MIN).collect();
    assert_eq!(exploring, (200..260).collect::<Vec<_>>());
    assert!(b.iter().all(|r| !r.exploration), "experiment_abort of node 2 without a start never marks exploration");
    let stale: Vec<i64> = a.iter().filter(|r| r.gap & 1 != 0).map(|r| r.minute - start() / MIN).collect();
    assert_eq!(stale, (605..=660).collect::<Vec<_>>());
    assert_eq!(a[0].gap & 1, 0);
    assert_eq!((a[0].gap & 2, a[100].gap & 2), (0, 2));
    assert_eq!(b[100].gap & 1, 0);
    assert!(a.iter().all(|r| r.gap & 8 == 0), "hourly weather with one missing hour never exceeds 3 h");
}

#[test]
fn export_writes_the_view_as_parquet() {
    let dir = tempfile::tempdir().unwrap();
    sample_lake(dir.path());
    let (path, rows) = export(dir.path(), "a", day(), day(), None).unwrap();
    assert_eq!(rows, 1440);
    assert_eq!(path, dir.path().join("exports").join("a_2026-10-20_2026-10-20.parquet"));
    let check = Connection::open_in_memory().unwrap();
    let p = path.to_string_lossy().replace('\\', "/");
    let (n, rooms, first): (i64, i64, f64) = check.query_row(&format!("SELECT count(*), count(DISTINCT room), min(actual) FROM read_parquet('{p}')"), [], |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?))).unwrap();
    assert_eq!((n, rooms), (1440, 1));
    assert!(first >= 20.0);
    let custom = dir.path().join("out").join("b.parquet");
    assert_eq!(export(dir.path(), "b", day(), day(), Some(custom.clone())).unwrap().1, 1440);
    assert!(custom.exists());
}

#[test]
fn export_rejects_unknown_room_bad_range_and_empty_lake() {
    let dir = tempfile::tempdir().unwrap();
    sample_lake(dir.path());
    let err = export(dir.path(), "kueche", day(), day(), None).unwrap_err().to_string();
    assert!(err.contains("unknown room kueche") && err.contains("a, b"), "{err}");
    assert!(export(dir.path(), "a", day(), day() - chrono::Duration::days(1), None).is_err());
    let empty = tempfile::tempdir().unwrap();
    assert!(export(empty.path(), "a", day(), day(), None).unwrap_err().to_string().contains("unknown room"));
    assert!(export(dir.path(), "a'; DROP", day(), day(), None).is_err());
}

#[test]
fn export_covers_several_days_without_gaps() {
    let dir = tempfile::tempdir().unwrap();
    sample_lake(dir.path());
    let (_, rows) = export(dir.path(), "a", day(), day() + chrono::Duration::days(1), None).unwrap();
    assert_eq!(rows, 2880);
}

#[test]
fn daily_report_covers_all_sections() {
    let dir = tempfile::tempdir().unwrap();
    sample_lake(dir.path());
    let (md, s) = build(dir.path(), day()).unwrap();
    assert!(md.starts_with("# heatd daily report 2026-10-20"));
    assert!(md.contains("| 1 | a | 1656 |"), "{md}");
    assert!(md.contains("| 2 | b | 1443 |"), "{md}");
    assert!(md.contains("| 3 | b | 1442 |"), "{md}");
    assert!(md.contains("90.0 / 85.0"), "battery\n{md}");
    assert!(md.contains("-65.0 -> -80.0 (-15.0)") && md.contains("2.5 -> 1.0 (-1.5)"), "thread link quality\n{md}");
    assert!(md.contains("-55.0 -> -80.0 (-25.0)"), "watch rssi\n{md}");
    assert!(md.contains("| a | watch | 2 |") && md.contains("| a | override | 1 |") && md.contains("| b | watch | 1 |"), "labels\n{md}");
    assert!(md.contains("23 of 24 observation hours present, missing 1: 05:00"), "weather\n{md}");
    assert!(md.contains("1 started, 1 ended, 1 aborted"), "experiments\n{md}");
    assert!(md.contains("| a | 1 | 1 | 2 | 50 |"), "filter\n{md}");
    assert!(md.contains("| a | 1 | 0 | 1 | 1 | 18 | recovery |"), "window section
{md}");
    assert!(md.contains("| v1 | 1 | 20 | 1 | 5.0 | 12 | 1 | 0 | 0 | 2.0 |") && md.contains("| v2 | 1 | 14 | 0 | 0.0 | 4 | 0 | 0 | 1 | - |"), "episode comparison
{md}");
    assert!(md.contains("experiment_start node 1 x1"), "{md}");
    assert!(md.contains("| attribute_reports | 1 |") && !md.contains("NOT ROLLED"), "roll status\n{md}");
    assert_eq!((s.nodes, s.labels, s.experiments, s.missing_weather_hours.clone(), s.roll_ok), (3, 4, 1, vec![5], true));
    assert!((s.longest_gap_s - 3720.0).abs() < 1.0 && s.longest_gap_node == Some(1), "{s:?}");
    let line = log_line(day(), &s);
    assert!(line.contains("1 weather hours missing") && line.contains("1 experiments") && line.contains("roll ok"), "{line}");
}

#[test]
fn report_flags_unrolled_days_and_silent_nodes() {
    let dir = tempfile::tempdir().unwrap();
    sample_lake(dir.path());
    std::fs::remove_file(parquet_dir(dir.path(), Table::Labels, day()).join(".rolled")).unwrap();
    std::fs::remove_dir_all(parquet_dir(dir.path(), Table::AttributeReports, day()).join("node=3")).unwrap();
    let (md, s) = build(dir.path(), day()).unwrap();
    assert!(md.contains("| labels | 1 | - | - | NOT ROLLED |"), "{md}");
    assert!(md.contains("| 3 | b | 0 | 0 | 0 | - | - | whole day | - |"), "{md}");
    assert!(!s.roll_ok);
    assert!(log_line(day(), &s).contains("INCOMPLETE"));
}

#[test]
fn report_on_an_empty_lake_and_file_output() {
    let dir = tempfile::tempdir().unwrap();
    let (md, s) = build(dir.path(), day()).unwrap();
    assert!(md.contains("Total 0 reports") && md.contains("No labels.") && md.contains("Weather not configured"), "{md}");
    assert!(s.roll_ok);
    sample_lake(dir.path());
    let path = report_command(dir.path(), Some("2026-10-20")).unwrap();
    assert_eq!(path, dir.path().join("reports").join("2026-10-20.md"));
    assert!(std::fs::read_to_string(&path).unwrap().contains("## Roll status"));
    assert!(report_command(dir.path(), Some("20.10.2026")).is_err());
}

#[test]
fn report_time_is_ten_past_three() {
    let t = |h, m| NaiveDate::from_ymd_opt(2026, 10, 20).unwrap().and_hms_opt(h, m, 0).unwrap();
    assert_eq!(secs_until_report(t(3, 0)), 600);
    assert_eq!(secs_until_report(t(3, 10)), 86_400);
    assert_eq!(secs_until_report(t(23, 10)), 4 * 3600);
}

const BASE: i64 = 600;

fn raw(minute: i64, sensor: Option<&str>, value: f32) -> WatchRaw {
    WatchRaw { ts: at(minute), device: "watch".into(), batch_id: 1, sensor_type: sensor.map(Into::into), value: Some(value), ..Default::default() }
}

fn scan_row(ts: i64, sender: &str, rssi: i8, legacy: bool) -> WatchRaw {
    WatchRaw { ts, sender: sender.into(), rssi, device: "watch".into(), batch_id: 1, sensor_type: (!legacy).then(|| "rssi".into()), value: (!legacy).then_some(rssi as f32), ..Default::default() }
}

fn watch_lake(root: &Path) {
    let clock = start() + 12 * HOUR_US;
    let sink = Sink::open_with_clock(LakeConfig::new(root), Arc::new(move || clock)).unwrap();
    let m = |k: i64| BASE + k;
    sink.append(raw(m(0), Some("offbody"), 1.0)).unwrap();
    sink.append(raw(m(0), Some("charging"), 0.0)).unwrap();
    sink.append(raw(m(0), Some("battery"), 90.0)).unwrap();
    for k in 0..10 {
        sink.append(raw(m(k), Some("hr"), 60.0)).unwrap();
        sink.append(raw(m(k), Some("motion"), 0.1)).unwrap();
        sink.append(raw(m(k), Some("temp"), 33.5)).unwrap();
    }
    for (k, v) in [(20, 0.0), (25, 1.0)] {
        sink.append(raw(m(k), Some("offbody"), v)).unwrap();
    }
    for (k, v) in [(40, 1.0), (50, 0.0)] {
        sink.append(raw(m(k), Some("charging"), v)).unwrap();
    }
    sink.append(raw(m(30), Some("battery"), 80.0)).unwrap();
    for k in 100..=110 {
        sink.append(raw(m(k), Some("hr"), 120.0)).unwrap();
    }
    sink.append(raw(1400, Some("hr"), 55.0)).unwrap();
    sink.append(raw(1400, Some("motion"), 0.1)).unwrap();
    sink.append(raw(1405, Some("hr"), 55.0)).unwrap();
    sink.append(raw(1405, Some("motion"), 2.0)).unwrap();
    for (ts, aps) in [(at(m(10)), vec![("aa", -50), ("bb", -70)]), (at(m(11)), vec![("aa", -60)]), (at(m(12)), vec![("aa", -55)]), (at(m(12)) + 10_000_000, vec![("bb", -75), ("aa", -65)])] {
        for (sender, rssi) in aps {
            sink.append(scan_row(ts, sender, rssi, false)).unwrap();
        }
    }
    sink.append(scan_row(at(m(14)), "cc", -33, true)).unwrap();
    let label = |ts: i64, room: &str, source: &str, window: Option<(i64, i64)>| Label { ts, from_ts: window.map(|w| w.0), to_ts: window.map(|w| w.1), room: room.into(), source: source.into(), ..Default::default() };
    sink.append(label(at(m(15)), "a", "watch", Some((at(m(10)), at(m(11)))))).unwrap();
    sink.append(label(at(m(12)), "b", "watch", None)).unwrap();
    sink.append(label(at(m(13)), "c", "override", None)).unwrap();
    sink.append(label(at(m(13)), "d", "model", None)).unwrap();
    sink.append(WatchInference { ts: at(m(10)), matcher: "room".into(), template_id: "all".into(), abstained: true, reason: Some("low_confidence".into()), state: "rest".into(), features_hash: "0".into(), model_version: "v1".into(), ..Default::default() }).unwrap();
    sink.append(WatchInference { ts: at(m(11)), matcher: "room".into(), template_id: "all".into(), result_room: Some("a".into()), state: "rest".into(), features_hash: "1".into(), model_version: "v1".into(), confidence: 0.9, ..Default::default() }).unwrap();
    sink.roll_day(day()).unwrap();
    sink.close().unwrap();
}

type MinuteRow = (String, Option<i64>, Option<f64>, Option<f64>);

fn minute_query(db: &LakeDb, minute: i64, columns: &str) -> duckdb::Result<MinuteRow> {
    db.conn.query_row(&format!("SELECT {columns} FROM watch_minute WHERE epoch_us(t) = {}", start() + minute * MIN), [], |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?, r.get(3)?)))
}

#[test]
fn watch_minute_view_classifies_states_and_derives_features() {
    let dir = tempfile::tempdir().unwrap();
    watch_lake(dir.path());
    let state = crate::config::WatchState::default();
    let db = open_watch_views(dir.path(), day(), day(), &state, 0).unwrap();
    let count: i64 = db.conn.query_row("SELECT count(*) FROM watch_minute", [], |r| r.get(0)).unwrap();
    assert_eq!(count, 1440);
    let at = |k: i64| BASE + k;
    let cols = "state, min_since_onbody_change::BIGINT, hr_mean_10, hr_std_10";
    assert_eq!(minute_query(&db, at(5), cols).unwrap(), ("rest".into(), None, Some(60.0), Some(0.0)));
    assert_eq!(minute_query(&db, at(22), cols).unwrap().0, "offbody");
    assert_eq!(minute_query(&db, at(22), cols).unwrap().1, Some(2));
    assert_eq!(minute_query(&db, at(30), cols).unwrap().1, Some(5));
    assert_eq!(minute_query(&db, at(27), cols).unwrap(), ("rest".into(), Some(2), None, None));
    assert_eq!(minute_query(&db, at(45), "state, min_since_charge_end::BIGINT, battery_pct, 0.0").unwrap(), ("charging".into(), None, Some(80.0), Some(0.0)));
    assert_eq!(minute_query(&db, at(55), "state, min_since_charge_end::BIGINT, battery_pct, 0.0").unwrap().1, Some(5));
    assert_eq!(minute_query(&db, at(105), cols).unwrap().0, "active");
    assert_eq!(minute_query(&db, 1400, cols).unwrap().0, "sleep");
    assert_eq!(minute_query(&db, 1405, "state, 0, motion_10, hr").unwrap().0, "active");
    assert_eq!(minute_query(&db, at(5), "state, n_rssi::BIGINT, skin_temp, tod_sin").unwrap().2, Some(33.5));
    let (_, n, ..) = minute_query(&db, at(12), "state, n_rssi::BIGINT, 0.0, 0.0").unwrap();
    assert_eq!(n, Some(3));
    let (_, _, sin, cos) = minute_query(&db, 360, "state, 0, tod_sin, tod_cos").unwrap();
    assert!((sin.unwrap() - 1.0).abs() < 1e-9 && cos.unwrap().abs() < 1e-9);
    let shifted = open_watch_views(dir.path(), day(), day(), &state, 840).unwrap();
    assert_eq!(minute_query(&shifted, at(5), cols).unwrap().0, "sleep");
    let strict = crate::config::WatchState { hr_rest_max: 50.0, hr_active_min: 58.0, ..state };
    let tuned = open_watch_views(dir.path(), day(), day(), &strict, 0).unwrap();
    assert_eq!(minute_query(&tuned, at(5), cols).unwrap().0, "active");
    assert_eq!(minute_query(&tuned, 1400, cols).unwrap().0, "rest");
}

#[test]
fn watch_room_calib_view_joins_fingerprint_and_labels() {
    let dir = tempfile::tempdir().unwrap();
    watch_lake(dir.path());
    let db = open_watch_views(dir.path(), day(), day(), &crate::config::WatchState::default(), 0).unwrap();
    let calib = |k: i64| -> (Option<i64>, Option<String>, Option<String>, Option<String>) {
        db.conn
            .query_row(&format!("SELECT n_aps::BIGINT, label_room, label_source, to_json(rssi)::VARCHAR FROM watch_room_calib WHERE epoch_us(t) = {}", start() + (BASE + k) * MIN), [], |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?, r.get(3)?)))
            .unwrap()
    };
    assert_eq!(calib(10), (Some(2), Some("a".into()), Some("watch".into()), Some(r#"[{"sender":"aa","rssi":-50},{"sender":"bb","rssi":-70}]"#.into())));
    assert_eq!(calib(11), (Some(1), Some("a".into()), Some("watch".into()), Some(r#"[{"sender":"aa","rssi":-60}]"#.into())));
    assert_eq!(calib(12), (Some(2), Some("b".into()), Some("watch".into()), Some(r#"[{"sender":"aa","rssi":-65},{"sender":"bb","rssi":-75}]"#.into())));
    assert_eq!(calib(13), (None, None, None, None));
    assert_eq!(calib(14).0, Some(1));
    assert_eq!(calib(14).1, None);
    let total: i64 = db.conn.query_row("SELECT count(*) FROM watch_room_calib", [], |r| r.get(0)).unwrap();
    assert_eq!(total, 1440);
}

#[test]
fn daily_report_has_a_watch_section() {
    let dir = tempfile::tempdir().unwrap();
    watch_lake(dir.path());
    let (md, s) = build(dir.path(), day()).unwrap();
    assert!(md.contains("## Watch") && md.contains("| room | rest | 2 | 1 | 50.0 % |"), "{md}");
    assert!(md.contains("Abstain reasons: low_confidence 1."), "{md}");
    assert!(md.contains("battery 2") && md.contains("hr 23") && md.contains("rssi 7"), "{md}");
    assert_eq!((s.watch_inferences, s.watch_abstained), (2, 1));
    assert!(md.contains("Watch scans: 7 access-point rows"), "{md}");
    assert!((s.watch_battery_per_h.unwrap() - 20.0).abs() < 1e-6);
}

fn usage_lake(root: &Path) {
    let clock = start() + 12 * HOUR_US;
    let sink = Sink::open_with_clock(LakeConfig::new(root), Arc::new(move || clock)).unwrap();
    write_meta(&sink, &config(), start()).unwrap();
    for m in 0..1440 {
        sink.append(attr(1, m, 513, 0, 2000)).unwrap();
        let valve = match m {
            120..=179 => 60,
            180..=239 => 255,
            300..=359 => 100,
            _ => 0,
        };
        sink.append(attr(1, m, 513, 8, valve)).unwrap();
        sink.append(attr(2, m, 513, 0, 1900)).unwrap();
    }
    for h in 0..24 {
        sink.append(Weather { ts: start() + h * HOUR_US, kind: "obs".into(), fetched_ts: start() + h * HOUR_US + 300_000_000, temp_c: Some(5.0), ..Default::default() }).unwrap();
    }
    sink.roll_day(day()).unwrap();
    sink.close().unwrap();
}

#[test]
fn usage_counts_valve_hours_ignores_invalid_and_weights_proxy() {
    let dir = tempfile::tempdir().unwrap();
    usage_lake(dir.path());
    let usage = usage(dir.path(), day(), day()).unwrap();
    let a = &usage[&day()]["a"];
    close(Some(a.valve_hours), 1.6);
    close(Some(a.minutes_over_10), 120.0);
    close(Some(a.minutes_over_50), 120.0);
    close(Some(a.hourly[2]), 0.6);
    close(Some(a.hourly[3]), 0.0);
    close(Some(a.hourly[5]), 1.0);
    close(Some(a.heat_proxy), 24.0);
    assert_eq!(a.first_us, Some(start() + 120 * MIN));
    assert_eq!(a.last_us, Some(start() + 359 * MIN));
    let b = &usage[&day()]["b"];
    assert_eq!((b.valve_hours, b.first_us), (0.0, None));
    let md = usage_section(dir.path(), day()).unwrap();
    assert!(md.contains("## Heizbetrieb") && md.contains("| a | 1.60 | 120 | 120 | 02:00 | 05:59 | 100 % |") && md.contains("Haus gesamt"), "{md}");
    let json = usage_json(dir.path(), day(), day()).unwrap();
    close(json["totals"]["house"]["valve_hours"].as_f64(), 1.6);
    close(json["days"][0]["rooms"]["a"]["share_of_house"].as_f64(), 1.0);
    assert_eq!(json["days"][0]["rooms"]["a"]["hourly_valve_hours"].as_array().unwrap().len(), 24);
    assert_eq!(json["days"][0]["rooms"]["a"]["first"], "2026-10-20T02:00:00Z");
    assert!(build(dir.path(), day()).unwrap().0.contains("## Heizbetrieb"));
}
