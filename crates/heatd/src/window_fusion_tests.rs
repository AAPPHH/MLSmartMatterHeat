use crate::config::{Config, WindowFusion};
use crate::window::*;
use crate::window_fusion::{absolute_humidity, gas_modes, thresholds, trv_modes, Balance, Chan, FitEpisode, Inputs, Lik, Obs, PhysFit, Posterior, Tables};
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
        let mut c = Config::parse(&format!("{ROOMS}{extra}")).unwrap();
        c.window.fusion.enabled = true;
        Self { w: Windows::default(), c, t: T0, events: Vec::new() }
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
    assert!(r.p("b") > 2.0 * before, "b {} -> {}", before, r.p("b"));
    assert!(!r.open("b"), "a posterior shift without own evidence does not freeze a room");
    assert_eq!(Chan::Couple.name(), "neighbour");
}

#[test]
fn fresh_air_evidence_is_never_vetoed_by_away_but_a_manual_hold_still_vetoes() {
    let mut r = Rig::warm();
    r.w.set_context([("a".to_string(), RoomCtx { hold: false, away: true, away_fresh: true })]);
    r.run(8, venting);
    assert!(r.open("a"));
    assert!(r.of("window_suppressed").is_empty());
    let mut held = Rig::warm();
    held.w.set_context([("a".to_string(), RoomCtx { hold: true, away: false, away_fresh: false })]);
    held.run(8, venting);
    assert!(!held.open("a"));
    assert!(held.of("window_suppressed").iter().any(|e| e.detail["reason"] == "manual_hold"));
}

#[test]
fn shadow_mode_computes_the_posterior_but_leaves_the_control_alone() {
    let mut r = Rig::warm();
    r.c.window.fusion.enabled = false;
    r.run(8, venting);
    assert!(!r.open("a") && r.of("window_open").is_empty());
    assert_eq!(r.of("window_shadow").len(), 1, "{:?}", r.events);
    assert_eq!(r.of("window_shadow")[0].detail["state"], "open");
    let view = r.w.view("a", r.t);
    assert!(view["shadow"]["open"].as_bool().unwrap() && view["p"].as_f64().unwrap() > 0.4);
    r.run(30, recovering);
    assert!(r.of("window_shadow").iter().any(|e| e.detail["state"] == "close"));
}

#[test]
fn a_closed_room_cooling_under_frost_does_not_hold_itself_open() {
    let mut r = Rig::warm();
    r.run(8, venting);
    assert!(r.open("a"));
    r.run(40, |m| Air { co2: 440.0 + 40.0 * m.min(25.0) + 8.0 * (m - 25.0).max(0.0), rh: 40.0 + 0.6 * m, temp: 17.6 - 0.012 * m });
    assert!(!r.open("a"), "p {}", r.p("a"));
}

#[test]
fn a_shut_empty_room_with_flat_low_co2_is_released_within_the_level_horizon() {
    let mut r = Rig::warm();
    r.run(8, venting);
    assert!(r.open("a"));
    let mut closed_at = None;
    for m in 1..=150 {
        r.minute(Some(Air { co2: 445.0 + (m as f64 * 1.7).sin() * 3.0, rh: 40.0 + 0.02 * m as f64, temp: 17.6 + 0.4 * (m as f64 / 15.0).min(1.0) - 0.004 * (m as f64 - 15.0).max(0.0) }));
        if closed_at.is_none() && !r.open("a") {
            closed_at = Some(m);
        }
    }
    let at = closed_at.expect("released");
    assert!(at <= 110, "frost held {at} min");
}

#[test]
fn a_flat_low_co2_plateau_with_noisy_humidity_holds_until_co2_rises() {
    let mut r = Rig::warm();
    r.run(8, venting);
    assert!(r.open("a"));
    r.run(45, |m| Air { co2: 440.0 + (m * 2.7).sin() * 4.0, rh: 40.0 + (m * 1.3).sin() * 0.5 + 0.1 * (m * 0.9).sin(), temp: 17.6 + (m * 0.7).sin() * 0.01 });
    assert!(r.open("a"), "p {}", r.p("a"));
    r.run(12, |m| Air { co2: 440.0 + 45.0 * m, rh: 40.0 + m, temp: 17.6 });
    assert!(!r.open("a"), "p {}", r.p("a"));
}

