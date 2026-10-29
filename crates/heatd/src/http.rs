use crate::config::Config;
use crate::infer::{Explanation, RoomModel};
use crate::explore::Explorer;
use crate::lake::{self, kind, Event, Label, Table, WatchRaw, WatchSensor};
use crate::rules::{presence_event, Tracker};
use crate::storage::{now_ms, AccessPoint, BatchBody, Motion, Power, ScanBody, Storage};
use crate::watchmatch::{Signal, WatchMatch};
use axum::extract::State;
use axum::http::StatusCode;
use axum::response::{IntoResponse, Response};
use axum::routing::{get, post};
use axum::{Json, Router};
use matter_controller::MatterController;
use serde::Deserialize;
use serde_json::{json, Value};

use std::collections::HashMap;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex};

#[derive(Clone)]
pub struct AppState {
    pub config: Arc<Config>,
    pub storage: Arc<Storage>,
    pub sink: crate::lake::Sink,
    pub latest: crate::capture::Latest,
    pub status: crate::status::Status,
    pub model: Option<Arc<RoomModel>>,
    pub controller: Arc<dyn MatterController>,
    pub windows: Arc<Mutex<crate::window::Windows>>,
    pub commanded: Arc<Mutex<crate::rules::Commands>>,
    pub explorer: Arc<Mutex<Explorer>>,
    pub tracker: Arc<Mutex<Tracker>>,
    pub batch_seq: Arc<AtomicU64>,
    pub watchmatch: WatchMatch,
}

impl AppState {
    pub fn next_batch(&self) -> u64 {
        self.batch_seq.fetch_add(1, Ordering::SeqCst)
    }
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
    #[serde(alias = "room")]
    label: String,
    #[serde(default)]
    heat: Option<bool>,
}

#[derive(Deserialize)]
struct MeasurementBody {
    device: String,
    ts: i64,
    #[serde(default)]
    device_ts: Option<i64>,
    #[serde(default)]
    id: Option<String>,
    #[serde(default)]
    batch_id: Option<u64>,
    #[serde(alias = "aps")]
    measurements: Vec<Measurement>,
}

#[derive(Deserialize)]
struct Measurement {
    #[serde(alias = "bssid")]
    sender: String,
    rssi: i32,
    #[serde(default)]
    channel: Option<u8>,
    #[serde(default)]
    freq: Option<u16>,
}

const MAX_LABEL_SPAN_MS: i64 = 3_600_000;

pub fn channel_of(freq: u16) -> Option<u8> {
    match freq {
        2484 => Some(14),
        2412..=2472 => Some(((freq - 2407) / 5) as u8),
        5160..=5885 => Some(((freq - 5000) / 5) as u8),
        5955..=7115 => Some(((freq - 5950) / 5) as u8),
        _ => None,
    }
}

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
        .route("/api/labels/progress", get(labels_progress))
        .route("/watch/measurement", post(watch_measurement))
        .route("/watch/sensors", post(watch_sensors))
        .route("/watch/label", post(label))
        .with_state(state)
}

async fn health(State(state): State<AppState>) -> Json<Value> {
    let mut body = state.status.health(crate::lake::now_us());
    let windows = state.windows.lock().unwrap();
    body["windows"] = state.config.rooms.iter().map(|r| json!({"room": r.name, "window": windows.view(&r.name, now_ms())})).collect();
    Json(body)
}

async fn metrics(State(state): State<AppState>) -> impl IntoResponse {
    ([(axum::http::header::CONTENT_TYPE, "text/plain; version=0.0.4; charset=utf-8")], format!("{}{}", state.status.metrics(crate::lake::now_us()), state.windows.lock().unwrap().metrics()))
}

const MAX_FUTURE_MS: i64 = 300_000;

