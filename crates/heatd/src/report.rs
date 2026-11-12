use crate::lake::{self, Sink, Table};
use crate::status::Status;
use anyhow::{anyhow, bail, Context, Result};
use chrono::{NaiveDate, NaiveTime};
use duckdb::Connection;
use serde_json::Value;
use std::collections::BTreeMap;
use std::fmt::Write;
use std::path::{Path, PathBuf};
use std::time::Duration;

pub const ROOM_MINUTE_SQL: &str = include_str!("../../../analysis/views/room_minute.sql");
#[cfg(test)]
pub const WATCH_MINUTE_SQL: &str = include_str!("../../../analysis/views/watch_minute.sql");
#[cfg(test)]
pub const WATCH_ROOM_CALIB_SQL: &str = include_str!("../../../analysis/views/watch_room_calib.sql");
const REPORT_HOUR: u32 = 3;
const REPORT_MINUTE: u32 = 10;
const DAY_US: i64 = 86_400_000_000;
const CATCH_UP_DELAY: Duration = Duration::from_secs(120);
const RSSI_PATTERN: &str = r#"(?i)"(?:average_?rssi|last_?rssi)"\s*:\s*(-?\d+)"#;
const LQI_PATTERN: &str = r#"(?i)"(?:lqi|link_?quality_?in|average_?lqi)"\s*:\s*(\d+)"#;

pub(crate) fn quote(text: &str) -> String {
    format!("'{}'", text.replace('\'', "''"))
}

fn parquet_files(root: &Path, table: Table, from: NaiveDate, to: NaiveDate) -> Vec<PathBuf> {
    let mut out = Vec::new();
    let mut day = from;
    while day <= to {
        let dir = lake::parquet_dir(root, table, day);
        if table.node_partitioned() {
            if let Ok(entries) = std::fs::read_dir(&dir) {
                let mut nodes: Vec<PathBuf> = entries.flatten().map(|e| e.path().join("part.parquet")).filter(|p| p.exists()).collect();
                nodes.sort();
                out.extend(nodes);
            }
        } else if dir.join("part.parquet").exists() {
            out.push(dir.join("part.parquet"));
        }
        day += chrono::Duration::days(1);
    }
    out
}

pub struct LakeDb {
    pub conn: Connection,
    scratch: Vec<PathBuf>,
}

impl Drop for LakeDb {
    fn drop(&mut self) {
        self.scratch.iter().for_each(|p| {
            let _ = std::fs::remove_file(p);
        });
    }
}

pub fn open_lake(root: &Path, first: NaiveDate, last: NaiveDate) -> Result<LakeDb> {
    let conn = Connection::open_in_memory()?;
    let mut scratch = Vec::new();
    for table in Table::ALL {
        let mut files = parquet_files(root, table, first, last);
        if files.is_empty() {
            static SCRATCH: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);
            let empty = std::env::temp_dir().join(format!("heatd_empty_{}_{}_{}.parquet", std::process::id(), SCRATCH.fetch_add(1, std::sync::atomic::Ordering::Relaxed), table.name()));
            lake::write_parquet(&empty, &table.schema(), &[])?;
            scratch.push(empty.clone());
            files.push(empty);
        }
        let list: Vec<String> = files.iter().map(|f| quote(&f.to_string_lossy().replace('\\', "/"))).collect();
        conn.execute_batch(&format!("CREATE OR REPLACE VIEW {} AS SELECT * FROM read_parquet([{}], union_by_name = true);", table.name(), list.join(", ")))
            .with_context(|| format!("view {}", table.name()))?;
    }
    Ok(LakeDb { conn, scratch })
}

pub(crate) fn midnight_us(date: NaiveDate) -> i64 {
    date.and_time(NaiveTime::MIN).and_utc().timestamp_micros()
}

fn set_window(conn: &Connection, from: NaiveDate, to_exclusive: NaiveDate) -> Result<()> {
    conn.execute_batch(&format!(
        "SET VARIABLE from_us = {}; SET VARIABLE to_us = {};",
        midnight_us(from),
        midnight_us(to_exclusive)
    ))?;
    Ok(())
}

pub fn open_room_minute(root: &Path, from: NaiveDate, to: NaiveDate) -> Result<LakeDb> {
    let db = open_lake(root, from - chrono::Duration::days(2), to + chrono::Duration::days(1))?;
    set_window(&db.conn, from, to + chrono::Duration::days(1))?;
    db.conn.execute_batch(ROOM_MINUTE_SQL).context("room_minute view")?;
    Ok(db)
}

#[cfg(test)]
pub fn open_watch_views(root: &Path, from: NaiveDate, to: NaiveDate, state: &crate::config::WatchState, local_offset_min: i32) -> Result<LakeDb> {
    let db = open_lake(root, from - chrono::Duration::days(2), to + chrono::Duration::days(1))?;
    set_window(&db.conn, from, to + chrono::Duration::days(1))?;
    let (sleep_from, sleep_to) = state.sleep_minutes()?;
    db.conn.execute_batch(&format!(
        "SET VARIABLE hr_rest_max = {}; SET VARIABLE hr_active_min = {}; SET VARIABLE motion_active_min = {}; SET VARIABLE sleep_from_min = {sleep_from}; SET VARIABLE sleep_to_min = {sleep_to}; SET VARIABLE local_offset_min = {local_offset_min};",
        state.hr_rest_max, state.hr_active_min, state.motion_active_min
    ))?;
    db.conn.execute_batch(WATCH_MINUTE_SQL).context("watch_minute view")?;
    db.conn.execute_batch(WATCH_ROOM_CALIB_SQL).context("watch_room_calib view")?;
    Ok(db)
}

pub fn roll_missing(root: &Path, from: NaiveDate, to: NaiveDate, today: NaiveDate) {
    let mut day = from;
    while day <= to.min(today - chrono::Duration::days(1)) {
        for r in lake::roll_day(root, day) {
            if let lake::RollStatus::Failed(e) = r.status {
                eprintln!("roll {day} {}: {e}", r.table.name());
            }
        }
        day += chrono::Duration::days(1);
    }
}

