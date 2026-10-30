use crate::config::Config;
use crate::context::write_meta;
use crate::http::{router, AppState};
use crate::lake::{self, Label, Sink, Table, Weather};
use crate::report;
use crate::rules::control_step;
use axum::body::Body;
use axum::http::Request;
use chrono::NaiveDate;
use matter_controller::MockController;
use std::sync::atomic::{AtomicI64, Ordering};
use std::sync::Arc;
use tower::ServiceExt;

const MIN: i64 = 60_000_000;
const HOUR: i64 = 60 * MIN;

fn day() -> NaiveDate {
    "2026-10-20".parse().unwrap()
}

fn midnight() -> i64 {
    day().and_hms_opt(0, 0, 0).unwrap().and_utc().timestamp_micros()
}

fn raw_rows(sink: &Sink, table: Table) -> i64 {
    sink.flush().unwrap();
    lake::raw_files(sink.root(), table, Some(day())).unwrap().iter().map(|(f, _)| lake::load_stream(f).unwrap().batches.iter().map(|b| b.num_rows() as i64).sum::<i64>()).sum()
}

async fn post(state: &AppState, uri: &str, body: String) -> axum::http::StatusCode {
    let request = Request::builder().method("POST").uri(uri).header("content-type", "application/json").body(Body::from(body)).unwrap();
    router(state.clone()).oneshot(request).await.unwrap().status()
}

fn count(db: &report::LakeDb, sql: &str) -> i64 {
    db.conn.query_row(sql, [], |r| r.get(0)).unwrap()
}

