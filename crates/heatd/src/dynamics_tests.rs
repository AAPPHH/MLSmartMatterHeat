use crate::config::Config;
use crate::context::{write_meta, HOUR_US};
use crate::dynamics::*;
use crate::lake::*;
use crate::report::LakeDb;
use chrono::NaiveDate;
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
    Config::parse("latitude = 52.5\nlongitude = 13.4\n[[rooms]]\nname = 'a'\nnodes = [1]\n[[rooms]]\nname = 'b'\nnodes = [2]\n[[rooms]]\nname = 'c'\nnodes = [3]\n").unwrap()
}

fn attr(node: u64, minute: i64, cluster: u32, attribute: u32, v: i64) -> AttributeReport {
    AttributeReport { ts: at(minute), node, endpoint: 1, cluster, attribute, dtype: "int".into(), v_i64: Some(v), source: "subscription".into(), fw: "1".into(), ..Default::default() }
}

#[derive(Clone, Copy)]
struct Plant {
    step_at: i64,
    from: f64,
    to: f64,
    dead: f64,
    tau: f64,
    zeta: f64,
}

const OMEGA: f64 = 0.05;

impl Plant {
    fn unit(&self, t: f64) -> f64 {
        let t = t - self.dead;
        if t <= 0.0 {
            0.0
        } else if self.zeta >= 1.0 {
            1.0 - (-t / self.tau).exp()
        } else {
            let wd = OMEGA * (1.0 - self.zeta * self.zeta).sqrt();
            1.0 - (-self.zeta * OMEGA * t).exp() * ((wd * t).cos() + self.zeta * OMEGA / wd * (wd * t).sin())
        }
    }
    fn overshoot(&self) -> f64 {
        (-std::f64::consts::PI * self.zeta / (1.0 - self.zeta * self.zeta).sqrt()).exp() * (self.to - self.from).abs()
    }
}

fn temperature(plants: &[Plant], m: i64) -> f64 {
    plants.iter().rev().find(|p| m >= p.step_at).map_or(plants[0].from, |p| p.from + (p.to - p.from) * p.unit((m - p.step_at) as f64))
}

fn setpoint(plants: &[Plant], m: i64) -> f64 {
    plants.iter().rev().find(|p| m >= p.step_at).map_or(plants[0].from, |p| p.to)
}

fn centi(v: f64) -> i64 {
    (v * 100.0).round() as i64
}

fn write_lake(root: &Path, rooms: &[(&str, u64, Vec<Plant>)], extra: impl Fn(&Sink)) {
    let clock = start() + 12 * HOUR_US;
    let sink = Sink::open_with_clock(LakeConfig::new(root), Arc::new(move || clock)).unwrap();
    write_meta(&sink, &config(), start()).unwrap();
    for (room, node, plants) in rooms {
        for m in 0..1440 {
            let (actual, sp) = (temperature(plants, m), setpoint(plants, m));
            sink.append(attr(*node, m, 513, 0, centi(actual))).unwrap();
            sink.append(attr(*node, m, 513, 18, centi(sp))).unwrap();
            sink.append(attr(*node, m, 513, 8, if sp - actual > 0.3 { 60 } else { 0 })).unwrap();
        }
        for p in plants {
            sink.append(Decision { ts: at(p.step_at), room: (*room).into(), node: *node, rule: "schedule".into(), applied: true, setpoint_new: Some(p.to), commit: "t".into(), ..Default::default() }).unwrap();
        }
    }
    for h in 0..24 {
        sink.append(Weather { ts: start() + h * HOUR_US, kind: "obs".into(), fetched_ts: start() + h * HOUR_US + 300_000_000, temp_c: Some(5.0), radiation_wm2: Some(80.0), wind_ms: Some(2.0), ..Default::default() }).unwrap();
    }
    extra(&sink);
    sink.roll_day(day()).unwrap();
    sink.close().unwrap();
}

