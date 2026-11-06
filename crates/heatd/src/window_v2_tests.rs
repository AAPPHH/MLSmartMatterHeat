use crate::config::Config;
use crate::window::*;
use crate::window_v2::*;
use serde_json::Value;

const MIN: i64 = 60_000;

struct H {
    w: Windows,
    c: Config,
    t: i64,
    events: Vec<WinEvent>,
}

fn flat(v: f64, n: usize) -> Vec<f64> {
    vec![v; n]
}

fn ramp(from: f64, per_min: f64, n: usize) -> Vec<f64> {
    (1..=n).map(|i| from + per_min * i as f64).collect()
}

impl H {
    fn new(window: &str, v2: &str) -> Self {
        let on = if v2.contains("enabled") { "" } else { "enabled = true\n" };
        let c = Config::parse(&format!("[window]\n{window}\n[window.v2]\n{on}{v2}\n[[rooms]]\nname = 'a'\nnodes = [5]\n")).unwrap();
        let mut h = Self { w: Windows::default(), c, t: 1_000_000_000_000, events: Vec::new() };
        h.w.observe(&h.c, 5, PI_HEATING_DEMAND, 0.0, h.t - 50 * 60_000);
        h
    }

    fn run(&mut self, temps: Vec<f64>) {
        for temp in temps {
            self.t += 60_000;
            self.w.observe(&self.c, 5, LOCAL_TEMPERATURE, temp, self.t);
            let found = self.w.step(&self.c, self.t, Some((2.0, None)));
            self.events.extend(found);
        }
    }

    fn valve(&mut self, v: f64) {
        self.w.observe(&self.c, 5, PI_HEATING_DEMAND, v, self.t);
    }

    fn episodes(&mut self, detector: &str) -> Vec<Value> {
        let found = self.w.finish(&self.c, self.t);
        self.events.extend(found);
        self.events.iter().filter(|e| e.kind == "window_episode" && e.detail["detector"] == detector).map(|e| e.detail.clone()).collect()
    }

    fn v1_kinds(&self) -> Vec<&'static str> {
        self.events.iter().map(|e| e.kind).filter(|k| *k != "window_episode").collect()
    }
}

fn drop_after_calm(h: &mut H) {
    h.run(flat(21.0, 30));
    h.run(ramp(21.0, -0.4, 5));
}

#[test]
fn baseline_learns_the_room_cooling_curve_online() {
    let c = crate::config::WindowV2Section::default();
    let mut base = Ar1::new(&c);
    let (k, floor) = (0.012, 19.0);
    let truth = |x: f64| -k * (x - floor);
    let before = (base.predict(22.0, 1.0, false) - truth(22.0)).abs();
    let mut x = 26.0;
    for i in 0..4000 {
        let delta = truth(x);
        base.update(x, 1.0, false, delta);
        x = if i % 400 == 399 { 26.0 } else { x + delta };
    }
    let after = (base.predict(22.0, 1.0, false) - truth(22.0)).abs();
    assert!(after < 0.5 * before, "{before} -> {after}");
}

#[test]
fn shadow_opens_on_a_drop_but_emits_no_v1_kinds_and_writes_nothing() {
    let mut h = H::new("", "");
    drop_after_calm(&mut h);
    assert_eq!(h.v1_kinds(), vec!["window_open"]);
    assert!(h.w.open_rooms().contains("a"));
    let v2 = h.episodes("v2");
    assert_eq!(v2.len(), 1, "{:?}", h.events);
    assert_eq!((v2[0]["end_reason"].as_str(), v2[0]["room"].as_str(), v2[0]["label"].as_str()), (Some("open_at_end"), Some("a"), Some("none")));
    assert!(v2[0]["onset_ms"].as_i64().unwrap() <= v2[0]["start_ms"].as_i64().unwrap());
}

#[test]
fn disabled_shadow_stays_silent() {
    let mut h = H::new("", "enabled = false");
    drop_after_calm(&mut h);
    assert!(h.episodes("v2").is_empty());
}

#[test]
fn radiator_cooldown_is_a_window_only_once_the_radiator_is_cold() {
    let mut warm = H::new("", "");
    warm.run(flat(24.0, 30));
    warm.valve(100.0);
    warm.run(flat(24.0, 10));
    warm.valve(0.0);
    warm.run(ramp(24.0, -0.35, 5));
    assert!(warm.episodes("v2").is_empty(), "{:?}", warm.events);
    let mut cold = H::new("", "");
    cold.run(flat(24.0, 30));
    cold.valve(100.0);
    cold.run(flat(24.0, 10));
    cold.valve(0.0);
    cold.run(flat(22.0, 45));
    cold.run(ramp(22.0, -0.4, 5));
    assert_eq!(cold.episodes("v2").len(), 1);
}

