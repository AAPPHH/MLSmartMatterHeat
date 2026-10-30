use anyhow::{anyhow, bail, Context, Result};
use ort::session::Session;
use ort::value::Tensor;
use serde_json::Value;
use std::collections::{BTreeMap, HashMap};
use std::path::{Path, PathBuf};
use std::sync::Mutex;

pub const GREY: &str = "grey_box";
pub const RESIDUAL: &str = "residual";
pub const BASELINE: &str = "baseline";
pub const VARIANTS: [&str; 2] = [GREY, RESIDUAL];
pub const AMBIENT: [&str; 3] = ["nvme_c", "pkg_w", "die_c"];
pub const SINCE_CAP_MIN: f64 = 240.0;
pub const REACH_C: f64 = 0.3;
const SIM_MIN: usize = 240;

#[derive(Clone, Debug, Default)]
pub struct Env {
    pub tout: f64,
    pub wind: f64,
    pub rad: f64,
    pub night: bool,
    pub nb: BTreeMap<String, (f64, f64)>,
}

#[derive(Clone, Copy, Debug)]
pub struct Terms {
    pub a: [f64; 4],
    pub c: [f64; 2],
}

#[derive(Clone, Debug)]
pub struct Physics {
    p: BTreeMap<String, f64>,
    rc2: bool,
    pub w: f64,
    pub gain: f64,
    pub night: (f64, f64),
}

impl Physics {
    pub fn from_doc(doc: &Value) -> Result<Self> {
        let p: BTreeMap<String, f64> = doc["params"].as_object().ok_or_else(|| anyhow!("physics: no params"))?.iter().filter_map(|(k, v)| v.as_f64().map(|x| (k.clone(), x))).collect();
        let rc2 = match doc["structure"].as_str() {
            Some("rc2") => true,
            Some("rc1") => false,
            other => bail!("physics: unknown structure {other:?}"),
        };
        let w = if rc2 { *p.get("w").ok_or_else(|| anyhow!("physics: rc2 without w"))? } else { 0.0 };
        let night = doc["night_hours"].as_array().and_then(|a| Some((a.first()?.as_f64()?, a.get(1)?.as_f64()?))).unwrap_or((22.0, 5.0));
        Ok(Self { p, rc2, w, gain: doc["valve_gain"].as_f64().ok_or_else(|| anyhow!("physics: no valve_gain"))?, night })
    }

    fn par(&self, name: &str) -> f64 {
        self.p.get(name).copied().unwrap_or(0.0)
    }

    pub fn is_night(&self, hour: f64) -> bool {
        let h = hour.rem_euclid(24.0).floor();
        h >= self.night.0 || h < self.night.1
    }

    pub fn terms(&self, u: f64, env: &Env, h: f64) -> Terms {
        let au = self.par("a1") * u;
        let ts = if env.night { self.p.get("ts_night").copied().unwrap_or_else(|| self.par("ts")) } else { self.par("ts") };
        let (mut cn, mut cs) = (0.0, 0.0);
        for (kind, (n, s)) in &env.nb {
            let c = self.par(&format!("c_{kind}"));
            cn += c * n;
            cs += c * s;
        }
        let amb = self.par("b2") + self.par("b3") * env.wind;
        let ga = amb * env.tout + self.par("b4") * env.rad / 100.0 + cs;
        let da = amb + cn;
        let (f11, f12, f21, f22, g1, g2) = if self.rc2 {
            (-(au + self.par("a2")), self.par("a2"), self.par("b1"), -(self.par("b1") + da), au * ts, ga)
        } else {
            (-1.0, 0.0, 0.0, -(au + da), 0.0, ga + au * ts)
        };
        Terms { a: [1.0 + h * f11, h * f12, h * f21, 1.0 + h * f22], c: [h * g1, h * g2] }
    }

    pub fn step(&self, x: (f64, f64), u: f64, env: &Env, h: f64) -> (f64, f64) {
        let t = self.terms(u, env, h);
        (t.a[0] * x.0 + t.a[1] * x.1 + t.c[0], t.a[2] * x.0 + t.a[3] * x.1 + t.c[1])
    }

    pub fn sensor(&self, x: (f64, f64)) -> f64 {
        self.w * x.0 + (1.0 - self.w) * x.1
    }

    pub fn pi(&self, setpoint: f64, sensor: f64) -> f64 {
        (self.gain * (setpoint - sensor)).clamp(0.0, 1.0)
    }
}

#[derive(Clone, Copy, Debug)]
pub struct Kalman {
    pub x: (f64, f64),
    p: (f64, f64, f64),
}

