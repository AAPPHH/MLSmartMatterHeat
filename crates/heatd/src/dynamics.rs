use crate::report::{dur, hhmm, midnight_us, open_lake, opt, quote, rows, LakeDb, ROOM_MINUTE_SQL};
use anyhow::{bail, Context, Result};
use chrono::NaiveDate;
use duckdb::types::{TimeUnit, Value};
use duckdb::{params, Connection};
use serde_json::Value as Json;
use std::collections::BTreeMap;
use std::fmt::Write;
use std::path::{Path, PathBuf};

pub const ROOM_DELTA_SQL: &str = include_str!("../../../analysis/views/room_delta.sql");
pub const RADIATOR_SQL: &str = include_str!("../../../analysis/views/radiator_summary.sql");
const MIN_US: i64 = 60_000_000;
const TAIL_US: i64 = 7 * 3_600_000_000;
const MIN_STEP: f64 = 0.5;
const SETPOINT_EPS: f64 = 0.05;
const SETTLE_MIN: usize = 3;
const MAX_TRANSIENT: usize = 360;
const MOVE_C: f64 = 0.1;
const REACH_C: f64 = 0.3;
const RATE_WINDOW: usize = 30;
const OVERSHOOT_WINDOW: usize = 60;
const DECISION_BEFORE_US: i64 = 3 * MIN_US;
const DECISION_AFTER_US: i64 = MIN_US;
pub const SUMMARY_DAYS: i64 = 14;
pub const VIEWS: [&str; 5] = ["step_responses", "room_delta", "room_delta_daily", "radiator_summary", "radiator_pairs"];
const DDL: &str = "CREATE OR REPLACE TABLE step_responses (room VARCHAR, node UBIGINT, t_start TIMESTAMP, source VARCHAR, direction VARCHAR, setpoint_old DOUBLE, setpoint_new DOUBLE, step_c DOUBLE, start_temp DOUBLE, dead_time_min DOUBLE, rate_c_per_h DOUBLE, rate_per_k DOUBLE, initial_valve_pct DOUBLE, rate_per_valve DOUBLE, time_to_target_min DOUBLE, overshoot_c DOUBLE, valve_mean_pct DOUBLE, valve_max_pct DOUBLE, outdoor_temp DOUBLE, radiation DOUBLE, others_heating INTEGER, exploration BOOLEAN, experiment_id VARCHAR)";

#[derive(Clone, Debug, Default)]
pub struct Minute {
    pub actual: Option<f64>,
    pub setpoint: Option<f64>,
    pub valve: Option<f64>,
    pub outdoor: Option<f64>,
    pub radiation: Option<f64>,
    pub exploration: bool,
}

#[derive(Clone, Debug, Default)]
pub struct Series {
    pub room: String,
    pub start_min: i64,
    pub minutes: Vec<Minute>,
}

#[derive(Clone, Debug)]
pub struct Command {
    pub us: i64,
    pub node: u64,
    pub rule: String,
    pub setpoint: f64,
    pub experiment_id: Option<String>,
}

#[derive(Clone, Debug, Default)]
pub struct Step {
    pub room: String,
    pub node: u64,
    pub start_us: i64,
    pub source: String,
    pub heat_up: bool,
    pub old: f64,
    pub new: f64,
    pub start_temp: Option<f64>,
    pub dead_time_min: Option<f64>,
    pub rate_c_per_h: Option<f64>,
    pub initial_valve: Option<f64>,
    pub rate_per_valve: Option<f64>,
    pub time_to_target_min: Option<f64>,
    pub overshoot_c: Option<f64>,
    pub valve_mean: Option<f64>,
    pub valve_max: Option<f64>,
    pub outdoor: Option<f64>,
    pub radiation: Option<f64>,
    pub others_heating: i32,
    pub exploration: bool,
    pub experiment_id: Option<String>,
}

impl Step {
    pub fn size(&self) -> f64 {
        self.new - self.old
    }
}

fn mean(values: impl Iterator<Item = f64>) -> Option<f64> {
    let (sum, n) = values.fold((0.0, 0usize), |(s, n), v| (s + v, n + 1));
    (n > 0).then(|| sum / n as f64)
}

fn slope_per_min(points: &[(f64, f64)]) -> Option<f64> {
    if points.len() < 8 {
        return None;
    }
    let n = points.len() as f64;
    let (mx, my) = (points.iter().map(|p| p.0).sum::<f64>() / n, points.iter().map(|p| p.1).sum::<f64>() / n);
    let var: f64 = points.iter().map(|p| (p.0 - mx).powi(2)).sum();
    (var > 0.0).then(|| points.iter().map(|p| (p.0 - mx) * (p.1 - my)).sum::<f64>() / var)
}

