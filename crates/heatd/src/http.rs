use crate::config::Config;
use crate::infer::RoomModel;
use crate::storage::{now_ms, ScanBody, Storage};
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
    pub model: Option<Arc<RoomModel>>,
    pub controller: Arc<dyn MatterController>,
    pub windows: Arc<Mutex<HashSet<String>>>,
    pub commanded: Arc<Mutex<HashMap<u64, f64>>>,
}

pub struct ApiError(anyhow::Error);

impl<E: Into<anyhow::Error>> From<E> for ApiError {
    fn from(e: E) -> Self {
        Self(e.into())
    }
}

impl IntoResponse for ApiError {
    fn into_response(self) -> Response {
        (StatusCode::INTERNAL_SERVER_ERROR, Json(json!({"error": self.0.to_string()}))).into_response()
    }
}

type ApiResult = Result<Json<Value>, ApiError>;

#[derive(Deserialize)]
struct WindowBody {
    room: String,
    open: bool,
}

pub fn router(state: AppState) -> Router {
    Router::new()
        .route("/health", get(|| async { Json(json!({"status": "ok"})) }))
        .route("/api/scan", post(scan))
        .route("/api/state", get(api_state))
        .route("/api/snapshot", post(snapshot))
        .route("/api/window", post(window))
        .with_state(state)
}

async fn scan(State(state): State<AppState>, Json(body): Json<ScanBody>) -> ApiResult {
    let received = now_ms();
    let stored = state.storage.insert_scan(&body, received)?;
    let mut room = None;
    if let Some(model) = &state.model {
        let (label, probability) = model.predict(&body.aps)?;
        state.storage.insert_prediction(received, &body.device, &label, probability)?;
        room = Some(label);
    }
    Ok(Json(json!({"stored": stored, "room": room})))
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
            config: Arc::new(config),
            model,
            controller: mock.clone(),
            windows: Default::default(),
            commanded: Default::default(),
        };
        Harness { state, mock, _dir: dir }
    }

    async fn call(state: &AppState, method: &str, uri: &str, body: Option<String>) -> (StatusCode, Value) {
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
        call(&h.state, "POST", "/api/scan", Some(scan_body())).await;
        call(&h.state, "POST", "/api/window", Some("{\"room\":\"wohnzimmer\",\"open\":false}".into())).await;
        let boosted = control_step(&h.state, noon, now_ms()).await.unwrap();
        assert_eq!((boosted[0].setpoint, boosted[0].reason), (19.0, "presence_boost"));
    }
}