#[test]
fn passive_closure_waits_for_the_hold_time_and_a_shut_valve() {
    let mut h = H::new("", "");
    drop_after_calm(&mut h);
    h.run(flat(19.0, 3));
    h.run(ramp(19.0, 0.2, 4));
    assert!(h.w.view("a", h.t)["state"] == "open");
    h.run(flat(19.8, 12));
    let v2 = h.episodes("v2");
    assert_eq!(v2.len(), 1, "{:?}", h.events);
    assert_eq!(v2[0]["end_reason"], "passive");
    assert!(v2[0]["duration_min"].as_f64().unwrap() >= 15.0);

    let mut heated = H::new("", "");
    drop_after_calm(&mut heated);
    heated.valve(60.0);
    heated.run(flat(19.0, 3));
    heated.run(ramp(19.0, 0.2, 4));
    heated.run(flat(19.8, 20));
    assert_eq!(heated.episodes("v2")[0]["end_reason"], "open_at_end");
}

#[test]
fn probe_costs_follow_the_hazard_of_the_duration_prior() {
    let c = crate::config::WindowV2Section::default();
    let short = [20.0; 8];
    let (wait, probe) = probe_costs(&c, 10.0, &short, 18.0, true);
    assert!(wait > probe, "{wait} {probe}");
    let (wait, probe) = probe_costs(&c, 10.0, &short, 2.0, true);
    assert!(wait < probe, "{wait} {probe}");
    let (wait, _) = probe_costs(&c, 10.0, &short, 18.0, false);
    assert_eq!(wait, 0.0);
    let long = [90.0; 8];
    let (wait, probe) = probe_costs(&c, 10.0, &long, 18.0, true);
    assert!(wait < probe, "{wait} {probe}");
    let cheap = crate::config::WindowV2Section { energy_cost_per_valve_min: 0.01, ..c.clone() };
    let (wait, probe) = probe_costs(&cheap, 10.0, &long, 18.0, true);
    assert!(probe < wait || wait == 0.0, "{wait} {probe}");
}

fn probes(window: &str, v2: &str, ctx: RoomCtx, flags: Flags) -> u64 {
    let cfg = Config::parse(&format!("[window]\n{window}\n[window.v2]\nenabled = true\ndefault_episode_min = [20.0, 20.0, 20.0]\n{v2}")).unwrap().window;
    let mut s = Shadow::default();
    let mut t = 1_000_000_000_000_i64;
    let mut temp = 21.0;
    let mut tick = |s: &mut Shadow, x: f64, flags: &Flags| {
        t += 60_000;
        s.on_report(&cfg, "a", t, x, flags, &ctx)
    };
    for _ in 0..30 {
        tick(&mut s, 21.0, &flags);
    }
    for _ in 0..5 {
        temp -= 0.4;
        tick(&mut s, temp, &flags);
    }
    for _ in 0..40 {
        tick(&mut s, temp, &flags);
    }
    s.finish(&cfg, t).iter().map(|e| u64::from(e.tally.probes)).sum()
}

#[test]
fn adaptive_probe_fires_only_when_waiting_costs_more_and_never_in_blocked_states() {
    let ctx = RoomCtx { target: Some(21.0), ..Default::default() };
    let flags = Flags { typical_k_per_h: 20.0, ..Default::default() };
    assert!(probes("", "", ctx.clone(), flags.clone()) >= 1);
    assert_eq!(probes("", "comfort_cost_per_min_below = 0.0", ctx.clone(), flags.clone()), 0);
    assert_eq!(probes("", "", RoomCtx { target: Some(15.0), ..ctx.clone() }, flags.clone()), 0, "not below the setpoint");
    for blocked in [RoomCtx { away: true, ..ctx.clone() }, RoomCtx { hold: true, ..ctx.clone() }, RoomCtx { boost: true, ..ctx.clone() }] {
        assert_eq!(probes("", "", blocked, flags.clone()), 0);
    }
    assert_eq!(probes("", "", ctx, Flags { no_supply: true, ..flags }), 0);
}

#[test]
fn episode_metrics_count_probe_and_below_setpoint_minutes_of_the_live_detector() {
    let mut h = H::new("", "enabled = false");
    h.w.set_context([("a".to_string(), RoomCtx { target: Some(21.0), ..Default::default() })]);
    drop_after_calm(&mut h);
    h.run(flat(19.0, 16));
    h.valve(50.0);
    h.run(flat(19.0, 14));
    let v1 = h.episodes("v1");
    assert_eq!(v1.len(), 1, "{:?}", h.events);
    let e = &v1[0];
    assert_eq!(e["end_reason"], "timeout_probe");
    assert_eq!(e["probe_count"], 1);
    assert!((e["probe_valve_min"].as_f64().unwrap() - 5.0).abs() < 1.5, "{e}");
    assert!(e["below_min"].as_f64().unwrap() >= 25.0, "{e}");
}