struct Outcome {
    stored: usize,
    duplicate: bool,
    room: Option<(String, i64)>,
    explanation: Explanation,
    matched: Option<Value>,
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

fn ingest(state: &AppState, body: &ScanBody, received: i64, raw: bool) -> Result<Outcome, ApiError> {
    if state.watchmatch.is_anchor(&body.device) {
        if body.id.as_ref().is_some_and(|id| !state.storage.claim_id(&body.device, id).unwrap_or(true)) {
            return Ok(Outcome { stored: 0, duplicate: true, room: None, explanation: Explanation::default(), matched: None });
        }
        if raw {
            let batch = state.next_batch();
            let aps = body.aps.iter().map(|a| (a.bssid.clone(), a.rssi, None, u16::try_from(a.freq).ok()));
            if let Err(e) = raw_rows(state, &body.device, body.ts.min(received), None, received, batch, aps) {
                eprintln!("watch_raw: {e}");
            }
        }
        return Ok(Outcome { stored: body.aps.len(), duplicate: false, room: None, explanation: Explanation::default(), matched: None });
    }
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
    let matched = if done.duplicate { None } else { record_scan(state, &body, received, room.as_ref(), &explanation, raw) };
    Ok(Outcome { stored: done.stored, duplicate: done.duplicate, room: room.map(|r| (r.0, body.ts)), explanation, matched })
}

fn raw_rows(state: &AppState, device: &str, ts_ms: i64, device_ts: Option<i64>, received_ms: i64, batch_id: u64, aps: impl Iterator<Item = (String, i32, Option<u8>, Option<u16>)>) -> anyhow::Result<usize> {
    let rows: Vec<WatchRaw> = aps
        .map(|(sender, rssi, channel, freq)| WatchRaw {
            ts: ts_ms * 1000,
            device_ts: device_ts.map(|t| t * 1000),
            sender: sender.to_lowercase(),
            rssi: rssi.clamp(i8::MIN as i32, i8::MAX as i32) as i8,
            channel: channel.or_else(|| freq.and_then(channel_of)),
            batch_id,
            device: device.into(),
            freq,
            received_ts: Some(received_ms * 1000),
            sensor_type: Some("rssi".into()),
            value: Some(rssi as f32),
            ..Default::default()
        })
        .collect();
    let n = rows.len();
    state.sink.append_many(rows)?;
    Ok(n)
}

fn record_scan(state: &AppState, body: &ScanBody, received: i64, room: Option<&(String, f64)>, explanation: &Explanation, raw: bool) -> Option<Value> {
    if raw {
        let batch = state.next_batch();
        if let Err(e) = raw_rows(state, &body.device, body.ts, None, received, batch, body.aps.iter().map(|a| (a.bssid.clone(), a.rssi, None, u16::try_from(a.freq).ok()))) {
            eprintln!("watch_raw: {e}");
        }
    }
    let heat = body.heat.unwrap_or(true);
    if let Some(label) = body.label.as_deref().filter(|l| !l.is_empty()) {
        let row = Label { ts: body.ts * 1000, room: label.into(), source: "watch".into(), device: Some(body.device.clone()), heat: Some(heat), ..Default::default() };
        if let Err(e) = state.sink.append(row) {
            eprintln!("labels: {e}");
        }
    }
    if heat && (!body.aps.is_empty() || body.label.as_deref().is_some_and(|l| !l.is_empty())) {
        state.watchmatch.note_stay(&body.device, body.ts, body.label.as_deref());
    }
    let detail = json!({"device": body.device, "received_ts": received * 1000, "motion": body.motion, "power": body.power, "queued": body.queued, "n_aps": body.aps.len(), "away": body.away, "bedtime": body.bedtime, "id": body.id});
    let mut state_event = Event::now(kind::WATCH_STATE, "watch").with_detail(detail);
    state_event.ts = body.ts * 1000;
    if let Err(e) = state.sink.append(state_event) {
        eprintln!("watch_state: {e}");
    }
    if let Some((label, probability)) = room {
        let probs: serde_json::Map<String, Value> = explanation.rooms.iter().map(|r| (r.room.clone(), json!(r.p))).collect();
        let model_id = std::path::Path::new(&state.config.model_path).file_name().map(|f| f.to_string_lossy().into_owned());
        let row = Label {
            ts: body.ts * 1000,
            room: label.clone(),
            source: "model".into(),
            device: Some(body.device.clone()),
            confidence: Some(*probability as f32),
            model_id,
            probs: (!probs.is_empty()).then(|| Value::Object(probs).to_string()),
            ..Default::default()
        };
        if let Err(e) = state.sink.append(row) {
            eprintln!("labels: {e}");
        }
    }
    if let Some(power) = body.power {
        state.watchmatch.signals(&body.device, [(body.ts, Signal::OnBody(power != Power::Offbody))]);
    }
    let pairs: Vec<(String, i32)> = body.aps.iter().map(|a| (a.bssid.clone(), a.rssi)).collect();
    let matched = state.watchmatch.scan_match(&body.device, body.ts, received, &pairs);
    let transition = state.tracker.lock().unwrap().observe(&body.device, body.ts, body.away, body.motion, body.power);
    if let Some(t) = transition {
        if let Err(e) = state.sink.append(presence_event(&body.device, t)) {
            eprintln!("presence event: {e}");
        }
    }
    matched
}

const SENSOR_TYPES: [&str; 7] = ["rssi", "temp", "hr", "offbody", "motion", "battery", "charging"];

#[derive(Deserialize)]
struct LongBody {
    device: String,
    batch_id: Value,
    #[serde(default)]
    clock_offset_ms: Option<i64>,
    samples: Vec<Sample>,
    #[serde(default)]
    id: Option<String>,
    #[serde(default)]
    label: Option<String>,
    #[serde(default)]
    motion: Option<Motion>,
    #[serde(default)]
    power: Option<Power>,
    #[serde(default)]
    away: Option<bool>,
    #[serde(default)]
    bedtime: Option<bool>,
    #[serde(default)]
    heat: Option<bool>,
    #[serde(default)]
    queued: Option<bool>,
}

impl LongBody {
    fn scan_body(&self, received: i64) -> Option<ScanBody> {
        let carries = self.id.is_some() || self.label.is_some() || self.motion.is_some() || self.power.is_some() || self.away.is_some() || self.bedtime.is_some() || self.queued.is_some();
        let rssi: Vec<&Sample> = self.samples.iter().filter(|s| s.sensor_type == "rssi").collect();
        let ts = rssi.iter().map(|s| s.ts.min(received + MAX_FUTURE_MS)).max()?;
        carries.then(|| ScanBody {
            ts,
            device: self.device.clone(),
            id: self.id.clone(),
            label: self.label.clone(),
            aps: rssi.iter().map(|s| AccessPoint { bssid: s.sender.clone().unwrap_or_default().to_lowercase(), rssi: s.value.round() as i32, freq: s.freq.map_or(0, i32::from) }).collect(),
            motion: self.motion,
            queued: self.queued.unwrap_or(false),
            power: self.power,
            away: self.away.unwrap_or(false),
            bedtime: self.bedtime,
            heat: self.heat,
        })
    }
}

#[derive(Deserialize)]
struct Sample {
    #[serde(default)]
    elapsed_ns: Option<i64>,
    ts: i64,
    sensor_type: String,
    #[serde(default)]
    sensor_name: Option<String>,
    value: f64,
    #[serde(default)]
    unit: Option<String>,
    #[serde(default)]
    sender: Option<String>,
    #[serde(default)]
    channel: Option<u8>,
    #[serde(default)]
    freq: Option<u16>,
}

fn batch_number(id: &Value) -> Result<(u64, String), ApiError> {
    match id {
        Value::Number(n) => n.as_u64().map(|v| (v, v.to_string())).ok_or_else(|| ApiError::bad("batch_id must be a non-negative integer or a string")),
        Value::String(t) if !t.is_empty() => Ok((t.parse().unwrap_or_else(|_| crate::watchmatch::hash64(t)), t.clone())),
        _ => Err(ApiError::bad("batch_id must be a non-negative integer or a string")),
    }
}

fn long_rows(body: &LongBody, batch: u64, received: i64) -> Result<Vec<WatchRaw>, ApiError> {
    body.samples
        .iter()
        .map(|s| {
            if !SENSOR_TYPES.contains(&s.sensor_type.as_str()) {
                return Err(ApiError::bad(&format!("unknown sensor_type {} (expected {})", s.sensor_type, SENSOR_TYPES.join(", "))));
            }
            let rssi = s.sensor_type == "rssi";
            let sender = s.sender.as_deref().map(str::to_lowercase).filter(|x| !x.is_empty());
            if rssi && sender.is_none() {
                return Err(ApiError::bad("rssi samples need a sender"));
            }
            if !s.value.is_finite() {
                return Err(ApiError::bad("value must be finite"));
            }
            Ok(WatchRaw {
                ts: s.ts.min(received + MAX_FUTURE_MS) * 1000,
                sender: sender.unwrap_or_default(),
                rssi: if rssi { s.value.round().clamp(i8::MIN as f64, i8::MAX as f64) as i8 } else { 0 },
                channel: s.channel.or_else(|| s.freq.and_then(channel_of)),
                batch_id: batch,
                device: body.device.clone(),
                freq: s.freq,
                received_ts: Some(received * 1000),
                elapsed_ns: s.elapsed_ns,
                clock_offset_ms: body.clock_offset_ms.map(|v| v.clamp(i32::MIN as i64, i32::MAX as i64) as i32),
                sensor_type: Some(s.sensor_type.clone()),
                sensor_name: s.sensor_name.clone(),
                value: Some(s.value as f32),
                unit: s.unit.clone(),
                ..Default::default()
            })
        })
        .collect()
}

fn feed_matcher(state: &AppState, body: &LongBody, received: i64, scans_fed: bool) -> Option<Value> {
    let ts = |s: &Sample| s.ts.min(received + MAX_FUTURE_MS);
    let charging: std::collections::BTreeMap<i64, bool> = body.samples.iter().filter(|s| s.sensor_type == "charging").map(|s| (ts(s), s.value >= 0.5)).collect();
    let mut signals = Vec::new();
    let mut scans: std::collections::BTreeMap<i64, Vec<(String, i32)>> = Default::default();
    let mut counts: std::collections::BTreeMap<&str, u64> = Default::default();
    for s in &body.samples {
        *counts.entry(s.sensor_type.as_str()).or_default() += 1;
        match s.sensor_type.as_str() {
            "rssi" => scans.entry(ts(s)).or_default().push((s.sender.clone().unwrap_or_default(), s.value.round() as i32)),
            "hr" => signals.push((ts(s), Signal::Hr(s.value))),
            "motion" => signals.push((ts(s), Signal::Motion(s.value))),
            "offbody" => signals.push((ts(s), Signal::OnBody(s.value >= 0.5))),
            "charging" => signals.push((ts(s), Signal::Charging(s.value >= 0.5))),
            "battery" => state.status.watch_battery(ts(s) * 1000, s.value, charging.range(..=ts(s)).next_back().is_some_and(|c| *c.1)),
            _ => {}
        }
    }
    counts.iter().for_each(|(t, n)| state.status.watch_samples(t, *n));
    state.watchmatch.signals(&body.device, signals);
    if scans_fed {
        return None;
    }
    scans.into_iter().filter_map(|(t, aps)| state.watchmatch.scan_match(&body.device, t, received, &aps)).collect::<Vec<_>>().pop()
}

async fn flushed(state: &AppState, table: Table) -> Result<(), ApiError> {
    let sink = state.sink.clone();
    tokio::task::spawn_blocking(move || sink.flush_table(table)).await.map_err(anyhow::Error::from)??;
    Ok(())
}

async fn watch_long(state: AppState, body: LongBody) -> ApiResult {
    let received = now_ms();
    let (batch, key) = batch_number(&body.batch_id)?;
    let key = format!("batch:{key}");
    if state.storage.seen_id(&body.device, &key)? {
        return Ok(Json(json!({"stored": 0, "duplicate": true, "batch_id": batch})));
    }
    let rows = long_rows(&body, batch, received)?;
    let stored = rows.len();
    if state.watchmatch.is_anchor(&body.device) {
        state.sink.append_many(rows)?;
        flushed(&state, Table::WatchRaw).await?;
        state.storage.claim_id(&body.device, &key)?;
        return Ok(Json(json!({"stored": stored, "batch_id": batch, "anchor": true})));
    }
    let scan = body.scan_body(received).map(|s| ingest(&state, &s, received, false)).transpose()?;
    state.sink.append_many(rows)?;
    flushed(&state, Table::WatchRaw).await?;
    state.storage.claim_id(&body.device, &key)?;
    let fed = feed_matcher(&state, &body, received, scan.is_some());
    let mut response = json!({"stored": stored, "batch_id": batch});
    if let Some(m) = scan.as_ref().and_then(|o| o.matched.clone()).or(fed) {
        response["match"] = m;
    }
    if let Some(out) = &scan {
        response["room"] = json!(out.room.as_ref().map(|r| r.0.clone()));
        merge(&mut response, attribution(Some(out)));
        if out.duplicate {
            response["duplicate"] = json!(true);
        }
    }
    Ok(Json(response))
}

async fn watch_measurement(State(state): State<AppState>, Json(value): Json<Value>) -> ApiResult {
    if value.get("samples").is_some() {
        let body: LongBody = serde_json::from_value(value).map_err(|e| ApiError(StatusCode::UNPROCESSABLE_ENTITY, e.into()))?;
        return watch_long(state, body).await;
    }
    let body: MeasurementBody = serde_json::from_value(value).map_err(|e| ApiError(StatusCode::UNPROCESSABLE_ENTITY, e.into()))?;
    let received = now_ms();
    if let Some(id) = &body.id {
        if state.storage.seen_id(&body.device, id)? {
            return Ok(Json(json!({"stored": 0, "duplicate": true})));
        }
    }
    let batch = body.batch_id.unwrap_or_else(|| state.next_batch());
    let ts = body.ts.min(received + MAX_FUTURE_MS);
    let pairs: Vec<(String, i32)> = body.measurements.iter().map(|m| (m.sender.clone(), m.rssi)).collect();
    let aps = body.measurements.into_iter().map(|m| (m.sender, m.rssi, m.channel, m.freq));
    let stored = raw_rows(&state, &body.device, ts, body.device_ts, received, batch, aps)?;
    flushed(&state, Table::WatchRaw).await?;
    if let Some(id) = &body.id {
        state.storage.claim_id(&body.device, id)?;
    }
    let mut response = json!({"stored": stored, "batch_id": batch});
    if state.watchmatch.is_anchor(&body.device) {
        response["anchor"] = json!(true);
        return Ok(Json(response));
    }
    state.status.watch_samples("rssi", stored as u64);
    if let Some(m) = state.watchmatch.scan_match(&body.device, ts, received, &pairs) {
        response["match"] = m;
    }
    Ok(Json(response))
}

#[derive(Deserialize)]
struct SensorsBody {
    #[serde(default)]
    ts: Option<i64>,
    #[serde(default)]
    app_version: Option<String>,
    sensors: Vec<SensorBody>,
}

#[derive(Deserialize)]
struct SensorBody {
    #[serde(alias = "name")]
    sensor_name: String,
    #[serde(alias = "type")]
    android_type: i32,
    #[serde(default)]
    vendor: Option<String>,
    #[serde(default)]
    resolution: Option<f32>,
    #[serde(default, alias = "maxRange")]
    max_range: Option<f32>,
    #[serde(default)]
    app_version: Option<String>,
}

async fn watch_sensors(State(state): State<AppState>, Json(body): Json<SensorsBody>) -> ApiResult {
    if body.sensors.is_empty() {
        return Err(ApiError::bad("sensors must not be empty"));
    }
    let ts = body.ts.unwrap_or_else(now_ms).min(now_ms() + MAX_FUTURE_MS) * 1000;
    let rows: Vec<WatchSensor> = body
        .sensors
        .into_iter()
        .map(|s| WatchSensor { ts, sensor_name: s.sensor_name, android_type: s.android_type, vendor: s.vendor, resolution: s.resolution, max_range: s.max_range, app_version: s.app_version.or_else(|| body.app_version.clone()) })
        .collect();
    let stored = rows.len();
    state.sink.append_many(rows)?;
    flushed(&state, Table::WatchSensors).await?;
    Ok(Json(json!({"stored": stored})))
}

async fn scan(State(state): State<AppState>, Json(body): Json<ScanBody>) -> ApiResult {
    let out = ingest(&state, &body, now_ms(), true)?;
    let mut response = json!({"stored": out.stored, "room": out.room.as_ref().map(|r| r.0.clone())});
    merge(&mut response, attribution(Some(&out)));
    if let Some(m) = out.matched {
        response["match"] = m;
    }
    if out.duplicate {
        response["duplicate"] = json!(true);
    }
    Ok(Json(response))
}

async fn scans(State(state): State<AppState>, Json(body): Json<BatchBody>) -> ApiResult {
    let received = now_ms();
    let (mut stored, mut duplicates) = (0, 0);
    let mut latest: Option<Outcome> = None;
    let mut matched: Option<(i64, Value)> = None;
    for scan in &body.scans {
        let out = ingest(&state, scan, received, true)?;
        if let Some(m) = out.matched.clone().filter(|_| matched.as_ref().is_none_or(|l| scan.ts >= l.0)) {
            matched = Some((scan.ts, m));
        }
        stored += out.stored;
        duplicates += usize::from(out.duplicate);
        if out.room.as_ref().is_some_and(|r| latest.as_ref().and_then(|l| l.room.as_ref()).is_none_or(|l| r.1 >= l.1)) {
            latest = Some(out);
        }
    }
    let mut response = json!({"stored": stored, "accepted": body.scans.len(), "duplicates": duplicates, "room": latest.as_ref().and_then(|o| o.room.as_ref().map(|r| r.0.clone()))});
    merge(&mut response, attribution(latest.as_ref()));
    if let Some((_, m)) = matched {
        response["match"] = m;
    }
    Ok(Json(response))
}

async fn api_state(State(state): State<AppState>) -> ApiResult {
    let readings = state.latest.all();
    let setpoints = state.storage.last_setpoints()?;
    let windows = state.windows.lock().unwrap();
    let open = windows.open_rooms();
    let commands = state.commanded.lock().unwrap();
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
                        "commanded_setpoint": commands.target(node),
                        "manual_hold": commands.hold(node).map(|h| json!({"value": h.value, "since_ts": h.since_ms})),
                        "reason": setpoint.map(|s| s.reason.clone()),
                        "setpoint_ts": setpoint.map(|s| s.ts),
                    })
                })
                .collect();
            json!({"name": room.name, "window_open": open.contains(&room.name), "window": windows.view(&room.name, now_ms()), "nodes": nodes})
        })
        .collect();
    Ok(Json(json!({"rooms": rooms, "devices": state.storage.last_predictions()?})))
}

async fn snapshot(State(state): State<AppState>) -> ApiResult {
    state.storage.snapshot(&state.config.snapshot_path)?;
    Ok(Json(json!({"path": state.config.snapshot_path})))
}

async fn window(State(state): State<AppState>, Json(body): Json<WindowBody>) -> ApiResult {
    let event = state.windows.lock().unwrap().manual(&body.room, body.open, now_ms());
    event.into_iter().for_each(|e| crate::rules::window_event(&state, e));
    let open = state.windows.lock().unwrap().open_rooms();
    Ok(Json(json!({"window_open": open.iter().collect::<Vec<_>>()})))
}

