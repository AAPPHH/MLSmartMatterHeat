use crate::br_watchdog::BrWatchdog;
use crate::config::Config;
use crate::http::AppState;
use crate::lake;
use crate::status::{Status, SubState};
use crate::storage::Storage;
use arrow::array::Array;
use axum::extract::State;
use axum::http::{HeaderMap, StatusCode};
use axum::routing::{get, post};
use axum::{Json, Router};
use matter_controller::MockController;
use serde_json::{json, Value};
use std::sync::{Arc, Mutex};

const TOKEN: &str = "secret";

#[derive(Default)]
struct Fake {
    heartbeats: Vec<(Option<String>, Value)>,
    status: Value,
    down: bool,
}

type Shared = Arc<Mutex<Fake>>;

async fn serve_fake(fake: Shared) -> String {
    async fn status(State(f): State<Shared>) -> (StatusCode, Json<Value>) {
        let f = f.lock().unwrap();
        (if f.down { StatusCode::SERVICE_UNAVAILABLE } else { StatusCode::OK }, Json(f.status.clone()))
    }
    async fn heartbeat(State(f): State<Shared>, headers: HeaderMap, Json(body): Json<Value>) -> (StatusCode, Json<Value>) {
        let token = headers.get("x-heatd-token").and_then(|v| v.to_str().ok()).map(String::from);
        let ok = token.as_deref() == Some(TOKEN);
        f.lock().unwrap().heartbeats.push((token, body));
        if ok { (StatusCode::OK, Json(json!({"ok": true, "mode": "dry_run", "age_s": 0}))) } else { (StatusCode::UNAUTHORIZED, Json(json!({"ok": false}))) }
    }
    let app = Router::new().route("/watchdog/status", get(status)).route("/watchdog/heartbeat", post(heartbeat)).with_state(fake);
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let url = format!("http://{}", listener.local_addr().unwrap());
    tokio::spawn(async move { axum::serve(listener, app).await.unwrap() });
    url
}

fn fake(triggered: bool) -> Shared {
    fake_mode(triggered, "dry_run")
}

fn fake_mode(triggered: bool, mode: &str) -> Shared {
    Arc::new(Mutex::new(Fake { status: json!({"mode": mode, "triggered": triggered, "die_temperature_c": 41.5, "last_heartbeat_age_s": 3.0}), ..Default::default() }))
}

struct Env {
    state: AppState,
    dog: BrWatchdog,
    dir: tempfile::TempDir,
}

fn env(url: &str, token_file: Option<&str>, started_us: i64) -> Env {
    let dir = tempfile::tempdir().unwrap();
    let root = dir.path().to_str().unwrap().replace('\\', "/");
    let token_path = format!("{root}/token");
    std::fs::write(&token_path, format!("{}\n", token_file.unwrap_or(TOKEN))).unwrap();
    let toml = format!("db_path = '{root}/live.duckdb'\n[br_watchdog]\nenabled = true\nhttp_timeout_s = 30\nurl = '{url}'\ntoken_file = '{token_path}'\n[[rooms]]\nname = 'a'\nnodes = [5]\n[[rooms]]\nname = 'b'\nnodes = [6]\n");
    let config = Config::parse(&toml).unwrap();
    let sink = lake::Sink::open(lake::LakeConfig::new(dir.path().join("lake"))).unwrap();
    let status = Status::new(started_us);
    status.register_nodes(config.all_nodes());
    let mock = Arc::new(MockController::new(1));
    mock.add_node(5, 19.0, 19.0);
    mock.add_node(6, 19.0, 19.0);
    let dog = BrWatchdog::new(&config);
    let state = AppState {
        watchmatch: crate::watchmatch::WatchMatch::new(&config.watchmatch, sink.clone(), status.clone()),
        storage: Arc::new(Storage::open(&config.db_path).unwrap()),
        sink,
        latest: Default::default(),
        config: Arc::new(config.clone()),
        model: None,
        controller: mock,
        windows: Default::default(),
        commanded: Default::default(),
        status,
        explorer: Arc::new(Mutex::new(crate::explore::Explorer::new(1))),
        tracker: Default::default(),
        boost: Default::default(),
        batch_seq: Arc::new(std::sync::atomic::AtomicU64::new(1)),
        failsafe: crate::failsafe::Failsafe::new(&config.failsafe, None),
    };
    Env { state, dog, dir }
}