fn plant(step_at: i64, from: f64, to: f64, dead: f64, tau: f64, zeta: f64) -> Plant {
    Plant { step_at, from, to, dead, tau, zeta }
}

struct Got {
    dead: Option<f64>,
    rate: Option<f64>,
    per_k: Option<f64>,
    reach: Option<f64>,
    overshoot: Option<f64>,
    valve_mean: Option<f64>,
    valve_max: Option<f64>,
    others: i32,
    outdoor: Option<f64>,
    source: String,
    direction: String,
    size: f64,
    start_temp: Option<f64>,
}

fn got(db: &LakeDb, room: &str, minute: i64) -> Got {
    db.conn
        .query_row(
            &format!("SELECT dead_time_min, rate_c_per_h, rate_per_k, time_to_target_min, overshoot_c, valve_mean_pct, valve_max_pct, others_heating, outdoor_temp, source, direction, step_c, start_temp FROM step_responses WHERE room = '{room}' AND epoch_us(t_start) // 60000000 = {}", start() / MIN + minute),
            [],
            |r| Ok(Got { dead: r.get(0)?, rate: r.get(1)?, per_k: r.get(2)?, reach: r.get(3)?, overshoot: r.get(4)?, valve_mean: r.get(5)?, valve_max: r.get(6)?, others: r.get(7)?, outdoor: r.get(8)?, source: r.get(9)?, direction: r.get(10)?, size: r.get(11)?, start_temp: r.get(12)? }),
        )
        .unwrap()
}

fn near(v: Option<f64>, expected: f64, tol: f64, what: &str) {
    assert!(v.is_some_and(|v| (v - expected).abs() <= tol), "{what}: {v:?} not within {tol} of {expected}");
}

fn count(db: &LakeDb, sql: &str) -> i64 {
    db.conn.query_row(sql, [], |r| r.get(0)).unwrap()
}

fn two_rooms() -> Vec<(&'static str, u64, Vec<Plant>)> {
    vec![
        ("a", 1, vec![plant(100, 18.0, 20.0, 8.0, 30.0, 1.0), plant(700, 20.0, 19.0, 8.0, 30.0, 1.0)]),
        ("b", 2, vec![plant(100, 18.0, 20.0, 15.0, 60.0, 1.0), plant(700, 20.0, 22.0, 12.0, 0.0, 0.4)]),
    ]
}

fn secant(p: &Plant, dead_min: f64) -> f64 {
    let t0 = dead_min - p.dead;
    (p.unit(p.dead + t0 + 29.0) - p.unit(p.dead + t0)) / 29.0 * 60.0 * (p.to - p.from).abs()
}

