use crate::config::{WatchMatchSection, WatchState};
use crate::lake::{self, LakeConfig, Sink, WatchInference};
use crate::status::Status;
use crate::watchmatch::*;
use serde_json::{json, Value};
use std::path::{Path, PathBuf};
use std::sync::Arc;

const FIXTURE: &str = concat!(env!("CARGO_MANIFEST_DIR"), "/tests/fixtures/watchroom/v1");
const T: i64 = 1_793_000_000_000;
const MIN: i64 = 60_000;
const NOON: u32 = 720;
const NIGHT: u32 = 1400;

struct Env {
    dir: tempfile::TempDir,
}

impl Env {
    fn new() -> Self {
        Self { dir: tempfile::tempdir().unwrap() }
    }

    fn models(&self) -> PathBuf {
        self.dir.path().join("models")
    }

    fn install(&self, name: &str, tweak: impl Fn(&mut Value)) -> PathBuf {
        let target = self.models().join(name);
        std::fs::create_dir_all(&target).unwrap();
        for file in ["all.onnx", "sleep.onnx"] {
            std::fs::copy(Path::new(FIXTURE).join(file), target.join(file)).unwrap();
        }
        let mut spec: Value = serde_json::from_str(&std::fs::read_to_string(Path::new(FIXTURE).join("model.json")).unwrap()).unwrap();
        spec["model_version"] = json!(name);
        tweak(&mut spec);
        std::fs::write(target.join("model.json"), spec.to_string()).unwrap();
        target
    }

    fn matcher(&self, version: &str) -> WatchMatch {
        let cfg = WatchMatchSection { enabled: true, model_dir: self.models().to_string_lossy().into_owned(), model_version: version.into(), ..Default::default() };
        let sink = Sink::open(LakeConfig::new(self.dir.path().join("lake"))).unwrap();
        WatchMatch::new(&cfg, sink, Status::new(lake::now_us()))
    }
}

fn aps(list: &[(i32, i32)]) -> Vec<(String, i32)> {
    list.iter().map(|(n, r)| (format!("AA:BB:CC:00:00:0{n}"), *r)).collect()
}

fn living() -> Vec<(String, i32)> {
    aps(&[(1, -40), (2, -60), (3, -90)])
}

fn bedroom() -> Vec<(String, i32)> {
    aps(&[(1, -75), (2, -45), (3, -60)])
}

fn ambiguous() -> Vec<(String, i32)> {
    aps(&[(1, -58), (2, -52), (3, -75)])
}

fn scan(wm: &WatchMatch, dev: &str, at: i64, minute: u32, list: &[(String, i32)]) -> WatchInference {
    wm.scan_at(dev, at, at, minute, list).expect("scan was processed")
}

#[test]
fn matches_rooms_and_picks_template_per_state() {
    let env = Env::new();
    env.install("v1", |_| {});
    let wm = env.matcher("latest");
    assert!(wm.active());
    let day = scan(&wm, "w", T, NOON, &living());
    assert_eq!((day.result_room.as_deref(), day.abstained, day.state.as_str(), day.template_id.as_str(), day.model_version.as_str(), day.matcher.as_str()), (Some("wohnzimmer"), false, "rest", "all", "v1", "room"));
    assert!(day.confidence > 0.9 && day.reason.is_none());
    let bed = scan(&wm, "w", T + MIN, NOON, &bedroom());
    assert_eq!(bed.result_room.as_deref(), Some("schlafzimmer"));
    wm.signals("other", [(T, Signal::Hr(58.0)), (T, Signal::Motion(0.1))]);
    let night = scan(&wm, "other", T, NIGHT, &living());
    assert_eq!((night.state.as_str(), night.template_id.as_str(), night.result_room.as_deref()), ("sleep", "sleep", Some("schlafzimmer")));
    assert_eq!(day.features_hash, scan(&wm, "third", T, NOON, &living().into_iter().rev().collect::<Vec<_>>()).features_hash);
    assert_ne!(day.features_hash, bed.features_hash);
}