impl Kalman {
    pub fn new(first: f64, p0: f64) -> Self {
        Self { x: (first, first), p: (p0, 0.0, p0) }
    }

    pub fn update(&mut self, y: f64, w: f64, r: f64) -> (f64, f64) {
        let (p11, p12, p22) = self.p;
        let s = w * w * p11 + 2.0 * w * (1.0 - w) * p12 + (1.0 - w) * (1.0 - w) * p22 + r;
        let v = y - (w * self.x.0 + (1.0 - w) * self.x.1);
        let k1 = (w * p11 + (1.0 - w) * p12) / s;
        let k2 = (w * p12 + (1.0 - w) * p22) / s;
        self.x = (self.x.0 + k1 * v, self.x.1 + k2 * v);
        self.p = (p11 - k1 * (w * p11 + (1.0 - w) * p12), p12 - k1 * (w * p12 + (1.0 - w) * p22), p22 - k2 * (w * p12 + (1.0 - w) * p22));
        (v, s)
    }

    pub fn predict(&mut self, t: &Terms, q1: f64, q2: f64) {
        let (p11, p12, p22) = self.p;
        let [a11, a12, a21, a22] = t.a;
        let (t11, t12) = (a11 * p11 + a12 * p12, a11 * p12 + a12 * p22);
        let (t21, t22) = (a21 * p11 + a22 * p12, a21 * p12 + a22 * p22);
        self.p = (t11 * a11 + t12 * a12 + q1, t11 * a21 + t12 * a22, t21 * a21 + t22 * a22 + q2);
        self.x = (a11 * self.x.0 + a12 * self.x.1 + t.c[0], a21 * self.x.0 + a22 * self.x.1 + t.c[1]);
    }
}

#[derive(Clone, Debug, PartialEq)]
pub enum Feat {
    Sensor(usize),
    Valve(usize),
    Outdoor,
    Radiation,
    HourSin,
    HourCos,
    Others,
    SinceBoost,
    PhysSensor,
    PhysAir,
    Ambient(usize),
    Room(String),
}

pub fn parse_feat(name: &str) -> Result<Feat> {
    let lag = |s: &str| s.parse::<usize>().with_context(|| format!("feature {name}"));
    Ok(match name {
        "outdoor_c" => Feat::Outdoor,
        "radiation_wm2" => Feat::Radiation,
        "hour_sin" => Feat::HourSin,
        "hour_cos" => Feat::HourCos,
        "others_heating" => Feat::Others,
        "minutes_since_boost" => Feat::SinceBoost,
        "phys_sensor_pred" => Feat::PhysSensor,
        "phys_air" => Feat::PhysAir,
        _ => {
            if let Some(n) = name.strip_prefix("sensor_lag_") {
                Feat::Sensor(lag(n)?)
            } else if let Some(n) = name.strip_prefix("valve_lag_") {
                Feat::Valve(lag(n)?)
            } else if let Some(s) = name.strip_prefix("ambient_") {
                Feat::Ambient(AMBIENT.iter().position(|a| *a == s).ok_or_else(|| anyhow!("unknown feature {name}"))?)
            } else if let Some(r) = name.strip_prefix("room_") {
                Feat::Room(r.to_string())
            } else {
                bail!("unknown feature {name}")
            }
        }
    })
}

pub struct FeatIn<'a> {
    pub sensor: &'a [f64],
    pub valve: &'a [f64],
    pub outdoor: f64,
    pub radiation: f64,
    pub hour: f64,
    pub others: f64,
    pub since: f64,
    pub phys_sensor: f64,
    pub phys_air: f64,
    pub ambient: [f64; 3],
    pub room: &'a str,
}

fn lagged(series: &[f64], lag: usize) -> f64 {
    series.len().checked_sub(1 + lag).map_or_else(|| series.first().copied().unwrap_or(f64::NAN), |i| series[i])
}

pub fn featurize(feats: &[Feat], i: &FeatIn) -> Vec<f32> {
    let angle = std::f64::consts::TAU * i.hour.rem_euclid(24.0) / 24.0;
    feats
        .iter()
        .map(|f| {
            (match f {
                Feat::Sensor(l) => lagged(i.sensor, *l),
                Feat::Valve(l) => lagged(i.valve, *l),
                Feat::Outdoor => i.outdoor,
                Feat::Radiation => i.radiation,
                Feat::HourSin => angle.sin(),
                Feat::HourCos => angle.cos(),
                Feat::Others => i.others,
                Feat::SinceBoost => i.since.min(SINCE_CAP_MIN),
                Feat::PhysSensor => i.phys_sensor,
                Feat::PhysAir => i.phys_air,
                Feat::Ambient(k) => i.ambient[*k],
                Feat::Room(r) => f64::from(u8::from(r == i.room)),
            }) as f32
        })
        .collect()
}

