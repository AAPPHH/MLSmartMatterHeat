#![allow(dead_code)]
use anyhow::{anyhow, bail, Context, Result};
use arrow::array::{new_null_array, Array, ArrayRef, BinaryArray, BooleanArray, DictionaryArray, PrimitiveArray, StringArray, StructArray, TimestampMicrosecondArray};
use arrow::compute::filter_record_batch;
use arrow::datatypes::{ArrowPrimitiveType, DataType, Field, Fields, Float32Type, Float64Type, Int32Type, Int64Type, Int8Type, Schema, SchemaRef, TimeUnit, UInt16Type, UInt32Type, UInt64Type, UInt8Type};
use arrow::ipc::reader::StreamReader;
use arrow::ipc::writer::StreamWriter;
use arrow::ipc::{root_as_message, MessageHeader};
use arrow::record_batch::RecordBatch;
use chrono::{NaiveDate, NaiveDateTime, NaiveTime, Utc};
use parquet::arrow::ArrowWriter;
use parquet::basic::{Compression, ZstdLevel};
use parquet::file::properties::WriterProperties;
use parquet::file::reader::{FileReader, SerializedFileReader};
use parquet::file::metadata::KeyValue;
use parquet::schema::types::ColumnPath;
use std::any::Any;
use std::collections::{BTreeMap, BTreeSet, HashMap};
use std::fs::{File, OpenOptions};
use std::io::{BufWriter, Cursor, Read, Write};
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex, OnceLock};
use std::time::{Duration, Instant};

pub const SCHEMA_VERSION: u32 = 5;
pub const COMMIT: &str = env!("HEATD_COMMIT");
pub const ROW_GROUP_ROWS: usize = 65_536;
pub const EOS: [u8; 8] = [0xff, 0xff, 0xff, 0xff, 0, 0, 0, 0];
pub const ROLL_HOUR: u32 = 3;
pub const NO_NODE: &str = "none";

pub mod kind {
    pub const MATTER_EVENT: &str = "matter_event";
    pub const SUBSCRIPTION_LOST: &str = "subscription_lost";
    pub const RESUBSCRIBED: &str = "resubscribed";
    pub const STARTUP: &str = "startup";
    pub const SHUTDOWN: &str = "shutdown";
    pub const FW_CHANGE: &str = "fw_change";
    pub const EXPERIMENT_START: &str = "experiment_start";
    pub const EXPERIMENT_END: &str = "experiment_end";
    pub const EXPERIMENT_ABORT: &str = "experiment_abort";
    pub const NTP_DRIFT: &str = "ntp_drift";
    pub const PRESENCE_ARRIVE: &str = "presence_arrive";
    pub const PRESENCE_LEAVE: &str = "presence_leave";
    pub const ROLL_OK: &str = "roll_ok";
    pub const ROLL_FAILED: &str = "roll_failed";
    pub const WATCH_STATE: &str = "watch_state";
    pub const POLL_FAILED: &str = "poll_failed";
    pub const MATCHER_DRIFT: &str = "matcher_drift";
    pub const OCCUPANCY_END: &str = "occupancy_end";
    pub const WINDOW_OPEN: &str = "window_open";
    pub const WINDOW_CLOSE: &str = "window_close";
    pub const WINDOW_REOPEN: &str = "window_reopen";
    pub const WINDOW_SUPPRESSED: &str = "window_suppressed";
    pub const BOOST_CYCLE: &str = "boost_cycle";
    pub const FAILSAFE: &str = "failsafe";
    pub const BR_WATCHDOG: &str = "br_watchdog";
    pub const SUPPLY: &str = "supply";
    pub const PI_RESET: &str = "pi_reset";
    pub const WINDOW_EPISODE: &str = "window_episode";
    pub const ALL: [&str; 28] = [
        MATTER_EVENT, SUBSCRIPTION_LOST, RESUBSCRIBED, STARTUP, SHUTDOWN, FW_CHANGE, EXPERIMENT_START, EXPERIMENT_END, EXPERIMENT_ABORT, NTP_DRIFT, PRESENCE_ARRIVE, PRESENCE_LEAVE,
        ROLL_OK, ROLL_FAILED, WATCH_STATE, POLL_FAILED, MATCHER_DRIFT, OCCUPANCY_END,
        WINDOW_OPEN, WINDOW_CLOSE, WINDOW_REOPEN, WINDOW_SUPPRESSED, BOOST_CYCLE, FAILSAFE, BR_WATCHDOG, SUPPLY, PI_RESET, WINDOW_EPISODE,
    ];
}

pub fn now_us() -> i64 {
    Utc::now().timestamp_micros()
}

pub fn mono_ns() -> i64 {
    static START: OnceLock<Instant> = OnceLock::new();
    START.get_or_init(Instant::now).elapsed().as_nanos() as i64
}

