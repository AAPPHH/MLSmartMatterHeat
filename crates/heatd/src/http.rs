use crate::config::Config;
use crate::infer::{Explanation, RoomModel};
use crate::storage::{now_ms, BatchBody, ScanBody, Storage};
use axum::extract::State;
use axum::http::StatusCode;
use axum::response::{IntoResponse, Response};
use axum::routing::{get, post};
use axum::{Json, Router};
use matter_controller::MatterController;
use serde::Deserialize;
use serde_json::{json, Value};
use std::collections::{HashMap, HashSet};
use std::sync::{Arc, Mutex};

#[derive(Clone)]
pub struct AppState {
    pub config: Arc<Config>,
    pub storage: Arc<Storage>,
    pub sink: crate::lake::Sink,
    pub status: crate::status::Status,
    pub model: Option<Arc<RoomModel>>,
    pub controller: Arc<dyn MatterController>,
    pub windows: Arc<Mutex<HashSet<String>>>,
    pub commanded: Arc<Mutex<HashMap<u64, f64>>>,
}

pub struct ApiError(StatusCode, anyhow::Error);

impl ApiError {
    fn bad(msg: &str) -> Self {
        Self(StatusCode::BAD_REQUEST, anyhow::anyhow!(msg.to_string()))
    }
}

impl<E: Into<anyhow::Error>> From<E> for ApiError {
    fn from(e: E) -> Self {
        Self(StatusCode::INTERNAL_SERVER_ERROR, e.into())
    }
}

impl IntoResponse for ApiError {
    fn into_response(self) -> Response {
        (self.0, Json(json!({"error": self.1.to_string()}))).into_response()
    }
}

type ApiResult = Result<Json<Value>, ApiError>;

#[derive(Deserialize)]
struct WindowBody {
    room: String,
    open: bool,
}

#[derive(Deserialize)]
struct LabelBody {
    device: String,
    from: i64,
    to: i64,
    label: String,
}

const MAX_LABEL_SPAN_MS: i64 = 3_600_000;

pub fn router(state: AppState) -> Router {
    Router::new()
        .route("/health", get(health))
        .route("/metrics", get(metrics))
        .route("/api/scan", post(scan))
        .route("/api/scans", post(scans))
        .route("/api/state", get(api_state))
        .route("/api/snapshot", post(snapshot))
        .route("/api/window", post(window))
        .route("/api/label", post(label))
        .with_state(state)
}

async fn health(State(state): State<AppState>) -> Json<Value> {
    Json(state.status.health(crate::lake::now_us()))
}

async fn metrics(State(state): State<AppState>) -> impl IntoResponse {
    ([(axum::http::header::CONTENT_TYPE, "text/plain; version=0.0.4; charset=utf-8")], state.status.metrics(crate::lake::now_us()))
}

const MAX_FUTURE_MS: i64 = 300_000;

struct Outcome {
    stored: usize,
    duplicate: bool,
    room: Option<(String, i64)>,
    explanation: Explanation,
}

fn attribution(out: Option<&Outcome>) -> Value {
    match out.and_then(|o| o.room.as_ref().map(|r| (o, &r.0))) {
        Some((o, label)) => json!({"probability": o.explanation.probability(label), "rooms": o.explanation.rooms, "aps": o.explanation.aps}),
        None => json!({"probability": null, "rooms": [], "aps": []}),
    }
}

fn merge(response: &mut Value, extra: Value) {
    for (k, v) in extra.as_object().unwrap() {
        response[k] = v.clone();
    }
}

fn ingest(state: &AppState, body: &ScanBody, received: i64) -> Result<Outcome, ApiError> {
    let mut body = body.clone();
    if body.ts > received + MAX_FUTURE_MS {
        eprintln!("scan ts from {} is {} ms in the future, clamped to received_at", body.device, body.ts - received);
        body.ts = received;
    }
    let mut room = None;
    let mut explanation = Explanation::default();
    if let Some(model) = state.model.as_ref().filter(|_| !body.aps.is_empty()) {
        room = Some(model.predict(&body.aps)?);
        explanation = model.explain(&body.aps).unwrap_or_else(|e| {
            eprintln!("explain failed for {}: {e}", body.device);
            Explanation::default()
        });
    }
    let prediction = room.as_ref().map(|(label, probability)| (label.as_str(), *probability));
    let done = state.storage.ingest(&body, received, prediction)?;
    Ok(Outcome { stored: done.stored, duplicate: done.duplicate, room: room.map(|r| (r.0, body.ts)), explanation })
}

