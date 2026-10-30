use crate::lake::*;
use arrow::ipc::reader::StreamReader;
use chrono::NaiveDate;
use parquet::arrow::arrow_reader::ParquetRecordBatchReaderBuilder;
use parquet::file::reader::{FileReader, SerializedFileReader};
use std::fs::File;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicI64, Ordering};
use std::sync::Arc;

const DOC: &str = include_str!("../../../docs/data-foundation.md");
const DAY_US: i64 = 86_400_000_000;

fn d(s: &str) -> NaiveDate {
    s.parse().unwrap()
}

fn day_us(s: &str) -> i64 {
    d(s).and_hms_opt(12, 0, 0).unwrap().and_utc().timestamp_micros()
}

fn documented(table: Table) -> String {
    let start = format!("```schema {}\n", table.name());
    let from = DOC.find(&start).unwrap_or_else(|| panic!("{} not documented", table.name())) + start.len();
    DOC[from..].split("\n```").next().unwrap().to_string()
}

fn check_schema(table: Table, batch: arrow::record_batch::RecordBatch) {
    let schema = table.schema();
    assert_eq!(describe(&schema), documented(table));
    assert_eq!(schema.metadata()["schema_version"], SCHEMA_VERSION.to_string());
    assert_eq!(schema.metadata()["heatd_commit"], COMMIT);
    assert_eq!(schema.metadata()["table"], table.name());
    assert_eq!(batch.schema(), schema);
    assert_eq!(batch.num_rows(), 1);
}

#[test]
fn schema_attribute_reports() {
    let row = AttributeReport { ts: 1, dtype: "int".into(), v_i64: Some(3), source: "poll".into(), fw: "1".into(), ..Default::default() };
    check_schema(Table::AttributeReports, AttributeReport::to_batch(&[row]).unwrap());
}

#[test]
fn schema_events() {
    check_schema(Table::Events, Event::to_batch(&[Event::now(kind::STARTUP, "heatd")]).unwrap());
    assert!(kind::ALL.iter().all(|k| DOC.contains(&format!("| {k} |"))));
}

#[test]
fn schema_decisions() {
    let row = Decision { ts: 1, room: "bad".into(), rule: "schedule".into(), commit: "x".into(), ..Default::default() };
    check_schema(Table::Decisions, Decision::to_batch(&[row]).unwrap());
}

#[test]
fn schema_labels() {
    let row = Label { ts: 1, from_ts: Some(0), to_ts: Some(2), room: "bad".into(), source: "watch".into(), ..Default::default() };
    check_schema(Table::Labels, Label::to_batch(&[row]).unwrap());
}

#[test]
fn schema_watch_raw() {
    let row = WatchRaw { ts: 1, sender: "aa".into(), rssi: -40, device: "w".into(), ..Default::default() };
    check_schema(Table::WatchRaw, WatchRaw::to_batch(&[row]).unwrap());
}

#[test]
fn schema_weather() {
    let row = Weather { ts: 1, kind: "obs".into(), fetched_ts: 2, temp_c: Some(1.5), ..Default::default() };
    check_schema(Table::Weather, Weather::to_batch(&[row]).unwrap());
}

#[test]
fn schema_meta() {
    let row = Meta { ts: 1, key: "rooms".into(), value: "[]".into(), valid_from: 1 };
    check_schema(Table::Meta, Meta::to_batch(&[row]).unwrap());
}

#[test]
fn schema_watch_sensors_and_inference() {
    let sensor = WatchSensor { ts: 1, sensor_name: "skin".into(), android_type: 65555, ..Default::default() };
    check_schema(Table::WatchSensors, WatchSensor::to_batch(&[sensor]).unwrap());
    let inference = WatchInference { ts: 1, matcher: "room".into(), template_id: "all".into(), state: "rest".into(), features_hash: "00".into(), model_version: "v1".into(), ..Default::default() };
    check_schema(Table::WatchInference, WatchInference::to_batch(&[inference]).unwrap());
}

