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

fn quote(text: &str) -> String {
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

fn midnight_us(date: NaiveDate) -> i64 {
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

fn hhmm(us: i64) -> String {
    chrono::DateTime::from_timestamp_micros(us).map_or("?".into(), |t| t.format("%H:%M:%S").to_string())
}

fn dur(seconds: f64) -> String {
    match seconds {
        s if s >= 3600.0 => format!("{:.1} h", s / 3600.0),
        s if s >= 120.0 => format!("{:.0} min", s / 60.0),
        s => format!("{s:.0} s"),
    }
}

fn opt(v: Option<f64>, digits: usize) -> String {
    v.map_or("-".into(), |v| format!("{v:.digits$}"))
}

fn trend(first: Option<f64>, second: Option<f64>, digits: usize) -> String {
    match (first, second) {
        (Some(a), Some(b)) => format!("{a:.digits$} -> {b:.digits$} ({:+.digits$})", b - a),
        _ => "-".into(),
    }
}

type Rows<T> = Result<Vec<T>>;

fn rows<T>(conn: &Connection, sql: &str, map: impl Fn(&duckdb::Row) -> duckdb::Result<T>) -> Rows<T> {
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
    let watch = rows(c, &format!("SELECT count(*), avg(rssi) FILTER (WHERE epoch_us(ts) < {mid}), avg(rssi) FILTER (WHERE epoch_us(ts) >= {mid}) FROM watch_raw WHERE coalesce(sensor_type, 'rssi') = 'rssi' AND {day}"), |r| {
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
    let samples = rows(c, &format!("SELECT coalesce(sensor_type, 'rssi'), count(*) FROM watch_raw WHERE {day} GROUP BY 1 ORDER BY 1"), |r| Ok((r.get::<_, String>(0)?, r.get::<_, i64>(1)?)))?;
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
