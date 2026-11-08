use crate::config::Config;
use crate::window::*;
use crate::window_model_tests::{toy_bundle, toy_bundle_with};
use serde_json::json;

const MIN: i64 = 60_000;
const T0: i64 = 1_800_000_000_000 - 1_800_000_000_000 % MIN;

struct Sim {
    w: Windows,
    c: Config,
    t: i64,
    nodes: Vec<u64>,
    steady: Option<f64>,
    events: Vec<WinEvent>,
}

fn flat(v: f64, n: usize) -> Vec<f64> {
    vec![v; n]
}

fn ramp(from: f64, per_min: f64, n: usize) -> Vec<f64> {
    (1..=n).map(|i| from + per_min * i as f64).collect()
}

impl Sim {
    fn single(extra: &str) -> Self {
        let c = Config::parse(&format!("{extra}\n[[rooms]]\nname = 'a'\nnodes = [5]\n")).unwrap();
        let mut s = Self { w: Windows::default(), c, t: T0, nodes: vec![5], steady: None, events: Vec::new() };
        s.w.observe(&s.c, 5, PI_HEATING_DEMAND, 0.0, s.t - 50 * MIN);
        s
    }

    fn house(extra: &str) -> Self {
        let c = Config::parse(&format!("{extra}\n[building]\nroof_rooms = ['schlafzimmer']\n[[rooms]]\nname = 'wohnzimmer'\nnodes = [5]\n[[rooms]]\nname = 'schlafzimmer'\nnodes = [6]\n[[rooms]]\nname = 'kueche'\nnodes = [8]\n")).unwrap();
        let mut w = Windows::default();
        w.set_model(toy_bundle(&[("wohnzimmer", 5, 1.0), ("schlafzimmer", 6, 0.3), ("kueche", 8, 1.0)], 3.0));
        Self { w, c, t: T0, nodes: vec![5, 6, 8], steady: Some(40.0), events: Vec::new() }
    }

    fn valve(&mut self, v: f64) {
        self.w.observe(&self.c, 5, PI_HEATING_DEMAND, v, self.t);
    }

    fn minute(&mut self, temps: &[f64]) {
        self.t += MIN;
        for (node, temp) in self.nodes.clone().into_iter().zip(temps) {
            self.w.observe(&self.c, node, LOCAL_TEMPERATURE, *temp, self.t);
            if let Some(valve) = self.steady {
                self.w.observe(&self.c, node, PI_HEATING_DEMAND, valve, self.t);
                self.w.observe(&self.c, node, OCCUPIED_SETPOINT, 21.0, self.t);
            }
        }
        let found = self.w.step(&self.c, self.t, |_| [5.0, 2.0, 0.0]);
        self.events.extend(found);
    }

    fn run(&mut self, temps: Vec<f64>) {
        for temp in temps {
            self.minute(&vec![temp; self.nodes.len()]);
        }
    }

    fn drops(&mut self, per_min: [f64; 3], n: usize) {
        for k in 1..=n {
            self.minute(&[20.0 - per_min[0] * k as f64, 20.0 - per_min[1] * k as f64, 20.0 - per_min[2] * k as f64]);
        }
    }

    fn kinds(&self) -> Vec<&'static str> {
        self.events.iter().map(|e| e.kind).collect()
    }

    fn of(&self, kind: &str) -> Vec<&WinEvent> {
        self.events.iter().filter(|e| e.kind == kind).collect()
    }

    fn open(&self) -> Vec<String> {
        let mut rooms: Vec<String> = self.w.open_rooms().into_iter().collect();
        rooms.sort();
        rooms
    }

    fn warm(extra: &str) -> Self {
        let mut s = Self::house(extra);
        s.run(flat(20.0, 30));
        s
    }
}