const LABEL_DEDUP_US: i64 = 35_000_000;

fn label_counts(state: &AppState, since_us: i64) -> anyhow::Result<HashMap<String, usize>> {
    use arrow::array::Array;
    state.sink.flush()?;
    let max_span_us = 2 * state.config.watchmatch.label_tolerance_s * 1_000_000;
    let mut rows: Vec<(i64, String)> = Vec::new();
    for batch in crate::context::read_raw(state.sink.root(), Table::Labels, None)? {
        let (ts, from, to) = (crate::context::ts_column(&batch, "ts")?, crate::context::ts_column(&batch, "from_ts")?, crate::context::ts_column(&batch, "to_ts")?);
        let (room, source) = (crate::context::text_column(&batch, "room")?, crate::context::text_column(&batch, "source")?);
        let at = |i: usize| if to.is_null(i) { ts.value(i) } else { to.value(i) };
        let retroactive = |i: usize| !from.is_null(i) && !to.is_null(i) && to.value(i) - from.value(i) > max_span_us;
        rows.extend((0..batch.num_rows()).filter(|&i| source.value(i) == "watch" && at(i) >= since_us && !retroactive(i)).map(|i| (at(i), room.value(i).to_string())));
    }
    rows.sort();
    let mut last: HashMap<String, i64> = HashMap::new();
    let mut counts = HashMap::new();
    for (ts, room) in rows {
        if last.get(&room).is_none_or(|&t| ts - t > LABEL_DEDUP_US) {
            *counts.entry(room.clone()).or_insert(0) += 1;
        }
        last.insert(room, ts);
    }
    Ok(counts)
}

