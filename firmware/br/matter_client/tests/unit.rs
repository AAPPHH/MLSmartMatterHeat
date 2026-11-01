mod common;
use common::*;
use matter_client::client::*;
use matter_client::crypto::*;
use matter_client::tlv::*;

#[test]
fn tlv_roundtrip() {
    let mut b = [0u8; 64];
    let mut w = Writer::new(&mut b);
    w.start(-1, T_STRUCT);
    w.uint(0, 42);
    w.sint(1, -17);
    w.end();
    let exp = [0x15, 0x24, 0x00, 0x2a, 0x20, 0x01, 0xef, 0x18];
    assert!(!w.err && w.len == exp.len());
    assert_eq!(&b[..exp.len()], &exp);
    let mut r = Reader::new(&b[..exp.len()]);
    let e = r.next().unwrap();
    assert!(e.typ == T_STRUCT && e.tag == -1 && e.d.len() == 6 && r.at_end());
    assert_eq!(find(e.d, 0).unwrap().u, 42);
    assert_eq!(find(e.d, 1).unwrap().s, -17);
    assert!(find(e.d, 2).is_none());
    let mut b = [0u8; 64];
    let mut w = Writer::new(&mut b);
    w.sint(4, 1800);
    w.uint(5, 0x1234567890);
    w.boolean(6, true);
    w.bytes(7, b"abc");
    let wl = w.len;
    let exp2 = [0x21, 0x04, 0x08, 0x07, 0x27, 0x05, 0x90, 0x78, 0x56, 0x34, 0x12, 0x00, 0x00, 0x00, 0x29, 0x06, 0x30, 0x07, 0x03, b'a', b'b', b'c'];
    assert_eq!(&b[..wl], &exp2);
    let mut r = Reader::new(&b[..wl]);
    let e = r.next().unwrap();
    assert!(e.typ == T_SINT && e.s == 1800 && e.tag == 4);
    assert_eq!(r.next().unwrap().u, 0x1234567890);
    let e = r.next().unwrap();
    assert!(e.typ == T_BOOL && e.u == 1);
    assert_eq!(r.next().unwrap().d, b"abc");
    assert!(r.next().is_none() && r.at_end());
    let mut r = Reader::new(&[0x15, 0x24, 0x00, 0x2a]);
    assert!(r.next().is_none() && r.bad);
    let nest = [0x15, 0x35, 0x01, 0x24, 0x02, 0x07, 0x18, 0x24, 0x03, 0x09, 0x18];
    let mut r = Reader::new(&nest);
    let e = r.next().unwrap();
    assert_eq!(e.d.len(), nest.len() - 2);
    assert_eq!(find(e.d, 3).unwrap().u, 9);
    let i = find(e.d, 1).unwrap();
    assert!(i.typ == T_STRUCT && i.d.len() == 3);
}

#[test]
fn headers() {
    let mut b = [0u8; 32];
    let h = Hdr { sid: 0xA1B2, ctr: 0x12345678, has_src: true, src: 0x1122334455667788, ..Default::default() };
    let n = hdr_encode(&h, &mut b).unwrap();
    let exp = [0x04, 0xb2, 0xa1, 0x00, 0x78, 0x56, 0x34, 0x12, 0x88, 0x77, 0x66, 0x55, 0x44, 0x33, 0x22, 0x11];
    assert_eq!(&b[..n], &exp);
    let d = hdr_decode(&b[..n]).unwrap();
    assert!(d.len == 16 && d.sid == 0xA1B2 && d.ctr == 0x12345678 && d.has_src && d.src == h.src && !d.has_dst);
    let h = Hdr { has_dst: true, dst: 7, ..h };
    let h = Hdr { has_src: false, ..h };
    let n = hdr_encode(&h, &mut b).unwrap();
    assert!(n == 16 && b[0] == 1);
    let d = hdr_decode(&b[..n]).unwrap();
    assert!(d.has_dst && d.dst == 7);
    b[0] = 0x10;
    assert!(hdr_decode(&b[..n]).is_none());
    b[0] = 0x03;
    assert!(hdr_decode(&b[..n]).is_none());
    b[0] = 0;
    b[3] = 0x80;
    assert!(hdr_decode(&b[..n]).is_none());
    assert!(hdr_decode(&b[..7]).is_none());
    let p = PHdr { xflags: XF_I | XF_R | XF_A, opcode: 0x30, xid: 0xBEEF, proto: 0, ack: 0xCAFEBABE, len: 0 };
    let n = phdr_encode(&p, &mut b).unwrap();
    assert_eq!(&b[..n], &[0x07, 0x30, 0xef, 0xbe, 0x00, 0x00, 0xbe, 0xba, 0xfe, 0xca]);
    let q = phdr_decode(&b[..n]).unwrap();
    assert!(q.len == 10 && q.xflags == 7 && q.opcode == 0x30 && q.xid == 0xBEEF && q.proto == 0 && q.ack == 0xCAFEBABE);
    assert!(phdr_decode(&[0x10, 0x01, 0x01, 0x00, 0x34, 0x12, 0x01, 0x00]).is_none());
    assert_eq!(nonce(0, 0x11223344, 0x8877665544332211), [0x00, 0x44, 0x33, 0x22, 0x11, 0x11, 0x22, 0x33, 0x44, 0x55, 0x66, 0x77, 0x88]);
}