#[test]
fn step_response_recovers_dead_time_rate_settling_and_overshoot() {
    let dir = tempfile::tempdir().unwrap();
    let rooms = two_rooms();
    write_lake(dir.path(), &rooms, |_| {});
    let db = open_dynamics(dir.path(), day(), day()).unwrap();
    assert_eq!(count(&db, "SELECT count(*) FROM step_responses"), 4);
    for (room, idx) in [("a", 0), ("b", 0)] {
        let p = rooms.iter().find(|r| r.0 == room).unwrap().2[idx];
        let g = got(&db, room, 100);
        let td = -p.tau * (1.0 - 0.1 / 2.0f64).ln();
        near(g.dead, p.dead + td, 1.6, &format!("{room} dead time"));
        near(g.reach, p.dead + p.tau * (1.0f64 / 0.15).ln(), 1.6, &format!("{room} time to target"));
        near(g.rate, secant(&p, g.dead.unwrap()), secant(&p, g.dead.unwrap()) * 0.1, &format!("{room} rate"));
        near(g.per_k, g.rate.unwrap() / 2.0, 1e-9, "rate per K");
        near(g.overshoot, 0.0, 0.03, &format!("{room} overshoot"));
        near(g.valve_max, 60.0, 1e-9, "valve max");
        near(g.valve_mean, 60.0, 1.0, "valve mean");
        near(g.outdoor, 5.0, 1e-9, "outdoor");
        near(g.start_temp, 18.0, 1e-9, "start temp");
        assert_eq!((g.others, g.source.as_str(), g.direction.as_str()), (1, "schedule", "heat_up"));
        near(Some(g.size), 2.0, 1e-9, "size");
    }
    let a_ratio = got(&db, "a", 100).rate.unwrap() / got(&db, "b", 100).rate.unwrap();
    let expected = (rooms[0].2[0].unit(8.0 + 1.5 + 29.0) - rooms[0].2[0].unit(8.0 + 1.5)) / (rooms[1].2[0].unit(15.0 + 3.1 + 29.0) - rooms[1].2[0].unit(15.0 + 3.1));
    assert!((a_ratio / expected - 1.0).abs() < 0.12, "{a_ratio} vs {expected}");
    let cool = got(&db, "a", 700);
    assert_eq!(cool.direction, "cool_down");
    near(cool.reach, 8.0 + 30.0 * (1.0f64 / 0.3).ln(), 1.6, "cool-down time to target");
    near(cool.dead, 8.0 + 30.0 * -(1.0f64 - 0.1).ln(), 1.6, "cool-down dead time");
    assert!(cool.rate.is_some_and(|r| r > 0.0), "rate is positive towards the target");
    near(cool.valve_mean, 0.0, 1e-9, "cool-down valve");
    let ring = got(&db, "b", 700);
    near(ring.overshoot, rooms[1].2[1].overshoot(), 0.07, "second order overshoot");
    let p = rooms[1].2[1];
    near(ring.dead, (0..400).map(|t| t as f64 / 4.0).find(|t| p.unit(*t) * 2.0 >= 0.1).unwrap(), 1.6, "second order dead time");
}

#[test]
fn radiator_summary_compares_rooms_with_overlapping_steps() {
    let dir = tempfile::tempdir().unwrap();
    write_lake(dir.path(), &two_rooms(), |_| {});
    let db = open_dynamics(dir.path(), day(), day()).unwrap();
    let (a, b) = (got(&db, "a", 100), got(&db, "b", 100));
    let row = |room: &str| -> (i64, i64, Option<f64>, Option<f64>, Option<f64>, i64, String) {
        db.conn
            .query_row(&format!("SELECT steps, heat_up_steps, median_dead_time_min, median_rate_per_k, rel_rate_vs_peers, pair_count, note FROM radiator_summary WHERE room = '{room}'"), [], |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?, r.get(3)?, r.get(4)?, r.get(5)?, r.get(6)?)))
            .unwrap()
    };
    let (ra, rb) = (row("a"), row("b"));
    assert_eq!((ra.0, ra.1, ra.5), (2, 1, 1));
    assert_eq!((rb.0, rb.1, rb.5), (2, 2, 1));
    assert!(ra.6.contains("optimistisch"));
    near(ra.3, a.per_k.unwrap(), 1e-9, "median rate per K");
    near(ra.4, a.per_k.unwrap() / b.per_k.unwrap(), 1e-9, "relative rate a/b");
    near(rb.4, b.per_k.unwrap() / a.per_k.unwrap(), 1e-9, "relative rate b/a");
    assert!(ra.4.unwrap() > 1.5, "the faster room is ranked faster");
    near(ra.2, (a.dead.unwrap() + got(&db, "a", 700).dead.unwrap()) / 2.0, 1e-9, "median dead time");
}

