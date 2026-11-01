use crate::tlv::{find, Elem, Reader, T_BOOL, T_BYTES, T_LIST, T_STRUCT, T_UINT, T_UTF8};
use aes::Aes128;
use ccm::aead::generic_array::GenericArray;
use ccm::aead::{AeadInPlace, KeyInit};
use ccm::consts::{U13, U16};
use ccm::Ccm;
use hmac::{Hmac, Mac};
use p256::ecdsa::signature::hazmat::PrehashVerifier;
use p256::ecdsa::{Signature, VerifyingKey};
use p256::elliptic_curve::ops::Reduce;
use p256::elliptic_curve::point::AffineCoordinates;
use p256::elliptic_curve::sec1::ToEncodedPoint;
use p256::elliptic_curve::{Field, PrimeField};
use p256::{ProjectivePoint, PublicKey, Scalar, SecretKey, U256};
use sha2::{Digest, Sha256};
use zeroize::Zeroize;

pub const PUB_LEN: usize = 65;
pub const SIG_LEN: usize = 64;
pub const KEY_LEN: usize = 16;
pub const TAG_LEN: usize = 16;
pub const HASH_LEN: usize = 32;
pub const RAND_LEN: usize = 32;

pub type Rng<'a> = &'a mut dyn FnMut(&mut [u8]);

pub fn ct_eq(a: &[u8], b: &[u8]) -> bool {
    use subtle::ConstantTimeEq;
    a.len() == b.len() && bool::from(a.ct_eq(b))
}

pub fn sha256(parts: &[&[u8]]) -> [u8; 32] {
    let mut h = Sha256::new();
    for p in parts {
        h.update(p);
    }
    h.finalize().into()
}

pub fn hmac256(key: &[u8], msg: &[u8]) -> Option<[u8; 32]> {
    let mut m = <Hmac<Sha256> as Mac>::new_from_slice(key).ok()?;
    m.update(msg);
    Some(m.finalize().into_bytes().into())
}

pub fn hkdf(salt: &[u8], ikm: &[u8], info: &[u8], out: &mut [u8]) -> bool {
    let s = if salt.is_empty() { None } else { Some(salt) };
    hkdf::Hkdf::<Sha256>::new(s, ikm).expand(info, out).is_ok()
}

pub fn compressed_fabric_id(root_pub: &[u8], fabric_id: u64) -> Option<[u8; 8]> {
    let mut out = [0u8; 8];
    if root_pub.len() != PUB_LEN || !hkdf(&fabric_id.to_be_bytes(), &root_pub[1..], b"CompressedFabric", &mut out) {
        return None;
    }
    Some(out)
}

pub fn ipk_operational(epoch: &[u8], cfid: &[u8; 8]) -> Option<[u8; 16]> {
    let mut out = [0u8; 16];
    if !hkdf(cfid, epoch, b"GroupKey v1.0", &mut out) {
        return None;
    }
    Some(out)
}

pub fn dest_id(ipk: &[u8], rnd: &[u8; 32], root_pub: &[u8], fabric_id: u64, node_id: u64) -> Option<[u8; 32]> {
    if root_pub.len() != PUB_LEN {
        return None;
    }
    let mut m = [0u8; RAND_LEN + PUB_LEN + 16];
    m[..RAND_LEN].copy_from_slice(rnd);
    m[RAND_LEN..RAND_LEN + PUB_LEN].copy_from_slice(root_pub);
    m[RAND_LEN + PUB_LEN..RAND_LEN + PUB_LEN + 8].copy_from_slice(&fabric_id.to_le_bytes());
    m[RAND_LEN + PUB_LEN + 8..].copy_from_slice(&node_id.to_le_bytes());
    hmac256(ipk, &m)
}

type Aes128Ccm = Ccm<Aes128, U16, U13>;

pub fn ccm_seal(key: &[u8; 16], nonce: &[u8; 13], aad: &[u8], buf: &mut [u8], n: usize) -> bool {
    let (data, tag) = match n.checked_add(TAG_LEN).and_then(|t| buf.get_mut(..t)) {
        Some(b) => b.split_at_mut(n),
        None => return false,
    };
    let c = Aes128Ccm::new(GenericArray::from_slice(key));
    match c.encrypt_in_place_detached(GenericArray::from_slice(nonce), aad, data) {
        Ok(t) => {
            tag.copy_from_slice(&t);
            true
        }
        Err(_) => false,
    }
}

