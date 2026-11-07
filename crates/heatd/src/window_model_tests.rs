use crate::config::Config;
use crate::window::{LOCAL_TEMPERATURE, OCCUPIED_SETPOINT, PI_HEATING_DEMAND};
use crate::window_model::*;
use serde_json::{json, Map, Value};
use std::collections::BTreeMap;
use std::sync::Arc;

const MIN: i64 = 60_000;
const T0: i64 = 1_800_000_000_000 - 1_800_000_000_000 % MIN;

pub fn toy_json(rooms: &[(&str, u64, f64)], threshold: f64, guard: usize) -> Value {
    let rooms: Map<String, Value> = rooms.iter().map(|(n, node, w)| (n.to_string(), json!({"node": node, "weight": w, "coef": vec![0.0; 16], "intercept": 0.0, "sigma": 1.0}))).collect();
    json!({"version": "toy", "kind": "window_physics_residual", "horizon_min": 20, "taus": [4.0, 12.0, 40.0], "since_open_cap": 90.0, "open_valve_pct": 10.0, "valve_guard_min": guard, "clip": 6.0, "veto_pad_min": 60, "threshold": threshold, "merge_min": 30, "ffill_min": {"temp": 10, "valve": 30, "setpoint": 360}, "valid": {"temp": [0.0, 45.0], "valve": [0.0, 100.0]}, "weather_max_age_min": 180.0, "rooms": rooms})
}

pub fn toy_bundle(rooms: &[(&str, u64, f64)], threshold: f64) -> Bundle {
    Bundle::parse(&toy_json(rooms, threshold, 0).to_string()).unwrap()
}

fn cell(text: &str) -> f64 {
    if text.is_empty() { f64::NAN } else { text.parse().unwrap() }
}

fn same(a: f64, b: f64) -> bool {
    (a.is_nan() && b.is_nan()) || (a - b).abs() <= 1e-6
}

#[test]
fn rust_scorer_reproduces_the_python_stream_on_the_real_slice() {
    let bundle = Arc::new(Bundle::parse(include_str!("../tests/fixtures/window_bundle.json")).unwrap());
    let csv = include_str!("../tests/fixtures/window_parity.csv");
    let mut lines = csv.lines();
    let header: Vec<&str> = lines.next().unwrap().split(',').collect();
    let col = |name: &str| header.iter().position(|h| *h == name).unwrap_or_else(|| panic!("column {name}"));
    let rooms: Vec<String> = header.iter().filter_map(|h| h.strip_prefix("z_")).filter(|r| *r != "house").map(String::from).collect();
    assert_eq!(rooms.len(), 4);
    let mut scorer = Scorer::new(bundle.clone());
    let (mut scored, mut alarms, mut vetoed, mut rows) = (0, 0, 0, 0);
    for line in lines {
        let f: Vec<&str> = line.split(',').collect();
        let shared = Shared { supply: f[col("supply")] == "1", out: cell(f[col("out")]), wind: cell(f[col("wind")]), rad: cell(f[col("rad")]) };
        let inputs: BTreeMap<String, [f64; 3]> = rooms.iter().map(|r| (r.clone(), [cell(f[col(&format!("{r}_temp"))]), cell(f[col(&format!("{r}_valve"))]), cell(f[col(&format!("{r}_setpoint"))])])).collect();
        let ts: i64 = f[col("ts")].parse().unwrap();
        let out = scorer.step(ts * 1000, &shared, &inputs);
        for r in &rooms {
            let want = cell(f[col(&format!("z_{r}"))]);
            assert!(same(out.z[r], want), "{ts} {r}: {} vs {want}", out.z[r]);
        }
        assert!(same(out.house, cell(f[col("house")])), "{ts} house {} vs {}", out.house, f[col("house")]);
        assert_eq!(out.veto, f[col("veto")] == "1", "{ts} veto");
        scored += out.house.is_finite() as usize;
        alarms += out.alarm(bundle.threshold) as usize;
        vetoed += out.veto as usize;
        rows += 1;
    }
    assert!(rows >= 400 && scored > 100 && alarms > 0 && vetoed > 0, "{rows} {scored} {alarms} {vetoed}");
}

