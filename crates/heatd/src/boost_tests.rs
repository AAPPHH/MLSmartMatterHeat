use super::*;
use crate::window::{LOCAL_TEMPERATURE, PI_HEATING_DEMAND};
use serde_json::Value;

const TOL: f64 = 1e-4;
const DATA: &str = concat!(env!("CARGO_MANIFEST_DIR"), "/testdata");
const BUNDLES: &str = concat!(env!("CARGO_MANIFEST_DIR"), "/tests/fixtures/thermal_bundle");

fn fixture(name: &str) -> Option<Value> {
    std::fs::read_to_string(format!("{DATA}/{name}")).ok().map(|t| serde_json::from_str(&t).unwrap())
}

fn physics() -> Value {
    fixture("thermal_physics_parity.json").unwrap()
}

fn nb_of(v: &Value) -> BTreeMap<String, (f64, f64)> {
    v.as_object().unwrap().iter().map(|(k, p)| (k.clone(), (p[0].as_f64().unwrap(), p[1].as_f64().unwrap()))).collect()
}

fn near(a: f64, b: f64, what: &str) {
    assert!((a - b).abs() < TOL, "{what}: rust {a} python {b}");
}

#[test]
fn physics_predict_matches_python() {
    let fx = physics();
    for structure in ["rc2", "rc1"] {
        let model = &fx["models"][structure];
        let ph = Physics::from_doc(&model["doc"]).unwrap();
        for case in model["predict"].as_array().unwrap() {
            let env = Env { tout: case["tout"].as_f64().unwrap(), wind: case["wind"].as_f64().unwrap(), rad: case["rad"].as_f64().unwrap(), night: case["night"].as_bool().unwrap(), nb: nb_of(&case["nb"]) };
            let mut x = (case["x0"][0].as_f64().unwrap(), case["x0"][1].as_f64().unwrap());
            for (k, row) in case["out"].as_array().unwrap().iter().enumerate() {
                let u = match case["u"].as_f64() {
                    Some(u) => u,
                    None => ph.pi(case["setpoint"].as_f64().unwrap(), ph.sensor(x)),
                };
                x = ph.step(x, u, &env, 1.0 / 60.0);
                let what = format!("{structure} {} step {k}", case["name"]);
                near(x.0, row[0].as_f64().unwrap(), &what);
                near(x.1, row[1].as_f64().unwrap(), &what);
                near(ph.sensor(x), row[2].as_f64().unwrap(), &what);
                near(u, row[3].as_f64().unwrap(), &what);
            }
        }
    }
}

#[test]
fn kalman_matches_python() {
    let fx = physics();
    let cfg = &fx["cfg"];
    let (q_r, q_a, noise, p0) = (cfg["q_r"].as_f64().unwrap(), cfg["q_a"].as_f64().unwrap(), cfg["noise_r"].as_f64().unwrap(), cfg["p0"].as_f64().unwrap());
    for structure in ["rc2", "rc1"] {
        let model = &fx["models"][structure];
        let (ph, k) = (Physics::from_doc(&model["doc"]).unwrap(), &model["kalman"]);
        let col = |name: &str| k[name].as_array().unwrap().iter().map(|v| v.as_f64()).collect::<Vec<_>>();
        let (y, u, tout, wind, rad) = (col("y"), col("u"), col("tout"), col("wind"), col("rad"));
        let night: Vec<bool> = k["night"].as_array().unwrap().iter().map(|v| v.as_bool().unwrap()).collect();
        let nb = |kind: &str, field: &str, i: usize| k["nb"][kind][field][i].as_f64().unwrap();
        let h = 1.0 / 60.0;
        let first = y.iter().flatten().next().copied().unwrap();
        let mut filter = Kalman::new(first, p0);
        let mut innovations = Vec::new();
        for i in 0..y.len() {
            if let Some(yk) = y[i] {
                let (v, s) = filter.update(yk, ph.w, noise * noise);
                innovations.push((i, v, s));
            }
            let want = &k["x_after_update"][i];
            near(filter.x.0, want[0].as_f64().unwrap(), &format!("{structure} x1 {i}"));
            near(filter.x.1, want[1].as_f64().unwrap(), &format!("{structure} x2 {i}"));
            let env = Env { tout: tout[i].unwrap(), wind: wind[i].unwrap(), rad: rad[i].unwrap(), night: night[i], nb: ["wall", "ceiling"].iter().map(|kind| (kind.to_string(), (nb(kind, "n", i), nb(kind, "s", i)))).collect() };
            filter.predict(&ph.terms(u[i].unwrap(), &env, h), q_r * h, q_a * h);
        }
        let (idx, vs, ss) = (k["innov_idx"].as_array().unwrap(), col("innov"), col("innov_var"));
        assert_eq!(innovations.len(), idx.len());
        for (n, (i, v, s)) in innovations.iter().enumerate() {
            assert_eq!(*i as u64, idx[n].as_u64().unwrap());
            near(*v, vs[n].unwrap(), "innovation");
            near(*s, ss[n].unwrap(), "innovation variance");
        }
    }
}