pub fn date_of_us(ts_us: i64) -> NaiveDate {
    chrono::DateTime::from_timestamp_micros(ts_us).unwrap_or_default().date_naive()
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub enum Table {
    AttributeReports,
    Events,
    Decisions,
    Labels,
    WatchRaw,
    Weather,
    Meta,
    WatchSensors,
    WatchInference,
    Ambient,
}

impl Table {
    pub const ALL: [Table; 10] = [Table::AttributeReports, Table::Events, Table::Decisions, Table::Labels, Table::WatchRaw, Table::Weather, Table::Meta, Table::WatchSensors, Table::WatchInference, Table::Ambient];

    pub fn name(self) -> &'static str {
        match self {
            Table::AttributeReports => "attribute_reports",
            Table::Events => "events",
            Table::Decisions => "decisions",
            Table::Labels => "labels",
            Table::WatchRaw => "watch_raw",
            Table::Weather => "weather",
            Table::Meta => "meta",
            Table::WatchSensors => "watch_sensors",
            Table::WatchInference => "watch_inference",
            Table::Ambient => "ambient",
        }
    }

    pub fn node_partitioned(self) -> bool {
        matches!(self, Table::AttributeReports | Table::Events | Table::Decisions)
    }

    pub fn schema(self) -> SchemaRef {
        let fields: Vec<Field> = match self {
            Table::AttributeReports => vec![
                tsf("ts", false), fld("mono_ns", DataType::Int64, false), fld("node", DataType::UInt64, false), fld("endpoint", DataType::UInt16, false), fld("cluster", DataType::UInt32, false),
                fld("attribute", DataType::UInt32, false), fld("dtype", dict(), false), fld("v_i64", DataType::Int64, true), fld("v_f64", DataType::Float64, true), fld("v_bool", DataType::Boolean, true),
                fld("v_str", DataType::Utf8, true), fld("v_bin", DataType::Binary, true), fld("value_raw", DataType::Binary, false), fld("source", dict(), false), fld("fw", dict(), false),
            ],
            Table::Events => vec![
                tsf("ts", false), fld("mono_ns", DataType::Int64, false), fld("kind", dict(), false), fld("node", DataType::UInt64, true), fld("endpoint", DataType::UInt16, true),
                fld("cluster", DataType::UInt32, true), fld("attribute", DataType::UInt32, true), fld("event_id", DataType::UInt32, true), fld("event_number", DataType::UInt64, true),
                fld("priority", DataType::UInt8, true), tsf("device_ts", true), fld("dtype", dict(), true), fld("v_i64", DataType::Int64, true), fld("v_f64", DataType::Float64, true),
                fld("v_bool", DataType::Boolean, true), fld("v_str", DataType::Utf8, true), fld("v_bin", DataType::Binary, true), fld("value_raw", DataType::Binary, true), fld("source", dict(), false),
                fld("fw", dict(), true), fld("experiment_id", DataType::Utf8, true), fld("detail", DataType::Utf8, true),
            ],
            Table::Decisions => vec![
                tsf("ts", false), fld("room", dict(), false), fld("node", DataType::UInt64, false), fld("rule", dict(), false), fld("inputs", DataType::Struct(inputs_fields()), false),
                fld("setpoint_old", DataType::Float64, true), fld("setpoint_new", DataType::Float64, true), fld("mode", dict(), true), fld("exploration", DataType::Boolean, false),
                fld("experiment_id", DataType::Utf8, true), fld("proposed", DataType::Boolean, false), fld("applied", DataType::Boolean, false), fld("policy_id", DataType::Utf8, true),
                fld("commit", dict(), false),
            ],
            Table::Labels => vec![
                tsf("ts", false), tsf("from_ts", true), tsf("to_ts", true), fld("room", dict(), false), fld("source", dict(), false), fld("device", dict(), true),
                fld("confidence", DataType::Float32, true), fld("model_id", DataType::Utf8, true), fld("probs", DataType::Utf8, true), fld("heat", DataType::Boolean, true),
                fld("label_id", DataType::Utf8, true), fld("voided_label_id", DataType::Utf8, true), fld("window_open", DataType::Boolean, true),
            ],
            Table::WatchRaw => vec![
                tsf("ts", false), tsf("device_ts", true), fld("sender", dict(), false), fld("rssi", DataType::Int8, false), fld("channel", DataType::UInt8, true), fld("batch_id", DataType::UInt64, false),
                fld("device", dict(), false), fld("freq", DataType::UInt16, true), tsf("received_ts", true),
                fld("elapsed_ns", DataType::Int64, true), fld("clock_offset_ms", DataType::Int32, true), fld("sensor_type", dict(), true), fld("sensor_name", dict(), true),
                fld("value", DataType::Float32, true), fld("unit", dict(), true),
            ],
            Table::Weather => vec![
                tsf("ts", false), fld("kind", dict(), false), tsf("issued_ts", true), tsf("fetched_ts", false), fld("temp_c", DataType::Float32, true), fld("wind_ms", DataType::Float32, true),
                fld("radiation_wm2", DataType::Float32, true), fld("cloud_pct", DataType::Float32, true), fld("precip_mm", DataType::Float32, true),
            ],
            Table::Meta => vec![tsf("ts", false), fld("key", dict(), false), fld("value", DataType::Utf8, false), tsf("valid_from", false)],
            Table::WatchSensors => vec![
                tsf("ts", false), fld("sensor_name", dict(), false), fld("android_type", DataType::Int32, false), fld("vendor", dict(), true), fld("resolution", DataType::Float32, true),
                fld("max_range", DataType::Float32, true), fld("app_version", dict(), true),
            ],
            Table::WatchInference => vec![
                tsf("ts", false), fld("matcher", dict(), false), fld("template_id", DataType::Utf8, false), fld("result_room", dict(), true), fld("result_temp", DataType::Float32, true),
                fld("confidence", DataType::Float32, false), fld("abstained", DataType::Boolean, false), fld("reason", dict(), true), fld("distance", DataType::Float32, true),
                fld("state", dict(), false), fld("features_hash", DataType::Utf8, false), fld("model_version", dict(), false),
            ],
            Table::Ambient => vec![tsf("ts", false), fld("source", dict(), false), fld("sensor", dict(), false), fld("value", DataType::Float64, false), fld("unit", dict(), false)],
        };
        let metadata = HashMap::from([("schema_version".to_string(), SCHEMA_VERSION.to_string()), ("heatd_commit".to_string(), COMMIT.to_string()), ("table".to_string(), self.name().to_string())]);
        Arc::new(Schema::new_with_metadata(fields, metadata))
    }
}

fn fld(name: &str, dt: DataType, nullable: bool) -> Field {
    Field::new(name, dt, nullable)
}

fn tsf(name: &str, nullable: bool) -> Field {
    fld(name, DataType::Timestamp(TimeUnit::Microsecond, Some("UTC".into())), nullable)
}

fn dict() -> DataType {
    DataType::Dictionary(Box::new(DataType::Int32), Box::new(DataType::Utf8))
}

fn inputs_fields() -> Fields {
    let f64s = ["local_temp", "outdoor_temp", "schedule_setpoint", "frost_setpoint", "demand_pct", "battery_pct", "prediction_age_min", "contact_age_min"];
    let mut fields: Vec<Field> = f64s.iter().map(|n| fld(n, DataType::Float64, true)).collect();
    fields.extend([
        fld("occupancy", DataType::Utf8, true), fld("room_estimate", DataType::Utf8, true), fld("window_open", DataType::Boolean, true), fld("away", DataType::Boolean, true),
        fld("system_mode", DataType::Utf8, true), fld("extra", DataType::Utf8, true),
    ]);
    fields.into()
}

pub fn type_name(dt: &DataType) -> String {
    match dt {
        DataType::Timestamp(TimeUnit::Microsecond, Some(tz)) => format!("timestamp[us,{tz}]"),
        DataType::Dictionary(_, v) => format!("dict<{}>", type_name(v)),
        DataType::Struct(fields) => format!("struct<{}>", fields.iter().map(|f| field_line(f)).collect::<Vec<_>>().join(", ")),
        DataType::Utf8 => "utf8".into(),
        DataType::Binary => "binary".into(),
        DataType::Boolean => "bool".into(),
        DataType::Int8 => "i8".into(),
        DataType::Int32 => "i32".into(),
        DataType::Int64 => "i64".into(),
        DataType::UInt8 => "u8".into(),
        DataType::UInt16 => "u16".into(),
        DataType::UInt32 => "u32".into(),
        DataType::UInt64 => "u64".into(),
        DataType::Float32 => "f32".into(),
        DataType::Float64 => "f64".into(),
        other => format!("{other:?}"),
    }
}

pub fn field_line(f: &Field) -> String {
    format!("{}: {}{}", f.name(), type_name(f.data_type()), if f.is_nullable() { "?" } else { "" })
}

