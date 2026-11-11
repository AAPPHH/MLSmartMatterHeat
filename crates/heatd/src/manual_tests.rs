use arrow::array::Array;
use crate::config::Config;
use crate::http::AppState;
use crate::intent::Intent;
use crate::storage::{now_ms, Storage};
use axum::body::Body;
use axum::http::{Request, StatusCode};
use chrono::NaiveDate;
use http_body_util::BodyExt;
use matter_controller::{MatterController, MockController};
use serde_json::{json, Value};
use std::sync::{Arc, Mutex};
use tower::ServiceExt;

const MIN: i64 = 60_000;

struct Env {
    state: AppState,
    mock: Arc<MockController>,
    dir: tempfile::TempDir,
    t: i64,
}

fn env() -> Env {
    let dir = tempfile::tempdir().unwrap();
    let root = dir.path().to_str().unwrap().replace('\\', "/");
    let config = Config::parse(&format!("db_path = '{root}/live.duckdb'\ndefault_setpoint = 20.0\nfrost_setpoint = 8.0\naway_guard_minutes = 0\n[[rooms]]\nname = 'wohnzimmer'\nnodes = [5]\n[[rooms]]\nname = 'bad'\nnodes = [6]\n[[rooms]]\nname = 'flur'\nnodes = []\n")).unwrap();
    let sink = crate::lake::Sink::open(crate::lake::LakeConfig::new(dir.path().join("lake"))).unwrap();
    let status = crate::status::Status::new(crate::lake::now_us());
    let mock = Arc::new(MockController::new(1));
    mock.add_node(5, 19.0, 19.0);
    mock.add_node(6, 19.0, 19.0);
    let state = AppState {
        watchmatch: crate::watchmatch::WatchMatch::new(&config.watchmatch, sink.clone(), status.clone()),
        storage: Arc::new(Storage::open(&config.db_path).unwrap()),
        sink,
        latest: Default::default(),
        failsafe: crate::failsafe::Failsafe::new(&config.failsafe, None),
        config: Arc::new(config),
        model: None,
        controller: mock.clone(),
        windows: Default::default(),
        commanded: Default::default(),
        status,
        explorer: Arc::new(Mutex::new(crate::explore::Explorer::new(1))),
        tracker: Default::default(),
        boost: Default::default(),
        batch_seq: Arc::new(std::sync::atomic::AtomicU64::new(1)),
        devices: Default::default(),
        room_model: Default::default(),
    };
    Env { state, mock, dir, t: now_ms() }
}

async fn call(state: &AppState, method: &str, uri: &str, body: Option<Value>) -> (StatusCode, Value) {
    let request = Request::builder().method(method).uri(uri).header("content-type", "application/json").body(body.map_or_else(Body::empty, |b| Body::from(b.to_string()))).unwrap();
    let response = crate::http::router(state.clone()).oneshot(request).await.unwrap();
    let status = response.status();
    let bytes = response.into_body().collect().await.unwrap().to_bytes();
    (status, serde_json::from_slice(&bytes).unwrap_or(Value::Null))
}

impl Env {
    async fn step(&mut self, minutes: i64) -> Vec<crate::rules::Desired> {
        self.t += minutes * MIN;
        let noon = NaiveDate::from_ymd_opt(2026, 11, 9).unwrap().and_hms_opt(12, 0, 0).unwrap();
        crate::rules::control_step(&self.state, noon, self.t).await.unwrap()
    }

    async fn device(&self, node: u64) -> f64 {
        self.mock.read_thermostat(node).await.unwrap().occupied_heating_setpoint_c.unwrap()
    }

    async fn post(&self, body: Value) -> (StatusCode, Value) {
        call(&self.state, "POST", "/api/setpoint", Some(body)).await
    }

