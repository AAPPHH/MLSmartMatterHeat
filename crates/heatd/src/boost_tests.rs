use super::*;
use crate::window::{LOCAL_TEMPERATURE, PI_HEATING_DEMAND};
use serde_json::Value;

const DATA: &str = concat!(env!("CARGO_MANIFEST_DIR"), "/testdata");
const BUNDLES: &str = concat!(env!("CARGO_MANIFEST_DIR"), "/testdata/thermal_bundle");
const ROOM: &str = "wohnzimmer";
const BOOST_SP: f64 = 28.0;

fn fixture() -> Value {
    serde_json::from_str(&std::fs::read_to_string(format!("{DATA}/thermal_ml_parity.json")).unwrap()).unwrap()
}

fn num(v: &Value) -> f64 {
    v.as_f64().unwrap_or(f64::NAN)
}

fn nums(v: &Value) -> Vec<f64> {
    v.as_array().unwrap().iter().map(num).collect()
}

fn reversed(v: &Value) -> Vec<f64> {
    let mut out = nums(v);
    out.reverse();
    out
}

fn nb_of(v: &Value) -> BTreeMap<String, (f64, f64)> {
    v.as_object().unwrap().iter().map(|(k, p)| (k.clone(), (num(&p[0]), num(&p[1])))).collect()
}

fn near(a: f64, b: f64, tol: f64, what: &str) {
    assert!((a.is_nan() && b.is_nan()) || (a - b).abs() <= tol * (1.0 + b.abs()), "{what}: rust {a} python {b}");
}

fn bundle_dir() -> PathBuf {
    Path::new(DATA).join(fixture()["bundle_dir"].as_str().unwrap())
}

fn test_bundle() -> Bundle {
    Bundle::load(&bundle_dir()).unwrap()
}

#[test]
fn physics_step_matches_python() {
    let fx = fixture();
    let tol = num(&fx["tol"]);
    for case in fx["physics"].as_array().unwrap() {
        let ph = Physics::from_doc(&json!({"params": case["params"], "structure": case["structure"], "valve_gain": 0.5})).unwrap();
        let env = Env { tout: num(&case["tout"]), wind: num(&case["wind"]), rad: num(&case["rad"]), night: case["night"].as_bool().unwrap(), nb: nb_of(&case["nb"]) };
        let x = ph.step((num(&case["x"][0]), num(&case["x"][1])), num(&case["u"]), &env, num(&case["dt_s"]) / 3600.0);
        near(x.0, num(&case["next"][0]), tol, "x1");
        near(x.1, num(&case["next"][1]), tol, "x2");
        near(ph.sensor(x), num(&case["sensor_next"]), tol, "sensor");
    }
}

#[test]
fn kalman_matches_python() {
    let fx = fixture();
    let tol = num(&fx["tol"]);
    for k in fx["kalman"].as_array().unwrap() {
        let ph = Physics::from_doc(&json!({"params": k["params"], "structure": k["structure"], "valve_gain": 0.5})).unwrap();
        let cfg = &k["cfg"];
        let (y, u, tout, wind, rad) = (nums(&k["y"]), nums(&k["u"]), nums(&k["tout"]), nums(&k["wind"]), nums(&k["rad"]));
        let night: Vec<bool> = k["night"].as_array().unwrap().iter().map(|v| v.as_bool().unwrap()).collect();
        let (h, noise) = (num(&k["dt_s"]) / 3600.0, num(&cfg["noise_r"]));
        let mut filter = Kalman::new(y.iter().copied().find(|v| v.is_finite()).unwrap(), num(&cfg["p0"]));
        let mut innovations = Vec::new();
        for i in 0..y.len() {
            if y[i].is_finite() {
                innovations.push((i, filter.update(y[i], ph.w, noise * noise)));
            }
            let what = format!("{} {i}", k["structure"]);
            near(filter.x.0, num(&k["x_post"][i][0]), tol, &format!("x1 {what}"));
            near(filter.x.1, num(&k["x_post"][i][1]), tol, &format!("x2 {what}"));
            near(filter.p.0, num(&k["p_post"][i][0]), tol, &format!("p11 {what}"));
            near(filter.p.1, num(&k["p_post"][i][1]), tol, &format!("p12 {what}"));
            near(filter.p.2, num(&k["p_post"][i][2]), tol, &format!("p22 {what}"));
            let env = Env { tout: tout[i], wind: wind[i], rad: rad[i], night: night[i], nb: nb_of(&k["nb"]) };
            filter.predict(&ph.terms(u[i], &env, h), num(&cfg["q_r"]) * h, num(&cfg["q_a"]) * h);
        }
        let (steps, v, s) = (nums(&k["innovation_steps"]), nums(&k["innovations"]), nums(&k["innovation_var"]));
        assert_eq!(innovations.len(), steps.len());
        for (n, (i, (iv, is))) in innovations.iter().enumerate() {
            assert_eq!(*i as f64, steps[n]);
            near(*iv, v[n], tol, "innovation");
            near(*is, s[n], tol, "innovation variance");
        }
    }
}