pub fn ccm_open(key: &[u8; 16], nonce: &[u8; 13], aad: &[u8], buf: &mut [u8], n: usize) -> bool {
    if n < TAG_LEN {
        return false;
    }
    let (data, tag) = match buf.get_mut(..n) {
        Some(b) => b.split_at_mut(n - TAG_LEN),
        None => return false,
    };
    let c = Aes128Ccm::new(GenericArray::from_slice(key));
    c.decrypt_in_place_detached(GenericArray::from_slice(nonce), aad, data, GenericArray::from_slice(tag)).is_ok()
}

pub fn ecdsa_verify(pubkey: &[u8], msg: &[u8], sig: &[u8]) -> bool {
    let vk = match VerifyingKey::from_sec1_bytes(pubkey) {
        Ok(v) => v,
        Err(_) => return false,
    };
    let s = match Signature::from_slice(sig) {
        Ok(s) => s,
        Err(_) => return false,
    };
    vk.verify_prehash(&sha256(&[msg]), &s).is_ok()
}

pub fn random_scalar_bytes(rng: Rng) -> Option<[u8; 32]> {
    for _ in 0..16 {
        let mut b = [0u8; 32];
        rng(&mut b);
        let sc: Option<Scalar> = Option::from(Scalar::from_repr(b.into()));
        if matches!(sc, Some(x) if !bool::from(x.is_zero())) {
            return Some(b);
        }
        b.zeroize();
    }
    None
}

pub fn ecdsa_sign(privkey: &[u8], msg: &[u8], rng: Rng) -> Option<[u8; 64]> {
    let d: Scalar = Option::from(Scalar::from_repr(*GenericArray::from_slice(privkey.get(..32)?)))?;
    let z = <Scalar as Reduce<U256>>::reduce_bytes(&sha256(&[msg]).into());
    for _ in 0..16 {
        let mut kb = random_scalar_bytes(rng)?;
        let k: Option<Scalar> = Option::from(Scalar::from_repr(kb.into()));
        kb.zeroize();
        let k = k?;
        let r = <Scalar as Reduce<U256>>::reduce_bytes(&(ProjectivePoint::GENERATOR * k).to_affine().x());
        let ki: Option<Scalar> = Option::from(k.invert());
        let s = ki? * (z + r * d);
        if bool::from(r.is_zero()) || bool::from(s.is_zero()) {
            continue;
        }
        let mut out = [0u8; 64];
        out[..32].copy_from_slice(&r.to_bytes());
        out[32..].copy_from_slice(&s.to_bytes());
        return Some(out);
    }
    None
}

pub fn pub_from_priv(privkey: &[u8]) -> Option<[u8; PUB_LEN]> {
    let sk = SecretKey::from_slice(privkey).ok()?;
    let ep = sk.public_key().to_encoded_point(false);
    let mut out = [0u8; PUB_LEN];
    out.copy_from_slice(ep.as_bytes().get(..PUB_LEN)?);
    Some(out)
}

pub fn ecdh(privkey: &[u8], peer_pub: &[u8]) -> Option<[u8; 32]> {
    let sk = SecretKey::from_slice(privkey).ok()?;
    let pk = PublicKey::from_sec1_bytes(peer_pub).ok()?;
    let ss = p256::ecdh::diffie_hellman(sk.to_nonzero_scalar(), pk.as_affine());
    let mut out = [0u8; 32];
    out.copy_from_slice(ss.raw_secret_bytes().as_slice());
    Some(out)
}

#[derive(Clone, Copy, Default)]
pub struct Cert<'a> {
    pub serial: Elem<'a>,
    pub issuer: Elem<'a>,
    pub subject: Elem<'a>,
    pub exts: Elem<'a>,
    pub not_before: u32,
    pub not_after: u32,
    pub pub_: &'a [u8],
    pub sig: &'a [u8],
    pub is_ca: bool,
    pub has_node: bool,
    pub has_fabric: bool,
    pub node_id: u64,
    pub fabric_id: u64,
}