async fn scan(State(state): State<AppState>, Json(body): Json<ScanBody>) -> ApiResult {
    let out = ingest(&state, &body, now_ms())?;
    let mut response = json!({"stored": out.stored, "room": out.room.as_ref().map(|r| r.0.clone())});
    merge(&mut response, attribution(Some(&out)));
    if out.duplicate {
        response["duplicate"] = json!(true);
    }
    Ok(Json(response))
}

async fn scans(State(state): State<AppState>, Json(body): Json<BatchBody>) -> ApiResult {
    let received = now_ms();
    let (mut stored, mut duplicates) = (0, 0);
    let mut latest: Option<Outcome> = None;
    for scan in &body.scans {
        let out = ingest(&state, scan, received)?;
        stored += out.stored;
        duplicates += usize::from(out.duplicate);
        if out.room.as_ref().is_some_and(|r| latest.as_ref().and_then(|l| l.room.as_ref()).is_none_or(|l| r.1 >= l.1)) {
            latest = Some(out);
        }
    }
    let mut response = json!({"stored": stored, "accepted": body.scans.len(), "duplicates": duplicates, "room": latest.as_ref().and_then(|o| o.room.as_ref().map(|r| r.0.clone()))});
    merge(&mut response, attribution(latest.as_ref()));
    Ok(Json(response))
}

async fn api_state(State(state): State<AppState>) -> ApiResult {
    let readings = state.storage.last_readings()?;
    let setpoints = state.storage.last_setpoints()?;
    let windows = state.windows.lock().unwrap().clone();
    let commanded = state.commanded.lock().unwrap().clone();
    let rooms: Vec<Value> = state
        .config
        .rooms
        .iter()
        .map(|room| {
            let nodes: Vec<Value> = room
                .nodes
                .iter()
                .map(|&node| {
                    let reading = |name: &str| readings.iter().find(|r| r.node == node && r.attribute == name).map(|r| r.value);
                    let setpoint = setpoints.iter().find(|s| s.node == node);
                    json!({
                        "node": node,
                        "local_temperature": reading("local_temperature"),
                        "occupied_heating_setpoint": reading("occupied_heating_setpoint"),
                        "system_mode": reading("system_mode"),
                        "commanded_setpoint": commanded.get(&node),
                        "reason": setpoint.map(|s| s.reason.clone()),
                        "setpoint_ts": setpoint.map(|s| s.ts),
                    })
                })
                .collect();
            json!({"name": room.name, "window_open": windows.contains(&room.name), "nodes": nodes})
        })
        .collect();
    Ok(Json(json!({"rooms": rooms, "devices": state.storage.last_predictions()?})))
}

async fn snapshot(State(state): State<AppState>) -> ApiResult {
    state.storage.snapshot(&state.config.snapshot_path)?;
    Ok(Json(json!({"path": state.config.snapshot_path})))
}

async fn window(State(state): State<AppState>, Json(body): Json<WindowBody>) -> ApiResult {
    let mut windows = state.windows.lock().unwrap();
    if body.open {
        windows.insert(body.room);
    } else {
        windows.remove(&body.room);
    }
    Ok(Json(json!({"window_open": windows.iter().collect::<Vec<_>>()})))
}