fn feat_in<'a>(ctx: &'a Value, sensor: &'a [f64], valve: &'a [f64], room: &'a str) -> FeatIn<'a> {
    let g = |k: &str| num(&ctx[k]);
    FeatIn {
        sensor,
        valve,
        outdoor: g("outdoor_c"),
        radiation: g("radiation_wm2"),
        hour: g("hour"),
        others: g("others_heating"),
        phys_sensor: g("phys_sensor_pred"),
        phys_air: g("phys_air"),
        ambient: AMBIENT.map(|s| g(&format!("ambient_{s}"))),
        room,
    }
}

#[test]
fn features_and_onnx_outputs_match_python() {
    let (fx, bundle) = (fixture(), test_bundle());
    let (latent, residual) = (bundle.latent.as_ref().unwrap(), bundle.residual.as_ref().unwrap());
    assert!(!fx["features"].as_array().unwrap().is_empty());
    for case in fx["features"].as_array().unwrap() {
        let (sensor, valve) = (reversed(&case["ctx"]["S"]), reversed(&case["ctx"]["V"]));
        let room = case["room"].as_str().unwrap();
        let input = feat_in(&case["ctx"], &sensor, &valve, room);
        for (net, vector) in [(residual, "residual_vector"), (latent, "latent_vector")] {
            let (got, want) = (net.features(&input), nums(&case[vector]));
            assert_eq!(got.len(), want.len());
            for (i, (a, b)) in got.iter().zip(&want).enumerate() {
                near(f64::from(*a), *b, 1e-5, &format!("{room} {vector}[{i}]"));
            }
        }
        near(latent.run(&input).unwrap(), num(&case["room_temp"]), 1e-4, "room_temp");
        near(residual.run(&input).unwrap(), num(&case["delta"]), 1e-4, "delta");
    }
}

fn rollout_ctx<'a>(bundle: &'a Bundle, case: &'a Value) -> Ctx<'a> {
    let (env, room) = (&case["env"], case["room"].as_str().unwrap());
    let mut valve = reversed(&case["V0"]);
    valve.push(0.0);
    let ambient = nums(&env["ambient"]);
    Ctx {
        phys: &bundle.rooms[room].phys,
        bundle,
        room,
        x: (num(&case["x0"][0]), num(&case["x0"][1])),
        env: Env { tout: num(&env["tout"]), wind: num(&env["wind"]), rad: num(&env["rad"]), night: false, nb: nb_of(&env["nb"]) },
        hour: num(&case["start_hour"]),
        target: num(&case["target"]),
        sensor: reversed(&case["S0"]),
        valve,
        others: num(&env["others"]),
        ambient: [ambient[0], ambient[1], ambient[2]],
        h: 1.0 / 60.0,
    }
}