#[test]
fn a_house_alarm_opens_the_contributing_rooms_and_the_hold_times_out() {
    let mut s = Sim::warm("");
    assert!(s.events.is_empty());
    s.drops([0.3, 0.0, 0.3], 6);
    assert_eq!(s.kinds(), vec!["window_open", "window_open"], "{:?}", s.events);
    assert_eq!(s.open(), vec!["kueche", "wohnzimmer"]);
    let d = &s.of("window_open")[0].detail;
    assert_eq!((d["detector"].as_str(), d["source"].as_str(), d["bundle"].as_str()), (Some("v3"), Some("auto"), Some("toy")));
    assert!(d["house"].as_f64().unwrap() > d["threshold"].as_f64().unwrap() && d["threshold"] == 3.0);
    assert_eq!(d["rooms"], json!(["wohnzimmer", "kueche"]));
    assert!(d["z"]["wohnzimmer"].as_f64().unwrap() >= 1.0 && d["z"]["schlafzimmer"].as_f64().unwrap().abs() < 1e-9);
    let opened = s.t;
    for k in 7..=25 {
        s.minute(&[20.0 - 0.3 * k as f64, 20.0, 20.0 - 0.3 * k as f64]);
    }
    assert_eq!((s.open().len(), s.t - opened), (2, 19 * MIN), "still held after 19 min");
    s.minute(&[12.0, 20.0, 12.0]);
    assert_eq!(s.t - opened, 20 * MIN);
    let closes = s.of("window_close");
    assert_eq!(closes.len(), 2, "{:?}", s.events);
    assert!(closes.iter().all(|e| e.detail["reason"] == "timeout" && e.detail["open_minutes"].as_f64().unwrap() >= 20.0));
    assert!(s.open().is_empty());
}

#[test]
fn a_closed_window_is_not_rearmed_for_thirty_minutes() {
    let mut s = Sim::warm("");
    s.drops([0.3, 0.0, 0.3], 80);
    let opens: Vec<i64> = s.events.iter().enumerate().filter(|(_, e)| e.kind == "window_open" && e.room == "wohnzimmer").map(|(i, _)| i as i64).collect();
    assert_eq!(opens.len(), 2, "open, hold 20 min, rearm 30 min, open again: {:?}", s.kinds());
    assert!(s.of("window_suppressed").iter().any(|e| e.detail["reason"] == "rearm"));
}

#[test]
fn only_rooms_above_the_room_level_get_frost_and_the_attic_never_does() {
    let mut s = Sim::warm("");
    s.drops([0.4, 0.0, 0.05], 8);
    assert_eq!(s.open(), vec!["wohnzimmer"], "{:?}", s.events);
    let mut s = Sim::warm("");
    s.drops([0.3, 0.3, 0.3], 6);
    assert_eq!(s.open(), vec!["kueche", "wohnzimmer"], "the roof room contributes score but is not opened");
    let mut s = Sim::warm("[window]\nroom_z = 100.0");
    s.drops([0.4, 0.0, 0.05], 8);
    assert_eq!(s.open(), vec!["kueche", "wohnzimmer"], "no room above the level: every ground floor thermostat room");
}

#[test]
fn vetoes_keep_single_rooms_closed() {
    let mut s = Sim::warm("");
    s.w.note_decrease("wohnzimmer", s.t);
    s.drops([0.3, 0.0, 0.3], 6);
    assert_eq!(s.open(), vec!["kueche"]);
    assert!(s.of("window_suppressed").iter().any(|e| e.room == "wohnzimmer" && e.detail["reason"] == "setpoint"));
    let mut s = Sim::warm("");
    s.w.set_context([("wohnzimmer".to_string(), RoomCtx { hold: true, away: false })]);
    s.drops([0.3, 0.0, 0.3], 6);
    assert_eq!(s.open(), vec!["kueche"]);
    assert!(s.of("window_suppressed").iter().any(|e| e.detail["reason"] == "manual_hold"));
    let mut s = Sim::warm("");
    s.w.set_context(["wohnzimmer", "kueche"].map(|r| (r.to_string(), RoomCtx { hold: false, away: true })));
    s.drops([0.3, 0.0, 0.3], 6);
    assert!(s.open().is_empty() && s.of("window_open").is_empty());
    assert_eq!(s.of("window_suppressed").len(), 2);
}

