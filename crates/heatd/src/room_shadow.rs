use crate::room_model::{bundle_candidates, Bundle, House, Out, ResetRec, RoomIn};
use crate::room_plan::{plan, Env, PlanOut, COAST_CAP};
use anyhow::{bail, Result};
use matter_controller::{CO2_CLUSTER, HUMIDITY_MEASUREMENT_CLUSTER, TEMPERATURE_MEASUREMENT_CLUSTER};
use serde::Deserialize;
use std::collections::{BTreeMap, HashMap};
use std::path::Path;
use std::sync::{Arc, Mutex};

const EPS: f64 = 1e-9;

#[derive(Debug, Clone, Deserialize)]
#[serde(default)]
pub struct RoomModelSection {
    pub enabled: bool,
    pub model_dir: String,
    pub raise_cost_min: f64,
    pub cold_cost_min: f64,
}

impl Default for RoomModelSection {
    fn default() -> Self {
        Self { enabled: false, model_dir: "/models/room_model".into(), raise_cost_min: 15.0, cold_cost_min: 1.0 }
    }
}

const MIN_MS: i64 = 60_000;
const TICK_MIN: i64 = 5;
const ROW_EVERY_MIN: i64 = 30;
const RELOAD_MIN: f64 = 10.0;
const MAX_CATCHUP_MIN: i64 = 30;
const TEMP_RANGE: [f64; 2] = [0.0, 45.0];
const VALVE_RANGE: [f64; 2] = [0.0, 100.0];

#[derive(Debug, Default)]
struct Feed {
    temp: Option<(i64, f64)>,
    valve: Option<(i64, f64)>,
    setpoint: Option<(i64, f64)>,
    fresh_temp: Option<f64>,
    fresh_valve: Option<f64>,
}

#[derive(Debug, Default)]
struct DevFeed {
    temp: Option<(i64, f64)>,
    rh: Option<(i64, f64)>,
    co2: Option<(i64, f64)>,
    fresh_temp: Option<f64>,
}

#[derive(Debug, Clone)]
pub struct RoomCtx {
    pub room: String,
    pub live: f64,
    pub target: f64,
    pub present: f64,
    pub sp_cur: Option<f64>,
    pub boost_ok: bool,
    pub window_live: bool,
    pub resid_live_k: Option<f64>,
}

#[derive(Debug, Default)]
pub struct StepOut {
    pub rows: Vec<crate::lake::ShadowDecision>,
    pub events: Vec<crate::lake::Event>,
}

#[derive(Default)]
struct Inner {
    bundle: Option<Arc<Bundle>>,
    house: Option<House>,
    cursor: Option<i64>,
    feeds: HashMap<u64, Feed>,
    devs: HashMap<u64, DevFeed>,
    inputs: BTreeMap<String, RoomIn>,
    decided: HashMap<String, i64>,
    last_row: HashMap<String, (f64, i64, bool)>,
    views: BTreeMap<String, serde_json::Value>,
    pending: Vec<crate::lake::Event>,
    away_since: Option<i64>,
    returned: Option<i64>,
    weather: [Option<f64>; 3],
    state: Option<&'static str>,
}

#[derive(Clone, Default)]
pub struct RoomModel(Arc<Mutex<Inner>>);

fn fmt_c(x: f64) -> String {
    if (x - x.round()).abs() < 1e-9 { format!("{x:.0}") } else { format!("{x:.1}") }
}

fn nan_null(v: f64) -> serde_json::Value {
    if v.is_finite() { serde_json::json!(v) } else { serde_json::Value::Null }
}

