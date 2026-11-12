use crate::room_model::*;
use crate::room_plan::*;
use std::collections::BTreeMap;
use serde_json::Value;

const TOL: f64 = 2e-6;

fn fixture() -> Value {
    serde_json::from_str(&std::fs::read_to_string(concat!(env!("CARGO_MANIFEST_DIR"), "/testdata/room_model_parity.json")).unwrap()).unwrap()
}

fn num(v: &Value) -> f64 {
    v.as_f64().unwrap_or(f64::NAN)
}

fn inputs(fx: &Value, k: usize) -> BTreeMap<String, RoomIn> {
    fx["rooms"]
        .as_array()
        .unwrap()
        .iter()
        .map(|n| {
            let name = n.as_str().unwrap();
            let i = &fx["inputs"][name];
            let at = |key: &str| num(&i[key][k]);
            let list = |key: &str| i[key][k].as_array().unwrap().iter().map(num).collect::<Vec<f64>>();
            (
                name.to_string(),
                RoomIn { y: at("y"), yv: at("yv"), u: at("u"), sp: at("sp"), tout: at("tout"), wind: at("wind"), rad: at("rad"), night: at("night") > 0.5, ncnt: list("ncnt"), nsum: list("nsum"), ya: list("ya"), co2: at("co2"), rh: at("rh"), th: at("th") },
            )
        })
        .collect()
}

fn near(a: f64, b: f64, tol: f64, what: &str) {
    assert!((a - b).abs() <= tol * (1.0 + b.abs().min(1e3)), "{what}: {a} vs {b}");
}

fn env(e: &Value) -> Env {
    let list = |key: &str| e[key].as_array().unwrap().iter().map(num).collect::<Vec<f64>>();
    Env {
        tout: num(&e["tout"]),
        wind: num(&e["wind"]),
        rad: num(&e["rad"]),
        night: e["night"].as_bool().unwrap(),
        ncnt: list("ncnt"),
        nsum: list("nsum"),
        target: num(&e["target"]),
        present: num(&e["present"]),
        sp_cur: e["sp_cur"].as_f64(),
        sp_live: num(&e["sp_live"]),
        frost: num(&e["frost"]),
        boost_c: num(&e["boost_c"]),
        boost_ok: e["boost_ok"].as_bool().unwrap(),
        raise_cost: num(&e["raise_cost"]),
        cold_cost: num(&e["cold_cost"]),
        r_grid: num(&e["r_grid"]),
        lag: e["lag"].as_array().map(|l| [num(&l[0]), num(&l[1])]),
        y_last: e["y_last"].as_f64(),
        u_now: e["u_now"].as_f64(),
        now_min: e["now_min"].as_i64().unwrap_or(0),
        q_net: e["q_net"].as_f64(),
        pcnn_active: e["pcnn_active"].as_bool().unwrap_or(false),
        probe: e["probe"].as_object().map(|m| ProbeMemo { start: m["start"].as_i64().unwrap(), d: m["d"].as_u64().unwrap() as usize, base: m["base"].as_i64().unwrap(), block_until: m["block_until"].as_i64().unwrap(), target: num(&m["target"]), present: num(&m["present"]), ended: m["ended"].as_bool().unwrap(), unc: num(&m["unc"]), strikes: m["strikes"].as_i64().unwrap() }),
    }
}

fn memo_json(m: &Option<ProbeMemo>) -> Value {
    m.as_ref().map_or(Value::Null, |m| serde_json::json!({"start": m.start, "d": m.d, "base": m.base, "block_until": m.block_until, "target": m.target, "present": m.present, "ended": m.ended, "unc": m.unc, "strikes": m.strikes}))
}

