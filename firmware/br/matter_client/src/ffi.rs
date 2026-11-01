use crate::client::*;
use core::ffi::{c_char, c_int, c_void};
use core::sync::atomic::{AtomicBool, Ordering};

const CERT_MAX: usize = 400;

#[repr(C)]
pub struct McIo {
    ctx: *mut c_void,
    send: unsafe extern "C" fn(*mut c_void, *const u8, usize) -> c_int,
    recv: unsafe extern "C" fn(*mut c_void, *mut u8, usize, u32) -> c_int,
    now_ms: unsafe extern "C" fn(*mut c_void) -> u32,
    rng: unsafe extern "C" fn(*mut c_void, *mut u8, usize),
}

#[repr(C)]
pub struct McMrp {
    idle_ms: u32,
    active_ms: u32,
    max_ms: u32,
    max_retx: u8,
    resp_ms: u32,
}

#[repr(C)]
pub struct WdCreds {
    fabric_id: u64,
    node_id: u64,
    rcac_len: u16,
    icac_len: u16,
    noc_len: u16,
    ipk: [u8; 16],
    key: [u8; 97],
    rcac: [u8; CERT_MAX],
    icac: [u8; CERT_MAX],
    noc: [u8; CERT_MAX],
}

struct CIo<'a>(&'a McIo);

impl Io for CIo<'_> {
    fn send(&mut self, b: &[u8]) -> i32 {
        unsafe { (self.0.send)(self.0.ctx, b.as_ptr(), b.len()) }
    }

    fn recv(&mut self, b: &mut [u8], timeout_ms: u32) -> i32 {
        unsafe { (self.0.recv)(self.0.ctx, b.as_mut_ptr(), b.len(), timeout_ms) }
    }

    fn now_ms(&mut self) -> u32 {
        unsafe { (self.0.now_ms)(self.0.ctx) }
    }

    fn rng(&mut self, b: &mut [u8]) {
        unsafe { (self.0.rng)(self.0.ctx, b.as_mut_ptr(), b.len()) }
    }
}

static BUSY: AtomicBool = AtomicBool::new(false);
static mut SCRATCH: Scratch = Scratch::new();

fn creds(c: &WdCreds) -> Option<Creds<'_>> {
    let (r, i, n) = (c.rcac_len as usize, c.icac_len as usize, c.noc_len as usize);
    if r > CERT_MAX || i > CERT_MAX || n > CERT_MAX {
        return None;
    }
    Some(Creds { fabric_id: c.fabric_id, node_id: c.node_id, ipk: &c.ipk, key: &c.key, rcac: &c.rcac[..r], icac: &c.icac[..i], noc: &c.noc[..n] })
}

unsafe fn put_err(err: *mut c_char, err_len: usize, e: &ErrBuf) {
    if err.is_null() || err_len == 0 {
        return;
    }
    let n = e.n.min(err_len - 1);
    for (i, b) in e.as_bytes().iter().take(n).enumerate() {
        *err.add(i) = *b as c_char;
    }
    *err.add(n) = 0;
}

#[no_mangle]
pub unsafe extern "C" fn mc_mrp_from_txt(m: *mut McMrp, sii_ms: u32, sai_ms: u32) {
    if m.is_null() {
        return;
    }
    let v = Mrp::from_txt(sii_ms, sai_ms);
    *m = McMrp { idle_ms: v.idle_ms, active_ms: v.active_ms, max_ms: v.max_ms, max_retx: v.max_tx, resp_ms: v.resp_ms };
}

#[no_mangle]
pub unsafe extern "C" fn mc_creds_cfid(cr: *const WdCreds, out: *mut u8) -> c_int {
    if cr.is_null() || out.is_null() {
        return E_ARG;
    }
    match creds(&*cr).and_then(|c| creds_cfid(&c)) {
        Some(cf) => {
            for (i, b) in cf.iter().enumerate() {
                *out.add(i) = *b;
            }
            OK
        }
        None => E_CERT,
    }
}

#[no_mangle]
pub unsafe extern "C" fn mc_write_int16(
    io: *const McIo,
    cr: *const WdCreds,
    peer_node: u64,
    mrp: *const McMrp,
    ep: u8,
    cluster: u32,
    attr: u32,
    val: i16,
    err: *mut c_char,
    err_len: usize,
) -> c_int {
    let mut eb = ErrBuf::new();
    if io.is_null() || cr.is_null() || mrp.is_null() {
        return E_ARG;
    }
    if BUSY.swap(true, Ordering::Acquire) {
        eb.set(format_args!("busy"));
        put_err(err, err_len, &eb);
        return E_ARG;
    }
    let m = &*mrp;
    let mrp = Mrp { idle_ms: m.idle_ms, active_ms: m.active_ms, max_ms: m.max_ms, max_tx: m.max_retx, resp_ms: m.resp_ms };
    let rc = match creds(&*cr) {
        Some(c) => {
            let s = &mut *core::ptr::addr_of_mut!(SCRATCH);
            write_int16(&mut CIo(&*io), s, &c, peer_node, &mrp, ep, cluster, attr, val, &mut eb)
        }
        None => {
            eb.set(format_args!("cert_len"));
            E_CERT
        }
    };
    put_err(err, err_len, &eb);
    BUSY.store(false, Ordering::Release);
    rc
}

#[cfg(target_os = "none")]
#[panic_handler]
fn panic(_: &core::panic::PanicInfo) -> ! {
    extern "C" {
        fn abort() -> !;
    }
    unsafe { abort() }
}