pub fn detect_steps(setpoint: &[Option<f64>]) -> Vec<(usize, f64, f64)> {
    let mut out = Vec::new();
    let mut level: Option<f64> = None;
    let mut i = 0;
    while i < setpoint.len() {
        let Some(v) = setpoint[i] else {
            i += 1;
            continue;
        };
        match level {
            Some(l) if (v - l).abs() >= SETPOINT_EPS => {
                let (mut target, mut last, mut k) = (v, i, i + 1);
                while k < setpoint.len() && k - last <= SETTLE_MIN {
                    if let Some(w) = setpoint[k].filter(|w| (w - target).abs() >= SETPOINT_EPS) {
                        target = w;
                        last = k;
                    }
                    k += 1;
                }
                if (target - l).abs() >= MIN_STEP {
                    out.push((i, l, target));
                }
                level = Some(target);
                i = k;
            }
            None => {
                level = Some(v);
                i += 1;
            }
            _ => i += 1,
        }
    }
    out
}

struct Env<'a> {
    all: &'a [Series],
    first_nodes: &'a BTreeMap<String, u64>,
}

fn analyse(cx: &Env, series: &Series, idx: usize, next_start: usize, old: f64, new: f64, command: Option<&Command>) -> Step {
    let s = &series.minutes;
    let sign = (new - old).signum();
    let end = (idx + MAX_TRANSIENT).min(next_start).min(s.len());
    let base = idx.checked_sub(1).and_then(|k| s[k].actual).or(s[idx].actual);
    let actual = |k: usize| s.get(k).and_then(|m| m.actual);
    let dead = base.and_then(|b| (idx..end).find(|&k| actual(k).is_some_and(|a| sign * (a - b) >= MOVE_C)));
    let window = |from: usize| (from..(from + RATE_WINDOW).min(end)).collect::<Vec<_>>();
    let rate = dead.and_then(|k0| {
        let pts: Vec<(f64, f64)> = window(k0).into_iter().filter_map(|k| actual(k).map(|a| ((k - k0) as f64, a))).collect();
        slope_per_min(&pts).map(|v| v * 60.0 * sign)
    });
    let initial_valve = dead.and_then(|k0| mean(window(k0).into_iter().filter_map(|k| s[k].valve)));
    let reached = (idx..end).find(|&k| actual(k).is_some_and(|a| sign * (a - new) >= -REACH_C));
    let overshoot = reached.map(|k| {
        let hi = (k + OVERSHOOT_WINDOW).min(next_start).min(s.len());
        (k..hi).filter_map(actual).map(|a| sign * (a - new)).fold(0.0f64, f64::max)
    });
    let transient = idx..reached.unwrap_or(end);
    let valves: Vec<f64> = transient.clone().filter_map(|k| s[k].valve).collect();
    let others = cx
        .all
        .iter()
        .filter(|o| o.room != series.room)
        .filter(|o| {
            let at = (series.start_min + idx as i64 - o.start_min).max(0) as usize;
            (at..(at + RATE_WINDOW).min(o.minutes.len())).any(|k| o.minutes[k].valve.is_some_and(|v| v > 0.0))
        })
        .count() as i32;
    Step {
        room: series.room.clone(),
        node: command.map_or(cx.first_nodes.get(&series.room).copied().unwrap_or(0), |c| c.node),
        start_us: (series.start_min + idx as i64) * MIN_US,
        source: command.map_or("device".into(), |c| c.rule.clone()),
        heat_up: sign > 0.0,
        old,
        new,
        start_temp: base,
        dead_time_min: dead.map(|k| (k - idx) as f64),
        rate_c_per_h: rate,
        initial_valve,
        rate_per_valve: rate.zip(initial_valve).filter(|(_, v)| *v > 0.0).map(|(r, v)| r / v),
        time_to_target_min: reached.map(|k| (k - idx) as f64),
        overshoot_c: overshoot,
        valve_mean: mean(valves.iter().copied()),
        valve_max: valves.iter().copied().reduce(f64::max),
        outdoor: s[idx].outdoor,
        radiation: s[idx].radiation,
        others_heating: others,
        exploration: transient.clone().any(|k| s[k].exploration),
        experiment_id: command.and_then(|c| c.experiment_id.clone()),
    }
}