pub struct Net {
    session: Mutex<Session>,
    feats: Vec<Feat>,
    output: &'static str,
}

impl Net {
    fn load(dir: &Path, spec: &Value, output: &'static str) -> Result<Self> {
        let file = spec["file"].as_str().ok_or_else(|| anyhow!("no file"))?;
        let feats = spec["features"].as_array().ok_or_else(|| anyhow!("no features"))?.iter().map(|f| parse_feat(f.as_str().unwrap_or_default())).collect::<Result<Vec<_>>>()?;
        let session = Session::builder()?.commit_from_file(dir.join(file)).with_context(|| format!("load {file}"))?;
        Ok(Self { session: Mutex::new(session), feats, output })
    }

    pub fn run(&self, i: &FeatIn) -> Result<f64> {
        let x = featurize(&self.feats, i);
        let mut session = self.session.lock().unwrap();
        let outputs = session.run(ort::inputs!["x" => Tensor::from_array(([1usize, x.len()], x))?])?;
        let (_, v) = outputs[self.output].try_extract_tensor::<f32>()?;
        v.first().map(|v| f64::from(*v)).ok_or_else(|| anyhow!("empty output {}", self.output))
    }

    #[cfg(test)]
    pub fn features(&self, i: &FeatIn) -> Vec<f32> {
        featurize(&self.feats, i)
    }
}

pub struct BRoom {
    pub phys: Physics,
    pub offset: f64,
    pub valid: bool,
}

pub struct Bundle {
    pub version: String,
    pub rooms: HashMap<String, BRoom>,
    pub latent: Option<Net>,
    pub residual: Option<Net>,
    pub metrics: Value,
}

impl Bundle {
    pub fn load(dir: &Path) -> Result<Self> {
        let path = dir.join("bundle.json");
        let doc: Value = serde_json::from_str(&std::fs::read_to_string(&path).with_context(|| format!("read {}", path.display()))?)?;
        let mut rooms = HashMap::new();
        for (name, r) in doc["rooms"].as_object().ok_or_else(|| anyhow!("bundle: no rooms"))? {
            match Physics::from_doc(&r["physics"]) {
                Ok(phys) => {
                    rooms.insert(name.clone(), BRoom { phys, offset: r["sensor_offset_c"].as_f64().unwrap_or(0.0), valid: r["valid"].as_bool().unwrap_or(false) });
                }
                Err(e) => eprintln!("boost: room {name} physics unusable: {e:#}"),
            }
        }
        let net = |key: &str, output: &'static str| doc[key].is_object().then(|| Net::load(dir, &doc[key], output).map_err(|e| eprintln!("boost: {key} model unusable: {e:#}")).ok()).flatten();
        Ok(Self {
            version: doc["version"].as_str().unwrap_or_default().to_string(),
            rooms,
            latent: net("latent", "room_temp"),
            residual: net("residual", "delta"),
            metrics: doc["metrics"].clone(),
        })
    }

    pub fn has_residual(&self) -> bool {
        self.latent.is_some() && self.residual.is_some()
    }

    pub fn backtest(&self, room: &str, variant: &str) -> (Option<f64>, usize) {
        backtest(&self.metrics, room, variant)
    }
}

pub fn backtest(metrics: &Value, room: &str, variant: &str) -> (Option<f64>, usize) {
    let r = &metrics["backtest"][room];
    let rmse = r[variant]["rmse"].as_f64().or_else(|| r[variant].as_f64()).or_else(|| r[format!("{variant}_rmse")].as_f64());
    let cycles = r["cycles"].as_u64().or_else(|| r[variant]["cycles"].as_u64()).unwrap_or(0);
    (rmse, cycles as usize)
}

pub fn newest_bundle_dir(base: &Path) -> Option<(String, PathBuf)> {
    let mut found: Vec<(String, String, PathBuf)> = std::fs::read_dir(base)
        .ok()?
        .flatten()
        .filter_map(|e| {
            let text = std::fs::read_to_string(e.path().join("bundle.json")).ok()?;
            let created = serde_json::from_str::<Value>(&text).ok()?["created"].as_str().unwrap_or_default().to_string();
            Some((created, e.file_name().to_string_lossy().into_owned(), e.path()))
        })
        .collect();
    found.sort();
    found.pop().map(|(_, name, path)| (name, path))
}

#[derive(Clone, Debug, Default)]
pub struct Sim {
    pub peak: f64,
    pub ttt: Option<f64>,
    pub valve50: f64,
}