#[tokio::test]
async fn phase1_acceptance_simulated_day() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().to_str().unwrap().replace('\\', "/");
    let exploring = "[rooms.exploration]\nenabled = true\np_per_day = 1.0\ncomfort_min = 16.0\ncomfort_max = 22.0\n";
    let config = Config::parse(&format!(
        "db_path = '{path}/live.duckdb'\nsnapshot_path = '{path}/out/snapshot.duckdb'\ndefault_setpoint = 18.0\nexploration_seed = 5\nlatitude = 52.5\nlongitude = 13.4\nneighbours = [['a','b']]\n[[rooms]]\nname = 'a'\nnodes = [5]\n{exploring}[[rooms]]\nname = 'b'\nnodes = [6]\n{exploring}[[rooms]]\nname = 'c'\nnodes = [7]\n"
    ))
    .unwrap();
    let clock = Arc::new(AtomicI64::new(midnight()));
    let sink = Sink::open_with_clock(lake::LakeConfig::new(format!("{path}/lake")), {
        let clock = clock.clone();
        Arc::new(move || clock.load(Ordering::SeqCst))
    })
    .unwrap();
    let mock = Arc::new(MockController::new(1));
    [5, 6, 7].into_iter().for_each(|n| mock.add_node(n, 19.0, 19.0));
    let config = Arc::new(config);
    let status = crate::status::Status::new(midnight());
    let state = AppState {
        watchmatch: crate::watchmatch::WatchMatch::new(&config.watchmatch, sink.clone(), status.clone()),
        config: config.clone(),
        storage: Arc::new(crate::storage::Storage::open(&config.db_path).unwrap()),
        sink: sink.clone(),
        latest: Default::default(),
        status,
        model: None,
        controller: mock.clone(),
        windows: Default::default(),
        commanded: Default::default(),
        explorer: Arc::new(std::sync::Mutex::new(crate::explore::Explorer::new(config.exploration_seed.unwrap()))),
        tracker: Default::default(),
        boost: Default::default(),
        failsafe: Default::default(),
        batch_seq: Arc::new(std::sync::atomic::AtomicU64::new(1)),
    };
    write_meta(&sink, &config, midnight()).unwrap();
    for h in 0..24 {
        sink.append(Weather { ts: midnight() + h * HOUR, kind: "obs".into(), fetched_ts: midnight() + h * HOUR + 300_000_000, temp_c: Some(8.0 + h as f32 / 4.0), radiation_wm2: Some(50.0), wind_ms: Some(3.0), ..Default::default() }).unwrap();
    }
    let start = day().and_hms_opt(0, 0, 0).unwrap();
    let scan: serde_json::Value = serde_json::from_str(&std::fs::read_to_string(concat!(env!("CARGO_MANIFEST_DIR"), "/../../contract/scan_example.json")).unwrap()).unwrap();
    let mut scans = 0;
    for m in 0..1440i64 {
        clock.store(midnight() + m * MIN, Ordering::SeqCst);
        for node in [5u64, 6, 7] {
            let ts = midnight() + m * MIN + 5_000_000;
            for (attribute, v) in [(0u32, 1900 + (m % 7) * 10), (18, 1800)] {
                sink.append(lake::AttributeReport { ts, node, endpoint: 1, cluster: 513, attribute, dtype: "int".into(), v_i64: Some(v), source: "subscription".into(), fw: "1".into(), ..Default::default() }).unwrap();
            }
        }
        if m % 10 == 0 {
            control_step(&state, start + chrono::Duration::minutes(m), (midnight() + m * MIN) / 1000).await.unwrap();
        }
        if m % 60 == 30 {
            let mut body = scan.clone();
            body["ts"] = serde_json::json!((midnight() + m * MIN) / 1000);
            body.as_object_mut().unwrap().remove("id");
            assert_eq!(post(&state, "/api/scan", body.to_string()).await, axum::http::StatusCode::OK);
            scans += 1;
        }
    }
    let label = serde_json::json!({"device": "pixel-watch", "from": midnight() / 1000, "to": midnight() / 1000 + 600_000, "label": "a"});
    assert_eq!(post(&state, "/api/label", label.to_string()).await, axum::http::StatusCode::OK);
    sink.append(Label { ts: midnight() + 12 * HOUR, room: "b".into(), source: "manual".into(), ..Default::default() }).unwrap();
    let sensors = serde_json::json!({"app_version": "1", "sensors": [{"name": "skin", "type": 65555}]});
    assert_eq!(post(&state, "/watch/sensors", sensors.to_string()).await, axum::http::StatusCode::OK);
    sink.append(lake::WatchInference { ts: midnight() + 12 * HOUR, matcher: "room".into(), template_id: "none".into(), abstained: true, reason: Some("no_model".into()), state: "rest".into(), features_hash: "0".into(), model_version: "none".into(), ..Default::default() }).unwrap();
    sink.append(lake::Ambient { ts: midnight() + 12 * HOUR, source: "minix".into(), sensor: "nvme_c".into(), value: 40.0, unit: "C".into() }).unwrap();
    let before: Vec<(Table, i64)> = Table::ALL.into_iter().map(|t| (t, raw_rows(&sink, t))).collect();
    for (t, n) in &before {
        assert!(*n > 0, "{} empty", t.name());
    }
    assert!(raw_rows(&sink, Table::WatchRaw) >= scans);
    clock.store(midnight() + 25 * HOUR, Ordering::SeqCst);
    let rolls = sink.roll_day(day()).unwrap();
    assert!(rolls.iter().all(|r| matches!(r.status, lake::RollStatus::Rolled)), "{rolls:?}");
    let root = sink.root().to_path_buf();
    let db = report::open_lake(&root, day(), day()).unwrap();
    for (t, n) in &before {
        let marker: serde_json::Value = serde_json::from_str(&std::fs::read_to_string(lake::parquet_dir(&root, *t, day()).join(".rolled")).unwrap()).unwrap();
        assert_eq!(marker["rows"].as_i64(), Some(*n), "{} marker", t.name());
        assert_eq!(count(&db, &format!("SELECT count(*) FROM {}", t.name())), *n, "{} parquet", t.name());
    }
    for (room, node) in [("a", 5), ("b", 6)] {
        assert!(count(&db, &format!("SELECT count(DISTINCT experiment_id) FROM events WHERE kind = 'experiment_start' AND node = {node}")) >= 1, "room {room} has no experiment");
        assert!(count(&db, &format!("SELECT count(*) FROM decisions WHERE room = '{room}' AND exploration AND experiment_id IS NOT NULL AND rule = 'exploration'")) >= 1, "room {room} has no marked decision");
    }
    assert_eq!(count(&db, "SELECT count(*) FROM events WHERE kind = 'experiment_start' AND node = 7"), 0);
    assert_eq!(count(&db, "SELECT count(*) FROM decisions WHERE exploration AND (experiment_id IS NULL OR room = 'c')"), 0);
    assert_eq!(count(&db, "SELECT count(*) FROM decisions WHERE experiment_id IS NOT NULL AND NOT exploration"), 0);
    assert_eq!(count(&db, "SELECT count(*) FROM events WHERE kind IN ('experiment_end','experiment_abort') AND experiment_id NOT IN (SELECT experiment_id FROM events WHERE kind = 'experiment_start')"), 0);
    let (file, summary) = report::generate(&root, day()).unwrap();
    assert!(file.ends_with("reports/2026-10-20.md") && std::fs::read_to_string(&file).unwrap().starts_with("# heatd daily report 2026-10-20"));
    assert!(summary.experiments >= 2 && summary.roll_ok && summary.nodes == 3, "{summary:?}");
    let grid = report::open_room_minute(&root, day(), day()).unwrap();
    for room in ["a", "b", "c"] {
        let (n, distinct, first, last): (i64, i64, i64, i64) = grid
            .conn
            .query_row(&format!("SELECT count(*), count(DISTINCT t), epoch(min(t)), epoch(max(t)) FROM room_minute WHERE room = '{room}'"), [], |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?, r.get(3)?)))
            .unwrap();
        assert_eq!((n, distinct, last - first), (1440, 1440, 1439 * 60), "room {room}");
    }
    assert!(count(&grid, "SELECT count(*) FROM room_minute WHERE room = 'a' AND exploration") >= 1);
}
