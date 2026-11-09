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
    assert!(off.room_model.view("b").is_null());
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
        state.room_model.step(&state.config, t, [4.0, 3.0, 0.0], 12.0, &[]);
    }
    let out = state.room_model.step(&state.config, T0 + 200 * MIN, [4.0, 3.0, 0.0], 12.0, &[]);
    assert_eq!(out.events.len(), 3);
    assert!(out.events.iter().all(|e| e.kind == crate::lake::kind::FILTER_RESET && e.detail.as_deref().is_some_and(|d| d.contains("restart_gap"))));
}
