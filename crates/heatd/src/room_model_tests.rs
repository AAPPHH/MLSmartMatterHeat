use crate::room_model::*;
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
            (name.to_string(), RoomIn { y: at("y"), yv: at("yv"), u: at("u"), sp: at("sp"), tout: at("tout"), wind: at("wind"), rad: at("rad"), night: at("night") > 0.5, ncnt: list("ncnt"), nsum: list("nsum") })
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
    }
}

#[test]
fn filter_and_planner_match_the_python_fixture() {
    let fx = fixture();
    let bundle = Bundle::parse(&fx["bundle"].to_string()).unwrap();
    let mut house = House::new(&bundle);
    let steps = fx["steps"].as_u64().unwrap() as usize;
    let plans = fx["plans"].as_array().unwrap();
    let mut next_plan = 0;
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
            let got = plan(&f.par, &bundle.shock, &f.comps, house.w[1], &env(&p["env"]));
            let want = &p["out"];
            let what = format!("plan {name} at {k}");
            assert_eq!(got.name, want["name"].as_str().unwrap(), "{what}");
            assert_eq!(got.kind, want["kind"].as_str().unwrap(), "{what}");
            assert_eq!(got.raise, want["raise"].as_bool().unwrap(), "{what}");
            assert_eq!(got.probe_min.map(|v| v as f64), want["probe_min"].as_f64(), "{what}");
            assert_eq!(got.boost_min.map(|v| v as f64), want["boost_min"].as_f64(), "{what}");
            assert_eq!(got.n_allowed as f64, num(&want["n_allowed"]), "{what}");
            for (key, v) in [("setpoint", got.setpoint), ("energy", got.energy), ("cold", got.cold), ("raise_cost", got.raise_cost), ("cost_live", got.cost_live), ("total", got.total), ("p_window", got.p_window), ("lam_closed", got.lam_closed),
                             ("lam_open", got.lam_open), ("mass", got.mass), ("air", got.air), ("sensor", got.sensor), ("forecast", got.forecast), ("evoi", got.evoi)] {
                near(v, num(&want[key]), 5e-6, &format!("{what}.{key}"));
            }
        }
    }
    assert_eq!(next_plan, plans.len());
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
    Env { tout: -5.0, wind: 3.0, rad: 0.0, night: false, ncnt: vec![], nsum: vec![], target: 21.0, present: 1.0, sp_cur: Some(21.0), sp_live: 21.0, frost: 7.0, boost_c: 24.0, boost_ok: false, raise_cost: 15.0, cold_cost: 1.0, r_grid: 0.25 / 12.0, lag: None, y_last: None }
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
    let half = plan(&par, &shock, &comps_at(17.0, 16.0, 2.0, 1e-2), 1.0, &preset_env());
    assert!((half.p_window - 0.5).abs() < 1e-6 && half.kind == "probe" && half.probe_min == Some(15) && half.setpoint == 21.0 && !half.raise && half.evoi > 1.0, "{half:?}");
    for lam in [1.0, 6.0] {
        let sure = plan(&par, &shock, &comps_at(17.0, 16.0, lam, 1e-4), 1.0, &preset_env());
        assert!(sure.kind != "probe" && sure.evoi == 0.0, "{sure:?}");
    }
}
