use super::*;
use crate::boostlearn::tests::light_model;

const T0: i64 = 1_700_000_000_000;
const ROOM: &str = "wohnzimmer";
const NODE: u64 = 5;

fn at(min: f64) -> i64 {
    T0 + (min * 60_000.0) as i64
}

fn config() -> Config {
    Config::parse("[boost]\nmodel_dir = '/nonexistent'\nseed = 3\n[[rooms]]\nname = 'wohnzimmer'\nnodes = [5]\n[[rooms]]\nname = 'bad'\nnodes = [7]\n").unwrap()
}

struct Rig {
    boost: Boost,
    temp: f64,
}

impl Rig {
    fn new() -> Self {
        let boost = Boost::new(&config());
        boost.set_model(light_model());
        Self { boost, temp: 18.0 }
    }

    fn bare() -> Self {
        Self { boost: Boost::new(&config()), temp: 18.0 }
    }

    fn feed(&self, min: f64, temp: f64) {
        self.boost.observe(NODE, crate::window::LOCAL_TEMPERATURE, temp, at(min));
        self.boost.observe(NODE, crate::window::PI_HEATING_DEMAND, 10.0, at(min));
    }

    fn room(setpoint: f64) -> RoomIn {
        RoomIn { room: ROOM.into(), setpoint, blocked: None, no_raise: false, no_supply: false }
    }

    fn tick_with(&self, min: f64, hour: f64, flags: (bool, bool), room: RoomIn) -> StepOut {
        self.feed(min, self.temp);
        self.boost.step(&StepIn { now_ms: at(min), hour, outdoor: Some((5.0, Some(3.0))), radiation: Some(0.0), presence_known: true, sleep: flags.0, away: flags.1, rooms: &[room] })
    }

    fn tick(&self, min: f64, hour: f64, flags: (bool, bool), setpoint: f64) -> StepOut {
        self.tick_with(min, hour, flags, Self::room(setpoint))
    }

    fn morning(&self) -> StepOut {
        self.tick(0.0, 6.0, (true, false), 18.0);
        self.tick(1.0, 6.0, (false, false), 21.0)
    }

    fn boosting(&self) -> f64 {
        let out = self.morning();
        assert!(matches!(out.directives.get(ROOM), Some(Directive::Boost { setpoint, .. }) if *setpoint == 24.0), "{:?}", self.boost.view(ROOM));
        self.boost.wrote(ROOM, "boost", at(1.0));
        self.boost.view(ROOM)["cycle"]["minutes"].as_f64().unwrap()
    }
}

fn detail(e: &Event) -> Value {
    serde_json::from_str(e.detail.as_deref().unwrap()).unwrap()
}

fn is_boost(out: &StepOut) -> bool {
    matches!(out.directives.get(ROOM), Some(Directive::Boost { .. }))
}

#[test]
fn morning_wake_up_boosts_the_room_whose_target_rises() {
    let rig = Rig::new();
    assert!(rig.tick(0.0, 6.0, (true, false), 18.0).directives.is_empty());
    let out = rig.tick(1.0, 6.0, (false, false), 21.0);
    assert!(is_boost(&out));
    let v = rig.boost.view(ROOM);
    assert_eq!((v["state"].as_str(), v["scenario"].as_str(), v["source"].as_str(), v["boosted"].as_bool()), (Some("armed"), Some("morning"), Some("auto"), Some(true)));
    assert!(v["allowed"].as_f64().unwrap() >= 5.0 && v["chosen"].as_f64().unwrap() <= v["allowed"].as_f64().unwrap() && v["reason"].as_str().unwrap().starts_with("Boost morning"), "{v}");
    assert_eq!(rig.boost.view("bad")["state"], "idle");
}

#[test]
fn rise_in_the_next_tick_inside_the_window_still_counts() {
    let rig = Rig::new();
    rig.tick(0.0, 6.0, (true, false), 18.0);
    assert!(!is_boost(&rig.tick(1.0, 6.0, (false, false), 18.0)));
    assert!(is_boost(&rig.tick(2.0, 6.0, (false, false), 20.0)));
}

#[test]
fn rise_after_the_trigger_window_or_below_one_kelvin_does_not_boost() {
    let rig = Rig::new();
    rig.tick(0.0, 6.0, (true, false), 18.0);
    rig.tick(1.0, 6.0, (false, false), 18.0);
    assert!(!is_boost(&rig.tick(6.0, 6.0, (false, false), 21.0)));
    let small = Rig::new();
    small.tick(0.0, 6.0, (true, false), 18.0);
    assert!(!is_boost(&small.tick(1.0, 6.0, (false, false), 18.9)));
}