pub fn describe(schema: &Schema) -> String {
    schema.fields().iter().map(|f| field_line(f)).collect::<Vec<_>>().join("\n")
}

pub trait Row: Send + Sized + 'static {
    const TABLE: Table;
    fn to_batch(rows: &[Self]) -> Result<RecordBatch>;
}

fn prim<T: ArrowPrimitiveType, R>(rows: &[R], f: impl Fn(&R) -> Option<T::Native>) -> ArrayRef {
    Arc::new(PrimitiveArray::<T>::from_iter(rows.iter().map(f)))
}

fn ts_col<R>(rows: &[R], f: impl Fn(&R) -> Option<i64>) -> ArrayRef {
    Arc::new(TimestampMicrosecondArray::from_iter(rows.iter().map(f)).with_timezone("UTC"))
}

fn bool_col<R>(rows: &[R], f: impl Fn(&R) -> Option<bool>) -> ArrayRef {
    Arc::new(BooleanArray::from_iter(rows.iter().map(f)))
}

fn str_col<R>(rows: &[R], f: impl for<'a> Fn(&'a R) -> Option<&'a str>) -> ArrayRef {
    Arc::new(StringArray::from_iter(rows.iter().map(f)))
}

fn dict_col<R>(rows: &[R], f: impl for<'a> Fn(&'a R) -> Option<&'a str>) -> ArrayRef {
    Arc::new(rows.iter().map(f).collect::<DictionaryArray<Int32Type>>())
}

fn bin_col<R>(rows: &[R], f: impl for<'a> Fn(&'a R) -> Option<&'a [u8]>) -> ArrayRef {
    Arc::new(BinaryArray::from_iter(rows.iter().map(f)))
}

#[derive(Clone, Debug, Default, PartialEq)]
pub struct AttributeReport {
    pub ts: i64,
    pub mono_ns: i64,
    pub node: u64,
    pub endpoint: u16,
    pub cluster: u32,
    pub attribute: u32,
    pub dtype: String,
    pub v_i64: Option<i64>,
    pub v_f64: Option<f64>,
    pub v_bool: Option<bool>,
    pub v_str: Option<String>,
    pub v_bin: Option<Vec<u8>>,
    pub value_raw: Vec<u8>,
    pub source: String,
    pub fw: String,
}

impl Row for AttributeReport {
    const TABLE: Table = Table::AttributeReports;
    fn to_batch(rows: &[Self]) -> Result<RecordBatch> {
        let cols = vec![
            ts_col(rows, |r| Some(r.ts)), prim::<Int64Type, _>(rows, |r| Some(r.mono_ns)), prim::<UInt64Type, _>(rows, |r| Some(r.node)), prim::<UInt16Type, _>(rows, |r| Some(r.endpoint)),
            prim::<UInt32Type, _>(rows, |r| Some(r.cluster)), prim::<UInt32Type, _>(rows, |r| Some(r.attribute)), dict_col(rows, |r| Some(&r.dtype)), prim::<Int64Type, _>(rows, |r| r.v_i64),
            prim::<Float64Type, _>(rows, |r| r.v_f64), bool_col(rows, |r| r.v_bool), str_col(rows, |r| r.v_str.as_deref()), bin_col(rows, |r| r.v_bin.as_deref()),
            bin_col(rows, |r| Some(&r.value_raw)), dict_col(rows, |r| Some(&r.source)), dict_col(rows, |r| Some(&r.fw)),
        ];
        Ok(RecordBatch::try_new(Self::TABLE.schema(), cols)?)
    }
}

#[derive(Clone, Debug, Default, PartialEq)]
pub struct Event {
    pub ts: i64,
    pub mono_ns: i64,
    pub kind: String,
    pub node: Option<u64>,
    pub endpoint: Option<u16>,
    pub cluster: Option<u32>,
    pub attribute: Option<u32>,
    pub event_id: Option<u32>,
    pub event_number: Option<u64>,
    pub priority: Option<u8>,
    pub device_ts: Option<i64>,
    pub dtype: Option<String>,
    pub v_i64: Option<i64>,
    pub v_f64: Option<f64>,
    pub v_bool: Option<bool>,
    pub v_str: Option<String>,
    pub v_bin: Option<Vec<u8>>,
    pub value_raw: Option<Vec<u8>>,
    pub source: String,
    pub fw: Option<String>,
    pub experiment_id: Option<String>,
    pub detail: Option<String>,
}

impl Event {
    pub fn now(kind: &str, source: &str) -> Self {
        Self { ts: now_us(), mono_ns: mono_ns(), kind: kind.into(), source: source.into(), ..Default::default() }
    }

    pub fn with_node(mut self, node: u64) -> Self {
        self.node = Some(node);
        self
    }

    pub fn with_detail(mut self, detail: serde_json::Value) -> Self {
        self.detail = Some(detail.to_string());
        self
    }

    pub fn with_experiment(mut self, id: &str) -> Self {
        self.experiment_id = Some(id.into());
        self
    }
}

impl Row for Event {
    const TABLE: Table = Table::Events;
    fn to_batch(rows: &[Self]) -> Result<RecordBatch> {
        let cols = vec![
            ts_col(rows, |r| Some(r.ts)), prim::<Int64Type, _>(rows, |r| Some(r.mono_ns)), dict_col(rows, |r| Some(&r.kind)), prim::<UInt64Type, _>(rows, |r| r.node),
            prim::<UInt16Type, _>(rows, |r| r.endpoint), prim::<UInt32Type, _>(rows, |r| r.cluster), prim::<UInt32Type, _>(rows, |r| r.attribute), prim::<UInt32Type, _>(rows, |r| r.event_id),
            prim::<UInt64Type, _>(rows, |r| r.event_number), prim::<UInt8Type, _>(rows, |r| r.priority), ts_col(rows, |r| r.device_ts), dict_col(rows, |r| r.dtype.as_deref()),
            prim::<Int64Type, _>(rows, |r| r.v_i64), prim::<Float64Type, _>(rows, |r| r.v_f64), bool_col(rows, |r| r.v_bool), str_col(rows, |r| r.v_str.as_deref()),
            bin_col(rows, |r| r.v_bin.as_deref()), bin_col(rows, |r| r.value_raw.as_deref()), dict_col(rows, |r| Some(&r.source)), dict_col(rows, |r| r.fw.as_deref()),
            str_col(rows, |r| r.experiment_id.as_deref()), str_col(rows, |r| r.detail.as_deref()),
        ];
        Ok(RecordBatch::try_new(Self::TABLE.schema(), cols)?)
    }
}