fn strings(e: &Env, table: &str, column: &str) -> Vec<String> {
    e.state.sink.flush().unwrap();
    let Ok(entries) = std::fs::read_dir(e.dir.path().join("lake/raw").join(table)) else { return Vec::new() };
    let mut files: Vec<_> = entries.flatten().map(|f| f.path()).collect();
    files.sort();
    files
        .iter()
        .flat_map(|f| lake::load_stream(f).unwrap().batches)
        .flat_map(|b| {
            let cast = arrow::compute::cast(b.column_by_name(column).unwrap(), &arrow::datatypes::DataType::Utf8).unwrap();
            let text = arrow::array::AsArray::as_string::<i32>(&cast);
            (0..text.len()).map(|i| text.value(i).to_string()).collect::<Vec<_>>()
        })
        .collect()
}

fn transitions(e: &Env) -> Vec<String> {
    let kinds = strings(e, "events", "kind");
    let details = strings(e, "events", "detail");
    kinds.into_iter().zip(details).filter(|(k, _)| k == "br_watchdog").map(|(_, d)| d).collect()
}

fn state_name(e: &Env) -> String {
    e.state.status.br_view(lake::now_us()).unwrap()["state"].as_str().unwrap().to_string()
}

#[tokio::test(flavor = "multi_thread")]
async fn heartbeat_carries_token_contract_body_and_die_temperature_becomes_ambient() {
    let f = fake(false);
    let url = serve_fake(f.clone()).await;
    let e = env(&url, None, lake::now_us());
    let now = lake::now_us();
    e.state.status.control_step_done(now);
    e.dog.tick(&e.state, now).await;
    {
        let f = f.lock().unwrap();
        assert_eq!(f.heartbeats.len(), 1);
        let (token, body) = &f.heartbeats[0];
        assert_eq!(token.as_deref(), Some(TOKEN));
        assert_eq!((body["timeout_s"].as_u64(), body["setpoint_c"].as_f64(), body["endpoint"].as_u64()), (Some(600), Some(18.0), Some(1)));
        assert_eq!(body["nodes"], json!([5, 6]));
        assert_eq!(body["ts"].as_i64(), Some(now / 1000));
    }
    assert_eq!((strings(&e, "ambient", "source"), strings(&e, "ambient", "sensor"), strings(&e, "ambient", "value")), (vec!["br".to_string()], vec!["die_c".to_string()], vec!["41.5".to_string()]));
    assert_eq!(e.state.boost.ambient_value("die_c"), Some(41.5));
    let view = e.state.status.br_view(now).unwrap();
    assert_eq!((view["state"].as_str(), view["br_mode"].as_str(), view["br_triggered"].as_bool(), view["die_temperature_c"].as_f64()), (Some("ok"), Some("dry_run"), Some(false), Some(41.5)));
    assert!(view["last_heartbeat_ok_age_s"].as_f64().unwrap() < 5.0);
    assert!(e.state.status.br_alert().is_none());
}

#[tokio::test(flavor = "multi_thread")]
async fn no_heartbeat_from_a_stale_or_blind_control_loop() {
    let f = fake(false);
    let url = serve_fake(f.clone()).await;
    let e = env(&url, None, lake::now_us());
    let now = lake::now_us();
    e.dog.tick(&e.state, now).await;
    e.state.status.control_step_done(now - 121_000_000);
    e.dog.tick(&e.state, now).await;
    assert!(f.lock().unwrap().heartbeats.is_empty());
    e.state.status.control_step_done(now);
    e.state.status.subscription(5, SubState::Lost, now);
    e.dog.tick(&e.state, now).await;
    assert_eq!(f.lock().unwrap().heartbeats.len(), 1);
    e.state.status.subscription(6, SubState::Lost, now);
    e.dog.tick(&e.state, now).await;
    assert_eq!(f.lock().unwrap().heartbeats.len(), 1);
    e.state.status.subscription(6, SubState::Subscribed, now);
    e.dog.tick(&e.state, now).await;
    assert_eq!(f.lock().unwrap().heartbeats.len(), 2);
}