#[test]
fn suppressed_window_alarms_still_block_boosts() {
    let mut s = Sim::warm("");
    s.w.note_decrease("wohnzimmer", s.t);
    s.drops([0.3, 0.0, 0.3], 6);
    assert!(s.w.open_ago_min("wohnzimmer", s.t).is_some_and(|a| a < 1.0));
    assert!(s.w.open_ago_min("kueche", s.t).is_some_and(|a| a < 1.0));
    for ctx in [RoomCtx { hold: true, away: false }, RoomCtx { hold: false, away: true }] {
        let mut s = Sim::warm("");
        s.w.set_context(["wohnzimmer", "kueche"].map(|r| (r.to_string(), ctx.clone())));
        s.drops([0.3, 0.0, 0.3], 6);
        assert!(s.open().is_empty() && s.w.open_ago_min("wohnzimmer", s.t).is_some(), "{ctx:?}");
    }
    let quiet = Sim::warm("");
    assert_eq!(quiet.w.open_ago_min("wohnzimmer", quiet.t), None);
}

fn supply_incident(extra: &str) -> Sim {
    let mut s = Sim::single(extra);
    s.w.set_model(toy_bundle(&[("a", 5, 1.0)], 3.0));
    s.steady = Some(100.0);
    s.run(flat(19.0, 5));
    s.run(ramp(19.0, -0.0155, 120));
    s
}