pub fn export(root: &Path, room: &str, from: NaiveDate, to: NaiveDate, out: Option<PathBuf>) -> Result<(PathBuf, usize)> {
    if to < from {
        bail!("--to {to} is before --from {from}");
    }
    roll_missing(root, from - chrono::Duration::days(2), to, chrono::Utc::now().date_naive());
    let db = open_room_minute(root, from, to)?;
    let rows: i64 = db.conn.query_row(&format!("SELECT count(*) FROM room_minute WHERE room = {}", quote(room)), [], |r| r.get(0))?;
    if rows == 0 {
        let mut stmt = db.conn.prepare("SELECT DISTINCT room FROM room_minute ORDER BY room")?;
        let known: Vec<String> = stmt.query_map([], |r| r.get(0))?.collect::<std::result::Result<_, _>>()?;
        bail!("unknown room {room} (meta rooms: {})", if known.is_empty() { "none".into() } else { known.join(", ") });
    }
    let path = out.unwrap_or_else(|| root.join("exports").join(format!("{room}_{from}_{to}.parquet")));
    if let Some(dir) = path.parent().filter(|d| !d.as_os_str().is_empty()) {
        std::fs::create_dir_all(dir)?;
    }
    db.conn.execute_batch(&format!(
        "COPY (SELECT * FROM room_minute WHERE room = {} ORDER BY t) TO {} (FORMAT parquet, COMPRESSION zstd);",
        quote(room),
        quote(&path.to_string_lossy().replace('\\', "/"))
    ))?;
    Ok((path, rows as usize))
}

pub(crate) fn hhmm(us: i64) -> String {
    chrono::DateTime::from_timestamp_micros(us).map_or("?".into(), |t| t.format("%H:%M:%S").to_string())
}

pub(crate) fn dur(seconds: f64) -> String {
    match seconds {
        s if s >= 3600.0 => format!("{:.1} h", s / 3600.0),
        s if s >= 120.0 => format!("{:.0} min", s / 60.0),
        s => format!("{s:.0} s"),
    }
}

pub(crate) fn opt(v: Option<f64>, digits: usize) -> String {
    v.map_or("-".into(), |v| format!("{v:.digits$}"))
}

fn trend(first: Option<f64>, second: Option<f64>, digits: usize) -> String {
    match (first, second) {
        (Some(a), Some(b)) => format!("{a:.digits$} -> {b:.digits$} ({:+.digits$})", b - a),
        _ => "-".into(),
    }
}

type Rows<T> = Result<Vec<T>>;

pub(crate) fn rows<T>(conn: &Connection, sql: &str, map: impl Fn(&duckdb::Row) -> duckdb::Result<T>) -> Rows<T> {
    let mut stmt = conn.prepare(sql).with_context(|| sql.to_string())?;
    let out = stmt.query_map([], |r| map(r))?.collect::<std::result::Result<Vec<_>, _>>()?;
    Ok(out)
}

#[derive(Debug, Default, Clone, PartialEq)]
pub struct DaySummary {
    pub reports: u64,
    pub nodes: usize,
    pub longest_gap_s: f64,
    pub longest_gap_node: Option<u64>,
    pub labels: u64,
    pub missing_weather_hours: Vec<u32>,
    pub weather_configured: bool,
    pub experiments: u64,
    pub roll_ok: bool,
    pub watch_inferences: u64,
    pub watch_abstained: u64,
    pub watch_battery_per_h: Option<f64>,
}

pub fn roll_markers(root: &Path, date: NaiveDate) -> Vec<(Table, Option<Value>, bool)> {
    Table::ALL
        .into_iter()
        .map(|t| {
            let marker = std::fs::read_to_string(lake::parquet_dir(root, t, date).join(".rolled")).ok().and_then(|s| serde_json::from_str::<Value>(&s).ok());
            (t, marker, lake::raw_dates(root).contains(&date) && !lake::raw_files(root, t, Some(date)).unwrap_or_default().is_empty())
        })
        .collect()
}

