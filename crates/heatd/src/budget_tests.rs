use crate::config::Config;
use crate::http::AppState;
use crate::rules::control_step;
use crate::storage::Storage;
use chrono::NaiveDate;
use matter_controller::MockController;
use std::sync::{Arc, Mutex};

const T0: i64 = 1_700_000_000_000;
const MIN: i64 = 60_000;

struct Env {
    state: AppState,
    mock: Arc<MockController>,
    _dir: tempfile::TempDir,
}

const SCHEDULE: &str = "[[rooms.schedules]]\ndays = ['weekdays']\nfrom = '{from}'\nto = '{to}'\nsetpoint = {sp}\n";

fn schedules(items: &[(&str, &str, f64)]) -> String {
    items.iter().map(|(from, to, sp)| SCHEDULE.replace("{from}", from).replace("{to}", to).replace("{sp}", &sp.to_string())).collect()
}

fn env(head: &str, schedule: &[(&str, &str, f64)]) -> Env {
    let dir = tempfile::tempdir().unwrap();
    let root = dir.path().to_str().unwrap().replace('\\', "/");
    let config = Config::parse(&format!("db_path = '{root}/live.duckdb'\ndefault_setpoint = 18.0\nmin_write_interval_min = 0\n{head}\n[[rooms]]\nname = 'a'\nnodes = [5]\n{}", schedules(schedule))).unwrap();
    let sink = crate::lake::Sink::open(crate::lake::LakeConfig::new(dir.path().join("lake"))).unwrap();
    let status = crate::status::Status::new(crate::lake::now_us());
    let mock = Arc::new(MockController::new(1));
    mock.add_node(5, 19.0, 19.0);
    let boost = crate::boost::Boost::new(&config);
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
        boost,
        batch_seq: Arc::new(std::sync::atomic::AtomicU64::new(1)),
    };
    Env { state, mock, _dir: dir }
}

impl Env {
    fn tags(&self) -> Vec<String> {
        use arrow::array::{Array, AsArray, StructArray};
        self.state.sink.flush().unwrap();
        let mut files: Vec<_> = std::fs::read_dir(self._dir.path().join("lake/raw/decisions")).unwrap().flatten().map(|e| e.path()).collect();
        files.sort();
        files.iter().flat_map(|f| crate::lake::load_stream(f).unwrap().batches).flat_map(|b| {
            let inputs = b.column_by_name("inputs").unwrap().as_any().downcast_ref::<StructArray>().unwrap().column_by_name("extra").unwrap().clone();
            let text = arrow::compute::cast(&inputs, &arrow::datatypes::DataType::Utf8).unwrap();
            let text = text.as_string::<i32>();
            (0..text.len()).filter(|i| !text.is_null(*i)).map(|i| text.value(i).to_string()).collect::<Vec<_>>()
        }).collect()
    }

    async fn at(&self, minute: i64) {
        let now = NaiveDate::from_ymd_opt(2025, 10, 27).unwrap().and_hms_opt(12, 0, 0).unwrap() + chrono::Duration::minutes(minute);
        control_step(&self.state, now, T0 + minute * MIN).await.unwrap();
    }

    async fn through(&self, from: i64, to: i64) {
        for m in from..=to {
            self.at(m).await;
        }
    }

    fn writes(&self) -> Vec<f64> {
        self.mock.setpoint_writes().into_iter().map(|w| w.1).collect()
    }

    fn used(&self, minute: i64) -> usize {
        self.state.commanded.lock().unwrap().budget.used("a", T0 + minute * MIN)
    }

    fn deferred(&self) -> Option<f64> {
        self.state.commanded.lock().unwrap().budget.deferred(&[5])
    }
}

#[tokio::test]
async fn a_deferred_raise_is_applied_later_with_the_current_desired_value() {
    let e = env("", &[("12:05", "12:20", 20.0), ("12:20", "12:40", 22.0), ("12:40", "23:00", 21.0)]);
    e.through(0, 64).await;
    assert_eq!(e.writes(), vec![18.0, 20.0]);
    assert_eq!((e.used(64), e.deferred()), (1, Some(21.0)));
    e.at(65).await;
    assert_eq!(e.writes(), vec![18.0, 20.0, 21.0]);
    assert_eq!((e.used(65), e.deferred()), (1, None));
}

