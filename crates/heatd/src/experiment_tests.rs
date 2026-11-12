use crate::experiment::{report_section, schedule, Doors, Here, Messfahrt, Phase, RoomIn, Tick, COOLDOWN_MIN, TOTAL_MIN};
use crate::lake::kind;
use crate::manual_tests::{call, env, env_with, Env};
use axum::http::StatusCode;
use chrono::NaiveDate;
use serde_json::{json, Value};
use std::sync::Arc;

const MIN: i64 = 60_000;
const DATE: &str = "2026-11-09";

fn rooms<'a>(names: &[&'a str], held: &[&str], window: &[&str], temp: Option<f64>) -> Vec<RoomIn<'a>> {
    names.iter().map(|n| RoomIn { room: n, held: held.contains(n), window: window.contains(n), temp }).collect()
}

fn tick(m: &mut Messfahrt, now_ms: i64, here: Here, night: bool, list: &[RoomIn]) -> crate::experiment::Out {
    m.tick(&Tick { now_ms, date: DATE, here, night, cap_c: 23.0, rooms: list })
}

fn running(names: &[&str]) -> (Messfahrt, i64) {
    let mut m = Messfahrt::default();
    m.arm(Doors::Closed, "chat", 0).unwrap();
    tick(&mut m, 1_000, Here::Away, false, &rooms(names, &[], &[], Some(19.0)));
    (m, 1_000)
}

fn state_of(m: &Messfahrt, now_ms: i64) -> String {
    m.view(now_ms)["state"].as_str().unwrap().to_string()
}

#[test]
fn schedule_is_deterministic_bounded_and_decorrelated() {
    let a = schedule("wohnzimmer", Doors::Closed, DATE);
    assert_eq!(a, schedule("wohnzimmer", Doors::Closed, DATE));
    assert_ne!(a, schedule("wohnzimmer", Doors::Open, DATE));
    assert_ne!(a, schedule("wohnzimmer", Doors::Closed, "2026-11-10"));
    assert_ne!(a, schedule("bad", Doors::Closed, DATE));
    assert_eq!((a[0].from_min, a[0].to_min, a[0].on, a[0].phase), (0, COOLDOWN_MIN, false, Phase::Cooldown));
    assert_eq!(a.last().unwrap().to_min, TOTAL_MIN);
    assert!(a[1..].iter().all(|s| s.phase == Phase::Pulses && [10, 20, 30, 40].contains(&(s.to_min - s.from_min))));
    assert!(a.windows(2).all(|w| w[0].to_min == w[1].from_min && (w[1].phase == Phase::Cooldown || w[0].on != w[1].on || w[0].phase == Phase::Cooldown)));
    let names = ["a", "b", "c", "d", "e", "f"];
    let all: Vec<_> = names.iter().map(|n| schedule(n, Doors::Open, DATE)).collect();
    assert!(all.iter().all(|r| r.iter().any(|s| s.on)));
    for i in 0..all.len() {
        assert!((i + 1..all.len()).all(|j| all[i] != all[j]));
    }
}

#[test]
fn arm_replaces_an_armed_run_refuses_a_running_one_and_cancel_ends_it() {
    let mut m = Messfahrt::default();
    assert_eq!(state_of(&m, 0), "idle");
    assert!(m.cancel(0).is_none());
    m.arm(Doors::Closed, "chat", 5).unwrap();
    m.arm(Doors::Open, "watch", 6).unwrap();
    let view = m.view(6);
    assert_eq!((view["state"].as_str(), view["doors"].as_str(), view["source"].as_str()), (Some("armed"), Some("open"), Some("watch")));
    assert_eq!(view["expires_ms"].as_i64(), Some(6 + 24 * 60 * MIN));
    tick(&mut m, 10, Here::Away, false, &rooms(&["a"], &[], &[], None));
    assert!(m.arm(Doors::Closed, "chat", 11).is_err());
    let ended = m.cancel(12).unwrap();
    assert_eq!(serde_json::from_str::<Value>(ended.detail.as_deref().unwrap()).unwrap()["reason"], "cancel");
    assert_eq!((state_of(&m, 12), m.view(12)["reason"].as_str().map(String::from)), ("aborted".into(), Some("cancel".into())));
    assert!(!m.running_in("a"));
    assert!(m.arm(Doors::Closed, "chat", 13).is_ok());
}