pub(crate) fn sentence(room: &str, p: &PlanOut, out: &Out, live: f64, boost_c: f64) -> String {
    let sp = fmt_c(p.setpoint);
    let saving = (p.energy_live - p.energy).max(0.0);
    if p.p_window >= 0.5 && p.kind != "probe" && p.setpoint < live - EPS {
        let tail = if saving >= 1.0 { format!(", spart ca. {saving:.0} min Heizen") } else { String::new() };
        return format!("{room}: Verlust {:.0}x normal -> Fenster offen -> Kälte zählt nicht, Heizen wäre Verlust -> {sp} °C{tail}", out.lam.max(1.0));
    }
    if out.p_supply < 0.5 && !p.raise {
        return format!("{room}: Kessel liefert nicht -> Anheben bringt nichts -> {sp} °C");
    }
    match p.kind {
        "probe" => format!("{room}: Fenster noch offen? Probe {} min bei {sp} °C", p.probe_min.unwrap_or(0)),
        "boost" => format!("{room}: Boost {} min bei {} °C -> schneller auf {} °C", p.boost_min.unwrap_or(0), fmt_c(boost_c), fmt_c(p.target)),
        _ if p.raise => format!("{room}: zu kalt bei Anwesenheit -> Soll auf {sp} °C anheben"),
        _ if p.setpoint < live - EPS => format!("{room}: Heizen lohnt kaum -> Soll {sp} °C statt {} °C", fmt_c(live)),
        _ => format!("{room}: Modell passt -> Soll {sp} °C bleibt"),
    }
}

fn fmt_de(x: f64) -> String {
    fmt_c(x).replace('.', ",")
}

fn fmt_span(min: f64) -> String {
    let m = min.round() as i64;
    match (m / 60, m % 60) {
        (0, r) => format!("{r} min"),
        (h, 0) => format!("{h} h"),
        (h, r) => format!("{h} h {r} min"),
    }
}

pub(crate) fn storage_sentence(room: &str, p: &PlanOut) -> String {
    let floor = fmt_de(p.target - 0.5);
    let coast = if p.coast_min <= 0.0 {
        format!("ohne Heizen schon unter {floor} °C")
    } else if p.coast_min >= COAST_CAP as f64 {
        format!("ohne Heizen länger als {} über {floor} °C", fmt_span(COAST_CAP as f64))
    } else {
        format!("ohne Heizen noch {} über {floor} °C", fmt_span(p.coast_min))
    };
    let stored = p.stored_min.map_or(String::new(), |v| format!(", Speicher {}{:.0} Heizmin", if v >= 0.5 { "+" } else { "" }, v));
    format!("{room}: Wände {} °C, Luft {} °C – {coast}{stored}", fmt_de(p.mass), fmt_de(p.air))
}

fn json_prior(p: [f64; 3]) -> serde_json::Value {
    serde_json::json!({"mass_c": nan_null(p[0]), "air_c": nan_null(p[1]), "lambda": nan_null(p[2])})
}

fn reset_event(rec: &ResetRec, now_ms: i64) -> crate::lake::Event {
    crate::lake::Event::now(crate::lake::kind::FILTER_RESET, "heatd").with_detail(serde_json::json!({"room": rec.room, "cause": rec.cause, "step_ms": now_ms, "prior": json_prior(rec.prior)}))
}

fn house_resets(house: &House, cause: &'static str, now_ms: i64) -> Vec<crate::lake::Event> {
    house
        .rooms
        .iter()
        .map(|(name, f)| reset_event(&ResetRec { room: name.clone(), cause, step: 0, prior: if f.last.ok { [f.last.m, f.last.a, f.last.lam] } else { [f64::NAN; 3] } }, now_ms))
        .collect()
}

impl Inner {
    fn set_state(&mut self, state: &'static str, out: &mut StepOut) {
        if self.state != Some(state) {
            self.state = Some(state);
            out.events.push(crate::lake::Event::now(crate::lake::kind::SHADOW_STATE, "heatd").with_detail(serde_json::json!({"state": state})));
        }
    }
}

impl RoomModel {
    pub fn set_bundle(&self, bundle: Bundle) {
        let mut inner = self.0.lock().unwrap();
        if let Some(house) = inner.house.take() {
            let events = house_resets(&house, "reload", crate::lake::now_us() / 1000);
            inner.pending.extend(events);
        }
        inner.house = Some(House::new(&bundle));
        inner.bundle = Some(Arc::new(bundle));
        inner.cursor = None;
        inner.inputs.clear();
        inner.decided.clear();
    }

    pub fn homecoming_open(&self, away: bool, now_ms: i64, min_away_min: f64) -> bool {
        let mut inner = self.0.lock().unwrap();
        match (away, inner.away_since) {
            (true, None) => inner.away_since = Some(now_ms),
            (false, Some(since)) => {
                inner.away_since = None;
                inner.returned = ((now_ms - since) as f64 / MIN_MS as f64 >= min_away_min).then_some(now_ms);
            }
            _ => {}
        }
        !away && inner.returned.is_some_and(|t| (now_ms - t) as f64 / MIN_MS as f64 <= min_away_min)
    }

