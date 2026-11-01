pub const T_UINT: u8 = 1;
pub const T_SINT: u8 = 2;
pub const T_BOOL: u8 = 3;
pub const T_BYTES: u8 = 4;
pub const T_UTF8: u8 = 5;
pub const T_STRUCT: u8 = 6;
pub const T_ARRAY: u8 = 7;
pub const T_LIST: u8 = 8;
pub const T_NULL: u8 = 9;
pub const T_FLOAT: u8 = 10;

pub const XF_I: u8 = 0x01;
pub const XF_A: u8 = 0x02;
pub const XF_R: u8 = 0x04;
pub const XF_SX: u8 = 0x08;
pub const XF_V: u8 = 0x10;

pub fn put_le(o: &mut [u8], v: u64) {
    for (i, b) in o.iter_mut().enumerate() {
        *b = (v >> (8 * i)) as u8;
    }
}

pub fn get_le(p: &[u8]) -> u64 {
    let mut v = 0u64;
    for (i, b) in p.iter().take(8).enumerate() {
        v |= (*b as u64) << (8 * i);
    }
    v
}

pub struct Writer<'a> {
    b: &'a mut [u8],
    pub len: usize,
    pub err: bool,
}

impl<'a> Writer<'a> {
    pub fn new(b: &'a mut [u8]) -> Self {
        Writer { b, len: 0, err: false }
    }

    fn put(&mut self, d: &[u8]) {
        let end = self.len.checked_add(d.len());
        match end.and_then(|e| self.b.get_mut(self.len..e)) {
            Some(dst) if !self.err => {
                dst.copy_from_slice(d);
                self.len += d.len();
            }
            _ => self.err = true,
        }
    }

    fn ctl(&mut self, tag: i32, t: u8) {
        if tag >= 0 {
            self.put(&[0x20 | t, tag as u8]);
        } else {
            self.put(&[t]);
        }
    }

    pub fn start(&mut self, tag: i32, t: u8) {
        let c = match t {
            T_STRUCT => 0x15,
            T_ARRAY => 0x16,
            _ => 0x17,
        };
        self.ctl(tag, c);
    }

    pub fn end(&mut self) {
        self.put(&[0x18]);
    }

    pub fn uint(&mut self, tag: i32, v: u64) {
        let sz = if v <= 0xFF { 0 } else if v <= 0xFFFF { 1 } else if v <= 0xFFFF_FFFF { 2 } else { 3 };
        let mut b = [0u8; 8];
        self.ctl(tag, 4 + sz);
        put_le(&mut b[..1 << sz], v);
        self.put(&b[..1 << sz]);
    }

    pub fn sint(&mut self, tag: i32, v: i64) {
        let sz = if (-128..=127).contains(&v) {
            0
        } else if (-32768..=32767).contains(&v) {
            1
        } else if (i32::MIN as i64..=i32::MAX as i64).contains(&v) {
            2
        } else {
            3
        };
        let mut b = [0u8; 8];
        self.ctl(tag, sz);
        put_le(&mut b[..1 << sz], v as u64);
        self.put(&b[..1 << sz]);
    }

    pub fn boolean(&mut self, tag: i32, v: bool) {
        self.ctl(tag, if v { 9 } else { 8 });
    }

    pub fn bytes(&mut self, tag: i32, d: &[u8]) {
        let sz = if d.len() <= 0xFF { 0 } else { 1 };
        let mut b = [0u8; 2];
        self.ctl(tag, 16 + sz);
        put_le(&mut b[..1 << sz], d.len() as u64);
        self.put(&b[..1 << sz]);
        self.put(d);
    }
}

#[derive(Clone, Copy, Default)]
pub struct Elem<'a> {
    pub typ: u8,
    pub tag: i32,
    pub u: u64,
    pub s: i64,
    pub d: &'a [u8],
}

const TAGSZ: [usize; 8] = [0, 1, 2, 4, 2, 4, 6, 8];