#[test]
fn labels_confirm_episodes_and_manual_entries_give_latency() {
    let mut h = H::new("", "enabled = false");
    h.run(flat(21.0, 20));
    let manual_ts = h.t;
    h.w.label("a", manual_ts, true, None, Some("m1"), h.t);
    assert!(h.w.open_rooms().contains("a"), "manual entry drives the window state");
    h.run(flat(20.0, 6));
    h.w.label("a", h.t, false, None, Some("m2"), h.t);
    assert!(h.w.open_rooms().is_empty());
    h.run(flat(20.0, 30));
    let v1 = h.episodes("v1");
    assert_eq!(v1.len(), 1);
    assert_eq!(v1[0]["end_reason"], "manual");
    assert_eq!(v1[0]["source"], "manual");
    assert_eq!(v1[0]["label"], "yes", "{:?}", v1[0]);
    assert_eq!(v1[0]["latency_min"].as_f64(), Some(0.0));
}

#[test]
fn manual_close_at_the_end_of_a_detected_opening_is_not_a_no() {
    let mut h = H::new("", "enabled = false");
    drop_after_calm(&mut h);
    h.run(flat(19.4, 8));
    h.w.label("a", h.t, false, None, Some("m1"), h.t);
    h.run(flat(19.4, 30));
    let v1 = h.episodes("v1");
    assert_eq!((v1[0]["source"].as_str(), v1[0]["end_reason"].as_str(), v1[0]["label"].as_str()), (Some("auto"), Some("manual"), Some("none")), "{:?}", v1[0]);
}

#[test]
fn manual_close_before_the_detector_opened_contradicts_it() {
    let mut h = H::new("", "enabled = false");
    h.run(flat(21.0, 30));
    h.w.label("a", h.t, false, None, Some("m1"), h.t);
    h.run(ramp(21.0, -0.4, 5));
    h.run(flat(19.4, 30));
    let v1 = h.episodes("v1");
    assert_eq!(v1[0]["label"], "no", "{:?}", v1[0]);
}

#[test]
fn late_manual_entries_are_stored_but_do_not_change_the_window_state() {
    let mut h = H::new("", "enabled = false");
    h.run(flat(21.0, 20));
    assert!(h.w.label("a", h.t - 301_000, true, None, Some("m1"), h.t).is_none());
    assert!(h.w.open_rooms().is_empty());
    assert!(h.w.label("a", h.t - 299_000, true, None, Some("m2"), h.t).is_some());
    assert!(h.w.open_rooms().contains("a"));
}

#[test]
fn void_only_reverts_the_manual_open_its_own_label_caused() {
    let mut h = H::new("", "enabled = false");
    h.run(flat(21.0, 5));
    h.w.label("a", h.t, true, None, Some("m1"), h.t);
    h.w.manual("a", false, h.t);
    h.w.label("a", h.t, true, None, Some("m2"), h.t);
    assert!(h.w.void_label("m1", h.t).is_none());
    assert!(h.w.open_rooms().contains("a"));
    assert!(h.w.void_label("m2", h.t).is_some());
    assert!(h.w.open_rooms().is_empty());
}

