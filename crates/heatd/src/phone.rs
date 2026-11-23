use crate::config::PhoneSection;
use crate::http::AppState;
use crate::lake::{self, kind};
use crate::rules::presence_event;
use crate::storage::now_ms;
use anyhow::{anyhow, bail, Result};
use serde_json::{json, Value};
use std::net::{Ipv4Addr, UdpSocket};
use std::time::{Duration, Instant};

pub const DEVICE: &str = "phone";
pub const SOURCE: &str = "phone_wifi";
pub const WHY: &str = "Handy im WLAN";
const ARP_PATH: &str = "/proc/net/arp";
const DISCARD_PORT: u16 = 9;
const ARP_COMPLETE: u32 = 0x2;
const MAX_HOST_BITS: u32 = 10;
const KNOWN_WAIT: Duration = Duration::from_millis(1000);
const SWEEP_WAIT: Duration = Duration::from_millis(2500);

pub fn parse_mac(text: &str) -> Result<[u8; 6]> {
    let parts: Vec<&str> = text.trim().split([':', '-']).collect();
    if parts.len() != 6 {
        bail!("presence.phone.mac: expected six hex bytes, got {text:?}");
    }
    let mut mac = [0u8; 6];
    for (slot, part) in mac.iter_mut().zip(parts) {
        *slot = u8::from_str_radix(part, 16).map_err(|_| anyhow!("presence.phone.mac: bad byte {part:?}"))?;
    }
    Ok(mac)
}

#[derive(Debug, Clone, Copy, PartialEq)]
pub struct Subnet {
    base: u32,
    bits: u32,
}

impl Subnet {
    pub fn parse(text: &str) -> Result<Self> {
        let (addr, prefix) = text.trim().split_once('/').ok_or_else(|| anyhow!("presence.phone.subnet: expected a.b.c.d/n, got {text:?}"))?;
        let addr: Ipv4Addr = addr.parse().map_err(|_| anyhow!("presence.phone.subnet: bad address {addr:?}"))?;
        let prefix: u32 = prefix.parse().map_err(|_| anyhow!("presence.phone.subnet: bad prefix {prefix:?}"))?;
        if !(32 - MAX_HOST_BITS..=30).contains(&prefix) {
            bail!("presence.phone.subnet: prefix must be {}..=30", 32 - MAX_HOST_BITS);
        }
        let bits = 32 - prefix;
        Ok(Self { base: u32::from(addr) & !((1u32 << bits) - 1), bits })
    }

    pub fn local() -> Result<Self> {
        let socket = UdpSocket::bind("0.0.0.0:0")?;
        socket.connect((Ipv4Addr::new(192, 0, 2, 1), DISCARD_PORT))?;
        match socket.local_addr()?.ip() {
            std::net::IpAddr::V4(ip) => Self::parse(&format!("{ip}/24")),
            other => bail!("presence.phone.subnet: local address {other} is not IPv4"),
        }
    }

    pub fn hosts(&self) -> impl Iterator<Item = Ipv4Addr> + '_ {
        (1..(1u32 << self.bits) - 1).map(|n| Ipv4Addr::from(self.base + n))
    }
}

pub fn find_phone(arp: &str, mac: &[u8; 6]) -> Option<Ipv4Addr> {
    arp.lines().skip(1).find_map(|line| {
        let cols: Vec<&str> = line.split_whitespace().collect();
        let flags = u32::from_str_radix(cols.get(2)?.trim_start_matches("0x"), 16).ok()?;
        (flags & ARP_COMPLETE != 0 && parse_mac(cols.get(3)?).ok().as_ref() == Some(mac)).then(|| cols[0].parse().ok()).flatten()
    })
}

pub struct Prober {
    mac: [u8; 6],
    subnet: Subnet,
    socket: UdpSocket,
    sweep_every: Duration,
    arp_path: String,
    last_ip: Option<Ipv4Addr>,
    last_sweep: Option<Instant>,
}

