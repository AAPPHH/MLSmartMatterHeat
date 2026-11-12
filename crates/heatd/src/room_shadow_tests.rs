use crate::capture::tests::{lake_rows, state_with, string_column};
use crate::room_model::Bundle;
use crate::rules::control_step;
use chrono::NaiveDate;
use matter_controller::MockController;
use serde_json::Value;
use std::sync::Arc;

const T0: i64 = 1_700_000_000_000;
const MIN: i64 = 60_000;
const ROOMS: &str = "[[rooms]]\nname = 'b'\nnodes = [6]\n[[rooms]]\nname = 'c'\nnodes = [7]\n";

fn bundle_text() -> String {
    let fx: Value = serde_json::from_str(&std::fs::read_to_string(concat!(env!("CARGO_MANIFEST_DIR"), "/testdata/room_model_parity.json")).unwrap()).unwrap();
    fx["bundle"].to_string().replace("wohnzimmer", "a").replace("kueche", "b").replace("schlafzimmer", "c")
}

fn rig(dir: &std::path::Path, enabled: bool) -> (crate::http::AppState, Arc<MockController>) {
    let mock = Arc::new(MockController::new(1));
    [5, 6, 7].iter().for_each(|n| mock.add_node(*n, 19.0, 19.0));
    let state = state_with(dir, mock.clone(), &format!("{ROOMS}[room_model]\nenabled = {enabled}\nraise_cost_min = 15.0\ncold_cost_min = 1.0\n"));
    state.status.outdoor_obs(crate::lake::now_us(), 4.0, Some(3.0));
    state.room_model.set_bundle(Bundle::parse(&bundle_text()).unwrap());
    (state, mock)
}

async fn run(state: &crate::http::AppState, minutes: i64, cooling_from: i64) {
    for minute in 0..minutes {
        let t = T0 + minute * MIN;
        let drop = (minute - cooling_from).max(0) as f64 * 0.08;
        for (node, base) in [(5u64, 20.6), (6, 20.2), (7, 20.4)] {
            let temp = if node == 6 { base - drop } else { base };
            state.room_model.observe(node, crate::window::LOCAL_TEMPERATURE, temp, t);
            state.room_model.observe(node, crate::window::PI_HEATING_DEMAND, if node == 6 && drop > 0.0 { 80.0 } else { 20.0 }, t);
            state.room_model.observe(node, crate::window::OCCUPIED_SETPOINT, 21.0, t);
            state.latest.update(node, crate::window::LOCAL_TEMPERATURE, temp, t);
        }
        let now = NaiveDate::from_ymd_opt(2025, 10, 27).unwrap().and_hms_opt(12, 0, 0).unwrap() + chrono::Duration::minutes(minute);
        control_step(state, now, t).await.unwrap();
    }
}

#[tokio::test]
async fn shadow_never_writes_and_logs_a_sentence_per_room() {
    let (a, b) = (tempfile::tempdir().unwrap(), tempfile::tempdir().unwrap());
    let (on, mock_on) = rig(a.path(), true);
    let (off, mock_off) = rig(b.path(), false);
    run(&on, 150, 60).await;
    run(&off, 150, 60).await;
    on.sink.flush().unwrap();
    off.sink.flush().unwrap();
    assert!(!mock_on.setpoint_writes().is_empty());
    assert_eq!(mock_on.setpoint_writes(), mock_off.setpoint_writes());
    assert_eq!(mock_on.limit_writes(), mock_off.limit_writes());
    assert_eq!(lake_rows(a.path(), "decisions"), lake_rows(b.path(), "decisions"));
    assert!(lake_rows(a.path(), "shadow_decisions") >= 3);
    assert_eq!(lake_rows(b.path(), "shadow_decisions"), 0);
    let why = string_column(a.path(), "shadow_decisions", "why");
    assert!(["a: ", "b: ", "c: "].iter().all(|r| why.iter().any(|w| w.starts_with(r))), "{why:?}");
    let view = on.room_model.view("b");
    assert!(view["why"].as_str().unwrap().starts_with("b: ") && view["loss_ratio"].is_number() && view["p_supply"].is_number() && view["setpoint_shadow"].is_number(), "{view}");
    assert!(view["storage"].as_str().unwrap().starts_with("b: Wände ") && view["coast_min"].is_number(), "{view}");
    let storage = string_column(a.path(), "shadow_decisions", "storage");
    assert!(storage.len() >= 3 && storage.iter().all(|w| w.contains("Wände") && w.contains("ohne Heizen")), "{storage:?}");
    assert!(off.room_model.view("b").is_null());
}

