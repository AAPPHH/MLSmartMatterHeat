use crate::assistant::{Assistant, Failure, Model, ModelError};
use crate::http::AppState;
use crate::manual_tests::{call, env_with};
use axum::http::StatusCode;
use serde_json::{json, Value};
use std::collections::VecDeque;
use std::sync::{Arc, Mutex};
use std::time::Duration;

struct Fake {
    script: Mutex<VecDeque<Result<Value, ModelError>>>,
    seen: Mutex<Vec<Value>>,
}

impl Fake {
    fn new(script: Vec<Result<Value, ModelError>>) -> Arc<Self> {
        Arc::new(Self { script: Mutex::new(script.into()), seen: Default::default() })
    }
}

impl Model for Fake {
    fn generate(&self, body: &Value, _timeout: Duration) -> Result<Value, ModelError> {
        self.seen.lock().unwrap().push(body.clone());
        self.script.lock().unwrap().pop_front().unwrap_or(Err(ModelError::Failed("script exhausted".into())))
    }
}

fn text(t: &str) -> Result<Value, ModelError> {
    Ok(json!({"candidates": [{"content": {"role": "model", "parts": [{"text": t}]}}]}))
}

fn calls(list: &[(&str, Value)]) -> Result<Value, ModelError> {
    let parts: Vec<Value> = list.iter().map(|(name, args)| json!({"functionCall": {"name": name, "args": args}})).collect();
    Ok(json!({"candidates": [{"content": {"role": "model", "parts": parts}}]}))
}

fn tomorrow_plan(celsius: f64) -> Value {
    let day = chrono::Local::now().date_naive() + chrono::Duration::days(1);
    json!({"rooms": ["bad"], "action": "setpoint", "celsius": celsius, "when_type": "once", "from": format!("{day}T15:00"), "to": format!("{day}T22:00"), "label": "Bad abends"})
}

fn setup(script: Vec<Result<Value, ModelError>>, extra: &str) -> (AppState, Arc<Fake>, tempfile::TempDir) {
    let e = env_with(&format!("[assistant]\nenabled = true\n{extra}"));
    let fake = Fake::new(script);
    let state = AppState { assistant: Assistant::with_model(fake.clone()), ..e.state.clone() };
    (state, fake, e.dir)
}

fn ask(state: &AppState, t: &str) -> Result<Value, Failure> {
    state.assistant.ask(state, t, chrono::Local::now().naive_local(), crate::storage::now_ms())
}

fn confirm(state: &AppState, id: &str, accept: bool) -> Result<Value, Failure> {
    state.assistant.confirm(state, id, accept, chrono::Local::now().naive_local(), crate::storage::now_ms())
}

#[test]
fn read_tools_run_in_the_loop_and_only_allow_listed_data_reaches_the_model() {
    let (state, fake, _dir) = setup(vec![calls(&[("get_state", json!({})), ("list_plans", json!({}))]), text("Im Bad sind es 19 Grad.")], "");
    state.latest.all();
    let out = ask(&state, "wie warm ist es im Bad").unwrap();
    assert_eq!((out["reply"].as_str(), out["pending"].is_null()), (Some("Im Bad sind es 19 Grad."), true));
    let seen = fake.seen.lock().unwrap();
    assert_eq!(seen.len(), 2);
    let second = &seen[1]["contents"];
    assert_eq!(second.as_array().unwrap().len(), 3);
    assert_eq!(second[1]["parts"][0]["functionCall"]["name"], "get_state");
    assert!(second[2]["parts"][0]["functionResponse"]["response"]["rooms"].as_array().unwrap().iter().any(|r| r["room"] == "bad"));
    assert_eq!(second[2]["parts"][1]["functionResponse"]["name"], "list_plans");
    let wire = seen[1].to_string().to_lowercase();
    for banned in ["phone", "\"mac\"", "ssid", "wifi", "presence", "\"node\"", "bssid", "latitude", "token"] {
        assert!(!wire.contains(banned), "{banned}");
    }
    assert!(seen[0]["systemInstruction"]["parts"][0]["text"].as_str().unwrap().contains("bad"));
    assert_eq!(seen[0]["tools"][0]["functionDeclarations"].as_array().unwrap().len(), 7);
}

