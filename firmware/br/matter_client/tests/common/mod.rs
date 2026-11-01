#![allow(dead_code)]
use matter_client::client::*;
use std::collections::HashMap;
use std::net::UdpSocket;
use std::time::{Duration, Instant};

pub fn vectors() -> HashMap<String, Vec<u8>> {
    let text = std::fs::read_to_string(concat!(env!("CARGO_MANIFEST_DIR"), "/../test/vectors.h")).unwrap();
    let mut out = HashMap::new();
    for line in text.lines() {
        let Some(rest) = line.strip_prefix("static const uint8_t ") else { continue };
        let name = rest.split('[').next().unwrap().to_string();
        let body = rest.split('{').nth(1).unwrap().split('}').next().unwrap();
        let bytes = body.split(',').map(|x| u8::from_str_radix(x.trim().trim_start_matches("0x"), 16).unwrap()).collect();
        out.insert(name, bytes);
    }
    out
}

pub fn hex(s: &str) -> Vec<u8> {
    (0..s.len()).step_by(2).map(|i| u8::from_str_radix(&s[i..i + 2], 16).unwrap()).collect()
}

pub struct Fixture {
    pub rcac: Vec<u8>,
    pub icac: Vec<u8>,
    pub noc: Vec<u8>,
    pub ipk: [u8; 16],
    pub key: [u8; 97],
    pub fabric_id: u64,
    pub node_id: u64,
}

impl Fixture {
    pub fn new() -> Fixture {
        let v = vectors();
        let noc = v["V_Node01_01_Chip"].clone();
        let c = matter_client::crypto::Cert::parse(&noc).unwrap();
        let mut key = [0u8; 97];
        key[..65].copy_from_slice(c.pub_);
        key[65..].copy_from_slice(&v["V_Node01_01_PrivateKey"]);
        Fixture {
            rcac: v["V_Root01_Chip"].clone(),
            icac: v["V_ICA01_Chip"].clone(),
            noc: noc.clone(),
            ipk: hex("235bf7e62823d358dca4ba50b1535f4b").try_into().unwrap(),
            key,
            fabric_id: c.fabric_id,
            node_id: c.node_id,
        }
    }

    pub fn creds(&self) -> Creds<'_> {
        Creds { fabric_id: self.fabric_id, node_id: self.node_id, ipk: &self.ipk, key: &self.key, rcac: &self.rcac, icac: &self.icac, noc: &self.noc }
    }
}

pub struct Net {
    pub sock: UdpSocket,
    pub t0: Instant,
    pub sent: u32,
    pub rng: Box<dyn FnMut(&mut [u8])>,
    pub log: Vec<Vec<u8>>,
}

impl Net {
    pub fn new(port: u16, rng: Box<dyn FnMut(&mut [u8])>) -> Net {
        let sock = UdpSocket::bind("127.0.0.1:0").unwrap();
        sock.connect(("127.0.0.1", port)).unwrap();
        Net { sock, t0: Instant::now(), sent: 0, rng, log: vec![] }
    }
}

impl Io for Net {
    fn send(&mut self, b: &[u8]) -> i32 {
        self.sent += 1;
        self.log.push(b.to_vec());
        self.sock.send(b).map(|n| n as i32).unwrap_or(-1)
    }

    fn recv(&mut self, b: &mut [u8], ms: u32) -> i32 {
        self.sock.set_read_timeout(Some(Duration::from_millis(ms.max(1) as u64))).unwrap();
        self.sock.recv(b).map(|n| n as i32).unwrap_or(0)
    }

    fn now_ms(&mut self) -> u32 {
        self.t0.elapsed().as_millis() as u32
    }

    fn rng(&mut self, b: &mut [u8]) {
        (self.rng)(b)
    }
}

pub fn lcg_rng(seed: u64) -> Box<dyn FnMut(&mut [u8])> {
    let mut s = seed;
    Box::new(move |b: &mut [u8]| {
        for x in b.iter_mut() {
            s = s.wrapping_mul(6364136223846793005).wrapping_add(1442695040888963407);
            *x = (s >> 33) as u8;
        }
    })
}