#[test]
fn counter_window() {
    let mut w = Window::default();
    assert!(!w.seen(100));
    assert!(w.seen(100));
    assert!(!w.seen(101));
    assert!(!w.seen(99));
    assert!(w.seen(99));
    assert!(w.seen(101));
    assert!(!w.seen(110));
    assert!(w.seen(101));
    assert!(!w.seen(102));
    assert!(w.seen(102));
    assert!(!w.seen(110 + 32));
    assert!(w.seen(110));
    assert!(!w.seen(110 + 32 - 31));
    assert!(w.seen(110 + 32 - 31));
    assert!(w.seen(110 + 32 - 33));
    assert!(!w.seen(10_000));
    assert!(w.seen(101));
    assert!(w.seen(142));
}

#[test]
fn crypto_vectors() {
    let ikm = [0x0bu8; 22];
    let salt: Vec<u8> = (0..13).collect();
    let info: Vec<u8> = (0..10).map(|i| 0xf0 + i).collect();
    let mut okm = [0u8; 42];
    assert!(hkdf(&salt, &ikm, &info, &mut okm));
    assert_eq!(okm.to_vec(), hex("3cb25f25faacd57a90434f64d0362f2a2d2d0a90cf1a5a4c5db02d56ecc4c5bf34007208d5b887185865"));
    assert!(hkdf(&[], &ikm, &[], &mut okm));
    assert_eq!(okm.to_vec(), hex("8da4e775a563c18f715f802a063c5a31b8a11f5c5ee1879ec3454e5f3c738d2d9d201395faa4b61a96c8"));
    let root = hex("044a9f42b1ca4840d37292bbc7f6a7e11e22200c976fc900dbc98a7a383a641cb8254a2e56d4e295a847943b4e3897c4a773e930277b4d9fbede8a052686bfacfa");
    let cf = compressed_fabric_id(&root, 0x2906C908D115D362).unwrap();
    assert_eq!(cf.to_vec(), hex("87e1b004e235a130"));
    let ipk = ipk_operational(&hex("235bf7e62823d358dca4ba50b1535f4b"), &cf).unwrap();
    assert_eq!(ipk.to_vec(), hex("a6f5306baf6d050af23ba4bd6b9dd960"));
    let opk = hex("9bc61cd9c62a2df6d64dfcaa9dc472d4");
    let rnd: [u8; 32] = hex("7e171231568dfa17206b3accf8faec2f4d21b580113196f47c7c4deb810a73dc").try_into().unwrap();
    let want = hex("dc35dd5fc9134cc5544538c9c3fc4297c1ec3370c839136a80e10796451d4c53");
    assert_eq!(dest_id(&opk, &rnd, &root, 0x2906C908D115D362, 0xCD5544AA7B13EF14).unwrap().to_vec(), want);
    assert_ne!(dest_id(&opk, &rnd, &root, 0x2906C908D115D362, 0xCD5544AA7B13EF15).unwrap().to_vec(), want);
    let key = [0u8; 16];
    let nc = [1u8, 2, 3, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0];
    let aad = [9u8, 8, 7, 0, 0, 0, 0, 0];
    let mut buf = [0x5au8; 64];
    assert!(ccm_seal(&key, &nc, &aad, &mut buf, 48));
    assert_ne!(&buf[..48], &[0x5au8; 48][..]);
    let mut t = buf;
    assert!(ccm_open(&key, &nc, &aad, &mut buf, 64));
    assert_eq!(&buf[..48], &[0x5au8; 48][..]);
    t[63] ^= 1;
    assert!(!ccm_open(&key, &nc, &aad, &mut t, 64));
    assert!(!ccm_open(&key, &nc, &aad, &mut t, 10));
    assert!(ct_eq(&key, &key) && !ct_eq(&key, &nc));
    let v = vectors();
    let c = Cert::parse(&v["V_Node01_01_Chip"]).unwrap();
    let mut privk = v["V_Node01_01_PrivateKey"].clone();
    assert_eq!(pub_from_priv(&privk).unwrap().to_vec(), c.pub_.to_vec());
    privk[3] ^= 1;
    assert_ne!(pub_from_priv(&privk).unwrap().to_vec(), c.pub_.to_vec());
    privk[3] ^= 1;
    let mut rng = lcg_rng(7);
    let sig = ecdsa_sign(&privk, b"hello", &mut *rng).unwrap();
    assert!(ecdsa_verify(c.pub_, b"hello", &sig));
    assert!(!ecdsa_verify(c.pub_, b"hellp", &sig));
}