    fn reasons(&self) -> Vec<String> {
        self.state.sink.flush().unwrap();
        let mut files: Vec<_> = std::fs::read_dir(self.dir.path().join("lake/raw/decisions")).map(|d| d.flatten().map(|e| e.path()).collect()).unwrap_or_default();
        files.sort();
        files
            .iter()
            .flat_map(|f| crate::lake::load_stream(f).unwrap().batches)
            .flat_map(|b| {
                let inputs = b.column_by_name("inputs").unwrap().as_any().downcast_ref::<arrow::array::StructArray>().unwrap().clone();
                let extra = arrow::compute::cast(inputs.column_by_name("extra").unwrap(), &arrow::datatypes::DataType::Utf8).unwrap();
                let extra = extra.as_any().downcast_ref::<arrow::array::StringArray>().unwrap().clone();
                (0..extra.len()).filter(|&i| !extra.is_null(i)).map(|i| extra.value(i).to_string()).collect::<Vec<_>>()
            })
            .collect()
    }
}

#[tokio::test]
async fn validation_rejects_out_of_range_steps_and_unknown_rooms() {
    let e = env();
    let bad = [
        (json!({"room": "wohnzimmer", "celsius": 26.5}), StatusCode::BAD_REQUEST),
        (json!({"room": "wohnzimmer", "celsius": 7.5}), StatusCode::BAD_REQUEST),
        (json!({"room": "wohnzimmer", "celsius": 21.3}), StatusCode::BAD_REQUEST),
        (json!({"room": "wohnzimmer"}), StatusCode::BAD_REQUEST),
        (json!({"room": "wohnzimmer", "celsius": 22.0, "hold_min": 14.0}), StatusCode::BAD_REQUEST),
        (json!({"room": "wohnzimmer", "celsius": 22.0, "hold_min": 721.0}), StatusCode::BAD_REQUEST),
        (json!({"room": "wohnzimmer", "celsius": 22.0, "source": "dial"}), StatusCode::BAD_REQUEST),
        (json!({"room": "keller", "celsius": 22.0}), StatusCode::NOT_FOUND),
        (json!({"room": "flur", "celsius": 22.0}), StatusCode::NOT_FOUND),
    ];
    for (body, status) in bad {
        let (got, reply) = e.post(body.clone()).await;
        assert_eq!((got, reply["ok"].as_bool()), (status, Some(false)), "{body}");
    }
    assert!(e.state.commanded.lock().unwrap().hold(5).is_none());
    for (celsius, hold) in [(8.0, 15.0), (26.0, 720.0), (22.5, 120.0)] {
        let (status, reply) = e.post(json!({"room": "wohnzimmer", "celsius": celsius, "hold_min": hold})).await;
        assert_eq!((status, reply["target"].as_f64()), (StatusCode::OK, Some(celsius)));
    }
}

#[tokio::test]
async fn hold_is_written_through_the_normal_path_and_expires_back_to_the_rules() {
    let mut e = env();
    e.step(0).await;
    assert_eq!(e.device(5).await, 20.0);
    let (status, reply) = e.post(json!({"room": "wohnzimmer", "celsius": 22.0, "source": "watch"})).await;
    assert_eq!((status, reply["hold_min"].as_f64()), (StatusCode::OK, Some(120.0)));
    let wants = e.step(1).await;
    assert!(wants.iter().any(|w| (w.node, w.setpoint, w.reason, w.rule) == (5, 22.0, "manual_watch", "manual")));
    assert_eq!((e.device(5).await, e.device(6).await), (22.0, 20.0));
    e.step(60).await;
    assert_eq!(e.device(5).await, 22.0);
    let state = call(&e.state, "GET", "/api/state", None).await.1;
    let room = state["rooms"].as_array().unwrap().iter().find(|r| r["name"] == "wohnzimmer").unwrap().clone();
    assert_eq!((room["manual"]["target"].as_f64(), room["manual"]["source"].as_str()), (Some(22.0), Some("watch")));
    let listed = call(&e.state, "GET", "/api/setpoint", None).await.1;
    assert_eq!(listed["holds"][0]["room"], "wohnzimmer");
    e.step(61).await;
    assert_eq!(e.device(5).await, 20.0);
    assert!(e.state.commanded.lock().unwrap().hold(5).is_none());
    assert!(e.reasons().iter().any(|x| x.contains("manual_watch")));
    assert!(!e.state.failsafe.stopping());
    assert!(!e.mock.setpoint_writes().iter().any(|w| w.1 == e.state.config.failsafe.setpoint));
}