#[test]
fn room_change_and_ordinary_rises_never_boost() {
    let rig = Rig::new();
    for (min, sp) in [(0.0, 18.0), (1.0, 21.0), (2.0, 18.0), (3.0, 21.0), (4.0, 18.0), (5.0, 21.5)] {
        assert!(rig.tick(min, 7.0, (false, false), sp).directives.is_empty(), "minute {min}");
    }
    let v = rig.boost.view(ROOM);
    assert_eq!((v["state"].as_str(), v["scenario"].clone(), v["reason"].as_str()), (Some("idle"), Value::Null, Some("noch keine Boost-Entscheidung")));
}

#[test]
fn waking_up_outside_the_morning_hours_or_while_away_does_not_boost() {
    for (hour, flags) in [(12.0, (false, false)), (4.0, (false, false)), (6.0, (false, true))] {
        let rig = Rig::new();
        rig.tick(0.0, hour, (true, flags.1), 18.0);
        assert!(!is_boost(&rig.tick(1.0, hour, (false, flags.1), 21.0)), "hour {hour} away {}", flags.1);
    }
    let rig = Rig::new();
    rig.tick(0.0, 6.0, (false, false), 18.0);
    assert!(!is_boost(&rig.tick(1.0, 6.0, (false, false), 21.0)));
}

#[test]
fn homecoming_boosts_rooms_rising_from_the_away_setpoint() {
    let rig = Rig { temp: 15.0, ..Rig::new() };
    assert!(!is_boost(&rig.tick(0.0, 17.5, (false, true), 18.0)));
    let out = rig.tick(1.0, 17.5, (false, false), 21.0);
    assert!(is_boost(&out));
    let v = rig.boost.view(ROOM);
    assert_eq!((v["scenario"].as_str(), v["source"].as_str()), (Some("homecoming"), Some("auto")));
    let other = Rig::new();
    other.tick(0.0, 17.5, (false, true), 18.0);
    assert!(!is_boost(&other.tick(1.0, 17.5, (false, false), 18.4)));
}

#[test]
fn missing_bundle_never_boosts_and_says_why() {
    let rig = Rig::bare();
    rig.tick(0.0, 6.0, (true, false), 18.0);
    let out = rig.tick(1.0, 6.0, (false, false), 21.0);
    assert!(out.directives.is_empty());
    let refused: Vec<Value> = out.events.iter().map(detail).collect();
    assert_eq!((refused.len(), refused[0]["mode"].as_str(), refused[0]["reason"].as_str()), (1, Some("refused"), Some("kein Boost-Modell geladen")));
    assert_eq!(rig.boost.view(ROOM)["reason"], "kein Boost-Modell geladen");
    let reply = rig.boost.request(ROOM, Some(3.0), at(1.0));
    assert_eq!((reply["ok"].clone(), reply["reason"].as_str()), (false.into(), Some("kein Boost-Modell geladen")));
}

#[test]
fn spent_raise_budget_blocked_room_and_full_target_refuse_the_boost() {
    for (room, why) in [
        (RoomIn { no_raise: true, ..Rig::room(21.0) }, "Budget"),
        (RoomIn { blocked: Some("window_open"), ..Rig::room(21.0) }, "blockiert"),
        (RoomIn { blocked: Some("manual_hold"), ..Rig::room(21.0) }, "blockiert"),
    ] {
        let rig = Rig::new();
        rig.tick(0.0, 6.0, (true, false), 18.0);
        let out = rig.tick_with(1.0, 6.0, (false, false), room);
        assert!(out.directives.is_empty() && rig.boost.view(ROOM)["reason"].as_str().unwrap().contains(why), "{}", rig.boost.view(ROOM));
        assert_eq!(out.events.len(), 1);
    }
    let warm = Rig { temp: 20.9, ..Rig::new() };
    warm.tick(0.0, 6.0, (true, false), 18.0);
    assert!(!is_boost(&warm.tick(1.0, 6.0, (false, false), 21.0)));
    assert!(warm.boost.view(ROOM)["reason"].as_str().unwrap().contains("am Ziel"));
    let hot = Rig::new();
    hot.tick(0.0, 6.0, (true, false), 22.0);
    assert!(!is_boost(&hot.tick(1.0, 6.0, (false, false), 23.5)));
    assert!(hot.boost.view(ROOM)["reason"].as_str().unwrap().contains("Boost-Sollwert"));
}