#[test]
fn watch_tables_roll_by_date_only() {
    let env = Env::new("2026-10-29");
    let sink = env.sink(10, 3600);
    sink.append_on(d("2026-10-20"), WatchSensor { ts: 1, sensor_name: "hr".into(), android_type: 21, ..Default::default() }).unwrap();
    sink.append_on(d("2026-10-20"), WatchInference { ts: 1, matcher: "room".into(), template_id: "all".into(), state: "rest".into(), features_hash: "00".into(), model_version: "v1".into(), ..Default::default() }).unwrap();
    sink.roll_pending().unwrap();
    assert_eq!(parquet_rows(&env.parquet(Table::WatchSensors, "2026-10-20").join("part.parquet")), 1);
    assert_eq!(parquet_rows(&env.parquet(Table::WatchInference, "2026-10-20").join("part.parquet")), 1);
}

struct Env {
    dir: tempfile::TempDir,
    clock: Arc<AtomicI64>,
}

impl Env {
    fn new(today: &str) -> Self {
        Self { dir: tempfile::tempdir().unwrap(), clock: Arc::new(AtomicI64::new(day_us(today))) }
    }

    fn sink(&self, flush_rows: usize, flush_secs: u64) -> Sink {
        let clock = self.clock.clone();
        let mut cfg = LakeConfig::new(self.dir.path());
        cfg.flush_rows = flush_rows;
        cfg.flush_secs = flush_secs;
        Sink::open_with_clock(cfg, Arc::new(move || clock.load(Ordering::SeqCst))).unwrap()
    }

    fn raw(&self, table: Table) -> Vec<PathBuf> {
        let mut v: Vec<_> = std::fs::read_dir(self.dir.path().join("raw").join(table.name())).unwrap().flatten().map(|e| e.path()).collect();
        v.sort();
        v
    }

    fn parquet(&self, table: Table, date: &str) -> PathBuf {
        self.dir.path().join("parquet").join(table.name()).join(format!("date={date}"))
    }
}

fn label(i: i64) -> Label {
    Label { ts: i, room: format!("r{}", i % 3), source: "watch".into(), ..Default::default() }
}

fn report(node: u64, i: i64) -> AttributeReport {
    AttributeReport { ts: i, mono_ns: i, node, endpoint: 1, cluster: 0x201, attribute: 0, dtype: "int".into(), v_i64: Some(i), value_raw: vec![1, 2], source: "subscription".into(), fw: "1.0".into(), ..Default::default() }
}

fn stream_rows(path: &Path) -> usize {
    StreamReader::try_new(File::open(path).unwrap(), None).unwrap().map(|b| b.unwrap().num_rows()).sum()
}

fn parquet_rows(path: &Path) -> i64 {
    SerializedFileReader::new(File::open(path).unwrap()).unwrap().metadata().file_metadata().num_rows()
}

#[test]
fn flushes_at_row_threshold() {
    let env = Env::new("2026-10-29");
    let sink = env.sink(5, 3600);
    (0..4).for_each(|i| sink.append(label(i)).unwrap());
    assert!(env.raw(Table::Labels).is_empty());
    sink.append(label(4)).unwrap();
    let files = env.raw(Table::Labels);
    assert_eq!(files.len(), 1);
    assert!(files[0].file_name().unwrap().to_str().unwrap() == "2026-10-29.arrows");
    assert_eq!(load_stream(&files[0]).unwrap().batches.iter().map(|b| b.num_rows()).sum::<usize>(), 5);
    assert_eq!(sink.tick().unwrap(), 0);
}

#[test]
fn flushes_after_age_and_is_durable_before_close() {
    let env = Env::new("2026-10-29");
    let sink = env.sink(500, 0);
    sink.append(label(1)).unwrap();
    sink.append(Event::now(kind::STARTUP, "heatd")).unwrap();
    assert_eq!(sink.tick().unwrap(), 2);
    let file = &env.raw(Table::Labels)[0];
    assert_eq!(std::fs::metadata(file).unwrap().len(), load_stream(file).unwrap().keep_len.unwrap() as u64);
    assert_eq!(stream_rows(file), 1);
    assert!(!load_stream(file).unwrap().complete);
    sink.close().unwrap();
    assert!(load_stream(file).unwrap().complete);
    assert_eq!(stream_rows(file), 1);
}