fn tbs_slice(der: &[u8]) -> &[u8] {
    let o = if der[1] & 0x80 != 0 { 2 + (der[1] & 0x7f) as usize } else { 2 };
    let l = der[o + 1] as usize;
    let (h, l) = if l & 0x80 != 0 {
        let nb = l & 0x7f;
        (2 + nb, if nb == 1 { der[o + 2] as usize } else { (der[o + 2] as usize) << 8 | der[o + 3] as usize })
    } else {
        (2, l)
    };
    &der[o..o + h + l]
}

fn check_cert(tlv: &[u8], der: &[u8], issuer_pub: Option<&[u8]>) {
    let c = Cert::parse(tlv).unwrap();
    let mut tbs = [0u8; 640];
    let n = c.tbs_der(&mut tbs).unwrap();
    assert_eq!(&tbs[..n], tbs_slice(der));
    assert!(c.verify(issuer_pub.unwrap_or(c.pub_)));
}

#[test]
fn certs() {
    let v = vectors();
    let root = Cert::parse(&v["V_Root01_Chip"]).unwrap();
    let ica = Cert::parse(&v["V_ICA01_Chip"]).unwrap();
    let node = Cert::parse(&v["V_Node01_01_Chip"]).unwrap();
    let node2 = Cert::parse(&v["V_Node01_02_Chip"]).unwrap();
    check_cert(&v["V_Root01_Chip"], &v["V_Root01_DER"], None);
    check_cert(&v["V_ICA01_Chip"], &v["V_ICA01_DER"], Some(root.pub_));
    check_cert(&v["V_Node01_01_Chip"], &v["V_Node01_01_DER"], Some(ica.pub_));
    check_cert(&v["V_Node01_02_Chip"], &v["V_Node01_02_DER"], Some(root.pub_));
    assert!(node.has_node && node.node_id == 0xDEDEDEDE00010001 && node.has_fabric && node.fabric_id == 0xFAB000000000001D);
    assert!(node2.node_id == 0xDEDEDEDE00010002 && !node.is_ca && ica.is_ca && root.is_ca);
    assert!(!node.verify(root.pub_));
    assert!(!node.verify(node2.pub_));
    let mut bad = v["V_Node01_01_Chip"].clone();
    bad[40] ^= 1;
    if let Some(b) = Cert::parse(&bad) {
        assert!(!b.verify(ica.pub_));
    }
    assert!(Cert::parse(&v["V_Node01_01_Chip"][..100]).is_none());
    assert!(Cert::parse(&v["V_Node01_01_DER"]).is_none());
}

#[test]
fn mrp() {
    let m = Mrp::from_txt(0, 0);
    assert!(m.idle_ms == 3000 && m.active_ms == 1500 && m.max_tx == 5);
    let m = Mrp::from_txt(100, 99999);
    assert!(m.idle_ms == 2000 && m.active_ms == 10000);
    assert_eq!(backoff_ms(300, 0, 60000, 0), 330);
    assert_eq!(backoff_ms(300, 1, 60000, 0), 330);
    assert_eq!(backoff_ms(300, 2, 60000, 0), 330 * 16 / 10);
    assert_eq!(backoff_ms(300, 9, 2000, 255), 2000);
}