pub fn step_responses(all: &[Series], commands: &BTreeMap<String, Vec<Command>>, first_nodes: &BTreeMap<String, u64>, until_us: i64) -> Vec<Step> {
    let cx = Env { all, first_nodes };
    let mut out = Vec::new();
    for series in all {
        let sp: Vec<Option<f64>> = series.minutes.iter().map(|m| m.setpoint).collect();
        let found = detect_steps(&sp);
        for (n, &(idx, old, new)) in found.iter().enumerate() {
            let start_us = (series.start_min + idx as i64) * MIN_US;
            if start_us >= until_us {
                continue;
            }
            let next_start = found.get(n + 1).map_or(usize::MAX, |f| f.0);
            let command = commands
                .get(&series.room)
                .and_then(|c| c.iter().filter(|c| c.us >= start_us - DECISION_BEFORE_US && c.us < start_us + DECISION_AFTER_US && (c.setpoint - new).abs() < 0.6).max_by_key(|c| c.us));
            out.push(analyse(&cx, series, idx, next_start, old, new, command));
        }
    }
    out.sort_by(|a, b| (a.start_us, &a.room).cmp(&(b.start_us, &b.room)));
    out
}

fn load_series(conn: &Connection) -> Result<Vec<Series>> {
    let data = rows(
        conn,
        "SELECT room, epoch_us(t) // 60000000, actual, setpoint, valve_pct, outdoor_temp, radiation, exploration, gap_mask FROM room_minute ORDER BY room, t",
        |r| {
            let gap = r.get::<_, i32>(8)?;
            let usable = |bit: i32, v: Option<f64>| if gap & bit == 0 { v } else { None };
            Ok((
                r.get::<_, String>(0)?,
                r.get::<_, i64>(1)?,
                Minute { actual: usable(1, r.get(2)?), setpoint: usable(2, r.get(3)?), valve: usable(4, r.get(4)?), outdoor: usable(8, r.get(5)?), radiation: usable(8, r.get(6)?), exploration: r.get(7)? },
            ))
        },
    )?;
    let mut out: Vec<Series> = Vec::new();
    for (room, m, minute) in data {
        if out.last().is_none_or(|s| s.room != room) {
            out.push(Series { room, start_min: m, minutes: Vec::new() });
        }
        out.last_mut().unwrap().minutes.push(minute);
    }
    Ok(out)
}

fn load_commands(conn: &Connection) -> Result<BTreeMap<String, Vec<Command>>> {
    let mut out: BTreeMap<String, Vec<Command>> = BTreeMap::new();
    for (room, c) in rows(conn, "SELECT room, epoch_us(ts), node, rule, setpoint_new, experiment_id FROM decisions WHERE applied AND setpoint_new IS NOT NULL", |r| {
        Ok((r.get::<_, String>(0)?, Command { us: r.get(1)?, node: r.get(2)?, rule: r.get(3)?, setpoint: r.get(4)?, experiment_id: r.get(5)? }))
    })? {
        out.entry(room).or_default().push(c);
    }
    Ok(out)
}

fn first_nodes(conn: &Connection) -> Result<BTreeMap<String, u64>> {
    let json = rows(conn, "SELECT arg_max(value, ts) FROM meta WHERE key = 'rooms'", |r| r.get::<_, Option<String>>(0))?.into_iter().next().flatten();
    let mut out = BTreeMap::new();
    if let Some(Ok(Json::Array(rooms))) = json.as_deref().map(serde_json::from_str::<Json>) {
        for room in rooms {
            if let (Some(name), Some(node)) = (room["name"].as_str(), room["nodes"].as_array().and_then(|n| n.iter().filter_map(Json::as_u64).min())) {
                out.insert(name.to_string(), node);
            }
        }
    }
    Ok(out)
}

fn store(conn: &Connection, steps: &[Step]) -> Result<()> {
    conn.execute_batch(DDL)?;
    let mut app = conn.appender("step_responses")?;
    for s in steps {
        app.append_row(params![
            s.room,
            s.node,
            Value::Timestamp(TimeUnit::Microsecond, s.start_us),
            s.source,
            if s.heat_up { "heat_up" } else { "cool_down" },
            s.old,
            s.new,
            s.size(),
            s.start_temp,
            s.dead_time_min,
            s.rate_c_per_h,
            s.rate_c_per_h.map(|r| r / s.size().abs()),
            s.initial_valve,
            s.rate_per_valve,
            s.time_to_target_min,
            s.overshoot_c,
            s.valve_mean,
            s.valve_max,
            s.outdoor,
            s.radiation,
            s.others_heating,
            s.exploration,
            s.experiment_id
        ])?;
    }
    app.flush()?;
    Ok(())
}

