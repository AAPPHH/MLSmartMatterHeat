use crate::config::Config;
use crate::lake::{self, kind, Event, Meta, Sink, Table, Weather};
use crate::status::Status;
use anyhow::{anyhow, bail, Context, Result};
use arrow::array::{Array, ArrayRef, StringArray, TimestampMicrosecondArray};
use arrow::compute::cast;
use arrow::datatypes::{DataType, TimeUnit};
use arrow::record_batch::RecordBatch;
use chrono::NaiveDate;
use serde_json::{json, Value};
use std::collections::HashMap;
use std::net::{ToSocketAddrs, UdpSocket};
use std::path::Path;
use std::sync::Arc;
use std::time::Duration;

pub const HOUR_US: i64 = 3_600_000_000;
pub const NTP_WARN_S: f64 = 1.0;
const NTP_UNIX_OFFSET_S: i64 = 2_208_988_800;
const HOURLY_VARS: &str = "temperature_2m,wind_speed_10m,shortwave_radiation,cloud_cover,precipitation";
const BACKFILL_DAYS: u32 = 2;
const FORECAST_DAYS: u32 = 2;
const WEATHER_RETRY: Duration = Duration::from_secs(600);

pub type Fetcher = Arc<dyn Fn(&str) -> Result<String> + Send + Sync>;
pub type OffsetFn = Arc<dyn Fn() -> Result<f64> + Send + Sync>;

#[derive(Clone)]
pub struct Io {
    pub fetch: Fetcher,
    pub ntp: OffsetFn,
}

impl Io {
    pub fn live(config: &Config) -> Self {
        let server = config.ntp_server.clone();
        Self { fetch: Arc::new(http_get), ntp: Arc::new(move || sntp_query(&server, Duration::from_secs(5))) }
    }
}

fn http_get(url: &str) -> Result<String> {
    let agent: ureq::Agent = ureq::Agent::config_builder().timeout_global(Some(Duration::from_secs(30))).build().into();
    Ok(agent.get(url).call().with_context(|| format!("GET {url}"))?.body_mut().read_to_string()?)
}

pub fn hour_floor(ts_us: i64) -> i64 {
    ts_us - ts_us.rem_euclid(HOUR_US)
}

#[derive(Clone, Debug, PartialEq)]
pub struct Hourly {
    pub ts: i64,
    pub temp_c: Option<f32>,
    pub wind_ms: Option<f32>,
    pub radiation_wm2: Option<f32>,
    pub cloud_pct: Option<f32>,
    pub precip_mm: Option<f32>,
}

impl Hourly {
    fn empty(&self) -> bool {
        [self.temp_c, self.wind_ms, self.radiation_wm2, self.cloud_pct, self.precip_mm].iter().all(Option::is_none)
    }
}

pub fn weather_url(base: &str, lat: f64, lon: f64) -> String {
    format!("{base}?latitude={lat}&longitude={lon}&hourly={HOURLY_VARS}&wind_speed_unit=ms&timezone=UTC&timeformat=unixtime&past_days={BACKFILL_DAYS}&forecast_days={FORECAST_DAYS}")
}

pub fn parse_hourly(text: &str) -> Result<Vec<Hourly>> {
    let doc: Value = serde_json::from_str(text).context("open-meteo json")?;
    if doc["error"].as_bool() == Some(true) {
        bail!("open-meteo: {}", doc["reason"].as_str().unwrap_or("error"));
    }
    let hourly = doc.get("hourly").ok_or_else(|| anyhow!("open-meteo: no hourly block"))?;
    let times = hourly["time"].as_array().ok_or_else(|| anyhow!("open-meteo: no hourly.time"))?;
    let column = |name: &str| -> Result<Vec<Option<f32>>> {
        let values = hourly[name].as_array().ok_or_else(|| anyhow!("open-meteo: no hourly.{name}"))?;
        if values.len() != times.len() {
            bail!("open-meteo: hourly.{name} has {} values for {} times", values.len(), times.len());
        }
        Ok(values.iter().map(|v| v.as_f64().map(|f| f as f32)).collect())
    };
    let (temp, wind, rad, cloud, precip) = (column("temperature_2m")?, column("wind_speed_10m")?, column("shortwave_radiation")?, column("cloud_cover")?, column("precipitation")?);
    times
        .iter()
        .enumerate()
        .map(|(i, t)| {
            let secs = t.as_i64().ok_or_else(|| anyhow!("open-meteo: bad time {t}"))?;
            Ok(Hourly { ts: secs * 1_000_000, temp_c: temp[i], wind_ms: wind[i], radiation_wm2: rad[i], cloud_pct: cloud[i], precip_mm: precip[i] })
        })
        .collect()
}

