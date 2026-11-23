use crate::config::Config;
use crate::window::{TraceRow, WinEvent, Windows, LOCAL_TEMPERATURE, OCCUPIED_SETPOINT, PI_HEATING_DEMAND};
use crate::window_model::Bundle;
use std::collections::BTreeMap;
use std::path::{Path, PathBuf};

const MIN: i64 = 60_000;
const DAY: i64 = 24 * 60 * MIN;
const STEP: i64 = 20_000;
const AIR_CLUSTER_TEMP: u64 = 1026;
const TRUE_AIR_DROP_K: f64 = 0.15;
const TRUE_TRV_DROP_K: f64 = 0.5;
const MAX_FALSE_PER_ROOM_DAY: f64 = 0.2;

fn utc(text: &str) -> i64 {
    chrono::NaiveDateTime::parse_from_str(text, "%Y-%m-%dT%H:%M").unwrap().and_utc().timestamp_millis()
}

fn clock(ms: i64) -> String {
    chrono::DateTime::from_timestamp_millis(ms).unwrap().format("%m-%d %H:%M").to_string()
}

fn table(dir: &Path, name: &str) -> Vec<Vec<String>> {
    let text = std::fs::read_to_string(dir.join(name)).unwrap_or_else(|e| panic!("{name}: {e}"));
    let mut reader = csv_lines(&text);
    reader.remove(0);
    reader
}

fn csv_lines(text: &str) -> Vec<Vec<String>> {
    text.lines().map(split_row).collect()
}

fn split_row(line: &str) -> Vec<String> {
    let (mut out, mut cur, mut quoted) = (Vec::new(), String::new(), false);
    let mut chars = line.chars().peekable();
    while let Some(c) = chars.next() {
        match c {
            '"' if quoted && chars.peek() == Some(&'"') => {
                cur.push('"');
                chars.next();
            }
            '"' => quoted = !quoted,
            ',' if !quoted => out.push(std::mem::take(&mut cur)),
            _ => cur.push(c),
        }
    }
    out.push(cur);
    out
}

type Series = BTreeMap<String, Vec<(i64, f64)>>;

enum Feed {
    Trv(u64, &'static str, f64),
    Air(u64, u32, f64),
}

struct Dump {
    config: Config,
    feeds: Vec<(i64, Feed)>,
    weather: Vec<(i64, [f64; 3])>,
    answers: Vec<(i64, String, bool)>,
    air_temp: Series,
    trv_temp: Series,
    bundle: Bundle,
}

fn num(s: &str) -> f64 {
    s.parse().unwrap_or(f64::NAN)
}

impl Dump {
    fn load(dir: &Path, from_ms: i64) -> Self {
        let config = Config::parse(&std::fs::read_to_string(dir.join("config.toml")).expect("config.toml")).unwrap();
        let mut feeds: Vec<(i64, Feed)> = Vec::new();
        let (mut trv_temp, mut air_temp): (Series, Series) = Default::default();
        for r in table(dir, "trv.csv") {
            let (ms, node, attr, v): (i64, u64, u32, f64) = (r[0].parse().unwrap(), r[1].parse().unwrap(), r[2].parse().unwrap(), num(&r[3]));
            if ms < from_ms {
                continue;
            }
            let room = config.rooms.iter().find(|x| x.nodes.contains(&node)).map(|x| x.name.clone());
            match attr {
                0 => {
                    feeds.push((ms, Feed::Trv(node, LOCAL_TEMPERATURE, v / 100.0)));
                    trv_temp.entry(room.unwrap_or_default()).or_default().push((ms, v / 100.0));
                }
                8 => feeds.push((ms, Feed::Trv(node, PI_HEATING_DEMAND, v))),
                _ => feeds.push((ms, Feed::Trv(node, OCCUPIED_SETPOINT, v / 100.0))),
            }
        }
        for r in table(dir, "air.csv") {
            let (ms, node, cluster, v): (i64, u64, u32, f64) = (r[0].parse().unwrap(), r[1].parse().unwrap(), r[2].parse().unwrap(), num(&r[3]));
            if ms < from_ms {
                continue;
            }
            if cluster as u64 == AIR_CLUSTER_TEMP {
                let room = config.devices.iter().find(|d| d.node == node).and_then(|d| d.room.clone()).unwrap_or_default();
                air_temp.entry(room).or_default().push((ms, v / 100.0));
            }
            feeds.push((ms, Feed::Air(node, cluster, v)));
        }
        feeds.sort_by_key(|f| f.0);
        let weather = table(dir, "weather.csv").iter().map(|r| (r[0].parse().unwrap(), [num(&r[3]), num(&r[4]), num(&r[5])])).collect();
        let answers = table(dir, "labels.csv").iter().filter(|r| r[2] == "window" && (r[5] == "yes" || r[5] == "no")).map(|r| (r[0].parse().unwrap(), r[1].clone(), r[5] == "yes")).collect();
        let bundle = Bundle::load(dir).unwrap();
        Self { config, feeds, weather, answers, air_temp, trv_temp, bundle }
    }