#[test]
fn an_armed_run_expires_after_24_hours_without_absence() {
    let mut m = Messfahrt::default();
    m.arm(Doors::Closed, "chat", 0).unwrap();
    let list = rooms(&["a"], &[], &[], None);
    assert!(tick(&mut m, 24 * 60 * MIN - 1, Here::Home, false, &list).events.is_empty());
    assert_eq!(state_of(&m, 0), "armed");
    let out = tick(&mut m, 24 * 60 * MIN, Here::Home, false, &list);
    assert_eq!(out.events.len(), 1);
    assert_eq!((state_of(&m, 0), m.view(0)["reason"].as_str().map(String::from)), ("aborted".into(), Some("expired".into())));
}

#[test]
fn it_starts_only_when_away_and_not_at_night() {
    let mut m = Messfahrt::default();
    m.arm(Doors::Closed, "chat", 0).unwrap();
    let list = rooms(&["a", "b"], &[], &[], Some(19.0));
    for (here, night) in [(Here::Home, false), (Here::Unknown, false), (Here::Away, true)] {
        let out = tick(&mut m, MIN, here, night, &list);
        assert!(out.events.is_empty() && out.calls.is_empty() && state_of(&m, MIN) == "armed");
    }
    let out = tick(&mut m, 2 * MIN, Here::Away, false, &list);
    assert_eq!(state_of(&m, 2 * MIN), "running");
    assert_eq!(out.events[0].kind, kind::EXPERIMENT_STARTED);
    let detail: Value = serde_json::from_str(out.events[0].detail.as_deref().unwrap()).unwrap();
    assert_eq!(detail["schedule"]["a"].as_array().unwrap().len(), schedule("a", Doors::Closed, DATE).len());
    assert_eq!(detail["planned_end_ms"].as_i64(), Some(2 * MIN + TOTAL_MIN * MIN));
    assert!(out.calls.values().all(|c| !c.on && c.rule == "experiment_abkuehlen"));
}

#[test]
fn cooldown_is_pure_frost_and_pulses_follow_the_plan_with_a_safety_cap() {
    let (mut m, t0) = running(&["a", "b"]);
    let plan = schedule("a", Doors::Closed, DATE);
    for s in &plan[1..] {
        let at = t0 + s.from_min * MIN;
        let call = tick(&mut m, at, Here::Away, false, &rooms(&["a", "b"], &[], &[], Some(19.0))).calls["a"];
        assert_eq!((call.on, call.rule), (s.on, "experiment_takt"));
    }
    let on = plan.iter().find(|s| s.on).unwrap();
    let (mut m, t0) = running(&["a"]);
    let at = t0 + on.from_min * MIN;
    assert!(tick(&mut m, at, Here::Away, false, &rooms(&["a"], &[], &[], Some(20.0))).calls["a"].on);
    let capped = tick(&mut m, at + MIN, Here::Away, false, &rooms(&["a"], &[], &[], Some(23.5)));
    assert!(!capped.calls["a"].on);
    assert!(capped.events.iter().any(|e| e.detail.as_deref().unwrap().contains("\"capped\":true")));
    assert!(!tick(&mut m, at + 2 * MIN, Here::Away, false, &rooms(&["a"], &[], &[], Some(19.0))).calls["a"].on);
    let next = plan.iter().find(|s| s.from_min >= on.to_min && s.on);
    if let Some(next) = next {
        assert!(tick(&mut m, t0 + next.from_min * MIN, Here::Away, false, &rooms(&["a"], &[], &[], Some(19.0))).calls["a"].on);
    }
}