pub fn weather_rows(hours: &[Hourly], now_us: i64, last_obs_us: Option<i64>) -> Vec<Weather> {
    let now_h = hour_floor(now_us);
    hours
        .iter()
        .filter(|h| !h.empty())
        .filter_map(|h| {
            let (kind, issued) = if h.ts <= now_h {
                if last_obs_us.is_some_and(|l| h.ts <= l) {
                    return None;
                }
                ("obs", None)
            } else {
                ("fc", Some(now_h))
            };
            Some(Weather { ts: h.ts, kind: kind.into(), issued_ts: issued, fetched_ts: now_us, temp_c: h.temp_c, wind_ms: h.wind_ms, radiation_wm2: h.radiation_wm2, cloud_pct: h.cloud_pct, precip_mm: h.precip_mm })
        })
        .collect()
}

pub fn read_raw(root: &Path, table: Table, dates: Option<&[NaiveDate]>) -> Result<Vec<RecordBatch>> {
    let mut out = Vec::new();
    let sets: Vec<Option<NaiveDate>> = dates.map_or(vec![None], |d| d.iter().copied().map(Some).collect());
    for date in sets {
        for (path, _) in lake::raw_files(root, table, date)? {
            out.extend(lake::load_stream(&path)?.batches);
        }
    }
    Ok(out)
}

pub fn text_column(batch: &RecordBatch, name: &str) -> Result<StringArray> {
    let column: &ArrayRef = batch.column_by_name(name).ok_or_else(|| anyhow!("no column {name}"))?;
    let cast = cast(column, &DataType::Utf8)?;
    cast.as_any().downcast_ref::<StringArray>().cloned().ok_or_else(|| anyhow!("column {name} is not text"))
}

pub fn ts_column(batch: &RecordBatch, name: &str) -> Result<TimestampMicrosecondArray> {
    let column = batch.column_by_name(name).ok_or_else(|| anyhow!("no column {name}"))?;
    let cast = cast(column, &DataType::Timestamp(TimeUnit::Microsecond, Some("UTC".into())))?;
    cast.as_any().downcast_ref::<TimestampMicrosecondArray>().cloned().ok_or_else(|| anyhow!("column {name} is not a timestamp"))
}

pub fn last_obs_in_lake(root: &Path, today: NaiveDate) -> Result<Option<i64>> {
    let dates: Vec<NaiveDate> = (0..=BACKFILL_DAYS as i64 + 1).map(|d| today - chrono::Duration::days(d)).collect();
    let mut last = None;
    for batch in read_raw(root, Table::Weather, Some(&dates))? {
        let (kinds, ts) = (text_column(&batch, "kind")?, ts_column(&batch, "ts")?);
        for i in (0..batch.num_rows()).filter(|&i| kinds.value(i) == "obs" && !ts.is_null(i)) {
            last = last.max(Some(ts.value(i)));
        }
    }
    Ok(last)
}

pub struct WeatherJob {
    url: String,
    fetch: Fetcher,
    last_obs: Option<i64>,
    primed: bool,
}

impl WeatherJob {
    pub fn new(config: &Config, fetch: Fetcher) -> Option<Self> {
        let (lat, lon) = config.coordinates()?;
        Some(Self { url: weather_url(&config.weather_url, lat, lon), fetch, last_obs: None, primed: false })
    }

    pub fn run(&mut self, sink: &Sink, status: &Status, now_us: i64) -> Result<usize> {
        if !self.primed {
            self.last_obs = last_obs_in_lake(sink.root(), lake::date_of_us(now_us))?;
            self.primed = true;
        }
        let hours = parse_hourly(&(self.fetch)(&self.url)?)?;
        let rows = weather_rows(&hours, now_us, self.last_obs);
        let newest_obs = rows.iter().filter(|r| r.kind == "obs").map(|r| r.ts).max();
        if let Some(o) = rows.iter().filter(|r| r.kind == "obs" && r.temp_c.is_some()).max_by_key(|r| r.ts) {
            status.outdoor_obs(now_us, o.temp_c.unwrap_or_default() as f64, o.wind_ms.map(f64::from));
        }
        let count = rows.len();
        sink.append_many(rows)?;
        self.last_obs = self.last_obs.max(newest_obs);
        status.weather_fetched(now_us, count);
        Ok(count)
    }
}