#[test]
fn a_late_yes_answer_does_not_reopen_a_window_that_was_already_closed() {
    let mut r = Rig::warm();
    r.run(8, venting);
    let id = r.w.prompt(&r.c.window, r.t).expect("prompt")["id"].as_str().unwrap().to_string();
    r.w.manual("a", false, r.t);
    r.run(2, noisy);
    r.w.label("a", r.t, true, Some(&id), None, r.t);
    assert!(!r.open("a") && r.p("a") < 0.3, "p {}", r.p("a"));
}

#[test]
fn rooms_without_an_air_channel_behave_exactly_like_the_old_detector() {
    let run = |fusion: bool| {
        let c = Config::parse(&format!("[window.fusion]\nenabled = {fusion}
shadow = false\n[[rooms]]\nname = 'wohnzimmer'\nnodes = [5]\n[[rooms]]\nname = 'kueche'\nnodes = [8]\n")).unwrap();
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
fn the_bedroom_incident_phase_b_is_held_by_the_air_channels_alone_and_phase_a_rises_partially() {
    let text = include_str!("../tests/fixtures/airwin/incident.csv");
    let rows: Vec<Vec<f64>> = text.lines().skip(1).map(|l| l.split(',').map(|v| v.parse().unwrap()).collect()).collect();
    let mut r = Rig::new("");
    let start = T0;
    let mut open_at: Vec<bool> = Vec::new();
    let mut peak_a: f64 = 0.0;
    for row in &rows {
        let ts = start + (row[0] * 1000.0) as i64;
        r.t = ts;
        r.w.observe_air(&r.c, AIR_NODE, TEMPERATURE_MEASUREMENT_CLUSTER, row[1], ts);
        r.w.observe_air(&r.c, AIR_NODE, HUMIDITY_MEASUREMENT_CLUSTER, row[2], ts);
        r.w.observe_air(&r.c, AIR_NODE, CO2_CLUSTER, row[3], ts);
        let found = r.w.step(&r.c, ts, |_| [0.5, 2.0, 0.0]);
        r.events.extend(found);
        open_at.push(r.open("a"));
        if (1700.0..2000.0).contains(&row[0]) {
            peak_a = peak_a.max(r.p("a"));
        }
    }
    let at = |s: f64| open_at[rows.iter().position(|row| row[0] >= s).unwrap()];
    let first = |from: f64, to: f64| rows.iter().zip(&open_at).find(|(row, o)| row[0] >= from && row[0] <= to && **o).map(|(row, _)| row[0]);
    assert!(first(3500.0, 4000.0).is_some_and(|s| s <= 66.0 * 60.0), "phase B: {:?}", first(3500.0, 4200.0));
    assert!(peak_a > 0.3, "phase A is only a partial rise on air alone with tables learned without these days: {peak_a}");
    assert!((66 * 60..=105 * 60).step_by(60).all(|s| at(s as f64)), "phase B stays open from 08:06 to 08:45");
}

fn physics_cfg() -> WindowFusion {
    WindowFusion { engine: "physics".into(), ..WindowFusion::default() }
}

fn obs(rate: f64, level: f64, sd: f64) -> Obs {
    Obs { rate, level, sd }
}

fn physics_step(p: &mut Posterior, cfg: &WindowFusion, inputs: &Inputs) {
    p.update(cfg, &Tables::builtin(), inputs);
}

fn physics_inputs(cfg: &WindowFusion, balance: Balance) -> Inputs {
    Inputs { phys: Some(Tables::builtin().phys_room(&cfg.physics, "a")), balance, out_c: 3.0, valve: 0.0, ..Inputs::default() }
}

#[test]
fn the_balance_likelihood_is_one_sided_a_fall_beyond_closed_ventilation_is_nearly_impossible_a_rise_is_cheap() {
    let ph = &physics_cfg().physics;
    let l = |rate: f64| gas_modes(obs(rate, 1020.0, 9.6), 600.0, ph, ph.lambda_h, ph.co2_source_ppm_min, 1.0).modes;
    let (flat, rise, fall) = (l(0.0), l(5.0), l(-60.0));
    assert!(rise[0] - flat[0] > -2.0, "a source explains a rise under a closed window: {} {}", rise[0], flat[0]);
    assert!(fall[0] - flat[0] < -10.0, "no source can pull the level down: {} {}", fall[0], flat[0]);
    assert!(fall[2] - fall[0] > 10.0, "a fast fall is strong evidence for the open mode: {:?}", fall);
    assert!(rise[2] - rise[0] < -5.0, "a rise argues against the open mode: {:?}", rise);
    let slow = l(-14.0);
    assert!(slow[1] > slow[0] && slow[1] > slow[2], "a moderate fall fits the tilted mode best: {slow:?}");
}

#[test]
fn near_the_outdoor_level_the_gas_channels_carry_no_evidence() {
    let cfg = physics_cfg();
    let ph = &cfg.physics;
    let at_outdoor = gas_modes(obs(-30.0, 425.0, 9.6), 5.0, ph, ph.lambda_h, ph.co2_source_ppm_min, 1.0).modes;
    assert!((at_outdoor[2] - at_outdoor[0]).abs() < 1.0, "{at_outdoor:?}");
    let mut p = Posterior::default();
    let dry = Balance { ah: Some(obs(-0.2, absolute_humidity(80.0, 3.0) + 0.4, 0.02)), ..Balance::default() };
    physics_step(&mut p, &cfg, &physics_inputs(&cfg, dry));
    assert_eq!(p.added[Chan::Rh as usize], 0.0, "the humidity channel is gated off below 1 g/m3 of excess");
    let wet = Balance { ah: Some(obs(-0.2, absolute_humidity(80.0, 3.0) + 3.0, 0.02)), ..Balance::default() };
    physics_step(&mut p, &cfg, &physics_inputs(&cfg, wet));
    assert!(p.added[Chan::Rh as usize] > 0.0);
}

fn door_or_window(temp_rate: f64, ah_rate: f64) -> Posterior {
    let cfg = physics_cfg();
    let mut p = Posterior::default();
    let balance = Balance { co2: Some(obs(-35.0, 1100.0, 9.6)), ah: Some(obs(ah_rate, absolute_humidity(80.0, 3.0) + 3.5, 0.0095)), temp: Some(obs(temp_rate, 18.0, 0.003)) };
    for _ in 0..4 {
        physics_step(&mut p, &cfg, &physics_inputs(&cfg, balance));
    }
    p
}

#[test]
fn the_night_door_pattern_gas_falls_but_the_air_stays_flat_does_not_open_a_real_window_does() {
    let cfg = physics_cfg();
    let (on, _) = thresholds(&cfg, 15.0, 1.0);
    let door = door_or_window(0.0, -0.01);
    let window = door_or_window(-0.06, -0.3);
    assert!(door.own < on, "door pattern stays closed: own {} on {on}", door.own);
    assert!(window.own > on, "a joint fall of gas and air temperature opens: own {} on {on}", window.own);
}

#[test]
fn an_unchanged_thermostat_reading_gives_no_false_closed_evidence_beyond_the_uncensored_value() {
    let ph = physics_cfg().physics;
    let sigma = 0.5;
    let plain = |z: f64| (ph.trv_open_z * z - ph.trv_open_z * ph.trv_open_z / 2.0) / (ph.trv_sd * ph.trv_sd);
    for z in [-0.5, 0.0, 0.3] {
        let l = trv_modes(z, sigma, false, &ph, ph.trv_beta).modes[2];
        assert!(l >= plain(z).max(-ph.trv_cap) - 1e-9, "censored {l} vs uncensored {} at z {z}", plain(z));
    }
    let strong = trv_modes(3.0, sigma, false, &ph, ph.trv_beta).modes[2];
    assert!(strong > 0.5 && strong <= ph.trv_cap, "{strong}");
    assert!(trv_modes(3.0, sigma, true, &ph, ph.trv_beta).modes[2] < strong, "an open valve masks part of the drop");
    assert_eq!(trv_modes(f64::NAN, sigma, false, &ph, ph.trv_beta), Lik::default());
}

#[test]
fn physics_runs_on_priors_alone_and_a_missing_channel_contributes_exactly_zero() {
    let t = Tables::builtin();
    assert!(t.physics.is_none(), "the built-in table carries no physics fit");
    let cfg = physics_cfg();
    let room = t.phys_room(&cfg.physics, "a");
    assert_eq!(room.lambda_h, cfg.physics.lambda_h);
    assert_eq!(room.weight, cfg.physics.weight);
    let mut p = Posterior::default();
    for _ in 0..30 {
        physics_step(&mut p, &cfg, &physics_inputs(&cfg, Balance::default()));
    }
    assert_eq!(p.added, [0.0; crate::window_fusion::CHANNELS]);
    assert!((p.own - Posterior::prior(&cfg)).abs() < 1e-2, "no data keeps the prior: {} {}", p.own, Posterior::prior(&cfg));
    assert!(!p.independent);
    let mut q = Posterior::default();
    let only_co2 = Balance { co2: Some(obs(-35.0, 1100.0, 9.6)), ..Balance::default() };
    physics_step(&mut q, &cfg, &physics_inputs(&cfg, only_co2));
    assert!(q.added[Chan::Rh as usize] == 0.0 && q.added[Chan::Temp as usize] == 0.0 && q.added[Chan::Trv as usize] == 0.0 && q.added[Chan::Co2 as usize] > 0.0);
}

#[test]
fn the_fitted_lambda_of_other_episodes_replaces_the_prior_only_for_its_room() {
    let fit = PhysFit {
        episodes: vec![FitEpisode { room: "a".into(), from_ms: 1, lambda_h: 5.0 }, FitEpisode { room: "a".into(), from_ms: 2, lambda_h: 1.2 }, FitEpisode { room: "b".into(), from_ms: 3, lambda_h: 9.0 }],
        ..PhysFit::default()
    };
    let held_out = fit.leave_out("a", 1, 3.0);
    assert_eq!(held_out.lambda_open.get("a"), Some(&9.0), "no open fit left in the room, the pooled one is used");
    assert_eq!(held_out.lambda_kipp.get("a"), Some(&1.2));
    let full = fit.leave_out("a", 99, 3.0);
    assert_eq!(full.lambda_open.get("a"), Some(&5.0));
}

#[test]
fn the_physics_engine_opens_on_a_venting_room_holds_it_and_closes_on_recovery() {
    let mut r = Rig::new("[window.fusion]\nengine = 'physics'\n");
    r.run(25, noisy);
    assert!(r.events.is_empty(), "{:?}", r.events);
    r.run(10, venting);
    assert!(r.open("a"), "p {}", r.p("a"));
    r.run(30, recovering);
    assert!(!r.open("a"), "p {}", r.p("a"));
    assert_eq!(r.of("window_open").len(), 1);
}

#[test]
fn the_bedroom_incident_is_found_by_the_physics_engine_without_any_fitted_table() {
    let text = include_str!("../tests/fixtures/airwin/incident.csv");
    let rows: Vec<Vec<f64>> = text.lines().skip(1).map(|l| l.split(',').map(|v| v.parse().unwrap()).collect()).collect();
    let mut r = Rig::new("[window.fusion]\nengine = 'physics'\n");
    let mut open_at: Vec<bool> = Vec::new();
    for row in &rows {
        let ts = T0 + (row[0] * 1000.0) as i64;
        r.t = ts;
        r.w.observe_air(&r.c, AIR_NODE, TEMPERATURE_MEASUREMENT_CLUSTER, row[1], ts);
        r.w.observe_air(&r.c, AIR_NODE, HUMIDITY_MEASUREMENT_CLUSTER, row[2], ts);
        r.w.observe_air(&r.c, AIR_NODE, CO2_CLUSTER, row[3], ts);
        let found = r.w.step(&r.c, ts, |_| [0.5, 2.0, 0.0]);
        r.events.extend(found);
        open_at.push(r.open("a"));
    }
    let at = |s: f64| open_at[rows.iter().position(|row| row[0] >= s).unwrap()];
    assert!((66 * 60..=105 * 60).step_by(60).all(|s| at(s as f64)), "phase B stays open from 08:06 to 08:45");
    assert!(rows.iter().zip(&open_at).any(|(row, o)| *o && (1700.0..2300.0).contains(&row[0])), "phase A is found");
}