#[test]
fn write_tools_become_one_pending_set_and_run_only_after_confirmation() {
    let (state, _fake, _dir) = setup(vec![calls(&[("preview_plans", json!({"plans": [tomorrow_plan(22.0)], "hours": 48}))]), calls(&[("create_plan", json!({"plan": tomorrow_plan(22.0)})), ("set_hold", json!({"room": "bad", "celsius": 21.0, "hold_min": 60}))])], "");
    let out = ask(&state, "morgen ab 15 Uhr Bad 22 Grad").unwrap();
    let pending = &out["pending"];
    assert!(pending["summary"].as_str().unwrap().contains("22 °C") && pending["summary"].as_str().unwrap().contains("21 °C"));
    assert_eq!(pending["expires_s"], 300);
    assert!(state.plans.list().is_empty() && state.commanded.lock().unwrap().hold(6).is_none());
    let id = pending["id"].as_str().unwrap();
    let done = confirm(&state, id, true).unwrap();
    assert!(done["reply"].as_str().unwrap().contains("Eingetragen"));
    let plans = state.plans.list();
    assert_eq!((plans.len(), plans[0].source.as_str(), plans[0].label.as_str()), (1, "assistant", "Bad abends"));
    assert_eq!(state.commanded.lock().unwrap().hold(6).map(|h| h.value), Some(21.0));
    assert!(matches!(confirm(&state, id, true), Err(Failure::Unknown)));
}

#[test]
fn preview_tool_returns_segments_including_unsaved_plans() {
    let (state, fake, _dir) = setup(vec![calls(&[("preview_plans", json!({"plans": [tomorrow_plan(23.0)], "hours": 48}))]), text("Ab 15 Uhr 23 Grad. Soll ich das eintragen?")], "");
    ask(&state, "x").unwrap();
    let seen = fake.seen.lock().unwrap();
    let rooms = &seen[1]["contents"][2]["parts"][0]["functionResponse"]["response"]["rooms"];
    assert!(rooms["bad"].as_array().unwrap().iter().any(|s| s["celsius"] == 23.0 && s["reason"] == "plan"));
    assert!(state.plans.list().is_empty());
}

#[test]
fn validation_errors_are_fed_back_so_the_model_can_fix_the_call() {
    let (state, fake, _dir) = setup(vec![calls(&[("create_plan", json!({"plan": tomorrow_plan(30.0)}))]), calls(&[("create_plan", json!({"plan": tomorrow_plan(22.0)}))])], "");
    let out = ask(&state, "x").unwrap();
    assert!(out["pending"]["summary"].as_str().unwrap().contains("22 °C"));
    let seen = fake.seen.lock().unwrap();
    assert_eq!(seen.len(), 2);
    assert!(seen[1]["contents"][2]["parts"][0]["functionResponse"]["response"]["error"].as_str().unwrap().contains("celsius"));
    let unknown_room = json!({"plan": {"rooms": ["keller"], "action": "away", "when_type": "once", "from": "2030-01-01T10:00", "to": "2030-01-01T12:00"}});
    drop(seen);
    let (state, fake, _dir) = setup(vec![calls(&[("create_plan", unknown_room), ("delete_plan", json!({"id": "nope"})), ("set_hold", json!({"room": "bad", "celsius": 40})), ("clear_hold", json!({"room": "keller"}))]), text("Das geht nicht.")], "");
    let out = ask(&state, "x").unwrap();
    assert!(out["pending"].is_null());
    let seen = fake.seen.lock().unwrap();
    let answers = seen[1]["contents"][2]["parts"].as_array().unwrap();
    assert!(answers.iter().all(|a| a["functionResponse"]["response"]["error"].is_string()));
}

#[test]
fn rejecting_discards_and_unknown_or_expired_ids_fail() {
    let (state, _fake, _dir) = setup(vec![calls(&[("create_plan", json!({"plan": tomorrow_plan(22.0)}))])], "");
    let id = ask(&state, "x").unwrap()["pending"]["id"].as_str().unwrap().to_string();
    assert_eq!(confirm(&state, &id, false).unwrap()["reply"], "Verworfen.");
    assert!(state.plans.list().is_empty());
    assert!(matches!(confirm(&state, &id, true), Err(Failure::Unknown)));
    assert!(matches!(confirm(&state, "a0", true), Err(Failure::Unknown)));
    let (state, _fake, _dir) = setup(vec![calls(&[("create_plan", json!({"plan": tomorrow_plan(22.0)}))])], "pending_ttl_s = 0\n");
    let id = ask(&state, "x").unwrap()["pending"]["id"].as_str().unwrap().to_string();
    assert!(matches!(confirm(&state, &id, true), Err(Failure::Unknown)));
    assert!(state.plans.list().is_empty());
}