async fn tick(e: &Env, extra_s: i64) {
    let now = lake::now_us() + extra_s * 1_000_000;
    e.state.status.control_step_done(now);
    e.dog.tick(&e.state, now).await;
}

#[tokio::test(flavor = "multi_thread")]
async fn states_transition_with_events_alerts_and_recovery() {
    let f = fake(false);
    let url = serve_fake(f.clone()).await;
    let started = lake::now_us() - 3_600_000_000;
    let e = env(&url, None, started);
    e.state.status.subscription(5, SubState::Subscribed, started);
    e.state.status.subscription(6, SubState::Subscribed, started);
    e.state.status.node_report(5, lake::now_us());
    tick(&e, 0).await;
    assert_eq!(state_name(&e), "ok");
    e.state.status.node_report(6, lake::now_us() - 1_000_000_000);
    f.lock().unwrap().down = true;
    tick(&e, 0).await;
    tick(&e, 0).await;
    assert_eq!(state_name(&e), "ok");
    tick(&e, 0).await;
    assert_eq!(state_name(&e), "br_http_down");
    let alert = e.state.status.br_alert().unwrap();
    assert!(alert["id"].as_str().unwrap().starts_with("br_http_down@") && !alert["text"].as_str().unwrap().is_empty() && !alert["title"].as_str().unwrap().is_empty());
    f.lock().unwrap().down = false;
    tick(&e, 0).await;
    assert_eq!(state_name(&e), "ok");
    tick(&e, 16 * 60).await;
    assert_eq!(state_name(&e), "thread_down");
    f.lock().unwrap().down = true;
    for _ in 0..3 {
        tick(&e, 16 * 60).await;
    }
    assert_eq!(state_name(&e), "both");
    f.lock().unwrap().down = false;
    e.state.status.node_report(5, lake::now_us() + 1_000_000_000);
    e.state.status.node_report(6, lake::now_us() + 1_000_000_000);
    tick(&e, 0).await;
    assert_eq!(state_name(&e), "ok");
    let details = transitions(&e);
    let pairs: Vec<(String, String)> = details.iter().map(|d| serde_json::from_str::<Value>(d).unwrap()).map(|v| (v["from"].as_str().unwrap().to_string(), v["to"].as_str().unwrap().to_string())).collect();
    let expect = [("ok", "br_http_down"), ("br_http_down", "ok"), ("ok", "thread_down"), ("thread_down", "both"), ("both", "ok")];
    assert_eq!(pairs, expect.iter().map(|(a, b)| (a.to_string(), b.to_string())).collect::<Vec<_>>());
}

#[tokio::test(flavor = "multi_thread")]
async fn rejected_heartbeats_count_as_router_down() {
    let f = fake(false);
    let url = serve_fake(f.clone()).await;
    let e = env(&url, Some("wrong"), lake::now_us());
    for _ in 0..3 {
        let now = lake::now_us();
        e.state.status.control_step_done(now);
        e.dog.tick(&e.state, now).await;
    }
    assert_eq!(f.lock().unwrap().heartbeats.len(), 3);
    assert_eq!(state_name(&e), "br_http_down");
    assert_eq!(e.state.status.br_view(lake::now_us()).unwrap()["heartbeats_failed"].as_u64(), Some(3));
}