fn step(b: &[u8], pos: &mut usize, depth: &mut i32) -> bool {
    let mut p = *pos;
    let ctl = match b.get(p) {
        Some(c) => *c,
        None => return false,
    };
    p += 1;
    let typ = ctl & 31;
    let ts = TAGSZ[(ctl >> 5) as usize];
    if b.len() - p < ts {
        return false;
    }
    p += ts;
    let mut vl = 0usize;
    if typ <= 7 {
        vl = 1 << (typ & 3);
    } else if typ == 10 {
        vl = 4;
    } else if typ == 11 {
        vl = 8;
    } else if (12..=19).contains(&typ) {
        let ls = 1usize << (typ & 3);
        if b.len() - p < ls || ls > 2 {
            return false;
        }
        vl = ls + get_le(&b[p..p + ls]) as usize;
    } else if typ == 24 {
        *depth -= 1;
        if *depth < 0 {
            return false;
        }
    } else if (21..=23).contains(&typ) {
        *depth += 1;
    } else if typ != 8 && typ != 9 && typ != 20 {
        return false;
    }
    if b.len() - p < vl {
        return false;
    }
    *pos = p + vl;
    true
}

#[derive(Clone, Copy)]
pub struct Reader<'a> {
    b: &'a [u8],
    pub pos: usize,
    pub bad: bool,
}

impl<'a> Reader<'a> {
    pub fn new(b: &'a [u8]) -> Self {
        Reader { b, pos: 0, bad: false }
    }

    pub fn at_end(&self) -> bool {
        !self.bad && self.pos == self.b.len()
    }

    pub fn next(&mut self) -> Option<Elem<'a>> {
        if self.bad || self.pos >= self.b.len() {
            return None;
        }
        let r = self.parse();
        if r.is_none() {
            self.bad = true;
        }
        r
    }

    fn parse(&mut self) -> Option<Elem<'a>> {
        let b = self.b;
        let mut p = self.pos;
        let ctl = *b.get(p)?;
        p += 1;
        let typ = ctl & 31;
        let form = (ctl >> 5) as usize;
        let ts = TAGSZ[form];
        if b.len() - p < ts || typ == 24 {
            return None;
        }
        let mut e = Elem { tag: if form == 1 { b[p] as i32 } else if form == 0 { -1 } else { -2 }, ..Default::default() };
        p += ts;
        if typ <= 7 {
            let vl = 1usize << (typ & 3);
            if b.len() - p < vl {
                return None;
            }
            let v = get_le(&b[p..p + vl]);
            p += vl;
            if typ <= 3 {
                e.typ = T_SINT;
                let sh = 64 - 8 * vl as u32;
                e.s = ((v << sh) as i64) >> sh;
            } else {
                e.typ = T_UINT;
                e.u = v;
            }
        } else if typ == 8 || typ == 9 {
            e.typ = T_BOOL;
            e.u = (typ == 9) as u64;
        } else if typ == 10 || typ == 11 {
            e.typ = T_FLOAT;
            let vl = if typ == 10 { 4 } else { 8 };
            if b.len() - p < vl {
                return None;
            }
            p += vl;
        } else if (12..=19).contains(&typ) {
            let ls = 1usize << (typ & 3);
            if ls > 2 || b.len() - p < ls {
                return None;
            }
            let n = get_le(&b[p..p + ls]) as usize;
            p += ls;
            if b.len() - p < n {
                return None;
            }
            e.typ = if typ < 16 { T_UTF8 } else { T_BYTES };
            e.d = &b[p..p + n];
            p += n;
        } else if typ == 20 {
            e.typ = T_NULL;
        } else if (21..=23).contains(&typ) {
            let mut depth = 1;
            let mut q = p;
            let mut last = p;
            while depth > 0 {
                last = q;
                if !step(b, &mut q, &mut depth) {
                    return None;
                }
            }
            e.typ = match typ {
                21 => T_STRUCT,
                22 => T_ARRAY,
                _ => T_LIST,
            };
            e.d = &b[p..last];
            p = q;
        } else {
            return None;
        }
        self.pos = p;
        Some(e)
    }
}

pub fn find<'a>(b: &'a [u8], tag: i32) -> Option<Elem<'a>> {
    let mut r = Reader::new(b);
    while let Some(e) = r.next() {
        if e.tag == tag {
            return Some(e);
        }
    }
    None
}

#[derive(Clone, Copy, Default)]
pub struct Hdr {
    pub flags: u8,
    pub sflags: u8,
    pub sid: u16,
    pub ctr: u32,
    pub has_src: bool,
    pub has_dst: bool,
    pub src: u64,
    pub dst: u64,
    pub len: usize,
}