#[tokio::test]
async fn next_free_time_and_state_view_follow_the_window() {
    let e = env("", &[("12:05", "23:00", 21.0), ("12:20", "23:00", 22.0)]);
    e.through(0, 30).await;
    let commands = e.state.commanded.lock().unwrap();
    let room = &e.state.config.rooms[0];
    let view = commands.budget.view(room, T0 + 30 * MIN, 1);
    assert_eq!((view["used"].clone(), view["next_free_ts"].clone()), (1.into(), (T0 + 65 * MIN).into()));
    assert!(commands.budget.metrics(&e.state.config.rooms, T0 + 30 * MIN, 1).contains("heatd_raise_next_free_seconds{room=\"a\"} 2100"));
}

#[tokio::test]
async fn decreases_are_immediate_and_free() {
    let e = env("", &[("12:05", "12:10", 21.0), ("12:10", "12:15", 19.0), ("12:15", "12:20", 16.0), ("12:20", "12:25", 22.0)]);
    e.through(0, 24).await;
    assert_eq!(e.writes(), vec![18.0, 21.0, 19.0, 16.0]);
    assert_eq!((e.used(26), e.deferred()), (1, Some(22.0)));
}

#[tokio::test]
async fn zero_disables_the_budget() {
    let e = env("raise_budget_per_hour = 0", &[("12:05", "12:10", 21.0), ("12:10", "12:15", 18.0), ("12:15", "12:20", 22.0)]);
    e.through(0, 16).await;
    assert_eq!(e.writes(), vec![18.0, 21.0, 18.0, 22.0]);
    assert_eq!(e.used(16), 0);
}

#[tokio::test]
async fn the_budget_is_configurable() {
    let e = env("raise_budget_per_hour = 2", &[("12:05", "12:10", 21.0), ("12:10", "12:15", 18.0), ("12:15", "12:20", 22.0), ("12:20", "12:25", 18.0), ("12:25", "12:30", 23.0)]);
    e.through(0, 26).await;
    assert_eq!(e.writes(), vec![18.0, 21.0, 18.0, 22.0, 18.0]);
    assert_eq!(e.deferred(), Some(22.0));
}

#[tokio::test]
async fn restoring_the_failsafe_value_after_a_restart_is_exempt_once() {
    let e = env("[failsafe]\nsetpoint = 12.0", &[("12:00", "12:10", 21.0), ("12:10", "23:00", 22.0)]);
    e.state.latest.update(5, "occupied_heating_setpoint", 12.0, T0);
    e.through(0, 12).await;
    assert_eq!(e.writes(), vec![21.0, 22.0]);
    assert_eq!(e.used(12), 1);
}

#[tokio::test]
async fn forgetting_writes_after_the_br_failsafe_makes_the_next_raise_exempt() {
    let e = env("[br_watchdog]\nsetpoint_c = 16.0", &[("12:00", "12:30", 20.0), ("12:30", "13:00", 21.0)]);
    e.at(0).await;
    e.through(1, 5).await;
    assert_eq!(e.used(5), 0);
    e.state.latest.update(5, "occupied_heating_setpoint", 16.0, T0);
    e.state.commanded.lock().unwrap().forget_desired_writes();
    e.at(6).await;
    assert_eq!(e.writes(), vec![20.0, 20.0]);
    assert_eq!(e.used(6), 0);
}

#[tokio::test]
async fn window_close_is_exempt_from_the_budget_and_logged_so() {
    let e = env("", &[("12:05", "12:40", 21.0), ("12:40", "23:00", 22.0)]);
    e.through(0, 20).await;
    let start = T0 + 21 * MIN;
    e.state.windows.lock().unwrap().manual("a", true, start);
    e.at(21).await;
    e.state.windows.lock().unwrap().manual("a", false, start + MIN);
    e.at(22).await;
    assert_eq!(e.writes(), vec![18.0, 21.0, 7.5, 21.0]);
    assert_eq!(e.used(22), 1);
    e.through(23, 45).await;
    assert_eq!(e.writes(), vec![18.0, 21.0, 7.5, 21.0]);
    assert_eq!(e.deferred(), Some(22.0));
    assert_eq!(e.tags().iter().filter(|x| x.contains("exempt_window_close")).count(), 1, "{:?}", e.tags());
}