#[test]
fn rollouts_and_t_off_match_python() {
    let (fx, bundle) = (fixture(), test_bundle());
    assert_eq!(bundle.residual.as_ref().unwrap().feedback, 0.5);
    for case in fx["rollouts"].as_array().unwrap() {
        let variant = case["variant"].as_str().unwrap();
        let ctx = rollout_ctx(&bundle, case);
        let offs: Vec<usize> = (0..=case["toff_max"].as_u64().unwrap() as usize).collect();
        let sims = ctx.simulate(variant, &offs).unwrap();
        let want = nums(&case["peaks"]);
        assert_eq!(sims.len(), want.len());
        for (t, (sim, peak)) in sims.iter().zip(&want).enumerate() {
            near(sim.peak, *peak, 2e-4, &format!("{variant} peak t_off {t}"));
        }
        for fixed in case["fixed"].as_array().unwrap() {
            let t = fixed["t_off_min"].as_u64().unwrap() as usize;
            near(sims[t].peak, num(&fixed["peak"]), 2e-4, &format!("{variant} fixed {t}"));
        }
        let (t_off, sim) = ctx.plan(variant, TOFF_MAX).unwrap();
        assert_eq!(t_off as u64, case["t_off_min"].as_u64().unwrap(), "{variant} t_off");
        near(sim.peak, num(&case["predicted_peak_c"]), 2e-4, &format!("{variant} predicted peak"));
    }
}

#[test]
fn feature_vocabulary_is_complete_and_lags_pad() {
    let names = ["sensor_lag_0", "sensor_lag_60", "valve_lag_5", "outdoor_c", "radiation_wm2", "hour_sin", "hour_cos", "others_heating", "minutes_since_boost", "phys_sensor_pred", "phys_air", "ambient_nvme_c", "ambient_pkg_w", "ambient_die_c", "room_bad"];
    let feats: Vec<Feat> = names.iter().map(|n| parse_feat(n).unwrap()).collect();
    assert!(parse_feat("bogus").is_err());
    let sensor = [18.0, 19.0, 20.0];
    let input = FeatIn { sensor: &sensor, valve: &[0.5], outdoor: 3.0, radiation: 40.0, hour: 6.0, others: 2.0, phys_sensor: 20.1, phys_air: 19.9, ambient: [41.0, f64::NAN, 55.0], room: "bad" };
    let x = featurize(&feats, &input);
    assert_eq!(&x[..4], &[20.0, 18.0, 0.5, 3.0]);
    assert!((x[5] - 1.0).abs() < 1e-6 && x[6].abs() < 1e-6);
    assert_eq!((x[7], x[8], x[11]), (2.0, 240.0, 41.0));
    assert!(x[12].is_nan() && x[14] == 1.0);
}

#[test]
fn minutes_since_boost_is_the_start_of_the_latest_half_open_run() {
    let mut valve = vec![0.0; 241];
    for v in &mut valve[100..130] {
        *v = 0.6;
    }
    assert_eq!(since_boost(&valve), 140.0);
    valve[240] = 1.0;
    assert_eq!(since_boost(&valve), 0.0);
    assert_eq!(since_boost(&[0.0; 241]), 240.0);
    assert_eq!(since_boost(&[0.8; 241]), 240.0);
    let mut old = vec![0.0; 241];
    old[..20].fill(1.0);
    assert_eq!(since_boost(&old), 240.0);
}

fn room_ctx<'a>(bundle: &'a Bundle, target: f64) -> Ctx<'a> {
    Ctx {
        phys: &bundle.rooms[ROOM].phys,
        bundle,
        room: ROOM,
        x: (19.2, 19.0),
        env: Env { tout: 4.0, wind: 2.0, rad: 0.0, night: false, nb: BTreeMap::new() },
        hour: 7.0,
        target,
        sensor: vec![19.0; SENSOR_HISTORY_MIN + 1],
        valve: vec![0.0; VALVE_HISTORY_MIN + 1],
        others: 0.0,
        ambient: [f64::NAN; 3],
        h: 1.0 / 60.0,
    }
}

