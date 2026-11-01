mod common;
use common::*;
use matter_client::client::*;
use std::path::PathBuf;
use std::process::{Child, Command, Stdio};
use std::sync::atomic::{AtomicU16, Ordering};
use std::time::Duration;

static PORT: AtomicU16 = AtomicU16::new(47600);

pub fn test_dir() -> PathBuf {
    PathBuf::from(concat!(env!("CARGO_MANIFEST_DIR"), "/../test")).canonicalize().unwrap()
}

pub fn spawn_responder(port: u16, scenario: &str, result: &std::path::Path, extra: &[(&str, String)]) -> Child {
    let dir = test_dir();
    let mut cmd = Command::new(std::env::var("PYTHON").unwrap_or_else(|_| "python".into()));
    cmd.current_dir(&dir)
        .args(["-I", "ref_responder.py"])
        .env("RESP_PORT", port.to_string())
        .env("RESP_SCENARIO", scenario)
        .env("RESP_RESULT", result)
        .env("RESP_VECTORS", dir.join("vectors.h"))
        .env("RESP_STRICT", "1")
        .stdout(Stdio::null());
    for (k, v) in extra {
        cmd.env(k, v);
    }
    let c = cmd.spawn().unwrap();
    std::thread::sleep(Duration::from_millis(1200));
    c
}

struct Case {
    scenario: &'static str,
    expect: &'static str,
    peer: u64,
    bad_ipk: bool,
    bad_key: bool,
}

fn run_case(c: Case) {
    let port = PORT.fetch_add(1, Ordering::SeqCst);
    let result = std::env::temp_dir().join(format!("rs_lb_result_{}", port));
    let _ = std::fs::remove_file(&result);
    let mut child = spawn_responder(port, c.scenario, &result, &[]);
    let mut f = Fixture::new();
    if c.bad_ipk {
        f.ipk[0] ^= 1;
    }
    if c.bad_key {
        f.key[70] ^= 1;
    }
    let cr = f.creds();
    let mut net = Net::new(port, lcg_rng(port as u64));
    let mut s = Scratch::new();
    let mut e = ErrBuf::new();
    let mrp = Mrp { idle_ms: 300, active_ms: 200, max_ms: 2000, max_tx: 5, resp_ms: 3000 };
    let rc = write_int16(&mut net, &mut s, &cr, c.peer, &mrp, 1, 0x201, 0x12, 1800, &mut e);
    let err = String::from_utf8_lossy(e.as_bytes()).to_string();
    println!("{} rc={} err={} sent={}", c.scenario, rc, err, net.sent);
    if c.expect == "ok" {
        assert_eq!(rc, OK, "{} err={}", c.scenario, err);
        child.wait().unwrap();
        assert_eq!(std::fs::read_to_string(&result).unwrap(), "1/0x201/0x12=1800", "{}", c.scenario);
    } else {
        let _ = child.kill();
        let _ = child.wait();
        assert!(rc != OK && err.contains(c.expect), "{} expected {} got rc={} err={}", c.scenario, c.expect, rc, err);
    }
}

const P2: u64 = 0xDEDEDEDE00010002;

fn case(scenario: &'static str, expect: &'static str) -> Case {
    Case { scenario, expect, peer: P2, bad_ipk: false, bad_key: false }
}

#[test]
fn normal() {
    run_case(case("ok", "ok"));
}

#[test]
fn dropped_sigma1() {
    run_case(case("drop_first", "ok"));
}

#[test]
fn delayed_sigma2() {
    run_case(case("late", "ok"));
}

#[test]
fn duplicated_datagrams() {
    run_case(case("dup", "ok"));
}

#[test]
fn wrong_node_id() {
    run_case(Case { peer: 0xDEDEDEDE00010003, ..case("bad_node", "case_nosharedroot") });
}

#[test]
fn wrong_ipk() {
    run_case(Case { bad_ipk: true, ..case("ok", "case_nosharedroot") });
}

#[test]
fn busy() {
    run_case(case("busy", "case_busy"));
}

#[test]
fn bad_signature() {
    run_case(case("bad_sig", "sig_sigma2"));
}

#[test]
fn write_status_0x87() {
    run_case(case("write_fail", "write_status_87"));
}

#[test]
fn corrupted_key() {
    run_case(Case { bad_key: true, ..case("ok", "key_mismatch") });
}

#[test]
fn silent_peer() {
    run_case(case("silent", "timeout_sigma1"));
}

#[test]
fn sigma1_answered_with_success_status_is_failure() {
    run_case(case("status_ok", "proto_sigma2"));
}

#[test]
fn peer_icac_chain() {
    run_case(Case { peer: 0xDEDEDEDE00010001, ..case("icac", "ok") });
}
