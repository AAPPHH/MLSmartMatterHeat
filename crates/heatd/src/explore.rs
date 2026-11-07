use crate::config::{Config, Room};
use chrono::{Duration, NaiveDate, NaiveDateTime, Timelike};
use std::collections::HashMap;

pub const MIN_STEP_C: f64 = 0.5;
pub const START_GRACE_MIN: i64 = 30;
const PLACEMENT_TRIES: usize = 64;

#[derive(Debug, Clone)]
pub struct Rng(u64);

impl Rng {
    pub fn new(seed: u64) -> Self {
        Self(seed ^ 0x9E37_79B9_7F4A_7C15)
    }

    pub fn next_u64(&mut self) -> u64 {
        self.0 = self.0.wrapping_add(0x9E37_79B9_7F4A_7C15);
        let mut z = self.0;
        z = (z ^ (z >> 30)).wrapping_mul(0xBF58_476D_1CE4_E5B9);
        z = (z ^ (z >> 27)).wrapping_mul(0x94D0_49BB_1331_11EB);
        z ^ (z >> 31)
    }

    pub fn unit(&mut self) -> f64 {
        (self.next_u64() >> 11) as f64 / (1u64 << 53) as f64
    }

    pub fn range(&mut self, lo: u32, hi: u32) -> u32 {
        lo + (self.next_u64() % u64::from(hi - lo + 1)) as u32
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Kind {
    Step,
    NightSetback,
    ValveClosed,
    ValveOpen,
}

impl Kind {
    pub fn name(self) -> &'static str {
        match self {
            Kind::Step => "setpoint_step",
            Kind::NightSetback => "night_setback",
            Kind::ValveClosed => "valve_0",
            Kind::ValveOpen => "valve_100",
        }
    }
}

#[derive(Debug, Clone)]
pub struct Plan {
    pub kind: Kind,
    pub start: NaiveDateTime,
    pub minutes: u32,
}

#[derive(Debug, Clone)]
pub struct Experiment {
    pub id: String,
    pub room: String,
    pub kind: Kind,
    pub setpoint: f64,
    pub base: f64,
    pub start: NaiveDateTime,
    pub end: NaiveDateTime,
}

#[derive(Debug, Clone)]
pub enum Lifecycle {
    Started(Experiment),
    Ended(Experiment),
    Aborted(Experiment, &'static str),
}

pub struct Inputs<'a> {
    pub config: &'a Config,
    pub blocked: &'a dyn Fn(&str) -> Option<&'static str>,
    pub no_raise: &'a dyn Fn(&str) -> bool,
    pub base: &'a dyn Fn(&Room) -> f64,
}

pub struct Explorer {
    rng: Rng,
    planned_day: Option<NaiveDate>,
    plans: HashMap<String, Vec<Plan>>,
    active: HashMap<String, Experiment>,
    serial: u64,
}

fn allowed(room: &Room, start: NaiveDateTime, minutes: u32) -> bool {
    (0..=minutes).step_by(5).all(|m| {
        let at = start + Duration::minutes(i64::from(m));
        !room.exploration.blackout.iter().any(|b| b.contains(at.hour() * 60 + at.minute()))
    })
}

pub fn target(kind: Kind, room: &Room, config: &Config, base: f64, up: bool, delta: f64) -> Option<f64> {
    let floor = room.exploration.comfort_min.max(config.frost_setpoint);
    let ceiling = room.exploration.comfort_max.max(floor);
    let wanted = match kind {
        Kind::Step => base + if up { delta } else { -delta },
        Kind::NightSetback | Kind::ValveClosed => floor,
        Kind::ValveOpen => ceiling,
    };
    let clamped = wanted.clamp(floor, ceiling);
    ((clamped - base).abs() >= MIN_STEP_C).then_some(clamped)
}

impl Explorer {
    pub fn new(seed: u64) -> Self {
        Self { rng: Rng::new(seed), planned_day: None, plans: HashMap::new(), active: HashMap::new(), serial: 0 }
    }

    pub fn active(&self) -> &HashMap<String, Experiment> {
        &self.active
    }

    pub fn active_in(&self, room: &str) -> Option<&Experiment> {
        self.active.get(room)
    }

    fn plan_day(&mut self, config: &Config, day: NaiveDate) {
        self.plans.clear();
        self.planned_day = Some(day);
        for room in config.rooms.iter().filter(|r| r.exploration.enabled) {
            let p = room.exploration.p_per_day.max(0.0);
            let count = p.floor() as u32 + u32::from(self.rng.unit() < p.fract());
            let mut plans = Vec::new();
            for _ in 0..count {
                let drawn = [Kind::Step, Kind::Step, Kind::NightSetback, Kind::ValveClosed, Kind::ValveOpen][self.rng.range(0, 4) as usize];
                'kinds: for kind in [drawn, Kind::Step] {
                    let minutes = match kind {
                        Kind::Step => self.rng.range(60, 180),
                        Kind::NightSetback => self.rng.range(120, 240),
                        _ => 30,
                    };
                    for _ in 0..PLACEMENT_TRIES {
                        let minute_of_day = if kind == Kind::NightSetback { (22 * 60 + self.rng.range(0, 5 * 60 - 1)) % 1440 } else { self.rng.range(0, 1439) };
                        let start = day.and_hms_opt(0, 0, 0).unwrap() + Duration::minutes(i64::from(minute_of_day));
                        if allowed(room, start, minutes) && !plans.iter().any(|p: &Plan| (p.start - start).num_minutes().abs() < 360) {
                            plans.push(Plan { kind, start, minutes });
                            break 'kinds;
                        }
                    }
                }
            }
            plans.sort_by_key(|p| p.start);
            self.plans.insert(room.name.clone(), plans);
        }
    }