#[tokio::test]
async fn open_window_beats_the_hold_and_the_hold_returns_after_closing() {
    let mut e = env();
    e.post(json!({"room": "wohnzimmer", "celsius": 22.0})).await;
    e.step(1).await;
    assert_eq!(e.device(5).await, 22.0);
    crate::intent::apply(&e.state, Intent::WindowOpen { room: "wohnzimmer".into() });
    e.step(1).await;
    assert_eq!(e.device(5).await, 8.0);
    let notes = e.reasons();
    assert!(notes.iter().any(|x| x.contains("Fenster offen -> 8 °C trotz Vorgabe 22 °C")), "{notes:?}");
    assert!(e.state.commanded.lock().unwrap().hold(5).is_some());
    crate::intent::apply(&e.state, Intent::WindowClose { room: "wohnzimmer".into() });
    e.step(1).await;
    assert_eq!(e.device(5).await, 22.0);
}

#[tokio::test]
async fn setpoint_reply_carries_a_german_note_while_the_window_is_open() {
    let e = env();
    assert!(e.post(json!({"room": "wohnzimmer", "celsius": 22.0})).await.1["note"].is_null());
    crate::intent::apply(&e.state, Intent::WindowOpen { room: "wohnzimmer".into() });
    let (_, reply) = e.post(json!({"room": "wohnzimmer", "celsius": 22.5})).await;
    assert_eq!(reply["note"], "Fenster offen – Vorgabe 22,5 °C gilt, sobald es zu ist");
    assert!(e.post(json!({"room": "bad", "celsius": 22.0})).await.1["note"].is_null());
}

#[tokio::test]
async fn away_does_not_cancel_an_explicit_hold() {
    let mut e = env();
    let now = now_ms();
    for (ts, motion) in [(now - 40 * MIN, "moving"), (now - 5 * MIN, "still")] {
        let body = json!({"ts": ts, "device": "pixel-watch", "motion": motion, "away": true});
        call(&e.state, "POST", "/api/scan", Some(body)).await;
    }
    let wants = e.step(0).await;
    assert!(wants.iter().all(|w| w.reason == "away"));
    e.post(json!({"room": "wohnzimmer", "celsius": 23.0})).await;
    let wants = e.step(1).await;
    assert!(wants.iter().any(|w| (w.node, w.setpoint, w.reason) == (5, 23.0, "manual_api")));
    assert!(wants.iter().any(|w| (w.node, w.reason) == (6, "away") || w.node != 6));
    assert_eq!(e.device(5).await, 23.0);
    e.step(30).await;
    assert_eq!(e.device(5).await, 23.0);
}

#[tokio::test]
async fn clear_ends_the_hold_early_by_delete_and_by_flag() {
    let mut e = env();
    for clear in [true, false] {
        e.post(json!({"room": "bad", "celsius": 24.0})).await;
        e.step(1).await;
        assert_eq!(e.device(6).await, 24.0);
        let (status, reply) = if clear { e.post(json!({"room": "bad", "clear": true})).await } else { call(&e.state, "DELETE", "/api/setpoint?room=bad", None).await };
        assert_eq!((status, reply["cleared"].as_bool()), (StatusCode::OK, Some(true)));
        e.step(1).await;
        assert_eq!(e.device(6).await, 20.0);
        assert_eq!(call(&e.state, "GET", "/api/setpoint", None).await.1["holds"].as_array().unwrap().len(), 0);
    }
    let (_, reply) = call(&e.state, "DELETE", "/api/setpoint?room=bad", None).await;
    assert_eq!(reply["cleared"].as_bool(), Some(false));
}

#[tokio::test]
async fn holds_survive_a_restart_and_cleared_or_expired_ones_do_not() {
    let mut e = env();
    e.post(json!({"room": "wohnzimmer", "celsius": 22.0, "hold_min": 60.0, "source": "watch"})).await;
    e.post(json!({"room": "bad", "celsius": 23.0})).await;
    e.post(json!({"room": "bad", "clear": true})).await;
    *e.state.commanded.lock().unwrap() = Default::default();
    assert_eq!(crate::restore::restore_holds(&e.state, e.t + MIN).unwrap(), 1);
    let hold = e.state.commanded.lock().unwrap().hold(5).cloned().unwrap();
    assert_eq!((hold.value, hold.source), (22.0, crate::rules::HoldSource::Watch));
    assert!(e.state.commanded.lock().unwrap().hold(6).is_none());
    e.step(1).await;
    assert_eq!(e.device(5).await, 22.0);
    *e.state.commanded.lock().unwrap() = Default::default();
    assert_eq!(crate::restore::restore_holds(&e.state, e.t + 61 * MIN).unwrap(), 0);
    assert!(e.dir.path().exists());
}