#[test]
fn abstains_with_reasons() {
    let env = Env::new();
    env.install("v1", |_| {});
    let wm = env.matcher("v1");
    let low = scan(&wm, "w", T, NOON, &ambiguous());
    assert_eq!((low.abstained, low.reason.as_deref(), low.result_room.clone()), (true, Some("low_confidence"), None));
    assert!((low.confidence - 0.6).abs() < 0.01, "{}", low.confidence);

    env.install("margin", |s| {
        s["tau_room"] = json!(0.5);
        s["delta_room"] = json!(0.3);
    });
    let margin = env.matcher("margin");
    assert_eq!(scan(&margin, "w", T, NOON, &ambiguous()).reason.as_deref(), Some("low_margin"));

    env.install("sleep_only", |s| s["states"] = json!({"sleep": "sleep.onnx"}));
    let sleepy = env.matcher("sleep_only");
    let unknown = scan(&sleepy, "w", T, NOON, &living());
    assert_eq!((unknown.reason.as_deref(), unknown.state.as_str(), unknown.abstained, unknown.template_id.as_str()), (Some("unknown_state"), "rest", true, "none"));

    wm.signals("off", [(T, Signal::OnBody(false))]);
    let off = scan(&wm, "off", T, NOON, &living());
    assert_eq!((off.state.as_str(), off.reason.as_deref()), ("offbody", Some("unknown_state")));
    wm.signals("dock", [(T, Signal::Charging(true))]);
    assert_eq!(scan(&wm, "dock", T, NOON, &living()).state, "charging");
}

#[test]
fn no_model_is_a_fallback_not_a_crash() {
    let env = Env::new();
    let wm = env.matcher("latest");
    assert!(!wm.active() && wm.model_version().is_none());
    let row = scan(&wm, "w", T, NOON, &living());
    assert_eq!((row.abstained, row.reason.as_deref(), row.template_id.as_str(), row.model_version.as_str(), row.confidence), (true, Some("no_model"), "none", "none", 0.0));
    assert_eq!(wm.effective(T), None);
    env.install("broken", |s| s["rooms"] = json!(["a", "b", "c", "d"]));
    let broken = env.matcher("broken");
    assert!(!broken.active());
    assert_eq!(scan(&broken, "w", T, NOON, &living()).reason.as_deref(), Some("no_model"));
    std::fs::write(env.models().join("broken").join("model.json"), "{not json").unwrap();
    assert!(env.matcher("broken").reload().is_err());
    let disabled = WatchMatch::new(&WatchMatchSection::default(), Sink::open(LakeConfig::new(env.dir.path().join("lake2"))).unwrap(), Status::new(0));
    assert!(disabled.scan_at("w", T, T, NOON, &living()).is_none() && disabled.effective(T).is_none() && !disabled.enabled());
}

#[test]
fn holds_last_confident_room_for_t_hold_then_unknown() {
    let env = Env::new();
    env.install("v1", |s| s["t_hold_min"] = json!(10));
    let wm = env.matcher("v1");
    assert_eq!(wm.effective(T), Some(None));
    scan(&wm, "w", T, NOON, &living());
    assert_eq!(wm.effective(T + 9 * MIN), Some(Some(("wohnzimmer".into(), T))));
    assert_eq!(wm.effective(T + 10 * MIN), Some(Some(("wohnzimmer".into(), T))));
    assert_eq!(wm.effective(T + 11 * MIN), Some(None));
    let later = scan(&wm, "w", T + 20 * MIN, NOON, &ambiguous());
    assert!(later.abstained);
    assert_eq!(wm.effective(T + 21 * MIN), Some(None));
    let again = scan(&wm, "w", T + 22 * MIN, NOON, &bedroom());
    assert_eq!(again.result_room.as_deref(), Some("schlafzimmer"));
    assert_eq!(wm.effective(T + 25 * MIN), Some(Some(("schlafzimmer".into(), T + 22 * MIN))));

    env.install("long_hold", |s| s["t_hold_min"] = json!(60));
    let held = env.matcher("long_hold");
    scan(&held, "w", T, NOON, &living());
    let abstained = scan(&held, "w", T + 20 * MIN, NOON, &ambiguous());
    assert!(abstained.abstained);
    assert_eq!(held.effective(T + 21 * MIN), Some(Some(("wohnzimmer".into(), T))));
}