pub fn open_dynamics(root: &Path, from: NaiveDate, to: NaiveDate) -> Result<LakeDb> {
    let db = open_lake(root, from - chrono::Duration::days(2), to + chrono::Duration::days(2))?;
    let (from_us, to_us) = (midnight_us(from), midnight_us(to + chrono::Duration::days(1)));
    db.conn.execute_batch(&format!("SET VARIABLE from_us = {from_us}; SET VARIABLE to_us = {}; SET VARIABLE delta_to_us = {to_us};", to_us + TAIL_US))?;
    db.conn.execute_batch(ROOM_MINUTE_SQL).context("room_minute view")?;
    db.conn.execute_batch(ROOM_DELTA_SQL).context("room_delta views")?;
    let steps = step_responses(&load_series(&db.conn)?, &load_commands(&db.conn)?, &first_nodes(&db.conn)?, to_us);
    store(&db.conn, &steps)?;
    db.conn.execute_batch(RADIATOR_SQL).context("radiator views")?;
    Ok(db)
}

pub fn export_view(root: &Path, view: &str, room: Option<&str>, from: NaiveDate, to: NaiveDate, out: Option<PathBuf>) -> Result<(PathBuf, usize)> {
    if !VIEWS.contains(&view) {
        bail!("unknown view {view} (one of {}, room_minute)", VIEWS.join(", "));
    }
    if to < from {
        bail!("--to {to} is before --from {from}");
    }
    crate::report::roll_missing(root, from - chrono::Duration::days(2), to, chrono::Utc::now().date_naive());
    let db = open_dynamics(root, from, to)?;
    let filter = room.filter(|_| view != "radiator_pairs").map_or(String::new(), |r| format!(" WHERE room = {}", quote(r)));
    let order = match view {
        "room_delta" => " ORDER BY room, t",
        "room_delta_daily" => " ORDER BY room, day",
        "step_responses" => " ORDER BY t_start, room",
        _ => " ORDER BY room",
    };
    let count: i64 = db.conn.query_row(&format!("SELECT count(*) FROM {view}{filter}"), [], |r| r.get(0))?;
    let path = out.unwrap_or_else(|| root.join("exports").join(format!("{view}_{from}_{to}.parquet")));
    if let Some(dir) = path.parent().filter(|d| !d.as_os_str().is_empty()) {
        std::fs::create_dir_all(dir)?;
    }
    db.conn.execute_batch(&format!("COPY (SELECT * FROM {view}{filter}{order}) TO {} (FORMAT parquet, COMPRESSION zstd);", quote(&path.to_string_lossy().replace('\\', "/"))))?;
    Ok((path, count as usize))
}

fn de(v: Option<f64>, digits: usize) -> String {
    opt(v, digits).replace('.', ",")
}

fn minutes(v: Option<f64>) -> String {
    v.map_or("-".into(), |m| dur(m * 60.0))
}

type DailyRow = (String, i64, Option<f64>, Option<f64>, Option<f64>, Option<f64>, i64, i64, i64);
type StepRow = (i64, String, String, String, Vec<Option<f64>>, i32, bool);
type SummaryRow = (String, u64, i64, i64, Vec<Option<f64>>, i64);

