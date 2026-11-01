use crate::crypto::*;
use crate::tlv::*;
use core::fmt::{Arguments, Write};
use sha2::{Digest, Sha256};
use zeroize::Zeroize;

pub const MTU: usize = 1280;
pub const OK: i32 = 0;
pub const E_ARG: i32 = -1;
pub const E_TIMEOUT: i32 = -2;
pub const E_PROTO: i32 = -3;
pub const E_CERT: i32 = -4;
pub const E_CRYPTO: i32 = -5;
pub const E_PEER: i32 = -6;
pub const E_STATUS: i32 = -7;
pub const E_KEY: i32 = -8;

const SC_PROTO: u16 = 0;
const IM_PROTO: u16 = 1;
const OP_ACK: u8 = 0x10;
const OP_SIGMA1: u8 = 0x30;
const OP_SIGMA2: u8 = 0x31;
const OP_SIGMA3: u8 = 0x32;
const OP_STATUS: u8 = 0x40;
const OP_WRITE_REQ: u8 = 0x06;
const OP_WRITE_RSP: u8 = 0x07;
const OP_STATUS_RSP: u8 = 0x01;
const IM_REVISION: u64 = 11;
const N2: [u8; 13] = *b"NCASE_Sigma2N";
const N3: [u8; 13] = *b"NCASE_Sigma3N";

pub trait Io {
    fn send(&mut self, b: &[u8]) -> i32;
    fn recv(&mut self, b: &mut [u8], timeout_ms: u32) -> i32;
    fn now_ms(&mut self) -> u32;
    fn rng(&mut self, b: &mut [u8]);
}

#[derive(Clone, Copy)]
pub struct Mrp {
    pub idle_ms: u32,
    pub active_ms: u32,
    pub max_ms: u32,
    pub max_tx: u8,
    pub resp_ms: u32,
}

impl Mrp {
    pub fn from_txt(sii_ms: u32, sai_ms: u32) -> Mrp {
        Mrp {
            idle_ms: if sii_ms != 0 { sii_ms.clamp(2000, 60000) } else { 3000 },
            active_ms: if sai_ms != 0 { sai_ms.clamp(1000, 10000) } else { 1500 },
            max_ms: 60000,
            max_tx: 5,
            resp_ms: 20000,
        }
    }
}

pub fn backoff_ms(base_ms: u32, send_count: u8, max_ms: u32, rnd: u32) -> u32 {
    let mut i = base_ms as u64 * 1127 / 1024;
    for _ in 1..send_count {
        i = i * 16 / 10;
    }
    i = i * (1024 + (rnd & 255) as u64) / 1024;
    if i > max_ms as u64 {
        max_ms
    } else {
        i as u32
    }
}

pub struct Creds<'a> {
    pub fabric_id: u64,
    pub node_id: u64,
    pub ipk: &'a [u8; 16],
    pub key: &'a [u8; 97],
    pub rcac: &'a [u8],
    pub icac: &'a [u8],
    pub noc: &'a [u8],
}

pub struct Scratch {
    tx: [u8; MTU],
    rx: [u8; MTU],
    pl: [u8; MTU],
    wk: [u8; MTU],
    wk2: [u8; MTU],
}

impl Scratch {
    pub const fn new() -> Scratch {
        Scratch { tx: [0; MTU], rx: [0; MTU], pl: [0; MTU], wk: [0; MTU], wk2: [0; MTU] }
    }
}

impl Default for Scratch {
    fn default() -> Self {
        Scratch::new()
    }
}

pub struct ErrBuf {
    pub b: [u8; 48],
    pub n: usize,
}

impl ErrBuf {
    pub const fn new() -> ErrBuf {
        ErrBuf { b: [0; 48], n: 0 }
    }

    pub fn as_bytes(&self) -> &[u8] {
        &self.b[..self.n]
    }

    pub fn set(&mut self, a: Arguments) {
        if self.n == 0 {
            let _ = self.write_fmt(a);
        }
    }
}