#[test]
fn grey_plan_follows_the_python_rule() {
    let bundle = test_bundle();
    let ctx = room_ctx(&bundle, 21.0);
    let offs: Vec<usize> = (0..=TOFF_MAX).collect();
    let sims = ctx.simulate(GREY, &offs).unwrap();
    let (t_off, sim) = ctx.plan(GREY, TOFF_MAX).unwrap();
    assert!(t_off > 0 && t_off < TOFF_MAX && sim.peak <= 21.0 + OVERSHOOT_TOL + 1e-9);
    let best = (0..sims.len()).filter(|i| sims[*i].peak <= 21.0 + OVERSHOOT_TOL).map(|i| (sims[i].peak - 21.0).abs()).fold(f64::INFINITY, f64::min);
    assert!(((sim.peak - 21.0).abs() - best).abs() < 1e-12);
    assert!(sims.windows(2).all(|w| w[1].peak >= w[0].peak - 1e-9), "peak grows with the boost length");
    assert!(ctx.plan(GREY, 10).unwrap().0 <= 10);
    assert_eq!(room_ctx(&bundle, 18.0).plan(GREY, TOFF_MAX).unwrap().0, 0);
}

fn config_in(dir: &str, extra: &str) -> Config {
    Config::parse(&format!("[boost]\nmodel_dir = '{dir}'\n{extra}\n[[rooms]]\nname = '{ROOM}'\nnodes = [5]\n[[rooms]]\nname = 'flur'\nnodes = [6]\n")).unwrap()
}

fn config(extra: &str) -> Config {
    config_in(BUNDLES, extra)
}

fn patched_bundles(patch: impl Fn(&mut Value)) -> tempfile::TempDir {
    let tmp = tempfile::tempdir().unwrap();
    let src = bundle_dir();
    let dst = tmp.path().join(src.file_name().unwrap());
    std::fs::create_dir_all(&dst).unwrap();
    for f in std::fs::read_dir(&src).unwrap().flatten() {
        std::fs::copy(f.path(), dst.join(f.file_name())).unwrap();
    }
    let mut doc: Value = serde_json::from_str(&std::fs::read_to_string(src.join("bundle.json")).unwrap()).unwrap();
    patch(&mut doc);
    std::fs::write(dst.join("bundle.json"), doc.to_string()).unwrap();
    tmp
}