pub fn meta_values(config: &Config) -> Vec<(&'static str, String)> {
    let rooms: Vec<Value> = config.rooms.iter().map(|r| json!({"name": r.name, "nodes": r.nodes})).collect();
    let mut pairs: Vec<[&str; 2]> = config.neighbours.iter().map(|n| if n.a <= n.b { [n.a.as_str(), n.b.as_str()] } else { [n.b.as_str(), n.a.as_str()] }).collect();
    pairs.sort();
    pairs.dedup();
    let heat = &config.heat_source;
    let mut out = vec![
        ("rooms", Value::Array(rooms)),
        ("adjacency", json!(pairs)),
        (
            "heat_source",
            json!({
                "type": heat.kind,
                "operating_hours": heat.operating_hours,
                "night_shutdown": heat.night_shutdown,
                "scope": heat.scope,
                "efficiency_class": heat.efficiency_class,
                "flow_temp_controllable": heat.flow_temp_controllable,
                "night_setback_controllable": heat.night_setback_controllable,
            }),
        ),
    ];
    let mut links: Vec<Value> = config.neighbours.iter().filter(|n| n.kind != "unknown").map(|n| json!({"a": n.a, "b": n.b, "kind": n.kind, "partial": n.partial})).collect();
    links.sort_by_key(|l| l.to_string());
    links.dedup();
    if !links.is_empty() {
        out.push(("links", Value::Array(links)));
    }
    let exterior: serde_json::Map<String, Value> = config.rooms.iter().filter(|r| !r.exterior.is_empty()).map(|r| (r.name.clone(), json!(r.exterior))).collect();
    if config.building.floor.is_some() || !config.building.roof_rooms.is_empty() || !exterior.is_empty() {
        out.push(("building", json!({"floor": config.building.floor, "roof_rooms": config.building.roof_rooms, "exterior": exterior})));
    }
    if let Some((lat, lon)) = config.coordinates() {
        out.push(("coordinates", json!({"lat": lat, "lon": lon})));
    }
    out.into_iter().map(|(k, v)| (k, v.to_string())).collect()
}

pub fn meta_in_lake(root: &Path) -> Result<HashMap<String, String>> {
    let mut latest: HashMap<String, (i64, String)> = HashMap::new();
    for batch in read_raw(root, Table::Meta, None)? {
        let (keys, values, ts) = (text_column(&batch, "key")?, text_column(&batch, "value")?, ts_column(&batch, "ts")?);
        for i in 0..batch.num_rows() {
            if latest.get(keys.value(i)).is_none_or(|(t, _)| ts.value(i) >= *t) {
                latest.insert(keys.value(i).to_string(), (ts.value(i), values.value(i).to_string()));
            }
        }
    }
    Ok(latest.into_iter().map(|(k, (_, v))| (k, v)).collect())
}

pub fn write_meta(sink: &Sink, config: &Config, now_us: i64) -> Result<usize> {
    let known = meta_in_lake(sink.root())?;
    let rows: Vec<Meta> = meta_values(config)
        .into_iter()
        .filter(|(k, v)| known.get(*k) != Some(v))
        .map(|(k, v)| Meta { ts: now_us, key: k.into(), value: v, valid_from: now_us })
        .collect();
    let count = rows.len();
    sink.append_many(rows)?;
    Ok(count)
}

fn ntp_us(bytes: &[u8]) -> i64 {
    let secs = u32::from_be_bytes(bytes[0..4].try_into().unwrap()) as i64;
    let frac = u32::from_be_bytes(bytes[4..8].try_into().unwrap()) as i64;
    (secs - NTP_UNIX_OFFSET_S) * 1_000_000 + ((frac * 1_000_000) >> 32)
}

pub fn sntp_request(t1_us: i64) -> [u8; 48] {
    let mut packet = [0u8; 48];
    packet[0] = 0x1b;
    let secs = (t1_us.div_euclid(1_000_000) + NTP_UNIX_OFFSET_S) as u32;
    let frac = ((t1_us.rem_euclid(1_000_000) << 32) / 1_000_000) as u32;
    packet[40..44].copy_from_slice(&secs.to_be_bytes());
    packet[44..48].copy_from_slice(&frac.to_be_bytes());
    packet
}