    fn weather_at(&self, ms: i64, max_age_ms: i64) -> [f64; 3] {
        let i = self.weather.partition_point(|w| w.0 <= ms);
        if i == 0 || ms - self.weather[i - 1].0 > max_age_ms {
            return [f64::NAN; 3];
        }
        self.weather[i - 1].1
    }
}

#[derive(Debug, Clone)]
struct Episode {
    room: String,
    from: i64,
    to: Option<i64>,
    source: String,
    text: String,
}

struct Outcome {
    episodes: Vec<Episode>,
    trace: Vec<TraceRow>,
}

fn replay(dump: &Dump, config: &Config, from_ms: i64, to_ms: i64, with_trace: bool) -> Outcome {
    let mut w = Windows::default();
    w.set_model(dump.bundle.clone());
    w.set_trace(with_trace);
    let (mut now, mut next) = (from_ms - 2 * DAY / 2, 0usize);
    let mut set_prev: BTreeMap<u64, f64> = BTreeMap::new();
    let mut episodes: Vec<Episode> = Vec::new();
    let mut trace = Vec::new();
    let age_ms = (dump.bundle.weather_max_age_min * MIN as f64) as i64;
    while now <= to_ms {
        while next < dump.feeds.len() && dump.feeds[next].0 <= now {
            let (ts, feed) = &dump.feeds[next];
            match feed {
                Feed::Trv(node, attr, v) => {
                    w.observe(config, *node, attr, *v, *ts);
                    if *attr == OCCUPIED_SETPOINT {
                        if let (Some(prev), Some(room)) = (set_prev.insert(*node, *v), config.rooms.iter().find(|r| r.nodes.contains(node))) {
                            if *v < prev {
                                w.note_decrease(&room.name, *ts);
                            }
                        }
                    }
                }
                Feed::Air(node, cluster, v) => w.observe_air(config, *node, *cluster, *v, *ts),
            }
            next += 1;
        }
        let wx = dump.weather_at(now, age_ms);
        for event in w.step(config, now, |_| wx) {
            record(&mut episodes, event, now);
        }
        if with_trace {
            trace.extend(w.take_trace());
        }
        now += STEP;
    }
    Outcome { episodes, trace }
}

fn record(episodes: &mut Vec<Episode>, event: WinEvent, now: i64) {
    let room = event.room.clone();
    match event.kind {
        "window_open" => episodes.push(Episode { room, from: now, to: None, source: event.detail["source"].as_str().unwrap_or("").into(), text: event.detail["reason"].as_str().unwrap_or("").into() }),
        "window_close" => {
            if let Some(e) = episodes.iter_mut().rev().find(|e| e.room == room && e.to.is_none()) {
                e.to = Some(now);
            }
        }
        _ => {}
    }
}

fn open_at(episodes: &[Episode], room: &str, at: i64) -> bool {
    episodes.iter().any(|e| e.room == room && e.from <= at && e.to.is_none_or(|t| t > at))
}

fn covered(episodes: &[Episode], room: &str, from: i64, to: i64) -> bool {
    (from..=to).step_by(MIN as usize).all(|t| open_at(episodes, room, t))
}

fn first_open(episodes: &[Episode], room: &str, from: i64, to: i64) -> Option<i64> {
    (from..=to).step_by(STEP as usize).find(|t| open_at(episodes, room, *t))
}

fn drop_around(series: Option<&Vec<(i64, f64)>>, at: i64, base: (i64, i64), after: (i64, i64)) -> f64 {
    let Some(series) = series else { return 0.0 };
    let mut before: Vec<f64> = series.iter().filter(|s| s.0 >= at - base.0 && s.0 <= at - base.1).map(|s| s.1).collect();
    let low = series.iter().filter(|s| s.0 >= at - after.0 && s.0 <= at + after.1).map(|s| s.1).fold(f64::INFINITY, f64::min);
    if before.is_empty() || !low.is_finite() {
        return 0.0;
    }
    before.sort_by(|a, b| a.total_cmp(b));
    low - before[before.len() / 2]
}

fn verdict(dump: &Dump, e: &Episode, confirmed: &[(String, i64, i64)], strict: bool) -> &'static str {
    let (air_k, trv_k) = if strict { (2.0 * TRUE_AIR_DROP_K, 2.0 * TRUE_TRV_DROP_K) } else { (TRUE_AIR_DROP_K, TRUE_TRV_DROP_K) };
    let answered = dump.answers.iter().filter(|a| a.1 == e.room && a.0 >= e.from - 30 * MIN && a.0 <= e.to.unwrap_or(e.from) + 30 * MIN).max_by_key(|a| a.0);
    if let Some(a) = answered {
        return if a.2 { "true" } else { "false" };
    }
    if confirmed.iter().any(|c| c.0 == e.room && c.1 <= e.to.unwrap_or(i64::MAX) && c.2 >= e.from - 10 * MIN) {
        return "true";
    }
    if drop_around(dump.air_temp.get(&e.room), e.from, (15 * MIN, 4 * MIN), (4 * MIN, 30 * MIN)) <= -air_k || drop_around(dump.trv_temp.get(&e.room), e.from, (30 * MIN, 12 * MIN), (12 * MIN, 30 * MIN)) <= -trv_k {
        return "true";
    }
    "false"
}