fn step_in(ts: i64, setpoint: f64, blocked: Option<&'static str>) -> StepIn<'static> {
    let rooms: &'static [RoomIn] = Box::leak(Box::new([RoomIn { room: ROOM.into(), setpoint, blocked }]));
    StepIn { now_ms: ts, hour: 7.0, outdoor: Some((4.0, Some(2.0))), radiation: Some(0.0), rooms }
}

fn drive(boost: &Boost, from_min: i64, to_min: i64, setpoint: f64, blocked: Option<&'static str>, mut temp: impl FnMut(i64) -> f64) -> Vec<StepOut> {
    let mut outs = Vec::new();
    for m in from_min..to_min {
        let ts = m * 60_000;
        boost.observe(5, PI_HEATING_DEMAND, 80.0, ts);
        boost.observe(5, LOCAL_TEMPERATURE, temp(m), ts);
        outs.push(boost.step(&step_in(ts, setpoint, blocked)));
    }
    outs
}

fn slow(m: i64) -> f64 {
    (19.0 + m as f64 * 0.01).min(20.0)
}

fn rise(m: i64) -> f64 {
    (19.0 + m as f64 * 0.03).min(21.5)
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
    let view = boost.view(ROOM);
    assert_eq!(view["variants"]["grey_box"]["mode"], "shadow");
    assert!(view["variants"]["grey_box"]["planned_t_off"].as_f64().is_some());
    assert!(view["variants"]["residual"]["predicted_peak"].as_f64().is_some());
    assert!(view["variants"]["grey_box"]["air_estimate"].as_f64().is_some() && view["variants"]["residual"]["air_estimate"].as_f64().is_some());
    assert!(cycle_events(&outs).is_empty(), "cycle still running");
    outs.extend(drive(&boost, 120, 125, 21.0, Some("window_open"), slow));
    let rows = cycle_events(&outs);
    assert_eq!(rows.len(), 2);
    for row in &rows {
        assert_eq!((row["mode"].as_str(), row["end"].as_str(), row["room"].as_str()), (Some("shadow"), Some("window_open"), Some(ROOM)));
        assert!(row["applied_t_off_min"].is_null() && row["model_version"] == test_bundle().version.as_str());
        assert!(row["predicted_peak_c"].as_f64().is_some() && row["planned_t_off_min"].as_f64().is_some());
    }
    assert_eq!(rows.iter().map(|r| r["variant"].as_str().unwrap()).collect::<Vec<_>>(), vec!["grey_box", "residual"]);
}

fn closed_loop(boost: &Boost, minutes: i64, target: f64) -> (Vec<StepOut>, f64, Vec<f64>) {
    let bundle = test_bundle();
    let ph = &bundle.rooms[ROOM].phys;
    let env = Env { tout: 4.0, wind: 2.0, rad: 0.0, night: false, nb: BTreeMap::new() };
    let (mut x, mut sp, mut peak, mut outs, mut applied) = ((19.0, 19.0), 18.0, 19.0f64, Vec::new(), Vec::new());
    for m in 0..minutes {
        let desired = if m < 5 { 18.0 } else { target };
        if m == 5 {
            sp = target;
        }
        let (sensor, ts) = (ph.sensor(x), m * 60_000);
        let u = if sp >= BOOST_SP - 0.5 { 1.0 } else { ph.pi(sp, sensor) };
        boost.observe(5, PI_HEATING_DEMAND, u * 100.0, ts);
        boost.observe(5, LOCAL_TEMPERATURE, sensor, ts);
        let out = boost.step(&step_in(ts, desired, None));
        match out.directives.get(ROOM) {
            Some(Directive::Boost { .. }) => sp = BOOST_SP,
            Some(Directive::Switch) => sp = target,
            None => {}
        }
        if let Some(a) = boost.view(ROOM)["cycle"]["applied_t_off"].as_f64() {
            applied.push(a);
        }
        x = ph.step(x, u, &env, 1.0 / 60.0);
        peak = peak.max(x.1);
        outs.push(out);
    }
    (outs, peak, applied)
}

fn force_live(boost: &Boost, variant: &str) {
    boost.0.lock().unwrap().live.insert(ROOM.into(), (variant.into(), "test".into()));
}

fn switches(outs: &[StepOut]) -> Vec<usize> {
    outs.iter().enumerate().filter(|(_, o)| o.directives.get(ROOM) == Some(&Directive::Switch)).map(|(i, _)| i).collect()
}

#[test]
fn live_grey_box_closed_loop_lands_the_air_peak_on_the_target() {
    let boost = Boost::new(&config("explore_min = 0.0"));
    force_live(&boost, GREY);
    let (outs, peak, _) = closed_loop(&boost, 500, 21.0);
    let directives: Vec<&Directive> = outs.iter().flat_map(|o| o.directives.values()).collect();
    assert!(matches!(directives.first(), Some(Directive::Boost { .. })));
    assert_eq!(switches(&outs).len(), 1);
    assert!(peak <= 21.0 + OVERSHOOT_TOL && peak > 20.5, "air peak {peak}");
    let rows = cycle_events(&outs);
    let live = rows.iter().find(|r| r["mode"] == "live").unwrap();
    assert_eq!(live["variant"].as_str(), Some("grey_box"));
    assert!(live["overshoot_c"].as_f64().is_some() && live["valve_over_50_min"].as_f64().unwrap() > 0.0 && live["exploration"] == true);
    assert!(rows.iter().any(|r| r["mode"] == "shadow" && r["variant"] == "residual") && rows.iter().all(|r| r["variant"] != "baseline"));
}

fn boost_with_offset(sign: f64) -> Boost {
    for seed in 0..200 {
        let boost = Boost::new(&config(&format!("explore_min = 10.0\nseed = {seed}")));
        force_live(&boost, GREY);
        drive(&boost, 0, 5, 18.0, None, |_| 19.0);
        drive(&boost, 5, 7, 21.0, None, rise);
        let offset = boost.view(ROOM)["cycle"]["explore_offset_min"].as_f64().unwrap();
        if offset * sign >= 6.0 {
            return Boost::new(&config(&format!("explore_min = 10.0\nseed = {seed}")));
        }
    }
    panic!("no seed with offset of sign {sign}");
}

#[test]
fn live_boost_switches_off_for_positive_and_negative_exploration_offsets() {
    for sign in [1.0, -1.0] {
        let boost = boost_with_offset(sign);
        force_live(&boost, GREY);
        let (outs, _, applied) = closed_loop(&boost, 400, 21.0);
        let offset = cycle_events(&outs).iter().find(|r| r["mode"] == "live").unwrap()["explore_offset_min"].as_f64().unwrap();
        assert!(offset * sign >= 6.0 && offset.abs() <= 10.0, "offset {offset}");
        let at = switches(&outs);
        assert_eq!(at.len(), 1, "offset {offset}: exactly one switch");
        let elapsed = at[0] as f64 - 5.0;
        let last = *applied.last().unwrap();
        assert!(elapsed >= last && elapsed < last + 1.5 && elapsed < 240.0, "offset {offset}: switched at {elapsed}, applied {last}");
        let rows = cycle_events(&outs);
        let live = rows.iter().find(|r| r["mode"] == "live").unwrap();
        assert_ne!(live["end"], "timeout", "offset {offset}");
        assert!((live["applied_t_off_min"].as_f64().unwrap() - last).abs() < 1e-9);
        assert!((live["applied_t_off_min"].as_f64().unwrap() - live["planned_t_off_min"].as_f64().unwrap() - offset).abs() < 1e-6 || live["applied_t_off_min"].as_f64() == Some(0.0));
    }
}

#[test]
fn live_switch_time_is_fixed_once_the_plan_is_reached() {
    let boost = boost_with_offset(1.0);
    force_live(&boost, GREY);
    let (_, _, applied) = closed_loop(&boost, 200, 21.0);
    let last = *applied.last().unwrap();
    assert!(applied.iter().rev().take_while(|a| **a == last).count() >= 10, "applied kept moving: {applied:?}");
}

#[test]
fn live_boost_aborts_on_blockers_and_target_decrease() {
    for (blocked, setpoint, why) in [(Some("window_open"), 21.0, "window_open"), (Some("away"), 21.0, "away"), (Some("manual_hold"), 21.0, "manual_hold"), (None, 19.5, "target_decrease")] {
        let boost = Boost::new(&config("live_rooms_override = ['wohnzimmer']\nseed = 3"));
        drive(&boost, 0, 5, 18.0, None, |_| 19.0);
        let started = drive(&boost, 5, 10, 21.0, None, rise);
        assert!(matches!(started.last().unwrap().directives.get(ROOM), Some(Directive::Boost { .. })));
        let stop = drive(&boost, 10, 11, setpoint, blocked, rise);
        let rows = cycle_events(&stop);
        assert!(rows.iter().all(|r| r["end"] == why) && !rows.is_empty(), "{why}");
        assert!(!matches!(stop[0].directives.get(ROOM), Some(Directive::Boost { .. })), "{why}");
        assert_eq!(stop[0].directives.contains_key(ROOM), why == "target_decrease");
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
    assert!(small.view(ROOM)["cycle"].is_null());
}

fn live_variant(boost: &Boost) -> Option<(String, String)> {
    boost.0.lock().unwrap().live.get(ROOM).cloned()
}

#[test]
fn live_gate_needs_shadow_days_and_out_of_sample_backtest_cycles() {
    let boost = Boost::new(&config(""));
    assert!(live_variant(&boost).is_none());
    boost.set_gate_days(HashMap::from([(ROOM.to_string(), 6)]));
    assert!(live_variant(&boost).is_none());
    boost.set_gate_days(HashMap::from([(ROOM.to_string(), 7)]));
    let live = live_variant(&boost).unwrap();
    assert_eq!(live.0, GREY);
    assert!(live.1.contains("grey_box") && live.1.contains("7 shadow days"));

    let gate = |patch: &dyn Fn(&mut Value)| {
        let dir = patched_bundles(|doc| patch(&mut doc["metrics"]["backtest"][ROOM]));
        let boost = Boost::new(&config_in(dir.path().to_str().unwrap(), ""));
        boost.set_gate_days(HashMap::from([(ROOM.to_string(), 30)]));
        live_variant(&boost)
    };
    assert_eq!(gate(&|r| r["cycles"] = json!(3)).unwrap().0, GREY);
    assert!(gate(&|r| r["cycles"] = json!(2)).is_none());
    assert!(gate(&|r| r["in_sample"] = json!(true)).is_none(), "in-sample backtests do not count");
    let better = gate(&|r| r["residual"] = json!(0.1)).unwrap();
    assert_eq!(better.0, RESIDUAL);
    assert!(better.1.contains("residual 0.100 < grey_box"));
}

#[test]
fn bundles_with_foreign_history_or_step_are_refused() {
    for (key, value) in [("history_min", 120), ("lag_step_min", 10), ("dt_s", 30)] {
        let dir = patched_bundles(|doc| doc[key] = json!(value));
        let err = format!("{:#}", Bundle::load(&dir.path().join(bundle_dir().file_name().unwrap())).err().unwrap());
        assert!(err.contains(key) && err.contains("refused"), "{err}");
        let boost = Boost::new(&config_in(dir.path().to_str().unwrap(), ""));
        assert!(boost.0.lock().unwrap().bundle.is_none());
    }
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
    assert!(newest_bundle_dir(Path::new(BUNDLES)).is_some());
}

#[test]
fn serve_features_use_forward_filled_history_long_valve_history_and_the_others_threshold() {
    let boost = Boost::new(&config(""));
    let flur_valve = |v: f64, ts: i64| boost.observe(6, PI_HEATING_DEMAND, v, ts);
    boost.observe(5, LOCAL_TEMPERATURE, 19.0, 0);
    boost.observe(5, LOCAL_TEMPERATURE, 21.0, 30 * 60_000);
    for m in 0..300i64 {
        boost.observe(5, PI_HEATING_DEMAND, if (100..130).contains(&m) { 60.0 } else { 0.0 }, m * 60_000);
    }
    flur_valve(4.0, 0);
    let ctx_at = |others_valve: Option<f64>| {
        if let Some(v) = others_valve {
            flur_valve(v, 299 * 60_000);
        }
        let inner = boost.0.lock().unwrap();
        let (rs, bundle) = (&inner.rooms[0], inner.bundle.clone().unwrap());
        let ctx = inner.ctx(rs, &bundle, ROOM, (20.0, 20.0), 21.0, 7.0, 299 * 60_000).unwrap();
        (ctx.sensor.clone(), ctx.valve.clone(), ctx.others)
    };
    let (sensor, valve, others) = ctx_at(None);
    assert_eq!((sensor.len(), valve.len(), others), (SENSOR_HISTORY_MIN + 1, VALVE_HISTORY_MIN + 1, 0.0));
    assert!(sensor.iter().all(|s| *s == 21.0));
    assert_eq!(since_boost(&valve), 199.0);
    assert_eq!(ctx_at(Some(6.0)).2, 1.0);
    assert_eq!(ctx_at(Some(5.0)).2, 0.0);
    boost.observe(5, LOCAL_TEMPERATURE, 22.0, 290 * 60_000);
    let inner = boost.0.lock().unwrap();
    let ctx = inner.ctx(&inner.rooms[0], inner.bundle.as_ref().unwrap(), ROOM, (20.0, 20.0), 21.0, 7.0, 299 * 60_000).unwrap();
    assert_eq!((ctx.sensor[60], ctx.sensor[51], ctx.sensor[50]), (22.0, 22.0, 21.0), "causal forward fill, no interpolation");
}
