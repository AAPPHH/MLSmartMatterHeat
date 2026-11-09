use crate::room_model::{bundle_candidates, plan, Bundle, Env, House, Out, PlanOut, ResetRec, RoomIn};
use anyhow::{bail, Result};
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
        Self { enabled: true, model_dir: "/models/room_model".into(), raise_cost_min: 15.0, cold_cost_min: 1.0 }
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
    inputs: BTreeMap<String, RoomIn>,
    decided: HashMap<String, i64>,
    last_row: HashMap<String, (f64, i64)>,
    views: BTreeMap<String, serde_json::Value>,
    resets_seen: usize,
    away_since: Option<i64>,
    returned: Option<i64>,
}

#[derive(Clone, Default)]
pub struct RoomModel(Arc<Mutex<Inner>>);

fn fmt_c(x: f64) -> String {
    if (x - x.round()).abs() < 1e-9 { format!("{x:.0}") } else { format!("{x:.1}") }
}

fn nan_null(v: f64) -> serde_json::Value {
    if v.is_finite() { serde_json::json!(v) } else { serde_json::Value::Null }
}

fn sentence(room: &str, p: &PlanOut, out: &Out, live: f64, boost_c: f64) -> String {
    let sp = fmt_c(p.setpoint);
    let saving = (p.energy_live - p.energy).max(0.0);
    if out.p_window >= 0.5 && p.kind != "probe" {
        let tail = if saving >= 1.0 { format!(", spart ca. {saving:.0} min Heizen") } else { String::new() };
        return format!("{room}: Verlust {:.0}x normal -> Fenster offen -> {sp} C{tail}", out.lam.max(1.0));
    }
    if out.p_supply < 0.5 && !p.raise {
        return format!("{room}: Kessel liefert nicht -> Anheben bringt nichts -> {sp} C");
    }
    match p.kind {
        "probe" => format!("{room}: Fenster noch offen? Probe {} min bei {sp} C", p.probe_min.unwrap_or(0)),
        "boost" => format!("{room}: Boost {} min bei {} C -> schneller auf {} C", p.boost_min.unwrap_or(0), fmt_c(boost_c), fmt_c(p.target)),
        _ if p.raise => format!("{room}: zu kalt bei Anwesenheit -> Soll auf {sp} C anheben"),
        _ if p.setpoint < live - EPS => format!("{room}: Heizen lohnt kaum -> Soll {sp} C statt {} C", fmt_c(live)),
        _ => format!("{room}: Modell passt -> Soll {sp} C bleibt"),
    }
}

fn json_prior(p: [f64; 3]) -> serde_json::Value {
    serde_json::json!({"mass_c": nan_null(p[0]), "air_c": nan_null(p[1]), "lambda": nan_null(p[2])})
}

fn reset_event(rec: &ResetRec, now_ms: i64) -> crate::lake::Event {
    crate::lake::Event::now(crate::lake::kind::FILTER_RESET, "heatd").with_detail(serde_json::json!({"room": rec.room, "cause": rec.cause, "step_ms": now_ms, "prior": json_prior(rec.prior)}))
}

impl RoomModel {
    pub fn set_bundle(&self, bundle: Bundle) {
        let mut inner = self.0.lock().unwrap();
        inner.house = Some(House::new(&bundle));
        inner.bundle = Some(Arc::new(bundle));
        inner.cursor = None;
        inner.resets_seen = 0;
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

    pub fn view(&self, room: &str) -> serde_json::Value {
        self.0.lock().unwrap().views.get(room).cloned().unwrap_or(serde_json::Value::Null)
    }

    pub fn step(&self, config: &crate::config::Config, now_ms: i64, weather: [f64; 3], local_hour: f64, ctxs: &[RoomCtx]) -> StepOut {
        let cfg = &config.room_model;
        let mut guard = self.0.lock().unwrap();
        let inner = &mut *guard;
        let (Some(bundle), Some(house)) = (inner.bundle.clone(), inner.house.as_mut()) else { return StepOut::default() };
        let mut out = StepOut::default();
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
            for (name, f) in house.rooms.iter() {
                let prior = if f.last.ok { [f.last.m, f.last.a, f.last.lam] } else { [f64::NAN; 3] };
                out.events.push(reset_event(&ResetRec { room: name.clone(), cause: "restart_gap", step: 0, prior }, now_ms));
            }
            *house = House::new(&bundle);
            inner.resets_seen = 0;
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
                let u = age_ok(feed.valve).map_or(f64::NAN, |v| (v / 100.0).clamp(0.0, 1.0));
                let sp = feed.setpoint.map_or(f64::NAN, |p| p.1);
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
                inputs.insert(name.clone(), RoomIn { y, yv, u, sp, tout: weather[0], wind, rad, night, ncnt, nsum });
            }
            house.step(&inputs);
            inner.inputs = inputs;
        }
        inner.cursor = Some(target);
        out.events.extend(house.resets.iter().skip(inner.resets_seen).map(|rec| reset_event(rec, now_ms)));
        inner.resets_seen = house.resets.len();
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
            };
            let p = plan(&filter.par, &bundle.shock, &filter.comps, filt.p_supply, &env);
            let why = sentence(&ctx.room, &p, &filt, ctx.live, bundle.house.boost_c);
            inner.views.insert(
                ctx.room.clone(),
                serde_json::json!({"loss_ratio": nan_null(filt.lam), "mass_c": nan_null(filt.m), "air_c": nan_null(filt.a), "p_supply": filt.p_supply, "p_window": filt.p_window, "setpoint_shadow": p.setpoint,
                                    "setpoint_live": ctx.live, "kind": p.kind, "why": why, "model": bundle.version, "ts": now_ms}),
            );
            let due = inner.last_row.get(&ctx.room).is_none_or(|(sp, at)| (sp - p.setpoint).abs() > EPS || target - at >= ROW_EVERY_MIN);
            if due {
                inner.last_row.insert(ctx.room.clone(), (p.setpoint, target));
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