struct XorShift(u64);

impl XorShift {
    fn next(&mut self) -> u64 {
        self.0 ^= self.0 << 13;
        self.0 ^= self.0 >> 7;
        self.0 ^= self.0 << 17;
        self.0
    }
}

#[test]
fn fuzz_parsers_do_not_panic() {
    let v = vectors();
    let seeds: Vec<Vec<u8>> = ["V_Root01_Chip", "V_ICA01_Chip", "V_Node01_01_Chip", "V_Node01_02_Chip"].iter().map(|k| v[*k].clone()).collect();
    let mut x = XorShift(0x9e3779b97f4a7c15);
    for _ in 0..20000 {
        let mut b = seeds[(x.next() % 4) as usize].clone();
        for _ in 0..(1 + x.next() % 4) {
            let i = (x.next() as usize) % b.len();
            match x.next() % 4 {
                0 => b[i] ^= 1 << (x.next() % 8),
                1 => b[i] = x.next() as u8,
                2 => b.truncate(i.max(1)),
                _ => b.insert(i, x.next() as u8),
            }
        }
        if let Some(c) = Cert::parse(&b) {
            let mut t = [0u8; 640];
            let _ = c.tbs_der(&mut t);
            let _ = c.verify(c.pub_);
        }
        let mut r = Reader::new(&b);
        while r.next().is_some() {}
        let _ = find(&b, 1);
        let _ = hdr_decode(&b);
        let _ = phdr_decode(&b);
        let mut small = [0u8; 40];
        let _ = Cert::parse(&b).map(|c| c.tbs_der(&mut small));
    }
    for n in 0..64 {
        let mut b = vec![0u8; n];
        for _ in 0..500 {
            for y in b.iter_mut() {
                *y = x.next() as u8;
            }
            let mut r = Reader::new(&b);
            while r.next().is_some() {}
            let _ = hdr_decode(&b);
            let _ = phdr_decode(&b);
        }
    }
}

struct Garbage {
    x: XorShift,
    t: u32,
    mode: u8,
    saved: Vec<u8>,
}

impl Io for Garbage {
    fn send(&mut self, b: &[u8]) -> i32 {
        self.saved = b.to_vec();
        b.len() as i32
    }

    fn recv(&mut self, b: &mut [u8], ms: u32) -> i32 {
        self.t += ms.min(50);
        let n = match self.mode {
            0 => (self.x.next() % 200) as usize,
            _ => self.saved.len().min(b.len()),
        };
        for (i, y) in b.iter_mut().take(n).enumerate() {
            *y = if self.mode == 0 { self.x.next() as u8 } else { self.saved[i] ^ ((self.x.next() % 7 == 0) as u8) << (self.x.next() % 8) };
        }
        n as i32
    }

    fn now_ms(&mut self) -> u32 {
        self.t += 1;
        self.t
    }

    fn rng(&mut self, b: &mut [u8]) {
        for y in b.iter_mut() {
            *y = self.x.next() as u8;
        }
    }
}

#[test]
fn fuzz_client_with_garbage_peer() {
    let f = Fixture::new();
    let cr = f.creds();
    for mode in 0..2u8 {
        for seed in 1..12u64 {
            let mut io = Garbage { x: XorShift(seed * 7919), t: 0, mode, saved: vec![] };
            let mut s = Scratch::new();
            let mut e = ErrBuf::new();
            let rc = write_int16(&mut io, &mut s, &cr, 0xDEDEDEDE00010002, &Mrp { idle_ms: 300, active_ms: 200, max_ms: 2000, max_tx: 5, resp_ms: 3000 }, 1, 0x201, 0x12, 1800, &mut e);
            assert!(rc != OK, "garbage peer must not succeed");
        }
    }
}

#[test]
fn creds_errors() {
    let f = Fixture::new();
    let mut cr = f.creds();
    let mut io = Garbage { x: XorShift(1), t: 0, mode: 0, saved: vec![] };
    let mut s = Scratch::new();
    let mut e = ErrBuf::new();
    let m = Mrp::from_txt(0, 0);
    let mut bad = f.key;
    bad[70] ^= 1;
    cr.key = &bad;
    assert_eq!(write_int16(&mut io, &mut s, &cr, 1, &m, 1, 0x201, 0x12, 0, &mut e), E_KEY);
    assert_eq!(e.as_bytes(), b"key_mismatch");
}