fn confirmed() -> Vec<(String, i64, i64)> {
    let room = |r: &str, a: &str, b: &str| (r.to_string(), utc(a), utc(b));
    vec![
        room("schlafzimmer", "2026-11-23T07:27", "2026-11-23T07:36"),
        room("schlafzimmer", "2026-11-23T08:00", "2026-11-23T08:50"),
        room("kueche", "2026-11-22T09:43", "2026-11-22T09:48"),
        room("wohnzimmer", "2026-11-22T09:43", "2026-11-22T09:48"),
    ]
}

fn dump_dir() -> PathBuf {
    PathBuf::from(std::env::var("HEATD_AIRWIN_DUMP").expect("HEATD_AIRWIN_DUMP"))
}

fn config_for(dump: &Dump, legacy: bool) -> Config {
    let mut config = dump.config.clone();
    config.window.fusion.enabled = !legacy;
    if let Ok(path) = std::env::var("HEATD_AIRWIN_TABLES") {
        config.window.fusion.tables = path;
    }
    if std::env::var("HEATD_AIRWIN_SKIPRH").is_ok() {
        config.window.fusion.skip_humidity = vec!["sink".into()];
    }
    for pair in std::env::var("HEATD_AIRWIN_SET").unwrap_or_default().split(',').filter(|p| !p.is_empty()) {
        let (key, value) = pair.split_once('=').expect("key=value");
        let (f, v) = (&mut config.window.fusion, value.parse::<f64>().expect("number"));
        match key {
            "window_min" => f.window_min = v,
            "temper_scale" => f.temper_scale = v,
            "couple_llr" => f.couple_llr = v,
            "floor_margin" => f.floor_margin = v,
            "stay_min" => f.stay_min = v,
            "open_every_min" => f.open_every_min = v,
            "fresh_min" => f.fresh_min = v,
            "horizon_min" => f.horizon_min = v,
            other => panic!("unknown override {other}"),
        }
    }
    config
}

fn write_trace(path: &str, trace: &[TraceRow]) {
    let mut out = String::from("ms,room,z,valve,co2,rh,temp,gate,own,total\n");
    for t in trace.iter().filter(|t| t.ts_ms % MIN == 0) {
        out += &format!("{},{},{},{},{},{},{},{},{:.3},{:.3}\n", t.ts_ms, t.room, t.z, t.valve, t.co2, t.rh, t.temp, t.gate, t.own, t.total);
    }
    std::fs::write(path, out).unwrap();
}