pub fn build(root: &Path, date: NaiveDate) -> Result<(String, DaySummary)> {
    let db = open_lake(root, date - chrono::Duration::days(1), date + chrono::Duration::days(1))?;
    let c = &db.conn;
    let (lo, hi) = (midnight_us(date), midnight_us(date) + DAY_US);
    let day = format!("epoch_us(ts) >= {lo} AND epoch_us(ts) < {hi}");
    let mid = lo + DAY_US / 2;
    let mut sum = DaySummary::default();
    let mut md = String::new();
    let _ = writeln!(md, "# heatd daily report {date}\n\nUTC day {date}, heatd {} (schema {}), generated {}. Rows are selected by `ts` from the Parquet partitions of {} to {}.\n", lake::COMMIT, lake::SCHEMA_VERSION, chrono::Utc::now().format("%Y-%m-%d %H:%M:%S UTC"), date - chrono::Duration::days(1), date + chrono::Duration::days(1));

    let rooms_json: Option<String> = rows(c, "SELECT arg_max(value, ts) FROM meta WHERE key = 'rooms'", |r| r.get::<_, Option<String>>(0))?.into_iter().next().flatten();
    let mut room_of_node: BTreeMap<u64, String> = BTreeMap::new();
    if let Some(Ok(Value::Array(rooms))) = rooms_json.as_deref().map(serde_json::from_str::<Value>) {
        for room in rooms {
            for node in room["nodes"].as_array().into_iter().flatten().filter_map(Value::as_u64) {
                room_of_node.insert(node, room["name"].as_str().unwrap_or("?").to_string());
            }
        }
    }
    sum.weather_configured = !rows(c, "SELECT 1 FROM meta WHERE key = 'coordinates' LIMIT 1", |r| r.get::<_, i32>(0))?.is_empty();

    let per_node = rows(
        c,
        &format!(
            "SELECT node, count(*), count(*) FILTER (WHERE source = 'subscription'), count(*) FILTER (WHERE source = 'poll'), min(epoch_us(ts)), max(epoch_us(ts)), max(gap)
             FROM (SELECT node, source, ts, (epoch_us(ts) - lag(epoch_us(ts)) OVER (PARTITION BY node ORDER BY ts)) / 1e6 AS gap FROM attribute_reports WHERE {day}) GROUP BY node ORDER BY node"
        ),
        |r| Ok((r.get::<_, u64>(0)?, r.get::<_, i64>(1)?, r.get::<_, i64>(2)?, r.get::<_, i64>(3)?, r.get::<_, i64>(4)?, r.get::<_, i64>(5)?, r.get::<_, Option<f64>>(6)?)),
    )?;
    let battery: BTreeMap<u64, (Option<f64>, Option<f64>)> = rows(
        c,
        &format!("SELECT node, arg_max(v_i64, ts) / 2.0, min(v_i64) / 2.0 FROM attribute_reports WHERE cluster = 47 AND attribute = 12 AND v_i64 IS NOT NULL AND {day} GROUP BY node"),
        |r| Ok((r.get::<_, u64>(0)?, (r.get::<_, Option<f64>>(1)?, r.get::<_, Option<f64>>(2)?))),
    )?
    .into_iter()
    .collect();
    md.push_str("## Reports per node\n\n| node | room | reports | subscription | poll | first | last | longest gap | battery last / min % |\n|---|---|---|---|---|---|---|---|---|\n");
    let mut seen = std::collections::BTreeSet::new();
    for (node, n, sub, poll, first, last, gap) in &per_node {
        seen.insert(*node);
        sum.reports += *n as u64;
        if gap.unwrap_or(0.0) > sum.longest_gap_s {
            sum.longest_gap_s = gap.unwrap_or(0.0);
            sum.longest_gap_node = Some(*node);
        }
        let bat = battery.get(node).map_or("-".into(), |(l, m)| format!("{} / {}", opt(*l, 1), opt(*m, 1)));
        let _ = writeln!(md, "| {node} | {} | {n} | {sub} | {poll} | {} | {} | {} | {bat} |", room_of_node.get(node).map_or("-", String::as_str), hhmm(*first), hhmm(*last), gap.map_or("-".into(), dur));
    }
    for (node, room) in room_of_node.iter().filter(|(n, _)| !seen.contains(n)) {
        let _ = writeln!(md, "| {node} | {room} | 0 | 0 | 0 | - | - | whole day | - |");
    }
    sum.nodes = seen.len().max(room_of_node.len());
    let _ = writeln!(md, "\nTotal {} reports, longest gap {}{}.\n", sum.reports, dur(sum.longest_gap_s), sum.longest_gap_node.map_or(String::new(), |n| format!(" (node {n})")));

    md.push_str("## Link quality trend (first half of the day -> second half)\n\n");
    let thread = rows(
        c,
        &format!(
            "WITH x AS (SELECT node, epoch_us(ts) AS us, list_avg(list_transform(regexp_extract_all(v_str, '{RSSI_PATTERN}', 1), lambda s: s::DOUBLE)) AS rssi, list_avg(list_transform(regexp_extract_all(v_str, '{LQI_PATTERN}', 1), lambda s: s::DOUBLE)) AS lqi
                        FROM attribute_reports WHERE cluster = 53 AND attribute = 7 AND v_str IS NOT NULL AND {day})
             SELECT node, count(*), avg(rssi) FILTER (WHERE us < {mid}), avg(rssi) FILTER (WHERE us >= {mid}), avg(lqi) FILTER (WHERE us < {mid}), avg(lqi) FILTER (WHERE us >= {mid}) FROM x GROUP BY node ORDER BY node"
        ),
        |r| Ok((r.get::<_, u64>(0)?, r.get::<_, i64>(1)?, r.get::<_, Option<f64>>(2)?, r.get::<_, Option<f64>>(3)?, r.get::<_, Option<f64>>(4)?, r.get::<_, Option<f64>>(5)?)),
    )?;
    if thread.is_empty() {
        md.push_str("Thread neighbour table: no reports.\n\n");
    } else {
        md.push_str("Thread neighbour table (Thread Network Diagnostics, mean over neighbours)\n\n| node | samples | RSSI dBm | LQI |\n|---|---|---|---|\n");
        for (node, n, a, b, la, lb) in thread {
            let _ = writeln!(md, "| {node} | {n} | {} | {} |", trend(a, b, 1), trend(la, lb, 1));
        }
        md.push('\n');
    }
    let watch = rows(c, &format!("SELECT count(*), avg(rssi) FILTER (WHERE epoch_us(ts) < {mid}), avg(rssi) FILTER (WHERE epoch_us(ts) >= {mid}) FROM watch_raw WHERE coalesce(sensor_type, 'rssi') = 'rssi' AND device IS DISTINCT FROM 'minix-anchor' AND {day}"), |r| {
        Ok((r.get::<_, i64>(0)?, r.get::<_, Option<f64>>(1)?, r.get::<_, Option<f64>>(2)?))
    })?;
    if let Some((n, a, b)) = watch.into_iter().next() {
        let _ = writeln!(md, "Watch scans: {n} access-point rows, RSSI {} dBm.\n", trend(a, b, 1));
    }

    md.push_str("## Labels per room\n\n");
    let labels = rows(c, &format!("SELECT room, source, count(*) FROM labels WHERE {day} GROUP BY room, source ORDER BY room, source"), |r| Ok((r.get::<_, String>(0)?, r.get::<_, String>(1)?, r.get::<_, i64>(2)?)))?;
    if labels.is_empty() {
        md.push_str("No labels.\n\n");
    } else {
        md.push_str("| room | source | labels |\n|---|---|---|\n");
        for (room, source, n) in labels {
            sum.labels += n as u64;
            let _ = writeln!(md, "| {room} | {source} | {n} |");
        }
        md.push('\n');
    }

    md.push_str("## Watch\n\n");
    let inference = rows(
        c,
        &format!("SELECT matcher, state, count(*), count(*) FILTER (WHERE abstained) FROM watch_inference WHERE reason IS DISTINCT FROM 'no_scan' AND {day} GROUP BY matcher, state ORDER BY matcher, state"),
        |r| Ok((r.get::<_, String>(0)?, r.get::<_, String>(1)?, r.get::<_, i64>(2)?, r.get::<_, i64>(3)?)),
    )?;
    if inference.is_empty() {
        md.push_str("Matcher: no inferences.\n\n");
    } else {
        md.push_str("| matcher | state | inferences | abstained | abstain rate |\n|---|---|---|---|---|\n");
        for (matcher, state, n, abstained) in &inference {
            sum.watch_inferences += *n as u64;
            sum.watch_abstained += *abstained as u64;
            let _ = writeln!(md, "| {matcher} | {state} | {n} | {abstained} | {:.1} % |", 100.0 * *abstained as f64 / (*n).max(1) as f64);
        }
        let reasons = rows(c, &format!("SELECT reason, count(*) FROM watch_inference WHERE abstained AND {day} GROUP BY reason ORDER BY reason"), |r| Ok((r.get::<_, Option<String>>(0)?, r.get::<_, i64>(1)?)))?;
        let _ = writeln!(md, "\nAbstain reasons: {}.\n", reasons.iter().map(|(k, n)| format!("{} {n}", k.as_deref().unwrap_or("-"))).collect::<Vec<_>>().join(", "));
    }
    let samples = rows(c, &format!("SELECT coalesce(sensor_type, 'rssi'), count(*) FROM watch_raw WHERE device IS DISTINCT FROM 'minix-anchor' AND {day} GROUP BY 1 ORDER BY 1"), |r| Ok((r.get::<_, String>(0)?, r.get::<_, i64>(1)?)))?;
    let _ = writeln!(md, "Sensor samples: {}.\n", if samples.is_empty() { "none".into() } else { samples.iter().map(|(k, n)| format!("{k} {n}")).collect::<Vec<_>>().join(", ") });
    let battery = rows(
        c,
        &format!(
            "WITH b AS (SELECT epoch_us(ts) AS us, value FROM watch_raw WHERE sensor_type = 'battery' AND value IS NOT NULL AND {day}),
                  g AS (SELECT epoch_us(ts) AS us, value >= 0.5 AS chg FROM watch_raw WHERE sensor_type = 'charging' AND value IS NOT NULL AND {day}),
                  j AS (SELECT b.us, b.value, coalesce(g.chg, false) AS chg FROM b ASOF LEFT JOIN g ON b.us >= g.us),
                  p AS (SELECT us - lag(us) OVER (ORDER BY us) AS dt, lag(value) OVER (ORDER BY us) - value AS drop, chg OR coalesce(lag(chg) OVER (ORDER BY us), false) AS charging FROM j)
             SELECT sum(greatest(drop, 0)) FILTER (WHERE NOT charging AND dt <= 1800000000), sum(dt) FILTER (WHERE NOT charging AND dt <= 1800000000) / 3.6e9, count(*) FROM p"
        ),
        |r| Ok((r.get::<_, Option<f64>>(0)?, r.get::<_, Option<f64>>(1)?)),
    )?;
    sum.watch_battery_per_h = battery.first().and_then(|(drop, hours)| drop.zip(*hours)).filter(|(_, h)| *h >= 0.5).map(|(d, h)| d / h);
    let _ = writeln!(md, "Watch battery drain while not charging: {} %/h.\n", sum.watch_battery_per_h.map_or("-".into(), |v| format!("{v:.2}")));

    md.push_str("## Weather\n\n");
    let hours: Vec<u32> = rows(c, &format!("SELECT DISTINCT ((epoch_us(ts) - {lo}) // 3600000000)::INTEGER FROM weather WHERE kind = 'obs' AND {day} ORDER BY 1"), |r| r.get::<_, i32>(0))?.into_iter().map(|h| h as u32).collect();
    sum.missing_weather_hours = (0..24).filter(|h| !hours.contains(h)).collect();
    if !sum.weather_configured && hours.is_empty() {
        md.push_str("Weather not configured (no coordinates in meta).\n\n");
    } else {
        let list = sum.missing_weather_hours.iter().map(|h| format!("{h:02}:00")).collect::<Vec<_>>().join(", ");
        let _ = writeln!(md, "{} of 24 observation hours present, missing {}: {}.\n", hours.len(), sum.missing_weather_hours.len(), if list.is_empty() { "none".into() } else { list });
    }

    md.push_str("## Experiments\n\n");
    let experiments = rows(
        c,
        &format!("SELECT epoch_us(ts), kind, node, experiment_id, detail FROM events WHERE kind IN ('experiment_start', 'experiment_end', 'experiment_abort') AND {day} ORDER BY ts"),
        |r| Ok((r.get::<_, i64>(0)?, r.get::<_, String>(1)?, r.get::<_, Option<u64>>(2)?, r.get::<_, Option<String>>(3)?, r.get::<_, Option<String>>(4)?)),
    )?;
    sum.experiments = experiments.iter().filter(|e| e.1 == "experiment_start").count() as u64;
    let count = |k: &str| experiments.iter().filter(|e| e.1 == k).count();
    let _ = writeln!(md, "{} started, {} ended, {} aborted.\n", count("experiment_start"), count("experiment_end"), count("experiment_abort"));
    for (us, kind, node, id, detail) in experiments.iter().take(50) {
        let _ = writeln!(md, "- {} {kind} node {} {} {}", hhmm(*us), node.map_or("-".into(), |n| n.to_string()), id.as_deref().unwrap_or("-"), detail.as_deref().unwrap_or(""));
    }

    md.push_str("\n## Heizfilter je Raum\n\n");
    let filter = rows(
        c,
        &format!(
            "WITH d AS (SELECT room, proposed, applied, json_extract_string(inputs.extra, '$.reason') AS why FROM decisions WHERE proposed AND {day}),
                  o AS (SELECT json_extract_string(detail, '$.room') AS room, json_extract(detail, '$.minutes')::DOUBLE AS minutes FROM events WHERE kind = 'occupancy_end' AND {day}),
                  r AS (SELECT room FROM d UNION SELECT room FROM o)
             SELECT r.room, (SELECT count(*) FROM d WHERE d.room = r.room AND applied), (SELECT count(*) FROM d WHERE d.room = r.room AND why = 'rate_limited'),
                    (SELECT count(*) FROM d WHERE d.room = r.room AND why = 'debounce'), (SELECT coalesce(sum(minutes), 0) FROM o WHERE o.room = r.room) FROM r ORDER BY r.room"
        ),
        |r| Ok((r.get::<_, String>(0)?, r.get::<_, i64>(1)?, r.get::<_, i64>(2)?, r.get::<_, i64>(3)?, r.get::<_, f64>(4)?)),
    )?;
    if filter.is_empty() {
        md.push_str("Keine Sollwert-Schreibvorgaenge, aufgeschobenen Vorschlaege oder belegten Intervalle.\n");
    } else {
        md.push_str("| Raum | Schreibvorgaenge | rate_limited | debounce | belegte Minuten |\n|---|---|---|---|---|\n");
        for (room, writes, limited, debounced, occupied) in filter {
            let _ = writeln!(md, "| {room} | {writes} | {limited} | {debounced} | {occupied:.0} |");
        }
    }

    md.push_str("\n## Anhebungs-Budget je Raum\n\n");
    let budget = rows(
        c,
        &format!(
            "WITH d AS (SELECT room, json_extract_string(inputs.extra, '$.budget') AS bud, json_extract_string(inputs.extra, '$.reason') AS why FROM decisions WHERE proposed AND {day})
             SELECT room, count(*) FILTER (WHERE bud = 'raise'), count(*) FILTER (WHERE why = 'raise_budget'), count(*) FILTER (WHERE bud LIKE 'exempt%') FROM d GROUP BY room HAVING count(*) FILTER (WHERE bud IS NOT NULL OR why = 'raise_budget') > 0 ORDER BY room"
        ),
        |r| Ok((r.get::<_, String>(0)?, r.get::<_, i64>(1)?, r.get::<_, i64>(2)?, r.get::<_, i64>(3)?)),
    )?;
    if budget.is_empty() {
        md.push_str("Keine Anhebung verbraucht Budget, keine aufgeschoben.\n");
    } else {
        md.push_str("| Raum | Anhebungen (Budget verbraucht) | aufgeschoben (raise_budget) | frei (exempt) |\n|---|---|---|---|\n");
        for (room, raises, deferred, exempt) in budget {
            let _ = writeln!(md, "| {room} | {raises} | {deferred} | {exempt} |");
        }
    }

    md.push_str("\n## Fenster-offen-Erkennung\n\n");
    let windows = rows(
        c,
        &format!(
            "WITH e AS (SELECT kind, json_extract_string(detail, '$.room') AS room, json_extract_string(detail, '$.reason') AS reason, json_extract(detail, '$.open_minutes')::DOUBLE AS minutes FROM events WHERE kind IN ('window_open', 'window_close', 'window_suppressed') AND {day})
             SELECT room, count(*) FILTER (WHERE kind = 'window_open'), count(*) FILTER (WHERE kind = 'window_suppressed'),
                    count(*) FILTER (WHERE kind = 'window_close'), coalesce(sum(minutes) FILTER (WHERE kind = 'window_close'), 0),
                    coalesce(string_agg(DISTINCT reason, ', ') FILTER (WHERE kind = 'window_close'), '-') FROM e GROUP BY room ORDER BY room"
        ),
        |r| Ok((r.get::<_, String>(0)?, r.get::<_, i64>(1)?, r.get::<_, i64>(2)?, r.get::<_, i64>(3)?, r.get::<_, f64>(4)?, r.get::<_, String>(5)?)),
    )?;
    if windows.is_empty() {
        md.push_str("Keine Fensterereignisse.\n");
    } else {
        md.push_str("| Raum | geoeffnet | unterdrueckt | geschlossen | offene Minuten | Gruende |\n|---|---|---|---|---|---|\n");
        for (room, opened, suppressed, closed, minutes, reasons) in windows {
            let _ = writeln!(md, "| {room} | {opened} | {suppressed} | {closed} | {minutes:.0} | {reasons} |");
        }
    }

    let episodes = rows(
        c,
        &format!(
            "SELECT coalesce(json_extract_string(detail, '$.source'), 'auto') AS s, count(*), coalesce(sum(json_extract(detail, '$.duration_min')::DOUBLE), 0), coalesce(avg(json_extract(detail, '$.house')::DOUBLE), 0),
                    count(*) FILTER (WHERE json_extract_string(detail, '$.label') = 'yes'), count(*) FILTER (WHERE json_extract_string(detail, '$.label') = 'no'), count(*) FILTER (WHERE json_extract_string(detail, '$.label') = 'none'),
                    avg(json_extract(detail, '$.latency_min')::DOUBLE) FROM events WHERE kind = 'window_episode' AND {day} GROUP BY s ORDER BY s"
        ),
        |r| Ok((r.get::<_, String>(0)?, r.get::<_, i64>(1)?, r.get::<_, f64>(2)?, r.get::<_, f64>(3)?, r.get::<_, i64>(4)?, r.get::<_, i64>(5)?, r.get::<_, i64>(6)?, r.get::<_, Option<f64>>(7)?)),
    )?;
    if !episodes.is_empty() {
        md.push_str("\nFensterepisoden (Detektor v3, Physik-Residuum; werden 25 min nach Ende gemeldet, Label = Antwort auf die Fensterfrage der Uhr):\n\n| Quelle | Episoden | offene Minuten | mittlerer Score | Label ja | nein | keins | Latenz vs. manuell (min) |\n|---|---|---|---|---|---|---|---|\n");
        for (source, count, minutes, house, yes, no, none, latency) in episodes {
            let latency = latency.map_or_else(|| "-".to_string(), |l| format!("{l:.1}"));
            let _ = writeln!(md, "| {source} | {count} | {minutes:.0} | {house:.1} | {yes} | {no} | {none} | {latency} |");
        }
    }

    match supply_section(root, date) {
        Ok(section) => md.push_str(&section),
        Err(e) => {
            let _ = writeln!(md, "
## Kesselversorgung

Nicht verfuegbar: {e:#}");
        }
    }

    match usage_section(root, date) {
        Ok(section) => md.push_str(&section),
        Err(e) => {
            let _ = writeln!(md, "\n## Heizbetrieb\n\nNicht verfuegbar: {e:#}");
        }
    }

    match crate::devices::report_section(root, date) {
        Ok(section) => md.push_str(&section),
        Err(e) => {
            let _ = writeln!(md, "\n## Zusatzgeraete\n\nNicht verfuegbar: {e:#}");
        }
    }

    match crate::manual::report_section(root, date) {
        Ok(section) => md.push_str(&section),
        Err(e) => {
            let _ = writeln!(md, "\n## Manuelle Vorgaben\n\nNicht verfuegbar: {e:#}");
        }
    }

    match crate::experiment::report_section(root, date) {
        Ok(section) => md.push_str(&section),
        Err(e) => {
            let _ = writeln!(md, "
## Messfahrten

Nicht verfuegbar: {e:#}");
        }
    }

    md.push_str(&crate::failsafe::report_section(&root.join("failsafe")));
    md.push_str("\n## Router-Watchdog\n\n");
    let br = rows(c, &format!("SELECT epoch_us(ts), detail FROM events WHERE kind = 'br_watchdog' AND {day} ORDER BY ts"), |r| Ok((r.get::<_, i64>(0)?, r.get::<_, Option<String>>(1)?)))?;
    if br.is_empty() {
        md.push_str("Keine Aenderungen, Router und Thermostate durchgehend erreichbar.\n");
    }
    for (us, detail) in br.iter().take(50) {
        let _ = writeln!(md, "- {} {}", hhmm(*us), detail.as_deref().unwrap_or(""));
    }

    md.push_str("\n## Thermostat-Gesundheit\n\n");
    let health = rows(c, &format!("SELECT epoch_us(ts), node, detail FROM events WHERE kind = 'node_health' AND {day} ORDER BY ts"), |r| Ok((r.get::<_, i64>(0)?, r.get::<_, Option<String>>(1)?, r.get::<_, Option<String>>(2)?)))?;
    md.push_str(&crate::node_health::report_section(&health));

    md.push_str("\n## Events by kind\n\n");
    let kinds = rows(c, &format!("SELECT kind, count(*) FROM events WHERE {day} GROUP BY kind ORDER BY kind"), |r| Ok((r.get::<_, String>(0)?, r.get::<_, i64>(1)?)))?;
    let _ = writeln!(md, "{}\n", if kinds.is_empty() { "none".into() } else { kinds.iter().map(|(k, n)| format!("{k} {n}")).collect::<Vec<_>>().join(", ") });

    md.push_str("## Roll status\n\n| table | raw files | rolled rows | partitions | status |\n|---|---|---|---|---|\n");
    sum.roll_ok = true;
    for (table, marker, has_raw) in roll_markers(root, date) {
        let status = match (&marker, has_raw) {
            (Some(_), _) => "rolled",
            (None, true) => "NOT ROLLED",
            (None, false) => "no data",
        };
        sum.roll_ok &= status != "NOT ROLLED";
        let raw = lake::raw_files(root, table, Some(date)).map_or(0, |f| f.len());
        let _ = writeln!(md, "| {} | {raw} | {} | {} | {status} |", table.name(), marker.as_ref().map_or("-".into(), |m| m["rows"].to_string()), marker.as_ref().map_or("-".into(), |m| m["partitions"].to_string()));
    }
    md.push('\n');
    match crate::dynamics::report_section(root, date) {
        Ok(section) => md.push_str(&section),
        Err(e) => {
            let _ = writeln!(md, "## Soll/Ist und Nachregeln\n\nNicht verfuegbar: {e:#}");
        }
    }
    Ok((md, sum))
}

pub fn log_line(date: NaiveDate, s: &DaySummary) -> String {
    format!(
        "daily report {date}: {} reports from {} nodes, longest gap {}, {} labels, {} weather hours missing, {} experiments, roll {}",
        s.reports,
        s.nodes,
        dur(s.longest_gap_s),
        s.labels,
        s.missing_weather_hours.len(),
        s.experiments,
        if s.roll_ok { "ok" } else { "INCOMPLETE" }
    )
}

pub fn generate(root: &Path, date: NaiveDate) -> Result<(PathBuf, DaySummary)> {
    let (md, summary) = build(root, date)?;
    let path = root.join("reports").join(format!("{date}.md"));
    std::fs::create_dir_all(root.join("reports"))?;
    let tmp = path.with_extension("md.tmp");
    std::fs::write(&tmp, md)?;
    std::fs::rename(&tmp, &path)?;
    println!("{}", log_line(date, &summary));
    Ok((path, summary))
}

pub fn report_command(root: &Path, date: Option<&str>) -> Result<PathBuf> {
    let today = chrono::Utc::now().date_naive();
    let date = match date {
        Some(d) => NaiveDate::parse_from_str(d, "%Y-%m-%d").map_err(|_| anyhow!("bad date {d}, expected YYYY-MM-DD"))?,
        None => today - chrono::Duration::days(1),
    };
    if date < today {
        roll_missing(root, date, date, today);
    }
    Ok(generate(root, date)?.0)
}

pub fn secs_until_report(now: chrono::NaiveDateTime) -> u64 {
    let at = NaiveTime::from_hms_opt(REPORT_HOUR, REPORT_MINUTE, 0).unwrap();
    let mut next = now.date().and_time(at);
    if next <= now {
        next += chrono::Duration::days(1);
    }
    (next - now).num_seconds().max(1) as u64
}

fn daily_job(sink: &Sink, status: &Status, only_missing: bool) {
    let date = sink.today() - chrono::Duration::days(1);
    let root = sink.root();
    if only_missing && root.join("reports").join(format!("{date}.md")).exists() {
        return;
    }
    match generate(root, date) {
        Ok((_, s)) => status.rolled(lake::now_us(), s.roll_ok),
        Err(e) => eprintln!("daily report {date} failed: {e:#}"),
    }
    if let Err(e) = crate::watchmatch::drift_alarm(sink) {
        eprintln!("matcher drift check failed: {e:#}");
    }
}

pub async fn daily_loop(sink: Sink, status: Status) {
    tokio::time::sleep(CATCH_UP_DELAY).await;
    loop {
        let (s, st) = (sink.clone(), status.clone());
        let _ = tokio::task::spawn_blocking(move || daily_job(&s, &st, true)).await;
        tokio::time::sleep(Duration::from_secs(secs_until_report(chrono::Local::now().naive_local()))).await;
        let (s, st) = (sink.clone(), status.clone());
        let _ = tokio::task::spawn_blocking(move || daily_job(&s, &st, false)).await;
    }
}

const USAGE_LOW_PCT: f64 = 10.0;
const USAGE_HIGH_PCT: f64 = 50.0;
const HOUSE: &str = "Haus gesamt";

#[derive(Debug, Default, Clone, PartialEq)]
pub struct RoomUsage {
    pub valve_hours: f64,
    pub minutes_over_10: f64,
    pub minutes_over_50: f64,
    pub first_us: Option<i64>,
    pub last_us: Option<i64>,
    pub hourly: [f64; 24],
    pub heat_proxy: f64,
}

impl RoomUsage {
    fn add(&mut self, other: &RoomUsage) {
        self.valve_hours += other.valve_hours;
        self.minutes_over_10 += other.minutes_over_10;
        self.minutes_over_50 += other.minutes_over_50;
        self.heat_proxy += other.heat_proxy;
        self.first_us = self.first_us.into_iter().chain(other.first_us).min();
        self.last_us = self.last_us.into_iter().chain(other.last_us).max();
        self.hourly.iter_mut().zip(other.hourly).for_each(|(a, b)| *a += b);
    }

    fn json(&self, house: &RoomUsage) -> Value {
        let share = |v: f64, total: f64| (total > 0.0).then(|| v / total);
        let iso = |us: Option<i64>| us.and_then(chrono::DateTime::from_timestamp_micros).map(|t| t.format("%Y-%m-%dT%H:%M:%SZ").to_string());
        serde_json::json!({
            "valve_hours": self.valve_hours, "minutes_over_10": self.minutes_over_10, "minutes_over_50": self.minutes_over_50,
            "first": iso(self.first_us), "last": iso(self.last_us), "hourly_valve_hours": self.hourly,
            "share_of_house": share(self.valve_hours, house.valve_hours), "heat_proxy_kh": self.heat_proxy, "heat_proxy_share": share(self.heat_proxy, house.heat_proxy),
        })
    }
}

pub type Usage = BTreeMap<NaiveDate, BTreeMap<String, RoomUsage>>;

pub fn usage(root: &Path, from: NaiveDate, to: NaiveDate) -> Result<Usage> {
    let db = open_room_minute(root, from, to)?;
    let valid = "gap_mask & 4 = 0 AND valve_pct BETWEEN 0 AND 100";
    let heat = "gap_mask & 13 = 0 AND valve_pct BETWEEN 0 AND 100";
    let data = rows(
        &db.conn,
        &format!(
            "SELECT room, strftime(t, '%Y-%m-%d'), hour(t), coalesce(sum(valve_pct) FILTER (WHERE {valid}), 0) / 6000.0, count(*) FILTER (WHERE {valid} AND valve_pct > {USAGE_LOW_PCT}), count(*) FILTER (WHERE {valid} AND valve_pct > {USAGE_HIGH_PCT}),
                    coalesce(sum(valve_pct / 100.0 * greatest(actual - outdoor_temp, 0)) FILTER (WHERE {heat}), 0) / 60.0, epoch_us(min(t) FILTER (WHERE {valid} AND valve_pct > {USAGE_LOW_PCT})), epoch_us(max(t) FILTER (WHERE {valid} AND valve_pct > {USAGE_LOW_PCT}))
             FROM room_minute WHERE epoch_us(t) >= {} AND epoch_us(t) < {} GROUP BY room, 2, 3 ORDER BY 2, 1, 3",
            midnight_us(from),
            midnight_us(to + chrono::Duration::days(1))
        ),
        |r| Ok((r.get::<_, String>(0)?, r.get::<_, String>(1)?, r.get::<_, i32>(2)?, r.get::<_, f64>(3)?, r.get::<_, i64>(4)?, r.get::<_, i64>(5)?, r.get::<_, f64>(6)?, r.get::<_, Option<i64>>(7)?, r.get::<_, Option<i64>>(8)?)),
    )?;
    let mut out = Usage::new();
    for (room, day, hour, hours, over_10, over_50, proxy, first, last) in data {
        let entry = out.entry(day.parse()?).or_default().entry(room).or_default();
        entry.hourly[hour as usize % 24] += hours;
        entry.add(&RoomUsage { valve_hours: hours, minutes_over_10: over_10 as f64, minutes_over_50: over_50 as f64, first_us: first, last_us: last, heat_proxy: proxy, ..Default::default() });
    }
    Ok(out)
}

fn house_total<'a>(rooms: impl Iterator<Item = &'a RoomUsage>) -> RoomUsage {
    let mut total = RoomUsage::default();
    rooms.for_each(|r| total.add(r));
    total
}

fn rooms_json<'a>(rooms: impl Iterator<Item = (&'a String, &'a RoomUsage)>, house: &RoomUsage) -> Value {
    Value::Object(rooms.map(|(k, v)| (k.clone(), v.json(house))).collect())
}

pub fn usage_json(root: &Path, from: NaiveDate, to: NaiveDate) -> Result<Value> {
    roll_missing(root, from - chrono::Duration::days(2), to, chrono::Utc::now().date_naive());
    let data = usage(root, from, to)?;
    let mut totals: BTreeMap<String, RoomUsage> = BTreeMap::new();
    let mut days = Vec::new();
    for (date, rooms) in &data {
        let house = house_total(rooms.values());
        days.push(serde_json::json!({"date": date.to_string(), "rooms": rooms_json(rooms.iter(), &house), "house": house.json(&house)}));
        rooms.iter().for_each(|(k, v)| totals.entry(k.clone()).or_default().add(v));
    }
    let house = house_total(totals.values());
    Ok(serde_json::json!({
        "from": from.to_string(), "to": to.to_string(), "days": days,
        "totals": {"rooms": rooms_json(totals.iter(), &house), "house": house.json(&house)},
        "note": "heat_proxy_kh = sum of valve fraction x max(sensor - outdoor, 0) x hours, a relative estimate and not measured energy; the current UTC day is included only after the 03:00 roll",
    }))
}

pub fn supply_section(root: &Path, date: NaiveDate) -> Result<String> {
    let db = open_lake(root, date - chrono::Duration::days(14), date + chrono::Duration::days(1))?;
    let events = rows(
        &db.conn,
        "SELECT CAST(epoch_us(ts) / 1000 AS BIGINT), json_extract_string(detail, '$.room'), json_extract_string(detail, '$.state'), CAST(json_extract(detail, '$.since_ms') AS BIGINT) FROM events WHERE kind = 'supply' ORDER BY ts",
        |r| Ok((r.get::<_, i64>(0)?, r.get::<_, Option<String>>(1)?, r.get::<_, Option<String>>(2)?, r.get::<_, Option<i64>>(3)?)),
    )?;
    let mut md = String::from("
## Kesselversorgung

");
    let (lo, hi) = (midnight_us(date) / 1000, midnight_us(date + chrono::Duration::days(1)) / 1000);
    let today: Vec<_> = events.iter().filter(|e| (lo..hi).contains(&e.0)).collect();
    if today.is_empty() {
        md.push_str("Keine Phasen ohne Waermelieferung (Ventil >= 90 % ohne Temperaturanstieg).
");
    }
    for (ms, room, state, since) in today {
        let _ = writeln!(md, "- {} {} {}{}", hhmm(ms * 1000), room.as_deref().unwrap_or("-"), state.as_deref().unwrap_or("-"), since.map_or(String::new(), |s| format!(" (seit {})", hhmm(s * 1000))));
    }
    let episodes: Vec<(i64, i64)> = events.iter().filter(|e| e.2.as_deref() == Some("end")).filter_map(|e| Some((e.3?, e.0))).collect();
    let learned = crate::window::supply_window(&episodes);
    match learned["label"].as_str() {
        Some(label) => {
            let _ = writeln!(md, "
{label} (Median ueber {} Phasen der letzten 14 Tage, Ortszeit).", learned["episodes"]);
        }
        None => md.push_str("
Kein wiederkehrendes Muster gelernt.
"),
    }
    Ok(md)
}

pub fn usage_section(root: &Path, date: NaiveDate) -> Result<String> {
    let data = usage(root, date, date)?;
    let rooms = data.get(&date).cloned().unwrap_or_default();
    let house = house_total(rooms.values());
    let mut md = String::from("\n## Heizbetrieb\n\n");
    if rooms.is_empty() {
        md.push_str("Keine Ventildaten.\n");
        return Ok(md);
    }
    md.push_str("Ventilstunden = Summe PIHeatingDemand-Anteil x Zeit (Werte ueber 100 % ignoriert). Waermeproxy = Ventilanteil x (Sensor - Aussen) x Zeit in K*h, nur eine relative Schaetzung, keine Messung. Zeiten UTC.\n\n| Raum | Ventilstunden | Min > 10 % | Min > 50 % | erstes Heizen | letztes Heizen | Anteil Haus | Waermeproxy (Schaetzung) | Anteil Proxy |\n|---|---|---|---|---|---|---|---|---|\n");
    let pct = |v: f64, total: f64| if total > 0.0 { format!("{:.0} %", 100.0 * v / total) } else { "-".into() };
    let when = |us: Option<i64>| us.map_or("-".into(), |u| hhmm(u)[..5].to_string());
    let all: Vec<(&str, &RoomUsage)> = rooms.iter().map(|(k, v)| (k.as_str(), v)).chain(std::iter::once((HOUSE, &house))).collect();
    for (room, u) in &all {
        let _ = writeln!(md, "| {room} | {:.2} | {:.0} | {:.0} | {} | {} | {} | {:.2} | {} |", u.valve_hours, u.minutes_over_10, u.minutes_over_50, when(u.first_us), when(u.last_us), pct(u.valve_hours, house.valve_hours), u.heat_proxy, pct(u.heat_proxy, house.heat_proxy));
    }
    md.push_str("\nStundenprofil (Ventilstunden je UTC-Stunde 00..23)\n\n");
    for (room, u) in &all {
        let _ = writeln!(md, "- {room}: {}", u.hourly.iter().map(|h| format!("{h:.2}")).collect::<Vec<_>>().join(" "));
    }
    Ok(md)
}
