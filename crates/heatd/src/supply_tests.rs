use crate::config::Config;
use crate::http::AppState;
use crate::storage::Storage;
use chrono::NaiveDate;
use matter_controller::MockController;
use std::sync::{Arc, Mutex};

const T0: i64 = 1_000_000_000_000;
const MIN: i64 = 60_000;

struct Env {
    state: AppState,
    mock: Arc<MockController>,
    t: i64,
    _dir: tempfile::TempDir,
}

fn env(extra: &str) -> Env {
    let dir = tempfile::tempdir().unwrap();
    let root = dir.path().to_str().unwrap().replace('\\', "/");
    let config = Config::parse(&format!("db_path = '{root}/live.duckdb'\ndefault_setpoint = 20.0\n{extra}\n[[rooms]]\nname = 'a'\nnodes = [5]\n")).unwrap();
    let sink = crate::lake::Sink::open(crate::lake::LakeConfig::new(dir.path().join("lake"))).unwrap();
    let status = crate::status::Status::new(crate::lake::now_us());
    let mock = Arc::new(MockController::new(1));
    mock.add_node(5, 19.0, 19.0);
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
    };
    Env { state, mock, t: T0, _dir: dir }
}

impl Env {
    fn valve(&self, v: f64) {
        self.state.windows.lock().unwrap().observe(&self.state.config, 5, crate::window::PI_HEATING_DEMAND, v, self.t);
    }

    async fn run(&mut self, temps: Vec<f64>) {
        let noon = NaiveDate::from_ymd_opt(2026, 10, 31).unwrap().and_hms_opt(12, 0, 0).unwrap();
        for temp in temps {
            self.t += MIN;
            self.state.windows.lock().unwrap().observe(&self.state.config, 5, crate::window::LOCAL_TEMPERATURE, temp, self.t);
            crate::rules::control_step(&self.state, noon, self.t).await.unwrap();
        }
    }

    fn target(&self) -> Option<f64> {
        self.state.commanded.lock().unwrap().target(5)
    }

    fn no_supply(&self) -> bool {
        self.state.windows.lock().unwrap().no_supply_rooms().contains("a")
    }
}

fn ramp(from: f64, per_min: f64, n: usize) -> Vec<f64> {
    (1..=n).map(|i| from + per_min * i as f64).collect()
}

async fn night() -> Env {
    let mut e = env("");
    e.run(vec![19.0; 5]).await;
    e.valve(100.0);
    e.run(ramp(19.0, -0.0155, 120)).await;
    e
}

#[tokio::test]
async fn night_without_supply_caps_the_setpoint_near_the_measured_temperature() {
    let e = night().await;
    assert!(e.no_supply());
    let temp = e.state.windows.lock().unwrap().local_temp(5).unwrap();
    let target = e.target().unwrap();
    assert!(target <= temp + 0.5 && target > temp, "target {target} temp {temp}");
    assert_eq!(e.mock.setpoint_writes().last().map(|w| w.1), Some(target));
}

#[tokio::test]
async fn morning_restart_releases_the_cap_without_a_wound_up_overshoot() {
    let mut e = night().await;
    let mut worst = f64::MIN;
    let mut temp = 17.14;
    for _ in 0..30 {
        temp += 0.1;
        e.run(vec![temp]).await;
        worst = worst.max(temp - e.target().unwrap());
    }
    assert!(!e.no_supply());
    assert_eq!(e.target(), Some(20.0));
    assert!(worst <= 1.0, "{worst}");
}

#[tokio::test]
async fn overshoot_with_open_valve_resets_the_setpoint_once_then_restores() {
    let mut e = env("");
    e.run(vec![20.0; 3]).await;
    e.valve(93.0);
    e.run(vec![21.5; 14]).await;
    assert_eq!(e.target(), Some(20.0));
    e.run(vec![21.5; 2]).await;
    assert_eq!(e.target(), Some(7.5));
    e.run(vec![21.5; 8]).await;
    assert_eq!(e.target(), Some(7.5));
    e.run(vec![21.5; 4]).await;
    assert_eq!(e.target(), Some(20.0));
    e.run(vec![21.5; 40]).await;
    assert_eq!(e.target(), Some(20.0));
}

#[tokio::test]
async fn overshoot_reset_skips_small_overshoot_and_manual_hold() {
    let mut e = env("");
    e.run(vec![20.0; 3]).await;
    e.valve(93.0);
    e.run(vec![20.8; 30]).await;
    assert_eq!(e.target(), Some(20.0));
    e.state.commanded.lock().unwrap().test_hold(5, 20.0);
    e.run(vec![21.5; 30]).await;
    assert_eq!(e.target(), Some(20.0));
}