#[test]
#[ignore]
fn replay_fusion() {
    let dir = dump_dir();
    let (from, to) = (utc("2026-11-09T20:00"), utc("2026-11-23T08:59"));
    let dump = Dump::load(&dir, from - 2 * DAY);
    let legacy = std::env::var("HEATD_AIRWIN_LEGACY").is_ok();
    let config = config_for(&dump, legacy);
    let want_trace = std::env::var("HEATD_AIRWIN_TRACE").ok();
    let out = replay(&dump, &config, from, to, want_trace.is_some());
    if let Some(path) = &want_trace {
        write_trace(path, &out.trace);
    }
    let conf = confirmed();
    let held: Vec<i64> = std::env::var("HEATD_AIRWIN_HELD").unwrap_or_default().split(',').filter(|d| !d.is_empty()).map(|d| utc(&format!("2026-11-{d}T00:00"))).collect();
    let days = (to - from) as f64 / DAY as f64;
    println!("replay {} .. {} ({days:.1} days) mode {}", clock(from), clock(to), if legacy { "legacy v3" } else { "fusion" });
    let rooms: Vec<String> = config.rooms.iter().filter(|r| !r.nodes.is_empty()).map(|r| r.name.clone()).collect();
    let mut worst: f64 = 0.0;
    for room in &rooms {
        let mine: Vec<&Episode> = out.episodes.iter().filter(|e| &e.room == room && e.from >= from).collect();
        let verdicts: Vec<&'static str> = mine.iter().map(|e| verdict(&dump, e, &conf, false)).collect();
        let strict_false = mine.iter().filter(|e| verdict(&dump, e, &conf, true) == "false").count();
        let falses = verdicts.iter().filter(|v| **v == "false").count();
        let on_held: Vec<(&&Episode, &&'static str)> = mine.iter().zip(&verdicts).filter(|(e, _)| held.iter().any(|d| e.from >= *d && e.from < d + DAY)).collect();
        let held_false = on_held.iter().filter(|(_, v)| **v == "false").count();
        let held_days = held.len() as f64;
        let by_source: BTreeMap<&str, usize> = mine.iter().fold(BTreeMap::new(), |mut m, e| {
            *m.entry(e.source.as_str()).or_default() += 1;
            m
        });
        println!("{room}: openings {} ({:.2}/day) true {} false {} ({:.2}/day, strict {strict_false} = {:.2}/day) sources {by_source:?}; held-out days false {held_false}/{held_days} ({:.2}/day)", mine.len(), mine.len() as f64 / days, verdicts.iter().filter(|v| **v == "true").count(), falses, falses as f64 / days, strict_false as f64 / days, if held_days > 0.0 { held_false as f64 / held_days } else { f64::NAN });
        worst = worst.max(if held_days > 0.0 { held_false as f64 / held_days } else { falses as f64 / days });
        if std::env::var("HEATD_AIRWIN_LIST").is_ok() {
            for (e, v) in mine.iter().zip(&verdicts) {
                println!("   {} .. {} {} {} {}", clock(e.from), e.to.map_or("open".into(), clock), e.source, v, e.text);
            }
        }
    }
    let after = |room: &str, from: &str, to: &str| first_open(&out.episodes, room, utc(from), utc(to));
    let a = after("schlafzimmer", "2026-11-23T07:20", "2026-11-23T07:40");
    let b = covered(&out.episodes, "schlafzimmer", utc("2026-11-23T08:06"), utc("2026-11-23T08:45"));
    let b_first = after("schlafzimmer", "2026-11-23T07:55", "2026-11-23T08:20");
    let kitchen = after("kueche", "2026-11-22T09:40", "2026-11-22T09:55");
    let living = after("wohnzimmer", "2026-11-22T09:40", "2026-11-22T09:55");
    println!("(a) bedroom phase A first frost {:?} (limit 07:33), phase B first {:?}, frost 08:06-08:45 continuous: {b}", a.map(clock), b_first.map(clock));
    println!("(b) 11-22 kitchen {:?}, living room {:?} (limit 09:46)", kitchen.map(clock), living.map(clock));
    println!("(c) worst room false openings per day: {worst:.2} (limit {MAX_FALSE_PER_ROOM_DAY})");
    if !legacy {
        assert!(a.is_some_and(|t| t <= utc("2026-11-23T07:33")) && b, "criterion (a) regressed");
    }
    for e in out.episodes.iter().filter(|e| e.from >= utc("2026-11-22T09:00") && e.from <= utc("2026-11-23T09:00") && e.source != "manual") {
        println!("   {} {} .. {} {} {}", e.room, clock(e.from), e.to.map_or("open".into(), clock), e.source, e.text);
    }
}