#[test]
fn homecoming_cancel_and_completion_end_the_run_with_a_reason() {
    for (here, at, reason, status) in [(Here::Home, 30 * MIN, "homecoming", "aborted"), (Here::Away, TOTAL_MIN * MIN + 1_000, "completed", "done")] {
        let (mut m, _) = running(&["a"]);
        let out = tick(&mut m, at, here, false, &rooms(&["a"], &[], &[], Some(19.0)));
        assert!(out.calls.is_empty());
        let view = m.view(at);
        assert_eq!((view["state"].as_str(), view["reason"].as_str()), (Some(status), Some(reason)));
        assert!(!m.running_in("a"));
    }
    let (mut m, _) = running(&["a"]);
    let out = tick(&mut m, 10 * MIN, Here::Unknown, false, &rooms(&["a"], &[], &[], Some(19.0)));
    assert!(out.calls.contains_key("a"));
}

#[test]
fn window_and_manual_hold_end_only_that_rooms_part() {
    let (mut m, _) = running(&["a", "b", "c"]);
    let out = tick(&mut m, 10 * MIN, Here::Away, false, &rooms(&["a", "b", "c"], &["b"], &["a"], Some(19.0)));
    assert_eq!(out.calls.keys().cloned().collect::<Vec<_>>(), vec!["c".to_string()]);
    let view = m.view(10 * MIN);
    let ended: Vec<_> = view["rooms"].as_array().unwrap().iter().map(|r| (r["room"].as_str().unwrap().to_string(), r["ended"].as_str().map(String::from))).collect();
    assert_eq!(ended, vec![("a".into(), Some("window".into())), ("b".into(), Some("manual_hold".into())), ("c".into(), None)]);
    assert!(m.running_in("c") && !m.running_in("a"));
    let out = tick(&mut m, 11 * MIN, Here::Away, false, &rooms(&["a", "b", "c"], &[], &[], Some(19.0)));
    assert_eq!(out.calls.len(), 1);
}

fn as_rows(events: &[crate::lake::Event]) -> Vec<(i64, String, Value)> {
    events.iter().map(|e| (e.ts / 1000, e.kind.clone(), serde_json::from_str(e.detail.as_deref().unwrap()).unwrap())).collect()
}

#[test]
fn events_restore_a_run_and_end_stale_ones() {
    let mut m = Messfahrt::default();
    let mut events = vec![m.arm(Doors::Closed, "chat", 0).unwrap()];
    let list = rooms(&["a", "b"], &["b"], &[], Some(19.0));
    let (t0, at) = (MIN, (COOLDOWN_MIN + 2) * MIN);
    for t in [t0, at, at + 10 * MIN] {
        events.extend(tick(&mut m, t, Here::Away, false, &list).events);
    }
    let rows = as_rows(&events);
    let now = at + 11 * MIN;
    let before = m.view(now);
    let (back, ended) = Messfahrt::from_events(&rows, now, Here::Away);
    assert!(ended.is_empty());
    let view = back.view(now);
    assert_eq!((view["state"].as_str(), view["started_ms"].clone(), view["rooms"][1]["ended"].as_str()), (Some("running"), before["started_ms"].clone(), Some("manual_hold")));
    assert_eq!(view["rooms"][0]["executed"], before["rooms"][0]["executed"]);
    assert!(view["rooms"][0]["executed"].as_u64().unwrap() >= 2);
    let (home, ended) = Messfahrt::from_events(&rows, now, Here::Unknown);
    assert_eq!((state_of(&home, 0), ended.len(), home.view(0)["reason"].as_str().map(String::from)), ("aborted".into(), 1, Some("restart".into())));
    let (late, ended) = Messfahrt::from_events(&rows, t0 + (TOTAL_MIN + 1) * MIN, Here::Away);
    assert_eq!((state_of(&late, 0), ended.len()), ("aborted".into(), 1));
    assert_eq!(state_of(&Messfahrt::from_events(&rows[..1], 5 * MIN, Here::Home).0, 0), "armed");
    let (expired, ended) = Messfahrt::from_events(&rows[..1], 25 * 60 * MIN, Here::Home);
    assert_eq!((state_of(&expired, 0), expired.view(0)["reason"].as_str().map(String::from), ended.len()), ("aborted".into(), Some("expired".into()), 1));
    let mut finished = rows.clone();
    finished.extend(as_rows(&Messfahrt::from_events(&rows, now, Here::Unknown).1));
    let (done, ended) = Messfahrt::from_events(&finished, now + MIN, Here::Away);
    assert!(ended.is_empty());
    assert_eq!(done.view(0)["reason"], "restart");
}