#[tokio::test]
async fn every_window_close_in_the_hour_returns_at_once() {
    let e = env("", &[("12:05", "23:00", 21.0)]);
    e.through(0, 6).await;
    for (i, minute) in [(0, 7), (1, 9)] {
        let ts = T0 + minute * MIN;
        e.state.windows.lock().unwrap().manual("a", true, ts);
        e.at(minute).await;
        e.state.windows.lock().unwrap().manual("a", false, ts + MIN);
        e.at(minute + 1).await;
        assert_eq!(e.writes().len(), 4 + 2 * i, "round {i}");
    }
    assert_eq!(e.writes(), vec![18.0, 21.0, 7.5, 21.0, 7.5, 21.0]);
    assert_eq!(e.used(10), 1);
}

#[tokio::test]
async fn a_boost_consumes_the_hourly_raise_and_the_return_is_a_free_decrease() {
    let e = env("", &[("12:00", "12:30", 18.0), ("12:30", "23:00", 21.0)]);
    e.state.boost.set_model(crate::boostlearn::tests::light_model_for("a"));
    e.state.status.outdoor_obs(crate::lake::now_us(), 5.0, Some(3.0));
    let feed = |m: i64| {
        e.state.boost.observe(5, crate::window::LOCAL_TEMPERATURE, 14.0, T0 + m * MIN);
        e.state.boost.observe(5, crate::window::PI_HEATING_DEMAND, 10.0, T0 + m * MIN);
    };
    for m in 0..=8 {
        feed(m);
        e.at(m).await;
    }
    assert_eq!((e.writes(), e.used(8)), (vec![18.0], 0));
    let reply = e.state.boost.request("a", Some(5.0), Some("morning"), T0 + 8 * MIN + 30_000);
    assert_eq!(reply["ok"], true, "{reply}");
    for m in 9..=29 {
        feed(m);
        e.at(m).await;
    }
    assert_eq!(e.writes(), vec![18.0, 24.0, 18.0]);
    assert_eq!((e.used(29), e.deferred()), (1, None));
    let limits: Vec<(&str, f64)> = e.mock.write_log().into_iter().filter(|w| w.0 == "limit" || w.2 >= 22.0 || w.2 == 18.0).map(|w| (w.0, w.2)).collect();
    assert_eq!(limits, vec![("setpoint", 18.0), ("limit", 22.0), ("limit", 24.0), ("setpoint", 24.0), ("setpoint", 18.0), ("limit", 22.0)]);
    for m in 30..=31 {
        feed(m);
        e.at(m).await;
    }
    assert_eq!((e.writes(), e.used(31), e.deferred()), (vec![18.0, 24.0, 18.0], 1, Some(21.0)));
    feed(70);
    e.at(70).await;
    assert_eq!((e.writes().last().copied(), e.used(70)), (Some(21.0), 1));
}

#[tokio::test]
async fn a_spent_raise_blocks_the_boost_and_a_boost_blocks_nothing_else() {
    let e = env("", &[("12:00", "12:05", 18.0), ("12:05", "23:00", 21.0)]);
    e.state.boost.set_model(crate::boostlearn::tests::light_model_for("a"));
    e.state.status.outdoor_obs(crate::lake::now_us(), 5.0, Some(3.0));
    for m in 0..=6 {
        e.state.boost.observe(5, crate::window::LOCAL_TEMPERATURE, 14.0, T0 + m * MIN);
        e.state.boost.observe(5, crate::window::PI_HEATING_DEMAND, 10.0, T0 + m * MIN);
        e.at(m).await;
    }
    assert_eq!((e.writes(), e.used(6)), (vec![18.0, 21.0], 1));
    let reply = e.state.boost.request("a", None, Some("morning"), T0 + 6 * MIN + 30_000);
    assert!(reply["ok"] == false && reply["reason"].as_str().unwrap().contains("Budget"), "{reply}");
}