fn compare_plan(got: &PlanOut, want: &Value, what: &str) {
    assert_eq!(got.name, want["name"].as_str().unwrap(), "{what}");
    assert_eq!(got.kind, want["kind"].as_str().unwrap(), "{what}");
    assert_eq!(got.raise, want["raise"].as_bool().unwrap(), "{what}");
    assert_eq!(got.unusable, want["unusable"].as_bool().unwrap(), "{what}");
    assert_eq!(got.probe_min.map(|v| v as f64), want["probe_min"].as_f64(), "{what}");
    assert_eq!(got.boost_min.map(|v| v as f64), want["boost_min"].as_f64(), "{what}");
    assert_eq!(got.probe_event, want["probe_event"].as_str(), "{what}.probe_event");
    assert_eq!(got.probed_min as f64, num(&want["probed_min"]), "{what}.probed_min");
    assert_eq!(memo_json(&got.probe_memo), want["probe_memo"], "{what}.probe_memo");
    assert_eq!(got.n_allowed as f64, num(&want["n_allowed"]), "{what}");
    near(got.coast_min, num(&want["coast_min"]), 5e-6, &format!("{what}.coast_min"));
    match (got.stored_min, want["stored_min"].as_f64()) {
        (Some(g), Some(w)) => near(g, w, 5e-6, &format!("{what}.stored_min")),
        (g, w) => assert_eq!(g, w, "{what}.stored_min"),
    }
    assert_eq!(got.pcnn_active, want["pcnn_active"].as_bool().unwrap(), "{what}.pcnn_active");
    assert_eq!(got.forecast_pcnn.is_nan(), want["forecast_pcnn"].is_null(), "{what}.forecast_pcnn");
    if !got.forecast_pcnn.is_nan() {
        near(got.forecast_pcnn, num(&want["forecast_pcnn"]), 5e-6, &format!("{what}.forecast_pcnn"));
    }
    for (key, v) in [("setpoint", got.setpoint), ("energy", got.energy), ("cold", got.cold), ("raise_cost", got.raise_cost), ("cost_live", got.cost_live), ("total", got.total), ("p_window", got.p_window), ("lam_closed", got.lam_closed),
                     ("lam_open", got.lam_open), ("mass", got.mass), ("air", got.air), ("sensor", got.sensor), ("forecast", got.forecast), ("forecast_phys", got.forecast_phys), ("evoi", got.evoi)] {
        near(v, num(&want[key]), 5e-6, &format!("{what}.{key}"));
    }
}