#[test]
fn onnx_features_and_outputs_match_python() {
    let bundle = Bundle::load(&Path::new(BUNDLES).join("20260101T000000Z")).unwrap();
    assert!(bundle.has_residual());
    let want = &physics()["onnx"];
    let g = |k: &str| want["inputs"][k].as_f64().unwrap();
    let (sensor, valve) = ([g("sensor_lag_0")], [g("valve_lag_0")]);
    let input = FeatIn { sensor: &sensor, valve: &valve, outdoor: g("outdoor_c"), radiation: 0.0, hour: 7.0, others: 0.0, since: 10.0, phys_sensor: g("phys_sensor_pred"), phys_air: g("phys_air"), ambient: [f64::NAN; 3], room: "t" };
    assert_eq!(bundle.latent.as_ref().unwrap().features(&input), vec![20.5, 19.8, 0.6, 1.0]);
    near(bundle.latent.as_ref().unwrap().run(&input).unwrap(), want["latent"].as_f64().unwrap(), "latent");
    near(bundle.residual.as_ref().unwrap().run(&input).unwrap(), want["residual"].as_f64().unwrap(), "residual");
    assert_eq!(bundle.backtest("t", RESIDUAL), (Some(0.2), 5));
}

#[test]
fn feature_vocabulary_is_complete_and_lags_pad() {
    let names = ["sensor_lag_0", "sensor_lag_60", "valve_lag_5", "outdoor_c", "radiation_wm2", "hour_sin", "hour_cos", "others_heating", "minutes_since_boost", "phys_sensor_pred", "phys_air", "ambient_nvme_c", "ambient_pkg_w", "ambient_die_c", "room_bad"];
    let feats: Vec<Feat> = names.iter().map(|n| parse_feat(n).unwrap()).collect();
    assert!(parse_feat("bogus").is_err());
    let sensor = [18.0, 19.0, 20.0];
    let input = FeatIn { sensor: &sensor, valve: &[0.5], outdoor: 3.0, radiation: 40.0, hour: 6.0, others: 2.0, since: 999.0, phys_sensor: 20.1, phys_air: 19.9, ambient: [41.0, f64::NAN, 55.0], room: "bad" };
    let x = featurize(&feats, &input);
    assert_eq!(&x[..4], &[20.0, 18.0, 0.5, 3.0]);
    assert!((x[5] - 1.0).abs() < 1e-6 && x[6].abs() < 1e-6);
    assert_eq!((x[7], x[8], x[11]), (2.0, 240.0, 41.0));
    assert!(x[12].is_nan() && x[14] == 1.0);
}

fn room_ctx<'a>(ph: &'a Physics, bundle: &'a Bundle, target: f64) -> Ctx<'a> {
    Ctx {
        phys: ph,
        bundle,
        room: "t",
        x: (19.2, 19.0),
        env: Env { tout: 4.0, wind: 2.0, rad: 0.0, night: false, nb: BTreeMap::new() },
        hour: 7.0,
        target,
        boost_sp: 28.0,
        sensor: vec![19.0; 61],
        valve: vec![0.0; 61],
        others: 0.0,
        since: 0.0,
        ambient: [f64::NAN; 3],
        h: 1.0 / 60.0,
    }
}