#[test]
fn ignores_stale_and_out_of_order_scans_and_logs_gaps() {
    let env = Env::new();
    env.install("v1", |_| {});
    let wm = env.matcher("v1");
    assert!(wm.scan_at("w", T - 10 * MIN, T, NOON, &living()).is_none());
    assert!(wm.scan_at("w", T, T, NOON, &[]).is_none());
    scan(&wm, "w", T, NOON, &living());
    assert!(wm.scan_at("w", T - MIN, T, NOON, &living()).is_none());
    assert!(wm.scan_at("w", T, T, NOON, &living()).is_none());
    wm.minute_tick(T + MIN);
    wm.minute_tick(T + 3 * MIN);
    wm.minute_tick(T + 4 * MIN);
    scan(&wm, "w", T + 5 * MIN, NOON, &living());
    wm.minute_tick(T + 8 * MIN);
    wm.sink().flush().unwrap();
    let rows = lake_reasons(&env);
    assert_eq!(rows, vec![None, Some("no_scan".to_string()), None, Some("no_scan".to_string())]);
}

fn lake_reasons(env: &Env) -> Vec<Option<String>> {
    use arrow::array::{Array, StringArray};
    let dir = env.dir.path().join("lake/raw/watch_inference");
    let mut files: Vec<_> = std::fs::read_dir(dir).unwrap().flatten().map(|e| e.path()).collect();
    files.sort();
    let mut out = Vec::new();
    for f in files {
        for b in lake::load_stream(&f).unwrap().batches {
            let col = arrow::compute::cast(b.column_by_name("reason").unwrap(), &arrow::datatypes::DataType::Utf8).unwrap();
            let col = col.as_any().downcast_ref::<StringArray>().unwrap();
            out.extend((0..col.len()).map(|i| (!col.is_null(i)).then(|| col.value(i).to_string())));
        }
    }
    out
}

#[test]
fn forward_step_is_time_aware() {
    let t = vec![vec![0.9, 0.1], vec![0.1, 0.9]];
    let prior = [0.98, 0.02];
    let emission = [0.4, 0.6];
    let quick = forward(Some((&prior, 1.0)), &t, &emission);
    assert!(quick[0] > 0.8, "{quick:?}");
    let slow = forward(Some((&prior, 60.0)), &t, &emission);
    assert!(slow[1] > slow[0], "{slow:?}");
    let first = forward(None, &t, &emission);
    assert!((first[1] - 0.6).abs() < 1e-3);
    let long_gap = forward(Some((&prior, 600.0)), &t, &emission);
    assert!((long_gap[1] - 0.6).abs() < 1e-3);
    let zero = forward(Some((&[1.0, 0.0], 1.0)), &[vec![1.0, 0.0], vec![0.0, 1.0]], &[0.0, 1.0]);
    assert!(zero[1] > 0.99);
    assert!((zero.iter().sum::<f64>() - 1.0).abs() < 1e-9);
}

#[test]
fn decision_thresholds() {
    assert_eq!(decide(&[0.8, 0.15, 0.05], 0.7, 0.3), (0, 0.8, None));
    assert_eq!(decide(&[0.6, 0.35, 0.05], 0.7, 0.3).2, Some("low_confidence"));
    assert_eq!(decide(&[0.7, 0.4, 0.0], 0.5, 0.4).2, Some("low_margin"));
    assert_eq!(decide(&[1.0], 0.9, 0.5).2, None);
}