    pub fn abort(&mut self, room: &str, reason: &'static str) -> Option<Lifecycle> {
        self.active.remove(room).map(|e| Lifecycle::Aborted(e, reason))
    }

    pub fn tick(&mut self, now: NaiveDateTime, inputs: &Inputs) -> Vec<Lifecycle> {
        let mut out = Vec::new();
        let mut rooms: Vec<String> = self.active.keys().cloned().collect();
        rooms.sort();
        for name in rooms {
            let reason = (inputs.blocked)(&name).or_else(|| inputs.config.rooms.iter().find(|r| r.name == name).filter(|r| !r.exploration.enabled).map(|_| "disabled"));
            if let Some(reason) = reason {
                out.extend(self.abort(&name, reason));
            } else if now >= self.active[&name].end {
                out.push(Lifecycle::Ended(self.active.remove(&name).unwrap()));
            }
        }
        if self.planned_day != Some(now.date()) {
            self.plan_day(inputs.config, now.date());
        }
        for room in inputs.config.rooms.iter().filter(|r| r.exploration.enabled) {
            if self.active.contains_key(&room.name) {
                continue;
            }
            let Some(plans) = self.plans.get_mut(&room.name) else { continue };
            plans.retain(|p| now < p.start + Duration::minutes(START_GRACE_MIN));
            let Some(i) = plans.iter().position(|p| p.start <= now) else { continue };
            if (inputs.blocked)(&room.name).is_some() || (inputs.no_raise)(&room.name) {
                continue;
            }
            let plan = plans.remove(i);
            let base = (inputs.base)(room);
            let up = self.rng.unit() < 0.5;
            let delta = f64::from(self.rng.range(2, 4)) * 0.5;
            let setpoint = target(plan.kind, room, inputs.config, base, up, delta).or_else(|| target(plan.kind, room, inputs.config, base, !up, delta));
            let Some(setpoint) = setpoint else { continue };
            self.serial += 1;
            let experiment = Experiment {
                id: format!("exp-{}-{}-{}", now.format("%Y%m%d%H%M"), room.name, self.serial),
                room: room.name.clone(),
                kind: plan.kind,
                setpoint,
                base,
                start: now,
                end: (plan.start + Duration::minutes(i64::from(plan.minutes))).max(now),
            };
            self.active.insert(room.name.clone(), experiment.clone());
            out.push(Lifecycle::Started(experiment));
        }
        out
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use chrono::Datelike;

    const CFG: &str = "
frost_setpoint = 7.0
default_setpoint = 18.0
[[rooms]]
name = 'a'
nodes = [1]
[rooms.exploration]
enabled = true
p_per_day = 2.0
comfort_min = 16.0
comfort_max = 22.0
[[rooms]]
name = 'b'
nodes = [2]
[rooms.exploration]
enabled = true
p_per_day = 1.0
comfort_min = 6.0
comfort_max = 21.0
blackout = [{ from = '22:00', to = '07:00' }]
[[rooms]]
name = 'off'
nodes = [3]
";

    fn run(seed: u64, windows: bool) -> Vec<(NaiveDateTime, Lifecycle)> {
        let config = Config::parse(CFG).unwrap();
        let mut explorer = Explorer::new(seed);
        let start = NaiveDate::from_ymd_opt(2025, 10, 27).unwrap().and_hms_opt(0, 0, 0).unwrap();
        let blocked = |_: &str| windows.then_some("window_open");
        let base = |_: &Room| 18.0;
        let inputs = Inputs { config: &config, blocked: &blocked, no_raise: &|_| false, base: &base };
        let mut out = Vec::new();
        for m in 0..1440 + 240 {
            let now = start + Duration::minutes(m);
            out.extend(explorer.tick(now, &inputs).into_iter().map(|l| (now, l)));
        }
        out
    }

    fn starts(events: &[(NaiveDateTime, Lifecycle)]) -> Vec<Experiment> {
        events.iter().filter_map(|(_, l)| if let Lifecycle::Started(e) = l { Some(e.clone()) } else { None }).filter(|e| e.start.date().day() == 27).collect()
    }

    #[test]
    fn rng_is_deterministic() {
        let (mut a, mut b) = (Rng::new(5), Rng::new(5));
        assert!((0..20).all(|_| a.next_u64() == b.next_u64()));
        assert_ne!(Rng::new(5).next_u64(), Rng::new(6).next_u64());
        let mut r = Rng::new(1);
        assert!((0..500).all(|_| (3..=7).contains(&r.range(3, 7)) && r.unit() < 1.0));
    }

    #[test]
    fn every_enabled_room_gets_experiments_within_24h_and_never_the_disabled_one() {
        for seed in 0..40 {
            let started = starts(&run(seed, false));
            let count = |room: &str| started.iter().filter(|e| e.room == room).count();
            assert!(count("a") >= 1 && count("b") >= 1, "seed {seed}: {started:?}");
            assert_eq!(count("off"), 0);
        }
        let ids = |seed| starts(&run(seed, false)).into_iter().map(|e| (e.id, e.setpoint)).collect::<Vec<_>>();
        assert_eq!(ids(7), ids(7));
        assert_ne!(ids(7), ids(8));
    }

    #[test]
    fn experiments_respect_bounds_blackout_durations_and_frost() {
        let config = Config::parse(CFG).unwrap();
        for seed in 0..40 {
            for e in starts(&run(seed, false)) {
                let room = config.rooms.iter().find(|r| r.name == e.room).unwrap();
                assert!(e.setpoint >= config.frost_setpoint && e.setpoint >= room.exploration.comfort_min && e.setpoint <= room.exploration.comfort_max, "{e:?}");
                assert!((e.setpoint - e.base).abs() >= MIN_STEP_C);
                let minutes = (e.end - e.start).num_minutes();
                let range = match e.kind {
                    Kind::Step => 60..=180,
                    Kind::NightSetback => 120..=240,
                    _ => 30..=30,
                };
                assert!(range.contains(&minutes), "{e:?}");
                assert!(allowed(room, e.start, minutes as u32), "{e:?}");
                if e.kind == Kind::Step {
                    assert!((e.setpoint - e.base).abs() <= 2.0 + 1e-9, "{e:?}");
                }
            }
        }
    }

    #[test]
    fn every_started_experiment_closes_exactly_once() {
        let events = run(3, false);
        for e in starts(&events) {
            let closed = events.iter().filter(|(_, c)| matches!(c, Lifecycle::Ended(x) | Lifecycle::Aborted(x, _) if x.id == e.id)).count();
            assert_eq!(closed, 1, "{}", e.id);
        }
    }

    #[test]
    fn window_open_blocks_all_starts_and_aborts_running() {
        assert!(starts(&run(3, true)).is_empty());
        let config = Config::parse(CFG).unwrap();
        let mut explorer = Explorer::new(3);
        let blocked = std::cell::Cell::new(false);
        let gate = |_: &str| blocked.get().then_some("window_open");
        let base = |_: &Room| 18.0;
        let inputs = Inputs { config: &config, blocked: &gate, no_raise: &|_| false, base: &base };
        let start = NaiveDate::from_ymd_opt(2025, 10, 27).unwrap().and_hms_opt(0, 0, 0).unwrap();
        let mut now = start;
        while explorer.active().is_empty() {
            explorer.tick(now, &inputs);
            now += Duration::minutes(1);
            assert!(now < start + Duration::days(1));
        }
        blocked.set(true);
        let out = explorer.tick(now, &inputs);
        assert!(out.iter().any(|l| matches!(l, Lifecycle::Aborted(_, "window_open"))));
        assert!(explorer.active().is_empty());
    }

    #[test]
    fn exhausted_raise_budget_blocks_starts_but_not_running_experiments() {
        let config = Config::parse(CFG).unwrap();
        let blocked = |_: &str| None;
        let base = |_: &Room| 18.0;
        let exhausted = std::cell::Cell::new(true);
        let no_raise = |_: &str| exhausted.get();
        let inputs = Inputs { config: &config, blocked: &blocked, no_raise: &no_raise, base: &base };
        let start = NaiveDate::from_ymd_opt(2025, 10, 27).unwrap().and_hms_opt(0, 0, 0).unwrap();
        let mut explorer = Explorer::new(3);
        let mut now = start;
        for _ in 0..1000 {
            explorer.tick(now, &inputs);
            now += Duration::minutes(1);
        }
        assert!(explorer.active().is_empty());
        exhausted.set(false);
        while explorer.active().is_empty() {
            explorer.tick(now, &inputs);
            now += Duration::minutes(1);
            assert!(now < start + Duration::days(1));
        }
        exhausted.set(true);
        assert!(explorer.tick(now, &inputs).iter().all(|l| !matches!(l, Lifecycle::Aborted(..))));
        assert!(!explorer.active().is_empty());
    }

    #[test]
    fn target_never_below_frost_even_with_low_comfort_min() {
        let config = Config::parse(CFG).unwrap();
        let room = &config.rooms[1];
        for kind in [Kind::Step, Kind::NightSetback, Kind::ValveClosed, Kind::ValveOpen] {
            for base in [7.0, 12.0, 18.0, 21.0] {
                for up in [true, false] {
                    if let Some(t) = target(kind, room, &config, base, up, 2.0) {
                        assert!((7.0..=21.0).contains(&t), "{kind:?} {base} {up} {t}");
                    }
                }
            }
        }
        assert_eq!(target(Kind::ValveClosed, room, &config, 18.0, true, 1.0), Some(7.0));
    }
}