impl Default for ErrBuf {
    fn default() -> Self {
        ErrBuf::new()
    }
}

impl Write for ErrBuf {
    fn write_str(&mut self, s: &str) -> core::fmt::Result {
        for c in s.bytes() {
            if let Some(d) = self.b.get_mut(self.n) {
                *d = c;
                self.n += 1;
            }
        }
        Ok(())
    }
}

struct Secrets {
    shared: [u8; 32],
    k: [u8; 16],
    keys: [u8; 48],
    salt: [u8; 145],
    eph_d: [u8; 32],
}

impl Drop for Secrets {
    fn drop(&mut self) {
        self.shared.zeroize();
        self.k.zeroize();
        self.keys.zeroize();
        self.salt.zeroize();
        self.eph_d.zeroize();
    }
}

struct Client<'a, I: Io> {
    io: &'a mut I,
    cr: &'a Creds<'a>,
    mrp: Mrp,
    err: &'a mut ErrBuf,
    tx: &'a mut [u8; MTU],
    rx: &'a mut [u8; MTU],
    pl: &'a mut [u8; MTU],
    txlen: usize,
    secured: bool,
    xi: bool,
    acked: bool,
    pend_ack: bool,
    lsid: u16,
    psid: u16,
    xid: u16,
    enc_key: [u8; 16],
    dec_key: [u8; 16],
    ipk: [u8; 16],
    ctr: u32,
    last_ctr: u32,
    pend_ctr: u32,
    win: Window,
    local_node: u64,
    peer_node: u64,
    eph_node: u64,
    base_ms: u32,
    active_ms: u32,
    rph: PHdr,
    rpl_off: usize,
    rpl_len: usize,
}

impl<'a, I: Io> Drop for Client<'a, I> {
    fn drop(&mut self) {
        self.enc_key.zeroize();
        self.dec_key.zeroize();
        self.ipk.zeroize();
    }
}

enum WriteRsp {
    Ok,
    Status(u32),
    Proto(&'static str),
}

fn parse_write_rsp(rpl: &[u8], opcode: u8, ep: u8, cluster: u32, attr: u32) -> WriteRsp {
    let mut rd = Reader::new(rpl);
    let top = match rd.next() {
        Some(t) if t.typ == T_STRUCT => t,
        _ => return WriteRsp::Proto("proto_write"),
    };
    if opcode == OP_STATUS_RSP {
        return WriteRsp::Status(match find(top.d, 0) {
            Some(e) if e.typ == T_UINT => e.u as u32,
            _ => 0xFF,
        });
    }
    let arr = match find(top.d, 0) {
        Some(e) if opcode == OP_WRITE_RSP && e.typ == T_ARRAY => e,
        _ => return WriteRsp::Proto("proto_write"),
    };
    let mut ar = Reader::new(arr.d);
    let mut count = 0;
    while let Some(item) = ar.next() {
        if item.typ != T_STRUCT {
            return WriteRsp::Proto("proto_write");
        }
        let path = match find(item.d, 0) {
            Some(p) if p.typ == T_LIST => p,
            _ => return WriteRsp::Proto("proto_write_path"),
        };
        let m = |t: i32, v: u64| matches!(find(path.d, t), Some(e) if e.typ == T_UINT && e.u == v);
        if !(m(2, ep as u64) && m(3, cluster as u64) && m(4, attr as u64)) {
            return WriteRsp::Proto("proto_write_path");
        }
        let code = match find(item.d, 1) {
            Some(st) if st.typ == T_STRUCT => match find(st.d, 0) {
                Some(c) if c.typ == T_UINT => c.u,
                _ => return WriteRsp::Proto("proto_write"),
            },
            _ => return WriteRsp::Proto("proto_write"),
        };
        count += 1;
        if code != 0 {
            return WriteRsp::Status(code as u32);
        }
    }
    if ar.bad || count == 0 {
        return WriteRsp::Proto("proto_write");
    }
    WriteRsp::Ok
}

struct Sigma2<'b> {
    rand: &'b [u8],
    sid: u16,
    pub_: &'b [u8],
    enc: &'b [u8],
    sai: u32,
}