#[test]
fn room_delta_daily_counts_only_gap_free_minutes_and_separates_exploration() {
    let dir = tempfile::tempdir().unwrap();
    let clock = start() + 12 * HOUR_US;
    let sink = Sink::open_with_clock(LakeConfig::new(dir.path()), Arc::new(move || clock)).unwrap();
    write_meta(&sink, &config(), start()).unwrap();
    for m in 0..1440 {
        if m < 1080 {
            sink.append(attr(3, m, 513, 0, if m < 720 { 2020 } else { 2100 })).unwrap();
        }
        sink.append(attr(3, m, 513, 18, 2000)).unwrap();
        sink.append(attr(3, m, 513, 8, if m < 360 { 50 } else { 0 })).unwrap();
    }
    for (m, k) in [(100, kind::EXPERIMENT_START), (160, kind::EXPERIMENT_END)] {
        sink.append(Event { ts: at(m), kind: k.into(), node: Some(3), source: "heatd".into(), ..Default::default() }.with_experiment("x1")).unwrap();
    }
    sink.roll_day(day()).unwrap();
    sink.close().unwrap();
    let db = open_dynamics(dir.path(), day(), day()).unwrap();
    let (minutes, mean, abs, p90, band, heating, expl, gaps, expl_abs): (i64, f64, f64, f64, f64, i64, i64, i64, f64) = db
        .conn
        .query_row("SELECT minutes, mean_delta, mean_abs_delta, p90_abs_delta, band_share, heating_minutes, exploration_minutes, gap_minutes, exploration_mean_abs_delta FROM room_delta_daily WHERE room = 'c'", [], |r| {
            Ok((r.get(0)?, r.get(1)?, r.get(2)?, r.get(3)?, r.get(4)?, r.get(5)?, r.get(6)?, r.get(7)?, r.get(8)?))
        })
        .unwrap();
    assert_eq!((minutes, heating, expl, gaps), (1025, 300, 60, 355));
    assert!((mean - 497.0 / 1025.0).abs() < 1e-6 && (abs - mean).abs() < 1e-9, "{mean} {abs}");
    assert!((band - 660.0 / 1025.0).abs() < 1e-6 && (p90 - 1.0).abs() < 1e-6 && (expl_abs - 0.2).abs() < 1e-6);
    assert_eq!(count(&db, "SELECT count(*) FROM room_delta WHERE room = 'c' AND NOT delta_ok"), 355);
}

#[test]
fn manual_step_without_decision_and_small_changes() {
    let mut sp: Vec<Option<f64>> = vec![Some(20.0); 50];
    sp.extend([Some(20.5), Some(20.5), Some(21.0)]);
    sp.extend(vec![Some(21.0); 40]);
    sp.extend([None, Some(21.3)]);
    sp.extend(vec![Some(21.3); 30]);
    sp.extend(vec![Some(19.0); 20]);
    assert_eq!(detect_steps(&sp), vec![(50, 20.0, 21.0), (125, 21.3, 19.0)]);
    assert!(detect_steps(&[]).is_empty());
    assert!(detect_steps(&[None, None]).is_empty());
}

#[test]
fn empty_lake_gives_empty_tables_and_a_report_section() {
    let dir = tempfile::tempdir().unwrap();
    let db = open_dynamics(dir.path(), day(), day()).unwrap();
    for view in VIEWS {
        assert_eq!(count(&db, &format!("SELECT count(*) FROM {view}")), 0, "{view}");
    }
    let section = report_section(dir.path(), day()).unwrap();
    assert!(section.starts_with("## Soll/Ist und Nachregeln") && section.contains("Keine Sprungantworten im Zeitraum"));
    let out = dir.path().join("x").join("steps.parquet");
    let (path, rows) = export_view(dir.path(), "step_responses", None, day(), day(), Some(out.clone())).unwrap();
    assert_eq!((path, rows), (out, 0));
    assert!(export_view(dir.path(), "nope", None, day(), day(), None).is_err());
}

