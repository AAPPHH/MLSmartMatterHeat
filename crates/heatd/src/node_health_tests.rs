use crate::config::StaleSection;
use crate::node_health::*;
use crate::status::{NodeStatus, SubState};
use serde_json::{json, Value};
use crate::capture::tests::{state_with, string_column};
use crate::http::AppState;
use matter_controller::{AttributeReport, MatterController, MockController};
use std::sync::Arc;

const MIN: i64 = 60_000_000;
const CONFIG: &str = "[node_health]
silent_min = 1
unreachable_min = 3
min_gap_s = 10
backoff_start_s = 20
backoff_max_min = 1
reset_gap_min = 5
verify_min = 2
alert_retries = 2
";
const ROOMS: &str = "[[rooms]]
name = 'b'
nodes = [6]
";
const FAST: &str = "unreachable_backoff_max_min = 1
restart_grace_s = 0
";

struct Rig {
    state: AppState,
    mock: Arc<MockController>,
    dir: tempfile::TempDir,
    t0: i64,
}

fn rig() -> Rig {
    rig_with(FAST)
}

fn rig_with(extra: &str) -> Rig {
    let dir = tempfile::tempdir().unwrap();
    let mock = Arc::new(MockController::new(1));
    mock.add_node(5, 19.0, 19.0);
    mock.add_node(6, 19.0, 19.0);
    let state = state_with(dir.path(), mock.clone(), &format!("{CONFIG}{extra}
{ROOMS}"));
    state.status.register_nodes([5, 6]);
    Rig { t0: state.status.started_us(), state, mock, dir }
}

impl Rig {
    fn health(&self, node: u64) -> Health {
        self.state.status.node_snapshot(node).unwrap().health.state
    }

    fn report(&self, node: u64, at_us: i64) {
        self.state.status.node_report(node, at_us);
        self.state.status.subscription_report(node, at_us);
    }

    fn device(&self, node: u64, value: f64, at_us: i64) {
        crate::rules::on_report(&self.state, &AttributeReport { node, endpoint: 1, attribute: "occupied_heating_setpoint".into(), value }, true, at_us / 1000);
    }

    fn push(&self, node: u64, value: f64, at_us: i64) {
        crate::rules::on_report(&self.state, &AttributeReport { node, endpoint: 1, attribute: "occupied_heating_setpoint".into(), value }, false, at_us / 1000);
    }

    fn events(&self) -> Vec<Value> {
        self.state.sink.flush().unwrap();
        string_column(self.dir.path(), "events", "kind").into_iter().zip(string_column(self.dir.path(), "events", "detail")).filter(|(k, _)| k == "node_health").map(|(_, d)| serde_json::from_str(&d).unwrap()).collect()
    }

    fn links(&self) -> Vec<String> {
        self.events().iter().filter(|e| e["aspect"] == "link").map(|e| format!("{}>{}", e["from"].as_str().unwrap(), e["to"].as_str().unwrap())).collect()
    }
}

#[test]
fn classification_follows_the_silence_and_subscription_thresholds() {
    let cfg = NodeHealthSection::default();
    let mut n = NodeStatus { last_report_us: Some(100 * MIN), last_subscription_us: Some(100 * MIN), ..Default::default() };
    let at = |n: &NodeStatus, minutes: i64| classify(&cfg, n, 0, (100 + minutes) * MIN);
    assert_eq!([0, 9, 10, 29, 30, 100].map(|m| at(&n, m)), [Health::Ok, Health::Ok, Health::Silent, Health::Silent, Health::Unreachable, Health::Unreachable]);
    n.state = SubState::Lost;
    assert_eq!((at(&n, 1), at(&n, 11)), (Health::Degraded, Health::Silent));
    n.state = SubState::Subscribed;
    n.last_report_us = Some(159 * MIN);
    n.last_subscription_us = Some(100 * MIN);
    assert_eq!((at(&n, 59), at(&n, 61)), (Health::Ok, Health::Ok));
    assert_eq!(classify(&cfg, &NodeStatus::default(), 0, 11 * MIN), Health::Silent);
}

#[test]
fn backoff_doubles_between_attempts_and_is_capped() {
    let cfg = NodeHealthSection::default();
    let stale = StaleSection::default();
    let secs: Vec<i64> = (0..7).map(|a| gap_us(&cfg, &stale, Health::Silent, a) / 1_000_000).collect();
    assert_eq!(secs, vec![60, 120, 240, 480, 600, 600, 600]);
    assert_eq!((gap_us(&cfg, &stale, Health::Degraded, 0) / 1_000_000, gap_us(&cfg, &stale, Health::Degraded, 3) / 1_000_000), (600, 900));
}

#[tokio::test]
async fn a_silent_node_walks_the_ladder_with_backoff_restarts_the_session_once_and_recovers() {
    let r = rig();
    let s = 1_000_000;
    r.report(5, r.t0);
    r.report(6, r.t0);
    r.mock.set_node_silent(5, true);
    step(&r.state, r.t0 + 30 * s).await;
    assert_eq!((r.health(5), r.mock.resubscribes().len()), (Health::Ok, 0));
    r.report(6, r.t0 + 60 * s);
    step(&r.state, r.t0 + 61 * s).await;
    assert_eq!((r.health(5), r.health(6)), (Health::Silent, Health::Ok));
    assert!(r.mock.resubscribes().is_empty());
    let mut attempts = Vec::new();
    for t in (62..400).step_by(2) {
        r.report(6, r.t0 + t * s);
        let before = r.mock.resubscribes().len() + r.mock.restarts();
        step(&r.state, r.t0 + t * s).await;
        if r.mock.resubscribes().len() + r.mock.restarts() > before {
            attempts.push(t);
        }
    }
    assert_eq!(r.mock.resubscribes(), vec![5; r.mock.resubscribes().len()]);
    assert_eq!(r.mock.restarts(), 1);
    let gaps: Vec<i64> = attempts.windows(2).map(|w| w[1] - w[0]).collect();
    assert!(gaps.windows(2).all(|g| g[1] >= g[0]), "gaps must not shrink: {gaps:?}");
    assert!(gaps.iter().all(|g| *g >= 10) && attempts[0] >= 71 && gaps.iter().all(|g| *g <= 62), "{attempts:?} {gaps:?}");
    assert_eq!(r.health(5), Health::Unreachable);
    let view = r.state.status.node_health_view(5, r.t0 + 400 * s).unwrap();
    assert_eq!(view["state"], "unreachable");
    assert!(view["attempts"].as_u64().unwrap() >= 4 && view["last_action"].is_string() && view["last_attempt_age_s"].is_number());
    let alerts = alerts(&r.state);
    assert_eq!(alerts.len(), 1);
    assert!(alerts[0]["id"].as_str().unwrap().starts_with("node_unreachable:5@") && alerts[0]["text"].as_str().unwrap().contains("Node 5"));
    r.mock.set_node_silent(5, false);
    r.report(5, r.t0 + 402 * s);
    step(&r.state, r.t0 + 403 * s).await;
    assert_eq!(r.health(5), Health::Ok);
    assert_eq!(r.state.status.node_snapshot(5).unwrap().health.attempts, 0);
    assert!(alerts_empty(&r.state));
    assert_eq!(r.links(), vec!["ok>silent", "silent>unreachable", "unreachable>ok"]);
    let last = r.events().into_iter().rfind(|e| e["to"] == "ok").unwrap();
    assert!(last["episode_s"].as_i64().unwrap() >= 300 && last["attempts"].as_u64().unwrap() >= 4);
}

fn alerts_empty(state: &AppState) -> bool {
    alerts(state).is_empty()
}

#[tokio::test]
async fn a_node_silent_from_the_start_is_detected_after_silent_min_of_uptime() {
    let r = rig();
    let s = 1_000_000;
    r.report(6, r.t0);
    step(&r.state, r.t0 + 50 * s).await;
    assert_eq!(r.health(5), Health::Ok);
    r.report(6, r.t0 + 61 * s);
    step(&r.state, r.t0 + 62 * s).await;
    assert_eq!(r.health(5), Health::Silent);
    let first = &r.events()[0];
    assert_eq!((first["from"].as_str(), first["to"].as_str()), (Some("ok"), Some("silent")));
    assert!(first["silent_s"].as_i64().unwrap() >= 60);
    r.report(6, r.t0 + 120 * s);
    r.report(5, r.t0 + 121 * s);
    step(&r.state, r.t0 + 122 * s).await;
    assert_eq!(r.links(), vec!["ok>silent", "silent>ok"]);
}

#[tokio::test]
async fn a_lost_subscription_with_answering_polls_is_degraded_and_resubscribed_once_per_gap() {
    let r = rig();
    let s = 1_000_000;
    r.report(5, r.t0);
    r.state.status.subscription(5, SubState::Lost, r.t0);
    for t in [1, 20, 40] {
        r.state.status.node_report(5, r.t0 + t * s);
        step(&r.state, r.t0 + t * s).await;
    }
    assert_eq!(r.health(5), Health::Degraded);
    assert_eq!(r.mock.resubscribes(), vec![5]);
    assert!(r.mock.restarts() == 0);
    r.state.status.subscription(5, SubState::Subscribed, r.t0 + 50 * s);
    r.report(5, r.t0 + 50 * s);
    step(&r.state, r.t0 + 51 * s).await;
    assert_eq!(r.links(), vec!["ok>degraded", "degraded>ok"]);
}

#[tokio::test]
async fn a_setpoint_the_device_does_not_report_is_written_again_with_backoff_and_counted() {
    let r = rig();
    let s = 1_000_000;
    let ms = r.t0 / 1000;
    r.state.commanded.lock().unwrap().test_sent(5, 21.0, 19.0, ms);
    r.device(5, 19.0, r.t0 + s);
    for t in [1, 30, 100] {
        r.report(5, r.t0 + t * s);
        r.report(6, r.t0 + t * s);
        step(&r.state, r.t0 + t * s).await;
    }
    assert!(r.mock.setpoint_writes().is_empty());
    let view = r.state.status.node_health_view(5, r.t0 + 100 * s).unwrap();
    assert_eq!(view["setpoint"]["mismatch"], true);
    r.report(5, r.t0 + 125 * s);
    step(&r.state, r.t0 + 125 * s).await;
    assert_eq!(r.mock.setpoint_writes(), vec![(5, 21.0)]);
    r.device(5, 21.0, r.t0 + 126 * s);
    r.report(5, r.t0 + 130 * s);
    r.report(6, r.t0 + 130 * s);
    step(&r.state, r.t0 + 130 * s).await;
    let view = r.state.status.node_health_view(5, r.t0 + 130 * s).unwrap();
    assert_eq!((view["setpoint"]["mismatch"].clone(), view["setpoint"]["rewrites_total"].clone()), (false.into(), 1.into()));
    let rewrites: Vec<Value> = r.events().into_iter().filter(|e| e["action"] == "rewrite").collect();
    assert_eq!((rewrites.len(), rewrites[0]["target"].clone(), rewrites[0]["device"].clone()), (1, 21.0.into(), 19.0.into()));
    assert!(alerts_empty(&r.state));
}

#[tokio::test]
async fn a_setpoint_that_never_sticks_raises_an_alert_after_the_retries_and_clears_when_it_does() {
    let r = rig();
    let s = 1_000_000;
    r.state.commanded.lock().unwrap().test_sent(5, 21.0, 19.0, r.t0 / 1000);
    r.mock.set_writes_dropped(5, true);
    for t in (1..700).step_by(5) {
        r.report(5, r.t0 + t * s);
        r.report(6, r.t0 + t * s);
        r.device(5, 19.0, r.t0 + t * s);
        step(&r.state, r.t0 + t * s).await;
    }
    let writes = r.mock.setpoint_writes().len();
    assert_eq!(writes, 2);
    let alerts = alerts(&r.state);
    assert_eq!(alerts.len(), 1);
    let text = alerts[0]["text"].as_str().unwrap();
    assert!(alerts[0]["id"].as_str().unwrap().starts_with("setpoint_not_applied:5@") && text.contains("21.0") && text.contains("19.0"), "{text}");
    assert!(r.events().iter().any(|e| e["aspect"] == "setpoint" && e["alert"] == true));
    assert_eq!(r.mock.setpoint_writes().len(), writes);
    r.mock.set_writes_dropped(5, false);
    for t in (700..1000).step_by(5) {
        r.report(5, r.t0 + t * s);
        r.report(6, r.t0 + t * s);
        r.device(5, 21.0, r.t0 + t * s);
        step(&r.state, r.t0 + t * s).await;
    }
    assert!(alerts_empty(&r.state));
    assert_eq!(r.mock.setpoint_writes().len(), writes);
    assert!(r.events().iter().any(|e| e["aspect"] == "setpoint" && e["alert"] == false));
}

#[tokio::test]
async fn consecutive_failed_writes_alert_and_a_successful_write_clears_it() {
    let r = rig();
    let s = 1_000_000;
    r.report(5, r.t0);
    r.state.status.setpoint_write(5, Some("no write status after 90s".into()));
    step(&r.state, r.t0 + s).await;
    assert!(alerts_empty(&r.state));
    r.state.status.setpoint_write(5, Some("no write status after 90s".into()));
    step(&r.state, r.t0 + 2 * s).await;
    assert_eq!(alerts(&r.state).len(), 1);
    let view = r.state.status.node_health_view(5, r.t0 + 2 * s).unwrap();
    assert_eq!((view["setpoint"]["failures_total"].clone(), view["setpoint"]["last_error"].clone()), (2.into(), "no write status after 90s".into()));
    r.state.status.setpoint_write(5, None);
    step(&r.state, r.t0 + 3 * s).await;
    assert!(alerts_empty(&r.state));
}

#[test]
fn the_report_section_summarises_transitions_per_node() {
    let link = |from: &str, to: &str, episode: i64| json!({"aspect": "link", "from": from, "to": to, "silent_s": 700, "episode_s": episode, "attempts": 3}).to_string();
    let rewrite = json!({"aspect": "setpoint", "action": "rewrite", "target": 21.0, "device": 19.0, "rewrites": 1}).to_string();
    let events = vec![(1_000_000, Some("6".to_string()), Some(link("ok", "silent", 0))), (2_000_000, Some("6".to_string()), Some(link("silent", "ok", 1500))), (3_000_000, Some("5".to_string()), Some(rewrite))];
    let text = report_section(&events);
    assert!(text.contains("| 6 | 2 | silent | 25 min | 0 | 0 |"), "{text}");
    assert!(text.contains("| 5 | 0 | ok |") && text.contains("Node 6: ok -> silent") && text.contains("Sollwert 21.0 nicht uebernommen"), "{text}");
    assert!(report_section(&[]).starts_with("Keine Stoerungen"));
}

#[tokio::test]
async fn an_unreachable_node_is_restarted_once_per_episode_then_only_resubscribed_with_growing_gaps() {
    let r = rig_with("");
    let s = 1_000_000;
    r.report(5, r.t0);
    r.mock.set_node_silent(5, true);
    let mut attempts = Vec::new();
    for t in (1..6 * 3600).step_by(10) {
        r.report(6, r.t0 + t * s);
        let before = r.mock.resubscribes().len();
        step(&r.state, r.t0 + t * s).await;
        if r.mock.resubscribes().len() > before {
            attempts.push(t);
        }
    }
    assert_eq!(r.mock.restarts(), 1);
    assert_eq!(r.mock.resubscribes(), vec![5; r.mock.resubscribes().len()]);
    let gaps: Vec<i64> = attempts.windows(2).map(|w| w[1] - w[0]).collect();
    assert!(attempts.len() <= 16 && *gaps.last().unwrap() >= 3600, "{attempts:?}");
    r.mock.set_node_silent(5, false);
    r.report(5, r.t0 + 6 * 3600 * s);
    step(&r.state, r.t0 + 6 * 3600 * s + s).await;
    assert_eq!(r.health(5), Health::Ok);
    r.mock.set_node_silent(5, true);
    for t in (1..600).step_by(10) {
        let at = r.t0 + (6 * 3600 + t) * s;
        r.report(6, at);
        step(&r.state, at).await;
    }
    assert_eq!(r.mock.restarts(), 2);
}

#[tokio::test]
async fn a_new_episode_waits_the_min_gap_before_its_first_resubscribe() {
    let r = rig();
    let s = 1_000_000;
    r.report(5, r.t0);
    r.report(6, r.t0);
    for t in (1..130).step_by(1) {
        r.report(6, r.t0 + t * s);
        step(&r.state, r.t0 + t * s).await;
    }
    assert_eq!(r.health(5), Health::Silent);
    let first = r.mock.resubscribes().len();
    assert!(first >= 1);
    r.report(5, r.t0 + 130 * s);
    step(&r.state, r.t0 + 131 * s).await;
    assert_eq!(r.health(5), Health::Ok);
    let mut entered = None;
    for t in 132..400 {
        r.report(6, r.t0 + t * s);
        step(&r.state, r.t0 + t * s).await;
        if entered.is_none() && r.health(5) == Health::Silent {
            entered = Some(t);
        }
        if let Some(e) = entered {
            let late = r.mock.resubscribes().len() - first;
            assert!(t - e >= 10 || late == 0, "resubscribed {} s after entering silent", t - e);
        }
    }
    assert!(entered.is_some() && r.mock.resubscribes().len() > first);
}

#[tokio::test]
async fn nodes_wait_out_the_grace_after_a_session_restart() {
    let r = rig_with("restart_grace_s = 120");
    let s = 1_000_000;
    r.report(5, r.t0);
    r.report(6, r.t0);
    r.mock.set_node_silent(5, true);
    let mut restart_at = None;
    for t in 1..300 {
        r.report(6, r.t0 + t * s);
        step(&r.state, r.t0 + t * s).await;
        if restart_at.is_none() && r.mock.restarts() == 1 {
            restart_at = Some(t);
            r.state.status.subscription(6, SubState::Lost, r.t0 + t * s);
        }
        if let Some(at) = restart_at.filter(|at| t > *at && t < at + 120) {
            assert!(!r.mock.resubscribes().contains(&6) && r.mock.restarts() == 1, "ladder ran {} s after the restart", t - at);
        }
    }
    assert!(restart_at.is_some());
}

#[tokio::test]
async fn a_device_within_the_control_deadband_or_at_its_acknowledged_value_is_not_rewritten() {
    let r = rig();
    let s = 1_000_000;
    r.state.commanded.lock().unwrap().test_sent(5, 20.0, 18.0, r.t0 / 1000);
    r.device(5, 19.8, r.t0 + s);
    r.state.commanded.lock().unwrap().test_sent(6, 21.0, 19.0, r.t0 / 1000);
    r.device(6, 20.0, r.t0 + s);
    for t in (1..400).step_by(5) {
        r.report(5, r.t0 + t * s);
        r.report(6, r.t0 + t * s);
        step(&r.state, r.t0 + t * s).await;
    }
    assert!(r.mock.setpoint_writes().is_empty(), "{:?}", r.mock.setpoint_writes());
    assert!(alerts_empty(&r.state));
}

#[tokio::test]
async fn a_late_priming_report_with_the_old_value_after_a_rewrite_is_no_manual_change() {
    let r = rig();
    let s = 1_000_000;
    r.state.commanded.lock().unwrap().test_sent(5, 21.0, 19.0, r.t0 / 1000);
    r.device(5, 19.0, r.t0 + s);
    r.mock.set_writes_dropped(5, true);
    for t in (1..140).step_by(5) {
        r.report(5, r.t0 + t * s);
        r.report(6, r.t0 + t * s);
        step(&r.state, r.t0 + t * s).await;
    }
    assert_eq!(r.mock.setpoint_writes().len(), 1);
    r.push(5, 19.0, r.t0 + 600 * s);
    assert!(r.state.commanded.lock().unwrap().hold(5).is_none());
    r.push(5, 17.0, r.t0 + 700 * s);
    assert!(r.state.commanded.lock().unwrap().hold(5).is_some());
}

#[test]
fn a_rewrite_claim_fails_when_a_newer_control_write_replaced_the_target() {
    let mut commands = crate::rules::Commands::default();
    commands.test_sent(5, 21.0, 19.0, 1000);
    commands.test_sent(5, 22.0, 19.0, 2000);
    assert!(commands.claim_rewrite(5, 21.0, 1000, 19.0, 3000).is_none());
    let prior = commands.claim_rewrite(5, 22.0, 2000, 19.0, 3000).unwrap();
    commands.test_sent(5, 23.0, 19.0, 4000);
    commands.release_rewrite(5, 3000, prior);
    assert_eq!(commands.target(5), Some(23.0));
}

#[tokio::test]
async fn the_setpoint_alert_survives_silence_and_failed_writes_clear_when_the_device_matches() {
    let r = rig();
    let s = 1_000_000;
    r.state.commanded.lock().unwrap().test_sent(5, 21.0, 19.0, r.t0 / 1000);
    r.mock.set_writes_dropped(5, true);
    for t in (1..700).step_by(5) {
        r.report(5, r.t0 + t * s);
        r.report(6, r.t0 + t * s);
        r.device(5, 19.0, r.t0 + t * s);
        step(&r.state, r.t0 + t * s).await;
    }
    assert!(alerts(&r.state).iter().any(|a| a["id"].as_str().unwrap().starts_with("setpoint_not_applied:5@")));
    for t in (700..1200).step_by(5) {
        r.report(6, r.t0 + t * s);
        step(&r.state, r.t0 + t * s).await;
    }
    assert_eq!(r.health(5), Health::Unreachable);
    let ids: Vec<String> = alerts(&r.state).iter().map(|a| a["id"].as_str().unwrap().split(':').next().unwrap().to_string()).collect();
    assert_eq!(ids.len(), 2);
    assert!(ids.contains(&"setpoint_not_applied".to_string()) && ids.contains(&"node_unreachable".to_string()));
    r.state.commanded.lock().unwrap().test_sent(6, 20.0, 19.0, (r.t0 + 1300 * s) / 1000);
    r.state.status.setpoint_write(6, Some("no write status".into()));
    r.device(6, 20.0, r.t0 + 1301 * s);
    r.report(6, r.t0 + 1301 * s);
    step(&r.state, r.t0 + 1302 * s).await;
    assert_eq!(r.state.status.node_snapshot(6).unwrap().health.set.consecutive_failures, 0);
}

#[test]
fn the_first_alert_is_the_oldest_active_one_so_a_new_alert_does_not_displace_it() {
    let r = rig();
    let set = |node: u64, since_ms: i64| {
        r.state.status.update_health(node, |rec| {
            rec.state = Health::Unreachable;
            rec.since_us = since_ms * 1000;
        })
    };
    set(6, 1000);
    let first = alerts(&r.state)[0]["id"].clone();
    assert!(first.as_str().unwrap().starts_with("node_unreachable:6@1000"));
    set(5, 2000);
    let both = alerts(&r.state);
    assert_eq!((both.len(), both[0]["id"].clone()), (2, first.clone()));
    r.state.status.update_health(5, |rec| rec.state = Health::Ok);
    assert_eq!(alerts(&r.state)[0]["id"], first);
}