#[derive(Clone, Debug, Default, PartialEq)]
pub struct DecisionInputs {
    pub local_temp: Option<f64>,
    pub outdoor_temp: Option<f64>,
    pub schedule_setpoint: Option<f64>,
    pub frost_setpoint: Option<f64>,
    pub demand_pct: Option<f64>,
    pub battery_pct: Option<f64>,
    pub prediction_age_min: Option<f64>,
    pub contact_age_min: Option<f64>,
    pub occupancy: Option<String>,
    pub room_estimate: Option<String>,
    pub window_open: Option<bool>,
    pub away: Option<bool>,
    pub system_mode: Option<String>,
    pub extra: Option<String>,
}

#[derive(Clone, Debug, Default, PartialEq)]
pub struct Decision {
    pub ts: i64,
    pub room: String,
    pub node: u64,
    pub rule: String,
    pub inputs: DecisionInputs,
    pub setpoint_old: Option<f64>,
    pub setpoint_new: Option<f64>,
    pub mode: Option<String>,
    pub exploration: bool,
    pub experiment_id: Option<String>,
    pub proposed: bool,
    pub applied: bool,
    pub policy_id: Option<String>,
    pub commit: String,
}

impl Row for Decision {
    const TABLE: Table = Table::Decisions;
    fn to_batch(rows: &[Self]) -> Result<RecordBatch> {
        let i: Vec<&DecisionInputs> = rows.iter().map(|r| &r.inputs).collect();
        let children: Vec<ArrayRef> = vec![
            prim::<Float64Type, _>(&i, |r| r.local_temp), prim::<Float64Type, _>(&i, |r| r.outdoor_temp), prim::<Float64Type, _>(&i, |r| r.schedule_setpoint),
            prim::<Float64Type, _>(&i, |r| r.frost_setpoint), prim::<Float64Type, _>(&i, |r| r.demand_pct), prim::<Float64Type, _>(&i, |r| r.battery_pct),
            prim::<Float64Type, _>(&i, |r| r.prediction_age_min), prim::<Float64Type, _>(&i, |r| r.contact_age_min), str_col(&i, |r| r.occupancy.as_deref()),
            str_col(&i, |r| r.room_estimate.as_deref()), bool_col(&i, |r| r.window_open), bool_col(&i, |r| r.away), str_col(&i, |r| r.system_mode.as_deref()), str_col(&i, |r| r.extra.as_deref()),
        ];
        let inputs: ArrayRef = Arc::new(StructArray::try_new(inputs_fields(), children, None)?);
        let cols = vec![
            ts_col(rows, |r| Some(r.ts)), dict_col(rows, |r| Some(&r.room)), prim::<UInt64Type, _>(rows, |r| Some(r.node)), dict_col(rows, |r| Some(&r.rule)), inputs,
            prim::<Float64Type, _>(rows, |r| r.setpoint_old), prim::<Float64Type, _>(rows, |r| r.setpoint_new), dict_col(rows, |r| r.mode.as_deref()), bool_col(rows, |r| Some(r.exploration)),
            str_col(rows, |r| r.experiment_id.as_deref()), bool_col(rows, |r| Some(r.proposed)), bool_col(rows, |r| Some(r.applied)), str_col(rows, |r| r.policy_id.as_deref()),
            dict_col(rows, |r| Some(&r.commit)),
        ];
        Ok(RecordBatch::try_new(Self::TABLE.schema(), cols)?)
    }
}

#[derive(Clone, Debug, Default, PartialEq)]
pub struct Label {
    pub ts: i64,
    pub from_ts: Option<i64>,
    pub to_ts: Option<i64>,
    pub room: String,
    pub source: String,
    pub device: Option<String>,
    pub confidence: Option<f32>,
    pub model_id: Option<String>,
    pub probs: Option<String>,
    pub heat: Option<bool>,
    pub label_id: Option<String>,
    pub voided_label_id: Option<String>,
    pub window_open: Option<bool>,
}

impl Row for Label {
    const TABLE: Table = Table::Labels;
    fn to_batch(rows: &[Self]) -> Result<RecordBatch> {
        let cols = vec![
            ts_col(rows, |r| Some(r.ts)), ts_col(rows, |r| r.from_ts), ts_col(rows, |r| r.to_ts), dict_col(rows, |r| Some(&r.room)), dict_col(rows, |r| Some(&r.source)),
            dict_col(rows, |r| r.device.as_deref()), prim::<Float32Type, _>(rows, |r| r.confidence), str_col(rows, |r| r.model_id.as_deref()), str_col(rows, |r| r.probs.as_deref()), bool_col(rows, |r| r.heat),
            str_col(rows, |r| r.label_id.as_deref()), str_col(rows, |r| r.voided_label_id.as_deref()), bool_col(rows, |r| r.window_open),
        ];
        Ok(RecordBatch::try_new(Self::TABLE.schema(), cols)?)
    }
}

#[derive(Clone, Debug, Default, PartialEq)]
pub struct WatchRaw {
    pub ts: i64,
    pub device_ts: Option<i64>,
    pub sender: String,
    pub rssi: i8,
    pub channel: Option<u8>,
    pub batch_id: u64,
    pub device: String,
    pub freq: Option<u16>,
    pub received_ts: Option<i64>,
    pub elapsed_ns: Option<i64>,
    pub clock_offset_ms: Option<i32>,
    pub sensor_type: Option<String>,
    pub sensor_name: Option<String>,
    pub value: Option<f32>,
    pub unit: Option<String>,
}

impl Row for WatchRaw {
    const TABLE: Table = Table::WatchRaw;
    fn to_batch(rows: &[Self]) -> Result<RecordBatch> {
        let cols = vec![
            ts_col(rows, |r| Some(r.ts)), ts_col(rows, |r| r.device_ts), dict_col(rows, |r| Some(&r.sender)), prim::<Int8Type, _>(rows, |r| Some(r.rssi)), prim::<UInt8Type, _>(rows, |r| r.channel),
            prim::<UInt64Type, _>(rows, |r| Some(r.batch_id)), dict_col(rows, |r| Some(&r.device)), prim::<UInt16Type, _>(rows, |r| r.freq), ts_col(rows, |r| r.received_ts),
            prim::<Int64Type, _>(rows, |r| r.elapsed_ns), prim::<Int32Type, _>(rows, |r| r.clock_offset_ms), dict_col(rows, |r| r.sensor_type.as_deref()), dict_col(rows, |r| r.sensor_name.as_deref()),
            prim::<Float32Type, _>(rows, |r| r.value), dict_col(rows, |r| r.unit.as_deref()),
        ];
        Ok(RecordBatch::try_new(Self::TABLE.schema(), cols)?)
    }
}

#[derive(Clone, Debug, Default, PartialEq)]
pub struct Weather {
    pub ts: i64,
    pub kind: String,
    pub issued_ts: Option<i64>,
    pub fetched_ts: i64,
    pub temp_c: Option<f32>,
    pub wind_ms: Option<f32>,
    pub radiation_wm2: Option<f32>,
    pub cloud_pct: Option<f32>,
    pub precip_mm: Option<f32>,
}