fn parse_sigma2(rpl: &[u8], opcode: u8) -> Option<Sigma2<'_>> {
    let mut rd = Reader::new(rpl);
    let top = rd.next()?;
    if opcode != OP_SIGMA2 || top.typ != T_STRUCT {
        return None;
    }
    let rand = find(top.d, 1).filter(|e| e.typ == T_BYTES && e.d.len() == RAND_LEN)?.d;
    let sid = find(top.d, 2).filter(|e| e.typ == T_UINT && e.u <= 0xFFFF)?.u as u16;
    let pub_ = find(top.d, 3).filter(|e| e.typ == T_BYTES && e.d.len() == PUB_LEN)?.d;
    let enc = find(top.d, 4).filter(|e| e.typ == T_BYTES && e.d.len() > TAG_LEN && e.d.len() <= MTU)?.d;
    let mut sai = 0;
    if let Some(par) = find(top.d, 5).filter(|e| e.typ == T_STRUCT) {
        if let Some(s) = find(par.d, 2).filter(|e| e.typ == T_UINT && e.u != 0) {
            sai = s.u as u32;
        }
    }
    Some(Sigma2 { rand, sid, pub_, enc, sai })
}

impl<'a, I: Io> Client<'a, I> {
    fn rnd32(&mut self) -> u32 {
        let mut b = [0u8; 4];
        self.io.rng(&mut b);
        u32::from_le_bytes(b)
    }

    fn fail(&mut self, rc: i32, a: Arguments) -> i32 {
        self.err.set(a);
        rc
    }

    fn send_raw(&mut self) -> i32 {
        match self.tx.get(..self.txlen) {
            Some(b) if self.io.send(b) >= 0 => OK,
            _ => E_PROTO,
        }
    }

    fn send_msg(&mut self, proto: u16, op: u8, payload: &[u8], reliable: bool) -> i32 {
        let ph = PHdr {
            xflags: (if self.xi { XF_I } else { 0 }) | (if reliable { XF_R } else { 0 }) | (if self.pend_ack { XF_A } else { 0 }),
            opcode: op,
            xid: self.xid,
            proto,
            ack: self.pend_ctr,
            len: 0,
        };
        let pn = match phdr_encode(&ph, &mut self.pl[..]) {
            Some(n) if payload.len() + n + 64 <= MTU => n,
            _ => return E_ARG,
        };
        self.pl[pn..pn + payload.len()].copy_from_slice(payload);
        let h = Hdr { sid: if self.secured { self.psid } else { 0 }, ctr: self.ctr, has_src: !self.secured, src: self.eph_node, ..Default::default() };
        let hl = match hdr_encode(&h, &mut self.tx[..]) {
            Some(n) => n,
            None => return E_ARG,
        };
        let mut n = pn + payload.len();
        self.tx[hl..hl + n].copy_from_slice(&self.pl[..n]);
        if self.secured {
            let nonce = nonce(h.sflags, h.ctr, self.local_node);
            let (aad, body) = self.tx.split_at_mut(hl);
            if !ccm_seal(&self.enc_key, &nonce, aad, body, n) {
                return E_CRYPTO;
            }
            n += TAG_LEN;
        }
        self.txlen = hl + n;
        self.last_ctr = self.ctr;
        self.ctr = self.ctr.wrapping_add(1);
        self.pend_ack = false;
        self.send_raw()
    }

    fn reack(&mut self, ctr: u32) {
        let (sc, pc, sl, pa) = (self.last_ctr, self.pend_ctr, self.txlen, self.pend_ack);
        let mut keep = [0u8; MTU];
        keep[..sl].copy_from_slice(&self.tx[..sl]);
        self.pend_ack = true;
        self.pend_ctr = ctr;
        self.send_msg(SC_PROTO, OP_ACK, &[], false);
        self.tx[..sl].copy_from_slice(&keep[..sl]);
        keep.zeroize();
        self.txlen = sl;
        self.last_ctr = sc;
        self.pend_ack = pa;
        self.pend_ctr = pc;
    }