#[test]
fn buffers_by_ingest_date() {
    let env = Env::new("2026-10-29");
    let sink = env.sink(500, 3600);
    sink.append(label(1)).unwrap();
    env.clock.fetch_add(DAY_US, Ordering::SeqCst);
    sink.append(label(2)).unwrap();
    sink.flush().unwrap();
    let names: Vec<_> = env.raw(Table::Labels).iter().map(|p| p.file_name().unwrap().to_str().unwrap().to_string()).collect();
    assert_eq!(names, ["2026-10-29.arrows", "2026-10-30.arrows"]);
}

#[test]
fn recovers_truncated_and_garbage_files() {
    let env = Env::new("2026-10-29");
    let sink = env.sink(2, 3600);
    (0..6).for_each(|i| sink.append(label(i)).unwrap());
    drop(sink);
    let file = env.raw(Table::Labels)[0].clone();
    assert_eq!(stream_rows(&file), 6);
    let len = std::fs::metadata(&file).unwrap().len();
    let f = std::fs::OpenOptions::new().write(true).open(&file).unwrap();
    f.set_len(len - 7).unwrap();
    drop(f);
    let sink = env.sink(2, 3600);
    assert_eq!(stream_rows(&file), 4);
    assert!(load_stream(&file).unwrap().complete);
    sink.append(label(10)).unwrap();
    sink.append(label(11)).unwrap();
    let files = env.raw(Table::Labels);
    assert_eq!(files.len(), 2);
    let second = env.dir.path().join("raw/labels/2026-10-29.1.arrows");
    assert!(files.contains(&second));
    assert_eq!(files.iter().map(|p| stream_rows(p)).sum::<usize>(), 6);
    drop(sink);
    let mut garbage = std::fs::OpenOptions::new().append(true).open(&second).unwrap();
    std::io::Write::write_all(&mut garbage, &[7u8; 33]).unwrap();
    drop(garbage);
    let _ = env.sink(2, 3600);
    assert_eq!(stream_rows(&second), 2);
    std::fs::write(env.dir.path().join("raw/labels/2026-10-28.arrows"), [1u8, 2, 3]).unwrap();
    let _ = env.sink(2, 3600);
    assert!(env.dir.path().join("raw/labels/2026-10-28.arrows.corrupt").exists());
}

#[test]
fn rolls_with_verification_marker_and_idempotence() {
    let env = Env::new("2026-10-29");
    let sink = env.sink(4, 3600);
    for i in 0..10 {
        sink.append(report(5 + (i % 2) as u64, i)).unwrap();
        sink.append(label(i)).unwrap();
    }
    sink.append(Event::now(kind::STARTUP, "heatd")).unwrap();
    sink.close().unwrap();
    let sink = env.sink(4, 3600);
    sink.append(report(5, 99)).unwrap();
    sink.close().unwrap();
    let raw_before: Vec<_> = env.raw(Table::AttributeReports).iter().map(|p| std::fs::read(p).unwrap()).collect();
    assert_eq!(raw_before.len(), 2);
    let rolls = sink.roll_day(d("2026-10-29")).unwrap();
    let get = |t: Table| rolls.iter().find(|r| r.table == t).unwrap().clone();
    assert_eq!((get(Table::AttributeReports).rows, get(Table::AttributeReports).partitions, get(Table::AttributeReports).status), (11, 2, RollStatus::Rolled));
    assert_eq!(get(Table::Labels).partitions, 1);
    assert_eq!(get(Table::Decisions).status, RollStatus::Empty);
    let dir = env.parquet(Table::AttributeReports, "2026-10-29");
    assert_eq!(parquet_rows(&dir.join("node=5/part.parquet")), 6);
    assert_eq!(parquet_rows(&dir.join("node=6/part.parquet")), 5);
    assert!(dir.join(".rolled").exists());
    assert_eq!(parquet_rows(&env.parquet(Table::Labels, "2026-10-29").join("part.parquet")), 10);
    assert_eq!(parquet_rows(&env.parquet(Table::Events, "2026-10-29").join("node=none/part.parquet")), 1);
    let reader = ParquetRecordBatchReaderBuilder::try_new(File::open(dir.join("node=5/part.parquet")).unwrap()).unwrap();
    assert_eq!(reader.schema().metadata()["schema_version"], SCHEMA_VERSION.to_string());
    let batches: Vec<_> = reader.build().unwrap().map(|b| b.unwrap()).collect();
    assert_eq!(batches.iter().map(|b| b.num_rows()).sum::<usize>(), 6);
    let raw_after: Vec<_> = env.raw(Table::AttributeReports).iter().map(|p| std::fs::read(p).unwrap()).collect();
    assert_eq!(raw_before, raw_after);
    let bytes = std::fs::read(dir.join("node=5/part.parquet")).unwrap();
    let again = sink.roll_day(d("2026-10-29")).unwrap();
    assert!(again.iter().filter(|r| r.status != RollStatus::Empty).all(|r| r.status == RollStatus::Unchanged));
    assert_eq!(bytes, std::fs::read(dir.join("node=5/part.parquet")).unwrap());
    let late = env.sink(4, 3600);
    late.append(report(7, 100)).unwrap();
    late.close().unwrap();
    let third = late.roll_day(d("2026-10-29")).unwrap();
    assert_eq!(third.iter().find(|r| r.table == Table::AttributeReports).unwrap().rows, 12);
    assert_eq!(parquet_rows(&dir.join("node=7/part.parquet")), 1);
}