#[test]
fn live_v1_transition_prompts_and_global_gap_and_daily_cap_apply() {
    let mut h = H::new("prompt_gap_min = 1
prompt_global_gap_min = 30
prompt_max_per_day = 3", "");
    let cfg = h.c.window.clone();
    drop_after_calm(&mut h);
    let first = h.w.prompt(&cfg, h.t).expect("v1 transition prompts")["id"].clone();
    let base = h.t + 100 * MIN;
    let mut last = first;
    let mut shown = Vec::new();
    for dt in [0, 10, 40, 70] {
        h.w.test_prompt(&cfg, "a", "open", base + dt * MIN);
        let id = h.w.prompt(&cfg, base + dt * MIN).map_or(Value::Null, |p| p["id"].clone());
        shown.push(!id.is_null() && id != last);
        if !id.is_null() {
            last = id;
        }
    }
    assert_eq!(shown, vec![true, false, true, false]);
}

#[test]
fn disabled_window_section_neither_accumulates_v2_episodes_nor_prompts() {
    let mut h = H::new("enabled = false", "");
    let cfg = h.c.window.clone();
    h.run(flat(21.0, 30));
    h.run(ramp(21.0, -0.4, 8));
    h.run(flat(18.0, 60));
    assert!(h.w.prompt(&cfg, h.t).is_none());
    assert!(h.episodes("v2").is_empty());
}

#[test]
fn prompts_follow_detector_transitions_expire_and_match_answers() {
    let mut h = H::new("", "enabled = false");
    let cfg = h.c.window.clone();
    assert!(h.w.prompt(&cfg, h.t).is_none());
    drop_after_calm(&mut h);
    let p = h.w.prompt(&cfg, h.t).unwrap();
    assert_eq!((p["room"].as_str(), p["question"].as_str()), (Some("a"), Some("open")));
    assert!(h.w.prompt(&cfg, h.t + 21 * 60_000).is_none(), "expires after 20 min");
    let id = p["id"].as_str().unwrap().to_string();
    assert!(h.w.label("a", h.t, true, Some(&id), Some("x"), h.t).is_none(), "an answer does not change the window state");
    assert!(h.w.prompt(&cfg, h.t).is_none(), "answered prompt is cleared");
    h.run(flat(19.4, 30));
    let v1 = h.episodes("v1");
    assert_eq!(v1[0]["label"], "yes", "{:?}", v1[0]);
    assert!(v1[0]["latency_min"].is_null(), "answers to prompts are not independent timing");
}

#[test]
fn void_reverts_a_manual_open_and_removes_the_label() {
    let mut h = H::new("", "enabled = false");
    h.run(flat(21.0, 5));
    h.w.label("a", h.t, true, None, Some("m1"), h.t);
    assert!(h.w.open_rooms().contains("a"));
    assert!(h.w.void_label("m1", h.t).is_some());
    assert!(h.w.open_rooms().is_empty());
    assert!(h.w.void_label("m1", h.t).is_none());
}

fn rows_of(temps: &[f64]) -> Vec<(i64, u64, String, f64)> {
    let t0 = 1_000_000_000_000_i64;
    let mut rows = vec![(t0 - 50 * 60_000, 5, PI_HEATING_DEMAND.to_string(), 0.0)];
    rows.extend(temps.iter().enumerate().map(|(i, v)| (t0 + i as i64 * 60_000, 5, LOCAL_TEMPERATURE.to_string(), *v)));
    rows
}

#[test]
fn replay_prints_machine_readable_metrics_for_both_detectors() {
    let mut temps = flat(21.0, 30);
    temps.extend(ramp(21.0, -0.4, 5));
    temps.extend(flat(19.0, 30));
    let out = replay("[window.v2]\ndefault_episode_min = [20.0]\n", &rows_of(&temps), Some((2.0, None))).unwrap();
    let text = out.to_string();
    let parsed: Value = serde_json::from_str(&text).unwrap();
    for d in ["v1", "v2"] {
        assert_eq!(parsed["summary"][d]["episodes"], 1, "{text}");
        assert!(parsed["summary"][d]["open_min"].as_f64().unwrap() > 0.0);
    }
    assert_eq!(parsed["episodes"].as_array().unwrap().len(), 2);
}

#[test]
fn replay_of_the_real_2026_10_29_radiator_trace_finds_no_window_in_either_detector() {
    let rows = parse_csv(include_str!("../testdata/window_real_2026-10-29.csv")).unwrap();
    let out = replay("", &rows, Some((13.5, Some(1.0)))).unwrap();
    assert_eq!(out["summary"]["v1"]["episodes"], 0, "{out}");
    assert_eq!(out["summary"]["v2"]["episodes"], 0, "{out}");
}

#[test]
fn replay_of_the_bounce_shape_yields_one_live_episode() {
    let mut temps = flat(21.0, 15);
    temps.extend(ramp(21.0, -0.4, 4));
    temps.extend(flat(19.5, 14));
    for _ in 0..3 {
        temps.push(20.0);
        temps.extend(ramp(20.0, -0.4, 4));
        temps.extend(flat(18.5, 4));
    }
    temps.extend(flat(18.5, 10));
    let out = replay("", &rows_of(&temps), Some((2.0, None))).unwrap();
    assert_eq!(out["summary"]["v1"]["episodes"], 1, "{out}");
}

#[test]
fn shadow_ignores_the_jump_across_a_reporting_gap() {
    let mut h = H::new("", "");
    h.run(flat(21.0, 30));
    h.t += 90 * 60_000;
    h.run(vec![17.0]);
    h.run(flat(17.0, 5));
    assert!(h.episodes("v2").is_empty() && h.v1_kinds().is_empty(), "{:?}", h.events);
}