    fn recv_one(&mut self, timeout_ms: u32) -> i32 {
        let t0 = self.io.now_ms();
        loop {
            let el = self.io.now_ms().wrapping_sub(t0);
            if el >= timeout_ms {
                return 0;
            }
            let n = self.io.recv(&mut self.rx[..], timeout_ms - el);
            if n < 0 {
                return E_PROTO;
            }
            if n == 0 {
                return 0;
            }
            let n = (n as usize).min(MTU);
            let h = match hdr_decode(&self.rx[..n]) {
                Some(h) => h,
                None => continue,
            };
            let hl = h.len;
            let mut pn = n - hl;
            if self.secured {
                if h.sid != self.lsid {
                    continue;
                }
                let nc = nonce(h.sflags, h.ctr, if h.has_src { h.src } else { self.peer_node });
                let (aad, body) = self.rx.split_at_mut(hl);
                if !ccm_open(&self.dec_key, &nc, aad, body, pn) {
                    continue;
                }
                pn -= TAG_LEN;
            } else if h.sid != 0 || (h.has_dst && h.dst != self.eph_node) {
                continue;
            }
            let ph = match self.rx.get(hl..hl + pn).and_then(phdr_decode) {
                Some(p) => p,
                None => continue,
            };
            if !self.secured && ph.proto != SC_PROTO {
                continue;
            }
            let dup = self.win.seen(h.ctr);
            let ours = ph.xid == self.xid && ((ph.xflags & XF_I) != 0) != self.xi;
            if dup {
                if ours && ph.xflags & XF_R != 0 {
                    self.reack(h.ctr);
                }
                continue;
            }
            if !ours {
                continue;
            }
            self.base_ms = self.active_ms;
            if ph.xflags & XF_A != 0 && ph.ack == self.last_ctr {
                self.acked = true;
            }
            if ph.xflags & XF_R != 0 {
                self.pend_ack = true;
                self.pend_ctr = h.ctr;
            }
            self.rph = ph;
            self.rpl_off = hl + ph.len;
            self.rpl_len = pn - ph.len;
            return if ph.proto == SC_PROTO && ph.opcode == OP_ACK { 2 } else { 1 };
        }
    }

    fn new_exchange(&mut self) {
        self.xid = (self.rnd32() | 1) as u16;
        self.xi = true;
        self.pend_ack = false;
        self.acked = false;
    }

    fn xchg(&mut self, proto: u16, op: u8, payload: &[u8], stage: &str) -> i32 {
        self.acked = false;
        let rc = self.send_msg(proto, op, payload, true);
        if rc != OK {
            return self.fail(rc, format_args!("send_failed"));
        }
        let mut k = 0u8;
        let r = self.rnd32();
        let mut next = self.io.now_ms().wrapping_add(backoff_ms(self.base_ms, 0, self.mrp.max_ms, r));
        let mut resp = 0u32;
        loop {
            let now = self.io.now_ms();
            if self.acked && resp == 0 {
                resp = now.wrapping_add(self.mrp.resp_ms);
            }
            let dl = if self.acked { resp } else { next };
            if (now.wrapping_sub(dl) as i32) >= 0 {
                if self.acked || k.saturating_add(1) >= self.mrp.max_tx {
                    self.err.set(format_args!("timeout_{}", stage));
                    return E_TIMEOUT;
                }
                k += 1;
                if self.send_raw() != OK {
                    return E_PROTO;
                }
                let r = self.rnd32();
                next = now.wrapping_add(backoff_ms(self.base_ms, k, self.mrp.max_ms, r));
                continue;
            }
            let r = self.recv_one(dl.wrapping_sub(now));
            if r < 0 {
                return r;
            }
            if r == 1 {
                return OK;
            }
        }
    }