    pub fn version(&self) -> Option<String> {
        self.0.lock().unwrap().bundle.as_ref().map(|b| b.version.clone())
    }

    pub fn observe(&self, node: u64, attribute: &str, value: f64, ts_ms: i64) {
        let range = match attribute {
            crate::window::LOCAL_TEMPERATURE | crate::window::OCCUPIED_SETPOINT => TEMP_RANGE,
            crate::window::PI_HEATING_DEMAND => VALVE_RANGE,
            _ => return,
        };
        if !value.is_finite() || value < range[0] || value > range[1] {
            return;
        }
        let mut inner = self.0.lock().unwrap();
        let feed = inner.feeds.entry(node).or_default();
        match attribute {
            crate::window::LOCAL_TEMPERATURE => (feed.temp, feed.fresh_temp) = (Some((ts_ms, value)), Some(value)),
            crate::window::PI_HEATING_DEMAND => (feed.valve, feed.fresh_valve) = (Some((ts_ms, value)), Some(value)),
            _ => feed.setpoint = Some((ts_ms, value)),
        }
    }

    pub fn observe_device(&self, node: u64, cluster: u32, attribute: u32, value: f64, ts_ms: i64) {
        if attribute != 0 || !value.is_finite() {
            return;
        }
        let mut inner = self.0.lock().unwrap();
        match cluster {
            TEMPERATURE_MEASUREMENT_CLUSTER if (TEMP_RANGE[0]..=TEMP_RANGE[1]).contains(&(value / 100.0)) => {
                let feed = inner.devs.entry(node).or_default();
                (feed.temp, feed.fresh_temp) = (Some((ts_ms, value / 100.0)), Some(value / 100.0));
            }
            HUMIDITY_MEASUREMENT_CLUSTER if (0.0..=10_000.0).contains(&value) => inner.devs.entry(node).or_default().rh = Some((ts_ms, value / 100.0)),
            CO2_CLUSTER if (0.0..=100_000.0).contains(&value) => inner.devs.entry(node).or_default().co2 = Some((ts_ms, value)),
            _ => {}
        }
    }

    pub fn view(&self, room: &str) -> serde_json::Value {
        self.0.lock().unwrap().views.get(room).cloned().unwrap_or(serde_json::Value::Null)
    }