impl<'a> Cert<'a> {
    pub fn parse(tlv: &'a [u8]) -> Option<Cert<'a>> {
        let mut c = Cert::default();
        let mut r = Reader::new(tlv);
        let top = r.next()?;
        if top.typ != T_STRUCT || top.tag != -1 || !r.at_end() {
            return None;
        }
        let mut got = [false; 12];
        let (mut sigalgo, mut pubalgo, mut curve) = (0u8, 0u8, 0u8);
        let mut r = Reader::new(top.d);
        while let Some(e) = r.next() {
            if e.tag < 1 || e.tag > 11 {
                continue;
            }
            got[e.tag as usize] = true;
            match e.tag {
                1 => c.serial = e,
                2 => sigalgo = e.u as u8,
                3 => c.issuer = e,
                4 => c.not_before = e.u as u32,
                5 => c.not_after = e.u as u32,
                6 => c.subject = e,
                7 => pubalgo = e.u as u8,
                8 => curve = e.u as u8,
                9 => {
                    if e.d.len() != PUB_LEN || e.d[0] != 4 {
                        return None;
                    }
                    c.pub_ = e.d;
                }
                10 => c.exts = e,
                _ => {
                    if e.d.len() != SIG_LEN {
                        return None;
                    }
                    c.sig = e.d;
                }
            }
        }
        if r.bad || got[1..].iter().any(|g| !g) {
            return None;
        }
        if c.serial.typ != T_BYTES
            || c.serial.d.is_empty()
            || c.serial.d.len() > 20
            || sigalgo != 1
            || pubalgo != 1
            || curve != 1
            || c.issuer.typ != T_LIST
            || c.subject.typ != T_LIST
            || c.exts.typ != T_LIST
        {
            return None;
        }
        let mut r = Reader::new(c.subject.d);
        while let Some(e) = r.next() {
            if e.tag >= 0 && (e.tag & 0x7F) == 17 && e.typ == T_UINT {
                c.has_node = true;
                c.node_id = e.u;
            } else if e.tag >= 0 && (e.tag & 0x7F) == 21 && e.typ == T_UINT {
                c.has_fabric = true;
                c.fabric_id = e.u;
            }
        }
        if r.bad {
            return None;
        }
        let mut r = Reader::new(c.exts.d);
        while let Some(e) = r.next() {
            if e.tag == 1 && e.typ == T_STRUCT {
                c.is_ca = matches!(find(e.d, 1), Some(ca) if ca.typ == T_BOOL && ca.u != 0);
            }
        }
        if r.bad {
            return None;
        }
        Some(c)
    }

    pub fn tbs_der(&self, out: &mut [u8]) -> Option<usize> {
        let cap = out.len();
        let mut d = Der { b: out, pos: cap, err: false };
        let m0 = d.mark();
        let mut el = [Elem::default(); 8];
        let mut n = 0;
        let mut r = Reader::new(self.exts.d);
        while n < 8 {
            match r.next() {
                Some(e) => {
                    el[n] = e;
                    n += 1;
                }
                None => break,
            }
        }
        if r.bad || !r.at_end() {
            return None;
        }
        let me = d.mark();
        for e in el[..n].iter().rev() {
            d.ext(e);
        }
        d.wrap(0x30, me);
        d.wrap(0xA3, me);
        let ms = d.mark();
        let mk = d.mark();
        d.raw(self.pub_);
        d.byte(0);
        d.wrap(0x03, mk);
        let ma = d.mark();
        d.tlv(0x06, &OID_P256);
        d.tlv(0x06, &OID_EC);
        d.wrap(0x30, ma);
        d.wrap(0x30, ms);
        d.dn(&self.subject);
        let mv = d.mark();
        d.time(self.not_after);
        d.time(self.not_before);
        d.wrap(0x30, mv);
        d.dn(&self.issuer);
        let mg = d.mark();
        d.tlv(0x06, &OID_SIG);
        d.wrap(0x30, mg);
        d.tlv(0x02, self.serial.d);
        d.raw(&[0xA0, 0x03, 0x02, 0x01, 0x02]);
        d.wrap(0x30, m0);
        if d.err {
            return None;
        }
        let pos = d.pos;
        out.copy_within(pos..cap, 0);
        Some(cap - pos)
    }

    pub fn verify(&self, issuer_pub: &[u8]) -> bool {
        let mut tbs = [0u8; 640];
        match self.tbs_der(&mut tbs) {
            Some(n) => ecdsa_verify(issuer_pub, &tbs[..n], self.sig),
            None => false,
        }
    }
}

const OID_SIG: [u8; 8] = [0x2A, 0x86, 0x48, 0xCE, 0x3D, 0x04, 0x03, 0x02];
const OID_EC: [u8; 7] = [0x2A, 0x86, 0x48, 0xCE, 0x3D, 0x02, 0x01];
const OID_P256: [u8; 8] = [0x2A, 0x86, 0x48, 0xCE, 0x3D, 0x03, 0x01, 0x07];
const OID_EXT: [[u8; 3]; 6] = [[0, 0, 0], [0x55, 0x1D, 0x13], [0x55, 0x1D, 0x0F], [0x55, 0x1D, 0x25], [0x55, 0x1D, 0x0E], [0x55, 0x1D, 0x23]];
const OID_DC: [u8; 10] = [0x09, 0x92, 0x26, 0x89, 0x93, 0xF2, 0x2C, 0x64, 0x01, 0x19];
const OID_PURPOSE: [u8; 8] = [0x2B, 0x06, 0x01, 0x05, 0x05, 0x07, 0x03, 0];
const PURPOSE_LAST: [u8; 7] = [0, 1, 2, 3, 4, 8, 9];

struct Der<'a> {
    b: &'a mut [u8],
    pos: usize,
    err: bool,
}