    fn status_report(&mut self, stage: &str) -> i32 {
        let rpl = self.rx.get(self.rpl_off..self.rpl_off + self.rpl_len).unwrap_or(&[]);
        if self.rph.opcode != OP_STATUS || self.rph.proto != SC_PROTO || rpl.len() < 8 {
            return self.fail(E_PROTO, format_args!("proto_status"));
        }
        let gen = rpl[0] as u32 | (rpl[1] as u32) << 8;
        let code = rpl[6] as u32 | (rpl[7] as u32) << 8;
        if gen == 0 && code == 0 {
            return OK;
        }
        match code {
            1 if self.rph.proto == SC_PROTO => self.err.set(format_args!("case_nosharedroot")),
            2 if self.rph.proto == SC_PROTO => self.err.set(format_args!("case_invalidparam")),
            4 if self.rph.proto == SC_PROTO => self.err.set(format_args!("case_busy")),
            _ => self.err.set(format_args!("{}_st{}_{}", stage, gen & 0xFF, code & 0xFFFF)),
        }
        E_PEER
    }

    fn check_chain<'b>(&mut self, rcac: &Cert, icac: &[u8], noc: &'b [u8], is_peer: bool) -> Result<Cert<'b>, i32> {
        let cr = self.cr;
        let nc = match Cert::parse(noc) {
            Some(c) => c,
            None => return Err(self.fail(E_CERT, format_args!("cert_noc_parse"))),
        };
        let mut issuer = rcac.pub_;
        let ic;
        if !icac.is_empty() {
            ic = match Cert::parse(icac) {
                Some(c) if c.is_ca => c,
                _ => return Err(self.fail(E_CERT, format_args!("cert_icac_parse"))),
            };
            if !ic.verify(rcac.pub_) {
                return Err(self.fail(E_CERT, format_args!("cert_icac_sig")));
            }
            if ic.has_fabric && ic.fabric_id != cr.fabric_id {
                return Err(self.fail(E_CERT, format_args!("cert_icac_fabric")));
            }
            issuer = ic.pub_;
        }
        if nc.is_ca || !nc.verify(issuer) {
            return Err(self.fail(E_CERT, format_args!("cert_noc_sig")));
        }
        if !nc.has_fabric || nc.fabric_id != cr.fabric_id {
            return Err(self.fail(E_CERT, format_args!("cert_fabric")));
        }
        if !nc.has_node || nc.node_id != if is_peer { self.peer_node } else { cr.node_id } {
            return Err(self.fail(E_CERT, format_args!("cert_node")));
        }
        Ok(nc)
    }

    fn put_certs(&self, w: &mut Writer) {
        w.bytes(1, self.cr.noc);
        if !self.cr.icac.is_empty() {
            w.bytes(2, self.cr.icac);
        }
    }

    fn case_connect(&mut self, wk: &mut [u8; MTU], wk2: &mut [u8; MTU], rcac: &Cert, sx: &mut Secrets) -> i32 {
        let cr = self.cr;
        let mut hash = Sha256::new();
        let eph_d = match random_scalar_bytes(&mut |b| self.io.rng(b)) {
            Some(d) => d,
            None => return self.fail(E_CRYPTO, format_args!("crypto_ephkey")),
        };
        sx.eph_d = eph_d;
        let eph_pub = match pub_from_priv(&sx.eph_d) {
            Some(p) => p,
            None => return self.fail(E_CRYPTO, format_args!("crypto_ephkey")),
        };
        let mut i_rand = [0u8; RAND_LEN];
        self.io.rng(&mut i_rand);
        loop {
            self.lsid = self.rnd32() as u16;
            if self.lsid != 0 {
                break;
            }
        }
        let dest = match dest_id(&self.ipk, &i_rand, rcac.pub_, cr.fabric_id, self.peer_node) {
            Some(d) => d,
            None => return self.fail(E_CRYPTO, format_args!("crypto_destid")),
        };
        let mut w = Writer::new(&mut wk[..]);
        w.start(-1, T_STRUCT);
        w.bytes(1, &i_rand);
        w.uint(2, self.lsid as u64);
        w.bytes(3, &dest);
        w.bytes(4, &eph_pub);
        w.end();
        let s1len = w.len;
        hash.update(&wk[..s1len]);
        self.secured = false;
        let hi = self.rnd32() as u64;
        let lo = self.rnd32() as u64;
        self.eph_node = hi << 32 | lo;
        self.ctr = (self.rnd32() & 0x0FFF_FFFF) + 1;
        self.new_exchange();
        let r = self.xchg(SC_PROTO, OP_SIGMA1, &wk[..s1len], "sigma1");
        if r != OK {
            return r;
        }
        if self.rph.proto != SC_PROTO {
            return self.fail(E_PROTO, format_args!("proto_sigma2"));
        }
        if self.rph.opcode == OP_STATUS {
            let r = self.status_report("sigma1");
            return if r == OK { self.fail(E_PROTO, format_args!("proto_sigma2")) } else { r };
        }
        let (rpl_off, rpl_len) = (self.rpl_off, self.rpl_len);
        let rpl = self.rx.get(rpl_off..rpl_off + rpl_len).unwrap_or(&[]);
        let (psid, sai, en, resp_pub, rrand) = {
            let s2 = match parse_sigma2(rpl, self.rph.opcode) {
                Some(s) => s,
                None => return self.fail(E_PROTO, format_args!("proto_sigma2")),
            };
            let mut rp = [0u8; PUB_LEN];
            rp.copy_from_slice(s2.pub_);
            let mut rr = [0u8; RAND_LEN];
            rr.copy_from_slice(s2.rand);
            wk[..s2.enc.len()].copy_from_slice(s2.enc);
            (s2.sid, s2.sai, s2.enc.len(), rp, rr)
        };
        self.psid = psid;
        if sai != 0 {
            self.active_ms = sai.clamp(self.active_ms, 10000);
        }
        sx.salt[..16].copy_from_slice(&self.ipk);
        sx.salt[16..48].copy_from_slice(&rrand);
        sx.salt[48..48 + PUB_LEN].copy_from_slice(&resp_pub);
        sx.salt[48 + PUB_LEN..].copy_from_slice(&hash.clone().finalize());
        hash.update(rpl);
        sx.shared = match ecdh(&sx.eph_d, &resp_pub) {
            Some(s) => s,
            None => return self.fail(E_CRYPTO, format_args!("crypto_ecdh")),
        };
        if !hkdf(&sx.salt, &sx.shared, b"Sigma2", &mut sx.k) || !ccm_open(&sx.k, &N2, &[], &mut wk[..], en) {
            return self.fail(E_CRYPTO, format_args!("crypto_sigma2"));
        }
        let tbe = {
            let mut rd = Reader::new(&wk[..en - TAG_LEN]);
            let tb = rd.next();
            tb.filter(|t| t.typ == T_STRUCT).and_then(|tb| {
                let noc = find(tb.d, 1).filter(|e| e.typ == T_BYTES)?;
                let sig = find(tb.d, 3).filter(|e| e.typ == T_BYTES && e.d.len() == SIG_LEN)?;
                let ic = find(tb.d, 2).filter(|e| e.typ == T_BYTES);
                Some((noc.d, ic.map(|e| e.d), sig.d))
            })
        };
        let (t_noc, t_ic, t_sig) = match tbe {
            Some(t) => t,
            None => return self.fail(E_PROTO, format_args!("proto_sigma2_tbe")),
        };
        let pn = match self.check_chain(rcac, t_ic.unwrap_or(&[]), t_noc, true) {
            Ok(c) => c,
            Err(r) => return r,
        };
        let mut w = Writer::new(&mut wk2[..]);
        w.start(-1, T_STRUCT);
        w.bytes(1, t_noc);
        if let Some(ic) = t_ic {
            w.bytes(2, ic);
        }
        w.bytes(3, &resp_pub);
        w.bytes(4, &eph_pub);
        w.end();
        let (werr, wl) = (w.err, w.len);
        if werr || !ecdsa_verify(pn.pub_, &wk2[..wl], t_sig) {
            return self.fail(E_CERT, format_args!("sig_sigma2"));
        }
        let mut w = Writer::new(&mut wk2[..]);
        w.start(-1, T_STRUCT);
        self.put_certs(&mut w);
        w.bytes(3, &eph_pub);
        w.bytes(4, &resp_pub);
        w.end();
        let tl = w.len;
        let sig = if w.err { None } else { ecdsa_sign(&cr.key[PUB_LEN..], &wk2[..tl], &mut |b| self.io.rng(b)) };
        let sig = match sig {
            Some(s) => s,
            None => return self.fail(E_CRYPTO, format_args!("crypto_sign")),
        };
        let mut w = Writer::new(&mut wk[..MTU - TAG_LEN]);
        w.start(-1, T_STRUCT);
        self.put_certs(&mut w);
        w.bytes(3, &sig);
        w.end();
        if w.err {
            return self.fail(E_ARG, format_args!("arg_sigma3_size"));
        }
        let pl = w.len;
        sx.salt[..16].copy_from_slice(&self.ipk);
        sx.salt[16..48].copy_from_slice(&hash.clone().finalize());
        if !hkdf(&sx.salt[..48], &sx.shared, b"Sigma3", &mut sx.k) || !ccm_seal(&sx.k, &N3, &[], &mut wk[..], pl) {
            return self.fail(E_CRYPTO, format_args!("crypto_sigma3"));
        }
        let mut w = Writer::new(&mut wk2[..]);
        w.start(-1, T_STRUCT);
        w.bytes(1, &wk[..pl + TAG_LEN]);
        w.end();
        if w.err {
            return self.fail(E_ARG, format_args!("arg_sigma3_size"));
        }
        let s3len = w.len;
        hash.update(&wk2[..s3len]);
        let r = self.xchg(SC_PROTO, OP_SIGMA3, &wk2[..s3len], "sigma3");
        if r != OK {
            return r;
        }
        let r = self.status_report("sigma3");
        if r != OK {
            return r;
        }
        if self.pend_ack {
            self.send_msg(SC_PROTO, OP_ACK, &[], false);
            self.pend_ack = false;
        }
        sx.salt[..16].copy_from_slice(&self.ipk);
        sx.salt[16..48].copy_from_slice(&hash.finalize());
        if !hkdf(&sx.salt[..48], &sx.shared, b"SessionKeys", &mut sx.keys) {
            return self.fail(E_CRYPTO, format_args!("crypto_keys"));
        }
        self.enc_key.copy_from_slice(&sx.keys[..16]);
        self.dec_key.copy_from_slice(&sx.keys[16..32]);
        self.secured = true;
        self.win = Window::default();
        self.ctr = (self.rnd32() & 0x0FFF_FFFF) + 1;
        OK
    }

    fn write_attr(&mut self, wk: &mut [u8; MTU], ep: u8, cluster: u32, attr: u32, val: i16) -> i32 {
        if !self.secured {
            return self.fail(E_PROTO, format_args!("proto_session"));
        }
        let mut w = Writer::new(&mut wk[..]);
        w.start(-1, T_STRUCT);
        w.boolean(0, false);
        w.boolean(1, false);
        w.start(2, T_ARRAY);
        w.start(-1, T_STRUCT);
        w.start(1, T_LIST);
        w.uint(2, ep as u64);
        w.uint(3, cluster as u64);
        w.uint(4, attr as u64);
        w.end();
        w.sint(2, val as i64);
        w.end();
        w.end();
        w.uint(255, IM_REVISION);
        w.end();
        if w.err {
            return self.fail(E_ARG, format_args!("arg_write"));
        }
        let wl = w.len;
        self.new_exchange();
        let r = self.xchg(IM_PROTO, OP_WRITE_REQ, &wk[..wl], "write");
        if r != OK {
            return r;
        }
        if self.rph.proto != IM_PROTO {
            return self.fail(E_PROTO, format_args!("proto_write"));
        }
        let rpl = self.rx.get(self.rpl_off..self.rpl_off + self.rpl_len).unwrap_or(&[]);
        match parse_write_rsp(rpl, self.rph.opcode, ep, cluster, attr) {
            WriteRsp::Proto(m) => return self.fail(E_PROTO, format_args!("{}", m)),
            WriteRsp::Status(s) => return self.fail(E_STATUS, format_args!("write_status_{:02x}", s)),
            WriteRsp::Ok => {}
        }
        if self.pend_ack {
            self.send_msg(SC_PROTO, OP_ACK, &[], false);
            self.pend_ack = false;
        }
        OK
    }

    fn close_session(&mut self) {
        self.new_exchange();
        self.send_msg(SC_PROTO, OP_STATUS, &[0, 0, 0, 0, 0, 0, 3, 0], false);
    }
}

