use crate::config::Config;
use crate::phone::*;
use crate::rules::{Tracker, Transition};
use crate::storage::{Motion, Power};

const MIN: i64 = 60_000;
const GRACE: i64 = 20 * MIN;
const ARP: &str = "IP address       HW type     Flags       HW address            Mask     Device
192.0.2.1    0x1         0x2         aa:bb:cc:dd:ee:01     *        eth0
192.0.2.42   0x1         0x2         AA:BB:CC:DD:EE:FF     *        eth0
192.0.2.50   0x1         0x0         aa:bb:cc:dd:ee:50     *        eth0
";

fn mac() -> [u8; 6] {
    parse_mac("aa:bb:cc:dd:ee:ff").unwrap()
}

#[test]
fn arp_parsing_needs_complete_flag_and_matching_mac() {
    assert_eq!(find_phone(ARP, &mac()), Some("192.0.2.42".parse().unwrap()));
    assert_eq!(find_phone(ARP, &parse_mac("aa:bb:cc:dd:ee:50").unwrap()), None);
    assert_eq!(find_phone(ARP, &parse_mac("aa:bb:cc:dd:ee:99").unwrap()), None);
    assert_eq!(find_phone("IP address HW type Flags HW address Mask Device\n", &mac()), None);
    assert!(parse_mac("aa:bb:cc").is_err() && parse_mac("zz:bb:cc:dd:ee:ff").is_err());
}

#[test]
fn subnet_hosts_are_the_usable_addresses() {
    let net = Subnet::parse("192.0.2.77/24").unwrap();
    let hosts: Vec<_> = net.hosts().collect();
    assert_eq!((hosts.len(), hosts[0].to_string(), hosts[253].to_string()), (254, "192.0.2.1".into(), "192.0.2.254".into()));
    assert!(Subnet::parse("10.0.0.0/8").is_err() && Subnet::parse("10.0.0.1").is_err());
}

fn away_watch(t: &mut Tracker, from: i64) {
    t.observe("w", from, true, Some(Motion::Still), Some(Power::Doze));
    t.observe("w", from + MIN, true, Some(Motion::Moving), None);
}

#[test]
fn phone_arrival_ends_away_immediately() {
    let mut t = Tracker::default();
    away_watch(&mut t, 0);
    assert_eq!(t.poll(30 * MIN, 20.0, None).len(), 1);
    assert!(t.away("w", true));
    let up = t.phone_probe(40 * MIN, Some(42), GRACE);
    assert_eq!((up.arrived, up.changed), (Some(Transition::Arrived), Some(true)));
    assert!(!t.confirmed("w") && !t.away("w", true) && t.confirmed_since().is_none());
    assert!(t.phone_probe(41 * MIN, Some(42), GRACE).arrived.is_none());
}

#[test]
fn phone_present_blocks_watch_leave_and_silence() {
    let mut t = Tracker::default();
    t.phone_probe(0, Some(42), GRACE);
    away_watch(&mut t, MIN);
    assert!(t.poll(100 * MIN, 20.0, None).is_empty() && !t.confirmed("w") && !t.away("w", true));
    let mut s = Tracker::default();
    for k in 0..4 {
        s.observe("w", k * 30 * MIN, false, Some(Motion::Still), None);
        s.note_heartbeat("w", k * 30 * MIN, k * 30 * MIN, false);
    }
    s.phone_probe(100 * MIN, Some(42), GRACE);
    assert!(s.poll(1000 * MIN, 20.0, Some(75.0)).is_empty() && !s.confirmed("w"));
}

#[test]
fn absence_alone_never_gives_away() {
    let mut t = Tracker::default();
    t.phone_probe(0, Some(42), GRACE);
    for k in 1..200 {
        let up = t.phone_probe(k * MIN, None, GRACE);
        assert!(up.arrived.is_none());
    }
    assert!(t.poll(300 * MIN, 20.0, Some(75.0)).is_empty());
    assert!(!t.phone().vouch && !t.away("phone", true) && t.confirmed_since().is_none());
}

#[test]
fn grace_expiry_reports_change_and_lets_watch_leave_act() {
    let mut t = Tracker::default();
    t.phone_probe(0, Some(42), GRACE);
    away_watch(&mut t, MIN);
    assert!(t.phone_probe(19 * MIN, None, GRACE).changed.is_none() && t.phone().vouch);
    assert_eq!(t.phone_probe(21 * MIN, None, GRACE).changed, Some(false));
    assert_eq!(t.poll(30 * MIN, 20.0, None).len(), 1);
    assert!(t.away("w", true));
}

#[test]
fn single_leave_during_vouch_then_silence_confirms_after_grace() {
    let mut t = Tracker::default();
    for k in 0..4 {
        t.observe("w", k * 30 * MIN, false, Some(Motion::Still), None);
        t.note_heartbeat("w", k * 30 * MIN, k * 30 * MIN, false);
    }
    t.phone_probe(90 * MIN, Some(42), GRACE);
    t.observe("w", 95 * MIN, true, Some(Motion::Moving), None);
    t.note_heartbeat("w", 95 * MIN, 95 * MIN, true);
    assert!(t.poll(100 * MIN, 20.0, Some(75.0)).is_empty() && !t.away("w", true));
    assert_eq!(t.phone_probe(112 * MIN, None, GRACE).changed, Some(false));
    assert_eq!(t.poll(116 * MIN, 20.0, None).len(), 1);
    assert!(t.away("w", true));
    let mut u = Tracker::default();
    u.phone_probe(0, Some(42), GRACE);
    u.observe("w", MIN, true, Some(Motion::Moving), None);
    u.phone_probe(30 * MIN, None, GRACE);
    assert_eq!(u.poll(31 * MIN, 20.0, None).len(), 1);
}

#[test]
fn feature_off_is_unchanged() {
    let mut t = Tracker::default();
    away_watch(&mut t, 0);
    assert_eq!(t.poll(30 * MIN, 20.0, None).len(), 1);
    assert!(t.away("w", true) && !t.phone().vouch);
    let cfg = Config::parse("").unwrap();
    assert!(cfg.presence.phone.is_none());
}

#[test]
fn config_section_defaults_and_validation() {
    let cfg = Config::parse("[presence.phone]\nmac = 'aa:bb:cc:dd:ee:ff'\nsubnet = '192.0.2.0/24'\n").unwrap();
    let p = cfg.presence.phone.unwrap();
    assert_eq!((p.probe_s, p.sweep_min, p.absent_grace_min), (60, 10.0, 20.0));
    assert!(Config::parse("[presence.phone]\nmac = 'nope'\n").is_err());
    assert!(Config::parse("[presence.phone]\nmac = 'aa:bb:cc:dd:ee:ff'\nsubnet = 'x'\n").is_err());
}