pub struct Ctx<'a> {
    pub phys: &'a Physics,
    pub bundle: &'a Bundle,
    pub room: &'a str,
    pub x: (f64, f64),
    pub env: Env,
    pub hour: f64,
    pub target: f64,
    pub boost_sp: f64,
    pub sensor: Vec<f64>,
    pub valve: Vec<f64>,
    pub others: f64,
    pub since: f64,
    pub ambient: [f64; 3],
    pub h: f64,
}

impl Ctx<'_> {
    fn env_at(&self, k: usize) -> Env {
        Env { night: self.phys.is_night(self.hour + k as f64 / 60.0), ..self.env.clone() }
    }

    fn setpoint(&self, k: usize, t_off: usize) -> f64 {
        if k < t_off {
            self.boost_sp
        } else {
            self.target
        }
    }

    pub fn grey(&self, t_off: usize) -> Sim {
        let mut x = self.x;
        let mut sim = Sim { peak: x.1, ..Default::default() };
        for k in 0..SIM_MIN {
            let u = self.phys.pi(self.setpoint(k, t_off), self.phys.sensor(x));
            x = self.phys.step(x, u, &self.env_at(k), self.h);
            sim.peak = sim.peak.max(x.1);
            sim.valve50 += f64::from(u8::from(u > 0.5));
            if sim.ttt.is_none() && x.1 >= self.target - REACH_C {
                sim.ttt = Some((k + 1) as f64);
            }
        }
        sim
    }

    fn feat_in<'a>(&'a self, sensor: &'a [f64], valve: &'a [f64], k: usize, x: (f64, f64), phys_sensor: f64) -> FeatIn<'a> {
        FeatIn {
            sensor,
            valve,
            outdoor: self.env.tout,
            radiation: self.env.rad,
            hour: self.hour + k as f64 / 60.0,
            others: self.others,
            since: self.since + k as f64,
            phys_sensor,
            phys_air: x.1,
            ambient: self.ambient,
            room: self.room,
        }
    }

    pub fn learned_air(&self) -> Result<f64> {
        let (latent, _) = self.nets()?;
        let u = self.valve.last().copied().unwrap_or(0.0);
        let next = self.phys.step(self.x, u, &self.env_at(0), self.h);
        latent.run(&self.feat_in(&self.sensor, &self.valve, 0, self.x, self.phys.sensor(next)))
    }

    fn nets(&self) -> Result<(&Net, &Net)> {
        match (&self.bundle.latent, &self.bundle.residual) {
            (Some(l), Some(r)) => Ok((l, r)),
            _ => bail!("residual models not in bundle"),
        }
    }

    pub fn residual(&self, t_off: usize) -> Result<Sim> {
        let (latent, residual) = self.nets()?;
        let (mut x, mut sensor, mut valve) = (self.x, self.sensor.clone(), self.valve.clone());
        let mut sim = Sim { peak: f64::NEG_INFINITY, ..Default::default() };
        for k in 0..SIM_MIN {
            let u = self.phys.pi(self.setpoint(k, t_off), *sensor.last().unwrap_or(&self.target));
            if let Some(last) = valve.last_mut() {
                *last = u;
            }
            let next = self.phys.step(x, u, &self.env_at(k), self.h);
            let phys_sensor = self.phys.sensor(next);
            let input = self.feat_in(&sensor, &valve, k, x, phys_sensor);
            let (air, delta) = (latent.run(&input)?, residual.run(&input)?);
            sim.peak = sim.peak.max(air);
            sim.valve50 += f64::from(u8::from(u > 0.5));
            if sim.ttt.is_none() && air >= self.target - REACH_C {
                sim.ttt = Some((k + 1) as f64);
            }
            sensor.push(phys_sensor + delta);
            valve.push(u);
            x = next;
        }
        Ok(sim)
    }

    pub fn plan(&self, variant: &str, max_off: usize) -> Result<(usize, Sim)> {
        let run = |t_off: usize| if variant == GREY { Ok(self.grey(t_off)) } else { self.residual(t_off) };
        let none = run(0)?;
        if none.peak >= self.target {
            return Ok((0, none));
        }
        let all = run(max_off)?;
        if all.peak <= self.target {
            return Ok((max_off, all));
        }
        let (mut lo, mut hi) = (0, max_off);
        while hi - lo > 1 {
            let mid = (lo + hi) / 2;
            if run(mid)?.peak <= self.target {
                lo = mid;
            } else {
                hi = mid;
            }
        }
        Ok((lo, run(lo)?))
    }
}