    pub fn step(&self, config: &crate::config::Config, now_ms: i64, weather: [f64; 3], local_hour: f64, ctxs: &[RoomCtx]) -> StepOut {
        let cfg = &config.room_model;
        let mut guard = self.0.lock().unwrap();
        let inner = &mut *guard;
        let mut out = StepOut { events: std::mem::take(&mut inner.pending), ..Default::default() };
        for (slot, v) in inner.weather.iter_mut().zip(weather) {
            if v.is_finite() {
                *slot = Some(v);
            }
        }
        let weather = inner.weather.map(|v| v.unwrap_or(f64::NAN));
        if inner.bundle.is_none() {
            inner.set_state("no_model", &mut out);
            return out;
        }
        inner.set_state(if weather[0].is_finite() { "running" } else { "no_weather" }, &mut out);
        let (Some(bundle), Some(house)) = (inner.bundle.clone(), inner.house.as_mut()) else { return out };
        let stale_ms = config.stale.after_ms();
        let nodes: BTreeMap<String, u64> = bundle
            .rooms
            .iter()
            .filter_map(|(name, doc)| {
                let room = config.rooms.iter().find(|r| &r.name == name)?;
                Some((name.clone(), if room.nodes.contains(&doc.node) { doc.node } else { *room.nodes.first()? }))
            })
            .collect();
        let target = now_ms.div_euclid(MIN_MS);
        let mut cursor = inner.cursor.unwrap_or(target - 1);
        if target - cursor > MAX_CATCHUP_MIN {
            out.events.extend(house_resets(house, "restart_gap", now_ms));
            *house = House::new(&bundle);
            cursor = target - 1;
        }
        let [night_from, night_to] = bundle.house.night_hours;
        let night = local_hour >= night_from || local_hour < night_to;
        let wind = if weather[1].is_finite() { weather[1] } else { config.boost.wind_default };
        let rad = if weather[2].is_finite() { weather[2] } else { 0.0 };
        for _minute in cursor + 1..=target {
            let mut inputs = BTreeMap::new();
            for (name, doc) in &bundle.rooms {
                let Some(node) = nodes.get(name).copied() else { continue };
                let age_ok = |p: Option<(i64, f64)>| p.filter(|(t, _)| now_ms - t <= stale_ms).map(|(_, v)| v);
                let feed = inner.feeds.entry(node).or_default();
                let (y, yv) = (feed.fresh_temp.take().unwrap_or(f64::NAN), feed.fresh_valve.take().map_or(f64::NAN, |v| v / 100.0));
                let u = feed.valve.map_or(f64::NAN, |(_, v)| (v / 100.0).clamp(0.0, 1.0));
                let sp = feed.setpoint.map_or(f64::NAN, |p| p.1);
                let own = |wanted: Option<u64>| wanted.filter(|n| config.devices.iter().any(|d| d.node == *n && d.room.as_deref() == Some(name.as_str())));
                let fresh = |p: Option<(i64, f64)>| p.filter(|(t, _)| now_ms - t <= stale_ms).map_or(f64::NAN, |(_, v)| v);
                let ya: Vec<f64> = doc.air.iter().map(|a| own(a.node).and_then(|n| inner.devs.get_mut(&n)).and_then(|d| d.fresh_temp.take()).unwrap_or(f64::NAN)).collect();
                let level = |g: &Option<crate::room_obs::GasDoc>, pick: &dyn Fn(&DevFeed) -> Option<(i64, f64)>| g.as_ref().and_then(|g| own(g.node)).and_then(|n| inner.devs.get(&n)).map_or(f64::NAN, |d| fresh(pick(d)));
                let co2 = level(&doc.co2, &|d| d.co2);
                let (rh, th) = (level(&doc.hum, &|d| d.rh), level(&doc.hum, &|d| d.temp));
                let (mut ncnt, mut nsum) = (Vec::new(), Vec::new());
                for kind in doc.c.keys() {
                    let (mut n, mut s) = (0.0, 0.0);
                    for nn in doc.neighbours.get(kind).into_iter().flatten() {
                        if let Some(v) = inner.feeds.get(nn).and_then(|f| age_ok(f.temp)) {
                            n += 1.0;
                            s += v;
                        }
                    }
                    ncnt.push(n);
                    nsum.push(s);
                }
                inputs.insert(name.clone(), RoomIn { y, yv, u, sp, tout: weather[0], wind, rad, night, ncnt, nsum, ya, co2, rh, th });
            }
            house.step(&inputs);
            inner.inputs = inputs;
        }
        inner.cursor = Some(target);
        out.events.extend(house.resets.drain(..).map(|rec| reset_event(&rec, now_ms)));
        if target % TICK_MIN != 0 {
            return out;
        }
        for ctx in ctxs {
            let (Some(filter), Some(input)) = (house.rooms.get(&ctx.room), inner.inputs.get(&ctx.room)) else { continue };
            let filt = filter.last;
            if !filt.ok || filter.comps.is_empty() || inner.decided.get(&ctx.room) == Some(&target) {
                continue;
            }
            inner.decided.insert(ctx.room.clone(), target);
            let env = Env {
                tout: input.tout,
                wind: input.wind,
                rad: input.rad,
                night: input.night,
                ncnt: input.ncnt.clone(),
                nsum: input.nsum.clone(),
                target: ctx.target,
                present: ctx.present,
                sp_cur: ctx.sp_cur,
                sp_live: ctx.live,
                frost: config.frost_setpoint,
                boost_c: bundle.house.boost_c,
                boost_ok: ctx.boost_ok && filt.p_window < 0.5 && filt.p_supply >= 0.5,
                raise_cost: cfg.raise_cost_min,
                cold_cost: cfg.cold_cost_min,
                r_grid: bundle.house.r_grid,
                lag: filter.lag,
                y_last: nodes.get(&ctx.room).and_then(|n| inner.feeds.get(n)).and_then(|f| f.temp).filter(|(t, _)| now_ms - t <= stale_ms).map(|(_, v)| v),
            };
            let p = plan(&filter.par, &bundle.shock, &filter.comps, filt.p_supply, &env);
            let why = sentence(&ctx.room, &p, &filt, ctx.live, bundle.house.boost_c);
            let storage = storage_sentence(&ctx.room, &p);
            inner.views.insert(
                ctx.room.clone(),
                serde_json::json!({"loss_ratio": nan_null(filt.lam), "mass_c": nan_null(filt.m), "air_c": nan_null(filt.a), "p_supply": filt.p_supply, "p_window": filt.p_window, "setpoint_shadow": p.setpoint,
                                    "setpoint_live": ctx.live, "kind": p.kind, "why": why, "coast_min": p.coast_min, "stored_min": p.stored_min.map_or(serde_json::Value::Null, nan_null), "storage": storage, "probe_min": p.probe_min, "model": bundle.version, "ts": now_ms}),
            );
            let flagged = filt.p_window >= 0.5;
            let due = inner.last_row.get(&ctx.room).is_none_or(|(sp, at, was)| (sp - p.setpoint).abs() > EPS || target - at >= ROW_EVERY_MIN || *was != flagged);
            if due {
                inner.last_row.insert(ctx.room.clone(), (p.setpoint, target, flagged));
                out.rows.push(crate::lake::ShadowDecision {
                    ts: crate::lake::now_us(),
                    room: ctx.room.clone(),
                    setpoint_live: ctx.live,
                    setpoint_shadow: p.setpoint,
                    raise: p.raise,
                    probe_min: p.probe_min.map(|v| v as f64),
                    boost_min: p.boost_min.map(|v| v as f64),
                    cost_energy: p.energy,
                    cost_cold: p.cold,
                    cost_raise: p.raise_cost,
                    cost_live: p.cost_live,
                    lambda: filt.lam,
                    lambda_std: filt.lam_std,
                    mass_c: filt.m,
                    air_c: filt.a,
                    p_supply: filt.p_supply,
                    p_window: filt.p_window,
                    p_window_live: ctx.window_live,
                    model_version: bundle.version.clone(),
                    why,
                    sensor_c: p.sensor,
                    forecast_c: Some(p.forecast).filter(|v| v.is_finite()),
                    resid_live_k: ctx.resid_live_k.filter(|v| v.is_finite()),
                    kind: p.kind.to_string(),
                    coast_min: Some(p.coast_min),
                    stored_min: p.stored_min.filter(|v| v.is_finite()),
                    storage: Some(storage),
                });
            }
        }
        out
    }
}