pub fn creds_cfid(cr: &Creds) -> Option<[u8; 8]> {
    let rcac = Cert::parse(cr.rcac)?;
    compressed_fabric_id(rcac.pub_, cr.fabric_id)
}

#[allow(clippy::too_many_arguments)]
pub fn write_int16<I: Io>(io: &mut I, s: &mut Scratch, cr: &Creds, peer_node: u64, mrp: &Mrp, ep: u8, cluster: u32, attr: u32, val: i16, err: &mut ErrBuf) -> i32 {
    let Scratch { tx, rx, pl, wk, wk2 } = s;
    let mut c = Client {
        io,
        cr,
        mrp: *mrp,
        err,
        tx,
        rx,
        pl,
        txlen: 0,
        secured: false,
        xi: false,
        acked: false,
        pend_ack: false,
        lsid: 0,
        psid: 0,
        xid: 0,
        enc_key: [0; 16],
        dec_key: [0; 16],
        ipk: [0; 16],
        ctr: 0,
        last_ctr: 0,
        pend_ctr: 0,
        win: Window::default(),
        local_node: cr.node_id,
        peer_node,
        eph_node: 0,
        base_ms: mrp.idle_ms,
        active_ms: mrp.active_ms,
        rph: PHdr::default(),
        rpl_off: 0,
        rpl_len: 0,
    };
    let rc = run(&mut c, wk, wk2, ep, cluster, attr, val);
    c.tx.zeroize();
    c.rx.zeroize();
    c.pl.zeroize();
    wk.zeroize();
    wk2.zeroize();
    if rc != OK {
        c.err.set(format_args!("error_{}", rc));
    }
    rc
}