#[test]
fn filter_and_planner_match_the_python_fixture() {
    let fx = fixture();
    let bundle = Bundle::parse(&fx["bundle"].to_string()).unwrap();
    let mut house = House::new(&bundle);
    let steps = fx["steps"].as_u64().unwrap() as usize;
    let plans = fx["plans"].as_array().unwrap();
    let (mut next_plan, mut nets) = (0, 0);
    for k in 0..steps {
        let outs = house.step(&inputs(&fx, k));
        for (name, out) in &outs {
            let e = &fx["expected"][name];
            assert_eq!(out.ok, e["ok"][k].as_bool().unwrap(), "{name} ok at {k}");
            if out.ok {
                for (key, got) in [("m", out.m), ("a", out.a), ("lam", out.lam), ("lam_std", out.lam_std), ("p_window", out.p_window), ("p_supply", out.p_supply)] {
                    near(got, num(&e[key][k]), TOL, &format!("{name}.{key} at {k}"));
                }
            }
        }
        while next_plan < plans.len() && plans[next_plan]["step"].as_u64().unwrap() as usize == k {
            let p = &plans[next_plan];
            next_plan += 1;
            let name = p["room"].as_str().unwrap();
            let f = &house.rooms[name];
            let feat = &p["feat"];
            let q = f.pcnn_q(&inputs(&fx, k)[name], num(&feat["hour"]), feat["weekend"].as_bool().unwrap(), num(&feat["present"]), num(&feat["coffee"]));
            assert_eq!(q.is_some(), p["env"]["q_net"].is_number(), "net presence {name} at {k}");
            if let Some(q) = q {
                near(q, num(&p["env"]["q_net"]), 1e-6, &format!("q_net {name} at {k}"));
                nets += 1;
            }
            let got = plan(&f.par, &bundle.shock, &f.comps, house.w[1], &Env { q_net: q, ..env(&p["env"]) });
            compare_plan(&got, &p["out"], &format!("plan {name} at {k}"));
        }
    }
    assert_eq!(next_plan, plans.len());
    assert!(nets > 100 && plans.iter().any(|p| p["out"]["pcnn_active"] == true) && plans.iter().any(|p| p["out"]["pcnn_active"] == false && p["env"]["q_net"].is_number()));
    let cases = fx["plan_cases"].as_array().unwrap();
    assert!(cases.iter().any(|c| c["out"]["probe_event"] == "start") && cases.iter().any(|c| c["out"]["probe_event"] == "end") && cases.iter().any(|c| c["out"]["probe_event"] == "abort") && cases.iter().any(|c| c["out"]["unusable"] == true));
    for (i, c) in cases.iter().enumerate() {
        let name = c["room"].as_str().unwrap();
        let comps: Vec<Comp> = c["comps"].as_array().unwrap().iter().map(|x| {
            let list = |key: &str| x[key].as_array().unwrap().iter().map(num).collect::<Vec<f64>>();
            let (xs, ps) = (list("x"), list("p"));
            Comp { mu: num(&x["mu"]), x: [xs[0], xs[1], xs[2]], p: ps.try_into().unwrap() }
        }).collect();
        let got = plan(&house.rooms[name].par, &bundle.shock, &comps, num(&c["p_supply"]), &env(&c["env"]));
        compare_plan(&got, &c["out"], &format!("case {i} {}", c["script"]));
    }
    assert!(cases.iter().any(|c| c["out"]["pcnn_active"] == true && c["out"]["forecast_pcnn"].is_number()));
    let want: Vec<(String, String, u64)> = fx["resets"].as_array().unwrap().iter().map(|r| (r["room"].as_str().unwrap().to_string(), r["cause"].as_str().unwrap().to_string(), r["step"].as_u64().unwrap())).collect();
    let mut got: Vec<(String, String, u64)> = house.resets.iter().map(|r| (r.room.clone(), r.cause.to_string(), r.step)).collect();
    let mut want_sorted = want;
    want_sorted.sort();
    got.sort();
    assert_eq!(got, want_sorted);
    assert!(!want_sorted.is_empty());
}

fn base_bundle() -> Value {
    fixture()["bundle"].clone()
}

#[test]
fn loader_rejects_bad_bundles() {
    let mut b = base_bundle();
    assert!(Bundle::parse(&b.to_string()).is_ok());
    b["kind"] = "window_room_channels".into();
    assert!(Bundle::parse(&b.to_string()).is_err());
    for (path, bad) in [("shock/p_jump", 1.5), ("shock/p_jump", 0.0), ("house/p_up", 1.0), ("house/p_supply0", -0.1), ("shock/lambda_w", 1.0), ("shock/lambda_jump", 0.9), ("shock/tau_l", 0.0), ("shock/q_calm", -1.0)] {
        let mut b = base_bundle();
        *b.pointer_mut(&format!("/{path}")).unwrap() = bad.into();
        assert!(Bundle::parse(&b.to_string()).is_err(), "{path}={bad}");
    }
    let mut b = base_bundle();
    b["rooms"]["kueche"]["params"]["a1"] = Value::Null;
    assert!(Bundle::parse(&b.to_string()).is_err());
    let mut b = base_bundle();
    b["rooms"]["kueche"]["params"]["w"] = 1.5.into();
    assert!(Bundle::parse(&b.to_string()).is_err());
    for (path, bad) in [
        ("house/trv_grid", Value::from(-0.5)),
        ("house/trv_floor", Value::Null),
        ("rooms/kueche/air", serde_json::json!([{"node": 11, "noise": 0.0}])),
        ("rooms/kueche/effort", serde_json::json!({"sigma": -1.0, "steady_u": 0.05})),
        ("rooms/kueche/effort", serde_json::json!({"sigma": 0.1})),
        ("rooms/schlafzimmer/co2/k", Value::from(-0.1)),
        ("rooms/schlafzimmer/co2/win_min", Value::from(0.0)),
        ("rooms/schlafzimmer/co2/win_min", Value::from(2.5)),
        ("rooms/schlafzimmer/co2/out", Value::Null),
        ("rooms/schlafzimmer/hum/rh_out", Value::from(120.0)),
        ("rooms/schlafzimmer/hum/sigma_up", Value::Null),
    ] {
        let mut b = base_bundle();
        *b.pointer_mut(&format!("/{path}")).unwrap() = bad.clone();
        assert!(Bundle::parse(&b.to_string()).is_err(), "{path}={bad}");
    }
    let mut b = base_bundle();
    for room in b["rooms"].as_object_mut().unwrap().values_mut() {
        for key in ["air", "effort", "co2", "hum"] {
            room.as_object_mut().unwrap().remove(key);
        }
    }
    b["house"].as_object_mut().unwrap().remove("trv_grid");
    assert!(Bundle::parse(&b.to_string()).is_ok(), "a bundle without observation blocks still loads");
    let mut b = base_bundle();
    b["rooms"] = serde_json::json!({});
    assert!(Bundle::parse(&b.to_string()).is_err());
}