#[test]
fn bundle_validation_rejects_broken_files() {
    let ok = toy_json(&[("a", 5, 1.0)], 3.0, 0);
    assert!(Bundle::parse(&ok.to_string()).is_ok());
    let edit = |f: &dyn Fn(&mut Value)| {
        let mut v = ok.clone();
        f(&mut v);
        Bundle::parse(&v.to_string()).is_err()
    };
    assert!(edit(&|v| v["kind"] = json!("other")));
    assert!(edit(&|v| v["rooms"]["a"]["coef"] = json!(vec![0.0; 15])));
    assert!(edit(&|v| v["rooms"]["a"]["sigma"] = json!(0.0)));
    assert!(edit(&|v| v["taus"] = json!([4.0, 12.0])));
    assert!(edit(&|v| v["rooms"] = json!({})));
    assert!(Bundle::parse("{").is_err());
}

#[test]
fn newest_bundle_is_the_latest_version_directory_with_a_bundle() {
    let dir = tempfile::tempdir().unwrap();
    for v in ["20261101T030000Z", "20261103T030000Z", "20261102T030000Z"] {
        std::fs::create_dir_all(dir.path().join(v)).unwrap();
        std::fs::write(dir.path().join(v).join("bundle.json"), toy_json(&[("a", 5, 1.0)], 3.0, 0).to_string()).unwrap();
    }
    std::fs::create_dir_all(dir.path().join("20261104T030000Z")).unwrap();
    let (name, path) = newest_bundle(dir.path()).unwrap();
    assert_eq!(name, "20261103T030000Z");
    assert_eq!(Bundle::load(&path).unwrap().version, "toy");
    assert!(newest_bundle(&dir.path().join("missing")).is_none() && try_load(dir.path().join("missing").to_str().unwrap()).is_none());
}

struct Feed {
    d: Detector,
    c: Config,
    t: i64,
}

impl Feed {
    fn new(guard: usize) -> Self {
        let mut d = Detector::default();
        d.set_bundle(Arc::new(Bundle::parse(&toy_json(&[("a", 5, 1.0)], 3.0, guard).to_string()).unwrap()));
        Self { d, c: Config::parse("[[rooms]]\nname = 'a'\nnodes = [5]\n").unwrap(), t: T0 }
    }

    fn report(&mut self, attribute: &str, value: f64) {
        self.d.push(5, attribute, self.t, value);
    }

    fn minute(&mut self, temp: f64, valve: Option<f64>) -> Vec<Scored> {
        self.t += MIN;
        self.report(LOCAL_TEMPERATURE, temp);
        if let Some(v) = valve {
            self.report(PI_HEATING_DEMAND, v);
        }
        self.report(OCCUPIED_SETPOINT, 21.0);
        self.d.advance(&self.c, self.t, [5.0, 2.0, 0.0], false)
    }
}

#[test]
fn live_feed_grids_reports_into_the_scorer() {
    let mut f = Feed::new(0);
    let mut last = Vec::new();
    for i in 0..30 {
        last = f.minute(21.0 - 0.1 * i as f64, Some(40.0));
        assert_eq!(last.len(), 1);
    }
    let z = last[0].z["a"];
    assert!((z - 2.0).abs() < 1e-9, "20 min drop of 2 K is z 2: {z}");
    assert_eq!(last[0].ts_ms, f.t);
}

#[test]
fn late_ticks_catch_up_minute_by_minute_without_backfilling() {
    let mut f = Feed::new(0);
    for _ in 0..3 {
        f.minute(20.0, Some(40.0));
    }
    let start = f.t;
    for i in 1..=5 {
        f.d.push(5, LOCAL_TEMPERATURE, start + i * MIN, 20.0 - i as f64);
    }
    f.d.push(5, LOCAL_TEMPERATURE, start + 5 * MIN + 30_000, 0.0);
    let out = f.d.advance(&f.c, start + 5 * MIN + 45_000, [5.0, 2.0, 0.0], false);
    assert_eq!(out.iter().map(|m| m.ts_ms).collect::<Vec<_>>(), (1..=5).map(|i| start + i * MIN).collect::<Vec<_>>());
    assert!(f.d.advance(&f.c, start + 5 * MIN + 50_000, [5.0, 2.0, 0.0], false).is_empty());
}