#[test]
fn confirm_revalidates_deleted_plans_and_deletes_existing_ones() {
    let (state, _fake, _dir) = setup(vec![calls(&[("create_plan", json!({"plan": tomorrow_plan(22.0)}))])], "");
    let first = ask(&state, "x").unwrap()["pending"]["id"].as_str().unwrap().to_string();
    let plan_id = confirm(&state, &first, true).map(|_| state.plans.list()[0].id.clone()).unwrap();
    let (fake_state, _fake) = (AppState { assistant: Assistant::with_model(Fake::new(vec![calls(&[("delete_plan", json!({"id": plan_id}))]), calls(&[("delete_plan", json!({"id": plan_id}))])])), ..state.clone() }, ());
    let one = ask(&fake_state, "lösch den Plan").unwrap()["pending"]["id"].as_str().unwrap().to_string();
    let two = ask(&fake_state, "lösch den Plan nochmal").unwrap()["pending"]["id"].as_str().unwrap().to_string();
    assert!(confirm(&fake_state, &one, true).unwrap()["reply"].as_str().unwrap().contains("Gelöscht"));
    assert!(fake_state.plans.list().is_empty());
    assert!(confirm(&fake_state, &two, true).unwrap()["reply"].as_str().unwrap().contains("Fehlgeschlagen"));
}

#[test]
fn model_failures_map_to_timeout_and_error_and_turns_are_bounded() {
    let (state, _fake, _dir) = setup(vec![Err(ModelError::Timeout)], "");
    assert!(matches!(ask(&state, "x"), Err(Failure::Timeout)));
    let (state, _fake, _dir) = setup(vec![Err(ModelError::Failed("HTTP 500".into()))], "");
    assert!(matches!(ask(&state, "x"), Err(Failure::Model(_))));
    let (state, fake, _dir) = setup((0..10).map(|_| calls(&[("get_state", json!({}))])).collect(), "max_turns = 3\n");
    let out = ask(&state, "x").unwrap();
    assert!(out["reply"].as_str().unwrap().contains("zu lange"));
    assert_eq!(fake.seen.lock().unwrap().len(), 3);
    assert!(matches!(Assistant::default().ask(&state, "x", chrono::Local::now().naive_local(), 0), Err(Failure::Disabled)));
}

#[test]
fn replies_and_summaries_are_truncated_by_characters() {
    let (state, _fake, _dir) = setup(vec![text(&"ä".repeat(450))], "");
    assert_eq!(ask(&state, "x").unwrap()["reply"].as_str().unwrap().chars().count(), 300);
    let long = json!({"rooms": ["bad"], "action": "frost", "when_type": "weekly", "days": ["monday", "tuesday", "wednesday", "thursday", "friday", "saturday", "sunday"], "from": "10:00", "to": "12:00", "label": "ü".repeat(80)});
    let (state, _fake, _dir) = setup(vec![calls(&(0..12).map(|_| ("create_plan", json!({"plan": long.clone()}))).collect::<Vec<_>>())], "");
    let out = ask(&state, "x").unwrap();
    assert!(out["pending"]["summary"].as_str().unwrap().chars().count() <= 200);
}