#[test]
fn rolls_large_day_into_64k_row_groups() {
    let env = Env::new("2026-10-29");
    let sink = env.sink(100_000, 3600);
    (0..70_000).for_each(|i| sink.append(label(i)).unwrap());
    sink.close().unwrap();
    let r = sink.roll_day(d("2026-10-29")).unwrap();
    assert_eq!(r.iter().find(|r| r.table == Table::Labels).unwrap().rows, 70_000);
    let meta = SerializedFileReader::new(File::open(env.parquet(Table::Labels, "2026-10-29").join("part.parquet")).unwrap()).unwrap().metadata().clone();
    assert_eq!(meta.num_row_groups(), 2);
    assert_eq!(meta.row_group(0).num_rows(), 65_536);
    let col = meta.row_group(0).column(3);
    assert_eq!(col.column_path().string(), "room");
    assert!(col.compression().to_string().starts_with("ZSTD"));
    assert!(col.dictionary_page_offset().is_some() || col.encodings().any(|e| e.to_string().contains("DICT")));
}

#[test]
fn conforms_older_segments_additively() {
    let env = Env::new("2026-10-29");
    let old = {
        let full = Table::Labels.schema();
        let fields: Vec<_> = full.fields().iter().take(full.fields().len() - 1).cloned().collect();
        Arc::new(arrow::datatypes::Schema::new(fields))
    };
    let current = Label::to_batch(&[label(1), label(2)]).unwrap();
    let cols: Vec<_> = current.columns().iter().take(old.fields().len()).cloned().collect();
    let batch = arrow::record_batch::RecordBatch::try_new(old.clone(), cols).unwrap();
    std::fs::create_dir_all(env.dir.path().join("raw/labels")).unwrap();
    let mut w = arrow::ipc::writer::StreamWriter::try_new(File::create(env.dir.path().join("raw/labels/2026-10-20.arrows")).unwrap(), &old).unwrap();
    w.write(&batch).unwrap();
    w.finish().unwrap();
    let sink = env.sink(10, 3600);
    let r = sink.roll_pending().unwrap();
    let labels = r.iter().find(|r| r.table == Table::Labels && r.status == RollStatus::Rolled).unwrap();
    assert_eq!(labels.rows, 2);
    let rolled = ParquetRecordBatchReaderBuilder::try_new(File::open(env.parquet(Table::Labels, "2026-10-20").join("part.parquet")).unwrap()).unwrap().build().unwrap().next().unwrap().unwrap();
    assert_eq!(rolled.schema().fields().len(), Table::Labels.schema().fields().len());
    assert_eq!(rolled.column(8).null_count(), 2);
}