pub fn report_section(root: &Path, date: NaiveDate) -> Result<String> {
    let db = open_dynamics(root, date - chrono::Duration::days(SUMMARY_DAYS - 1), date)?;
    let c = &db.conn;
    let mut md = String::from("## Soll/Ist und Nachregeln\n\nDelta = gemessen minus eingestellt (positiv: zu warm). Nur Minuten ohne Datenluecke; Exploration-Minuten getrennt. Das Thermostat misst direkt am Heizkoerper (kein Raumthermometer): Anstiegsraten und Zeiten sind optimistisch.\n\n### Abweichung je Raum (Tag)\n\n");
    let daily: Vec<DailyRow> = rows(
        c,
        &format!("SELECT room, minutes, mean_delta, mean_abs_delta, p90_abs_delta, band_share, heating_minutes, exploration_minutes, gap_minutes FROM room_delta_daily WHERE day = DATE '{date}' ORDER BY room"),
        |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?, r.get(3)?, r.get(4)?, r.get(5)?, r.get(6)?, r.get(7)?, r.get(8)?)),
    )?;
    if daily.is_empty() {
        md.push_str("Keine Raeume mit Daten.\n\n");
    } else {
        md.push_str("| Raum | Minuten | mittleres Delta C | mittlerer Betrag C | P90 Betrag C | im Band 0,5 C | Heizminuten | Exploration Min. | Luecken Min. |\n|---|---|---|---|---|---|---|---|---|\n");
        for (room, n, d, a, p, band, heat, expl, gap) in daily {
            let _ = writeln!(md, "| {room} | {n} | {} | {} | {} | {} | {heat} | {expl} | {gap} |", de(d, 2), de(a, 2), de(p, 2), band.map_or("-".into(), |b| format!("{:.0} %", b * 100.0)));
        }
        md.push('\n');
    }
    md.push_str("### Sprungantworten des Tages\n\n");
    let steps: Vec<StepRow> = rows(
        c,
        &format!(
            "SELECT epoch_us(t_start), room, source, direction, step_c, start_temp, dead_time_min, rate_c_per_h, time_to_target_min, overshoot_c, valve_mean_pct, valve_max_pct, outdoor_temp, radiation, others_heating, exploration
             FROM step_responses WHERE CAST(t_start AS DATE) = DATE '{date}' ORDER BY t_start, room"
        ),
        |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?, r.get(3)?, (4..14).map(|i| r.get::<_, Option<f64>>(i)).collect::<duckdb::Result<_>>()?, r.get(14)?, r.get(15)?)),
    )?;
    if steps.is_empty() {
        md.push_str("Keine Sollwertspruenge ab 0,5 C.\n\n");
    } else {
        md.push_str("| Zeit | Raum | Quelle | Richtung | Sprung C | Start C | Totzeit | Anstieg C/h | bis Ziel | Ueberschwingen C | Ventil mittel/max % | Aussen C | Strahlung W/m2 | andere heizen | Exploration |\n|---|---|---|---|---|---|---|---|---|---|---|---|---|---|---|\n");
        for (us, room, source, dir, v, others, expl) in steps {
            let _ = writeln!(
                md,
                "| {} | {room} | {source} | {} | {} | {} | {} | {} | {} | {} | {}/{} | {} | {} | {others} | {} |",
                hhmm(us),
                if dir == "heat_up" { "auf" } else { "ab" },
                de(v[0], 1),
                de(v[1], 1),
                minutes(v[2]),
                de(v[3], 2),
                minutes(v[4]),
                de(v[5], 2),
                de(v[6], 0),
                de(v[7], 0),
                de(v[8], 1),
                de(v[9], 0),
                if expl { "ja" } else { "nein" }
            );
        }
        md.push('\n');
    }
    let _ = writeln!(md, "### Heizkoerper ({SUMMARY_DAYS} Tage)\n");
    let summary: Vec<SummaryRow> = rows(
        c,
        "SELECT room, node, steps, heat_up_steps, median_dead_time_min, median_rate_per_k, median_rate_per_valve, median_settle_min, median_overshoot_c, rel_rate_vs_peers, pair_count FROM radiator_summary ORDER BY room, node",
        |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?, r.get(3)?, (4..10).map(|i| r.get::<_, Option<f64>>(i)).collect::<duckdb::Result<_>>()?, r.get(10)?)),
    )?;
    if summary.is_empty() {
        md.push_str("Keine Sprungantworten im Zeitraum.\n\n");
    } else {
        md.push_str("| Raum | Knoten | Spruenge (auf) | Totzeit (Median) | Anstieg C/h je K Sprung | Anstieg C/h je % Ventil | bis Ziel (Median) | Ueberschwingen C | Rate relativ zu Nachbarn (Paare) |\n|---|---|---|---|---|---|---|---|---|\n");
        for (room, node, n, up, v, pairs) in summary {
            let _ = writeln!(md, "| {room} | {node} | {n} ({up}) | {} | {} | {} | {} | {} | {} ({pairs}) |", minutes(v[0]), de(v[1], 2), de(v[2], 3), minutes(v[3]), de(v[4], 2), de(v[5], 2));
        }
        md.push_str("\nRelativ-Wert: Median des Verhaeltnisses der Aufheizrate je K Sprung gegen andere Raeume bei gleichzeitigem Sprung, aehnlicher Ventiloeffnung und Aussentemperatur. Abweichungen deuten auf hydraulischen Abgleich oder Vorlauftemperatur hin, sind aber nur ein erster Hinweis.\n\n");
    }
    Ok(md)
}