pub fn sntp_offset(t1_us: i64, response: &[u8], t4_us: i64) -> Result<f64> {
    if response.len() < 48 {
        bail!("ntp: short response ({} bytes)", response.len());
    }
    if response[0] & 7 != 4 || response[1] == 0 || response[0] >> 6 == 3 {
        bail!("ntp: unusable response (mode {}, stratum {}, leap {})", response[0] & 7, response[1], response[0] >> 6);
    }
    let (t2, t3) = (ntp_us(&response[32..40]), ntp_us(&response[40..48]));
    if response[40..48].iter().all(|b| *b == 0) {
        bail!("ntp: empty transmit timestamp");
    }
    Ok((((t2 - t1_us) + (t3 - t4_us)) as f64) / 2.0 / 1e6)
}

pub fn sntp_query(server: &str, timeout: Duration) -> Result<f64> {
    let addr = (server, 123).to_socket_addrs()?.next().ok_or_else(|| anyhow!("ntp: cannot resolve {server}"))?;
    let socket = UdpSocket::bind(if addr.is_ipv4() { "0.0.0.0:0" } else { "[::]:0" })?;
    socket.set_read_timeout(Some(timeout))?;
    socket.connect(addr)?;
    let t1 = lake::now_us();
    socket.send(&sntp_request(t1))?;
    let mut buf = [0u8; 128];
    let n = socket.recv(&mut buf)?;
    sntp_offset(t1, &buf[..n], lake::now_us())
}

pub fn check_ntp(sink: &Sink, status: &Status, offset: &OffsetFn, server: &str) -> Result<f64> {
    let seconds = offset()?;
    status.ntp_offset(seconds);
    if seconds.abs() >= NTP_WARN_S {
        eprintln!("warning: clock offset {seconds:+.3} s against {server}");
        sink.append(Event::now(kind::NTP_DRIFT, "heatd").with_detail(json!({"offset_s": seconds, "server": server})))?;
    }
    Ok(seconds)
}

pub fn spawn_background(sink: Sink, status: Status, config: Arc<Config>, io: Io) {
    {
        let (sink, config) = (sink.clone(), config.clone());
        tokio::spawn(async move {
            match tokio::task::spawn_blocking(move || write_meta(&sink, &config, lake::now_us())).await {
                Ok(Err(e)) => eprintln!("meta write failed: {e:#}"),
                Ok(Ok(n)) if n > 0 => println!("meta: {n} keys updated"),
                _ => {}
            }
        });
    }
    if !config.ntp_server.is_empty() {
        let (sink, status, ntp, server) = (sink.clone(), status.clone(), io.ntp.clone(), config.ntp_server.clone());
        tokio::spawn(async move {
            loop {
                let (s, st, n, srv) = (sink.clone(), status.clone(), ntp.clone(), server.clone());
                if let Ok(Err(e)) = tokio::task::spawn_blocking(move || check_ntp(&s, &st, &n, &srv)).await {
                    eprintln!("ntp check failed: {e:#}");
                }
                tokio::time::sleep(Duration::from_secs(3600)).await;
            }
        });
    }
    match WeatherJob::new(&config, io.fetch.clone()) {
        Some(job) => {
            let (sink, status) = (sink.clone(), status.clone());
            tokio::spawn(async move {
                let job = Arc::new(std::sync::Mutex::new(job));
                loop {
                    let (s, st, j) = (sink.clone(), status.clone(), job.clone());
                    let wait = match tokio::task::spawn_blocking(move || j.lock().unwrap().run(&s, &st, lake::now_us())).await {
                        Ok(Ok(_)) => Duration::from_secs((HOUR_US - lake::now_us().rem_euclid(HOUR_US)) as u64 / 1_000_000 + 300),
                        Ok(Err(e)) => {
                            eprintln!("weather fetch failed: {e:#}");
                            WEATHER_RETRY
                        }
                        Err(_) => WEATHER_RETRY,
                    };
                    tokio::time::sleep(wait).await;
                }
            });
        }
        None => println!("weather disabled: latitude/longitude not set"),
    }
    tokio::spawn(crate::report::daily_loop(sink, status));
}