fn run<I: Io>(c: &mut Client<I>, wk: &mut [u8; MTU], wk2: &mut [u8; MTU], ep: u8, cluster: u32, attr: u32, val: i16) -> i32 {
    let cr = c.cr;
    let rcac = match Cert::parse(cr.rcac) {
        Some(r) if r.is_ca => r,
        _ => return c.fail(E_CERT, format_args!("cert_rcac")),
    };
    let own = match c.check_chain(&rcac, cr.icac, cr.noc, false) {
        Ok(o) => o,
        Err(_) => return E_CERT,
    };
    let chk = pub_from_priv(&cr.key[PUB_LEN..]);
    if cr.key[0] != 4 || !ct_eq(&cr.key[..PUB_LEN], own.pub_) || !matches!(chk, Some(p) if ct_eq(&p, own.pub_)) {
        return c.fail(E_KEY, format_args!("key_mismatch"));
    }
    let ipk = compressed_fabric_id(rcac.pub_, cr.fabric_id).and_then(|cf| ipk_operational(cr.ipk, &cf));
    match ipk {
        Some(k) => c.ipk = k,
        None => return c.fail(E_CRYPTO, format_args!("crypto_ipk")),
    }
    let mut sx = Secrets { shared: [0; 32], k: [0; 16], keys: [0; 48], salt: [0; 145], eph_d: [0; 32] };
    let mut rc = c.case_connect(wk, wk2, &rcac, &mut sx);
    if rc == OK {
        rc = c.write_attr(wk, ep, cluster, attr, val);
        c.close_session();
    }
    rc
}