impl Row for Weather {
    const TABLE: Table = Table::Weather;
    fn to_batch(rows: &[Self]) -> Result<RecordBatch> {
        let cols = vec![
            ts_col(rows, |r| Some(r.ts)), dict_col(rows, |r| Some(&r.kind)), ts_col(rows, |r| r.issued_ts), ts_col(rows, |r| Some(r.fetched_ts)), prim::<Float32Type, _>(rows, |r| r.temp_c),
            prim::<Float32Type, _>(rows, |r| r.wind_ms), prim::<Float32Type, _>(rows, |r| r.radiation_wm2), prim::<Float32Type, _>(rows, |r| r.cloud_pct), prim::<Float32Type, _>(rows, |r| r.precip_mm),
        ];
        Ok(RecordBatch::try_new(Self::TABLE.schema(), cols)?)
    }
}

#[derive(Clone, Debug, Default, PartialEq)]
pub struct Meta {
    pub ts: i64,
    pub key: String,
    pub value: String,
    pub valid_from: i64,
}

impl Row for Meta {
    const TABLE: Table = Table::Meta;
    fn to_batch(rows: &[Self]) -> Result<RecordBatch> {
        let cols = vec![ts_col(rows, |r| Some(r.ts)), dict_col(rows, |r| Some(&r.key)), str_col(rows, |r| Some(&r.value)), ts_col(rows, |r| Some(r.valid_from))];
        Ok(RecordBatch::try_new(Self::TABLE.schema(), cols)?)
    }
}

#[derive(Clone, Debug, Default, PartialEq)]
pub struct WatchSensor {
    pub ts: i64,
    pub sensor_name: String,
    pub android_type: i32,
    pub vendor: Option<String>,
    pub resolution: Option<f32>,
    pub max_range: Option<f32>,
    pub app_version: Option<String>,
}

impl Row for WatchSensor {
    const TABLE: Table = Table::WatchSensors;
    fn to_batch(rows: &[Self]) -> Result<RecordBatch> {
        let cols = vec![
            ts_col(rows, |r| Some(r.ts)), dict_col(rows, |r| Some(&r.sensor_name)), prim::<Int32Type, _>(rows, |r| Some(r.android_type)), dict_col(rows, |r| r.vendor.as_deref()),
            prim::<Float32Type, _>(rows, |r| r.resolution), prim::<Float32Type, _>(rows, |r| r.max_range), dict_col(rows, |r| r.app_version.as_deref()),
        ];
        Ok(RecordBatch::try_new(Self::TABLE.schema(), cols)?)
    }
}

#[derive(Clone, Debug, Default, PartialEq)]
pub struct Ambient {
    pub ts: i64,
    pub source: String,
    pub sensor: String,
    pub value: f64,
    pub unit: String,
}

impl Row for Ambient {
    const TABLE: Table = Table::Ambient;
    fn to_batch(rows: &[Self]) -> Result<RecordBatch> {
        let cols = vec![ts_col(rows, |r| Some(r.ts)), dict_col(rows, |r| Some(&r.source)), dict_col(rows, |r| Some(&r.sensor)), prim::<Float64Type, _>(rows, |r| Some(r.value)), dict_col(rows, |r| Some(&r.unit))];
        Ok(RecordBatch::try_new(Self::TABLE.schema(), cols)?)
    }
}

#[derive(Clone, Debug, Default, PartialEq)]
pub struct WatchInference {
    pub ts: i64,
    pub matcher: String,
    pub template_id: String,
    pub result_room: Option<String>,
    pub result_temp: Option<f32>,
    pub confidence: f32,
    pub abstained: bool,
    pub reason: Option<String>,
    pub distance: Option<f32>,
    pub state: String,
    pub features_hash: String,
    pub model_version: String,
}

impl Row for WatchInference {
    const TABLE: Table = Table::WatchInference;
    fn to_batch(rows: &[Self]) -> Result<RecordBatch> {
        let cols = vec![
            ts_col(rows, |r| Some(r.ts)), dict_col(rows, |r| Some(&r.matcher)), str_col(rows, |r| Some(&r.template_id)), dict_col(rows, |r| r.result_room.as_deref()),
            prim::<Float32Type, _>(rows, |r| r.result_temp), prim::<Float32Type, _>(rows, |r| Some(r.confidence)), bool_col(rows, |r| Some(r.abstained)), dict_col(rows, |r| r.reason.as_deref()),
            prim::<Float32Type, _>(rows, |r| r.distance), dict_col(rows, |r| Some(&r.state)), str_col(rows, |r| Some(&r.features_hash)), dict_col(rows, |r| Some(&r.model_version)),
        ];
        Ok(RecordBatch::try_new(Self::TABLE.schema(), cols)?)
    }
}

#[derive(Debug, Clone)]
pub struct LakeConfig {
    pub root: PathBuf,
    pub flush_rows: usize,
    pub flush_secs: u64,
}

impl LakeConfig {
    pub fn new(root: impl Into<PathBuf>) -> Self {
        Self { root: root.into(), flush_rows: 500, flush_secs: 30 }
    }
}

pub fn raw_dir(root: &Path, table: Table) -> PathBuf {
    root.join("raw").join(table.name())
}

pub fn parquet_dir(root: &Path, table: Table, date: NaiveDate) -> PathBuf {
    root.join("parquet").join(table.name()).join(format!("date={date}"))
}

fn sync_dir(dir: &Path) {
    if let Ok(f) = File::open(dir) {
        let _ = f.sync_all();
    }
}

struct Framing {
    schema_end: Option<usize>,
    batch_ends: Vec<usize>,
    eos: bool,
}

fn frame(bytes: &[u8]) -> Framing {
    let mut out = Framing { schema_end: None, batch_ends: Vec::new(), eos: false };
    let mut pos = 0;
    while let Some(head) = bytes.get(pos..pos + 8) {
        if head[..4] != [0xff; 4] {
            break;
        }
        let meta_len = i32::from_le_bytes(head[4..8].try_into().unwrap());
        if meta_len == 0 {
            out.eos = pos + 8 == bytes.len();
            break;
        }
        let Some(meta) = (meta_len > 0).then(|| bytes.get(pos + 8..pos + 8 + meta_len as usize)).flatten() else { break };
        let Ok(message) = root_as_message(meta) else { break };
        let end = pos + 8 + meta_len as usize + message.bodyLength().max(0) as usize;
        if end > bytes.len() {
            break;
        }
        match message.header_type() {
            MessageHeader::Schema => out.schema_end = Some(end),
            MessageHeader::RecordBatch => out.batch_ends.push(end),
            MessageHeader::DictionaryBatch => {}
            _ => break,
        }
        pos = end;
    }
    out
}

pub struct Loaded {
    pub batches: Vec<RecordBatch>,
    pub keep_len: Option<usize>,
    pub complete: bool,
}