pub fn spawn_reload(model: RoomModel, dir: String) {
    tokio::spawn(async move {
        let mut last_error = String::new();
        loop {
            let current = model.version();
            let base = dir.clone();
            let loaded = tokio::task::spawn_blocking(move || -> Result<Option<Bundle>> {
                let candidates = bundle_candidates(Path::new(&base));
                if candidates.is_empty() {
                    bail!("no room model bundle in {base}, shadow off until one appears");
                }
                let mut failures = Vec::new();
                for (name, path) in candidates {
                    if current.as_deref() == Some(name.as_str()) {
                        if failures.is_empty() {
                            return Ok(None);
                        }
                        bail!("newer room model bundles failed to load, keeping the current model ({name}): {}", failures.join("; "));
                    }
                    match Bundle::load(&path) {
                        Ok(bundle) => return Ok(Some(bundle)),
                        Err(e) => failures.push(format!("{name}: {e:#}")),
                    }
                }
                bail!("no loadable room model bundle in {base} ({})", failures.join("; "))
            })
            .await;
            match loaded {
                Ok(Ok(Some(bundle))) => {
                    last_error.clear();
                    eprintln!("room model: loaded bundle {} (shadow)", bundle.version);
                    model.set_bundle(bundle);
                }
                Ok(Err(e)) if last_error != format!("{e:#}") => {
                    last_error = format!("{e:#}");
                    eprintln!("room model: {last_error}");
                }
                _ => {}
            }
            tokio::time::sleep(std::time::Duration::from_secs_f64(RELOAD_MIN * 60.0)).await;
        }
    });
}