#[test]
fn an_unsafe_context_refuses_with_the_model_reason() {
    let rig = Rig::new();
    rig.tick(0.0, 6.0, (true, false), 18.0);
    let out = rig.tick_with(1.0, 6.0, (false, false), RoomIn { no_supply: true, ..Rig::room(21.0) });
    assert!(out.directives.is_empty());
    assert!(rig.boost.view(ROOM)["reason"].as_str().unwrap().contains("versorgung"), "{}", rig.boost.view(ROOM));
}

#[test]
fn full_cycle_arms_boosts_hands_back_after_the_dose_and_reports_the_overshoot_hour() {
    let rig = Rig::new();
    let minutes = rig.boosting();
    assert_eq!(rig.boost.view(ROOM)["state"], "boosting");
    assert_eq!(rig.boost.next_deadline_ms(), Some(at(1.0) + (minutes * 60_000.0) as i64));
    assert!(is_boost(&rig.tick(2.0, 6.0, (false, false), 21.0)));
    rig.boost.observe(NODE, crate::window::LOCAL_TEMPERATURE, 20.0, at(10.0));
    rig.boost.observe(NODE, crate::window::LOCAL_TEMPERATURE, 21.4, at(30.0));
    let end = 1.0 + minutes.ceil() + 1.0;
    let out = rig.tick(end, 6.0, (false, false), 21.0);
    assert_eq!(out.directives.get(ROOM), Some(&Directive::Switch));
    assert_eq!(rig.boost.view(ROOM)["state"], "measuring");
    assert!(rig.boost.next_deadline_ms().is_none());
    assert!(rig.tick(end + 30.0, 6.0, (false, false), 21.0).events.is_empty());
    let out = rig.tick(end + 60.0, 6.0, (false, false), 21.0);
    let d = detail(&out.events[0]);
    assert_eq!(out.events[0].kind, "boost_cycle");
    assert_eq!((d["mode"].as_str(), d["variant"].as_str(), d["scenario"].as_str(), d["source"].as_str(), d["end"].as_str(), d["room"].as_str()), (Some("live"), Some("boost_learn"), Some("morning"), Some("auto"), Some("complete"), Some(ROOM)));
    assert!((d["overshoot_c"].as_f64().unwrap() - 0.4).abs() < 1e-9 && (d["peak_c"].as_f64().unwrap() - 21.4).abs() < 1e-9);
    assert!((d["boost_min"].as_f64().unwrap() - (end - 1.0)).abs() < 1e-6 && d["chosen_min"].as_f64() == Some(minutes) && d["start_ts_ms"].as_i64() == Some(at(1.0)));
    for key in ["allowed_min", "d_safe_min", "known_min", "context", "decision", "p_overshoot", "target", "model_version", "cycle_id"] {
        assert!(d.get(key).is_some(), "{key}");
    }
    assert_eq!((d["context"]["hour"].as_f64(), d["context"]["supply_ok"].as_f64()), (Some(6.0), Some(1.0)));
    assert_eq!(out.events[0].experiment_id.as_deref(), d["cycle_id"].as_str());
    assert_eq!(rig.boost.view(ROOM)["state"], "idle");
    assert!(rig.tick(end + 61.0, 6.0, (false, false), 21.0).directives.is_empty());
}

#[test]
fn cycle_never_written_times_out_without_a_live_event() {
    let rig = Rig::new();
    assert!(is_boost(&rig.morning()));
    assert!(is_boost(&rig.tick(3.0, 6.0, (false, false), 21.0)));
    let out = rig.tick(5.0, 6.0, (false, false), 21.0);
    assert!(out.directives.is_empty());
    let d = detail(&out.events[0]);
    assert_eq!((d["mode"].as_str(), d["end"].as_str(), d["overshoot_c"].is_null()), (Some("aborted"), Some("not_written"), true));
}