#[test]
fn state_rules() {
    let cfg = WatchState::default();
    let sleep = cfg.sleep_minutes().unwrap();
    assert_eq!(sleep, (1380, 420));
    let rule = Rule::of(&cfg, sleep);
    let mut s = Signals::default();
    assert_eq!(s.classify(&rule, T, NOON), "rest");
    assert_eq!(s.classify(&rule, T, NIGHT), "rest");
    s.push(T - MIN, Signal::Hr(110.0));
    assert_eq!(s.classify(&rule, T, NOON), "active");
    assert_eq!(s.classify(&rule, T, NIGHT), "active");
    assert_eq!(s.classify(&rule, T + 11 * MIN, NOON), "rest");
    s.push(T, Signal::Hr(60.0));
    s.push(T, Signal::Motion(0.1));
    assert_eq!(s.classify(&rule, T, NIGHT), "rest");
    let mut quiet = Signals::default();
    quiet.push(T, Signal::Hr(58.0));
    quiet.push(T, Signal::Motion(0.1));
    assert_eq!(quiet.classify(&rule, T, NIGHT), "sleep");
    assert_eq!(quiet.classify(&rule, T, 300), "sleep");
    assert_eq!(quiet.classify(&rule, T, NOON), "rest");
    quiet.push(T, Signal::Motion(1.5));
    assert_eq!(quiet.classify(&rule, T, NOON), "active");
    quiet.push(T, Signal::OnBody(false));
    assert_eq!(quiet.classify(&rule, T, NOON), "offbody");
    assert_eq!(quiet.classify(&rule, T + 11 * MIN, NOON), "rest");
    quiet.push(T - MIN, Signal::OnBody(true));
    assert_eq!(quiet.classify(&rule, T, NOON), "offbody");
    quiet.push(T + MIN, Signal::Charging(true));
    assert_eq!(quiet.classify(&rule, T + MIN, NOON), "charging");
    assert_eq!(quiet.classify(&rule, T + 12 * MIN, NOON), "rest");
    quiet.push(T + 2 * MIN, Signal::Charging(false));
    assert_eq!(quiet.classify(&rule, T + 2 * MIN, NOON), "offbody");
    quiet.push(T + 3 * MIN, Signal::OnBody(true));
    assert_eq!(quiet.classify(&rule, T + 3 * MIN, NOON), "active");
    assert!(WatchState { sleep_hours: "22:30-6:15".into(), ..Default::default() }.sleep_minutes().unwrap() == (1350, 375));
    assert!(WatchState { sleep_hours: "late".into(), ..Default::default() }.sleep_minutes().is_err());
}

#[test]
fn model_state_rule_overrides_config() {
    let env = Env::new();
    env.install("v1", |s| s["state_rule"] = json!({"hr_rest_max": 50.0, "hr_active_min": 60.0, "motion_active_min": 0.5, "sleep_hours": [23, 7], "window_s": 600.0}));
    let wm = env.matcher("latest");
    wm.signals("w", [(T - MIN, Signal::Hr(65.0))]);
    let row = scan(&wm, "w", T, NOON, &living());
    assert_eq!(row.state, "active");
}

#[test]
fn offbody_and_charging_never_get_templates_and_unknown_aps_abstain() {
    let env = Env::new();
    env.install("v1", |_| {});
    let wm = env.matcher("latest");
    wm.signals("w", [(T - MIN, Signal::OnBody(false))]);
    let row = scan(&wm, "w", T, NOON, &living());
    assert_eq!((row.state.as_str(), row.reason.as_deref(), row.abstained), ("offbody", Some("unknown_state"), true));
    wm.signals("w", [(T + 21 * MIN, Signal::OnBody(true))]);
    let row = scan(&wm, "w", T + 22 * MIN, NOON, &[("ff:ff:ff:ff:ff:ff".to_string(), -50)]);
    assert_eq!((row.reason.as_deref(), row.abstained, row.result_room), (Some("no_known_ap"), true, None));
}