async fn labels_progress(State(state): State<AppState>) -> ApiResult {
    let cfg = &state.config.watchmatch;
    let since_us = cfg.labels_since.as_deref().and_then(|s| chrono::DateTime::parse_from_rfc3339(s).ok()).map_or(0, |t| t.timestamp_micros());
    let counts = label_counts(&state, since_us)?;
    let model_rooms = state.watchmatch.model_rooms();
    let rooms: Vec<Value> = state.config.rooms.iter().filter(|r| !r.nodes.is_empty() || r.name == "flur" || model_rooms.contains(&r.name)).map(|r| json!({"room": r.name, "n": counts.get(&r.name).copied().unwrap_or(0)})).collect();
    let done = !rooms.is_empty() && rooms.iter().all(|r| r["n"].as_u64().unwrap_or(0) >= cfg.label_target as u64);
    Ok(Json(json!({"target": cfg.label_target, "since": cfg.labels_since, "rooms": rooms, "done": done})))
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
    let heat = body.heat.unwrap_or(true);
    if heat {
        state.watchmatch.note_stay(&body.device, body.to.min(now_ms()), Some(&body.label));
    }
    let row = Label { ts: lake::now_us(), from_ts: Some(body.from * 1000), to_ts: Some(body.to * 1000), room: body.label.clone(), source: "watch".into(), device: Some(body.device.clone()), heat: Some(heat), ..Default::default() };
    if let Err(e) = state.sink.append(row) {
        eprintln!("labels: {e}");
    }
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
        harness_with(with_model, "away_guard_minutes = 0\n", "[[rooms]]\nname = 'wohnzimmer'\nnodes = [5]\n")
    }

    fn harness_with(with_model: bool, head: &str, rooms: &str) -> Harness {
        let dir = tempfile::tempdir().unwrap();
        let toml = format!(
            "db_path = '{0}/live.duckdb'\nsnapshot_path = '{0}/out/snapshot.duckdb'\ndefault_setpoint = 18.0\n{head}{rooms}",
            dir.path().to_str().unwrap().replace('\\', "/")
        );
        let config = Config::parse(&toml).unwrap();
        let mock = Arc::new(MockController::new(1));
        mock.add_node(5, 19.0, 19.0);
        let model = with_model.then(|| Arc::new(RoomModel::load(&Path::new(FIXTURES).join("room_model.onnx")).unwrap()));
        let sink = crate::lake::Sink::open(crate::lake::LakeConfig::new(dir.path().join("lake"))).unwrap();
        let status = crate::status::Status::new(crate::lake::now_us());
        let state = AppState {
            watchmatch: WatchMatch::new(&config.watchmatch, sink.clone(), status.clone()),
            storage: Arc::new(Storage::open(&config.db_path).unwrap()),
            sink: sink.clone(),
            latest: Default::default(),
            status: status.clone(),
            config: Arc::new(config),
            model,
            controller: mock.clone(),
            windows: Default::default(),
            commanded: Default::default(),
            explorer: Arc::new(Mutex::new(crate::explore::Explorer::new(1))),
            tracker: Default::default(),
            batch_seq: Arc::new(AtomicU64::new(1)),
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
        let h = harness_with(true, "away_guard_minutes = 0
occupancy_enter_min = 0
", "[[rooms]]
name = 'wohnzimmer'
nodes = [5]
");
        let noon = NaiveDate::from_ymd_opt(2025, 10, 27).unwrap().and_hms_opt(12, 0, 0).unwrap();
        let now = now_ms();
        let old = now - 3 * 3_600_000;
        call(&h.state, "POST", "/api/scan", Some(scan_at(old, json!({"motion": "still"})))).await;
        call(&h.state, "POST", "/api/scan", Some(json!({"ts": now, "device": "pixel-watch", "motion": "still", "power": "doze"}).to_string())).await;
        let boosted = control_step(&h.state, noon, now).await.unwrap();
        assert_eq!((boosted[0].setpoint, boosted[0].reason), (21.0, "occupied"));
        call(&h.state, "POST", "/api/scan", Some(json!({"ts": now + 1, "device": "pixel-watch", "motion": "moving", "away": true}).to_string())).await;
        let eco = control_step(&h.state, noon, now + 1).await.unwrap();
        assert_eq!((eco[0].setpoint, eco[0].reason), (18.0, "away"));
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
        let h = harness_with(true, "away_guard_minutes = 0
occupancy_enter_min = 0
", "[[rooms]]
name = 'wohnzimmer'
nodes = [5]
");
        let noon = NaiveDate::from_ymd_opt(2025, 10, 27).unwrap().and_hms_opt(12, 0, 0).unwrap();
        let first = control_step(&h.state, noon, 1_000).await.unwrap();
        assert_eq!((first.len(), first[0].setpoint, first[0].reason), (1, 18.0, "default"));
        assert!(control_step(&h.state, noon, 2_000).await.unwrap().is_empty());
        call(&h.state, "POST", "/api/window", Some("{\"room\":\"wohnzimmer\",\"open\":true}".into())).await;
        let frost = control_step(&h.state, noon, 3_000).await.unwrap();
        assert_eq!((frost[0].setpoint, frost[0].reason), (7.5, "window_open"));
        assert_eq!(h.mock.setpoint_writes(), vec![(5, 18.0), (5, 7.5)]);
        assert_eq!(h.state.storage.last_setpoints().unwrap()[0].reason, "window_open");
        call(&h.state, "POST", "/api/scan", Some(scan_at(now_ms(), json!({})))).await;
        call(&h.state, "POST", "/api/window", Some("{\"room\":\"wohnzimmer\",\"open\":false}".into())).await;
        let boosted = control_step(&h.state, noon, now_ms()).await.unwrap();
        assert_eq!((boosted[0].setpoint, boosted[0].reason), (21.0, "occupied"));
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
    async fn label_progress_counts_since_collapses_duplicates_and_reports_done() {
        let since = now_ms() - 3_600_000;
        let iso = chrono::DateTime::from_timestamp_millis(since).unwrap().to_rfc3339();
        let h = harness_with(false, "away_guard_minutes = 0
", &format!("[[rooms]]
name = 'wohnzimmer'
nodes = [5]
[[rooms]]
name = 'bad'
nodes = [7]
[[rooms]]
name = 'flur'
[[rooms]]
name = 'diele'
[watchmatch]
labels_since = '{iso}'
label_target = 2
"));
        let (status, body) = call(&h.state, "GET", "/api/labels/progress", None).await;
        assert_eq!(status, StatusCode::OK);
        assert_eq!((body["target"].clone(), body["since"].clone(), body["done"].clone()), (json!(2), json!(iso), json!(false)));
        assert_eq!(body["rooms"], json!([{"room": "wohnzimmer", "n": 0}, {"room": "bad", "n": 0}, {"room": "flur", "n": 0}]));
        for (offset, room) in [(-60_000, "bad"), (1_000, "bad"), (2_000, "bad"), (50_000, "bad"), (1_000, "wohnzimmer"), (20_000, "flur"), (30_000, "diele")] {
            call(&h.state, "POST", "/api/scan", Some(scan_at(since + offset, json!({"label": room})))).await;
        }
        let (_, body) = call(&h.state, "GET", "/api/labels/progress", None).await;
        assert_eq!(body["rooms"], json!([{"room": "wohnzimmer", "n": 1}, {"room": "bad", "n": 2}, {"room": "flur", "n": 1}]));
        assert_eq!(body["done"], json!(false));
        for (offset, room) in [(40_000, "wohnzimmer"), (60_000, "flur")] {
            call(&h.state, "POST", "/api/scan", Some(scan_at(since + offset, json!({"label": room})))).await;
        }
        let (_, body) = call(&h.state, "GET", "/api/labels/progress", None).await;
        assert_eq!(body["done"], json!(true));
    }

    #[tokio::test]
    async fn label_progress_counts_one_per_tap_on_to_ts_and_ignores_retroactive_ranges() {
        let now = now_ms();
        let iso = chrono::DateTime::from_timestamp_millis(now - 3_600_000).unwrap().to_rfc3339();
        let h = harness_with(false, "away_guard_minutes = 0
", &format!("[[rooms]]
name = 'bad'
nodes = [7]
[[rooms]]
name = 'flur'
nodes = [8]
[watchmatch]
labels_since = '{iso}'
label_target = 2
"));
        let tap = |room: &str, at: i64, half: i64| call(&h.state, "POST", "/watch/label", label_body("pixel-watch", at - half, at + half, room));
        tap("bad", now - 1_800_000, 1_000).await;
        tap("bad", now - 1_800_000 + 20_000, 1_000).await;
        tap("bad", now - 1_800_000 + 25_000, 1_000).await;
        tap("bad", now - 1_000_000, 1_000).await;
        tap("flur", now - 900_000, 100_000).await;
        tap("flur", now - 800_000, 1_000).await;
        tap("flur", now - 7_200_000, 1_000).await;
        let (_, body) = call(&h.state, "GET", "/api/labels/progress", None).await;
        assert_eq!(body["rooms"], json!([{"room": "bad", "n": 2}, {"room": "flur", "n": 1}]));
    }

    #[tokio::test]
    async fn heat_false_labels_are_stored_but_never_occupy_or_bypass() {
        let h = stay_harness();
        let now = now_ms();
        let heat_off = |extra: Value| {
            let mut v = json!({"device": "pixel-watch", "heat": false});
            extra.as_object().unwrap().iter().for_each(|(k, x)| v[k] = x.clone());
            v.to_string()
        };
        assert_eq!(call(&h.state, "POST", "/api/label", Some(heat_off(json!({"from": now - 2_000, "to": now, "label": "wohnzimmer"})))).await.0, StatusCode::OK);
        assert_eq!(call(&h.state, "POST", "/watch/label", Some(heat_off(json!({"from": now - 2_000, "to": now, "room": "wohnzimmer"})))).await.0, StatusCode::OK);
        call(&h.state, "POST", "/api/scan", Some(heat_off(json!({"ts": now, "label": "wohnzimmer", "aps": [{"bssid": "aa:bb:cc:dd:ee:ff", "rssi": -50, "freq": 2412}]})))).await;
        assert!(h.state.watchmatch.stay().is_none());
        assert_eq!(wanted(&h, now)[0].2, "default");
        let labels = lake_rows(&h, "labels");
        assert_eq!(some(&column(&labels, "heat")), vec!["false".to_string(); 3]);
        call(&h.state, "POST", "/api/label", label_body("pixel-watch", now - 2_000, now, "wohnzimmer")).await;
        assert_eq!(wanted(&h, now)[0].2, "occupied");
        assert_eq!(some(&column(&lake_rows(&h, "labels"), "heat"))[3], "true");
    }

    #[tokio::test]
    async fn anchor_scans_land_in_watch_raw_and_touch_nothing_else() {
        let head = format!("away_guard_minutes = 0
[watchmatch]
enabled = true
model_dir = '{}'
scan_fresh_s = 600
anchor_devices = ['minix-anchor']
", fixture_models().replace('\\', "/"));
        let h = harness_with(false, &head, "[[rooms]]
name = 'wohnzimmer'
nodes = [5]
[[rooms]]
name = 'schlafzimmer'
nodes = [6]
");
        let now = now_ms();
        let aps = json!([{"bssid": "aa:bb:cc:00:00:01", "rssi": -40, "freq": 2437}]);
        let (status, body) = call(&h.state, "POST", "/api/scan", Some(json!({"ts": now, "device": "minix-anchor", "label": "wohnzimmer", "id": "a-1", "aps": aps}).to_string())).await;
        assert_eq!((status, body["stored"].clone()), (StatusCode::OK, json!(1)));
        assert_eq!(call(&h.state, "POST", "/api/scan", Some(json!({"ts": now, "device": "minix-anchor", "id": "a-1", "aps": aps}).to_string())).await.1["duplicate"], json!(true));
        let mut long = long_body(77, now + 1000, [-40, -60, -90]);
        long["device"] = json!("minix-anchor");
        long["label"] = json!("wohnzimmer");
        assert_eq!(call(&h.state, "POST", "/watch/measurement", Some(long.to_string())).await.1["anchor"], json!(true));
        let short = json!({"device": "minix-anchor", "ts": now + 2000, "measurements": [{"sender": "aa:bb:cc:00:00:02", "rssi": -55}]});
        assert!(call(&h.state, "POST", "/watch/measurement", Some(short.to_string())).await.1.get("match").is_none());
        let raw = lake_rows(&h, "watch_raw");
        assert_eq!(some(&column(&raw, "device")).iter().filter(|d| *d == "minix-anchor").count(), raw.iter().map(|b| b.num_rows()).sum::<usize>());
        assert!(raw.iter().map(|b| b.num_rows()).sum::<usize>() >= 8);
        assert!(lake_rows(&h, "watch_inference").is_empty() && lake_rows(&h, "labels").is_empty());
        assert!(h.state.storage.device_statuses().unwrap().is_empty() && h.state.storage.last_predictions().unwrap().is_empty());
        assert_eq!(h.state.storage.count("scans").unwrap(), 0);
        assert!(h.state.watchmatch.stay().is_none() && h.state.watchmatch.effective(now_ms()).unwrap().is_none());
        assert!(!h.state.tracker.lock().unwrap().confirmed("minix-anchor"));
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

    fn lake_rows(h: &Harness, table: &str) -> Vec<arrow::record_batch::RecordBatch> {
        h.state.sink.flush().unwrap();
        let dir = h._dir.path().join("lake/raw").join(table);
        let mut files: Vec<_> = std::fs::read_dir(dir).map(|d| d.flatten().map(|e| e.path()).collect()).unwrap_or_default();
        files.sort();
        files.iter().flat_map(|f| lake::load_stream(f).unwrap().batches).collect()
    }

    fn column(batches: &[arrow::record_batch::RecordBatch], name: &str) -> Vec<Option<String>> {
        use arrow::array::{Array, StringArray};
        batches
            .iter()
            .flat_map(|b| {
                let (parent, field) = name.split_once('.').unwrap_or(("", name));
                let col = if parent.is_empty() { b.column_by_name(field).unwrap().clone() } else { b.column_by_name(parent).unwrap().as_any().downcast_ref::<arrow::array::StructArray>().unwrap().column_by_name(field).unwrap().clone() };
                let col = if matches!(col.data_type(), arrow::datatypes::DataType::Timestamp(..)) { arrow::compute::cast(&col, &arrow::datatypes::DataType::Int64).unwrap() } else { col };
                let text = arrow::compute::cast(&col, &arrow::datatypes::DataType::Utf8).unwrap();
                let text = text.as_any().downcast_ref::<StringArray>().unwrap();
                (0..text.len()).map(|i| (!text.is_null(i)).then(|| text.value(i).to_string())).collect::<Vec<_>>()
            })
            .collect()
    }

    fn some(v: &[Option<String>]) -> Vec<String> {
        v.iter().map(|s| s.clone().unwrap_or_else(|| "-".into())).collect()
    }

    fn rooms_config(extra_room: &str) -> Harness {
        harness_with(false, "", &format!("[[rooms]]\nname = 'a'\nnodes = [5]\n[[rooms]]\nname = 'b'\nnodes = [6]\n{extra_room}"))
    }

    #[tokio::test]
    async fn decision_is_persisted_before_write_and_followed_by_result_row() {
        let h = harness(false);
        let noon = NaiveDate::from_ymd_opt(2025, 10, 27).unwrap().and_hms_opt(12, 0, 0).unwrap();
        control_step(&h.state, noon, 1_700_000_000_000).await.unwrap();
        let d = lake_rows(&h, "decisions");
        assert_eq!((some(&column(&d, "rule")), some(&column(&d, "applied")), some(&column(&d, "proposed"))), (vec!["schedule".to_string(); 2], vec!["false".to_string(), "true".into()], vec!["true".to_string(); 2]));
        assert_eq!(some(&column(&d, "setpoint_new")), vec!["18.0".to_string(); 2]);
        assert_eq!(some(&column(&d, "room")), vec!["wohnzimmer".to_string(); 2]);
        assert_eq!(some(&column(&d, "exploration")), vec!["false".to_string(); 2]);
        assert_eq!(some(&column(&d, "inputs.occupancy")), vec!["unknown".to_string(); 2]);
        assert_eq!(some(&column(&d, "inputs.schedule_setpoint")), vec!["18.0".to_string(); 2]);
        assert_eq!(some(&column(&d, "inputs.frost_setpoint")), vec!["7.0".to_string(); 2]);
        assert_eq!(some(&column(&d, "commit")), vec![lake::COMMIT.to_string(); 2]);
        let extra = column(&d, "inputs.extra");
        assert!(extra[0].as_ref().unwrap().contains("\"proposal\"") && extra[1].as_ref().unwrap().contains("\"result\""));
        control_step(&h.state, noon, 1_700_000_060_000).await.unwrap();
        assert_eq!(column(&lake_rows(&h, "decisions"), "rule").len(), 2);
    }

    #[tokio::test]
    async fn failed_write_is_logged_and_does_not_abort_other_rooms() {
        let h = rooms_config("");
        let noon = NaiveDate::from_ymd_opt(2025, 10, 27).unwrap().and_hms_opt(12, 0, 0).unwrap();
        let written = control_step(&h.state, noon, 1_000).await.unwrap();
        assert_eq!(written.iter().map(|w| w.node).collect::<Vec<_>>(), vec![5]);
        let d = lake_rows(&h, "decisions");
        let by_node: Vec<(String, String)> = some(&column(&d, "node")).into_iter().zip(some(&column(&d, "applied"))).collect();
        assert_eq!(by_node, vec![("5".into(), "false".into()), ("5".into(), "true".into()), ("6".into(), "false".into()), ("6".into(), "false".into())]);
        assert!(column(&d, "inputs.extra")[3].as_ref().unwrap().contains("error"));
        assert!(h.state.commanded.lock().unwrap().target(6).is_none());
        let retried = control_step(&h.state, noon, 2_000).await.unwrap();
        assert!(retried.is_empty());
    }

    #[tokio::test]
    async fn sink_failure_does_not_abort_tick() {
        let mut h = rooms_config("");
        h.mock.add_node(6, 19.0, 19.0);
        h.state.sink = lake::Sink::open(lake::LakeConfig { flush_rows: 1, ..lake::LakeConfig::new(h._dir.path().join("broken")) }).unwrap();
        std::fs::remove_dir_all(h._dir.path().join("broken/raw/decisions")).unwrap();
        std::fs::write(h._dir.path().join("broken/raw/decisions"), b"not a dir").unwrap();
        let noon = NaiveDate::from_ymd_opt(2025, 10, 27).unwrap().and_hms_opt(12, 0, 0).unwrap();
        let written = control_step(&h.state, noon, 1_000).await.unwrap();
        assert_eq!(written.len(), 2);
    }

    fn explore_room() -> &'static str {
        "[rooms.exploration]\nenabled = true\np_per_day = 1.0\ncomfort_min = 16.0\ncomfort_max = 22.0\n"
    }

    fn three_rooms() -> Harness {
        let rooms = format!("[[rooms]]
name = 'a'
nodes = [5]
{0}[[rooms]]
name = 'b'
nodes = [6]
{0}blackout = [{{ from = '22:00', to = '07:00' }}]
[[rooms]]
name = 'c'
nodes = [7]
", explore_room());
        let h = harness_with(false, "exploration_seed = 11
", &rooms);
        h.mock.add_node(6, 19.0, 19.0);
        h.mock.add_node(7, 19.0, 19.0);
        h
    }

    #[tokio::test]
    async fn simulated_day_runs_marked_experiments_for_every_enabled_room() {
        let h = three_rooms();
        let start = NaiveDate::from_ymd_opt(2025, 10, 27).unwrap().and_hms_opt(0, 0, 0).unwrap();
        for m in (0..1440 + 240).step_by(10) {
            control_step(&h.state, start + chrono::Duration::minutes(m), 1_700_000_000_000 + m * 60_000).await.unwrap();
        }
        let e = lake_rows(&h, "events");
        let kinds = some(&column(&e, "kind"));
        let node = some(&column(&e, "node"));
        for n in ["5", "6"] {
            assert!(kinds.iter().zip(&node).any(|(k, x)| k == "experiment_start" && x == n), "node {n}");
        }
        assert!(!kinds.iter().zip(&node).any(|(k, x)| k.starts_with("experiment") && x == "7"));
        let ids = column(&e, "experiment_id");
        let first_day = |k: &str, i: &Option<String>| k == "experiment_start" && i.as_ref().is_some_and(|i| i.starts_with("exp-20251027"));
        let started: Vec<&String> = kinds.iter().zip(&ids).filter(|(k, i)| first_day(k, i)).filter_map(|(_, i)| i.as_ref()).collect();
        assert!(started.len() >= 2);
        for id in started {
            let closed = kinds.iter().zip(&ids).filter(|(k, i)| (*k == "experiment_end" || *k == "experiment_abort") && i.as_ref() == Some(id)).count();
            assert_eq!(closed, 1, "{id}");
        }
        assert!(kinds.iter().zip(&ids).all(|(k, i)| !k.starts_with("experiment") || i.is_some()));
        let d = lake_rows(&h, "decisions");
        let (explo, id, rule, new) = (column(&d, "exploration"), column(&d, "experiment_id"), column(&d, "rule"), column(&d, "setpoint_new"));
        let marked = (0..explo.len()).filter(|&i| explo[i].as_deref() == Some("true")).count();
        assert!(marked >= 4);
        for i in 0..explo.len() {
            assert_eq!(explo[i].as_deref() == Some("true"), id[i].is_some());
            assert_eq!(rule[i].as_deref() == Some("exploration"), id[i].is_some());
            assert!(new[i].as_ref().unwrap().parse::<f64>().unwrap() >= 7.0);
        }
        let room = column(&d, "room");
        assert!((0..explo.len()).all(|i| room[i].as_deref() != Some("c") || explo[i].as_deref() == Some("false")));
    }

    #[tokio::test]
    async fn window_open_prevents_experiments_and_manual_change_aborts_running() {
        let h = harness_with(false, "exploration_seed = 3\n", &format!("[[rooms]]\nname = 'a'\nnodes = [5]\n{}", explore_room()));
        let start = NaiveDate::from_ymd_opt(2025, 10, 27).unwrap().and_hms_opt(0, 0, 0).unwrap();
        call(&h.state, "POST", "/api/window", Some("{\"room\":\"a\",\"open\":true}".into())).await;
        for m in (0..1440).step_by(30) {
            control_step(&h.state, start + chrono::Duration::minutes(m), 1_700_000_000_000 + m * 60_000).await.unwrap();
        }
        assert!(!some(&column(&lake_rows(&h, "events"), "kind")).iter().any(|k| k == "experiment_start"));
        call(&h.state, "POST", "/api/window", Some("{\"room\":\"a\",\"open\":false}".into())).await;
        let mut m = 1440;
        while h.state.explorer.lock().unwrap().active_in("a").is_none() {
            control_step(&h.state, start + chrono::Duration::minutes(m), 1_700_000_000_000 + m * 60_000).await.unwrap();
            m += 10;
            assert!(m < 4000);
        }
        let report = matter_controller::AttributeReport { node: 5, endpoint: 1, attribute: "occupied_heating_setpoint".into(), value: 23.5 };
        crate::rules::on_report(&h.state, &report, false, 1_700_000_000_000 + m * 60_000);
        assert!(h.state.explorer.lock().unwrap().active_in("a").is_none());
        let e = lake_rows(&h, "events");
        let kinds = some(&column(&e, "kind"));
        assert_eq!(kinds.iter().filter(|k| *k == "experiment_abort").count(), 1);
        assert!(column(&e, "detail").iter().flatten().any(|d| d.contains("manual_override")));
        let d = lake_rows(&h, "decisions");
        let i = some(&column(&d, "rule")).iter().position(|r| r == "manual").unwrap();
        assert_eq!((column(&d, "setpoint_new")[i].clone(), column(&d, "applied")[i].clone(), column(&d, "proposed")[i].clone()), (Some("23.5".into()), Some("true".into()), Some("false".into())));
        assert!(column(&d, "experiment_id")[i].is_some());
        let l = lake_rows(&h, "labels");
        assert_eq!((some(&column(&l, "source")), some(&column(&l, "room"))), (vec!["override".to_string()], vec!["a".to_string()]));
    }

    #[tokio::test]
    async fn echo_of_own_write_and_first_report_are_not_manual() {
        let h = harness(false);
        let noon = NaiveDate::from_ymd_opt(2025, 10, 27).unwrap().and_hms_opt(12, 0, 0).unwrap();
        let report = |value| matter_controller::AttributeReport { node: 5, endpoint: 1, attribute: "occupied_heating_setpoint".into(), value };
        crate::rules::on_report(&h.state, &report(19.0), false, 1_000);
        control_step(&h.state, noon, 2_000).await.unwrap();
        crate::rules::on_report(&h.state, &report(18.0), false, 2_001);
        assert!(!some(&column(&lake_rows(&h, "decisions"), "rule")).iter().any(|r| r == "manual"));
        crate::rules::on_report(&h.state, &report(20.0), false, 300_000);
        assert!(some(&column(&lake_rows(&h, "decisions"), "rule")).iter().any(|r| r == "manual"));
    }

    fn setpoint_report(value: f64) -> matter_controller::AttributeReport {
        matter_controller::AttributeReport { node: 5, endpoint: 1, attribute: "occupied_heating_setpoint".into(), value }
    }

    async fn device_setpoint(h: &Harness) -> f64 {
        h.mock.read_thermostat(5).await.unwrap().occupied_heating_setpoint_c.unwrap()
    }

    #[tokio::test]
    async fn manual_change_is_held_for_exactly_the_hold_time_even_when_the_schedule_changes() {
        let schedule = "[[rooms.schedules]]
days = ['weekdays']
from = '13:00'
to = '14:00'
setpoint = 21.0
";
        let h = harness_with(false, "away_guard_minutes = 0
manual_hold_minutes = 60
", &format!("[[rooms]]
name = 'wohnzimmer'
nodes = [5]
{schedule}"));
        let monday = |h, m| NaiveDate::from_ymd_opt(2025, 10, 27).unwrap().and_hms_opt(h, m, 0).unwrap();
        let t0 = 1_700_000_000_000;
        let min = 60_000;
        crate::rules::on_report(&h.state, &setpoint_report(19.0), false, t0 - min);
        control_step(&h.state, monday(12, 0), t0).await.unwrap();
        assert_eq!(device_setpoint(&h).await, 18.0);
        h.mock.write_heating_setpoint(5, 22.0).await.unwrap();
        crate::rules::on_report(&h.state, &setpoint_report(22.0), false, t0 + 5 * min);
        assert!(h.state.commanded.lock().unwrap().holding(&[5]));
        for m in 6..30 {
            control_step(&h.state, monday(12, m as u32), t0 + m * min).await.unwrap();
        }
        assert_eq!(device_setpoint(&h).await, 22.0);
        let rules = some(&column(&lake_rows(&h, "decisions"), "rule"));
        assert_eq!(rules.iter().filter(|r| *r == "manual").count(), 1);
        control_step(&h.state, monday(13, 0), t0 + 64 * min).await.unwrap();
        assert_eq!(device_setpoint(&h).await, 22.0);
        assert!(h.state.commanded.lock().unwrap().holding(&[5]));
        control_step(&h.state, monday(13, 1), t0 + 65 * min).await.unwrap();
        assert_eq!(device_setpoint(&h).await, 21.0);
        assert!(!h.state.commanded.lock().unwrap().holding(&[5]));
    }

    #[tokio::test]
    async fn manual_hold_expires_after_the_configured_time() {
        let h = harness_with(false, "manual_hold_minutes = 60
", "[[rooms]]
name = 'wohnzimmer'
nodes = [5]
");
        let noon = NaiveDate::from_ymd_opt(2025, 10, 27).unwrap().and_hms_opt(12, 0, 0).unwrap();
        let (t0, min) = (1_700_000_000_000, 60_000);
        crate::rules::on_report(&h.state, &setpoint_report(19.0), false, t0 - min);
        control_step(&h.state, noon, t0).await.unwrap();
        h.mock.write_heating_setpoint(5, 22.0).await.unwrap();
        crate::rules::on_report(&h.state, &setpoint_report(22.0), false, t0 + 5 * min);
        control_step(&h.state, noon, t0 + 64 * min).await.unwrap();
        assert_eq!(device_setpoint(&h).await, 22.0);
        control_step(&h.state, noon, t0 + 65 * min).await.unwrap();
        assert_eq!(device_setpoint(&h).await, 18.0);
    }

    #[tokio::test]
    async fn stale_reports_and_clamped_values_right_after_a_write_are_not_manual() {
        let h = harness(false);
        let noon = NaiveDate::from_ymd_opt(2025, 10, 27).unwrap().and_hms_opt(12, 0, 0).unwrap();
        let t0 = 1_700_000_000_000;
        crate::rules::on_report(&h.state, &setpoint_report(19.0), false, t0 - 1_000);
        control_step(&h.state, noon, t0).await.unwrap();
        crate::rules::on_report(&h.state, &setpoint_report(19.0), true, t0 + 1_000);
        crate::rules::on_report(&h.state, &setpoint_report(18.5), false, t0 + 2_000);
        control_step(&h.state, noon, t0 + 60_000).await.unwrap();
        crate::rules::on_report(&h.state, &setpoint_report(21.0), true, t0 + 600_000);
        assert!(!h.state.commanded.lock().unwrap().holding(&[5]));
        assert!(!some(&column(&lake_rows(&h, "decisions"), "rule")).iter().any(|r| r == "manual"));
    }

    #[tokio::test]
    async fn watch_labels_and_state_in_scans_reach_the_lake() {
        let h = harness(false);
        let body = json!({"ts": now_ms(), "device": "pixel-watch", "label": "kueche", "motion": "still", "aps": [{"bssid": "aa:bb:cc:dd:ee:ff", "rssi": -50, "freq": 2412}]}).to_string();
        call(&h.state, "POST", "/api/scan", Some(body)).await;
        let labels = lake_rows(&h, "labels");
        assert_eq!(some(&column(&labels, "room")), vec!["kueche".to_string()]);
        assert!(some(&column(&lake_rows(&h, "events"), "kind")).iter().any(|k| k == "watch_state"));
    }

    #[tokio::test]
    async fn away_needs_guard_then_sets_all_rooms_and_logs_presence_events() {
        let schedule = "[[rooms.schedules]]
days = ['weekdays']
from = '06:00'
to = '08:30'
setpoint = 21.0
";
        let h = harness_with(false, "", &format!("[[rooms]]
name = 'a'
nodes = [5]
{schedule}[[rooms]]
name = 'b'
nodes = [6]
{schedule}"));
        h.mock.add_node(6, 19.0, 19.0);
        let monday_7 = NaiveDate::from_ymd_opt(2025, 10, 27).unwrap().and_hms_opt(7, 0, 0).unwrap();
        let now = now_ms();
        let body = |ts: i64, away: bool, motion: &str| json!({"ts": ts, "device": "pixel-watch", "motion": motion, "away": away}).to_string();
        call(&h.state, "POST", "/api/scan", Some(body(now - 40 * 60_000, true, "moving"))).await;
        call(&h.state, "POST", "/api/scan", Some(body(now - 5 * 60_000, true, "still"))).await;
        let early = control_step(&h.state, monday_7, now - 30 * 60_000).await.unwrap();
        assert!(early.iter().all(|w| (w.setpoint, w.reason) == (21.0, "schedule")));
        let away = control_step(&h.state, monday_7, now).await.unwrap();
        assert_eq!(away.iter().map(|w| (w.setpoint, w.reason, w.rule)).collect::<Vec<_>>(), vec![(18.0, "away", "presence"); 2]);
        assert!([5, 6].iter().all(|n| h.state.commanded.lock().unwrap().target(*n) == Some(18.0)));
        let events = lake_rows(&h, "events");
        let kinds = some(&column(&events, "kind"));
        assert_eq!(kinds.iter().filter(|k| *k == "presence_leave").count(), 1);
        assert!(column(&events, "detail").iter().flatten().any(|d| d.contains("watch_wifi")));
        call(&h.state, "POST", "/api/scan", Some(body(now + 1, false, "moving"))).await;
        assert!(some(&column(&lake_rows(&h, "events"), "kind")).iter().any(|k| k == "presence_arrive"));
    }

    fn extras(h: &Harness, why: &str) -> usize {
        some(&column(&lake_rows(h, "decisions"), "inputs.extra")).iter().filter(|e| e.contains(&format!("\"reason\":\"{why}\""))).count()
    }

    #[tokio::test]
    async fn occupied_needs_ten_minutes_and_holds_45_after_leaving() {
        let h = harness_with(true, "away_guard_minutes = 0
min_write_interval_min = 5
", "[[rooms]]
name = 'wohnzimmer'
nodes = [5]
");
        let noon = NaiveDate::from_ymd_opt(2025, 10, 27).unwrap().and_hms_opt(12, 0, 0).unwrap();
        let t0 = now_ms();
        call(&h.state, "POST", "/api/scan", Some(scan_at(t0, json!({})))).await;
        let at = |m: i64| t0 + m * 60_000;
        for m in 0..10 {
            control_step(&h.state, noon, at(m)).await.unwrap();
        }
        assert_eq!(h.mock.setpoint_writes(), vec![(5, 18.0)]);
        control_step(&h.state, noon, at(10)).await.unwrap();
        assert_eq!(h.mock.setpoint_writes(), vec![(5, 18.0), (5, 21.0)]);
        for m in 11..75 {
            control_step(&h.state, noon, at(m)).await.unwrap();
        }
        assert_eq!(device_setpoint(&h).await, 21.0);
        control_step(&h.state, noon, at(75)).await.unwrap();
        assert_eq!(h.mock.setpoint_writes(), vec![(5, 18.0), (5, 21.0), (5, 18.0)]);
        assert_eq!(extras(&h, "debounce"), 1);
        assert!(some(&column(&lake_rows(&h, "events"), "kind")).iter().any(|k| k == "occupancy_end"));
    }

    #[tokio::test]
    async fn debounce_compares_against_the_clamped_occupied_setpoint() {
        let h = harness_with(true, "away_guard_minutes = 0
occupied_setpoint = 30.0
", "[[rooms]]
name = 'wohnzimmer'
nodes = [5]
");
        let noon = NaiveDate::from_ymd_opt(2025, 10, 27).unwrap().and_hms_opt(12, 0, 0).unwrap();
        let t0 = now_ms();
        h.state.latest.update(5, "max_heat_setpoint_limit", 25.0, t0);
        h.state.latest.update(5, "occupied_heating_setpoint", 25.0, t0);
        call(&h.state, "POST", "/api/scan", Some(scan_at(t0, json!({})))).await;
        for m in 0..5 {
            control_step(&h.state, noon, t0 + m * 60_000).await.unwrap();
        }
        assert_eq!(extras(&h, "debounce"), 0);
    }

    #[tokio::test]
    async fn explicit_tap_occupies_immediately() {
        let h = harness(false);
        let noon = NaiveDate::from_ymd_opt(2025, 10, 27).unwrap().and_hms_opt(12, 0, 0).unwrap();
        let now = now_ms();
        call(&h.state, "POST", "/api/label", label_body("pixel-watch", now - 1_000, now, "wohnzimmer")).await;
        control_step(&h.state, noon, now).await.unwrap();
        assert_eq!(h.mock.setpoint_writes(), vec![(5, 21.0)]);
    }

    #[tokio::test]
    async fn second_change_within_the_interval_is_rate_limited_and_logged_once() {
        let h = harness_with(false, "", "[[rooms]]\nname = 'wohnzimmer'\nnodes = [5]\n[[rooms.schedules]]\ndays = ['weekdays']\nfrom = '12:05'\nto = '23:00'\nsetpoint = 21.0\n");
        let at = |m: u32| NaiveDate::from_ymd_opt(2025, 10, 27).unwrap().and_hms_opt(12, m, 0).unwrap();
        let t0 = 1_700_000_000_000;
        for m in 0..15 {
            control_step(&h.state, at(m), t0 + m as i64 * 60_000).await.unwrap();
        }
        assert_eq!(h.mock.setpoint_writes(), vec![(5, 18.0)]);
        assert_eq!(extras(&h, "rate_limited"), 1);
        control_step(&h.state, at(15), t0 + 15 * 60_000).await.unwrap();
        assert_eq!(h.mock.setpoint_writes(), vec![(5, 18.0), (5, 21.0)]);
    }

    #[tokio::test]
    async fn changes_below_the_deadband_are_not_written() {
        let h = harness_with(false, "", "[[rooms]]\nname = 'wohnzimmer'\nnodes = [5]\n[[rooms.schedules]]\ndays = ['weekdays']\nfrom = '12:05'\nto = '23:00'\nsetpoint = 18.5\n");
        let at = |m: u32| NaiveDate::from_ymd_opt(2025, 10, 27).unwrap().and_hms_opt(12, m, 0).unwrap();
        let t0 = 1_700_000_000_000;
        control_step(&h.state, at(0), t0).await.unwrap();
        crate::rules::on_report(&h.state, &setpoint_report(18.3), true, t0 + 60_000);
        for m in [5, 20, 40] {
            control_step(&h.state, at(m), t0 + m as i64 * 60_000).await.unwrap();
        }
        assert_eq!(h.mock.setpoint_writes(), vec![(5, 18.0)]);
    }

    #[tokio::test]
    async fn watch_still_at_home_is_never_away() {
        let h = harness(false);
        let noon = NaiveDate::from_ymd_opt(2025, 10, 27).unwrap().and_hms_opt(12, 0, 0).unwrap();
        let now = now_ms();
        for k in 0..6 {
            let body = json!({"ts": now - (6 - k) * 600_000, "device": "pixel-watch", "motion": "still", "away": true, "power": "doze"}).to_string();
            call(&h.state, "POST", "/api/scan", Some(body)).await;
        }
        let out = control_step(&h.state, noon, now).await.unwrap();
        assert!(out.iter().all(|w| w.reason != "away"));
        assert!(!some(&column(&lake_rows(&h, "events"), "kind")).iter().any(|k| k == "presence_leave"));
    }

    #[tokio::test]
    async fn watch_measurement_writes_watch_raw_with_derived_channel() {
        let h = harness(false);
        let body = json!({"device": "w", "ts": 1_761_580_800_000i64, "device_ts": 5, "id": "m-1", "batch_id": 77, "measurements": [
            {"sender": "AA:BB:CC:00:00:01", "rssi": -61, "freq": 2437}, {"sender": "aa:bb:cc:00:00:02", "rssi": -70, "freq": 5180}, {"sender": "aa:bb:cc:00:00:03", "rssi": -80, "channel": 9}]});
        let (status, resp) = call(&h.state, "POST", "/watch/measurement", Some(body.to_string())).await;
        assert_eq!((status, resp["stored"].clone(), resp["batch_id"].clone()), (StatusCode::OK, json!(3), json!(77)));
        let (_, again) = call(&h.state, "POST", "/watch/measurement", Some(body.to_string())).await;
        assert_eq!(again, json!({"stored": 0, "duplicate": true}));
        let r = lake_rows(&h, "watch_raw");
        assert_eq!(some(&column(&r, "sender")), ["aa:bb:cc:00:00:01", "aa:bb:cc:00:00:02", "aa:bb:cc:00:00:03"].map(String::from).to_vec());
        assert_eq!(some(&column(&r, "channel")), ["6", "36", "9"].map(String::from).to_vec());
        assert_eq!((some(&column(&r, "rssi")), some(&column(&r, "batch_id")), some(&column(&r, "device"))), (vec!["-61".to_string(), "-70".into(), "-80".into()], vec!["77".to_string(); 3], vec!["w".to_string(); 3]));
        assert_eq!(column(&r, "ts")[0].as_deref(), Some("1761580800000000"));
        assert_eq!(column(&r, "device_ts")[0].as_deref(), Some("5000"));
        assert_eq!([2412u16, 2472, 2484, 5180, 5955, 1000].map(channel_of), [Some(1), Some(13), Some(14), Some(36), Some(1), None]);
    }

    #[tokio::test]
    async fn watch_app_example_payload_is_accepted_verbatim() {
        let h = harness(false);
        let text = std::fs::read_to_string(concat!(env!("CARGO_MANIFEST_DIR"), "/../../contract/measurement_example.json")).unwrap();
        let kotlin = std::fs::read_to_string(concat!(env!("CARGO_MANIFEST_DIR"), "/../../wear/app/src/test/resources/measurement_example.json")).unwrap();
        assert_eq!(text, kotlin);
        let (status, resp) = call(&h.state, "POST", "/watch/measurement", Some(text.clone())).await;
        assert_eq!((status, resp["stored"].clone(), resp["batch_id"].clone()), (StatusCode::OK, json!(7), json!(1761580800500123i64)));
        let (_, again) = call(&h.state, "POST", "/watch/measurement", Some(text)).await;
        assert_eq!(again["stored"], json!(0));
        assert_eq!(again["duplicate"], json!(true));
        let r = lake_rows(&h, "watch_raw");
        let mut kinds = some(&column(&r, "sensor_type"));
        kinds.sort();
        assert_eq!(kinds, ["battery", "charging", "hr", "motion", "offbody", "rssi", "temp"].map(String::from).to_vec());
        assert_eq!(some(&column(&r, "device")), vec!["pixel-watch".to_string(); 7]);
        assert!(some(&column(&r, "sender")).contains(&"aa:bb:cc:00:00:01".to_string()));
    }

    #[tokio::test]
    async fn legacy_endpoints_write_new_tables_and_keep_contract() {
        let h = harness(true);
        let (status, body) = call(&h.state, "POST", "/api/scan", Some(scan_body())).await;
        assert_eq!((status, body), (StatusCode::OK, json!({"stored": 4, "room": "wohnzimmer"})));
        call(&h.state, "POST", "/api/scan", Some(scan_body())).await;
        let raw = lake_rows(&h, "watch_raw");
        assert_eq!(column(&raw, "sender").len(), 4);
        assert!(column(&raw, "channel").iter().all(|c| c.is_some()));
        let model = lake_rows(&h, "labels");
        assert_eq!((some(&column(&model, "source")), some(&column(&model, "room"))), (vec!["watch".to_string(), "model".to_string()], vec!["wohnzimmer".to_string(); 2]));
        assert!(column(&model, "confidence")[1].as_ref().unwrap().parse::<f64>().unwrap() > 0.9);
        assert_eq!(column(&model, "model_id")[1].as_deref(), Some("room_model.onnx"));
        assert!(column(&model, "probs")[1].as_ref().unwrap().contains("wohnzimmer"));
        let (_, batch) = call(&h.state, "POST", "/api/scans", Some(fixture("batch_example.json"))).await;
        assert_eq!(batch["stored"], 4);
        assert_eq!(column(&lake_rows(&h, "watch_raw"), "sender").len(), 8);
        let (status, labelled) = call(&h.state, "POST", "/api/label", label_body("pixel-watch", 1_000, 2_000, "bad")).await;
        assert_eq!((status, labelled), (StatusCode::OK, json!({"labelled": 0})));
        let (status, labelled) = call(&h.state, "POST", "/watch/label", Some(json!({"device": "pixel-watch", "from": 3_000, "to": 4_000, "room": "flur"}).to_string())).await;
        assert_eq!((status, labelled), (StatusCode::OK, json!({"labelled": 0})));
        let l = lake_rows(&h, "labels");
        let watch: Vec<usize> = (0..column(&l, "source").len()).filter(|&i| column(&l, "source")[i].as_deref() == Some("watch")).collect();
        assert_eq!(watch.len(), 3);
        assert_eq!((column(&l, "room")[watch[2]].as_deref(), column(&l, "from_ts")[watch[2]].as_deref(), column(&l, "to_ts")[watch[2]].as_deref()), (Some("flur"), Some("3000000"), Some("4000000")));
        assert_eq!(call(&h.state, "POST", "/watch/label", label_body("pixel-watch", 0, 10, "Bad")).await.0, StatusCode::BAD_REQUEST);
    }

    fn matcher_harness(model_dir: &str) -> Harness {
        let head = format!("away_guard_minutes = 0\n[watchmatch]\nenabled = true\nmodel_dir = '{}'\nscan_fresh_s = 600\n", model_dir.replace('\\', "/"));
        harness_with(false, &head, "[[rooms]]\nname = 'wohnzimmer'\nnodes = [5]\n[[rooms]]\nname = 'schlafzimmer'\nnodes = [6]\n")
    }

    fn fixture_models() -> String {
        format!("{FIXTURES}/watchroom")
    }

    fn long_body(batch: u64, ts: i64, rssi: [i64; 3]) -> Value {
        let mut samples: Vec<Value> = rssi
            .iter()
            .enumerate()
            .map(|(i, r)| json!({"elapsed_ns": 1_000 + i as i64, "ts": ts, "sensor_type": "rssi", "sensor_name": "wifi", "value": r, "unit": "dBm", "sender": format!("AA:BB:CC:00:00:0{}", i + 1), "freq": 2437}))
            .collect();
        samples.push(json!({"elapsed_ns": 2_000, "ts": ts, "sensor_type": "hr", "sensor_name": "hr", "value": 64.0, "unit": "bpm"}));
        samples.push(json!({"elapsed_ns": 3_000, "ts": ts, "sensor_type": "battery", "value": 80}));
        samples.push(json!({"elapsed_ns": 4_000, "ts": ts, "sensor_type": "temp", "value": 33.4, "unit": "C"}));
        json!({"device": "pixel-watch", "batch_id": batch, "sent_ts": ts, "clock_offset_ms": -120, "app_version": "1.4", "samples": samples})
    }

    #[tokio::test]
    async fn scan_reply_carries_matcher_guess_with_top_rooms() {
        let h = matcher_harness(&fixture_models());
        let body = json!({"device": "pixel-watch", "ts": now_ms(), "aps": [{"bssid": "aa:bb:cc:00:00:01", "rssi": -40, "freq": 2437}, {"bssid": "aa:bb:cc:00:00:02", "rssi": -60, "freq": 2437}, {"bssid": "aa:bb:cc:00:00:03", "rssi": -90, "freq": 2437}]});
        let (status, resp) = call(&h.state, "POST", "/api/scan", Some(body.to_string())).await;
        let m = &resp["match"];
        assert_eq!((status, m["room"].clone(), m["abstained"].clone(), m["reason"].clone(), m["model"].clone()), (StatusCode::OK, json!("wohnzimmer"), json!(false), Value::Null, json!("v1")));
        assert!(m["p"].as_f64().unwrap() > 0.9 && m["top"].as_array().unwrap().len() == 3 && m["top"][0]["room"] == "wohnzimmer");
    }

    #[tokio::test]
    async fn scan_reply_without_model_abstains_with_no_model() {
        let dir = std::env::temp_dir().join(format!("heatd-empty-models-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let h = matcher_harness(dir.to_str().unwrap());
        let body = json!({"device": "pixel-watch", "ts": now_ms(), "aps": [{"bssid": "aa:bb:cc:00:00:01", "rssi": -40, "freq": 2437}]});
        let (_, resp) = call(&h.state, "POST", "/api/scan", Some(body.to_string())).await;
        assert_eq!(resp["match"], json!({"room": null, "p": 0.0, "abstained": true, "reason": "no_model", "top": [], "model": null}));
        let (_, batch) = call(&h.state, "POST", "/api/scans", Some(json!({"scans": [{"device": "pixel-watch", "ts": now_ms() + 1_000, "aps": body["aps"].clone()}]}).to_string())).await;
        assert_eq!(batch["match"]["reason"], "no_model");
    }

    #[tokio::test]
    async fn long_format_is_stored_idempotently_and_feeds_the_matcher() {
        let h = matcher_harness(&fixture_models());
        let now = now_ms();
        let body = long_body(7, now, [-40, -60, -90]).to_string();
        let (status, resp) = call(&h.state, "POST", "/watch/measurement", Some(body.clone())).await;
        assert_eq!((status, resp["stored"].clone(), resp["batch_id"].clone(), resp["match"]["room"].clone(), resp["match"]["abstained"].clone()), (StatusCode::OK, json!(6), json!(7), json!("wohnzimmer"), json!(false)));
        let (_, again) = call(&h.state, "POST", "/watch/measurement", Some(body)).await;
        assert_eq!(again, json!({"stored": 0, "duplicate": true, "batch_id": 7}));
        let raw = lake_rows(&h, "watch_raw");
        assert_eq!(some(&column(&raw, "sensor_type")), ["rssi", "rssi", "rssi", "hr", "battery", "temp"].map(String::from).to_vec());
        assert_eq!(some(&column(&raw, "sender"))[..4], ["aa:bb:cc:00:00:01", "aa:bb:cc:00:00:02", "aa:bb:cc:00:00:03", ""].map(String::from));
        assert_eq!(some(&column(&raw, "rssi"))[..4], ["-40", "-60", "-90", "0"].map(String::from));
        assert_eq!((column(&raw, "value")[3].as_deref(), column(&raw, "clock_offset_ms")[0].as_deref(), column(&raw, "elapsed_ns")[1].as_deref(), column(&raw, "channel")[0].as_deref(), column(&raw, "unit")[0].as_deref()), (Some("64.0"), Some("-120"), Some("1001"), Some("6"), Some("dBm")));
        let inference = lake_rows(&h, "watch_inference");
        assert_eq!((some(&column(&inference, "result_room")), some(&column(&inference, "matcher")), some(&column(&inference, "state"))), (vec!["wohnzimmer".to_string()], vec!["room".to_string()], vec!["rest".to_string()]));
        let presence = crate::rules::current_presence(&h.state, now_ms()).unwrap().unwrap();
        assert_eq!(presence.room.as_deref(), Some("wohnzimmer"));
        assert_eq!(presence.occupancy(&h.state.config), crate::rules::Occupancy::Room("wohnzimmer"));
        let (_, health) = call(&h.state, "GET", "/health", None).await;
        assert_eq!((health["watch"]["model_version"].clone(), health["watch"]["abstain"][0]["total"].clone(), health["watch"]["samples"]["hr"].clone(), health["watch"]["samples"]["rssi"].clone()), (json!("v1"), json!(1), json!(1), json!(3)));
        let (_, metrics) = call_text(&h.state, "/metrics").await;
        assert!(metrics.contains("heatd_watch_inference_total{matcher=\"room\",state=\"rest\"} 1") && metrics.contains("heatd_watch_samples_total{sensor_type=\"hr\"} 1"), "{metrics}");
        let (status, _) = call(&h.state, "POST", "/watch/measurement", Some(long_body(8, now + 1000, [-75, -45, -60]).to_string())).await;
        assert_eq!(status, StatusCode::OK);
        let moved = crate::rules::current_presence(&h.state, now_ms()).unwrap().unwrap();
        assert_eq!(moved.room.as_deref(), Some("schlafzimmer"));
    }

    async fn call_text(state: &AppState, uri: &str) -> (StatusCode, String) {
        let request = Request::builder().method("GET").uri(uri).body(Body::empty()).unwrap();
        let response = router(state.clone()).oneshot(request).await.unwrap();
        let status = response.status();
        (status, String::from_utf8(response.into_body().collect().await.unwrap().to_bytes().to_vec()).unwrap())
    }

    #[tokio::test]
    async fn long_format_rejects_bad_samples_and_accepts_string_batch_ids() {
        let h = matcher_harness(&fixture_models());
        let now = now_ms();
        let mut bad = long_body(1, now, [-40, -60, -90]);
        bad["samples"][3]["sensor_type"] = json!("magnetometer");
        assert_eq!(call(&h.state, "POST", "/watch/measurement", Some(bad.to_string())).await.0, StatusCode::BAD_REQUEST);
        let mut nosender = long_body(1, now, [-40, -60, -90]);
        nosender["samples"][0].as_object_mut().unwrap().remove("sender");
        assert_eq!(call(&h.state, "POST", "/watch/measurement", Some(nosender.to_string())).await.0, StatusCode::BAD_REQUEST);
        let mut missing = long_body(1, now, [-40, -60, -90]);
        missing["samples"][0].as_object_mut().unwrap().remove("value");
        assert_eq!(call(&h.state, "POST", "/watch/measurement", Some(missing.to_string())).await.0, StatusCode::UNPROCESSABLE_ENTITY);
        let mut named = long_body(1, now, [-40, -60, -90]);
        named["batch_id"] = json!("pixel-watch-000123");
        let (status, first) = call(&h.state, "POST", "/watch/measurement", Some(named.to_string())).await;
        assert_eq!((status, first["stored"].clone()), (StatusCode::OK, json!(6)));
        assert_eq!(call(&h.state, "POST", "/watch/measurement", Some(named.to_string())).await.1["duplicate"], json!(true));
        assert_eq!(lake_rows(&h, "watch_raw").iter().map(|b| b.num_rows()).sum::<usize>(), 6);
        assert!(h.state.storage.seen_id("pixel-watch", "batch:pixel-watch-000123").unwrap());
        let mut negative = long_body(1, now, [-40, -60, -90]);
        negative["batch_id"] = json!(-4);
        assert_eq!(call(&h.state, "POST", "/watch/measurement", Some(negative.to_string())).await.0, StatusCode::BAD_REQUEST);
    }

    #[tokio::test]
    async fn watch_sensors_are_stored() {
        let h = harness(false);
        let body = json!({"app_version": "1.4", "sensors": [
            {"name": "com.google.sensor.skin_temperature", "type": 65555, "vendor": "Google", "resolution": 0.01, "maxRange": 60.0},
            {"sensor_name": "Heart Rate", "android_type": 21}]});
        let (status, resp) = call(&h.state, "POST", "/watch/sensors", Some(body.to_string())).await;
        assert_eq!((status, resp), (StatusCode::OK, json!({"stored": 2})));
        let rows = lake_rows(&h, "watch_sensors");
        assert_eq!((some(&column(&rows, "sensor_name")), some(&column(&rows, "android_type")), some(&column(&rows, "app_version"))), (vec!["com.google.sensor.skin_temperature".to_string(), "Heart Rate".into()], vec!["65555".to_string(), "21".into()], vec!["1.4".to_string(); 2]));
        assert_eq!((column(&rows, "vendor")[1].clone(), column(&rows, "max_range")[0].as_deref()), (None, Some("60.0")));
        assert_eq!(call(&h.state, "POST", "/watch/sensors", Some(json!({"sensors": []}).to_string())).await.0, StatusCode::BAD_REQUEST);
    }

    #[tokio::test]
    async fn missing_model_abstains_and_presence_falls_back() {
        let empty = tempfile::tempdir().unwrap();
        let h = matcher_harness(empty.path().to_str().unwrap());
        let now = now_ms();
        let (status, _) = call(&h.state, "POST", "/watch/measurement", Some(long_body(1, now, [-40, -60, -90]).to_string())).await;
        assert_eq!(status, StatusCode::OK);
        let inference = lake_rows(&h, "watch_inference");
        assert_eq!((some(&column(&inference, "reason")), some(&column(&inference, "abstained")), some(&column(&inference, "model_version"))), (vec!["no_model".to_string()], vec!["true".to_string()], vec!["none".to_string()]));
        assert!(!h.state.watchmatch.active());
        assert!(crate::rules::current_presence(&h.state, now_ms()).unwrap().is_none());
        call(&h.state, "POST", "/api/scan", Some(scan_at(now + 1000, json!({})))).await;
        let legacy = crate::rules::current_presence(&h.state, now_ms()).unwrap().unwrap();
        assert_eq!((legacy.room.as_deref(), legacy.source), (Some("wohnzimmer"), "stay_label"));
        let (_, health) = call(&h.state, "GET", "/health", None).await;
        assert_eq!(health["watch"]["abstain"][0]["abstained"], json!(2));
    }

    fn stay_harness() -> Harness {
        harness_with(false, "away_guard_minutes = 0
sleep_room = 'schlafzimmer'
", "[[rooms]]
name = 'wohnzimmer'
nodes = [5]
[[rooms]]
name = 'schlafzimmer'
nodes = [6]
[[rooms]]
name = 'flur'
")
    }

    fn wanted(h: &Harness, now: i64) -> Vec<(String, f64, &'static str, &'static str, bool)> {
        let noon = NaiveDate::from_ymd_opt(2025, 10, 27).unwrap().and_hms_opt(12, 0, 0).unwrap();
        let presence = crate::rules::current_presence(&h.state, now).unwrap();
        crate::rules::desired(noon, &h.state.config, presence.as_ref(), &presence.as_ref().and_then(|p| p.detected(&h.state.config)).into_iter().collect(), &Default::default(), &Default::default()).into_iter().map(|d| (d.room, d.setpoint, d.reason, d.source, d.sleep)).collect()
    }

    #[tokio::test]
    async fn stay_label_without_model_occupies_the_room_until_it_expires_or_the_wearer_moves() {
        let h = stay_harness();
        let now = now_ms();
        let post = |extra: Value| {
            let mut v = json!({"device": "pixel-watch"});
            extra.as_object().unwrap().iter().for_each(|(k, x)| v[k] = x.clone());
            call(&h.state, "POST", "/api/scan", Some(v.to_string()))
        };
        post(json!({"ts": now - 60_000, "label": "wohnzimmer", "motion": "still", "aps": [{"bssid": "aa:bb:cc:dd:ee:ff", "rssi": -50, "freq": 2412}]})).await;
        assert_eq!(wanted(&h, now), vec![("wohnzimmer".to_string(), 21.0, "occupied", "stay_label", false), ("schlafzimmer".to_string(), 18.0, "default", "stay_label", false)]);
        assert_eq!(wanted(&h, now + 31 * 60_000)[0].1, 18.0);
        post(json!({"ts": now - 30_000, "motion": "moving"})).await;
        assert_eq!(wanted(&h, now)[0].1, 18.0);
    }

    #[tokio::test]
    async fn label_endpoint_sets_the_stay_and_an_unlabeled_scan_ends_it() {
        let h = stay_harness();
        let now = now_ms();
        call(&h.state, "POST", "/api/label", label_body("pixel-watch", now - 120_000, now - 1_000, "wohnzimmer")).await;
        assert_eq!(wanted(&h, now)[0].2, "occupied");
        call(&h.state, "POST", "/api/scan", Some(json!({"ts": now, "device": "pixel-watch", "aps": [{"bssid": "aa:bb:cc:dd:ee:ff", "rssi": -50, "freq": 2412}]}).to_string())).await;
        assert_eq!(wanted(&h, now)[0].2, "default");
    }

    #[tokio::test]
    async fn bedtime_flag_keeps_the_bedroom_at_its_night_value_and_is_stored() {
        let h = stay_harness();
        let now = now_ms();
        call(&h.state, "POST", "/api/scan", Some(json!({"ts": now, "device": "pixel-watch", "label": "schlafzimmer", "bedtime": true, "motion": "still", "aps": [{"bssid": "aa:bb:cc:dd:ee:ff", "rssi": -50, "freq": 2412}]}).to_string())).await;
        assert_eq!(h.state.storage.device_statuses().unwrap()[0].bedtime, Some(true));
        assert_eq!(wanted(&h, now)[1], ("schlafzimmer".to_string(), 18.0, "sleep", "stay_label", true));
        let mut long = long_body(900, now + 1000, [-40, -60, -90]);
        long["bedtime"] = json!(false);
        assert_eq!(call(&h.state, "POST", "/watch/measurement", Some(long.to_string())).await.0, StatusCode::OK);
        assert_eq!(h.state.storage.device_statuses().unwrap()[0].bedtime, Some(false));
    }

    #[tokio::test]
    async fn legacy_scan_and_phase_one_body_feed_the_matcher_too() {
        let h = matcher_harness(&fixture_models());
        let now = now_ms();
        let phase_one = json!({"device": "w", "ts": now, "id": "m-9", "measurements": [
            {"sender": "aa:bb:cc:00:00:01", "rssi": -75}, {"sender": "aa:bb:cc:00:00:02", "rssi": -45}, {"sender": "aa:bb:cc:00:00:03", "rssi": -60}]});
        call(&h.state, "POST", "/watch/measurement", Some(phase_one.to_string())).await;
        let raw = lake_rows(&h, "watch_raw");
        assert_eq!((some(&column(&raw, "sensor_type")), column(&raw, "value")[0].as_deref()), (vec!["rssi".to_string(); 3], Some("-75.0")));
        assert_eq!(h.state.watchmatch.effective(now_ms()).unwrap().unwrap().0, "schlafzimmer");
        call(&h.state, "POST", "/api/scan", Some(scan_at(now + 2000, json!({"device": "pixel-watch"})))).await;
        assert_eq!(lake_rows(&h, "watch_inference").iter().map(|b| b.num_rows()).sum::<usize>(), 2);
    }

    fn scan_form(batch: u64, ts: i64, id: &str, away: bool, motion: &str) -> Value {
        let aps = [("aa:bb:cc:00:00:01", -45, 2412), ("aa:bb:cc:00:00:02", -67, 5180), ("aa:bb:cc:00:00:03", -82, 2437), ("aa:bb:cc:00:00:99", -90, 2462)];
        let samples: Vec<Value> = aps.iter().map(|(b, r, f)| json!({"elapsed_ns": 900_000_000_000i64, "ts": ts, "sensor_type": "rssi", "sensor_name": "wifi", "value": *r as f64, "unit": "dBm", "sender": b, "freq": f})).collect();
        json!({"device": "pixel-watch", "batch_id": batch, "sent_ts": ts + 5, "clock_offset_ms": 100, "app_version": "2.5", "samples": samples, "id": id, "motion": motion, "power": "normal", "away": away, "label": "kueche", "queued": true})
    }

    #[tokio::test]
    async fn rssi_batch_in_scan_form_runs_the_full_phase_one_path() {
        let head = format!("away_guard_minutes = 0\n[watchmatch]\nenabled = true\nmodel_dir = '{}'\nscan_fresh_s = 600\n", fixture_models().replace('\\', "/"));
        let h = harness_with(true, &head, "[[rooms]]\nname = 'wohnzimmer'\nnodes = [5]\n");
        let now = now_ms();
        let body = scan_form(500, now, "pixel-watch-000001", false, "still");
        let (status, resp) = call(&h.state, "POST", "/watch/measurement", Some(body.to_string())).await;
        assert_eq!((status, resp["stored"].clone(), resp["batch_id"].clone(), resp["room"].clone()), (StatusCode::OK, json!(4), json!(500), json!("wohnzimmer")));
        assert!(resp["probability"].as_f64().unwrap() > 0.9);
        assert_eq!(resp["rooms"][0]["room"], json!("wohnzimmer"));
        assert!(resp["aps"].as_array().is_some_and(|a| !a.is_empty()));
        assert_eq!((h.state.storage.count("scans").unwrap(), h.state.storage.count("device_state").unwrap(), h.state.storage.count("predictions").unwrap()), (4, 1, 1));
        let st = &h.state.storage.device_statuses().unwrap()[0];
        assert_eq!((st.motion, st.power, st.away), (Some(Motion::Still), Some(Power::Normal), false));
        assert_eq!(h.state.storage.scan_labels("pixel-watch").unwrap().iter().filter(|(_, l)| l.as_deref() == Some("kueche")).count(), 1);
        let raw = lake_rows(&h, "watch_raw");
        assert_eq!(some(&column(&raw, "sensor_type")), vec!["rssi".to_string(); 4]);
        let labels = lake_rows(&h, "labels");
        assert_eq!(some(&column(&labels, "source")), ["watch", "model"].map(String::from).to_vec());
        assert_eq!(some(&column(&lake_rows(&h, "watch_inference"), "result_room")), vec!["wohnzimmer".to_string()]);
        let (_, again) = call(&h.state, "POST", "/watch/measurement", Some(body.to_string())).await;
        assert_eq!(again, json!({"stored": 0, "duplicate": true, "batch_id": 500}));
        assert_eq!(h.state.storage.count("device_state").unwrap(), 1);
        let schedule = "[[rooms.schedules]]
days = ['weekdays']
from = '06:00'
to = '08:30'
setpoint = 21.0
";
        let g = harness_with(false, "away_guard_minutes = 0
", &format!("[[rooms]]
name = 'a'
nodes = [5]
{schedule}"));
        let monday_7 = NaiveDate::from_ymd_opt(2025, 10, 27).unwrap().and_hms_opt(7, 0, 0).unwrap();
        for (batch, ts, id, away, motion) in [(502, now - 40 * 60_000, "pixel-watch-000002", true, "moving"), (503, now - 5 * 60_000, "pixel-watch-000003", true, "still")] {
            assert_eq!(call(&g.state, "POST", "/watch/measurement", Some(scan_form(batch, ts, id, away, motion).to_string())).await.0, StatusCode::OK);
        }
        let away = control_step(&g.state, monday_7, now).await.unwrap();
        assert!(away.iter().all(|w| (w.setpoint, w.reason) == (18.0, "away")));
        call(&g.state, "POST", "/watch/measurement", Some(scan_form(504, now + 1, "pixel-watch-000004", false, "moving").to_string())).await;
        let kinds = some(&column(&lake_rows(&g, "events"), "kind"));
        assert_eq!((kinds.iter().filter(|k| *k == "presence_leave").count(), kinds.iter().filter(|k| *k == "presence_arrive").count()), (1, 1));
    }

    #[tokio::test]
    async fn watch_offbody_value_one_means_on_the_wrist() {
        let h = matcher_harness(&fixture_models());
        let now = now_ms();
        let mut worn = long_body(1, now, [-40, -60, -90]);
        worn["samples"].as_array_mut().unwrap().push(json!({"elapsed_ns": 5_000, "ts": now - 1000, "sensor_type": "offbody", "sensor_name": "low_latency_offbody", "value": 1.0, "unit": "bool"}));
        assert_eq!(call(&h.state, "POST", "/watch/measurement", Some(worn.to_string())).await.0, StatusCode::OK);
        let mut off = long_body(2, now + 120_000, [-40, -60, -90]);
        off["samples"].as_array_mut().unwrap().push(json!({"elapsed_ns": 6_000, "ts": now + 60_000, "sensor_type": "offbody", "sensor_name": "low_latency_offbody", "value": 0.0, "unit": "bool"}));
        assert_eq!(call(&h.state, "POST", "/watch/measurement", Some(off.to_string())).await.0, StatusCode::OK);
        call(&h.state, "POST", "/watch/measurement", Some(long_body(3, now + 180_000, [-40, -60, -90]).to_string())).await;
        let inference = lake_rows(&h, "watch_inference");
        assert_eq!(some(&column(&inference, "state")), ["rest", "offbody", "offbody"].map(String::from).to_vec());
        assert_eq!(some(&column(&inference, "result_room"))[0], "wohnzimmer");
        assert_eq!(some(&column(&inference, "reason"))[1], "unknown_state");
    }
}
