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
        let mut s = Self { w: Windows::default(), c, t: 1_000_000_000_000, out: Some((2.0, None)), events: Vec::new() };
        s.w.observe(&s.c, 5, PI_HEATING_DEMAND, 0.0, s.t - 50 * 60_000);
        s
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

    fn idle(&mut self, minutes: usize) {
        for _ in 0..minutes {
            self.t += 60_000;
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
    opened_with("")
}

fn opened_with(extra: &str) -> Sim {
    let mut s = Sim::new(extra);
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
    s.run(flat(19.4, 13));
    assert_eq!(s.state(), "open");
    s.run(ramp(19.4, 0.2, 6));
    assert_eq!(s.kinds(), vec!["window_open", "window_close"]);
    let d = &s.find("window_close").detail;
    assert_eq!(d["reason"], "recovery");
    assert!((d["min_temp"].as_f64().unwrap() - 19.4).abs() < 1e-9 && d["open_minutes"].as_f64().unwrap() >= 15.0, "{d}");
    assert!(s.w.open_rooms().is_empty());
}

#[test]
fn recovery_waits_for_minimum_open_time_and_for_heating_off() {
    let mut s = opened_with("[window]
min_open_hold_min = 0.0
close_debounce_min = 0.0");
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
    s.run(ramp(22.0, -1.5, 4));
    assert_eq!(s.kinds().last(), Some(&"window_open"));
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
    assert_eq!(s.w.active_rooms().len(), 1);
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
    assert_eq!(s.w.samples(5).len(), 1);
    assert_eq!(s.kinds(), vec!["window_open", "window_close"]);
    assert_eq!(s.find("window_close").detail["reason"], "timeout_probe");
    assert_eq!(s.state(), "closed");
}

#[test]
fn reopen_extends_the_probe_interval_to_at_least_the_probe_cooldown() {
    let mut s = opened_with("[window]
probe_cooldown_min = 50");
    s.run(flat(19.4, 21));
    s.valve(30.0);
    s.run(ramp(19.4, -0.12, 6));
    assert_eq!(s.find("window_reopen").detail["timeout_min"], 50.0);
    s.valve(0.0);
    s.run(flat(18.7, 45));
    assert_eq!(s.state(), "open");
    s.run(flat(18.7, 8));
    assert_eq!(s.state(), "probe");
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

fn trace() -> Vec<(i64, u64, String, f64)> {
    include_str!("../testdata/window_real_2026-10-29.csv")
        .lines()
        .skip(1)
        .map(|l| {
            let f: Vec<&str> = l.split(',').collect();
            (f[0].parse().unwrap(), f[1].parse().unwrap(), f[2].to_string(), f[3].parse().unwrap())
        })
        .collect()
}

fn replay(extra: &str) -> Vec<WinEvent> {
    let rooms: String = [5, 6, 7, 8].iter().map(|n| format!("[[rooms]]\nname = 'r{n}'\nnodes = [{n}]\n")).collect();
    let c = Config::parse(&format!("{extra}\n{rooms}")).unwrap();
    let mut w = Windows::default();
    let rows = trace();
    let (mut t, end) = (rows[0].0 - 60_000, rows.last().unwrap().0 + 20 * 60_000);
    let mut i = 0;
    let mut events = Vec::new();
    while t <= end {
        while i < rows.len() && rows[i].0 <= t {
            w.observe(&c, rows[i].1, &rows[i].2, rows[i].3, rows[i].0);
            i += 1;
        }
        events.extend(w.step(&c, t, Some((13.5, Some(1.0)))));
        t += 60_000;
    }
    events
}

#[test]
fn real_radiator_cooldowns_of_2026_10_29_never_open_a_window() {
    let events = replay("");
    assert!(!events.iter().any(|e| e.kind == "window_open"), "{events:?}");
    let suppressed = events.iter().filter(|e| e.kind == "window_suppressed" && e.detail["reason"] == "cooldown").count();
    assert!(suppressed >= 3, "{events:?}");
}

#[test]
fn real_trace_would_have_opened_windows_without_the_radiator_guard() {
    let events = replay("[window]\nradiator_cold_min = 0.0\nfallback_cooldown_k_per_h = 0.5\nexpected_cooldown_factor = 1.0");
    assert!(events.iter().any(|e| e.kind == "window_open"), "{events:?}");
}

#[test]
fn warm_radiator_needs_a_drop_far_beyond_the_cooldown_rate() {
    let mut s = Sim::new("");
    s.run(flat(22.0, 12));
    s.valve(40.0);
    s.run(flat(22.0, 3));
    s.valve(0.0);
    s.run(ramp(22.0, -0.4, 4));
    assert!(s.kinds().iter().all(|k| *k != "window_open"), "{:?}", s.events);
    let mut s = Sim::new("");
    s.valve(134.0);
    s.run(flat(22.0, 60));
    s.run(ramp(22.0, -0.4, 4));
    assert!(s.kinds().iter().all(|k| *k != "window_open"), "{:?}", s.events);
    assert_eq!(s.find("window_suppressed").detail["reason"], "cooldown");
}

#[test]
fn radiator_is_cold_only_after_45_minutes_of_closed_valve() {
    let mut s = Sim::new("");
    s.valve(40.0);
    s.run(flat(21.0, 3));
    s.valve(0.0);
    s.run(flat(21.0, 46));
    s.run(ramp(21.0, -0.4, 4));
    assert_eq!(s.kinds(), vec!["window_open"]);
    assert_eq!(s.find("window_open").detail["radiator_warm"], false);
}

#[test]
fn large_valve_drop_starts_a_cooldown() {
    let mut s = Sim::new("");
    s.valve(80.0);
    s.run(flat(22.0, 3));
    s.valve(40.0);
    s.run(ramp(22.0, -0.2, 4));
    s.run(flat(21.2, 20));
    assert!(!s.kinds().contains(&"window_open"));
    assert_eq!(s.w.samples(5).len(), 1);
}

#[test]
fn learned_cooldowns_include_fast_ones() {
    let mut s = Sim::new("");
    for _ in 0..3 {
        s.valve(40.0);
        s.run(flat(22.0, 2));
        s.valve(0.0);
        s.run(vec![21.0, 20.0, 19.0, 18.0]);
        s.run(flat(18.0, 16));
        s.run(ramp(18.0, 0.25, 16));
    }
    let samples = s.w.samples(5);
    assert!(samples.len() >= 3 && samples.iter().all(|r| *r >= 30.0), "{samples:?}");
}

#[test]
fn restart_rebuilds_cooldowns_from_the_lake() {
    let dir = tempfile::tempdir().unwrap();
    let sink = crate::lake::Sink::open(crate::lake::LakeConfig::new(dir.path())).unwrap();
    let now_ms = crate::lake::now_us() / 1000;
    let row = |min_ago: i64, attribute: u32, v: i64| crate::lake::AttributeReport { ts: (now_ms - min_ago * 60_000) * 1000, node: 5, cluster: 513, attribute, dtype: "int".into(), v_i64: Some(v), source: "poll".into(), fw: "1".into(), ..Default::default() };
    for (min_ago, attribute, v) in [(200, 8, 50), (200, 0, 2200), (199, 8, 0), (195, 0, 2100), (189, 0, 2050), (120, 0, 2050)] {
        sink.append(row(min_ago, attribute, v)).unwrap();
    }
    sink.flush().unwrap();
    let c = Config::parse("[[rooms]]\nname = 'a'\nnodes = [5]\n").unwrap();
    let mut w = Windows::default();
    assert_eq!(w.rebuild(&c, dir.path(), now_ms), 6);
    assert_eq!(w.samples(5).len(), 1);
    assert!(w.samples(5)[0] > 5.0);
    assert_eq!(w.view("a", now_ms)["state"], "closed");
}

#[test]
fn second_node_reporting_late_does_not_look_like_a_drop() {
    let c = Config::parse("[[rooms]]\nname = 'a'\nnodes = [5, 6]\n").unwrap();
    let mut w = Windows::default();
    let t0 = 1_000_000_000_000;
    w.observe(&c, 5, PI_HEATING_DEMAND, 0.0, t0 - 3_000_000);
    w.observe(&c, 6, PI_HEATING_DEMAND, 0.0, t0 - 3_000_000);
    let mut events = Vec::new();
    for i in 1..=15 {
        w.observe(&c, 5, LOCAL_TEMPERATURE, 21.0, t0 + i * 60_000);
        events.extend(w.step(&c, t0 + i * 60_000, Some((2.0, None))));
    }
    w.observe(&c, 6, LOCAL_TEMPERATURE, 18.0, t0 + 16 * 60_000);
    for i in 16..=20 {
        events.extend(w.step(&c, t0 + i * 60_000, Some((2.0, None))));
    }
    assert!(events.is_empty(), "{events:?}");
}

#[test]
fn sparse_reports_across_a_gap_are_not_a_fast_drop() {
    let mut s = Sim::new("");
    s.run(flat(21.0, 5));
    s.t += 16 * 60_000;
    s.run(vec![20.0]);
    s.run(flat(20.0, 3));
    assert!(s.kinds().is_empty(), "{:?}", s.events);
}

#[test]
fn manual_open_times_out() {
    let mut s = Sim::new("[window]\nmanual_max_min = 30.0");
    s.run(flat(21.0, 5));
    s.w.manual("a", true, s.t);
    s.run(flat(21.0, 29));
    assert_eq!(s.state(), "open");
    s.run(flat(21.0, 2));
    assert_eq!(s.state(), "closed");
    assert_eq!(s.find("window_close").detail["reason"], "manual_timeout");
}

fn incident() -> Sim {
    let mut s = Sim::new("");
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
    let mut s = Sim::new("");
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
fn fast_drop_during_no_supply_opens_the_window_despite_the_open_valve() {
    let mut s = incident();
    s.run(flat(17.1, 100));
    s.run(ramp(17.1, -0.1, 45));
    assert!(s.kinds().contains(&"window_open"), "{:?}", s.events);
    assert_eq!(s.find("window_open").detail["radiator_warm"], false);
}

#[test]
fn same_drop_with_supply_and_hot_radiator_is_not_a_window() {
    let mut s = Sim::new("[supply]\nenabled = false");
    s.run(flat(19.0, 5));
    s.valve(100.0);
    s.run(ramp(19.0, -0.0155, 120));
    s.run(flat(17.1, 100));
    s.run(ramp(17.1, -0.1, 45));
    assert!(!s.kinds().contains(&"window_open"), "{:?}", s.events);
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
fn recovery_needs_the_hold_time_and_a_debounced_rise() {
    let mut s = opened_with("[window]
max_open_min = 60.0");
    s.run(flat(19.4, 8));
    s.run(ramp(19.4, 0.2, 3));
    assert_eq!(s.state(), "open", "rise inside the 15 min hold must not close");
    s.run(flat(19.4, 8));
    s.run(vec![19.8, 19.4]);
    s.run(vec![19.8]);
    s.run(flat(19.8, 2));
    assert_eq!(s.state(), "open", "a rise that has not held for the debounce does not close");
    s.run(flat(19.8, 1));
    assert_eq!(s.state(), "closed");
}

#[test]
fn closed_window_is_not_rearmed_for_twenty_minutes_and_probe_close_waits_thirty() {
    let mut s = opened_with("[window]
min_open_hold_min = 0.0
close_debounce_min = 0.0");
    s.run(flat(19.4, 6));
    s.run(ramp(19.4, 0.2, 3));
    assert_eq!(s.state(), "closed");
    s.run(flat(20.0, 2));
    s.run(ramp(20.0, -0.4, 4));
    assert_eq!(s.state(), "closed");
    assert_eq!(s.find("window_suppressed").detail["reason"], "rearm");
    let mut p = opened();
    p.run(flat(19.4, 20));
    p.run(flat(19.4, 10));
    assert_eq!(p.find("window_close").detail["reason"], "timeout_probe");
    p.run(flat(21.0, 2));
    p.run(ramp(21.0, -0.4, 4));
    assert_eq!(p.state(), "closed");
    p.run(flat(19.4, 20));
    p.run(flat(21.0, 2));
    p.run(ramp(21.0, -0.4, 4));
    assert_eq!(p.state(), "open");
}

const NO_HYSTERESIS: &str = "[window]
min_open_hold_min = 0.0
rearm_min = 0.0
probe_cooldown_min = 0.0
close_debounce_min = 0.0";

fn flapping_shape(extra: &str) -> (Sim, usize, Vec<(usize, bool)>) {
    let mut s = Sim::new(extra);
    s.run(flat(21.0, 15));
    let mut toggles = Vec::new();
    let mut frozen = false;
    let mut minute = 0;
    let mut feed = |s: &mut Sim, temps: Vec<f64>| {
        for t in temps {
            s.run(vec![t]);
            minute += 1;
            let now = s.w.open_rooms().contains("a");
            if now != frozen {
                frozen = now;
                toggles.push((minute, now));
            }
        }
    };
    feed(&mut s, ramp(21.0, -0.4, 4));
    feed(&mut s, flat(19.5, 14));
    for _ in 0..3 {
        feed(&mut s, vec![20.0]);
        feed(&mut s, ramp(20.0, -0.4, 4));
        feed(&mut s, flat(18.5, 4));
    }
    feed(&mut s, flat(18.5, 10));
    let opens = s.events.iter().filter(|e| e.kind == "window_open").count();
    (s, opens, toggles)
}

#[test]
fn recovery_bounce_shape_of_2026_10_31_opens_once_without_setpoint_toggling() {
    let (_, old_opens, old_toggles) = flapping_shape(NO_HYSTERESIS);
    assert!(old_opens >= 3 && old_toggles.len() >= 5, "baseline must flap: {old_opens} {old_toggles:?}");
    let (s, opens, toggles) = flapping_shape("");
    assert_eq!(opens, 1, "{:?}", s.events);
    assert!(toggles.len() <= 2, "{toggles:?}");
}

#[test]
fn a_silent_node_holds_an_open_window_instead_of_timing_it_out() {
    let mut s = opened();
    assert_eq!(s.state(), "open");
    let before = s.events.len();
    s.idle(600);
    assert_eq!((s.state().as_str(), s.events.len()), ("open", before));
}

#[test]
fn a_flat_stale_temperature_never_starts_no_supply_and_the_jump_after_reconnect_is_no_drop() {
    let mut s = Sim::new("");
    s.run(flat(19.0, 5));
    s.valve(100.0);
    s.run(flat(19.0, 10));
    s.idle(120);
    assert!(supply_events(&s).is_empty() && s.w.no_supply_rooms().is_empty(), "{:?}", s.events);
    s.run(vec![17.0]);
    s.run(flat(17.0, 3));
    assert!(s.kinds().is_empty(), "{:?}", s.events);
}

#[test]
fn reconnect_after_a_gap_forgets_the_old_series_for_detection() {
    let mut s = Sim::new("");
    s.run(flat(21.0, 20));
    s.t += 20 * 60_000;
    s.run(vec![19.0]);
    s.run(flat(19.0, 4));
    assert!(s.kinds().is_empty(), "{:?}", s.events);
}

#[test]
fn stale_node_data_never_triggers_a_pi_reset() {
    let s = Sim::new("");
    let mut w = Windows::default();
    let t = s.t;
    w.observe(&s.c, 5, LOCAL_TEMPERATURE, 22.0, t);
    w.observe(&s.c, 5, PI_HEATING_DEMAND, 100.0, t);
    assert!(w.overshoot_due(&s.c.supply, 5, 20.0, t, 900_000).is_none());
    assert!(w.overshoot_due(&s.c.supply, 5, 20.0, t + 16 * 60_000, 36_000_000).is_some());
    let mut w = Windows::default();
    w.observe(&s.c, 5, LOCAL_TEMPERATURE, 22.0, t);
    w.observe(&s.c, 5, PI_HEATING_DEMAND, 100.0, t);
    w.overshoot_due(&s.c.supply, 5, 20.0, t, 900_000);
    assert!(w.overshoot_due(&s.c.supply, 5, 20.0, t + 16 * 60_000, 900_000).is_none());
    assert!(w.local_temp(5, t + 16 * 60_000, 900_000).is_none() && w.local_temp(5, t + 60_000, 900_000) == Some(22.0));
}
