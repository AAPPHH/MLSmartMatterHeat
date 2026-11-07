use super::*;
use serde_json::{json, Value};

const TOL: f64 = 1e-6;

fn fixture() -> Value {
    serde_json::from_str(&std::fs::read_to_string(concat!(env!("CARGO_MANIFEST_DIR"), "/testdata/boost_parity.json")).unwrap()).unwrap()
}

fn model() -> BoostModel {
    BoostModel::parse(&fixture()["bundle"].to_string()).unwrap()
}

pub fn light_model() -> BoostModel {
    light_model_for("wohnzimmer")
}

pub fn light_model_for(room: &str) -> BoostModel {
    let mut b: Value = serde_json::from_str(&fixture()["bundle"].to_string().replace("\"wohnzimmer\"", &format!("\"{room}\""))).unwrap();
    b["kernel"]["len_min"] = 30.into();
    b["horizon_min"] = 60.into();
    for room in [room, "bad"] {
        let keep: Vec<Value> = b["rooms"][room]["samples"].as_array().unwrap().iter().take(4).cloned().collect();
        b["rooms"][room]["samples"] = Value::Array(keep);
    }
    BoostModel::parse(&b.to_string()).unwrap()
}

fn opt(v: &Value) -> Option<f64> {
    v.as_f64()
}

fn close(a: f64, b: f64, what: &str) {
    assert!((a - b).abs() <= TOL, "{what}: {a} vs {b}");
}

fn close_opt(a: Option<f64>, b: Option<f64>, what: &str) {
    match (a, b) {
        (Some(a), Some(b)) => close(a, b, what),
        (None, None) => {}
        _ => panic!("{what}: {a:?} vs {b:?}"),
    }
}

#[test]
fn decide_matches_the_python_implementation() {
    let (fx, m) = (fixture(), model());
    for (n, case) in fx["cases"].as_array().unwrap().iter().enumerate() {
        let ctx: Context = serde_json::from_value(case["ctx"].clone()).unwrap();
        let hist: Vec<f64> = case["valve_hist"].as_array().unwrap().iter().map(|v| v.as_f64().unwrap()).collect();
        let (room, scenario, target) = (case["room"].as_str().unwrap(), case["scenario"].as_str().unwrap(), case["target"].as_f64().unwrap());
        let exp = &case["expected"];
        let j = exp["j"].as_u64().map(|v| v as usize);
        let d = m.decide(room, &ctx, scenario, target, Some(&hist), &|_| j.unwrap_or(0)).unwrap();
        let tag = |k: &str| format!("case {n} {k}");
        close(d.d_safe, exp["d_safe"].as_f64().unwrap(), &tag("d_safe"));
        close(d.allowed, exp["allowed"].as_f64().unwrap(), &tag("allowed"));
        close(d.chosen, exp["chosen"].as_f64().unwrap(), &tag("chosen"));
        close(d.p_overshoot, exp["p_overshoot"].as_f64().unwrap(), &tag("p_overshoot"));
        close(d.p_at_allowed, exp["p_at_allowed"].as_f64().unwrap(), &tag("p_at_allowed"));
        close(d.p_no_boost, exp["p_no_boost"].as_f64().unwrap(), &tag("p_no_boost"));
        close_opt(d.known, opt(&exp["known"]), &tag("known"));
        close_opt(d.nearest_dist, opt(&exp["nearest_dist"]), &tag("nearest_dist"));
        close_opt(d.nearest_dose, opt(&exp["nearest_dose"]), &tag("nearest_dose"));
        assert_eq!(d.n_similar as u64, exp["n_similar"].as_u64().unwrap(), "{}", tag("n_similar"));
        assert_eq!(d.j.map(|v| v as u64), exp["j"].as_u64(), "{}", tag("j"));
        let (p, pk) = m.risk(room, &ctx, target, Some(&hist)).unwrap();
        for (i, v) in case["p_sample"].as_object().unwrap() {
            close(p[i.parse::<usize>().unwrap()], v.as_f64().unwrap(), &tag(&format!("p[{i}]")));
        }
        for (i, v) in case["peak_sample"].as_object().unwrap() {
            close(pk[i.parse::<usize>().unwrap()].iter().copied().fold(f64::MIN, f64::max), v.as_f64().unwrap(), &tag(&format!("peak[{i}]")));
        }
    }
}