fn test_bundle() -> Bundle {
    Bundle::load(&Path::new(BUNDLES).join("20260101T000000Z")).unwrap()
}

#[test]
fn rollout_places_the_peak_exactly_at_the_target() {
    let bundle = test_bundle();
    let ph = &bundle.rooms["t"].phys;
    for variant in VARIANTS {
        let ctx = room_ctx(ph, &bundle, 21.0);
        let (t_off, sim) = ctx.plan(variant, 240).unwrap();
        assert!(t_off > 0 && t_off < 240, "{variant}: t_off {t_off}");
        assert!(sim.peak <= 21.0 + 1e-9, "{variant}: peak {}", sim.peak);
        let later = if variant == GREY { ctx.grey(t_off + 1) } else { ctx.residual(t_off + 1).unwrap() };
        assert!(later.peak > 21.0, "{variant}: one minute later the peak is {}", later.peak);
        assert!(sim.peak > 20.5, "{variant}: peak {} is not close to the target", sim.peak);
        let hotter = ctx.plan(variant, 240).unwrap().0;
        let colder = room_ctx(ph, &bundle, 20.0).plan(variant, 240).unwrap().0;
        assert!(colder < hotter, "{variant}: lower target must switch off earlier ({colder} vs {hotter})");
    }
    let ctx = room_ctx(ph, &bundle, 18.9);
    assert_eq!(ctx.plan(GREY, 240).unwrap().0, 0);
}

fn config(extra: &str) -> Config {
    Config::parse(&format!("[boost]\nmodel_dir = '{BUNDLES}'\n{extra}\n[[rooms]]\nname = 't'\nnodes = [5]\n")).unwrap()
}

fn drive(boost: &Boost, from_min: i64, to_min: i64, setpoint: f64, blocked: Option<&'static str>, mut temp: impl FnMut(i64) -> f64) -> Vec<StepOut> {
    let mut outs = Vec::new();
    for m in from_min..to_min {
        let ts = m * 60_000;
        boost.observe(5, PI_HEATING_DEMAND, 80.0, ts);
        boost.observe(5, LOCAL_TEMPERATURE, temp(m), ts);
        outs.push(boost.step(&StepIn { now_ms: ts, hour: 7.0, outdoor: Some((4.0, Some(2.0))), radiation: Some(0.0), rooms: &[RoomIn { room: "t".into(), setpoint, blocked }] }));
    }
    outs
}

fn rise(m: i64) -> f64 {
    (19.0 + m as f64 * 0.03).min(21.5)
}

fn slow(m: i64) -> f64 {
    (19.0 + m as f64 * 0.01).min(20.0)
}

fn cycle_events(outs: &[StepOut]) -> Vec<Value> {
    outs.iter().flat_map(|o| o.events.iter()).map(|e| serde_json::from_str(e.detail.as_ref().unwrap()).unwrap()).collect()
}

#[test]
fn shadow_cycle_plans_both_variants_without_controlling() {
    let boost = Boost::new(&config(""));
    let mut outs = drive(&boost, 0, 5, 18.0, None, |_| 19.0);
    outs.extend(drive(&boost, 5, 120, 21.0, None, slow));
    assert!(outs.iter().all(|o| o.directives.is_empty()), "shadow must not control");
    let view = boost.view("t");
    assert_eq!(view["variants"]["grey_box"]["mode"], "shadow");
    assert!(view["variants"]["grey_box"]["planned_t_off"].as_f64().unwrap() > 0.0);
    assert!(view["variants"]["residual"]["predicted_peak"].as_f64().is_some());
    let events = cycle_events(&outs);
    assert!(events.is_empty(), "cycle still running");
    outs.extend(drive(&boost, 120, 125, 21.0, Some("window_open"), slow));
    let rows = cycle_events(&outs);
    assert_eq!(rows.len(), 2);
    for row in &rows {
        assert_eq!((row["mode"].as_str(), row["end"].as_str(), row["room"].as_str()), (Some("shadow"), Some("window_open"), Some("t")));
        assert!(row["planned_t_off_min"].as_f64().unwrap() > 0.0 && row["predicted_peak_c"].as_f64().unwrap() <= 21.0 + 1e-6);
        assert!(row["applied_t_off_min"].is_null() && row["minutes_to_target_0_3"].as_f64().is_some() && row["model_version"] == "20260101T000000Z");
    }
    assert_eq!(rows.iter().map(|r| r["variant"].as_str().unwrap()).collect::<Vec<_>>(), vec!["grey_box", "residual"]);
}