impl<'a> Der<'a> {
    fn raw(&mut self, p: &[u8]) {
        if p.len() > self.pos {
            self.err = true;
            return;
        }
        self.pos -= p.len();
        let pos = self.pos;
        self.b[pos..pos + p.len()].copy_from_slice(p);
    }

    fn byte(&mut self, v: u8) {
        self.raw(&[v]);
    }

    fn mark(&self) -> usize {
        self.b.len() - self.pos
    }

    fn wrap(&mut self, tag: u8, mark: usize) {
        let n = self.mark() - mark;
        if n < 128 {
            self.byte(n as u8);
        } else if n < 256 {
            self.byte(n as u8);
            self.byte(0x81);
        } else {
            self.byte((n & 255) as u8);
            self.byte((n >> 8) as u8);
            self.byte(0x82);
        }
        self.byte(tag);
    }

    fn tlv(&mut self, tag: u8, p: &[u8]) {
        let m = self.mark();
        self.raw(p);
        self.wrap(tag, m);
    }

    fn dn(&mut self, list: &Elem) {
        let mut el = [Elem::default(); 16];
        let mut n = 0;
        let mut r = Reader::new(list.d);
        while n < 16 {
            match r.next() {
                Some(e) => {
                    el[n] = e;
                    n += 1;
                }
                None => break,
            }
        }
        if r.bad || !r.at_end() {
            self.err = true;
            return;
        }
        let m0 = self.mark();
        for e in el[..n].iter().rev() {
            let id = (e.tag & 0x7F) as u8;
            let mut oid = [0u8; 12];
            let ol;
            if e.tag < 0 {
                self.err = true;
                return;
            }
            if (1..=15).contains(&id) {
                const AT: [u8; 16] = [0, 3, 4, 5, 6, 7, 8, 10, 11, 12, 41, 42, 43, 44, 46, 65];
                oid[..3].copy_from_slice(&[0x55, 0x04, AT[id as usize]]);
                ol = 3;
            } else if id == 16 {
                oid[..10].copy_from_slice(&OID_DC);
                ol = 10;
            } else if (17..=22).contains(&id) {
                oid[..9].copy_from_slice(&[0x2B, 0x06, 0x01, 0x04, 0x01, 0x82, 0xA2, 0x7C, 0x01]);
                oid[9] = id - 16;
                ol = 10;
            } else {
                self.err = true;
                return;
            }
            let ms = self.mark();
            if (17..=22).contains(&id) {
                let digits = if id == 22 { 8 } else { 16 };
                let mut s = [0u8; 16];
                if e.typ != T_UINT {
                    self.err = true;
                }
                for (i, c) in s.iter_mut().take(digits).enumerate() {
                    *c = b"0123456789ABCDEF"[((e.u >> (4 * (digits - 1 - i))) & 15) as usize];
                }
                self.tlv(0x0C, &s[..digits]);
            } else {
                if e.typ != T_UTF8 {
                    self.err = true;
                }
                let t = if id == 16 { 0x16 } else if e.tag & 0x80 != 0 { 0x13 } else { 0x0C };
                self.tlv(t, e.d);
            }
            self.tlv(0x06, &oid[..ol]);
            self.wrap(0x30, ms);
            self.wrap(0x31, ms);
        }
        self.wrap(0x30, m0);
    }

