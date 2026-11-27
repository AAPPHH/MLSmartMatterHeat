use crate::config::Config;
use crate::http::Scope;
use crate::manual_tests::{call, env_with};
use crate::plan::{self, Action, NewPlan, Plan, Plans, When};
use crate::rules::{desired_planned, Presence};
use axum::http::StatusCode;
use chrono::{NaiveDate, NaiveDateTime};
use serde_json::json;
use std::collections::{HashMap, HashSet};

const CONFIG: &str = "default_setpoint = 20.0
away_setpoint = 17.0
frost_setpoint = 8.0
occupied_setpoint = 21.0
[watchmatch.state]
sleep_hours = '23-6'
[[rooms]]
name = 'wohnzimmer'
nodes = [5]
home_setpoint = 21.0
[[rooms]]
name = 'bad'
nodes = [6]
[[rooms.schedules]]
days = ['weekdays']
from = '06:00'
to = '08:00'
setpoint = 22.0
[[rooms]]
name = 'flur'
";

fn config() -> Config {
    Config::parse(CONFIG).unwrap()
}

fn at(day: u32, h: u32, m: u32) -> NaiveDateTime {
    NaiveDate::from_ymd_opt(2026, 11, day).unwrap().and_hms_opt(h, m, 0).unwrap()
}

fn plan(id: &str, created: i64, rooms: &[&str], action: Action, when: When) -> Plan {
    Plan { id: id.into(), rooms: rooms.iter().map(|r| r.to_string()).collect(), action, when, label: id.into(), source: "api".into(), created_ms: created }
}

fn once(from: &str, to: &str) -> When {
    When::Once { from: from.into(), to: to.into() }
}

fn weekly(days: &[&str], from: &str, to: &str, valid_from: Option<&str>, valid_until: Option<&str>) -> When {
    When::Weekly { days: days.iter().map(|d| d.to_string()).collect(), from: from.into(), to: to.into(), valid_from: valid_from.map(Into::into), valid_until: valid_until.map(Into::into) }
}

fn wants(plans: &[Plan], now: NaiveDateTime, presence: Option<&Presence>, windows: &[&str]) -> Vec<(String, f64, &'static str)> {
    let windows: HashSet<String> = windows.iter().map(|w| w.to_string()).collect();
    desired_planned(now, &config(), presence, &HashSet::new(), &windows, &HashMap::new(), None, plans).into_iter().map(|d| (d.room, d.setpoint, d.reason)).collect()
}