pub fn hdr_encode(h: &Hdr, out: &mut [u8]) -> Option<usize> {
    let n = 8 + if h.has_src { 8 } else { 0 } + if h.has_dst { 8 } else { 0 };
    let o = out.get_mut(..n)?;
    o[0] = (if h.has_src { 0x04 } else { 0 }) | (if h.has_dst { 0x01 } else { 0 });
    put_le(&mut o[1..3], h.sid as u64);
    o[3] = h.sflags;
    put_le(&mut o[4..8], h.ctr as u64);
    let mut p = 8;
    if h.has_src {
        put_le(&mut o[p..p + 8], h.src);
        p += 8;
    }
    if h.has_dst {
        put_le(&mut o[p..p + 8], h.dst);
    }
    Some(n)
}

pub fn hdr_decode(b: &[u8]) -> Option<Hdr> {
    if b.len() < 8 {
        return None;
    }
    let mut h = Hdr { flags: b[0], sflags: b[3], sid: get_le(&b[1..3]) as u16, ctr: get_le(&b[4..8]) as u32, ..Default::default() };
    if (b[0] >> 4) != 0 || (b[0] & 0x02) != 0 || (b[3] & 0xA0) != 0 || (b[3] & 0x03) != 0 {
        return None;
    }
    let mut o = 8;
    if b[0] & 0x04 != 0 {
        h.src = get_le(b.get(o..o + 8)?);
        h.has_src = true;
        o += 8;
    }
    match b[0] & 0x03 {
        1 => {
            h.dst = get_le(b.get(o..o + 8)?);
            h.has_dst = true;
            o += 8;
        }
        0 => {}
        _ => return None,
    }
    h.len = o;
    Some(h)
}

#[derive(Clone, Copy, Default)]
pub struct PHdr {
    pub xflags: u8,
    pub opcode: u8,
    pub xid: u16,
    pub proto: u16,
    pub ack: u32,
    pub len: usize,
}

pub fn phdr_encode(h: &PHdr, out: &mut [u8]) -> Option<usize> {
    let n = 6 + if h.xflags & XF_A != 0 { 4 } else { 0 };
    let o = out.get_mut(..n)?;
    o[0] = h.xflags & !(XF_V | XF_SX);
    o[1] = h.opcode;
    put_le(&mut o[2..4], h.xid as u64);
    put_le(&mut o[4..6], h.proto as u64);
    if h.xflags & XF_A != 0 {
        put_le(&mut o[6..10], h.ack as u64);
    }
    Some(n)
}

pub fn phdr_decode(b: &[u8]) -> Option<PHdr> {
    if b.len() < 6 || b[0] & (XF_SX | 0xE0) != 0 {
        return None;
    }
    let mut h = PHdr { xflags: b[0], opcode: b[1], xid: get_le(&b[2..4]) as u16, ..Default::default() };
    let mut o = 4;
    if b[0] & XF_V != 0 {
        if b.len() < o + 4 || get_le(&b[o..o + 2]) != 0 {
            return None;
        }
        o += 2;
    }
    h.proto = get_le(b.get(o..o + 2)?) as u16;
    o += 2;
    if b[0] & XF_A != 0 {
        h.ack = get_le(b.get(o..o + 4)?) as u32;
        o += 4;
    }
    h.len = o;
    Some(h)
}

pub fn nonce(sflags: u8, ctr: u32, node: u64) -> [u8; 13] {
    let mut o = [0u8; 13];
    o[0] = sflags;
    put_le(&mut o[1..5], ctr as u64);
    put_le(&mut o[5..13], node);
    o
}

#[derive(Clone, Copy, Default)]
pub struct Window {
    init: bool,
    max: u32,
    bits: u32,
}

impl Window {
    pub fn seen(&mut self, ctr: u32) -> bool {
        if !self.init {
            self.init = true;
            self.max = ctr;
            self.bits = 0;
            return false;
        }
        if ctr == self.max {
            return true;
        }
        if ctr.wrapping_sub(self.max) < 0x8000_0000 {
            let d = ctr.wrapping_sub(self.max);
            let shifted = if d < 32 { self.bits << d } else { 0 };
            self.bits = shifted | if d <= 32 { 1u32 << (d - 1) } else { 0 };
            self.max = ctr;
            return false;
        }
        let back = self.max.wrapping_sub(ctr);
        if back > 32 {
            return true;
        }
        let bit = 1u32 << (back - 1);
        if self.bits & bit != 0 {
            return true;
        }
        self.bits |= bit;
        false
    }
}