async fn away_scan(e: &Env) {
    for (ts, motion) in [(e.t - 40 * MIN, "moving"), (e.t - MIN, "still")] {
        call(&e.state, "POST", "/api/scan", Some(json!({"ts": ts, "device": "pixel-watch", "motion": motion, "away": true}))).await;
    }
}

async fn arm(e: &Env, doors: &str) -> (StatusCode, Value) {
    call(&e.state, "POST", "/api/experiment", Some(json!({"doors": doors, "source": "watch"}))).await
}

fn local_temp(e: &Env, node: u64, value: f64) {
    let report = matter_controller::AttributeReport { node, endpoint: 1, attribute: "local_temperature".into(), value };
    crate::rules::on_report(&e.state, &report, false, e.t);
}

#[tokio::test]
async fn api_validates_arms_cancels_and_shows_the_state_block() {
    let e = env();
    for body in [json!({}), json!({"doors": "ajar"}), json!({"doors": "closed", "source": "dial"})] {
        let (status, reply) = call(&e.state, "POST", "/api/experiment", Some(body.clone())).await;
        assert_eq!((status, reply["ok"].as_bool()), (StatusCode::BAD_REQUEST, Some(false)), "{body}");
    }
    assert_eq!(call(&e.state, "GET", "/api/experiment", None).await.1["state"], "idle");
    let (status, reply) = arm(&e, "closed").await;
    assert_eq!((status, reply["experiment"]["state"].as_str(), reply["experiment"]["doors"].as_str()), (StatusCode::OK, Some("armed"), Some("closed")));
    let state = call(&e.state, "GET", "/api/state", None).await.1;
    assert_eq!((state["experiment"]["state"].as_str(), state["experiment"]["source"].as_str()), (Some("armed"), Some("watch")));
    let (_, reply) = call(&e.state, "POST", "/api/experiment", Some(json!({"cancel": true}))).await;
    assert_eq!((reply["cancelled"].as_bool(), reply["experiment"]["reason"].as_str()), (Some(true), Some("cancel")));
    assert_eq!(call(&e.state, "POST", "/api/experiment", Some(json!({"cancel": true}))).await.1["cancelled"], false);
    let request = axum::http::Request::builder().method("POST").uri("/api/experiment").header("content-type", "application/json").body(axum::body::Body::from("{")).unwrap();
    let response = tower::ServiceExt::oneshot(crate::http::router(e.state.clone()), request).await.unwrap();
    assert_eq!(response.status(), StatusCode::BAD_REQUEST);
}

