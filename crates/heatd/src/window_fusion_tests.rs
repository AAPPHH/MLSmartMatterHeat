use crate::config::Config;
use crate::window::*;
use crate::window_fusion::{thresholds, Chan, Posterior, Tables};
use crate::window_model_tests::toy_bundle;
use matter_controller::{CO2_CLUSTER, HUMIDITY_MEASUREMENT_CLUSTER, TEMPERATURE_MEASUREMENT_CLUSTER};

type Logged = (i32, &'static str, String, Option<String>, Option<String>);
const MIN: i64 = 60_000;
const T0: i64 = 1_800_000_000_000 - 1_800_000_000_000 % MIN;
const AIR_NODE: u64 = 20;
const OUTDOOR: f64 = 4.0;
const ROOMS: &str = "[[rooms]]\nname = 'a'\nnodes = [5]\n[[rooms]]\nname = 'b'\nnodes = [6]\n[[rooms]]\nname = 'hall'\nneighbours = [{ a = 'a', b = 'hall', kind = 'wall' }, { a = 'b', b = 'hall', kind = 'wall' }]\n[[devices]]\nnode = 20\nkind = 'air'\nrole = 'air'\nroom = 'a'\n";

#[derive(Clone, Copy)]
struct Air {
    co2: f64,
    rh: f64,
    temp: f64,
}

const CLOSED: Air = Air { co2: 900.0, rh: 52.0, temp: 19.0 };

fn venting(m: f64) -> Air {
    Air { co2: 420.0 + 480.0 * (-0.2 * m).exp(), rh: (52.0 - 1.4 * m).max(40.0), temp: 19.0 - 0.07 * m }
}

fn plateau(_: f64) -> Air {
    Air { co2: 440.0, rh: 40.0, temp: 17.6 }
}

fn recovering(m: f64) -> Air {
    Air { co2: 440.0 + 45.0 * m, rh: 40.0 + 1.0 * m, temp: 17.6 + 0.04 * m }
}

fn noisy(m: f64) -> Air {
    let wobble = ((m * 7.3).sin() * 4.0).round();
    Air { co2: CLOSED.co2 + wobble, rh: CLOSED.rh + (m * 3.1).sin() * 0.1, temp: CLOSED.temp + (m * 5.0).sin().round() * 0.01 }
}

struct Rig {
    w: Windows,
    c: Config,
    t: i64,
    events: Vec<WinEvent>,
}

impl Rig {
    fn new(extra: &str) -> Self {
        Self { w: Windows::default(), c: Config::parse(&format!("{ROOMS}{extra}")).unwrap(), t: T0, events: Vec::new() }
    }

    fn minute(&mut self, air: Option<Air>) {
        for _ in 0..2 {
            self.t += MIN / 2;
            if let Some(air) = air {
                self.w.observe_air(&self.c, AIR_NODE, CO2_CLUSTER, air.co2, self.t);
                self.w.observe_air(&self.c, AIR_NODE, HUMIDITY_MEASUREMENT_CLUSTER, air.rh * 100.0, self.t);
                self.w.observe_air(&self.c, AIR_NODE, TEMPERATURE_MEASUREMENT_CLUSTER, air.temp * 100.0, self.t);
            }
            let found = self.w.step(&self.c, self.t, |_| [OUTDOOR, 2.0, 0.0]);
            self.events.extend(found);
        }
    }

    fn run(&mut self, minutes: usize, f: impl Fn(f64) -> Air) {
        (1..=minutes).for_each(|m| self.minute(Some(f(m as f64))));
    }

    fn open(&self, room: &str) -> bool {
        self.w.open_rooms().contains(room)
    }

    fn of(&self, kind: &str) -> Vec<&WinEvent> {
        self.events.iter().filter(|e| e.kind == kind).collect()
    }

    fn p(&self, room: &str) -> f64 {
        self.w.view(room, self.t)["p"].as_f64().unwrap()
    }

    fn warm() -> Self {
        let mut r = Self::new("");
        r.run(25, noisy);
        r
    }
}

#[test]
fn quiet_air_never_opens_or_flaps() {
    let mut r = Rig::new("");
    r.run(240, noisy);
    assert!(r.events.is_empty(), "{:?}", r.events);
    assert!(r.p("a") < 0.1);
}

#[test]
fn fusion_opens_on_the_air_channels_alone_with_a_german_reason() {
    let mut r = Rig::warm();
    let mut opened_at = None;
    for m in 1..=10 {
        r.minute(Some(venting(m as f64)));
        if opened_at.is_none() && r.open("a") {
            opened_at = Some(m);
        }
    }
    let at = opened_at.expect("window opened on air evidence alone");
    assert!(at <= 6, "opened after {at} min");
    let open = r.of("window_open");
    assert_eq!(open.len(), 1, "{:?}", r.events);
    let d = &open[0].detail;
    assert_eq!(d["source"], "fusion");
    let reason = d["reason"].as_str().unwrap();
    assert!(reason.contains("CO2 fällt schnell") && reason.contains("Fenster offen (P "), "{reason}");
    assert!(d["fusion"]["p"].as_f64().unwrap() > 0.4 && d["fusion"]["channels"]["co2"].as_f64().unwrap() > 0.0);
    assert!(!r.open("b"));
}

#[test]
fn the_window_stays_open_while_the_posterior_is_high_and_closes_on_recovery() {
    let mut r = Rig::warm();
    r.run(8, venting);
    assert!(r.open("a"));
    r.run(40, plateau);
    assert!(r.open("a"), "air still says open past the old 20 minute hold: p {}", r.p("a"));
    assert!(r.of("window_close").is_empty());
    r.run(25, recovering);
    assert!(!r.open("a"), "recovered air closes the window: p {}", r.p("a"));
    let close = r.of("window_close");
    assert_eq!(close.len(), 1);
    assert_eq!(close[0].detail["reason"], "fusion");
    let text = close[0].detail["fusion"]["text"].as_str().unwrap();
    assert!(text.contains("steigt wieder") && text.contains("Fenster zu"), "{text}");
}

#[test]
fn stale_air_falls_back_to_the_hold_timer() {
    let mut r = Rig::warm();
    r.run(8, venting);
    assert!(r.open("a"));
    for _ in 0..19 {
        r.minute(None);
    }
    assert!(r.open("a"), "held until hold_min after the last air evidence");
    for _ in 0..4 {
        r.minute(None);
    }
    assert!(!r.open("a"));
    assert_eq!(r.of("window_close")[0].detail["reason"], "timeout");
}

#[test]
fn a_manual_close_wins_and_is_fed_back_as_an_observation() {
    let mut r = Rig::warm();
    r.run(8, venting);
    assert!(r.open("a"));
    let event = r.w.manual("a", false, r.t).unwrap();
    assert_eq!(event.kind, "window_close");
    assert!(!r.open("a") && r.p("a") < 0.1, "p {}", r.p("a"));
    r.minute(Some(plateau(1.0)));
    assert!(!r.open("a"), "the old samples before the answer no longer count");
    let manual = r.w.manual("a", true, r.t).unwrap();
    assert_eq!(manual.detail["source"], "manual");
    assert!(r.p("a") > 0.9);
}

#[test]
fn a_neighbour_that_is_open_raises_the_prior_of_the_room_next_to_it_but_never_opens_it_alone() {
    let mut r = Rig::new("neighbours = []\n");
    r.c.neighbours = vec![crate::config::Neighbour { a: "a".into(), b: "b".into(), kind: "wall".into(), partial: false }];
    r.run(25, noisy);
    let before = r.p("b");
    r.run(8, venting);
    assert!(r.open("a"));
    assert!(r.p("b") > before + 0.1, "b {} -> {}", before, r.p("b"));
    assert!(!r.open("b"), "a posterior shift without own evidence does not freeze a room");
    assert_eq!(Chan::Couple.name(), "neighbour");
}

#[test]
fn away_and_manual_hold_veto_the_opening_but_only_while_set() {
    let mut r = Rig::warm();
    r.w.set_context([("a".to_string(), RoomCtx { hold: false, away: true })]);
    r.run(8, venting);
    assert!(!r.open("a"));
    assert!(r.of("window_suppressed").iter().any(|e| e.detail["reason"] == "away"));
    r.w.set_context([("a".to_string(), RoomCtx::default())]);
    r.run(2, plateau);
    assert!(r.open("a"), "fresh independent evidence is not blocked once away is no longer confirmed");
}

#[test]
fn rooms_without_an_air_channel_behave_exactly_like_the_old_detector() {
    let run = |fusion: bool| {
        let c = Config::parse(&format!("[window.fusion]\nenabled = {fusion}\n[[rooms]]\nname = 'wohnzimmer'\nnodes = [5]\n[[rooms]]\nname = 'kueche'\nnodes = [8]\n")).unwrap();
        let mut w = Windows::default();
        w.set_model(toy_bundle(&[("wohnzimmer", 5, 1.0), ("kueche", 8, 1.0)], 3.0));
        let mut log: Vec<Logged> = Vec::new();
        let mut t = T0;
        for k in 0..75 {
            t += MIN;
            let drop = if k < 30 { 0.0 } else { 0.3 * (k - 29) as f64 };
            for node in [5, 8] {
                w.observe(&c, node, LOCAL_TEMPERATURE, 20.0 - drop, t);
                w.observe(&c, node, PI_HEATING_DEMAND, 40.0, t);
                w.observe(&c, node, OCCUPIED_SETPOINT, 21.0, t);
            }
            for e in w.step(&c, t, |_| [5.0, 2.0, 0.0]) {
                log.push((k, e.kind, e.room.clone(), e.detail["source"].as_str().map(str::to_string), e.detail["reason"].as_str().map(str::to_string)));
            }
        }
        log
    };
    let (legacy, fused) = (run(false), run(true));
    assert!(legacy.iter().any(|e| e.1 == "window_open"), "scenario must alarm: {legacy:?}");
    let essence = |l: &[Logged]| l.iter().filter(|e| e.1 != "window_episode").map(|e| (e.0, e.1, e.2.clone(), e.3.clone())).collect::<Vec<_>>();
    let (mut a, mut b) = (essence(&legacy), essence(&fused));
    a.sort();
    b.sort();
    assert_eq!(a, b);
}

#[test]
fn the_interval_censored_thermostat_channel_is_smooth_and_monotone() {
    let t = Tables::builtin();
    let sigma = 0.3;
    let mut last = f64::NEG_INFINITY;
    for i in -40..=80 {
        let z = i as f64 * 0.1;
        let l = t.trv.llr(z, sigma, false);
        assert!(l >= last - 1e-9, "monotone at {z}");
        if i > -40 {
            assert!((l - last).abs() < 0.5, "a 0.1 sigma change moves the evidence by {} at {z}", l - last);
        }
        last = l;
    }
    let step = t.trv.llr(2.9, sigma, false) - t.trv.llr(1.4, sigma, false);
    assert!(step < t.trv.cap, "a 0.5 K quantisation step stays below the cap: {step}");
    assert_eq!(t.trv.llr(f64::NAN, sigma, true), 0.0);
}

#[test]
fn the_cost_knobs_fix_the_posterior_thresholds_with_hysteresis() {
    let cfg = crate::config::WindowFusion::default();
    let (on, off) = thresholds(&cfg, 15.0, 1.0);
    assert!(on > off + 1.0 && on < 0.0, "on {on} off {off}");
    let (on2, _) = thresholds(&cfg, 60.0, 1.0);
    assert!(on2 > on, "a dearer raise makes the frost stricter");
    let p = Posterior::prior(&cfg);
    assert!(p < off, "the closed prior sits below the release level");
}

#[test]
fn the_bedroom_incident_is_caught_by_the_air_channels_alone() {
    let text = include_str!("../tests/fixtures/airwin/incident.csv");
    let rows: Vec<Vec<f64>> = text.lines().skip(1).map(|l| l.split(',').map(|v| v.parse().unwrap()).collect()).collect();
    let mut r = Rig::new("");
    let start = T0;
    let mut open_at: Vec<bool> = Vec::new();
    for row in &rows {
        let ts = start + (row[0] * 1000.0) as i64;
        r.t = ts;
        r.w.observe_air(&r.c, AIR_NODE, TEMPERATURE_MEASUREMENT_CLUSTER, row[1], ts);
        r.w.observe_air(&r.c, AIR_NODE, HUMIDITY_MEASUREMENT_CLUSTER, row[2], ts);
        r.w.observe_air(&r.c, AIR_NODE, CO2_CLUSTER, row[3], ts);
        let found = r.w.step(&r.c, ts, |_| [0.5, 2.0, 0.0]);
        r.events.extend(found);
        open_at.push(r.open("a"));
    }
    let at = |s: f64| open_at[rows.iter().position(|row| row[0] >= s).unwrap()];
    let first = |from: f64, to: f64| rows.iter().zip(&open_at).find(|(row, o)| row[0] >= from && row[0] <= to && **o).map(|(row, _)| row[0]);
    assert!(first(1500.0, 2000.0).is_some_and(|s| s <= 33.0 * 60.0), "phase A: {:?}", first(1500.0, 2200.0));
    assert!(first(3500.0, 4000.0).is_some_and(|s| s <= 66.0 * 60.0), "phase B: {:?}", first(3500.0, 4200.0));
    assert!((66 * 60..=105 * 60).step_by(60).all(|s| at(s as f64)), "phase B stays open from 08:06 to 08:45");
}