#[test]
fn abort_paths_hand_back_or_step_aside() {
    type Case = (&'static str, f64, f64, Option<&'static str>, bool, &'static str);
    let cases: [Case; 7] = [
        ("sensor at the limit", 24.1, 21.0, None, true, "sensor_limit"),
        ("target decrease", 18.0, 19.5, None, true, "target_decrease"),
        ("target change", 18.0, 22.0, None, true, "target_changed"),
        ("window", 18.0, 21.0, Some("window_open"), false, "window_open"),
        ("away", 18.0, 21.0, Some("away"), false, "away"),
        ("manual change", 18.0, 21.0, Some("manual_hold"), false, "manual_hold"),
        ("stale sensor", f64::NAN, 21.0, None, true, "sensor_stale"),
    ];
    for (name, temp, setpoint, blocked, switch, end) in cases {
        let rig = Rig::new();
        rig.boosting();
        let room = RoomIn { blocked, ..Rig::room(setpoint) };
        let out = if temp.is_nan() {
            rig.boost.step(&StepIn { now_ms: at(30.0), hour: 6.0, outdoor: Some((5.0, None)), radiation: None, presence_known: true, sleep: false, away: false, rooms: &[room] })
        } else {
            rig.boost.observe(NODE, crate::window::LOCAL_TEMPERATURE, temp, at(2.0));
            rig.boost.step(&StepIn { now_ms: at(2.0), hour: 6.0, outdoor: Some((5.0, None)), radiation: None, presence_known: true, sleep: false, away: false, rooms: &[room] })
        };
        assert_eq!(out.directives.get(ROOM) == Some(&Directive::Switch), switch, "{name}");
        assert!(!is_boost(&out), "{name}");
        let d = detail(&out.events[0]);
        assert_eq!((d["end"].as_str(), d["mode"].as_str()), (Some(end), Some("live")), "{name}");
        assert_eq!(rig.boost.view(ROOM)["state"], "idle", "{name}");
        if end == "sensor_limit" {
            assert!(d["overshoot_c"].as_f64().unwrap() > 3.0);
        } else {
            assert!(d["overshoot_c"].is_null(), "{name}");
        }
    }
}

#[test]
fn manual_boost_is_tagged_manual_with_the_default_dose_and_one_cycle_at_a_time() {
    let rig = Rig::new();
    rig.tick(0.0, 6.0, (false, false), 21.0);
    let reply = rig.boost.request(ROOM, None, at(0.5));
    assert_eq!((reply["ok"].clone(), reply["scenario"].as_str(), reply["source"].as_str(), reply["state"].as_str(), reply["cycle"]["minutes"].as_f64(), reply["cycle"]["setpoint"].as_f64()), (true.into(), Some("manual"), Some("manual"), Some("armed"), Some(5.0), Some(24.0)), "{reply}");
    assert!(is_boost(&rig.tick(1.0, 6.0, (false, false), 21.0)));
    let again = rig.boost.request(ROOM, None, at(1.0));
    assert!(again["ok"] == false && again["reason"].as_str().unwrap().contains("laeuft bereits"));
}

#[test]
fn manual_dose_is_the_request_capped_at_twenty_minutes() {
    for (asked, expected) in [(Some(2.0), 2.0), (Some(12.0), 12.0), (Some(1000.0), 20.0), (None, 5.0)] {
        let rig = Rig::new();
        rig.tick(0.0, 6.0, (false, false), 21.0);
        let reply = rig.boost.request(ROOM, asked, at(0.5));
        assert_eq!(reply["cycle"]["minutes"].as_f64(), Some(expected), "{asked:?} {reply}");
    }
}

#[test]
fn manual_boost_refuses_unknown_rooms_stale_ticks_bad_minutes_and_lost_supply() {
    let rig = Rig::new();
    assert!(rig.boost.request(ROOM, None, at(0.0))["reason"].as_str().unwrap().contains("Regelschritt"));
    rig.tick(0.0, 6.0, (false, false), 21.0);
    assert!(rig.boost.request("keller", None, at(0.5))["reason"].as_str().unwrap().contains("unbekannter Raum"));
    assert!(rig.boost.request(ROOM, Some(-1.0), at(0.5))["reason"].as_str().unwrap().contains("groesser als 0"));
    assert!(rig.boost.request(ROOM, None, at(10.0))["reason"].as_str().unwrap().contains("Regelschritt"));
    assert_eq!(rig.boost.view(ROOM)["state"], "idle");
    let refused = rig.boost.step(&StepIn { now_ms: at(11.0), hour: 6.0, outdoor: Some((5.0, None)), radiation: None, presence_known: true, sleep: false, away: false, rooms: &[] }).events;
    assert_eq!(refused.len(), 2);
    let blocked = Rig::new();
    blocked.tick_with(0.0, 6.0, (false, false), RoomIn { no_supply: true, ..Rig::room(21.0) });
    assert!(blocked.boost.request(ROOM, None, at(0.5))["reason"].as_str().unwrap().contains("versorgung"));
}

#[test]
fn manual_cycles_are_logged_as_manual_and_unknown_presence_never_triggers() {
    let rig = Rig::new();
    rig.tick(0.0, 6.0, (false, false), 21.0);
    assert_eq!(rig.boost.request(ROOM, Some(3.0), at(0.5))["ok"], true);
    rig.boost.wrote(ROOM, "boost", at(0.6));
    rig.tick(4.0, 6.0, (false, false), 21.0);
    let out = rig.tick(100.0, 6.0, (false, false), 21.0);
    let d: Vec<Value> = out.events.iter().map(detail).collect();
    assert_eq!((d[0]["scenario"].as_str(), d[0]["source"].as_str(), d[0]["end"].as_str()), (Some("manual"), Some("manual"), Some("complete")), "{d:?}");
    let gap = Rig::new();
    gap.tick(0.0, 6.0, (true, false), 18.0);
    gap.boost.step(&StepIn { now_ms: at(0.5), hour: 6.0, outdoor: Some((5.0, None)), radiation: None, presence_known: false, sleep: false, away: false, rooms: &[Rig::room(18.0)] });
    assert!(is_boost(&gap.tick(1.0, 6.0, (false, false), 21.0)));
}

#[test]
fn no_supply_or_a_foreign_final_reason_invalidates_the_running_boost() {
    for how in ["no_supply", "overridden"] {
        let rig = Rig::new();
        rig.boosting();
        rig.feed(2.0, 19.0);
        let out = if how == "no_supply" {
            rig.tick_with(2.0, 6.0, (false, false), RoomIn { no_supply: true, ..Rig::room(21.0) })
        } else {
            rig.boost.settle(&[(ROOM, "no_supply")], at(2.0));
            rig.tick(2.0, 6.0, (false, false), 21.0)
        };
        let d: Vec<Value> = out.events.iter().map(detail).collect();
        assert_eq!((d.len(), d[0]["end"].as_str(), d[0]["overshoot_c"].is_null()), (1, Some(how), true), "{how} {d:?}");
        assert_eq!(rig.boost.view(ROOM)["state"], "idle");
    }
}

#[test]
fn reload_prefers_the_newest_valid_bundle_and_keeps_the_running_one() {
    let dir = tempfile::tempdir().unwrap();
    let text = std::fs::read_to_string(concat!(env!("CARGO_MANIFEST_DIR"), "/testdata/boost_parity.json")).unwrap();
    let bundle = serde_json::from_str::<Value>(&text).unwrap()["bundle"].to_string();
    let cfg = Config::parse(&format!("[boost]\nmodel_dir = '{}'\n[[rooms]]\nname = 'wohnzimmer'\nnodes = [5]\n", dir.path().to_str().unwrap().replace('\\', "/"))).unwrap();
    let boost = Boost::new(&cfg);
    assert!(boost.reload().is_err() && boost.summary()["model_version"].is_null());
    let put = |name: &str, body: &str| {
        std::fs::create_dir_all(dir.path().join(name)).unwrap();
        std::fs::write(dir.path().join(name).join("bundle.json"), body).unwrap();
    };
    put("20261101T000000Z", &bundle);
    assert!(boost.reload().unwrap());
    put("20261102T000000Z", "{broken");
    assert!(!boost.reload().unwrap());
    assert_eq!(boost.summary()["model_version"], "20261101T000000Z");
    put("20261103T000000Z", &bundle.replace("20261101T000000Z", "newer"));
    assert!(boost.reload().unwrap());
    assert_eq!(boost.summary()["model_version"], "newer");
}

#[test]
fn metrics_and_view_report_the_state() {
    let rig = Rig::new();
    rig.boosting();
    let m = rig.boost.metrics();
    assert!(m.contains("heatd_boost_model_loaded 1") && m.contains("heatd_boost_active{room=\"wohnzimmer\"} 1") && m.contains("heatd_boost_active{room=\"bad\"} 0") && m.contains("heatd_boost_cycles_total{room=\"wohnzimmer\"} 1"), "{m}");
    assert!(Rig::bare().boost.metrics().contains("heatd_boost_model_loaded 0"));
}