pub fn load_stream(path: &Path) -> Result<Loaded> {
    let mut bytes = Vec::new();
    File::open(path).with_context(|| format!("open {}", path.display()))?.read_to_end(&mut bytes)?;
    let framed = frame(&bytes);
    let Some(schema_end) = framed.schema_end else { return Ok(Loaded { batches: Vec::new(), keep_len: None, complete: false }) };
    let limit = framed.batch_ends.last().copied().unwrap_or(schema_end);
    let mut prefix = bytes[..limit].to_vec();
    prefix.extend_from_slice(&EOS);
    let mut batches = Vec::new();
    if let Ok(reader) = StreamReader::try_new(Cursor::new(prefix), None) {
        for batch in reader {
            match batch {
                Ok(b) => batches.push(b),
                Err(_) => break,
            }
        }
    }
    let complete = framed.eos && batches.len() == framed.batch_ends.len();
    let keep = if batches.len() < framed.batch_ends.len() { batches.len().checked_sub(1).map_or(schema_end, |i| framed.batch_ends[i]) } else { limit };
    Ok(Loaded { complete, batches, keep_len: Some(keep) })
}

pub struct Recovery {
    pub files: usize,
    pub truncated_bytes: u64,
    pub quarantined: usize,
}

fn recover_all(root: &Path) -> Result<Recovery> {
    let mut rec = Recovery { files: 0, truncated_bytes: 0, quarantined: 0 };
    for table in Table::ALL {
        for (path, _) in raw_files(root, table, None)? {
            let len = std::fs::metadata(&path)?.len();
            let loaded = load_stream(&path)?;
            if loaded.complete {
                continue;
            }
            match loaded.keep_len {
                None => {
                    std::fs::rename(&path, path.with_extension("arrows.corrupt"))?;
                    rec.quarantined += 1;
                }
                Some(keep) => {
                    let file = OpenOptions::new().write(true).open(&path)?;
                    file.set_len(keep as u64)?;
                    file.sync_all()?;
                    append_at(&path, keep as u64)?;
                    rec.files += 1;
                    rec.truncated_bytes += len.saturating_sub(keep as u64);
                }
            }
        }
    }
    Ok(rec)
}

fn append_at(path: &Path, offset: u64) -> std::io::Result<()> {
    use std::io::{Seek, SeekFrom};
    let mut f = OpenOptions::new().write(true).open(path)?;
    f.seek(SeekFrom::Start(offset))?;
    f.write_all(&EOS)?;
    f.sync_all()
}

fn parse_raw_name(name: &str) -> Option<(NaiveDate, u32)> {
    let stem = name.strip_suffix(".arrows")?;
    let (date, n) = stem.split_once('.').map_or((stem, Some(0)), |(d, n)| (d, n.parse().ok()));
    Some((NaiveDate::parse_from_str(date, "%Y-%m-%d").ok()?, n?))
}

pub fn raw_files(root: &Path, table: Table, date: Option<NaiveDate>) -> Result<Vec<(PathBuf, u64)>> {
    let dir = raw_dir(root, table);
    let Ok(entries) = std::fs::read_dir(&dir) else { return Ok(Vec::new()) };
    let mut files: Vec<(NaiveDate, u32, PathBuf, u64)> = Vec::new();
    for entry in entries {
        let entry = entry?;
        if let Some((d, n)) = entry.file_name().to_str().and_then(parse_raw_name).filter(|(d, _)| date.is_none_or(|x| x == *d)) {
            files.push((d, n, entry.path(), entry.metadata()?.len()));
        }
    }
    files.sort();
    Ok(files.into_iter().map(|(_, _, p, l)| (p, l)).collect())
}

pub fn raw_dates(root: &Path) -> BTreeSet<NaiveDate> {
    let mut dates = BTreeSet::new();
    for table in Table::ALL {
        if let Ok(entries) = std::fs::read_dir(raw_dir(root, table)) {
            dates.extend(entries.flatten().filter_map(|e| e.file_name().to_str().and_then(parse_raw_name).map(|(d, _)| d)));
        }
    }
    dates
}

struct Seg {
    writer: StreamWriter<BufWriter<File>>,
}

impl Seg {
    fn create(root: &Path, table: Table, date: NaiveDate) -> Result<Self> {
        let dir = raw_dir(root, table);
        std::fs::create_dir_all(&dir)?;
        let mut n = 0u32;
        let file = loop {
            let name = if n == 0 { format!("{date}.arrows") } else { format!("{date}.{n}.arrows") };
            match OpenOptions::new().create_new(true).write(true).open(dir.join(name)) {
                Ok(f) => break f,
                Err(e) if e.kind() == std::io::ErrorKind::AlreadyExists => n += 1,
                Err(e) => return Err(e.into()),
            }
        };
        sync_dir(&dir);
        Ok(Self { writer: StreamWriter::try_new(BufWriter::new(file), &table.schema())? })
    }

    fn write(&mut self, batch: &RecordBatch) -> Result<()> {
        self.writer.write(batch)?;
        self.sync()
    }

    fn sync(&mut self) -> Result<()> {
        self.writer.get_mut().flush()?;
        self.writer.get_ref().get_ref().sync_all()?;
        Ok(())
    }

    fn finish(&mut self) -> Result<()> {
        self.writer.finish()?;
        self.sync()
    }
}

struct Buf {
    rows: Box<dyn Any + Send>,
    len: usize,
    since: Instant,
    to_batch: fn(&dyn Any) -> Result<RecordBatch>,
}

fn convert<R: Row>(rows: &dyn Any) -> Result<RecordBatch> {
    R::to_batch(rows.downcast_ref::<Vec<R>>().ok_or_else(|| anyhow!("buffer type mismatch"))?)
}

#[derive(Default)]
struct State {
    buffers: BTreeMap<(Table, NaiveDate), Buf>,
    segments: HashMap<(Table, NaiveDate), Seg>,
}

type Clock = Arc<dyn Fn() -> i64 + Send + Sync>;

#[derive(Clone)]
pub struct Sink {
    cfg: Arc<LakeConfig>,
    state: Arc<Mutex<State>>,
    clock: Clock,
}

impl Sink {
    pub fn open(cfg: LakeConfig) -> Result<Self> {
        Self::open_with_clock(cfg, Arc::new(now_us))
    }

    pub fn open_with_clock(cfg: LakeConfig, clock: Clock) -> Result<Self> {
        for table in Table::ALL {
            std::fs::create_dir_all(raw_dir(&cfg.root, table))?;
        }
        let rec = recover_all(&cfg.root)?;
        if rec.files + rec.quarantined > 0 {
            eprintln!("lake recovery: {} files closed, {} bytes dropped, {} quarantined", rec.files, rec.truncated_bytes, rec.quarantined);
        }
        Ok(Self { cfg: Arc::new(cfg), state: Default::default(), clock })
    }

