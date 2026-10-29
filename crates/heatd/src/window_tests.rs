use crate::config::Config;
use crate::window::*;
use serde_json::json;

struct Sim {
    w: Windows,
    c: Config,
    t: i64,
    out: Option<(f64, Option<f64>)>,
    events: Vec<WinEvent>,
}

fn flat(v: f64, n: usize) -> Vec<f64> {
    vec![v; n]
}

fn ramp(from: f64, per_min: f64, n: usize) -> Vec<f64> {
    (1..=n).map(|i| from + per_min * i as f64).collect()
}

impl Sim {
    fn new(extra: &str) -> Self {
        let c = Config::parse(&format!("{extra}\n[[rooms]]\nname = 'a'\nnodes = [5]\n")).unwrap();
        Self { w: Windows::default(), c, t: 1_000_000_000_000, out: Some((2.0, None)), events: Vec::new() }
    }

    fn valve(&mut self, v: f64) {
        self.w.observe(&self.c, 5, PI_HEATING_DEMAND, v, self.t);
    }

    fn run(&mut self, temps: Vec<f64>) {
        for temp in temps {
            self.t += 60_000;
            self.w.observe(&self.c, 5, LOCAL_TEMPERATURE, temp, self.t);
            let found = self.w.step(&self.c, self.t, self.out);
            self.events.extend(found);
        }
    }

    fn kinds(&self) -> Vec<&'static str> {
        self.events.iter().map(|e| e.kind).collect()
    }

    fn find(&self, kind: &str) -> &WinEvent {
        self.events.iter().find(|e| e.kind == kind).unwrap_or_else(|| panic!("{kind} missing in {:?}", self.events))
    }

    fn state(&self) -> String {
        self.w.view("a", self.t)["state"].as_str().unwrap().into()
    }
}

fn opened() -> Sim {
    let mut s = Sim::new("");
    s.valve(0.0);
    s.run(flat(21.0, 15));
    s.run(ramp(21.0, -0.4, 4));
    s
}

#[test]
fn sharp_drop_with_cold_outside_opens_and_recovery_closes() {
    let mut s = opened();
    assert_eq!(s.kinds(), vec!["window_open"]);
    let d = &s.find("window_open").detail;
    assert!(d["drop_k"].as_f64().unwrap() >= 1.0 && d["source"] == "auto" && d["outdoor"] == 2.0 && d["valve_before"] == 0.0 && d["room"] == "a", "{d}");
    assert_eq!(s.w.open_rooms().len(), 1);
    s.run(flat(19.4, 6));
    assert_eq!(s.state(), "open");
    s.run(ramp(19.4, 0.2, 3));
    assert_eq!(s.kinds(), vec!["window_open", "window_close"]);
    let d = &s.find("window_close").detail;
    assert_eq!(d["reason"], "recovery");
    assert!((d["min_temp"].as_f64().unwrap() - 19.4).abs() < 1e-9 && d["open_minutes"].as_f64().unwrap() >= 5.0, "{d}");
    assert!(s.w.open_rooms().is_empty());
}

#[test]
fn recovery_waits_for_minimum_open_time_and_for_heating_off() {
    let mut s = opened();
    s.run(vec![19.4, 19.8, 20.0]);
    assert_eq!(s.state(), "open");
    s.valve(40.0);
    s.run(flat(20.2, 8));
    assert_eq!(s.state(), "open");
    s.valve(0.0);
    s.run(flat(20.4, 1));
    assert_eq!(s.state(), "closed");
}

#[test]
fn radiator_cooldown_after_valve_close_is_not_a_window() {
    let mut s = Sim::new("");
    s.run(flat(22.0, 12));
    s.valve(40.0);
    s.run(flat(22.0, 3));
    s.valve(0.0);
    for i in 0..3 {
        s.w.seed_cooldown(5, s.t - 3_600_000 * (i + 1), 4.0);
    }
    s.run(ramp(22.0, -0.12, 10));
    s.run(flat(20.8, 10));
    assert!(!s.kinds().contains(&"window_open"), "{:?}", s.events);
    assert_eq!(s.find("window_suppressed").detail["reason"], "cooldown");
}

#[test]
fn drop_much_faster_than_cooldown_still_opens() {
    let mut s = Sim::new("");
    s.run(flat(22.0, 12));
    s.valve(40.0);
    s.run(flat(22.0, 3));
    s.valve(0.0);
    s.run(ramp(22.0, -0.5, 4));
    assert_eq!(s.kinds(), vec!["window_open"]);
    assert_eq!(s.find("window_open").detail["valve_before"], 40.0);
}

#[test]
fn warm_outside_blocks_detection() {
    let mut s = Sim::new("");
    s.out = Some((17.0, None));
    s.run(flat(21.0, 15));
    s.run(ramp(21.0, -0.4, 6));
    assert!(!s.kinds().contains(&"window_open"));
    assert_eq!(s.find("window_suppressed").detail["reason"], "outdoor");
    let mut s = Sim::new("");
    s.out = None;
    s.run(flat(21.0, 15));
    s.run(ramp(21.0, -0.4, 4));
    assert_eq!(s.kinds(), vec!["window_open"]);
}