const DEVICES: &str = "[[devices]]
node = 11
kind = 'climate'
role = 'sink'
room = 'b'
[[devices]]
node = 12
kind = 'air'
role = 'air'
room = 'c'
[[devices]]
node = 13
kind = 'climate'
role = 'sink'
room = 'a'
";

fn device_rig(dir: &std::path::Path, bundle: String) -> crate::http::AppState {
    let mock = Arc::new(MockController::new(1));
    [5, 6, 7].iter().for_each(|n| mock.add_node(*n, 19.0, 19.0));
    let state = state_with(dir, mock, &format!("{ROOMS}{DEVICES}[room_model]
enabled = true
"));
    state.status.outdoor_obs(crate::lake::now_us(), 4.0, Some(3.0));
    state.room_model.set_bundle(Bundle::parse(&bundle).unwrap());
    state
}

fn aligned() -> i64 {
    T0 - T0.rem_euclid(5 * MIN)
}

fn device_run(state: &crate::http::AppState, minutes: i64, feed: bool) {
    for minute in 0..minutes {
        let t = aligned() + minute * MIN;
        let drop = (minute - 60).max(0) as f64;
        for (node, temp) in [(5u64, 20.6), (6, 20.2), (7, 20.4 - 0.05 * drop)] {
            state.room_model.observe(node, crate::window::LOCAL_TEMPERATURE, temp, t);
            state.room_model.observe(node, crate::window::PI_HEATING_DEMAND, if node == 7 && drop > 0.0 { 60.0 } else { 20.0 }, t);
            state.room_model.observe(node, crate::window::OCCUPIED_SETPOINT, 21.0, t);
        }
        if feed {
            state.room_model.observe_device(11, 0x0402, 0, 2060.0, t);
            state.room_model.observe_device(13, 0x0402, 0, 2060.0, t);
            state.room_model.observe_device(12, 0x040D, 0, (1400.0 - 30.0 * drop).max(500.0), t);
            state.room_model.observe_device(12, 0x0402, 0, 2040.0 - 5.0 * drop, t);
            state.room_model.observe_device(12, 0x0405, 0, 4500.0, t);
        }
        state.room_model.step(&state.config, t, [4.0, 3.0, 0.0], 12.0, false, &[]);
    }
}

#[test]
fn device_readings_reach_the_air_and_gas_evidence_of_their_own_room_only() {
    let (a, b) = (tempfile::tempdir().unwrap(), tempfile::tempdir().unwrap());
    let fed = device_rig(a.path(), bundle_text());
    let plain = device_rig(b.path(), bundle_text());
    device_run(&fed, 120, true);
    device_run(&plain, 120, false);
    let air = |s: &crate::http::AppState, room: &str| {
        s.room_model.step(&s.config, aligned() + 120 * MIN, [4.0, 3.0, 0.0], 12.0, false, &[crate::room_shadow::RoomCtx { room: room.into(), live: 21.0, target: 21.0, present: 1.0, here: false, sp_cur: Some(21.0), boost_ok: false, window_live: false, resid_live_k: None }]);
        s.room_model.view(room)
    };
    let (kitchen, bedroom, kitchen_plain, bedroom_plain) = (air(&fed, "b"), air(&fed, "c"), air(&plain, "b"), air(&plain, "c"));
    let a_c = |v: &Value| v["air_c"].as_f64().unwrap();
    assert!(a_c(&kitchen) > a_c(&kitchen_plain) + 0.1, "{kitchen} {kitchen_plain}");
    assert!(bedroom["loss_ratio"].as_f64().unwrap() > bedroom_plain["loss_ratio"].as_f64().unwrap() + 0.3, "{bedroom} {bedroom_plain}");
    let wrong = air(&fed, "a");
    assert!((a_c(&wrong) - a_c(&air(&plain, "a"))).abs() < 0.1, "a device of another room must not feed this room: {wrong}");
}

#[tokio::test]
async fn a_cooling_room_raises_loss_and_window_probability_in_the_shadow() {
    let dir = tempfile::tempdir().unwrap();
    let (state, _) = rig(dir.path(), true);
    run(&state, 140, 50).await;
    let (cold, calm) = (state.room_model.view("b"), state.room_model.view("a"));
    assert!(cold["loss_ratio"].as_f64().unwrap() > calm["loss_ratio"].as_f64().unwrap(), "{cold} {calm}");
    assert!(cold["p_window"].as_f64().unwrap() > calm["p_window"].as_f64().unwrap());
}

#[test]
fn restart_gap_becomes_filter_reset_events() {
    let dir = tempfile::tempdir().unwrap();
    let (state, _) = rig(dir.path(), true);
    for minute in 0..40 {
        let t = T0 + minute * MIN;
        state.room_model.observe(5, crate::window::LOCAL_TEMPERATURE, 20.5, t);
        state.room_model.observe(5, crate::window::PI_HEATING_DEMAND, 20.0, t);
        state.room_model.step(&state.config, t, [4.0, 3.0, 0.0], 12.0, false, &[]);
    }
    let out = state.room_model.step(&state.config, T0 + 200 * MIN, [4.0, 3.0, 0.0], 12.0, false, &[]);
    assert_eq!(out.events.len(), 3);
    assert!(out.events.iter().all(|e| e.kind == crate::lake::kind::FILTER_RESET && e.detail.as_deref().is_some_and(|d| d.contains("restart_gap"))));
}

#[test]
fn open_window_sentence_says_cold_does_not_count() {
    use crate::room_model_tests::{comps_at, preset_env, preset_rig};
    let (par, shock) = preset_rig(6.0);
    let p = crate::room_plan::plan(&par, &shock, &comps_at(17.0, 16.0, 6.0, 1e-4), 1.0, &preset_env());
    let out = crate::room_model::Out { ok: true, lam: 6.0, p_window: p.p_window, p_supply: 1.0, ..Default::default() };
    let why = crate::room_shadow::sentence("b", &p, &out, 21.0, 24.0);
    assert_eq!(why.split(", spart").next().unwrap(), "b: Verlust 6x normal -> Fenster offen -> Kälte zählt nicht, Heizen wäre Verlust -> 7 °C", "{why}");
}

#[test]
fn storage_sentence_reads_in_german() {
    use crate::room_model_tests::{comps_at, preset_env, preset_rig};
    let (par, shock) = preset_rig(6.0);
    let p = crate::room_plan::plan(&par, &shock, &comps_at(30.0, 22.0, 1.0, 1e-4), 1.0, &preset_env());
    let why = crate::room_shadow::storage_sentence("b", &with_storage(p.clone(), 20.8, 19.5, 220.0, Some(25.4)));
    assert_eq!(why, "b: Wände 20,8 °C, Luft 19,5 °C – ohne Heizen noch 3 h 40 min über 20,5 °C, Speicher +25 Heizmin");
    let under = crate::room_shadow::storage_sentence("b", &with_storage(p.clone(), 18.0, 18.0, 0.0, Some(-12.0)));
    assert_eq!(under, "b: Wände 18 °C, Luft 18 °C – ohne Heizen schon unter 20,5 °C, Speicher -12 Heizmin");
    let long = crate::room_shadow::storage_sentence("b", &with_storage(p, 22.0, 21.0, 1440.0, None));
    assert_eq!(long, "b: Wände 22 °C, Luft 21 °C – ohne Heizen länger als 24 h über 20,5 °C");
}

fn with_storage(mut p: crate::room_plan::PlanOut, mass: f64, air: f64, coast: f64, stored: Option<f64>) -> crate::room_plan::PlanOut {
    (p.mass, p.air, p.coast_min, p.stored_min) = (mass, air, coast, stored);
    p
}

fn ctx(room: &str) -> crate::room_shadow::RoomCtx {
    crate::room_shadow::RoomCtx { room: room.into(), live: 21.0, target: 21.0, present: 1.0, here: false, sp_cur: Some(21.0), boost_ok: false, window_live: false, resid_live_k: None }
}

fn feed(state: &crate::http::AppState, minute: i64, valve: bool) {
    let t = T0 + minute * MIN;
    for node in [5u64, 6, 7] {
        state.room_model.observe(node, crate::window::LOCAL_TEMPERATURE, 20.5, t);
        if valve {
            state.room_model.observe(node, crate::window::PI_HEATING_DEMAND, 30.0, t);
        }
        state.room_model.observe(node, crate::window::OCCUPIED_SETPOINT, 21.0, t);
    }
}

#[test]
fn stale_valve_is_forward_filled_and_the_filter_keeps_predicting_through_a_gap() {
    let dir = tempfile::tempdir().unwrap();
    let (state, _) = rig(dir.path(), true);
    let weather = [4.0, 3.0, 0.0];
    for minute in 0..30 {
        feed(&state, minute, minute < 5);
        state.room_model.step(&state.config, T0 + minute * MIN, weather, 12.0, false, &[ctx("b")]);
    }
    let before = state.room_model.view("b");
    assert!(before["ts"].as_i64().unwrap() > T0 + 24 * MIN, "{before}");
    let mut seen = Vec::new();
    for minute in 30..150 {
        let out = state.room_model.step(&state.config, T0 + minute * MIN, weather, 12.0, false, &[ctx("b")]);
        assert!(out.events.iter().all(|e| e.kind != crate::lake::kind::FILTER_RESET), "{minute}");
        seen.push(state.room_model.view("b"));
    }
    let after = seen.last().unwrap();
    assert!(after["ts"].as_i64().unwrap() > T0 + 144 * MIN, "{after}");
    assert!((after["air_c"].as_f64().unwrap() - before["air_c"].as_f64().unwrap()).abs() > 1e-3, "{before} {after}");
}

#[test]
fn stale_weather_falls_back_to_the_last_value_and_no_weather_is_reported() {
    let dir = tempfile::tempdir().unwrap();
    let (state, _) = rig(dir.path(), true);
    let nan = [f64::NAN; 3];
    let first = state.room_model.step(&state.config, T0, nan, 12.0, false, &[]);
    let states = |out: &crate::room_shadow::StepOut| out.events.iter().filter(|e| e.kind == crate::lake::kind::SHADOW_STATE).filter_map(|e| e.detail.clone()).collect::<Vec<_>>();
    assert!(states(&first).iter().any(|d| d.contains("no_weather")), "{:?}", first.events);
    for minute in 1..20 {
        feed(&state, minute, true);
        state.room_model.step(&state.config, T0 + minute * MIN, if minute < 10 { nan } else { [4.0, 3.0, 0.0] }, 12.0, false, &[ctx("b")]);
    }
    assert!(state.room_model.view("b")["ts"].as_i64().unwrap() > T0 + 14 * MIN);
    for minute in 20..40 {
        feed(&state, minute, true);
        let out = state.room_model.step(&state.config, T0 + minute * MIN, nan, 12.0, false, &[ctx("b")]);
        assert!(states(&out).is_empty(), "{minute}");
    }
    assert!(state.room_model.view("b")["ts"].as_i64().unwrap() > T0 + 34 * MIN);
}

#[test]
fn reload_resets_every_room_with_cause_reload() {
    let dir = tempfile::tempdir().unwrap();
    let (state, _) = rig(dir.path(), true);
    for minute in 0..20 {
        feed(&state, minute, true);
        state.room_model.step(&state.config, T0 + minute * MIN, [4.0, 3.0, 0.0], 12.0, false, &[]);
    }
    state.room_model.set_bundle(Bundle::parse(&bundle_text()).unwrap());
    let out = state.room_model.step(&state.config, T0 + 20 * MIN, [4.0, 3.0, 0.0], 12.0, false, &[]);
    let reload: Vec<_> = out.events.iter().filter(|e| e.kind == crate::lake::kind::FILTER_RESET && e.detail.as_deref().is_some_and(|d| d.contains("\"reload\""))).collect();
    assert_eq!(reload.len(), 3);
    assert!(state.room_model.step(&state.config, T0 + 21 * MIN, [4.0, 3.0, 0.0], 12.0, false, &[]).events.iter().all(|e| !e.detail.as_deref().is_some_and(|d| d.contains("reload"))));
}

#[test]
fn room_model_is_off_by_default() {
    assert!(!crate::room_shadow::RoomModelSection::default().enabled);
}

fn states(out: &crate::room_shadow::StepOut) -> Vec<String> {
    out.events.iter().filter(|e| e.kind == crate::lake::kind::SHADOW_STATE).filter_map(|e| e.detail.clone()).collect()
}

#[test]
fn weather_older_than_the_cap_reports_no_weather() {
    let dir = tempfile::tempdir().unwrap();
    let (state, _) = rig(dir.path(), true);
    let nan = [f64::NAN; 3];
    for minute in 0..10 {
        feed(&state, minute, true);
        state.room_model.step(&state.config, T0 + minute * MIN, [4.0, 3.0, 0.0], 12.0, false, &[]);
    }
    let inside = state.room_model.step(&state.config, T0 + 170 * MIN, nan, 12.0, false, &[]);
    assert!(states(&inside).is_empty(), "{:?}", inside.events);
    let beyond = state.room_model.step(&state.config, T0 + 200 * MIN, nan, 12.0, false, &[]);
    assert!(states(&beyond).iter().any(|d| d.contains("no_weather")), "{:?}", beyond.events);
}

#[tokio::test]
async fn a_panic_in_the_shadow_disables_it_and_never_touches_live_control() {
    let (a, b) = (tempfile::tempdir().unwrap(), tempfile::tempdir().unwrap());
    let (hit, mock_hit) = rig(a.path(), true);
    let (off, mock_off) = rig(b.path(), false);
    run(&hit, 40, 1000).await;
    hit.room_model.panic_on_next_step();
    run(&hit, 5, 1000).await;
    run(&off, 40, 1000).await;
    run(&off, 5, 1000).await;
    hit.sink.flush().unwrap();
    assert_eq!(mock_hit.setpoint_writes(), mock_off.setpoint_writes());
    assert!(hit.room_model.view("a").is_null());
    let first = hit.room_model.step(&hit.config, T0 + 500 * MIN, [4.0, 3.0, 0.0], 12.0, false, &[ctx("a")]);
    assert!(first.rows.is_empty() && first.events.is_empty());
    hit.room_model.observe(5, crate::window::LOCAL_TEMPERATURE, 20.0, T0);
    assert!(hit.room_model.version().is_some());
    let events = string_column(a.path(), "events", "detail");
    assert!(events.iter().any(|d| d.contains("\"error\"")), "{events:?}");
}

#[test]
fn probe_sentence_names_the_closed_valve_and_the_cooling_that_shows_it() {
    use crate::room_model_tests::{comps_at, preset_env, preset_rig};
    let (par, shock) = preset_rig(6.0);
    let p = crate::room_plan::plan(&par, &shock, &comps_at(21.0, 20.5, 2.0, 1e-2), 1.0, &preset_env());
    assert_eq!(p.kind, "probe");
    let out = crate::room_model::Out { ok: true, lam: 2.0, p_window: p.p_window, p_supply: 1.0, ..Default::default() };
    let d = p.probe_min.unwrap();
    assert_eq!(crate::room_shadow::sentence("b", &p, &out, 21.0, 24.0), format!("b: Fenster noch offen? Ventil {d} min zu, die Abkühlung zeigt es -> 7 °C"));
    assert_eq!(crate::room_shadow::probe_end_sentence("b", "end", 10), "b: Probe nach 10 min zu Ende");
}

#[test]
fn a_reload_or_gap_turns_a_running_probe_into_a_probe_end_row_with_the_minutes_spent() {
    use crate::room_plan::ProbeMemo;
    let memo = |start: i64, ended: bool| ProbeMemo { start, d: 20, base: 40, block_until: start + 40, target: 21.0, present: 1.0, ended, unc: 0.4, strikes: 0 };
    let mut probes = std::collections::HashMap::from([("a".to_string(), memo(100, false)), ("b".to_string(), memo(50, true)), ("c".to_string(), memo(10, false))]);
    let rows = crate::room_shadow::close_probes(&mut probes, "v1", 107 * MIN);
    assert!(probes.is_empty());
    let got: Vec<_> = rows.iter().map(|r| (r.room.as_str(), r.kind.as_str(), r.probe_min)).collect();
    assert_eq!(got, [("a", "probe_end", Some(7.0)), ("c", "probe_end", Some(20.0))]);
    assert_eq!(rows[0].why, "a: Probe nach 7 min zu Ende");
}

#[test]
fn a_real_exported_bundle_parses_runs_and_flags_the_degenerate_fits() {
    use crate::room_model::{House, RoomIn};
    use crate::room_plan::{plan, Env};
    let bundle = Bundle::parse(&std::fs::read_to_string(concat!(env!("CARGO_MANIFEST_DIR"), "/testdata/room_model_real_bundle.json")).unwrap()).unwrap();
    assert_eq!(bundle.rooms.keys().map(String::as_str).collect::<Vec<_>>(), ["bad", "kueche", "schlafzimmer", "wohnzimmer"]);
    let mut house = House::new(&bundle);
    let input = |room: &str| RoomIn { y: 20.5, yv: f64::NAN, u: 0.2, sp: 21.0, tout: 5.0, wind: 3.0, rad: 0.0, night: false, ncnt: vec![1.0; bundle.rooms[room].c.len()], nsum: vec![20.0; bundle.rooms[room].c.len()], ya: vec![], co2: f64::NAN, rh: f64::NAN, th: f64::NAN };
    for _ in 0..40 {
        let outs = house.step(&bundle.rooms.keys().map(|r| (r.clone(), input(r))).collect());
        assert!(outs.values().all(|o| !o.ok || (o.m.is_finite() && o.a.is_finite() && o.lam.is_finite())));
    }
    let env = |night: bool, target: f64| Env { tout: 5.0, wind: 3.0, rad: 0.0, night, ncnt: vec![], nsum: vec![], target, present: 1.0, sp_cur: Some(target), sp_live: target, frost: 7.0, boost_c: 24.0, boost_ok: false, raise_cost: 15.0, cold_cost: 1.0, r_grid: bundle.house.r_grid, lag: None, y_last: None, u_now: None, now_min: 0, probe: None, q_net: None, pcnn_active: false };
    for (room, f) in &house.rooms {
        let kinds = bundle.rooms[room].c.len();
        let e = Env { ncnt: vec![1.0; kinds], nsum: vec![20.0; kinds], ..env(false, 22.0) };
        let p = plan(&f.par, &bundle.shock, &f.comps, 1.0, &e);
        assert!(p.total.is_finite() && p.cold.is_finite() && p.setpoint.is_finite(), "{room} {p:?}");
        assert_eq!(p.unusable, room == "bad", "{room} {p:?}");
    }
    let f = &house.rooms["schlafzimmer"];
    let night = plan(&f.par, &bundle.shock, &f.comps, 1.0, &Env { ncnt: vec![1.0], nsum: vec![20.0], ..env(true, 18.0) });
    assert!(night.unusable && night.kind == "fit_unusable", "{night:?}");
}

const PLUG: &str = "[[devices]]
node = 14
kind = 'plug'
role = 'coffee'
room = 'b'
";

fn net_rows(active: bool, here: bool, coffee_mw: f64, weekend: bool) -> Vec<crate::lake::ShadowDecision> {
    let mut text: Value = serde_json::from_str(&bundle_text()).unwrap();
    text["pcnn"]["active"] = active.into();
    let dir = tempfile::tempdir().unwrap();
    let mock = Arc::new(MockController::new(1));
    [5, 6, 7].iter().for_each(|n| mock.add_node(*n, 19.0, 19.0));
    let state = state_with(dir.path(), mock, &format!("{ROOMS}{PLUG}[room_model]\nenabled = true\n"));
    state.status.outdoor_obs(crate::lake::now_us(), 4.0, Some(3.0));
    state.room_model.set_bundle(Bundle::parse(&text.to_string()).unwrap());
    device_run(&state, 120, false);
    let t = aligned() + 120 * MIN;
    state.room_model.observe_device(14, 0x0090, 8, coffee_mw, t);
    let ctxs: Vec<_> = ["b", "c"].iter().map(|r| crate::room_shadow::RoomCtx { here: here && *r == "b", ..ctx(r) }).collect();
    state.room_model.step(&state.config, t, [4.0, 3.0, 0.0], 12.0, weekend, &ctxs).rows
}

#[test]
fn shadow_rows_carry_both_forecasts_and_the_net_follows_presence_coffee_and_weekday() {
    let find = |rows: &[crate::lake::ShadowDecision], room: &str| rows.iter().find(|r| r.room == room).cloned().unwrap();
    let base = net_rows(true, false, 0.0, false);
    let (b, c) = (find(&base, "b"), find(&base, "c"));
    assert_eq!(b.pcnn_active, Some(true));
    assert!(b.forecast_phys_c.is_some() && b.forecast_pcnn_c.is_some() && b.forecast_c == b.forecast_pcnn_c);
    assert!((b.forecast_pcnn_c.unwrap() - b.forecast_phys_c.unwrap()).abs() > 1e-4, "{b:?}");
    assert_eq!((c.pcnn_active, c.forecast_pcnn_c), (Some(false), None), "a room without a net block has no net forecast and the net cannot be active there");
    assert_eq!(c.forecast_c, c.forecast_phys_c);
    for (what, rows) in [("present", net_rows(true, true, 0.0, false)), ("coffee", net_rows(true, false, 1_500_000.0, false)), ("weekend", net_rows(true, false, 0.0, true)), ("idle plug", net_rows(true, false, 5_000.0, false))] {
        let r = find(&rows, "b");
        assert_eq!(r.forecast_phys_c, b.forecast_phys_c, "{what}: the physics forecast never depends on the net inputs");
        if what == "idle plug" {
            assert_eq!(r.forecast_pcnn_c, b.forecast_pcnn_c, "a plug below the on threshold counts as off");
        } else {
            assert_ne!(r.forecast_pcnn_c, b.forecast_pcnn_c, "{what} must reach the net");
        }
    }
    let off = net_rows(false, false, 0.0, false);
    let r = find(&off, "b");
    assert_eq!((r.pcnn_active, r.forecast_pcnn_c, r.forecast_phys_c, r.forecast_c), (Some(false), b.forecast_pcnn_c, b.forecast_phys_c, b.forecast_phys_c));
    let stale_plug = find(&net_rows(true, false, f64::NAN, false), "b");
    assert_eq!(stale_plug.forecast_pcnn_c, b.forecast_pcnn_c);
}