#[test]
fn hot_reloads_newest_model_only_for_latest() {
    let env = Env::new();
    env.install("v1", |s| s["created"] = json!("2026-10-29T12:00:00+00:00"));
    let wm = env.matcher("latest");
    assert_eq!(wm.model_version().as_deref(), Some("v1"));
    assert!(!wm.reload().unwrap());
    scan(&wm, "w", T, NOON, &living());
    env.install("v2", |s| s["created"] = json!("2026-10-30T12:00:00+00:00"));
    assert!(wm.reload().unwrap());
    assert_eq!(wm.model_version().as_deref(), Some("v2"));
    assert_eq!(scan(&wm, "w", T + MIN, NOON, &living()).model_version, "v2");
    assert!(!wm.reload().unwrap());
    let broken = env.install("v3", |s| s["created"] = json!("2026-10-31T12:00:00+00:00"));
    std::fs::remove_file(broken.join("all.onnx")).unwrap();
    assert!(wm.reload().is_err());
    assert_eq!(wm.model_version().as_deref(), Some("v2"));
    assert_eq!(scan(&wm, "w", T + 2 * MIN, NOON, &living()).result_room.as_deref(), Some("wohnzimmer"));

    let pinned_env = Env::new();
    pinned_env.install("v1", |_| {});
    let pinned = pinned_env.matcher("v1");
    pinned_env.install("v2", |s| s["created"] = json!("2027-01-01T00:00:00+00:00"));
    assert!(!pinned.reload().unwrap());
    assert_eq!(pinned.model_version().as_deref(), Some("v1"));

    let late = Env::new();
    let waiting = late.matcher("latest");
    assert!(!waiting.active());
    late.install("v1", |_| {});
    assert!(waiting.reload().unwrap() && waiting.active());
}

type Mutation = fn(&mut Value);

#[test]
fn invalid_models_are_rejected() {
    let env = Env::new();
    let cases: [(&str, Mutation); 5] = [
        ("stay", |s| s["smoothing"]["stay_prob"] = json!(1.5)),
        ("matcher", |s| s["matcher"] = json!("temp")),
        ("states", |s| s["states"] = json!({})),
        ("missing", |s| s["states"] = json!({"all": "nope.onnx"})),
        ("span", |s| s["scale"]["span"] = json!(0)),
    ];
    for (name, tweak) in cases {
        let dir = env.install(name, tweak);
        assert!(Model::load(&dir).is_err(), "{name}");
    }
    let ok = Model::load(&env.install("good", |_| {})).unwrap();
    assert_eq!(ok.features(&living()).len(), 4);
    assert!((ok.features(&living())[0] - 60.0 / 70.0).abs() < 1e-6);
    assert_eq!(ok.features(&living())[3], 0.0);
    let p = ok.probabilities("all", &ok.features(&bedroom())).unwrap();
    assert!((p.iter().sum::<f64>() - 1.0).abs() < 1e-9 && p[1] > 0.99);
    assert_eq!(ok.template_for("rest"), Some("all"));
    assert_eq!(ok.template_for("sleep"), Some("sleep"));
    assert_eq!(ok.template_for("offbody"), None);
}