async fn label(State(state): State<AppState>, Json(body): Json<LabelBody>) -> ApiResult {
    let valid = (1..=32).contains(&body.label.len()) && body.label.bytes().all(|b| b.is_ascii_lowercase() || b == b'_');
    if !valid {
        return Err(ApiError::bad("label must match ^[a-z_]{1,32}$"));
    }
    if body.from > body.to || body.to - body.from > MAX_LABEL_SPAN_MS {
        return Err(ApiError::bad("range must satisfy from <= to and to - from <= 3600000"));
    }
    let labelled = state.storage.relabel(&body.device, body.from, body.to, &body.label)?;
    Ok(Json(json!({"labelled": labelled})))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::rules::control_step;
    use axum::body::Body;
    use axum::http::Request;
    use chrono::NaiveDate;
    use http_body_util::BodyExt;
    use matter_controller::MockController;
    use std::path::Path;
    use tower::ServiceExt;

    const CONTRACT: &str = concat!(env!("CARGO_MANIFEST_DIR"), "/../../contract");
    const FIXTURES: &str = concat!(env!("CARGO_MANIFEST_DIR"), "/tests/fixtures");

    struct Harness {
        state: AppState,
        mock: Arc<MockController>,
        _dir: tempfile::TempDir,
    }

    fn harness(with_model: bool) -> Harness {
        let dir = tempfile::tempdir().unwrap();
        let toml = format!(
            "db_path = '{0}/live.duckdb'\nsnapshot_path = '{0}/out/snapshot.duckdb'\ndefault_setpoint = 18.0\n[[rooms]]\nname = 'wohnzimmer'\nnodes = [5]\n",
            dir.path().to_str().unwrap().replace('\\', "/")
        );
        let config = Config::parse(&toml).unwrap();
        let mock = Arc::new(MockController::new(1));
        mock.add_node(5, 19.0, 19.0);
        let model = with_model.then(|| Arc::new(RoomModel::load(&Path::new(FIXTURES).join("room_model.onnx")).unwrap()));
        let state = AppState {
            storage: Arc::new(Storage::open(&config.db_path).unwrap()),
            sink: crate::lake::Sink::open(crate::lake::LakeConfig::new(dir.path().join("lake"))).unwrap(),
            status: crate::status::Status::new(crate::lake::now_us()),
            config: Arc::new(config),
            model,
            controller: mock.clone(),
            windows: Default::default(),
            commanded: Default::default(),
        };
        Harness { state, mock, _dir: dir }
    }

    async fn call(state: &AppState, method: &str, uri: &str, body: Option<String>) -> (StatusCode, Value) {
        let (status, mut value) = call_raw(state, method, uri, body).await;
        if let Some(map) = value.as_object_mut() {
            for key in ["probability", "rooms", "aps"].into_iter().filter(|_| uri.starts_with("/api/scan")) {
                map.remove(key);
            }
        }
        (status, value)
    }

    async fn call_raw(state: &AppState, method: &str, uri: &str, body: Option<String>) -> (StatusCode, Value) {
        let request = Request::builder()
            .method(method)
            .uri(uri)
            .header("content-type", "application/json")
            .body(body.map_or_else(Body::empty, Body::from))
            .unwrap();
        let response = router(state.clone()).oneshot(request).await.unwrap();
        let status = response.status();
        let bytes = response.into_body().collect().await.unwrap().to_bytes();
        (status, serde_json::from_slice(&bytes).unwrap_or(Value::Null))
    }

    fn scan_body() -> String {
        std::fs::read_to_string(format!("{CONTRACT}/scan_example.json")).unwrap()
    }

    fn scan_at(ts: i64, extra: Value) -> String {
        let mut v: Value = serde_json::from_str(&scan_body()).unwrap();
        v["ts"] = json!(ts);
        v.as_object_mut().unwrap().remove("id");
        for (k, x) in extra.as_object().unwrap() {
            v[k] = x.clone();
        }
        v.to_string()
    }

    fn fixture(name: &str) -> String {
        std::fs::read_to_string(format!("{CONTRACT}/{name}")).unwrap()
    }

    #[tokio::test]
    async fn heartbeat_stores_state_without_prediction() {
        let h = harness(true);
        let (status, body) = call(&h.state, "POST", "/api/scan", Some(fixture("heartbeat_example.json"))).await;
        assert_eq!((status, body), (StatusCode::OK, json!({"stored": 0, "room": null})));
        assert_eq!((h.state.storage.count("scans").unwrap(), h.state.storage.count("predictions").unwrap(), h.state.storage.count("device_state").unwrap()), (0, 0, 1));
        let st = &h.state.storage.device_statuses().unwrap()[0];
        assert_eq!((st.motion, st.power, st.away), (Some(crate::storage::Motion::Still), Some(crate::storage::Power::Doze), false));
        let (status, _) = call(&h.state, "POST", "/api/scan", Some("{\"ts\":1,\"device\":\"w\",\"motion\":\"sideways\"}".into())).await;
        assert_eq!(status, StatusCode::UNPROCESSABLE_ENTITY);
        let (status, body) = call(&h.state, "POST", "/api/scan", Some("{\"ts\":1,\"device\":\"w\"}".into())).await;
        assert_eq!((status, body), (StatusCode::OK, json!({"stored": 0, "room": null})));
    }

    #[tokio::test]
    async fn batch_with_mixed_queued_scans() {
        let h = harness(true);
        let (status, body) = call(&h.state, "POST", "/api/scans", Some(fixture("batch_example.json"))).await;
        assert_eq!((status, body), (StatusCode::OK, json!({"stored": 4, "accepted": 2, "duplicates": 0, "room": "wohnzimmer"})));
        assert_eq!((h.state.storage.count("scans").unwrap(), h.state.storage.count("predictions").unwrap(), h.state.storage.count("device_state").unwrap()), (4, 1, 2));
        let predictions = h.state.storage.last_predictions().unwrap();
        assert_eq!(predictions[0].ts, 1_761_580_800_000);
        let st = &h.state.storage.device_statuses().unwrap()[0];
        assert_eq!((st.state_ts, st.motion), (1_761_582_600_000, Some(crate::storage::Motion::Still)));
        let (status, body) = call(&h.state, "POST", "/api/scans", Some("{\"scans\":[]}".into())).await;
        assert_eq!((status, body), (StatusCode::OK, json!({"stored": 0, "accepted": 0, "duplicates": 0, "room": null})));
    }

    #[tokio::test]
    async fn out_of_order_batch_keeps_latest_ts() {
        let h = harness(true);
        let newer = serde_json::from_str::<Value>(&scan_at(2_000, json!({"queued": true, "motion": "still"}))).unwrap();
        let older = serde_json::from_str::<Value>(&scan_at(1_000, json!({"queued": true, "motion": "moving"}))).unwrap();
        let (_, body) = call(&h.state, "POST", "/api/scans", Some(json!({"scans": [newer, older]}).to_string())).await;
        assert_eq!((body["stored"].clone(), body["room"].clone()), (json!(8), json!("wohnzimmer")));
        call(&h.state, "POST", "/api/scan", Some(scan_at(500, json!({"queued": true, "motion": "moving"})))).await;
        assert_eq!(h.state.storage.last_predictions().unwrap()[0].ts, 2_000);
        let st = &h.state.storage.device_statuses().unwrap()[0];
        assert_eq!((st.state_ts, st.motion), (2_000, Some(crate::storage::Motion::Still)));
    }

    #[tokio::test]
    async fn still_heartbeat_keeps_presence_and_away_goes_eco() {
        let h = harness(true);
        let noon = NaiveDate::from_ymd_opt(2025, 10, 27).unwrap().and_hms_opt(12, 0, 0).unwrap();
        let now = now_ms();
        let old = now - 3 * 3_600_000;
        call(&h.state, "POST", "/api/scan", Some(scan_at(old, json!({"motion": "still"})))).await;
        call(&h.state, "POST", "/api/scan", Some(json!({"ts": now, "device": "pixel-watch", "motion": "still", "power": "doze"}).to_string())).await;
        let boosted = control_step(&h.state, noon, now).await.unwrap();
        assert_eq!((boosted[0].setpoint, boosted[0].reason), (19.0, "presence_boost"));
        call(&h.state, "POST", "/api/scan", Some(json!({"ts": now + 1, "device": "pixel-watch", "motion": "moving", "away": true}).to_string())).await;
        let eco = control_step(&h.state, noon, now + 1).await.unwrap();
        assert_eq!((eco[0].setpoint, eco[0].reason), (16.0, "away_eco"));
    }

    #[tokio::test]
    async fn scan_ingest_end_to_end() {
        let h = harness(true);
        let (status, body) = call(&h.state, "POST", "/api/scan", Some(scan_body())).await;
        assert_eq!((status, body), (StatusCode::OK, json!({"stored": 4, "room": "wohnzimmer"})));
        assert_eq!(h.state.storage.count("scans").unwrap(), 4);
        assert_eq!(h.state.storage.count("predictions").unwrap(), 1);
        let (_, state) = call(&h.state, "GET", "/api/state", None).await;
        assert_eq!(state["devices"][0]["device"], "pixel-watch");
        assert_eq!(state["devices"][0]["room"], "wohnzimmer");
        assert_eq!(state["rooms"][0]["name"], "wohnzimmer");
        let (status, snap) = call(&h.state, "POST", "/api/snapshot", None).await;
        assert_eq!(status, StatusCode::OK);
        assert!(Path::new(snap["path"].as_str().unwrap()).exists());
        assert_eq!(call(&h.state, "GET", "/health", None).await.0, StatusCode::OK);
    }

    #[tokio::test]
    async fn health_and_metrics_expose_node_status() {
        let h = harness(false);
        h.state.status.register_nodes([5]);
        h.state.status.subscription(5, crate::status::SubState::Subscribed, crate::lake::now_us());
        h.state.status.node_report(5, crate::lake::now_us());
        let (code, health) = call(&h.state, "GET", "/health", None).await;
        assert_eq!((code, health["status"].as_str(), health["nodes"][0]["node"].as_u64(), health["nodes"][0]["subscription"].as_str()), (StatusCode::OK, Some("ok"), Some(5), Some("subscribed")));
        assert!(health["nodes"][0]["last_report_age_s"].as_f64().unwrap() < 5.0);
        let response = router(h.state.clone()).oneshot(Request::builder().uri("/metrics").body(Body::empty()).unwrap()).await.unwrap();
        assert_eq!(response.status(), StatusCode::OK);
        assert!(response.headers()["content-type"].to_str().unwrap().starts_with("text/plain"));
        let text = String::from_utf8(response.into_body().collect().await.unwrap().to_bytes().to_vec()).unwrap();
        assert!(text.contains("heatd_node_subscription{node=\"5\",state=\"subscribed\"} 1") && text.contains("heatd_node_reports_total{node=\"5\"} 1"), "{text}");
    }

    #[tokio::test]
    async fn scan_reply_carries_attribution() {
        let h = harness(true);
        let (_, body) = call_raw(&h.state, "POST", "/api/scan", Some(scan_body())).await;
        assert!(body["probability"].as_f64().unwrap() > 0.9);
        let rooms = body["rooms"].as_array().unwrap();
        assert_eq!(rooms.len(), 3);
        assert_eq!(rooms[0]["room"], "wohnzimmer");
        let sum: f64 = rooms[0]["parts"].as_array().unwrap().iter().map(|p| p["share"].as_f64().unwrap()).sum();
        assert!((sum - rooms[0]["p"].as_f64().unwrap()).abs() < 1e-6);
        let aps: Vec<&str> = body["aps"].as_array().unwrap().iter().map(|a| a["bssid"].as_str().unwrap()).collect();
        assert_eq!(aps[0], "aa:bb:cc:00:00:01");
        assert!(!aps.contains(&"aa:bb:cc:00:00:99"));
        let (_, batch) = call_raw(&h.state, "POST", "/api/scans", Some(fixture("batch_example.json"))).await;
        assert_eq!(batch["rooms"].as_array().unwrap().len(), 3);
        assert!(!batch["aps"].as_array().unwrap().is_empty());
        let (_, heartbeat) = call_raw(&h.state, "POST", "/api/scan", Some(fixture("heartbeat_example.json"))).await;
        assert_eq!((heartbeat["probability"].clone(), heartbeat["rooms"].clone(), heartbeat["aps"].clone()), (Value::Null, json!([]), json!([])));
        let bare = harness(false);
        let (_, none) = call_raw(&bare.state, "POST", "/api/scan", Some(scan_body())).await;
        assert_eq!((none["probability"].clone(), none["rooms"].clone(), none["aps"].clone()), (Value::Null, json!([]), json!([])));
    }

    #[tokio::test]
    async fn scan_without_model_has_null_room() {
        let h = harness(false);
        let (status, body) = call(&h.state, "POST", "/api/scan", Some(scan_body())).await;
        assert_eq!((status, body), (StatusCode::OK, json!({"stored": 4, "room": null})));
        assert_eq!(h.state.storage.count("predictions").unwrap(), 0);
        let (status, _) = call(&h.state, "POST", "/api/scan", Some("{\"nope\":1}".into())).await;
        assert_eq!(status, StatusCode::UNPROCESSABLE_ENTITY);
    }

    #[tokio::test]
    async fn control_loop_commands_mock_and_respects_hysteresis() {
        let h = harness(true);
        let noon = NaiveDate::from_ymd_opt(2025, 10, 27).unwrap().and_hms_opt(12, 0, 0).unwrap();
        let first = control_step(&h.state, noon, 1_000).await.unwrap();
        assert_eq!((first.len(), first[0].setpoint, first[0].reason), (1, 18.0, "default"));
        assert!(control_step(&h.state, noon, 2_000).await.unwrap().is_empty());
        call(&h.state, "POST", "/api/window", Some("{\"room\":\"wohnzimmer\",\"open\":true}".into())).await;
        let frost = control_step(&h.state, noon, 3_000).await.unwrap();
        assert_eq!((frost[0].setpoint, frost[0].reason), (7.0, "window_open"));
        assert_eq!(h.mock.setpoint_writes(), vec![(5, 18.0), (5, 7.0)]);
        assert_eq!(h.state.storage.last_setpoints().unwrap()[0].reason, "window_open");
        call(&h.state, "POST", "/api/scan", Some(scan_at(now_ms(), json!({})))).await;
        call(&h.state, "POST", "/api/window", Some("{\"room\":\"wohnzimmer\",\"open\":false}".into())).await;
        let boosted = control_step(&h.state, noon, now_ms()).await.unwrap();
        assert_eq!((boosted[0].setpoint, boosted[0].reason), (19.0, "presence_boost"));
    }

    #[tokio::test]
    async fn duplicate_single_is_accepted_but_stored_once() {
        let h = harness(true);
        let first = call(&h.state, "POST", "/api/scan", Some(scan_body())).await;
        assert_eq!(first.1, json!({"stored": 4, "room": "wohnzimmer"}));
        let (status, body) = call(&h.state, "POST", "/api/scan", Some(scan_body())).await;
        assert_eq!((status, body), (StatusCode::OK, json!({"stored": 0, "room": "wohnzimmer", "duplicate": true})));
        assert_eq!((h.state.storage.count("scans").unwrap(), h.state.storage.count("predictions").unwrap(), h.state.storage.count("device_state").unwrap()), (4, 1, 1));
        let other = scan_at(1_761_580_800_000, json!({"id": "pixel-watch-000124"}));
        assert_eq!(call(&h.state, "POST", "/api/scan", Some(other)).await.1["stored"], 4);
        let mut elsewhere: Value = serde_json::from_str(&scan_body()).unwrap();
        elsewhere["device"] = json!("other-watch");
        assert_eq!(call(&h.state, "POST", "/api/scan", Some(elsewhere.to_string())).await.1["stored"], 4);
        assert_eq!(h.state.storage.count("ingest_ids").unwrap(), 3);
    }

    #[tokio::test]
    async fn duplicate_within_batch_and_whole_batch_retry() {
        let h = harness(true);
        let a = serde_json::from_str::<Value>(&scan_at(2_000, json!({"id": "w-1"}))).unwrap();
        let b = serde_json::from_str::<Value>(&scan_at(3_000, json!({"id": "w-2", "aps": []}))).unwrap();
        let batch = json!({"scans": [a.clone(), a.clone(), b]}).to_string();
        let (_, body) = call(&h.state, "POST", "/api/scans", Some(batch.clone())).await;
        assert_eq!(body, json!({"stored": 4, "accepted": 3, "duplicates": 1, "room": "wohnzimmer"}));
        let counts = |h: &Harness| (h.state.storage.count("scans").unwrap(), h.state.storage.count("predictions").unwrap(), h.state.storage.count("device_state").unwrap());
        assert_eq!(counts(&h), (4, 1, 2));
        let (_, body) = call(&h.state, "POST", "/api/scans", Some(batch)).await;
        assert_eq!(body, json!({"stored": 0, "accepted": 3, "duplicates": 3, "room": "wohnzimmer"}));
        assert_eq!(counts(&h), (4, 1, 2));
        let (_, body) = call(&h.state, "POST", "/api/scans", Some(fixture("batch_example.json"))).await;
        assert_eq!(body["duplicates"], 0);
        let (_, body) = call(&h.state, "POST", "/api/scans", Some(fixture("batch_example.json"))).await;
        assert_eq!((body["stored"].clone(), body["duplicates"].clone()), (json!(0), json!(2)));
    }

    #[tokio::test]
    async fn future_ts_is_clamped_to_received_at() {
        let h = harness(true);
        let before = now_ms();
        call(&h.state, "POST", "/api/scan", Some(scan_at(before + 600_000, json!({"motion": "still"})))).await;
        let after = now_ms();
        let st = &h.state.storage.device_statuses().unwrap()[0];
        assert!(st.state_ts >= before && st.state_ts <= after, "{}", st.state_ts);
        let p = &h.state.storage.last_predictions().unwrap()[0];
        assert!(p.ts >= before && p.ts <= after);
        call(&h.state, "POST", "/api/scan", Some(scan_at(after + 120_000, json!({"motion": "still"})))).await;
        let st = &h.state.storage.device_statuses().unwrap()[0];
        assert!(st.state_ts > after + 100_000);
    }

    fn label_body(device: &str, from: i64, to: i64, label: &str) -> Option<String> {
        Some(json!({"device": device, "from": from, "to": to, "label": label}).to_string())
    }

    #[tokio::test]
    async fn label_relabels_range_overwrites_and_is_idempotent() {
        let h = harness(true);
        for ts in [1_000_000, 1_500_000, 3_000_000] {
            call(&h.state, "POST", "/api/scan", Some(scan_at(ts, json!({"label": null})))).await;
        }
        let mut other: Value = serde_json::from_str(&scan_at(1_200_000, json!({"label": null}))).unwrap();
        other["device"] = json!("other-watch");
        call(&h.state, "POST", "/api/scan", Some(other.to_string())).await;
        let (status, body) = call(&h.state, "POST", "/api/label", label_body("pixel-watch", 1_000_000, 1_500_000, "bad")).await;
        assert_eq!((status, body), (StatusCode::OK, json!({"labelled": 2})));
        let again = call(&h.state, "POST", "/api/label", label_body("pixel-watch", 1_000_000, 1_500_000, "bad")).await;
        assert_eq!(again.1, json!({"labelled": 2}));
        let (_, body) = call(&h.state, "POST", "/api/label", label_body("pixel-watch", 1_500_000, 1_500_000, "flur")).await;
        assert_eq!(body, json!({"labelled": 1}));
        let labels = |device: &str| h.state.storage.scan_labels(device).unwrap();
        assert_eq!(
            labels("pixel-watch"),
            vec![(1_000_000, Some("bad".to_string())), (1_500_000, Some("flur".to_string())), (3_000_000, None)]
        );
        assert_eq!(labels("other-watch"), vec![(1_200_000, None)]);
    }

    #[tokio::test]
    async fn label_rejects_bad_input() {
        let h = harness(true);
        for bad in [label_body("pixel-watch", 0, 10, "Bad"), label_body("pixel-watch", 0, 10, ""), label_body("pixel-watch", 0, 10, &"a".repeat(33)), label_body("pixel-watch", 0, 10, "a-b"), label_body("pixel-watch", 10, 0, "bad"), label_body("pixel-watch", 0, 3_600_001, "bad")] {
            let (status, body) = call(&h.state, "POST", "/api/label", bad).await;
            assert_eq!(status, StatusCode::BAD_REQUEST);
            assert!(body["error"].is_string());
        }
        assert_eq!(call(&h.state, "POST", "/api/label", label_body("pixel-watch", 0, 3_600_000, "bad")).await.0, StatusCode::OK);
    }
}