#[test]
fn roll_pending_catches_up_past_days_only_and_logs_events() {
    let env = Env::new("2026-10-27");
    let sink = env.sink(500, 3600);
    sink.append(label(1)).unwrap();
    env.clock.store(day_us("2026-10-29"), Ordering::SeqCst);
    sink.append(label(2)).unwrap();
    let rolls = sink.roll_pending().unwrap();
    assert_eq!(rolls.iter().filter(|r| r.status == RollStatus::Rolled).count(), 1);
    assert!(env.parquet(Table::Labels, "2026-10-27").join(".rolled").exists());
    assert!(!env.parquet(Table::Labels, "2026-10-29").exists());
    sink.flush().unwrap();
    let events = env.raw(Table::Events);
    let batch = &load_stream(&events[0]).unwrap().batches[0];
    assert_eq!(batch.num_rows(), 1);
    assert_eq!(sink.roll_pending().unwrap().iter().filter(|r| r.status == RollStatus::Rolled).count(), 0);
}

#[test]
fn schedules_roll_at_three_local() {
    let at = |s: &str| chrono::NaiveDateTime::parse_from_str(s, "%Y-%m-%d %H:%M:%S").unwrap();
    assert_eq!(secs_until_roll(at("2026-10-29 02:00:00")), 3600);
    assert_eq!(secs_until_roll(at("2026-10-29 03:00:00")), 86_400);
    assert_eq!(secs_until_roll(at("2026-10-29 23:00:00")), 4 * 3600);
}

#[test]
fn migrates_legacy_duckdb() {
    let env = Env::new("2026-10-29");
    let db = env.dir.path().join("heatd.duckdb");
    {
        let storage = crate::storage::Storage::open(db.to_str().unwrap()).unwrap();
        let scan = crate::storage::ScanBody {
            ts: 1_761_580_800_000,
            device: "w".into(),
            id: None,
            label: Some("bad".into()),
            aps: vec![crate::storage::AccessPoint { bssid: "AA:BB:CC:00:00:01".into(), rssi: -45, freq: 2412 }, crate::storage::AccessPoint { bssid: "aa:bb:cc:00:00:02".into(), rssi: -67, freq: 5180 }],
            motion: Some(crate::storage::Motion::Still),
            queued: false,
            power: None,
            away: false,
            bedtime: None,
            heat: None,
            label_id: None,
        };
        storage.ingest(&scan, 1_761_580_801_000, Some(("bad", 0.8))).unwrap();
        storage.insert_reading(1_761_580_802_000, 5, 1, "local_temperature", 21.5).unwrap();
        storage.insert_setpoint(1_761_580_803_000, 5, "bad", 20.0, "schedule").unwrap();
        storage.insert_reading(1_761_580_900_000, 5, 1, "local_temperature", 22.5).unwrap();
        storage.insert_setpoint(1_761_580_901_000, 5, "bad", 21.0, "schedule").unwrap();
    }
    {
        let sink = Sink::open(LakeConfig::new(env.dir.path())).unwrap();
        let mut start = Event::now(kind::STARTUP, "heatd");
        start.ts = 1_761_580_850_000_000;
        sink.append_on(date_of_us(start.ts), start).unwrap();
        sink.close().unwrap();
    }
    let config = crate::config::Config { db_path: db.to_str().unwrap().into(), data_dir: env.dir.path().to_str().unwrap().into(), ..Default::default() };
    crate::migrate::run(&config).unwrap();
    assert!(crate::migrate::run(&config).is_err());
    let sink = env.sink(10, 3600);
    sink.roll_pending().unwrap();
    let date = "2025-10-27";
    assert_eq!(parquet_rows(&env.parquet(Table::WatchRaw, date).join("part.parquet")), 2);
    assert_eq!(parquet_rows(&env.parquet(Table::Labels, date).join("part.parquet")), 2);
    assert_eq!(parquet_rows(&env.parquet(Table::AttributeReports, date).join("node=5/part.parquet")), 1);
    assert_eq!(parquet_rows(&env.parquet(Table::Decisions, date).join("node=5/part.parquet")), 1);
    assert_eq!(parquet_rows(&env.parquet(Table::Events, date).join("node=none/part.parquet")), 2);
}