#[test]
fn drift_alarm_needs_a_rise_of_half_and_enough_rows() {
    assert!(drift_of("room", (100, 30), (100, 10)).is_some());
    assert!(drift_of("room", (100, 14), (100, 10)).is_none());
    assert!(drift_of("room", (100, 3), (100, 1)).is_none());
    assert!(drift_of("room", (10, 9), (100, 10)).is_none());
    assert!(drift_of("room", (100, 20), (100, 20)).is_none());
    let env = Env::new();
    let today: chrono::NaiveDate = "2026-10-29".parse().unwrap();
    let clock = today.and_hms_opt(12, 0, 0).unwrap().and_utc().timestamp_micros();
    let sink = Sink::open_with_clock(LakeConfig::new(env.dir.path().join("lake")), Arc::new(move || clock)).unwrap();
    for back in 1..=14 {
        let day = today - chrono::Duration::days(back);
        let abstained = if back <= 7 { 3 } else { 1 };
        for i in 0..10 {
            let ts = day.and_hms_opt(12, i, 0).unwrap().and_utc().timestamp_micros();
            sink.append_on(day, WatchInference { ts, matcher: "room".into(), template_id: "all".into(), abstained: i < abstained, reason: (i < abstained).then(|| "low_confidence".into()), state: "rest".into(), features_hash: "0".into(), model_version: "v1".into(), ..Default::default() }).unwrap();
        }
    }
    sink.append_on(today - chrono::Duration::days(1), WatchInference { ts: clock - 3_600_000_000, matcher: "room".into(), template_id: "none".into(), abstained: true, reason: Some("no_scan".into()), state: "rest".into(), features_hash: "".into(), model_version: "v1".into(), ..Default::default() }).unwrap();
    sink.roll_pending().unwrap();
    let drifts = drift_check(sink.root(), today).unwrap();
    assert_eq!(drifts.len(), 1);
    assert_eq!((drifts[0].rows_current, drifts[0].rows_previous), (70, 70));
    assert!((drifts[0].current - 0.3).abs() < 1e-9 && (drifts[0].previous - 0.1).abs() < 1e-9);
    assert_eq!(drift_alarm(&sink).unwrap(), 1);
    sink.flush().unwrap();
}

const PIPELINE: &str = concat!(env!("CARGO_MANIFEST_DIR"), "/tests/fixtures/watchroom_trained");

fn pipeline_env() -> Env {
    let env = Env::new();
    let target = env.models().join("py1");
    std::fs::create_dir_all(&target).unwrap();
    for entry in std::fs::read_dir(Path::new(PIPELINE).join("py1")).unwrap().flatten() {
        std::fs::copy(entry.path(), target.join(entry.file_name())).unwrap();
    }
    env
}

fn lake_inference_rows(env: &Env) -> usize {
    let dir = env.dir.path().join("lake/raw/watch_inference");
    std::fs::read_dir(dir).unwrap().flatten().map(|e| lake::load_stream(&e.path()).unwrap().batches.iter().map(|b| b.num_rows()).sum::<usize>()).sum()
}

#[test]
fn python_trained_model_classifies_and_survives_model_removal() {
    let env = pipeline_env();
    let wm = env.matcher("latest");
    assert!(wm.active() && wm.model_version().as_deref() == Some("py1"));
    let samples: Vec<Value> = serde_json::from_str(&std::fs::read_to_string(Path::new(PIPELINE).join("samples.json")).unwrap()).unwrap();
    let (mut right, mut sure) = (0, 0);
    for (i, s) in samples.iter().enumerate() {
        let list: Vec<(String, i32)> = s["aps"].as_array().unwrap().iter().map(|a| (a[0].as_str().unwrap().to_uppercase(), a[1].as_i64().unwrap() as i32)).collect();
        let row = wm.scan_at(&format!("dev{i}"), T, T, NOON, &list).expect("scan was processed");
        assert_eq!((row.matcher.as_str(), row.model_version.as_str(), row.state.as_str()), ("room", "py1", "rest"));
        if !row.abstained {
            sure += 1;
            right += (row.result_room.as_deref() == s["room"].as_str()) as usize;
        }
    }
    assert!(sure * 10 >= samples.len() * 8, "coverage {sure}/{}", samples.len());
    assert_eq!(right, sure);
    wm.sink().flush().unwrap();
    assert_eq!(lake_inference_rows(&env), samples.len());
    std::fs::remove_dir_all(env.models()).unwrap();
    let after = env.matcher("latest");
    assert!(!after.active());
    let row = after.scan_at("dev0", T, T, NOON, &[("AA:BB:CC:00:00:01".into(), -50)]).unwrap();
    assert_eq!((row.abstained, row.reason.as_deref(), row.model_version.as_str(), row.result_room.clone()), (true, Some("no_model"), "none", None));
    assert_eq!(after.effective(T), None);
    after.sink().flush().unwrap();
    assert!(lake_reasons(&env).contains(&Some("no_model".to_string())));
}