#[tokio::test]
async fn a_run_waits_for_the_absence_then_writes_frost_and_pulses_through_the_normal_path() {
    let mut e = env_with("raise_budget_per_hour = 1\n");
    arm(&e, "closed").await;
    e.step(1).await;
    assert_eq!(state_of_view(&e).await, "armed");
    away_scan(&e).await;
    let wants = e.step(1).await;
    assert_eq!(state_of_view(&e).await, "running");
    assert!(wants.iter().all(|w| (w.reason, w.rule, w.setpoint) == ("experiment", "experiment_abkuehlen", 8.0)), "{wants:?}");
    assert_eq!((e.device(5).await, e.device(6).await), (8.0, 8.0));
    let plan = schedule("wohnzimmer", Doors::Closed, DATE);
    let first_on = plan.iter().find(|s| s.on).unwrap();
    let boost_max = e.state.config.failsafe.boost_max_c;
    let started = call(&e.state, "GET", "/api/experiment", None).await.1["started_ms"].as_i64().unwrap();
    let mut seen_on = false;
    let mut minute = 0;
    while minute < first_on.to_min + 1 {
        let step = 5;
        e.step(step).await;
        minute += step;
        away_scan(&e).await;
        local_temp(&e, 5, 19.0);
        let elapsed = (e.t - started) / MIN;
        if let Some(seg) = plan.iter().find(|s| elapsed >= s.from_min && elapsed < s.to_min) {
            let device = e.device(5).await;
            assert!(device <= boost_max && (device == 8.0 || seg.on), "{elapsed} {seg:?} {device}");
            seen_on |= seg.on && device == boost_max;
        }
    }
    assert!(seen_on);
    let used = e.state.commanded.lock().unwrap().budget.used("wohnzimmer", e.t);
    assert_eq!(used, 0);
    let rules: Vec<String> = e.reasons();
    assert!(rules.iter().any(|x| x.contains("\"reason\":\"experiment\"")) && rules.iter().any(|x| x.contains("exempt_experiment")), "{rules:?}");
    let (_, reply) = call(&e.state, "POST", "/api/experiment", Some(json!({"cancel": true}))).await;
    assert_eq!(reply["experiment"]["state"], "aborted");
    away_scan(&e).await;
    e.step(1).await;
    assert_eq!((e.device(5).await, e.device(6).await), (18.0, 18.0));
}

async fn state_of_view(e: &Env) -> String {
    call(&e.state, "GET", "/api/experiment", None).await.1["state"].as_str().unwrap().to_string()
}

#[tokio::test]
async fn an_open_window_and_a_manual_hold_win_over_the_run() {
    let mut e = env();
    call(&e.state, "POST", "/api/setpoint", Some(json!({"room": "bad", "celsius": 22.0}))).await;
    arm(&e, "open").await;
    away_scan(&e).await;
    e.step(1).await;
    let view = call(&e.state, "GET", "/api/experiment", None).await.1;
    let ended: Vec<_> = view["rooms"].as_array().unwrap().iter().map(|r| (r["room"].as_str().unwrap().to_string(), r["ended"].as_str().map(String::from))).collect();
    assert!(ended.contains(&("bad".into(), Some("manual_hold".into()))) && ended.contains(&("wohnzimmer".into(), None)));
    assert_eq!((e.device(5).await, e.device(6).await), (8.0, 22.0));
    crate::intent::apply(&e.state, crate::intent::Intent::WindowOpen { room: "wohnzimmer".into() });
    away_scan(&e).await;
    let wants = e.step(1).await;
    assert!(wants.iter().filter(|w| w.room == "wohnzimmer").all(|w| w.reason == "window_open"));
    let view = call(&e.state, "GET", "/api/experiment", None).await.1;
    assert_eq!(view["rooms"][0]["ended"], "window");
    assert_eq!(view["state"], "running");
    assert!(!e.state.commanded.lock().unwrap().messfahrt.running_in("wohnzimmer"));
}

#[tokio::test]
async fn armed_and_running_runs_survive_a_restart_through_the_lake() {
    let mut e = env();
    arm(&e, "closed").await;
    e.state.sink.flush().unwrap();
    *e.state.commanded.lock().unwrap() = Default::default();
    assert_eq!(crate::experiment::restore(&e.state, e.t + MIN).unwrap(), "armed");
    away_scan(&e).await;
    e.step(1).await;
    e.step(5).await;
    e.state.sink.flush().unwrap();
    *e.state.commanded.lock().unwrap() = Default::default();
    away_scan(&e).await;
    assert_eq!(crate::experiment::restore(&e.state, e.t + MIN).unwrap(), "running");
    *e.state.commanded.lock().unwrap() = Default::default();
    let stale = e.t + 600 * MIN;
    assert_eq!(crate::experiment::restore(&e.state, stale).unwrap(), "aborted");
    assert_eq!(e.state.commanded.lock().unwrap().messfahrt.view(stale)["reason"], "restart");
}