fn closed_loop(boost: &Boost, minutes: i64, target: f64) -> (Vec<StepOut>, f64) {
    let bundle = test_bundle();
    let ph = &bundle.rooms["t"].phys;
    let env = Env { tout: 4.0, wind: 2.0, rad: 0.0, night: false, nb: BTreeMap::new() };
    let (mut x, mut sp, mut peak, mut outs) = ((19.0, 19.0), 18.0, 19.0f64, Vec::new());
    for m in 0..minutes {
        let desired = if m < 5 { 18.0 } else { target };
        if m == 5 {
            sp = target;
        }
        let (sensor, ts) = (ph.sensor(x), m * 60_000);
        let u = ph.pi(sp, sensor);
        boost.observe(5, PI_HEATING_DEMAND, u * 100.0, ts);
        boost.observe(5, LOCAL_TEMPERATURE, sensor, ts);
        let out = boost.step(&StepIn { now_ms: ts, hour: 7.0, outdoor: Some((4.0, Some(2.0))), radiation: Some(0.0), rooms: &[RoomIn { room: "t".into(), setpoint: desired, blocked: None }] });
        match out.directives.get("t") {
            Some(Directive::Boost { .. }) => sp = 28.0,
            Some(Directive::Switch) => sp = target,
            None => {}
        }
        x = ph.step(x, u, &env, 1.0 / 60.0);
        peak = peak.max(x.1);
        outs.push(out);
    }
    (outs, peak)
}

fn force_live(boost: &Boost, variant: &str) {
    boost.0.lock().unwrap().live.insert("t".into(), (variant.into(), "test".into()));
}

#[test]
fn live_grey_box_closed_loop_lands_the_air_peak_on_the_target() {
    let boost = Boost::new(&config("explore_min = 0.0"));
    force_live(&boost, GREY);
    let (outs, peak) = closed_loop(&boost, 400, 21.0);
    let directives: Vec<&Directive> = outs.iter().flat_map(|o| o.directives.values()).collect();
    assert!(matches!(directives.first(), Some(Directive::Boost { .. })));
    assert_eq!(directives.iter().filter(|d| ***d == Directive::Switch).count(), 1);
    assert!((peak - 21.0).abs() < 0.3, "air peak {peak}");
    let rows = cycle_events(&outs);
    let live = rows.iter().find(|r| r["mode"] == "live").unwrap();
    assert_eq!((live["variant"].as_str(), live["end"].as_str()), (Some("grey_box"), Some("held")));
    assert!(live["overshoot_c"].as_f64().is_some() && live["valve_over_50_min"].as_f64().unwrap() > 0.0 && live["exploration"] == true);
    assert!(rows.iter().any(|r| r["mode"] == "shadow" && r["variant"] == "residual") && rows.iter().all(|r| r["variant"] != "baseline"));
}