#[tokio::test]
async fn http_routes_gate_validate_and_map_errors() {
    let e = env_with("");
    assert_eq!(call(&e.state, "POST", "/api/assistant", Some(json!({"text": "hallo"}))).await, (StatusCode::SERVICE_UNAVAILABLE, json!({"error": "assistant disabled"})));
    assert_eq!(call(&e.state, "POST", "/api/assistant/confirm", Some(json!({"id": "a", "accept": true}))).await.0, StatusCode::SERVICE_UNAVAILABLE);
    let fake = Fake::new(vec![Err(ModelError::Timeout), Err(ModelError::Failed("HTTP 500 geheim".into())), calls(&[("create_plan", json!({"plan": tomorrow_plan(22.0)}))]), text("ok"), calls(&[("create_plan", json!({"plan": tomorrow_plan(21.0)}))])]);
    let state = AppState { assistant: Assistant::with_model(fake), ..e.state.clone() };
    assert_eq!(call(&state, "POST", "/api/assistant", Some(json!({"text": ""}))).await.0, StatusCode::BAD_REQUEST);
    assert_eq!(call(&state, "POST", "/api/assistant", Some(json!({"text": "ä".repeat(501)}))).await.0, StatusCode::BAD_REQUEST);
    assert_eq!(call(&state, "POST", "/api/assistant", Some(json!({"nope": 1}))).await.0, StatusCode::BAD_REQUEST);
    assert_eq!(call(&state, "POST", "/api/assistant", Some(json!({"text": "ä".repeat(500)}))).await.0, StatusCode::GATEWAY_TIMEOUT);
    let (status, body) = call(&state, "POST", "/api/assistant", Some(json!({"text": "hallo"}))).await;
    assert_eq!((status, body["error"].as_str()), (StatusCode::BAD_GATEWAY, Some("assistant model error")));
    let (status, body) = call(&state, "POST", "/api/assistant", Some(json!({"text": "morgen Bad 22"}))).await;
    assert_eq!(status, StatusCode::OK, "{body}");
    let id = body["pending"]["id"].as_str().unwrap().to_string();
    assert!(body["reply"].as_str().unwrap().chars().count() <= 300);
    assert_eq!(call(&state, "POST", "/api/assistant/confirm", Some(json!({"id": "zzz", "accept": true}))).await.0, StatusCode::NOT_FOUND);
    let (status, body) = call(&state, "POST", "/api/assistant/confirm", Some(json!({"id": id, "accept": true}))).await;
    assert_eq!(status, StatusCode::OK);
    assert!(body["reply"].as_str().unwrap().contains("Eingetragen"));
    assert_eq!(call(&state, "GET", "/api/plans", None).await.1["plans"].as_array().unwrap().len(), 1);
    state.sink.flush().unwrap();
    assert!(crate::capture::tests::lake_rows(e.dir.path(), "events") >= 4);
}

#[test]
fn a_failed_write_in_a_turn_drops_the_valid_writes_of_that_turn() {
    let (state, fake, _dir) = setup(vec![calls(&[("create_plan", json!({"plan": tomorrow_plan(22.0)})), ("create_plan", json!({"plan": tomorrow_plan(30.0)}))]), text("Welchen Wert?")], "");
    let out = ask(&state, "x").unwrap();
    assert!(out["pending"].is_null());
    let seen = fake.seen.lock().unwrap();
    let answers = seen[1]["contents"][2]["parts"].as_array().unwrap();
    assert!(answers.iter().all(|a| a["functionResponse"]["response"]["error"].is_string()));
    assert!(state.assistant.confirm(&state, "any", true, chrono::Local::now().naive_local(), 0).is_err());
}

#[test]
fn stored_setpoint_reasons_never_reach_the_model() {
    for reason in ["away", "sleep", "occupied", "night", "home"] {
        let (state, fake, _dir) = setup(vec![calls(&[("get_state", json!({}))]), text("ok")], "");
        state.storage.insert_setpoint(crate::storage::now_ms(), 6, "bad", 21.0, reason).unwrap();
        ask(&state, "x").unwrap();
        let seen = fake.seen.lock().unwrap();
        let wire = seen[1]["contents"].to_string();
        assert!(wire.contains("\"setpoint\":21.0") && !wire.contains(reason) && !wire.contains("\"reason\""), "{reason}");
    }
}

#[test]
fn parameterless_declarations_omit_parameters() {
    let (state, fake, _dir) = setup(vec![text("ok")], "");
    ask(&state, "x").unwrap();
    let seen = fake.seen.lock().unwrap();
    let decls = seen[0]["tools"][0]["functionDeclarations"].as_array().unwrap();
    for d in decls.iter().filter(|d| matches!(d["name"].as_str(), Some("get_state" | "list_plans"))) {
        assert!(d.get("parameters").is_none());
    }
}