#[test]
fn the_report_lists_runs_with_planned_and_executed_steps_per_room() {
    let dir = tempfile::tempdir().unwrap();
    let date = "2026-11-10".parse::<NaiveDate>().unwrap();
    let base = crate::report::midnight_us(date);
    let sink = crate::lake::Sink::open_with_clock(crate::lake::LakeConfig::new(dir.path()), Arc::new(move || base)).unwrap();
    let (b, t0) = (base / 1000, base / 1000 + 500 * MIN);
    let mut m = Messfahrt::default();
    let mut events = vec![m.arm(Doors::Open, "chat", b + 480 * MIN).unwrap()];
    for k in 0..40 {
        let list = [RoomIn { room: "wohnzimmer", held: false, window: false, temp: Some(17.0 + k as f64 * 0.1) }];
        events.extend(m.tick(&Tick { now_ms: t0 + k * 5 * MIN, date: DATE, here: Here::Away, night: false, cap_c: 23.0, rooms: &list }).events);
    }
    events.extend(m.cancel(t0 + 200 * MIN));
    for e in events {
        sink.append_on(date, e).unwrap();
    }
    sink.roll_day(date).unwrap();
    let md = report_section(dir.path(), date).unwrap();
    assert!(md.contains("## Messfahrten") && md.contains("Türen offen") && md.contains("Grund cancel"), "{md}");
    assert!(md.contains("wohnzimmer: ") && md.contains(" von ") && md.contains("Schritten ausgeführt") && md.contains("Sensor 17 bis 20,9 °C"), "{md}");
    assert!(report_section(tempfile::tempdir().unwrap().path(), date).unwrap().contains("Keine Messfahrt"));
}

#[test]
fn a_missing_sensor_reading_keeps_the_room_off_without_capping_the_step() {
    let (mut m, t0) = running(&["a"]);
    let on = schedule("a", Doors::Closed, DATE).into_iter().find(|s| s.on).unwrap();
    let at = t0 + on.from_min * MIN;
    let blind = tick(&mut m, at, Here::Away, false, &rooms(&["a"], &[], &[], None));
    assert!(!blind.calls["a"].on && blind.events.iter().all(|e| !e.detail.as_deref().unwrap().contains("\"capped\":true")));
    assert!(tick(&mut m, at + MIN, Here::Away, false, &rooms(&["a"], &[], &[], Some(19.0))).calls["a"].on);
}

#[tokio::test]
async fn a_watch_back_in_saver_mode_ends_the_run_as_homecoming() {
    let mut e = env();
    arm(&e, "closed").await;
    away_scan(&e).await;
    e.step(1).await;
    assert_eq!(state_of_view(&e).await, "running");
    call(&e.state, "POST", "/api/scan", Some(json!({"ts": e.t + 30 * MIN, "device": "pixel-watch", "motion": "still", "power": "saver", "away": false}))).await;
    let wants = e.step(31).await;
    let view = call(&e.state, "GET", "/api/experiment", None).await.1;
    assert_eq!((view["state"].as_str(), view["reason"].as_str()), (Some("aborted"), Some("homecoming")));
    assert!(wants.iter().all(|w| w.reason != "experiment"));
    assert!(!e.state.commanded.lock().unwrap().messfahrt.running_in("wohnzimmer"));
}

#[tokio::test]
async fn an_arming_event_from_the_middle_day_of_the_lookback_is_restored() {
    let e = env();
    let armed_ms = "2026-11-09".parse::<NaiveDate>().unwrap().and_hms_opt(2, 0, 0).unwrap().and_utc().timestamp_millis();
    let event = Messfahrt::default().arm(Doors::Open, "chat", armed_ms).unwrap();
    e.state.sink.append_on("2026-11-09".parse::<NaiveDate>().unwrap(), event).unwrap();
    e.state.sink.flush().unwrap();
    let now = armed_ms + 23 * 60 * MIN;
    assert_eq!(crate::experiment::restore(&e.state, now).unwrap(), "armed");
    assert_eq!(e.state.commanded.lock().unwrap().messfahrt.view(now)["doors"], "open");
}