    fn time(&mut self, secs: u32) {
        let (mut y, mut mo, mut dd, mut hh, mut mi, mut ss) = (9999i64, 12i64, 31i64, 23i64, 59i64, 59i64);
        if secs != 0 {
            let z = (secs / 86400) as i64 + 10957 + 719468;
            let era = z / 146097;
            let doe = z - era * 146097;
            let yoe = (doe - doe / 1460 + doe / 36524 - doe / 146096) / 365;
            let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
            let mp = (5 * doy + 2) / 153;
            dd = doy - (153 * mp + 2) / 5 + 1;
            mo = if mp < 10 { mp + 3 } else { mp - 9 };
            y = yoe + era * 400 + (mo <= 2) as i64;
            let t = (secs % 86400) as i64;
            hh = t / 3600;
            mi = t % 3600 / 60;
            ss = t % 60;
        }
        let vals = [y, mo, dd, hh, mi, ss];
        let gen = y >= 2050;
        let mut s = [0u8; 16];
        let mut o = 0;
        for (i, v) in vals.iter().enumerate() {
            let w = if i == 0 && gen { 4 } else { 2 };
            let v = if i == 0 && !gen { y % 100 } else { *v };
            let mut p = 10i64.pow(w - 1);
            while p > 0 {
                s[o] = b'0' + ((v / p) % 10) as u8;
                o += 1;
                p /= 10;
            }
        }
        s[o] = b'Z';
        o += 1;
        self.tlv(if gen { 0x18 } else { 0x17 }, &s[..o]);
    }

    fn ext(&mut self, e: &Elem) {
        let m0 = self.mark();
        if e.tag < 1 || e.tag > 5 {
            self.err = true;
            return;
        }
        let m1 = self.mark();
        match e.tag {
            1 => {
                let pl = if e.typ == T_STRUCT { find(e.d, 2).filter(|p| p.typ == T_UINT) } else { None };
                let is_ca = e.typ == T_STRUCT && matches!(find(e.d, 1), Some(ca) if ca.typ == T_BOOL && ca.u != 0);
                let ms = self.mark();
                if let Some(p) = pl {
                    if p.u > 127 {
                        self.err = true;
                    }
                    self.tlv(0x02, &[p.u as u8]);
                }
                if is_ca {
                    self.tlv(0x01, &[0xFF]);
                }
                self.wrap(0x30, ms);
            }
            2 => {
                let v = e.u as u16;
                let mut rev = [0u8; 2];
                for (i, r) in rev.iter_mut().enumerate() {
                    *r = ((v >> (8 * i)) as u8).reverse_bits();
                }
                let n = if (v >> 8) != 0 { 2 } else { 1 };
                let mut unused = 0u8;
                if v != 0 {
                    while unused < 8 && (rev[n - 1] >> unused) & 1 == 0 {
                        unused += 1;
                    }
                }
                let b = [unused, rev[0], rev[1]];
                if e.typ != T_UINT {
                    self.err = true;
                }
                self.tlv(0x03, &b[..n + 1]);
            }
            3 => {
                let mut el = [Elem::default(); 8];
                let mut n = 0;
                let mut r = Reader::new(e.d);
                while n < 8 {
                    match r.next() {
                        Some(x) => {
                            el[n] = x;
                            n += 1;
                        }
                        None => break,
                    }
                }
                let ms = self.mark();
                for x in el[..n].iter().rev() {
                    let mut o = OID_PURPOSE;
                    match PURPOSE_LAST.get(x.u as usize).filter(|_| x.u >= 1 && x.u <= 6) {
                        Some(l) => o[7] = *l,
                        None => self.err = true,
                    }
                    self.tlv(0x06, &o);
                }
                self.wrap(0x30, ms);
            }
            4 => self.tlv(0x04, e.d),
            _ => {
                let ms = self.mark();
                self.tlv(0x80, e.d);
                self.wrap(0x30, ms);
            }
        }
        self.wrap(0x04, m1);
        if e.tag <= 3 {
            self.tlv(0x01, &[0xFF]);
        }
        self.tlv(0x06, &OID_EXT[e.tag as usize]);
        self.wrap(0x30, m0);
    }
}