fn get(list: &[(String, f64, &'static str)], room: &str) -> (f64, &'static str) {
    list.iter().find(|w| w.0 == room).map(|w| (w.1, w.2)).unwrap()
}

fn away() -> Presence {
    Presence { room: None, prediction_age_minutes: None, motion: None, power: None, away: true, contact_age_minutes: Some(1.0), source: "matcher", sleep: false, bedtime: false }
}

fn home() -> Presence {
    Presence { room: Some("flur".into()), prediction_age_minutes: Some(1.0), motion: None, power: None, away: false, contact_age_minutes: None, source: "matcher", sleep: false, bedtime: false }
}

fn new_plan(rooms: &[&str], action: Action, when: When) -> NewPlan {
    NewPlan { rooms: rooms.iter().map(|r| r.to_string()).collect(), action, when, label: String::new(), source: "api".into() }
}

#[test]
fn once_plan_matches_half_open_range_and_all_rooms() {
    let p = plan("a", 1, &["all"], Action::Setpoint { celsius: 23.0 }, once("2026-11-28T15:00", "2026-11-28T18:00"));
    assert!(!p.active(at(28, 14, 59)) && p.active(at(28, 15, 0)) && p.active(at(28, 17, 59)) && !p.active(at(28, 18, 0)));
    assert!(p.applies("bad") && p.applies("wohnzimmer"));
    assert!(!plan("b", 1, &["bad"], Action::Away, once("2026-11-28T15:00", "2026-11-28T18:00")).applies("wohnzimmer"));
}

#[test]
fn weekly_plan_handles_weekdays_overnight_and_valid_dates() {
    let p = plan("a", 1, &["bad"], Action::Frost, weekly(&["saturday"], "18:00", "23:00", None, None));
    assert!(p.active(at(28, 18, 0)) && p.active(at(28, 22, 59)) && !p.active(at(28, 23, 0)) && !p.active(at(27, 20, 0)) && !p.active(at(29, 20, 0)));
    let night = plan("n", 1, &["bad"], Action::Frost, weekly(&["saturday"], "22:00", "02:00", None, Some("2026-11-28")));
    assert!(night.active(at(28, 23, 0)) && night.active(at(29, 1, 59)) && !night.active(at(29, 2, 0)) && !night.active(at(29, 22, 30)) && !night.active(at(30, 1, 0)));
    assert!(!plan("v", 1, &["bad"], Action::Frost, weekly(&["saturday"], "22:00", "02:00", Some("2026-11-29"), None)).active(at(29, 1, 0)));
    assert!(plan("v", 1, &["bad"], Action::Frost, weekly(&["saturday"], "22:00", "02:00", Some("2026-11-28"), None)).active(at(29, 1, 0)));
    assert!(!plan("w", 1, &["bad"], Action::Frost, weekly(&["saturday"], "22:00", "02:00", None, Some("2026-11-21"))).active(at(28, 23, 0)));
    assert!(plan("e", 1, &["bad"], Action::Frost, weekly(&["weekdays"], "06:00", "06:00", None, None)).active(at(27, 12, 0)));
    assert!(plan("e", 1, &["bad"], Action::Frost, weekly(&["weekend"], "10:00", "12:00", None, None)).active(at(29, 11, 0)));
}

#[test]
fn plan_replaces_the_base_and_beats_schedule_and_night() {
    let plans = [plan("a", 1, &["wohnzimmer", "bad"], Action::Setpoint { celsius: 23.0 }, once("2026-11-27T05:00", "2026-11-27T23:59"))];
    let now = wants(&plans, at(27, 7, 0), Some(&home()), &[]);
    assert_eq!((get(&now, "bad"), get(&now, "wohnzimmer")), ((23.0, "plan"), (23.0, "plan")));
    let outside = wants(&plans, at(27, 4, 0), Some(&home()), &[]);
    assert_eq!((get(&outside, "bad"), get(&outside, "wohnzimmer")), ((20.0, "default"), (20.0, "night")));
    let night = wants(&[plan("a", 1, &["wohnzimmer"], Action::Setpoint { celsius: 23.0 }, once("2026-11-27T22:00", "2026-11-28T03:00"))], at(28, 1, 0), Some(&home()), &[]);
    assert_eq!(get(&night, "wohnzimmer"), (23.0, "plan"));
}

#[test]
fn offset_adds_to_the_base_that_would_apply_and_is_clamped() {
    let plans = [plan("a", 1, &["bad", "wohnzimmer"], Action::Offset { k: -2.0 }, once("2026-11-27T00:00", "2026-11-28T00:00"))];
    let w = wants(&plans, at(27, 7, 0), Some(&home()), &[]);
    assert_eq!((get(&w, "bad"), get(&w, "wohnzimmer")), ((20.0, "plan"), (19.0, "plan")));
    let hot = [plan("a", 1, &["bad"], Action::Offset { k: 3.0 }, once("2026-11-27T00:00", "2026-11-28T00:00"))];
    assert_eq!(get(&wants(&hot, at(27, 7, 0), Some(&home()), &[]), "bad"), (25.0, "plan"));
}

#[test]
fn offset_applies_on_top_of_occupied_and_weather_comfort() {
    let config = Config::parse(&format!("{CONFIG}[comfort.weather]
enabled = true
")).unwrap();
    let plans = [plan("a", 1, &["bad", "wohnzimmer"], Action::Offset { k: 1.0 }, once("2026-11-27T00:00", "2026-11-28T00:00"))];
    let run = |held: &[&str], outdoor: Option<f64>| {
        let held: HashSet<String> = held.iter().map(|h| h.to_string()).collect();
        desired_planned(at(27, 12, 0), &config, Some(&home()), &held, &HashSet::new(), &HashMap::new(), outdoor, &plans).into_iter().map(|d| (d.room, d.setpoint, d.reason)).collect::<Vec<_>>()
    };
    assert_eq!(get(&run(&["wohnzimmer"], None), "wohnzimmer"), (22.0, "plan"));
    assert_eq!(get(&run(&[], Some(-5.0)), "wohnzimmer"), (23.0, "plan"));
    assert_eq!(get(&run(&["wohnzimmer"], Some(-5.0)), "wohnzimmer"), (23.0, "plan"));
    assert_eq!(get(&run(&[], None), "bad"), (21.0, "plan"));
}

#[test]
fn priority_window_frost_away_presence_then_setpoint() {
    let span = once("2026-11-27T00:00", "2026-11-28T00:00");
    let frost = plan("f", 1, &["bad"], Action::Frost, span.clone());
    let away_plan = plan("a", 2, &["bad"], Action::Away, span.clone());
    let value = plan("v", 3, &["bad"], Action::Setpoint { celsius: 24.0 }, span);
    let now = at(27, 12, 0);
    let all = [frost, away_plan.clone(), value.clone()];
    assert_eq!(get(&wants(&all, now, Some(&home()), &["bad"]), "bad"), (8.0, "window_open"));
    assert_eq!(get(&wants(&all, now, Some(&away()), &[]), "bad"), (8.0, "plan"));
    assert_eq!(get(&wants(std::slice::from_ref(&value), now, Some(&away()), &[]), "bad"), (17.0, "away"));
    assert_eq!(get(&wants(&[away_plan.clone(), value.clone()], now, Some(&home()), &[]), "bad"), (17.0, "plan"));
    assert_eq!(get(&wants(std::slice::from_ref(&value), now, Some(&home()), &[]), "bad"), (24.0, "plan"));
    let experiment = crate::explore::Experiment { id: "x".into(), room: "bad".into(), kind: crate::explore::Kind::Step, setpoint: 21.0, base: 20.0, start: now, end: now };
    let exploring: HashMap<String, crate::explore::Experiment> = [("bad".to_string(), experiment)].into();
    let explore = |plans: &[Plan]| get(&desired_planned(now, &config(), Some(&home()), &HashSet::new(), &HashSet::new(), &exploring, None, plans).into_iter().map(|d| (d.room, d.setpoint, d.reason)).collect::<Vec<_>>(), "bad");
    assert_eq!(explore(&[away_plan]), (17.0, "plan"));
    assert_eq!(explore(&[value]), (21.0, "exploration"));
}

#[test]
fn newest_plan_wins_within_a_class_and_no_plans_change_nothing() {
    let span = once("2026-11-27T00:00", "2026-11-28T00:00");
    let old = plan("old", 1, &["bad"], Action::Setpoint { celsius: 22.0 }, span.clone());
    let new = plan("new", 2, &["bad"], Action::Setpoint { celsius: 19.0 }, span);
    let d = desired_planned(at(27, 12, 0), &config(), Some(&home()), &HashSet::new(), &HashSet::new(), &HashMap::new(), None, &[old, new]);
    let bad = d.iter().find(|w| w.room == "bad").unwrap();
    assert_eq!((bad.setpoint, bad.plan.as_ref().map(|p| p.id.as_str())), (19.0, Some("new")));
    let base = crate::rules::desired(at(27, 12, 0), &config(), Some(&home()), &HashSet::new(), &HashSet::new(), &HashMap::new(), None);
    assert!(base.iter().all(|w| w.plan.is_none() && w.reason != "plan"));
}

#[test]
fn validation_covers_rooms_ranges_steps_and_spans() {
    let c = config();
    let now = at(27, 12, 0);
    let ok = new_plan(&["bad"], Action::Setpoint { celsius: 22.5 }, once("2026-11-28T15:00", "2026-11-28T18:00"));
    assert!(plan::validate(&c, &ok, now).is_ok());
    let bad = |p: NewPlan| plan::validate(&c, &p, now).unwrap_err();
    assert!(bad(new_plan(&["keller"], Action::Away, ok.when.clone())).contains("keller"));
    assert!(bad(new_plan(&[], Action::Away, ok.when.clone())).contains("Raum"));
    for celsius in [7.5, 26.5, 21.3, f64::NAN] {
        assert!(bad(new_plan(&["bad"], Action::Setpoint { celsius }, ok.when.clone())).contains("celsius"));
    }
    for k in [3.5, -4.0, 0.3] {
        assert!(bad(new_plan(&["bad"], Action::Offset { k }, ok.when.clone())).contains('k'));
    }
    assert!(bad(new_plan(&["bad"], Action::Away, once("2026-11-28T18:00", "2026-11-28T15:00"))).contains("nach from"));
    assert!(bad(new_plan(&["bad"], Action::Away, once("2026-11-28T00:00", "2027-02-01T00:00"))).contains("60"));
    assert!(bad(new_plan(&["bad"], Action::Away, once("2026-11-20T00:00", "2026-11-21T00:00"))).contains("Vergangenheit"));
    assert!(bad(new_plan(&["bad"], Action::Away, weekly(&["funday"], "10:00", "12:00", None, None))).contains("days"));
    assert!(bad(new_plan(&["bad"], Action::Away, weekly(&["weekend"], "25:00", "12:00", None, None))).contains("HH:MM"));
    assert!(bad(new_plan(&["bad"], Action::Away, weekly(&["weekend"], "10:00", "12:00", Some("2026-12-02"), Some("2026-12-01")))).contains("valid_until"));
    let mut long = ok.clone();
    long.label = "x".repeat(81);
    assert!(bad(long).contains("80"));
    let all = plan::validate(&c, &new_plan(&["bad", "all"], Action::Frost, ok.when.clone()), now).unwrap();
    assert_eq!(all.rooms, vec!["all".to_string()]);
}

#[test]
fn store_persists_atomically_prunes_expired_and_caps_the_count() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("data/plans.json");
    let c = config();
    let now = at(27, 12, 0);
    let plans = Plans::open(path.clone(), now);
    let made = plans.insert(&c, &new_plan(&["bad"], Action::Away, once("2026-11-27T13:00", "2026-11-27T14:00")), now, 1000).unwrap();
    let kept = plans.insert(&c, &new_plan(&["bad"], Action::Frost, weekly(&["monday"], "10:00", "12:00", None, None)), now, 1000).unwrap();
    assert_ne!(made.id, kept.id);
    assert!(!path.with_extension("json.tmp").exists());
    assert_eq!(Plans::open(path.clone(), now).list().len(), 2);
    assert_eq!(Plans::open(path.clone(), at(27, 15, 0)).list().iter().map(|p| p.id.clone()).collect::<Vec<_>>(), vec![kept.id.clone()]);
    assert!(plans.remove(&made.id).unwrap().is_some() && plans.remove(&made.id).unwrap().is_none());
    for i in 0..plan::MAX_PLANS - 1 {
        plans.insert(&c, &new_plan(&["bad"], Action::Frost, weekly(&["monday"], "10:00", "12:00", None, None)), now, 2000 + i as i64).unwrap();
    }
    assert!(plans.insert(&c, &new_plan(&["bad"], Action::Frost, weekly(&["monday"], "10:00", "12:00", None, None)), now, 9000).unwrap_err().contains("64"));
}

#[test]
fn preview_merges_fifteen_minute_steps_into_segments() {
    let c = config();
    let plans = [plan("a", 1, &["bad"], Action::Setpoint { celsius: 23.0 }, once("2026-11-27T15:00", "2026-11-27T16:30"))];
    let rows = |out: &serde_json::Value, room: &str| -> Vec<(String, String, f64, String)> { out["rooms"][room].as_array().unwrap().iter().map(|s| (s["from"].as_str().unwrap().into(), s["to"].as_str().unwrap().into(), s["celsius"].as_f64().unwrap(), s["reason"].as_str().unwrap().into())).collect() };
    let out = plan::preview(&c, &plans, at(27, 14, 50), 3);
    assert_eq!(rows(&out, "bad"), vec![
        ("2026-11-27T14:45:00".into(), "2026-11-27T15:00:00".into(), 20.0, "default".into()),
        ("2026-11-27T15:00:00".into(), "2026-11-27T16:30:00".into(), 23.0, "plan".into()),
        ("2026-11-27T16:30:00".into(), "2026-11-27T17:45:00".into(), 20.0, "default".into()),
    ]);
    assert!(out["rooms"].get("flur").is_none());
    let sched = plan::preview(&c, &[], at(27, 5, 0), 4);
    assert_eq!(rows(&sched, "bad").iter().map(|r| (r.0.clone(), r.2, r.3.clone())).collect::<Vec<_>>(), vec![("2026-11-27T05:00:00".into(), 20.0, "default".into()), ("2026-11-27T06:00:00".into(), 22.0, "schedule".into()), ("2026-11-27T08:00:00".into(), 20.0, "default".into())]);
    let high = [plan("h", 1, &["bad"], Action::Setpoint { celsius: 24.0 }, once("2026-11-27T15:00", "2026-11-27T16:00"))];
    assert!(rows(&plan::preview(&c, &high, at(27, 15, 0), 1), "bad").iter().all(|r| r.2 == 24.0));
    let capped = Config::parse(&format!("{CONFIG}[failsafe]
boost_max_c = 23.0
")).unwrap();
    assert!(rows(&plan::preview(&capped, &high, at(27, 15, 0), 1), "bad").iter().all(|r| r.2 == 23.0));
    let night = plan::preview(&c, &[], at(27, 22, 0), 3);
    assert_eq!(rows(&night, "wohnzimmer").iter().map(|r| (r.2, r.3.clone())).collect::<Vec<_>>(), vec![(21.0, "home".to_string()), (20.0, "night".to_string())]);
}

#[test]
fn scopes_cover_plan_and_assistant_routes() {
    for path in ["/api/plans", "/api/plans/preview", "/api/plans/p19c0", "/api/assistant", "/api/assistant/confirm"] {
        assert_eq!(Scope::of(path, false), Scope::Watch, "{path}");
    }
    assert_eq!(Scope::of("/api/window", false), Scope::Admin);
}

#[tokio::test]
async fn http_routes_create_list_preview_delete_and_reject() {
    let e = env_with("");
    let state = &e.state;
    let day = chrono::Local::now().date_naive() + chrono::Duration::days(1);
    let (from, to) = (format!("{day}T15:00"), format!("{day}T18:00"));
    let body = json!({"rooms": ["bad"], "action": {"type": "setpoint", "celsius": 22.0}, "when": {"type": "once", "from": from, "to": to}, "label": "Bad warm"});
    let (status, created) = call(state, "POST", "/api/plans", Some(body)).await;
    assert_eq!(status, StatusCode::OK, "{created}");
    let id = created["plan"]["id"].as_str().unwrap().to_string();
    assert!(created["summary"].as_str().unwrap().contains("22 °C"));
    assert_eq!(call(state, "GET", "/api/plans", None).await.1["plans"].as_array().unwrap().len(), 1);
    let (status, rejected) = call(state, "POST", "/api/plans", Some(json!({"rooms": ["keller"], "action": {"type": "away"}, "when": {"type": "once", "from": from, "to": to}}))).await;
    assert_eq!((status, rejected["error"].as_str().unwrap().contains("keller")), (StatusCode::BAD_REQUEST, true));
    assert_eq!(call(state, "POST", "/api/plans", Some(json!({"nope": 1}))).await.0, StatusCode::BAD_REQUEST);
    let (status, view) = call(state, "POST", "/api/plans/preview", Some(json!({"hours": 48, "plans": [{"rooms": ["wohnzimmer"], "action": {"type": "frost"}, "when": {"type": "weekly", "days": ["weekdays", "weekend"], "from": "00:00", "to": "00:00"}}]}))).await;
    assert_eq!(status, StatusCode::OK);
    assert!(view["rooms"]["bad"].as_array().unwrap().iter().all(|s| s["celsius"].is_number()), "{view}");
    assert!(view["rooms"]["wohnzimmer"].as_array().unwrap().iter().all(|s| s["celsius"] == 8.0));
    assert_eq!(call(state, "POST", "/api/plans/preview", Some(json!({"hours": 400}))).await.0, StatusCode::BAD_REQUEST);
    assert_eq!(call(state, "DELETE", &format!("/api/plans/{id}"), None).await.0, StatusCode::OK);
    assert_eq!(call(state, "DELETE", &format!("/api/plans/{id}"), None).await.0, StatusCode::NOT_FOUND);
    state.sink.flush().unwrap();
    assert!(crate::capture::tests::lake_rows(e.dir.path(), "events") >= 2);
}

#[tokio::test]
async fn active_plan_drives_the_device_and_shows_up_in_state_and_metrics() {
    let mut e = env_with("");
    let now = chrono::Local::now().naive_local();
    let fmt = |t: NaiveDateTime| t.format("%Y-%m-%dT%H:%M").to_string();
    let (from, to) = (fmt(now - chrono::Duration::hours(1)), fmt(now + chrono::Duration::hours(2)));
    let (status, _) = call(&e.state, "POST", "/api/plans", Some(json!({"rooms": ["bad"], "action": {"type": "setpoint", "celsius": 24.0}, "when": {"type": "once", "from": from, "to": to}, "label": "Testplan"}))).await;
    assert_eq!(status, StatusCode::OK);
    e.t += 60_000;
    let wants = crate::rules::control_step(&e.state, now, e.t).await.unwrap();
    let bad = wants.iter().find(|w| w.room == "bad").unwrap();
    assert_eq!((bad.setpoint, bad.reason, bad.plan.as_ref().map(|p| p.label.as_str())), (24.0, "plan", Some("Testplan")));
    assert_eq!(e.device(6).await, 24.0);
    assert_eq!(e.device(5).await, 20.0);
    let (_, state) = call(&e.state, "GET", "/api/state", None).await;
    assert_eq!(state["rooms"][1]["plans"][0]["label"], "Testplan");
    assert!(plan::metrics(&e.state, now).contains("heatd_plan_active{room=\"bad\"} 1"));
    let _ = call(&e.state, "POST", "/api/setpoint", Some(json!({"room": "bad", "celsius": 19.0}))).await;
    e.t += 60_000;
    crate::rules::control_step(&e.state, now, e.t).await.unwrap();
    assert_eq!(e.device(6).await, 19.0);
}