#[test]
fn live_switch_time_is_randomised_within_the_explore_window() {
    let boost = Boost::new(&config("live_rooms_override = ['t']
explore_min = 10.0
seed = 3"));
    assert_eq!(boost.0.lock().unwrap().live["t"].0, RESIDUAL);
    let (outs, _) = closed_loop(&boost, 30, 21.0);
    assert!(matches!(outs[5].directives.get("t"), Some(Directive::Boost { .. })));
    let cycle = &boost.view("t")["cycle"];
    let (applied, offset) = (cycle["applied_t_off"].as_f64().unwrap(), cycle["explore_offset_min"].as_f64().unwrap());
    let planned = boost.view("t")["variants"]["residual"]["planned_t_off"].as_f64().unwrap();
    assert!(offset.abs() <= 10.0 && offset != 0.0, "offset {offset}");
    assert!((applied - (planned + offset).clamp(0.0, 240.0)).abs() < 1e-6, "applied {applied} planned {planned} offset {offset}");
    let other = Boost::new(&config("live_rooms_override = ['t']
explore_min = 10.0
seed = 4"));
    closed_loop(&other, 10, 21.0);
    assert_ne!(other.view("t")["cycle"]["explore_offset_min"], cycle["explore_offset_min"]);
}

#[test]
fn live_boost_aborts_on_blockers_and_target_decrease() {
    for (blocked, setpoint, why) in [(Some("window_open"), 21.0, "window_open"), (Some("away"), 21.0, "away"), (Some("manual_hold"), 21.0, "manual_hold"), (None, 19.5, "target_decrease")] {
        let boost = Boost::new(&config("live_rooms_override = ['t']\nseed = 3"));
        drive(&boost, 0, 5, 18.0, None, |_| 19.0);
        let started = drive(&boost, 5, 10, 21.0, None, rise);
        assert!(matches!(started.last().unwrap().directives.get("t"), Some(Directive::Boost { .. })));
        let stop = drive(&boost, 10, 11, setpoint, blocked, rise);
        let rows = cycle_events(&stop);
        assert!(rows.iter().all(|r| r["end"] == why) && !rows.is_empty(), "{why}");
        assert!(!matches!(stop[0].directives.get("t"), Some(Directive::Boost { .. })), "{why}");
        assert_eq!(stop[0].directives.contains_key("t"), why == "target_decrease");
    }
}

#[test]
fn no_cycle_without_bundle_room_or_for_small_rises() {
    let other = Config::parse(&format!("[boost]\nmodel_dir = '{BUNDLES}'\n[[rooms]]\nname = 'x'\nnodes = [9]\n")).unwrap();
    let boost = Boost::new(&other);
    boost.observe(9, LOCAL_TEMPERATURE, 19.0, 0);
    let out = boost.step(&StepIn { now_ms: 60_000, hour: 7.0, outdoor: None, radiation: None, rooms: &[RoomIn { room: "x".into(), setpoint: 18.0, blocked: None }] });
    assert!(out.events.is_empty() && boost.view("x")["cycle"].is_null());
    let small = Boost::new(&config(""));
    drive(&small, 0, 5, 20.0, None, |_| 19.0);
    drive(&small, 5, 10, 20.6, None, |_| 19.0);
    assert!(small.view("t")["cycle"].is_null());
}

#[test]
fn live_gate_needs_shadow_days_and_backtest_cycles() {
    let boost = Boost::new(&config(""));
    assert!(boost.0.lock().unwrap().live.is_empty());
    boost.set_gate_days(HashMap::from([("t".to_string(), 6)]));
    assert!(boost.0.lock().unwrap().live.is_empty());
    boost.set_gate_days(HashMap::from([("t".to_string(), 7)]));
    let live = boost.0.lock().unwrap().live.clone();
    assert_eq!(live["t"].0, RESIDUAL);
    assert!(live["t"].1.contains("0.200 < grey_box 0.300") && live["t"].1.contains("7 shadow days"));
    let few = Config::parse(&format!("[boost]\nmodel_dir = '{BUNDLES}'\nlive_min_backtest_cycles = 6\n[[rooms]]\nname = 't'\nnodes = [5]\n")).unwrap();
    let boost = Boost::new(&few);
    boost.set_gate_days(HashMap::from([("t".to_string(), 30)]));
    assert!(boost.0.lock().unwrap().live.is_empty());
}

#[test]
fn newest_bundle_wins_by_created_stamp() {
    let dir = tempfile::tempdir().unwrap();
    for (name, created) in [("20260101T000000Z", "2026-01-01T00:00:00Z"), ("20260301T000000Z", "2026-03-01T00:00:00Z"), ("20260201T000000Z", "2026-02-01T00:00:00Z")] {
        std::fs::create_dir_all(dir.path().join(name)).unwrap();
        std::fs::write(dir.path().join(name).join("bundle.json"), format!("{{\"created\":\"{created}\"}}")).unwrap();
    }
    assert_eq!(newest_bundle_dir(dir.path()).unwrap().0, "20260301T000000Z");
    assert!(newest_bundle_dir(&dir.path().join("missing")).is_none());
}