    pub fn root(&self) -> &Path {
        &self.cfg.root
    }

    pub fn now_us(&self) -> i64 {
        (self.clock)()
    }

    pub fn today(&self) -> NaiveDate {
        date_of_us((self.clock)())
    }

    pub fn append<R: Row>(&self, row: R) -> Result<()> {
        self.append_on(self.today(), row)
    }

    pub fn append_many<R: Row>(&self, rows: impl IntoIterator<Item = R>) -> Result<()> {
        let date = self.today();
        rows.into_iter().try_for_each(|r| self.append_on(date, r))
    }

    pub fn append_on<R: Row>(&self, date: NaiveDate, row: R) -> Result<()> {
        let mut st = self.state.lock().unwrap();
        let key = (R::TABLE, date);
        let buf = st.buffers.entry(key).or_insert_with(|| Buf { rows: Box::new(Vec::<R>::new()), len: 0, since: Instant::now(), to_batch: convert::<R> });
        buf.rows.downcast_mut::<Vec<R>>().ok_or_else(|| anyhow!("buffer type mismatch"))?.push(row);
        buf.len += 1;
        if buf.len >= self.cfg.flush_rows {
            self.flush_key(&mut st, key)?;
        }
        Ok(())
    }

    fn flush_key(&self, st: &mut State, key: (Table, NaiveDate)) -> Result<()> {
        let Some(buf) = st.buffers.get(&key) else { return Ok(()) };
        let batch = (buf.to_batch)(buf.rows.as_ref())?;
        if let std::collections::hash_map::Entry::Vacant(slot) = st.segments.entry(key) {
            slot.insert(Seg::create(&self.cfg.root, key.0, key.1)?);
        }
        let written = st.segments.get_mut(&key).unwrap().write(&batch);
        if let Err(e) = written {
            st.segments.remove(&key);
            return Err(e);
        }
        st.buffers.remove(&key);
        Ok(())
    }

    fn flush_where(&self, due: impl Fn(&Buf) -> bool) -> Result<usize> {
        let mut st = self.state.lock().unwrap();
        let keys: Vec<_> = st.buffers.iter().filter(|(_, b)| due(b)).map(|(k, _)| *k).collect();
        let mut first_err = None;
        for key in &keys {
            if let Err(e) = self.flush_key(&mut st, *key) {
                first_err.get_or_insert(e);
            }
        }
        let today = self.today();
        let idle: Vec<_> = st.segments.keys().filter(|k| k.1 < today && !st.buffers.contains_key(k)).copied().collect();
        for key in idle {
            if let Some(mut seg) = st.segments.remove(&key) {
                let _ = seg.finish();
            }
        }
        first_err.map_or(Ok(keys.len()), Err)
    }

    pub fn tick(&self) -> Result<usize> {
        let age = Duration::from_secs(self.cfg.flush_secs);
        self.flush_where(|b| b.since.elapsed() >= age)
    }

    pub fn flush(&self) -> Result<usize> {
        self.flush_where(|_| true)
    }

    pub fn flush_table(&self, table: Table) -> Result<usize> {
        let mut st = self.state.lock().unwrap();
        let keys: Vec<_> = st.buffers.keys().filter(|k| k.0 == table).copied().collect();
        for key in &keys {
            self.flush_key(&mut st, *key)?;
        }
        Ok(keys.len())
    }

    pub fn close(&self) -> Result<()> {
        self.flush()?;
        let mut st = self.state.lock().unwrap();
        for (_, mut seg) in st.segments.drain() {
            seg.finish()?;
        }
        Ok(())
    }

    pub fn roll_day(&self, date: NaiveDate) -> Result<Vec<TableRoll>> {
        self.flush()?;
        Ok(roll_day(&self.cfg.root, date))
    }

    pub fn roll_pending(&self) -> Result<Vec<TableRoll>> {
        self.flush()?;
        let today = self.today();
        let mut out = Vec::new();
        for date in raw_dates(&self.cfg.root).into_iter().filter(|d| *d < today) {
            let rolls = roll_day(&self.cfg.root, date);
            for r in rolls.iter().filter(|r| matches!(r.status, RollStatus::Rolled | RollStatus::Failed(_))) {
                let (kind, detail) = match &r.status {
                    RollStatus::Failed(e) => (kind::ROLL_FAILED, serde_json::json!({"date": date.to_string(), "table": r.table.name(), "error": e})),
                    _ => (kind::ROLL_OK, serde_json::json!({"date": date.to_string(), "table": r.table.name(), "rows": r.rows, "partitions": r.partitions})),
                };
                self.append(Event::now(kind, "heatd").with_detail(detail))?;
            }
            out.extend(rolls);
        }
        Ok(out)
    }
}

#[derive(Debug, Clone, PartialEq)]
pub enum RollStatus {
    Rolled,
    Unchanged,
    Empty,
    Failed(String),
}

#[derive(Debug, Clone, PartialEq)]
pub struct TableRoll {
    pub table: Table,
    pub rows: u64,
    pub partitions: usize,
    pub status: RollStatus,
}

static ROLL_LOCK: std::sync::Mutex<()> = std::sync::Mutex::new(());

pub fn roll_day(root: &Path, date: NaiveDate) -> Vec<TableRoll> {
    let _guard = ROLL_LOCK.lock().unwrap_or_else(|e| e.into_inner());
    Table::ALL
        .into_iter()
        .map(|table| match roll_table(root, table, date) {
            Ok(r) => r,
            Err(e) => TableRoll { table, rows: 0, partitions: 0, status: RollStatus::Failed(format!("{e:#}")) },
        })
        .collect()
}

fn conform(batch: &RecordBatch, target: &SchemaRef) -> Result<RecordBatch> {
    if batch.schema().fields() == target.fields() {
        return Ok(RecordBatch::try_new(target.clone(), batch.columns().to_vec())?);
    }
    let src = batch.schema();
    for f in src.fields() {
        if target.field_with_name(f.name()).is_err() {
            bail!("raw column {} unknown to schema v{SCHEMA_VERSION}", f.name());
        }
    }
    let cols = target
        .fields()
        .iter()
        .map(|f| match src.index_of(f.name()) {
            Ok(i) if batch.column(i).data_type() == f.data_type() => Ok(batch.column(i).clone()),
            Ok(_) => bail!("column {} changed type", f.name()),
            Err(_) => Ok(new_null_array(f.data_type(), batch.num_rows())),
        })
        .collect::<Result<Vec<_>>>()?;
    Ok(RecordBatch::try_new(target.clone(), cols)?)
}