#[tokio::test(flavor = "multi_thread")]
async fn armed_triggered_router_makes_heatd_write_its_setpoints_again_once_and_keeps_holds() {
    let f = fake_mode(false, "armed");
    let url = serve_fake(f.clone()).await;
    let e = env(&url, None, lake::now_us());
    let noon = chrono::NaiveDate::from_ymd_opt(2025, 10, 27).unwrap().and_hms_opt(12, 0, 0).unwrap();
    crate::rules::control_step(&e.state, noon, 1_000).await.unwrap();
    assert_eq!(e.state.commanded.lock().unwrap().target(5), Some(18.0));
    let now = lake::now_us();
    e.state.status.control_step_done(now);
    e.dog.tick(&e.state, now).await;
    assert_eq!(e.state.commanded.lock().unwrap().target(5), Some(18.0));
    e.state.commanded.lock().unwrap().test_hold(6, 21.0);
    f.lock().unwrap().status["triggered"] = json!(true);
    e.dog.tick(&e.state, now).await;
    assert_eq!(e.state.commanded.lock().unwrap().target(5), None);
    assert!(e.state.commanded.lock().unwrap().hold(6).is_some());
    crate::rules::control_step(&e.state, noon, 2_000).await.unwrap();
    assert_eq!(e.state.commanded.lock().unwrap().target(5), Some(18.0));
    e.dog.tick(&e.state, now).await;
    assert_eq!(e.state.commanded.lock().unwrap().target(5), Some(18.0));
    let rewrites = transitions(&e).iter().filter(|d| d.contains("\"rewrite\"")).count();
    assert_eq!(rewrites, 1);
    f.lock().unwrap().status["triggered"] = json!(false);
    e.dog.tick(&e.state, now).await;
    f.lock().unwrap().status["triggered"] = json!(true);
    e.dog.tick(&e.state, now).await;
    assert_eq!(transitions(&e).iter().filter(|d| d.contains("\"rewrite\"")).count(), 2);
}

#[tokio::test(flavor = "multi_thread")]
async fn dry_run_triggered_router_causes_no_rewrite() {
    let f = fake(false);
    let url = serve_fake(f.clone()).await;
    let e = env(&url, None, lake::now_us());
    let noon = chrono::NaiveDate::from_ymd_opt(2025, 10, 27).unwrap().and_hms_opt(12, 0, 0).unwrap();
    crate::rules::control_step(&e.state, noon, 1_000).await.unwrap();
    let now = lake::now_us();
    e.state.status.control_step_done(now);
    f.lock().unwrap().status["triggered"] = json!(true);
    e.dog.tick(&e.state, now).await;
    assert_eq!(e.state.commanded.lock().unwrap().target(5), Some(18.0));
    assert_eq!(transitions(&e).iter().filter(|d| d.contains("\"rewrite\"")).count(), 0);
}

#[tokio::test(flavor = "multi_thread")]
async fn heartbeats_stop_when_all_thermostats_are_silent_or_not_subscribed() {
    let f = fake(false);
    let url = serve_fake(f.clone()).await;
    let started = lake::now_us() - 3_600_000_000;
    let e = env(&url, None, started);
    let now = lake::now_us();
    e.state.status.control_step_done(now);
    e.dog.tick(&e.state, now).await;
    assert!(f.lock().unwrap().heartbeats.is_empty());
    e.state.status.subscription(5, SubState::Subscribed, now);
    e.state.status.node_report(5, now);
    e.dog.tick(&e.state, now).await;
    assert_eq!(f.lock().unwrap().heartbeats.len(), 1);
    e.state.status.control_step_done(now + 16 * 60_000_000);
    e.dog.tick(&e.state, now + 16 * 60_000_000).await;
    assert_eq!(f.lock().unwrap().heartbeats.len(), 1);
}

#[tokio::test(flavor = "multi_thread")]
async fn no_heartbeat_when_node_count_or_endpoint_exceed_router_limits() {
    let f = fake(false);
    let url = serve_fake(f.clone()).await;
    let mut e = env(&url, None, lake::now_us());
    let mut config = (*e.state.config).clone();
    config.chip_tool.endpoint = 300;
    e.dog = BrWatchdog::new(&config);
    let now = lake::now_us();
    e.state.status.control_step_done(now);
    e.dog.tick(&e.state, now).await;
    assert!(f.lock().unwrap().heartbeats.is_empty());
}

#[tokio::test(flavor = "multi_thread")]
async fn no_rewrite_while_heatd_is_unhealthy() {
    let f = fake_mode(true, "armed");
    let url = serve_fake(f.clone()).await;
    let e = env(&url, None, lake::now_us());
    let noon = chrono::NaiveDate::from_ymd_opt(2025, 10, 27).unwrap().and_hms_opt(12, 0, 0).unwrap();
    crate::rules::control_step(&e.state, noon, 1_000).await.unwrap();
    e.dog.tick(&e.state, lake::now_us()).await;
    assert_eq!(e.state.commanded.lock().unwrap().target(5), Some(18.0));
    assert!(f.lock().unwrap().heartbeats.is_empty());
}
