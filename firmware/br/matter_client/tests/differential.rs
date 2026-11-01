mod common;
use common::*;
use matter_client::client::*;
use std::path::Path;
use std::process::Command;

fn frames(capture: &Path) -> Vec<Vec<u8>> {
    let text = std::fs::read_to_string(capture).unwrap();
    let mut out: Vec<Vec<u8>> = vec![];
    let mut seen: Vec<(Vec<u8>, Vec<u8>)> = vec![];
    for l in text.lines().filter(|l| l.starts_with("rx ")) {
        let d = hex(l.split(' ').nth(1).unwrap());
        let key = (d[1..3].to_vec(), d[4..8].to_vec());
        if seen.contains(&key) {
            continue;
        }
        seen.push(key);
        let secured = d[1..3] != [0, 0];
        let n_unsecured = seen.iter().filter(|k| k.0 == [0, 0]).count();
        let n_secured = seen.len() - n_unsecured;
        if (!secured && n_unsecured <= 2) || (secured && n_secured == 1) {
            out.push(d);
        }
    }
    out
}

fn run_c(exe: &str, port: u16) -> bool {
    Command::new(exe).args([port.to_string(), "ok".into(), "DEDEDEDE00010002".into()]).env("LB_FIXED_RNG", "1").status().unwrap().success()
}

#[test]
fn c_and_rust_emit_identical_sigma1_sigma3_write_request() {
    let Ok(exe) = std::env::var("C_LOOPBACK") else {
        println!("C_LOOPBACK not set, differential test skipped");
        return;
    };
    let tmp = std::env::temp_dir();
    let (tape, cap_c, cap_r) = (tmp.join("diff_tape"), tmp.join("diff_cap_c"), tmp.join("diff_cap_r"));
    for p in [&tape, &cap_c, &cap_r] {
        let _ = std::fs::remove_file(p);
    }
    let no_strict = ("RESP_STRICT", "0".to_string());
    let mut rec = spawn_responder(47890, "ok", &tmp.join("diff_res0"), &[no_strict.clone(), ("RESP_CAPTURE", tape.display().to_string())]);
    assert!(run_c(&exe, 47890), "C client against recording responder");
    rec.wait().unwrap();
    let mut rc = spawn_responder(47891, "ok", &tmp.join("diff_res1"), &[no_strict.clone(), ("RESP_REPLAY", tape.display().to_string()), ("RESP_CAPTURE", cap_c.display().to_string())]);
    assert!(run_c(&exe, 47891), "C client against replay");
    rc.wait().unwrap();
    let mut rr = spawn_responder(47892, "ok", &tmp.join("diff_res2"), &[no_strict, ("RESP_REPLAY", tape.display().to_string()), ("RESP_CAPTURE", cap_r.display().to_string())]);
    let f = Fixture::new();
    let cr = f.creds();
    let mut net = Net::new(47892, Box::new(|b: &mut [u8]| b.fill(0x5a)));
    let mut s = Scratch::new();
    let mut e = ErrBuf::new();
    let mrp = Mrp { idle_ms: 300, active_ms: 200, max_ms: 2000, max_tx: 5, resp_ms: 3000 };
    let r = write_int16(&mut net, &mut s, &cr, 0xDEDEDEDE00010002, &mrp, 1, 0x201, 0x12, 1800, &mut e);
    assert_eq!(r, OK, "{}", String::from_utf8_lossy(e.as_bytes()));
    rr.wait().unwrap();
    let (fc, fr, ft) = (frames(&cap_c), frames(&cap_r), frames(&tape));
    assert_eq!(fc.len(), 3, "sigma1, sigma3, write request captured from C");
    assert_eq!(fr.len(), 3, "sigma1, sigma3, write request captured from Rust");
    assert_eq!(fc, ft, "C replay run reproduces recorded C run");
    for (i, name) in ["Sigma1", "Sigma3", "WriteRequest"].iter().enumerate() {
        assert_eq!(fc[i], fr[i], "{} differs between C and Rust", name);
    }
}