fn string_columns(fields: &Fields, prefix: &[String], out: &mut Vec<Vec<String>>) {
    for f in fields {
        let mut path = prefix.to_vec();
        path.push(f.name().clone());
        match f.data_type() {
            DataType::Utf8 => out.push(path),
            DataType::Dictionary(_, v) if **v == DataType::Utf8 => out.push(path),
            DataType::Struct(children) => string_columns(children, &path, out),
            _ => {}
        }
    }
}

pub fn write_parquet(path: &Path, schema: &SchemaRef, batches: &[RecordBatch]) -> Result<u64> {
    let mut paths = Vec::new();
    string_columns(schema.fields(), &[], &mut paths);
    let mut props = WriterProperties::builder()
        .set_compression(Compression::ZSTD(ZstdLevel::try_new(3)?))
        .set_max_row_group_row_count(Some(ROW_GROUP_ROWS))
        .set_dictionary_enabled(false)
        .set_key_value_metadata(Some(schema.metadata().iter().map(|(k, v)| KeyValue::new(k.clone(), v.clone())).collect()));
    for p in paths {
        props = props.set_column_dictionary_enabled(ColumnPath::new(p), true);
    }
    let file = File::create(path)?;
    let mut writer = ArrowWriter::try_new(file, schema.clone(), Some(props.build()))?;
    for b in batches {
        writer.write(b)?;
    }
    let file = writer.into_inner()?;
    file.sync_all()?;
    let rows = SerializedFileReader::new(File::open(path)?)?.metadata().file_metadata().num_rows();
    Ok(rows as u64)
}

fn node_values(batches: &[RecordBatch]) -> BTreeSet<Option<u64>> {
    let mut set = BTreeSet::new();
    for b in batches {
        let col = b.column_by_name("node").and_then(|c| c.as_any().downcast_ref::<PrimitiveArray<UInt64Type>>()).expect("node column");
        set.extend(col.iter());
    }
    set
}

fn filter_node(batch: &RecordBatch, node: Option<u64>) -> Result<RecordBatch> {
    let col = batch.column_by_name("node").and_then(|c| c.as_any().downcast_ref::<PrimitiveArray<UInt64Type>>()).expect("node column");
    let mask: BooleanArray = col.iter().map(|v| Some(v == node)).collect();
    Ok(filter_record_batch(batch, &mask)?)
}

fn write_atomic(path: &Path, bytes: &[u8]) -> Result<()> {
    let tmp = path.with_extension("tmp");
    let mut f = File::create(&tmp)?;
    f.write_all(bytes)?;
    f.sync_all()?;
    std::fs::rename(&tmp, path)?;
    if let Some(dir) = path.parent() {
        sync_dir(dir);
    }
    Ok(())
}

fn roll_table(root: &Path, table: Table, date: NaiveDate) -> Result<TableRoll> {
    let files = raw_files(root, table, Some(date))?;
    let none = |status| TableRoll { table, rows: 0, partitions: 0, status };
    if files.is_empty() {
        return Ok(none(RollStatus::Empty));
    }
    let signature: Vec<serde_json::Value> = files.iter().map(|(p, l)| serde_json::json!({"name": p.file_name().and_then(|n| n.to_str()), "bytes": l})).collect();
    let dir = parquet_dir(root, table, date);
    let marker = dir.join(".rolled");
    if let Ok(text) = std::fs::read_to_string(&marker) {
        let old: serde_json::Value = serde_json::from_str(&text).unwrap_or_default();
        if old["files"] == serde_json::Value::Array(signature.clone()) {
            return Ok(TableRoll { table, rows: old["rows"].as_u64().unwrap_or(0), partitions: old["partitions"].as_u64().unwrap_or(0) as usize, status: RollStatus::Unchanged });
        }
        std::fs::remove_file(&marker)?;
    }
    let target = table.schema();
    let mut batches = Vec::new();
    for (path, _) in &files {
        for b in load_stream(path)?.batches {
            batches.push(conform(&b, &target).with_context(|| path.display().to_string())?);
        }
    }
    let raw_rows: u64 = batches.iter().map(|b| b.num_rows() as u64).sum();
    std::fs::create_dir_all(&dir)?;
    let parts: Vec<(PathBuf, Vec<RecordBatch>)> = if table.node_partitioned() {
        let mut parts = Vec::new();
        for node in node_values(&batches) {
            let sub = batches.iter().map(|b| filter_node(b, node)).collect::<Result<Vec<_>>>()?;
            parts.push((dir.join(format!("node={}", node.map_or(NO_NODE.to_string(), |n| n.to_string()))), sub));
        }
        parts
    } else {
        vec![(dir.clone(), batches)]
    };
    let mut written = 0u64;
    let mut partitions = 0;
    for (pdir, sub) in parts {
        let expected: u64 = sub.iter().map(|b| b.num_rows() as u64).sum();
        if expected == 0 {
            continue;
        }
        std::fs::create_dir_all(&pdir)?;
        let tmp = pdir.join("part.parquet.tmp");
        let rows = write_parquet(&tmp, &target, &sub)?;
        if rows != expected {
            let _ = std::fs::remove_file(&tmp);
            bail!("{}: partition {} has {rows} parquet rows, expected {expected}", table.name(), pdir.display());
        }
        std::fs::rename(&tmp, pdir.join("part.parquet"))?;
        sync_dir(&pdir);
        written += rows;
        partitions += 1;
    }
    if written != raw_rows {
        bail!("{}: rolled {written} rows, raw has {raw_rows}", table.name());
    }
    let info = serde_json::json!({"rows": raw_rows, "partitions": partitions, "files": signature, "schema_version": SCHEMA_VERSION, "heatd_commit": COMMIT, "rolled_at": Utc::now().to_rfc3339()});
    write_atomic(&marker, info.to_string().as_bytes())?;
    Ok(TableRoll { table, rows: raw_rows, partitions, status: RollStatus::Rolled })
}

pub fn secs_until_roll(now: NaiveDateTime) -> u64 {
    let at = NaiveTime::from_hms_opt(ROLL_HOUR, 0, 0).unwrap();
    let mut next = now.date().and_time(at);
    if next <= now {
        next += chrono::Duration::days(1);
    }
    (next - now).num_seconds().max(1) as u64
}

pub fn spawn_background(sink: Sink) {
    let flusher = sink.clone();
    tokio::spawn(async move {
        let mut tick = tokio::time::interval(Duration::from_secs(1));
        loop {
            tick.tick().await;
            let s = flusher.clone();
            if let Ok(Err(e)) = tokio::task::spawn_blocking(move || s.tick()).await {
                eprintln!("lake flush failed: {e:#}");
            }
        }
    });
    tokio::spawn(async move {
        loop {
            let s = sink.clone();
            if let Ok(Err(e)) = tokio::task::spawn_blocking(move || s.roll_pending()).await {
                eprintln!("lake roll failed: {e:#}");
            }
            tokio::time::sleep(Duration::from_secs(secs_until_roll(chrono::Local::now().naive_local()))).await;
        }
    });
}