#[test]
fn own_setpoint_decrease_blocks_detection() {
    let mut s = Sim::new("");
    s.run(flat(21.0, 15));
    s.w.note_decrease("a", s.t);
    s.run(ramp(21.0, -0.4, 6));
    assert!(!s.kinds().contains(&"window_open"));
    assert_eq!(s.find("window_suppressed").detail["reason"], "setpoint");
}

#[test]
fn sustained_slope_alone_opens() {
    let mut s = Sim::new("[window]\nopen_drop_k = 100.0");
    s.run(flat(21.0, 10));
    s.run(ramp(21.0, -0.1, 12));
    assert_eq!(s.kinds(), vec!["window_open"]);
    assert!(s.find("window_open").detail["slope"].as_f64().unwrap() <= -4.0);
}

#[test]
fn wind_lowers_thresholds() {
    let mut calm = Sim::new("[window]\nopen_slope_k_per_h = -100.0");
    calm.out = Some((2.0, Some(3.0)));
    let mut windy = Sim::new("[window]\nopen_slope_k_per_h = -100.0");
    windy.out = Some((2.0, Some(8.0)));
    for s in [&mut calm, &mut windy] {
        s.run(flat(21.0, 15));
        s.run(vec![20.5, 20.15, 20.15]);
    }
    assert!(calm.kinds().is_empty());
    assert_eq!(windy.kinds(), vec!["window_open"]);
    assert_eq!(windy.find("window_open").detail["wind"], 8.0);
}

#[test]
fn timeout_probe_reopens_with_doubled_timeout_while_temperature_keeps_falling() {
    let mut s = opened();
    s.run(flat(19.4, 21));
    assert_eq!(s.w.view("a", s.t)["state"], "probe");
    assert!(s.w.open_rooms().is_empty());
    s.valve(30.0);
    s.run(ramp(19.4, -0.12, 6));
    assert_eq!(s.kinds(), vec!["window_open", "window_reopen"]);
    assert_eq!(s.find("window_reopen").detail["timeout_min"], 40.0);
    assert_eq!(s.state(), "open");
    assert_eq!(s.w.view("a", s.t)["reopens"], 1);
}

#[test]
fn timeout_probe_with_stable_temperature_closes() {
    let mut s = opened();
    s.run(flat(19.4, 21));
    s.valve(30.0);
    s.run(flat(19.5, 11));
    assert_eq!(s.kinds(), vec!["window_open", "window_close"]);
    assert_eq!(s.find("window_close").detail["reason"], "timeout_probe");
    assert_eq!(s.state(), "closed");
}

#[test]
fn reopen_timeout_is_capped() {
    let mut s = opened();
    let mut level = 19.4;
    for wait in [21, 41, 61, 61] {
        s.valve(0.0);
        s.run(flat(level, wait));
        s.valve(30.0);
        s.run(ramp(level, -0.12, 6));
        level -= 0.72;
    }
    let timeouts: Vec<f64> = s.events.iter().filter(|e| e.kind == "window_reopen").map(|e| e.detail["timeout_min"].as_f64().unwrap()).collect();
    assert_eq!(timeouts, vec![40.0, 60.0, 60.0, 60.0]);
}

#[test]
fn flapping_limit_suppresses_further_automatic_openings() {
    let mut s = Sim::new("");
    for _ in 0..5 {
        s.run(flat(21.0, 3));
        s.run(ramp(21.0, -0.4, 4));
        s.w.manual("a", false, s.t);
    }
    assert_eq!(s.kinds().iter().filter(|k| **k == "window_open").count(), 4);
    assert_eq!(s.find("window_suppressed").detail["reason"], "flapping");
    let v = s.w.view("a", s.t);
    assert_eq!((v["auto_opens"].as_u64(), v["suppressed"].as_u64().is_some_and(|n| n >= 1)), (Some(4), true));
}

#[test]
fn manual_override_opens_and_closes_regardless_of_temperature() {
    let mut s = Sim::new("");
    s.run(flat(21.0, 5));
    let e = s.w.manual("a", true, s.t).unwrap();
    assert_eq!((e.kind, e.detail["source"].clone()), ("window_open", json!("manual")));
    assert!(s.w.manual("a", true, s.t).is_none());
    s.run(ramp(21.0, 0.3, 30));
    assert_eq!(s.state(), "open");
    assert_eq!(s.w.view("a", s.t)["source"], "manual");
    let e = s.w.manual("a", false, s.t).unwrap();
    assert_eq!((e.kind, e.detail["reason"].clone()), ("window_close", json!("manual")));
    assert!(s.w.manual("a", false, s.t).is_none());
    assert!(s.w.open_rooms().is_empty());
    let mut s = opened();
    assert_eq!(s.w.manual("a", false, s.t).unwrap().detail["reason"], "manual");
}

#[test]
fn restart_starts_closed_and_disabled_detects_nothing() {
    let mut s = opened();
    assert_eq!(s.state(), "open");
    s.w = Windows::default();
    assert_eq!(s.state(), "closed");
    assert!(s.w.open_rooms().is_empty());
    let mut off = Sim::new("[window]\nenabled = false");
    off.run(flat(21.0, 15));
    off.run(ramp(21.0, -0.4, 6));
    assert!(off.events.is_empty());
}

#[test]
fn metrics_expose_state_per_room() {
    let s = opened();
    let m = s.w.metrics();
    assert!(m.contains("heatd_window_state{room=\"a\"} 1") && m.contains("heatd_window_auto_opens_total{room=\"a\"} 1"), "{m}");
}