#[test]
fn export_and_report_section_with_data() {
    let dir = tempfile::tempdir().unwrap();
    write_lake(dir.path(), &two_rooms(), |_| {});
    let out = dir.path().join("exports").join("s.parquet");
    let (_, rows) = export_view(dir.path(), "step_responses", Some("a"), day(), day(), Some(out.clone())).unwrap();
    assert_eq!(rows, 2);
    let conn = duckdb::Connection::open_in_memory().unwrap();
    let n: i64 = conn.query_row(&format!("SELECT count(*) FROM read_parquet('{}')", out.to_string_lossy().replace('\\', "/")), [], |r| r.get(0)).unwrap();
    assert_eq!(n, 2);
    for view in ["room_delta_daily", "radiator_summary", "radiator_pairs", "room_delta"] {
        let (p, _) = export_view(dir.path(), view, None, day(), day(), Some(dir.path().join(format!("{view}.parquet")))).unwrap();
        assert!(p.exists(), "{view}");
    }
    let section = report_section(dir.path(), day()).unwrap();
    assert!(section.contains("### Sprungantworten des Tages") && section.contains("| a |") && section.contains("schedule") && section.contains("optimistisch"), "{section}");
}

fn boost_event(variant: &str, mode: &str, extra: serde_json::Value) -> Event {
    let mut detail = serde_json::json!({"room": "a", "variant": variant, "mode": mode, "cycle_id": "boost-a-1", "start_ts_ms": at(100) / 1000, "target": 20.0, "setpoint_old": 18.0, "start_temp": 18.0, "model_version": "v1", "exploration": mode == "live"});
    detail.as_object_mut().unwrap().extend(extra.as_object().unwrap().clone());
    let mut event = Event::now("boost_cycle", "heatd").with_detail(detail);
    event.ts = at(190);
    event.node = Some(1);
    event
}

#[test]
fn boost_cycles_extend_step_responses_and_the_report() {
    let dir = tempfile::tempdir().unwrap();
    write_lake(dir.path(), &two_rooms(), |sink| {
        sink.append(boost_event("grey_box", "shadow", serde_json::json!({"planned_t_off_min": 31.0, "predicted_peak_c": 20.0, "overshoot_c": 0.0, "minutes_to_target_0_3": 40.0, "valve_over_50_min": 35.0}))).unwrap();
        sink.append(boost_event("residual", "shadow", serde_json::json!({"planned_t_off_min": 28.0, "predicted_peak_c": 19.97, "overshoot_c": 0.0, "minutes_to_target_0_3": 38.0, "valve_over_50_min": 30.0}))).unwrap();
        sink.append(boost_event("residual", "shadow", serde_json::json!({"status": "unavailable"}))).unwrap();
    });
    let db = open_dynamics(dir.path(), day(), day()).unwrap();
    let row = |variant: &str, col: &str| -> Option<f64> { db.conn.query_row(&format!("SELECT {col} FROM step_responses WHERE room = 'a' AND variant = '{variant}'"), [], |r| r.get(0)).unwrap() };
    assert_eq!(count(&db, "SELECT count(*) FROM step_responses WHERE variant = 'baseline' AND mode = 'measured'"), 4);
    assert_eq!(count(&db, "SELECT count(*) FROM step_responses WHERE mode = 'shadow'"), 2);
    assert_eq!(row("grey_box", "planned_t_off_min"), Some(31.0));
    assert_eq!(row("residual", "predicted_peak_c"), Some(19.97));
    assert_eq!(row("grey_box", "valve_over_50_min"), Some(35.0));
    assert!(db.conn.query_row::<Option<f64>, _, _>("SELECT minutes_to_target_0_3 FROM step_responses WHERE variant = 'baseline' AND room = 'a' AND direction = 'heat_up'", [], |r| r.get(0)).unwrap().is_some());
    assert_eq!(count(&db, "SELECT count(*) FROM radiator_summary"), 2);
    let section = report_section(dir.path(), day()).unwrap();
    assert!(section.contains("### Aufheizen: baseline gegen grey_box und residual") && section.contains("| a | baseline / measured |") && section.contains("| a | grey_box / shadow | 1 |") && section.contains("| a | residual / shadow | 1 |") && section.contains("Schatten, noch 6 Tage"), "{section}");
}