impl Prober {
    pub fn new(cfg: &PhoneSection) -> Result<Self> {
        let subnet = if cfg.subnet.trim().is_empty() { Subnet::local()? } else { Subnet::parse(&cfg.subnet)? };
        Ok(Self {
            mac: parse_mac(&cfg.mac)?,
            subnet,
            socket: UdpSocket::bind("0.0.0.0:0")?,
            sweep_every: Duration::from_secs_f64(cfg.sweep_min * 60.0),
            arp_path: ARP_PATH.into(),
            last_ip: None,
            last_sweep: None,
        })
    }

    fn poke(&self, ip: Ipv4Addr) {
        let _ = self.socket.send_to(&[0], (ip, DISCARD_PORT));
    }

    fn lookup(&self) -> Option<Ipv4Addr> {
        std::fs::read_to_string(&self.arp_path).ok().and_then(|text| find_phone(&text, &self.mac))
    }

    pub fn probe(&mut self) -> Option<Ipv4Addr> {
        if let Some(ip) = self.last_ip {
            self.poke(ip);
            std::thread::sleep(KNOWN_WAIT);
            if let Some(found) = self.lookup() {
                self.last_ip = Some(found);
                return Some(found);
            }
        }
        if self.last_sweep.is_some_and(|t| t.elapsed() < self.sweep_every) {
            return None;
        }
        self.last_sweep = Some(Instant::now());
        self.subnet.hosts().for_each(|ip| self.poke(ip));
        std::thread::sleep(SWEEP_WAIT);
        self.last_ip = self.lookup();
        self.last_ip
    }
}

#[derive(Debug, Default)]
pub struct PhoneLink {
    pub vouch: bool,
    last_seen_ms: Option<i64>,
    octet: Option<u8>,
}

impl PhoneLink {
    pub fn note(&mut self, now_ms: i64, found: Option<u8>, grace_ms: i64) {
        if let Some(octet) = found {
            self.last_seen_ms = Some(now_ms);
            self.octet = Some(octet);
        }
        self.vouch = self.last_seen_ms.is_some_and(|t| now_ms - t <= grace_ms);
    }

    pub fn age_minutes(&self, now_ms: i64) -> Option<f64> {
        self.last_seen_ms.map(|t| (now_ms - t).max(0) as f64 / 60_000.0)
    }

    pub fn view(&self, now_ms: i64) -> Value {
        json!({"present": self.vouch, "last_seen_age_s": self.last_seen_ms.map(|t| (now_ms - t).max(0) as f64 / 1000.0), "ip_last_octet": self.octet, "source": "arp", "why": self.vouch.then_some(WHY)})
    }
}

#[derive(Debug, Default)]
pub struct PhoneUpdate {
    pub arrived: Option<crate::rules::Transition>,
    pub changed: Option<bool>,
}

fn apply(state: &AppState, grace_ms: i64, found: Option<Ipv4Addr>) {
    let now = now_ms();
    let update = state.tracker.lock().unwrap().phone_probe(now, found.map(|ip| ip.octets()[3]), grace_ms);
    if let Some(transition) = update.arrived {
        if let Err(e) = state.sink.append(presence_event(DEVICE, transition)) {
            eprintln!("presence event: {e}");
        }
    }
    if let Some(present) = update.changed {
        let event = lake::Event::now(kind::PHONE_PRESENCE, "heatd").with_detail(json!({"present": present, "source": SOURCE}));
        if let Err(e) = state.sink.append(event) {
            eprintln!("phone event: {e}");
        }
    }
}

pub fn spawn(state: AppState) {
    let Some(cfg) = state.config.presence.phone.clone() else { return };
    let mut prober = match Prober::new(&cfg) {
        Ok(p) => p,
        Err(e) => {
            eprintln!("phone presence disabled: {e}");
            return;
        }
    };
    let grace_ms = (cfg.absent_grace_min * 60_000.0) as i64;
    tokio::spawn(async move {
        let mut tick = tokio::time::interval(Duration::from_secs(cfg.probe_s.max(1)));
        tick.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
        loop {
            tick.tick().await;
            let Ok((back, found)) = tokio::task::spawn_blocking(move || {
                let found = prober.probe();
                (prober, found)
            })
            .await
            else {
                eprintln!("phone probe task failed");
                return;
            };
            prober = back;
            apply(&state, grace_ms, found);
        }
    });
}