#[tokio::test]
async fn dial_turn_replaces_an_api_hold_and_report_lists_the_hold() {
    let mut e = env();
    e.post(json!({"room": "wohnzimmer", "celsius": 22.0})).await;
    e.step(1).await;
    let report = matter_controller::AttributeReport { node: 5, endpoint: 1, attribute: "occupied_heating_setpoint".into(), value: 22.0 };
    crate::rules::on_report(&e.state, &report, false, e.t + 1_000);
    assert_eq!(e.state.commanded.lock().unwrap().hold(5).unwrap().source, crate::rules::HoldSource::Api);
    e.mock.write_heating_setpoint(5, 19.0).await.unwrap();
    crate::rules::on_report(&e.state, &matter_controller::AttributeReport { value: 19.0, ..report }, false, e.t + 10 * MIN);
    let hold = e.state.commanded.lock().unwrap().hold(5).cloned().unwrap();
    assert_eq!((hold.source, hold.value), (crate::rules::HoldSource::Dial, 19.0));
    e.state.sink.flush().unwrap();
    *e.state.commanded.lock().unwrap() = Default::default();
    assert_eq!(crate::restore::restore_holds(&e.state, e.t + 11 * MIN).unwrap(), 0);
}

#[test]
fn the_report_lists_one_german_line_per_hold_and_per_early_end() {
    let dir = tempfile::tempdir().unwrap();
    let date = "2026-11-10".parse::<NaiveDate>().unwrap();
    let base = crate::report::midnight_us(date);
    let sink = crate::lake::Sink::open_with_clock(crate::lake::LakeConfig::new(dir.path()), Arc::new(move || base)).unwrap();
    let event = |at: i64, detail: Value| crate::lake::Event { ts: base + at * 60_000_000, ..crate::lake::Event::now(crate::lake::kind::MANUAL_SETPOINT, "heatd").with_detail(detail) };
    sink.append_on(date, event(600, json!({"room": "bad", "target": 22.5, "hold_min": 120.0, "source": "watch", "until_ts": 1}))).unwrap();
    sink.append_on(date, event(660, json!({"room": "bad", "cleared": true, "source": "api", "until_ts": 1}))).unwrap();
    sink.roll_day(date).unwrap();
    let md = crate::manual::report_section(dir.path(), date).unwrap();
    assert!(md.contains("10:00:00 bad: 22,5 °C fuer 120 min vorgegeben (watch)") || md.contains("bad: 22,5 °C fuer 120 min vorgegeben (watch)"), "{md}");
    assert!(md.contains("bad: Vorgabe vorzeitig beendet (api)"), "{md}");
    assert!(crate::manual::report_section(tempfile::tempdir().unwrap().path(), date).unwrap().contains("Keine Raumtemperatur"));
}

#[tokio::test]
async fn bad_json_is_a_german_400_not_a_plain_422() {
    let e = env();
    for body in ["{", "{\"room\": \"bad\", \"celsius\": \"warm\"}", "{\"celsius\": 21.0}"] {
        let request = Request::builder().method("POST").uri("/api/setpoint").header("content-type", "application/json").body(Body::from(body)).unwrap();
        let response = crate::http::router(e.state.clone()).oneshot(request).await.unwrap();
        assert_eq!(response.status(), StatusCode::BAD_REQUEST, "{body}");
        let reply: Value = serde_json::from_slice(&response.into_body().collect().await.unwrap().to_bytes()).unwrap();
        assert_eq!(reply["ok"].as_bool(), Some(false));
        assert!(reply["error"].as_str().unwrap().contains("JSON"));
    }
}