#[test]
fn a_missing_valve_is_unscorable_never_closed() {
    let mut f = Feed::new(5);
    let mut z = Vec::new();
    for i in 0..60 {
        z.push(f.minute(20.0 - 0.05 * i as f64, Some(40.0)).pop().unwrap().z["a"]);
    }
    assert!(z[..25].iter().all(|v| v.is_nan()), "warm-up needs horizon + guard");
    assert!(z[26..].iter().all(|v| v.is_finite()), "{z:?}");
    let mut after = Vec::new();
    for _ in 0..40 {
        f.t += MIN;
        f.d.push(5, LOCAL_TEMPERATURE, f.t, 19.0);
        after.push(f.d.advance(&f.c, f.t, [5.0, 2.0, 0.0], false).pop().unwrap().z["a"]);
    }
    assert!(after[..29].iter().all(|v| v.is_finite()), "valve carries for 30 min");
    assert!(after[30..].iter().all(|v| v.is_nan()), "valve gap beyond 30 min never counts as closed: {after:?}");
}

#[test]
fn a_silent_sensor_is_stale_after_ten_minutes() {
    let mut f = Feed::new(0);
    for _ in 0..40 {
        f.minute(20.0, Some(40.0));
    }
    let mut quiet = Vec::new();
    for _ in 0..15 {
        f.t += MIN;
        f.report(PI_HEATING_DEMAND, 40.0);
        f.report(OCCUPIED_SETPOINT, 21.0);
        quiet.push(f.d.advance(&f.c, f.t, [5.0, 2.0, 0.0], false).pop().unwrap().z["a"]);
    }
    assert!(quiet[..10].iter().all(|v| v.is_finite()) && quiet[10..].iter().all(|v| v.is_nan()), "{quiet:?}");
}

#[test]
fn weather_outside_the_bundle_age_leaves_the_minute_unscorable() {
    let mut f = Feed::new(0);
    let mut z = f64::NAN;
    for _ in 0..40 {
        f.t += MIN;
        f.report(LOCAL_TEMPERATURE, 20.0);
        f.report(PI_HEATING_DEMAND, 30.0);
        f.report(OCCUPIED_SETPOINT, 21.0);
        z = f.d.advance(&f.c, f.t, [f64::NAN, 2.0, 0.0], false).pop().unwrap().z["a"];
    }
    assert!(z.is_nan());
}

#[test]
fn supply_minutes_veto_and_the_pad_runs_from_the_last_supply_minute() {
    let mut f = Feed::new(0);
    let mut vetoes = Vec::new();
    for i in 0..140 {
        f.t += MIN;
        f.report(LOCAL_TEMPERATURE, 20.0);
        let supply = (10..20).contains(&i);
        vetoes.push(f.d.advance(&f.c, f.t, [5.0, 2.0, 0.0], supply).pop().unwrap().veto);
    }
    assert!(!vetoes[9] && vetoes[10] && vetoes[19] && vetoes[20] && vetoes[79] && !vetoes[80], "{vetoes:?}");
}

#[test]
fn a_bundle_swap_keeps_state_unless_the_shape_changes() {
    let mut f = Feed::new(0);
    for _ in 0..30 {
        f.minute(20.0, Some(40.0));
    }
    let mut next = toy_json(&[("a", 5, 1.0)], 5.0, 0);
    next["version"] = json!("toy2");
    f.d.set_bundle(Arc::new(Bundle::parse(&next.to_string()).unwrap()));
    assert_eq!(f.d.version(), Some("toy2"));
    assert!(f.minute(20.0, Some(40.0)).pop().unwrap().z["a"].is_finite(), "state kept across a version swap");
    next["horizon_min"] = json!(10);
    f.d.set_bundle(Arc::new(Bundle::parse(&next.to_string()).unwrap()));
    assert!(f.minute(20.0, Some(40.0)).pop().unwrap().z["a"].is_nan(), "a new shape restarts the stream");
}