#[test]
fn dose_grid_and_scenario_gate() {
    let m = model();
    assert_eq!(m.doses().len(), 151);
    assert!((m.doses()[150] - 20.0).abs() < 1e-9);
    assert!(m.scenario_allowed("morning") && m.scenario_allowed("homecoming") && !m.scenario_allowed("room_change"));
    let ctx: Context = serde_json::from_value(fixture()["cases"][0]["ctx"].clone()).unwrap();
    let d = m.decide("wohnzimmer", &ctx, "room_change", 20.0, None, &|_| 0).unwrap();
    assert_eq!((d.allowed, d.chosen, d.j), (0.0, 0.0, None));
    assert!(m.decide("kueche", &ctx, "morning", 20.0, None, &|_| 0).is_err());
}

#[test]
fn newest_valid_bundle_wins_with_fallback() {
    let dir = tempfile::tempdir().unwrap();
    let text = fixture()["bundle"].to_string();
    for (name, body) in [("20261101T000000Z", text.replace("\"version\":\"20261101T000000Z\"", "\"version\":\"old\"")), ("20261102T000000Z", text.replace("\"version\":\"20261101T000000Z\"", "\"version\":\"new\"")), ("20261103T000000Z", "{broken".to_string()), ("latest", text.clone())] {
        std::fs::create_dir_all(dir.path().join(name)).unwrap();
        std::fs::write(dir.path().join(name).join("bundle.json"), body).unwrap();
    }
    assert_eq!(try_load(dir.path().to_str().unwrap()).unwrap().version(), "new");
    std::fs::write(dir.path().join("20261102T000000Z/bundle.json"), text.replace("boost_learn", "other")).unwrap();
    assert_eq!(try_load(dir.path().to_str().unwrap()).unwrap().version(), "old");
    assert!(try_load(dir.path().join("none").to_str().unwrap()).is_none());
}

#[test]
fn bundles_outside_the_safety_envelope_are_rejected() {
    let ok = fixture()["bundle"].clone();
    assert!(BoostModel::parse(&ok.to_string()).is_ok());
    for (path, value) in [
        ("/boost_setpoint_c", json!(24.5)),
        ("/abort_c", json!(25.0)),
        ("/dose/min_min", json!(4.0)),
        ("/dose/max_min", json!(25.0)),
        ("/trust/growth", json!(1.3)),
        ("/risk/p_max", json!(0.2)),
        ("/risk/margin_k", json!(0.8)),
        ("/scenarios", json!(["morning", "room_change"])),
    ] {
        let mut b = ok.clone();
        *b.pointer_mut(path).unwrap() = value;
        let err = BoostModel::parse(&b.to_string()).unwrap_err().to_string();
        assert!(err.contains("safety envelope"), "{path}: {err}");
    }
    let mut tighter = ok.clone();
    *tighter.pointer_mut("/scenarios").unwrap() = json!(["morning"]);
    *tighter.pointer_mut("/risk/p_max").unwrap() = json!(0.05);
    assert!(BoostModel::parse(&tighter.to_string()).is_ok());
}

#[test]
fn the_risk_bound_is_attributable_to_the_boost() {
    let m = model();
    for case in fixture()["cases"].as_array().unwrap() {
        let ctx: Context = serde_json::from_value(case["ctx"].clone()).unwrap();
        let (room, target) = (case["room"].as_str().unwrap(), case["target"].as_f64().unwrap());
        let d = m.decide(room, &ctx, "morning", target, None, &|_| 0).unwrap();
        let (p, _) = m.risk(room, &ctx, target, None).unwrap();
        let expected = p.iter().rposition(|v| v - d.p_no_boost <= 0.10).map_or(0.0, |i| m.doses()[i]);
        assert_eq!(d.d_safe, expected);
    }
    let ctx: Context = serde_json::from_value(fixture()["cases"][1]["ctx"].clone()).unwrap();
    let manual = m.manual("wohnzimmer", &ctx, 20.0, None, 7.0).unwrap();
    assert_eq!((manual.allowed, manual.chosen, manual.known, manual.n_similar), (7.0, 7.0, None, 0));
}