#[test]
fn newest_valid_bundle_wins_and_broken_ones_are_skipped() {
    let dir = std::env::temp_dir().join(format!("room_model_{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    for (name, text) in [("20261101T000000Z", base_bundle().to_string()), ("20261102T000000Z", "{".to_string()), ("not-a-timestamp", base_bundle().to_string())] {
        std::fs::create_dir_all(dir.join(name)).unwrap();
        std::fs::write(dir.join(name).join("bundle.json"), text).unwrap();
    }
    let loaded = try_load(dir.to_str().unwrap()).unwrap();
    assert_eq!(loaded.version, "20261101T000000Z");
    assert!(try_load(dir.join("missing").to_str().unwrap()).is_none());
    std::fs::remove_dir_all(&dir).unwrap();
}

#[test]
fn observation_primitives_match_the_python_checks() {
    use crate::room_obs::{abs_hum, bin_prob, trv_update};
    let fx = fixture();
    let c = &fx["checks"];
    for b in c["bin"].as_array().unwrap() {
        near(bin_prob(num(&b[0]), num(&b[1])), num(&b[2]), 1e-12, "bin_prob");
    }
    for h in c["abs_hum"].as_array().unwrap() {
        near(abs_hum(num(&h[0]), num(&h[1])), num(&h[2]), 1e-12, "abs_hum");
    }
    for t in c["trv"].as_array().unwrap() {
        let (yhat, p0) = (num(&t["yhat"]), num(&t["p0"]));
        let (mut x, mut p) = ([yhat, yhat, 1.0], [p0, 0.01, 0.0, 0.01, p0, 0.0, 0.0, 0.0, 0.02]);
        let noise = 0.15f64.powi(2);
        let (ll, s) = trv_update(&mut x, &mut p, [0.3, 0.7, 0.0], num(&t["y"]), yhat, noise, 0.5, 0.25, 0.5f64.powi(2) / 12.0 + noise);
        near(ll, num(&t["ll"]), 1e-9, "trv ll");
        near(s, num(&t["s"]), 1e-9, "trv s");
        for i in 0..3 {
            near(x[i], num(&t["x"][i]), 1e-9, "trv x");
        }
        for i in 0..9 {
            near(p[i], num(&t["p"][i]), 1e-9, "trv p");
        }
    }
}

#[test]
fn repeated_identical_readings_keep_the_sensor_variance_on_the_grid_scale() {
    use crate::room_obs::{obs_update, trv_update};
    let (w, noise) = (0.3, 0.15f64.powi(2));
    let sd = |p: &[f64; 9]| (w * w * p[0] + 2.0 * w * (1.0 - w) * p[1] + (1.0 - w).powi(2) * p[4]).sqrt();
    let (mut x, mut p) = ([20.0, 20.0, 1.0], [1.0, 0.0, 0.0, 0.0, 1.0, 0.0, 0.0, 0.0, 0.01]);
    let (mut xg, mut pg) = (x, p);
    let mut ll = 0.0;
    for _ in 0..60 {
        let yhat = w * x[0] + (1.0 - w) * x[1];
        ll = trv_update(&mut x, &mut p, [w, 1.0 - w, 0.0], 20.0, yhat, noise, 0.5, 0.25, 0.25 / 12.0 + noise).0;
        let innovation = 20.0 - (w * xg[0] + (1.0 - w) * xg[1]);
        obs_update(&mut xg, &mut pg, [w, 1.0 - w, 0.0], innovation, 0.25 / 12.0 + noise);
    }
    let floor = (0.25f64 * 0.25 / 12.0).sqrt();
    assert!(sd(&p) > 0.95 * floor && sd(&p) < 1.3 * floor && sd(&pg) < 0.5 * sd(&p) && ll > -0.2 && ll <= 0.0, "{} {} {ll}", sd(&p), sd(&pg));
}

#[test]
fn nan_state_resets_with_a_record_and_never_propagates() {
    let fx = fixture();
    let bundle = Bundle::parse(&fx["bundle"].to_string()).unwrap();
    let mut house = House::new(&bundle);
    for k in 0..300 {
        for (name, out) in house.step(&inputs(&fx, k)) {
            if out.ok {
                assert!([out.m, out.a, out.lam, out.lam_std, out.p_window, out.p_supply].iter().all(|v| v.is_finite()), "{name} at {k}");
            }
        }
    }
    assert!(house.resets.is_empty());
    house.rooms.get_mut("kueche").unwrap().corrupt_for_test();
    let outs = house.step(&inputs(&fx, 300));
    assert!(house.resets.iter().any(|r| r.room == "kueche" && r.cause == "nan"));
    assert!(!house.resets.is_empty());
    for k in 301..360 {
        for out in house.step(&inputs(&fx, k)).values().filter(|o| o.ok) {
            assert!([out.m, out.a, out.lam].iter().all(|v| v.is_finite()));
        }
    }
    drop(outs);
}

#[test]
fn erf_matches_reference_values() {
    for (x, want) in [(0.0, 0.0), (0.5, 0.5204998778), (1.0, 0.8427007929), (-1.5, -0.9661051465), (3.0, 0.9999779095)] {
        assert!((erf_as(x) - want).abs() < 2e-7, "{x}");
    }
    assert!((norm_cdf(0.0) - 0.5).abs() < 1e-9);
}

pub(crate) fn preset_env() -> Env {
    Env { tout: -5.0, wind: 3.0, rad: 0.0, night: false, ncnt: vec![], nsum: vec![], target: 21.0, present: 1.0, sp_cur: Some(21.0), sp_live: 21.0, frost: 7.0, boost_c: 24.0, boost_ok: false, raise_cost: 15.0, cold_cost: 1.0, r_grid: 0.25 / 12.0, lag: None, y_last: None, u_now: None, now_min: 0, probe: None, q_net: None, pcnn_active: false }
}

pub(crate) fn comps_at(m: f64, a: f64, lam: f64, p: f64) -> Vec<Comp> {
    (0..4).map(|_| Comp { mu: 0.25, x: [m, a, lam], p: [0.01, 0.0, 0.0, 0.0, 0.01, 0.0, 0.0, 0.0, p] }).collect()
}

pub(crate) fn preset_rig(lambda_jump: f64) -> (RoomPar, Shock) {
    let bundle = Bundle::parse(&base_bundle().to_string()).unwrap();
    (RoomPar::new(&bundle.rooms["kueche"]), Shock { lambda_jump, ..bundle.shock })
}

#[test]
fn preset_costs_drop_to_frost_for_an_open_window_and_heat_a_closed_cold_room() {
    for lam in [3.0, 6.0] {
        let (par, shock) = preset_rig(lam);
        let open = plan(&par, &shock, &comps_at(17.0, 16.0, lam, 1e-4), 1.0, &preset_env());
        assert!(open.p_window > 0.99 && open.kind == "frost" && open.setpoint == 7.0 && !open.raise, "{lam} {open:?}");
        assert!(open.cold < 1e-6 && open.total < open.cost_live);
    }
    let (par, shock) = preset_rig(3.0);
    let cold = plan(&par, &shock, &comps_at(17.0, 16.0, 1.0, 1e-4), 1.0, &preset_env());
    assert!(cold.p_window < 0.01 && cold.kind != "frost" && cold.setpoint >= 21.0, "{cold:?}");
    let warm = plan(&par, &shock, &comps_at(23.0, 22.5, 1.0, 1e-4), 1.0, &preset_env());
    assert!(warm.p_window < 0.01 && !warm.raise && warm.setpoint <= 21.0, "{warm:?}");
}

#[test]
fn half_window_probability_probes_and_certainty_does_not() {
    let (par, shock) = preset_rig(6.0);
    let half = plan(&par, &shock, &comps_at(21.0, 20.5, 2.0, 1e-2), 1.0, &preset_env());
    assert!((half.p_window - 0.5).abs() < 1e-6 && half.kind == "probe" && matches!(half.probe_min, Some(10 | 20 | 30)) && half.setpoint == 7.0 && !half.raise && half.raise_cost == 0.0 && half.evoi > 15.0 && half.probe_event == Some("start"), "{half:?}");
    let cold = plan(&par, &shock, &comps_at(17.0, 16.0, 2.0, 1e-2), 1.0, &preset_env());
    assert!(cold.kind != "probe" && cold.setpoint > 7.0, "{cold:?}");
    for lam in [1.0, 6.0] {
        let sure = plan(&par, &shock, &comps_at(21.0, 20.5, lam, 1e-4), 1.0, &preset_env());
        assert!(sure.kind != "probe" && sure.evoi == 0.0, "{sure:?}");
    }
}

#[test]
fn close_probe_is_a_decrease_without_raise_penalty() {
    let (par, shock) = preset_rig(6.0);
    for cur in [21.0, 18.0] {
        let env = Env { sp_cur: Some(cur), sp_live: cur, ..preset_env() };
        let p = plan(&par, &shock, &comps_at(21.0, 20.5, 2.0, 1e-2), 1.0, &env);
        assert!(p.kind == "probe" && p.raise_cost == 0.0 && !p.raise && p.setpoint == 7.0, "{cur} {p:?}");
    }
}

#[test]
fn storage_coast_and_stored_heat() {
    let (par, shock) = preset_rig(6.0);
    let at = |m: f64, a: f64, env: &Env| plan(&par, &shock, &comps_at(m, a, 1.0, 1e-4), 1.0, env);
    let cold = at(17.0, 17.0, &preset_env());
    assert!(cold.coast_min == 0.0 && cold.stored_min.unwrap() < 0.0, "{cold:?}");
    let warm = at(30.0, 22.0, &preset_env());
    let hot = at(40.0, 25.0, &preset_env());
    assert!(warm.coast_min > 0.0 && warm.stored_min.unwrap() > 0.0, "{warm:?}");
    assert!(hot.coast_min > warm.coast_min && hot.stored_min.unwrap() > warm.stored_min.unwrap());
    let mild = at(23.0, 21.0, &Env { tout: 21.0, wind: 0.0, ..preset_env() });
    assert_eq!(mild.coast_min, crate::room_plan::COAST_CAP as f64, "{mild:?}");
}

fn run_probe(steps: &[(i64, f64, Env)], comps: &[Vec<Comp>]) -> Vec<PlanOut> {
    let (par, shock) = preset_rig(6.0);
    let mut memo = None;
    steps.iter().zip(comps).map(|((t, _, env), c)| {
        let p = plan(&par, &shock, c, 1.0, &Env { now_min: *t, probe: memo.clone(), ..env.clone() });
        memo = p.probe_memo.clone();
        p
    }).collect()
}

#[test]
fn probe_runs_to_its_end_is_reported_once_and_is_not_repeated_immediately() {
    let half = comps_at(21.0, 20.5, 2.0, 1e-2);
    let steps: Vec<(i64, f64, Env)> = (0..13).map(|i| (i * 5, 21.0, preset_env())).collect();
    let seq = run_probe(&steps, &vec![half; 13]);
    assert_eq!((seq[0].kind, seq[0].probe_event, seq[0].probe_min), ("probe", Some("start"), Some(10)));
    assert_eq!((seq[1].kind, seq[1].probe_event, seq[1].probe_min), ("probe", None, Some(5)));
    assert!(seq[2].kind != "probe" && seq[2].probe_event == Some("end") && seq[2].probed_min == 10);
    assert!(seq[3].kind != "probe");
    let starts: Vec<usize> = (0..seq.len()).filter(|&i| seq[i].probe_event == Some("start")).collect();
    assert!(starts.len() >= 2 && starts[1] - starts[0] >= 4, "{starts:?}");
}

#[test]
fn probe_stops_when_target_changes_or_the_window_resolves_or_the_fit_is_unusable() {
    let half = comps_at(21.0, 20.5, 2.0, 1e-2);
    let sure = comps_at(21.0, 20.5, 1.0, 1e-4);
    let base = preset_env();
    for (env, comps) in [(Env { target: 19.0, ..base.clone() }, half.clone()), (base.clone(), sure), (Env { target: 40.0, ..base.clone() }, half.clone()), (Env { present: 0.0, ..base.clone() }, half.clone())] {
        let steps = [(0, 0.0, base.clone()), (5, 0.0, env.clone()), (10, 0.0, env)];
        let seq = run_probe(&steps, &[half.clone(), comps.clone(), comps]);
        assert_eq!(seq[0].probe_event, Some("start"));
        assert!(seq[1].probe_event == Some("abort") && seq[1].probed_min == 5 && seq[1].kind != "probe" && seq[2].probe_event.is_none(), "{:?}", seq[1]);
    }
}

#[test]
fn terminal_cost_keeps_an_occupied_cold_room_off_frost_and_an_empty_one_on_frost() {
    let (par, shock) = preset_rig(6.0);
    let cold = comps_at(17.0, 16.0, 1.0, 1e-4);
    let full = plan(&par, &shock, &cold, 1.0, &preset_env());
    let empty = plan(&par, &shock, &cold, 1.0, &Env { present: 0.0, ..preset_env() });
    assert!(full.kind != "frost" && full.setpoint > 7.0, "{full:?}");
    assert!(empty.kind == "frost" && empty.cold == 0.0, "{empty:?}");
    let none = plan(&par, &shock, &cold, 0.0, &preset_env());
    assert!(none.total.is_finite() && none.cold.is_finite());
}

#[test]
fn fit_is_unusable_when_the_supply_cannot_reach_the_target_or_the_bundle_says_so() {
    let (par, shock) = preset_rig(3.0);
    let high = plan(&par, &shock, &comps_at(17.0, 16.0, 1.0, 1e-4), 1.0, &Env { target: 40.0, sp_live: 40.0, sp_cur: Some(40.0), ..preset_env() });
    assert!(high.kind == "fit_unusable" && high.unusable && high.setpoint == 40.0 && !high.raise && high.probe_min.is_none(), "{high:?}");
    let mut doc = base_bundle();
    doc["rooms"]["kueche"]["unusable"] = true.into();
    let bundle = Bundle::parse(&doc.to_string()).unwrap();
    let flagged = plan(&RoomPar::new(&bundle.rooms["kueche"]), &bundle.shock, &comps_at(17.0, 16.0, 1.0, 1e-4), 1.0, &preset_env());
    assert!(flagged.kind == "fit_unusable", "{flagged:?}");
}

#[test]
fn the_net_matches_the_python_reference_and_non_finite_inputs_give_zero() {
    let fx = fixture();
    let bundle = Bundle::parse(&fx["bundle"].to_string()).unwrap();
    let net = bundle.rooms["kueche"].pcnn.clone().unwrap();
    let mut finite = 0;
    for (i, c) in fx["net_cases"].as_array().unwrap().iter().enumerate() {
        let r = &c["raw"];
        let row = crate::room_obs::feature_row(num(&r["hour"]), r["weekend"].as_bool().unwrap(), num(&r["rad"]), num(&r["tout"]), num(&r["present"]), num(&r["co2_rate"]), num(&r["hum_rate"]), num(&r["coffee"]));
        for (j, want) in c["row"].as_array().unwrap().iter().enumerate() {
            if want.is_null() {
                assert!(row[j].is_nan(), "case {i} feature {j}");
            } else {
                near(row[j], num(want), 1e-12, &format!("case {i} feature {j}"));
            }
        }
        let q = net.q(&row);
        near(q, num(&c["q"]), 1e-12, &format!("case {i} q"));
        assert!(q.abs() <= net.qmax);
        finite += usize::from(row.iter().all(|v| v.is_finite()) && q != 0.0);
        if !row.iter().all(|v| v.is_finite()) {
            assert_eq!(q, 0.0, "case {i}: a missing required input switches the net off");
        }
    }
    assert!(finite > 20);
    let mut row = crate::room_obs::feature_row(12.0, false, 100.0, 5.0, 1.0, f64::NAN, f64::NAN, 0.0);
    assert_eq!((row[6], row[7], row[8], row[9]), (0.0, 0.0, 0.0, 0.0));
    assert_ne!(net.q(&row), 0.0);
    for bad in [f64::NAN, f64::INFINITY, f64::NEG_INFINITY] {
        for i in 0..row.len() {
            let keep = row[i];
            row[i] = bad;
            assert_eq!(net.q(&row), 0.0);
            row[i] = keep;
        }
    }
}

#[test]
fn the_loader_validates_the_net_block_and_accepts_bundles_without_one() {
    let mut b = base_bundle();
    assert!(Bundle::parse(&b.to_string()).is_ok());
    for (path, bad) in [
        ("rooms/kueche/pcnn/qmax", Value::from(-1.0)),
        ("rooms/kueche/pcnn/qmax", Value::Null),
        ("rooms/kueche/pcnn/sd/0", Value::from(0.0)),
        ("rooms/kueche/pcnn/w2/1", Value::Null),
        ("rooms/kueche/pcnn/b2", Value::Null),
        ("rooms/kueche/pcnn/w1/2", serde_json::json!([1.0, 2.0])),
        ("rooms/kueche/pcnn/mu", serde_json::json!([0.0])),
        ("rooms/kueche/pcnn/features", serde_json::json!(["sin_hour"])),
        ("pcnn/features", serde_json::json!(["x"])),
        ("pcnn/coffee_on_mw", Value::from(-1.0)),
    ] {
        let mut b = base_bundle();
        *b.pointer_mut(&format!("/{path}")).unwrap() = bad.clone();
        assert!(Bundle::parse(&b.to_string()).is_err(), "{path}={bad}");
    }
    let mut swapped = base_bundle();
    swapped["rooms"]["kueche"]["pcnn"]["features"].as_array_mut().unwrap().swap(0, 1);
    assert!(Bundle::parse(&swapped.to_string()).is_err());
    for room in b["rooms"].as_object_mut().unwrap().values_mut() {
        room.as_object_mut().unwrap().remove("pcnn");
    }
    b.as_object_mut().unwrap().remove("pcnn");
    let plain = Bundle::parse(&b.to_string()).unwrap();
    assert!(plain.pcnn.is_none() && plain.rooms.values().all(|r| r.pcnn.is_none()));
    let (house, mut none) = (House::new(&plain), None);
    for f in house.rooms.values() {
        none = f.pcnn_q(&RoomIn::blank(1), 12.0, false, 1.0, 0.0);
    }
    assert!(none.is_none());
}