#[test]
fn a_boiler_supply_outage_vetoes_the_alarm() {
    let mut s = supply_incident("");
    assert!(s.w.no_supply_rooms().contains("a"));
    s.run(ramp(17.14, -0.2, 25));
    assert!(s.of("window_open").is_empty(), "{:?}", s.events);
    let mut off = supply_incident("[supply]
enabled = false");
    off.run(ramp(17.14, -0.2, 25));
    assert!(!off.of("window_open").is_empty(), "the same drop without the outage is an alarm");
}

#[test]
fn no_bundle_or_disabled_means_no_detection_and_heating_is_unaffected() {
    let mut s = Sim::warm("");
    s.w = Windows::default();
    s.drops([0.3, 0.0, 0.3], 30);
    assert!(s.events.is_empty() && s.open().is_empty());
    assert_eq!(s.w.view("wohnzimmer", s.t)["state"], "closed");
    let mut off = Sim::warm("[window]\nenabled = false");
    off.drops([0.3, 0.0, 0.3], 30);
    assert!(off.events.is_empty());
}

#[test]
fn a_watch_answer_that_says_closed_ends_an_auto_window_but_open_changes_nothing() {
    let mut s = Sim::warm("");
    s.drops([0.3, 0.0, 0.3], 6);
    let prompt = s.w.prompt(&s.c.window, s.t).unwrap();
    assert_eq!((prompt["question"].as_str(), prompt["room"].as_str().is_some_and(|r| r == "wohnzimmer" || r == "kueche")), (Some("open"), true));
    let id = prompt["id"].as_str().unwrap().to_string();
    assert!(s.w.label("wohnzimmer", s.t, true, Some(&id), Some("l1"), s.t).is_none());
    assert_eq!(s.open().len(), 2);
    let e = s.w.label("wohnzimmer", s.t, false, Some(&id), Some("l2"), s.t).unwrap();
    assert_eq!((e.kind, e.detail["reason"].clone()), ("window_close", json!("label")));
    assert_eq!(s.open(), vec!["kueche"]);
    assert!(s.w.label("wohnzimmer", s.t, false, Some(&id), Some("l3"), s.t).is_none());
}

#[test]
fn a_closed_episode_is_reported_after_the_label_grace_with_the_score() {
    let mut s = Sim::warm("");
    s.drops([0.3, 0.0, 0.3], 6);
    s.w.manual("kueche", false, s.t);
    s.w.manual("wohnzimmer", false, s.t);
    assert!(s.of("window_episode").is_empty());
    s.run(flat(18.5, 26));
    let episodes = s.of("window_episode");
    assert_eq!(episodes.len(), 2, "{:?}", s.kinds());
    let d = &episodes[0].detail;
    assert_eq!((d["detector"].as_str(), d["source"].as_str(), d["end_reason"].as_str(), d["label"].as_str()), (Some("v3"), Some("auto"), Some("manual"), Some("none")));
    assert!(d["house"].as_f64().unwrap() > 3.0);
}

#[test]
fn manual_override_opens_and_closes_regardless_of_temperature() {
    let mut s = Sim::single("");
    s.run(flat(21.0, 5));
    let e = s.w.manual("a", true, s.t).unwrap();
    assert_eq!((e.kind, e.detail["source"].clone()), ("window_open", json!("manual")));
    assert!(s.w.manual("a", true, s.t).is_none());
    s.run(ramp(21.0, 0.3, 30));
    assert_eq!(s.w.view("a", s.t)["state"], "open");
    assert_eq!(s.w.view("a", s.t)["source"], "manual");
    let e = s.w.manual("a", false, s.t).unwrap();
    assert_eq!((e.kind, e.detail["reason"].clone()), ("window_close", json!("manual")));
    assert!(s.w.manual("a", false, s.t).is_none());
    assert!(s.w.open_rooms().is_empty());
}

#[test]
fn manual_open_times_out() {
    let mut s = Sim::single("[window]\nmanual_max_min = 30.0");
    s.run(flat(21.0, 5));
    s.w.manual("a", true, s.t);
    s.run(flat(21.0, 29));
    assert_eq!(s.w.view("a", s.t)["state"], "open");
    s.run(flat(21.0, 2));
    assert_eq!(s.w.view("a", s.t)["state"], "closed");
    assert_eq!(s.of("window_close")[0].detail["reason"], "manual_timeout");
}

#[test]
fn metrics_expose_state_and_evidence_per_room() {
    let mut s = Sim::warm("");
    s.drops([0.3, 0.0, 0.3], 6);
    let m = s.w.metrics();
    assert!(m.contains("heatd_window_state{room=\"wohnzimmer\"} 1") && m.contains("heatd_window_auto_opens_total{room=\"kueche\"} 1") && m.contains("heatd_window_z{room=\"wohnzimmer\"} 1.") && m.contains("heatd_window_house_score "), "{m}");
    assert_eq!(s.w.view("wohnzimmer", s.t)["model"], "toy");
}

#[test]
fn restart_replays_the_lake_so_the_stream_is_warm_again() {
    let dir = tempfile::tempdir().unwrap();
    let sink = crate::lake::Sink::open(crate::lake::LakeConfig::new(dir.path())).unwrap();
    let now_ms = crate::lake::now_us() / 1000 / MIN * MIN;
    let row = |min_ago: i64, attribute: u32, v: i64| crate::lake::AttributeReport { ts: (now_ms - min_ago * MIN) * 1000, node: 5, cluster: 513, attribute, dtype: "int".into(), v_i64: Some(v), source: "poll".into(), fw: "1".into(), ..Default::default() };
    for min_ago in 0..60 {
        for (attribute, v) in [(0, 2000), (8, 40), (18, 2100)] {
            sink.append(row(min_ago, attribute, v)).unwrap();
        }
    }
    sink.flush().unwrap();
    let mut s = Sim::single("");
    s.w = Windows::default();
    s.w.set_model(toy_bundle(&[("a", 5, 1.0)], 3.0));
    assert_eq!(s.w.rebuild(&s.c, dir.path(), now_ms), 180);
    assert!(s.w.view("a", now_ms)["z"].is_null() && s.w.open_rooms().is_empty() && s.events.is_empty());
    s.t = now_ms;
    s.steady = Some(40.0);
    s.run(flat(20.0, 1));
    assert!(s.w.view("a", s.t)["z"].is_null(), "no weather yet in the replayed history");
    s.run(flat(20.0, 20));
    assert!(s.w.view("a", s.t)["z"].as_f64().is_some(), "{}", s.w.view("a", s.t));
}

fn incident() -> Sim {
    let mut s = Sim::single("");
    s.run(flat(19.0, 5));
    s.valve(100.0);
    s.run(ramp(19.0, -0.0155, 120));
    s
}

fn supply_events(s: &Sim) -> Vec<&WinEvent> {
    s.events.iter().filter(|e| e.kind == "supply").collect()
}

#[test]
fn valve_wide_open_without_temperature_rise_is_no_supply() {
    let s = incident();
    let ev = supply_events(&s);
    assert_eq!(ev.len(), 1, "{:?}", s.events);
    assert_eq!(ev[0].detail["state"], "start");
    assert!(s.w.no_supply_rooms().contains("a"));
    assert_eq!(s.w.view("a", s.t)["no_supply_since"].as_i64(), ev[0].detail["since_ms"].as_i64());
}

#[test]
fn rising_radiator_with_wide_open_valve_is_normal_heating() {
    let mut s = Sim::single("");
    s.run(flat(19.0, 5));
    s.valve(100.0);
    s.run(ramp(19.0, 0.03, 120));
    assert!(supply_events(&s).is_empty());
}

#[test]
fn no_supply_ends_when_the_radiator_heats_up_again() {
    let mut s = incident();
    s.run(ramp(17.1, 0.1, 20));
    let ev = supply_events(&s);
    assert_eq!(ev.len(), 2, "{:?}", s.events);
    assert_eq!(ev[1].detail["state"], "end");
    assert!(ev[1].detail["duration_min"].as_f64().unwrap() > 100.0);
    assert!(s.w.no_supply_rooms().is_empty());
}

#[test]
fn learned_supply_window_is_the_median_of_episodes() {
    use chrono::{Local, TimeZone};
    let ms = |d: u32, h: u32, m: u32| Local.with_ymd_and_hms(2026, 10, d, h, m, 0).unwrap().timestamp_millis();
    let eps = vec![(ms(20, 23, 40), ms(21, 6, 40)), (ms(21, 23, 45), ms(22, 6, 45)), (ms(22, 23, 50), ms(23, 6, 50))];
    let v = supply_window(&eps);
    assert_eq!((v["from"].as_str(), v["to"].as_str()), (Some("23:45"), Some("06:45")));
    assert!(v["label"].as_str().unwrap().contains("Kessel liefert nicht: ~23:45-06:45"));
    assert!(supply_window(&eps[..1]).is_null());
}

#[test]
fn a_flat_stale_temperature_never_starts_no_supply() {
    let mut s = Sim::single("");
    s.run(flat(19.0, 5));
    s.valve(100.0);
    s.run(flat(19.0, 10));
    for _ in 0..120 {
        s.t += MIN;
        let found = s.w.step(&s.c, s.t, |_| [f64::NAN; 3]);
        s.events.extend(found);
    }
    assert!(supply_events(&s).is_empty() && s.w.no_supply_rooms().is_empty(), "{:?}", s.events);
    s.run(flat(17.0, 4));
    assert!(s.kinds().is_empty(), "{:?}", s.events);
}

#[test]
fn stale_node_data_never_triggers_a_pi_reset() {
    let s = Sim::single("");
    let mut w = Windows::default();
    let t = s.t;
    w.observe(&s.c, 5, LOCAL_TEMPERATURE, 22.0, t);
    w.observe(&s.c, 5, PI_HEATING_DEMAND, 100.0, t);
    assert!(w.overshoot_due(&s.c.supply, 5, 20.0, t, 900_000).is_none());
    assert!(w.overshoot_due(&s.c.supply, 5, 20.0, t + 16 * MIN, 36_000_000).is_some());
    let mut w = Windows::default();
    w.observe(&s.c, 5, LOCAL_TEMPERATURE, 22.0, t);
    w.observe(&s.c, 5, PI_HEATING_DEMAND, 100.0, t);
    w.overshoot_due(&s.c.supply, 5, 20.0, t, 900_000);
    assert!(w.overshoot_due(&s.c.supply, 5, 20.0, t + 16 * MIN, 900_000).is_none());
    assert!(w.local_temp(5, t + 16 * MIN, 900_000).is_none() && w.local_temp(5, t + MIN, 900_000) == Some(22.0));
}

fn channel_sim(thresholds: &[(&str, f64)]) -> Sim {
    let mut s = Sim::house("");
    s.w.set_model(toy_bundle_with(&[("wohnzimmer", 5, 1.0), ("schlafzimmer", 6, 0.3), ("kueche", 8, 1.0)], thresholds));
    s.run(flat(20.0, 30));
    s
}

#[test]
fn a_room_channel_alarm_opens_only_that_room_with_its_reason_in_event_and_prompt() {
    let mut s = channel_sim(&[("house", 100.0), ("wohnzimmer", 3.0)]);
    s.drops([0.3, 0.0, 0.0], 12);
    assert_eq!(s.open(), vec!["wohnzimmer"], "{:?}", s.events);
    let opens = s.of("window_open");
    assert_eq!(opens.len(), 1);
    let d = &opens[0].detail;
    assert_eq!((d["channel"].as_str(), d["threshold"].as_f64(), d["rooms"].clone()), (Some("wohnzimmer"), Some(3.0), json!(["wohnzimmer"])));
    let reason = d["reason"].as_str().unwrap();
    assert!(reason.starts_with("wohnzimmer ") && reason.contains("K below forecast") && reason.contains("% open"), "{reason}");
    let prompt = s.w.prompt(&s.c.window, s.t).unwrap();
    assert_eq!((prompt["room"].as_str(), prompt["reason"].as_str()), (Some("wohnzimmer"), Some(reason)));
}

#[test]
fn a_house_alarm_gives_every_opened_room_its_own_reason_and_the_top_room_the_prompt() {
    let mut s = Sim::warm("");
    s.drops([0.3, 0.0, 0.3], 6);
    let opens = s.of("window_open");
    assert_eq!(opens.len(), 2);
    assert!(opens.iter().all(|e| e.detail["channel"] == "house" && e.detail["reason"].as_str().unwrap().starts_with("house score ") && e.detail["reason"].as_str().unwrap().contains(e.room.as_str())));
    let prompt = s.w.prompt(&s.c.window, s.t).unwrap();
    assert!(opens.iter().any(|e| e.detail["reason"] == prompt["reason"]));
}

#[test]
fn a_room_alarm_respects_the_vetoes_of_that_room() {
    let mut s = channel_sim(&[("house", 100.0), ("wohnzimmer", 3.0)]);
    s.w.note_decrease("wohnzimmer", s.t);
    s.drops([0.3, 0.0, 0.0], 12);
    assert!(s.open().is_empty() && s.of("window_suppressed").iter().any(|e| e.room == "wohnzimmer" && e.detail["reason"] == "setpoint"), "{:?}", s.events);
}

#[test]
fn open_ago_covers_the_house_channel_close_and_a_restored_event() {
    let mut s = Sim::warm("");
    assert_eq!(s.w.open_ago_min("kueche", s.t), None);
    s.drops([0.3, 0.0, 0.3], 6);
    assert_eq!(s.w.open_ago_min("kueche", s.t), Some(0.0));
    assert_eq!(s.w.open_ago_min("schlafzimmer", s.t), None);
    for k in 7..=26 {
        s.minute(&[20.0 - 0.3 * k as f64, 20.0, 20.0 - 0.3 * k as f64]);
    }
    assert!(s.open().is_empty());
    let closed = s.t;
    assert_eq!(s.w.open_ago_min("kueche", closed + 30 * MIN), Some(30.0));
    s.w.note_open("schlafzimmer", closed - 10 * MIN);
    assert_eq!(s.w.open_ago_min("schlafzimmer", closed), Some(10.0));
    s.w.note_open("schlafzimmer", closed - 50 * MIN);
    assert_eq!(s.w.open_ago_min("schlafzimmer", closed), Some(10.0));
}
