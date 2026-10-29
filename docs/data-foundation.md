# Data foundation (phase 1 storage contract)

Phase 2 reads only the Parquet tables described here (and DuckDB views over them). This file is the contract; the schema blocks below are machine-checked against the code (`crates/heatd/src/lake_tests.rs` parses this file and compares every block with the Arrow schema built in `crates/heatd/src/lake.rs`).

Notation in schema blocks: `name: type`, trailing `?` = nullable, `dict<utf8>` = Arrow `Dictionary(Int32, Utf8)`, `timestamp[us,UTC]` = `Timestamp(Microsecond, "UTC")`.

## Layout

```
/data/raw/<table>/<date>.arrows            hot path, Arrow IPC stream, one file per (table, UTC date, segment)
/data/raw/<table>/<date>.<n>.arrows        further segments (n = 1, 2, ...)
/data/raw/<table>/<file>.arrows.corrupt    quarantined file without a readable schema message (never deleted)
/data/raw/.migrated_duckdb                 marker written by `heatd migrate`
/data/parquet/<table>/date=<d>/part.parquet                tables without node column
/data/parquet/<table>/date=<d>/node=<n>/part.parquet       attribute_reports, events, decisions (node=none for events without node)
/data/parquet/<table>/date=<d>/.rolled                     roll marker (JSON)
/data/heatd.duckdb                         operational state only (see below)
```

`data_dir` in heatd.toml (default `data`, Docker `/data`) is the root. Phase 2 reads `parquet/` only, never `raw/`.

`<d>` and `<date>` are the UTC date on which heatd wrote (ingested) the row, not the date of the row's `ts`. A watch batch uploaded two days late therefore lands in today's partition, and a closed day never receives new rows. Queries by `ts` must pad the date range (suggested: date-1d .. date+1d; weather forecast rows can be dated ahead of their partition). Partition columns exist only in the path: read with `hive_partitioning=true`, e.g. `read_parquet('/data/parquet/events/*/*/part.parquet', hive_partitioning=true, union_by_name=true)` (for tables without node use `*/part.parquet`). Exception: `heatd migrate` partitions legacy rows by the UTC date of their `ts`.

## Schema metadata and versioning

Every Arrow schema (IPC stream, Parquet footer key/value) carries `schema_version` (integer as string, currently `1`), `heatd_commit` (git short hash from the build; env `HEATD_COMMIT` overrides, e.g. Docker build arg; fallback `unknown`) and `table`. Parquet files carry the metadata of the heatd build that rolled them; a raw segment carries that of the build that wrote it.

Additive migration rules: columns are only ever appended at the end of the schema, as nullable; nothing is renamed, retyped, reordered or removed; every addition bumps `schema_version`. The struct `decisions.inputs` is frozen in v1 (new inputs go into `inputs.extra` as JSON, or become a new top-level nullable column). New dictionary values (event kinds, rules, sources) are not schema changes. The roller conforms older raw segments to the current schema (missing columns become null) and fails the table (`roll_failed`) if a raw segment has a column the running build does not know or a changed type; it never drops data. Schema changes requested by phase 2 go through `docs/schema-requests.md`.

## Hot path (raw Arrow IPC streams)

- Writer: `lake::Sink` (crates/heatd/src/lake.rs). One in-memory buffer per (table, date); a buffer is flushed as one record batch when it reaches 500 rows (synchronously inside `append`) or when its oldest row is 30 s old (`Sink::tick`, driven every second by `lake::spawn_background`), and on `close()` (graceful shutdown). No preprocessing, no aggregation, no retention.
- Durability: after every batch the segment is flushed and fsynced (`sync_all`); creating a segment also fsyncs the directory. A crash loses at most the rows still in memory (under 30 s / 500 rows per table).
- Dictionary columns use the IPC "resend" policy: every batch carries its full dictionary, so any prefix of batches is self-contained.
- Segments: the Arrow IPC stream format cannot be appended to after its schema message without writing a second schema, so heatd never reopens a file for writing. Each process run creates a new segment on the first write per (table, date): `<date>.arrows`, then `<date>.1.arrows`, `<date>.2.arrows`, ... (first free name). A segment closed cleanly (or by recovery) ends with the 8-byte IPC end-of-stream marker; the segment of a running or crashed process has none and is still readable by any stream reader up to the last complete batch. After a write error the failed segment is abandoned and the next flush opens a new one (buffered rows are kept and retried).
- Recovery (every start, before any write): every raw file not ending in a valid end-of-stream marker is scanned message by message; it is truncated to the end of the last record batch that is completely present and decodes, and the end-of-stream marker is appended. A file without a readable schema message (e.g. crash right after creation) is renamed to `*.arrows.corrupt`. Complete files are only read for the check. Recovery prints one line to stderr only if it changed something.
- Time columns are UTC microseconds. `mono_ns` is a monotonic clock (ns since heatd process start) to order rows within a process run independent of NTP steps.

## Roll job (hot to cold)

- `lake::roll_day(root, date)` / `Sink::roll_day(date)`: per table, read all raw segments of the date (valid prefix only), conform them to the current schema, split by `node` for node-partitioned tables (null node = `node=none`), write each partition to `part.parquet.tmp` with Zstd (level 3), dictionary encoding only on string columns (utf8 and dict<utf8>, including `inputs.*` children), row groups of 65 536 rows. Verify: the Parquet footer row count of each partition equals the rows sent to it and the sum over partitions equals the raw row count. Only then the temp file is renamed to `part.parquet`; after all partitions the `.rolled` marker (JSON: rows, partitions, raw file names with sizes, schema_version, heatd_commit, rolled_at) is written atomically as the last step. Any mismatch or error leaves no marker and yields `roll_failed`.
- Raw files are never deleted or modified by the roll.
- Idempotent: a day whose marker lists exactly the current raw files (names and sizes) is skipped (status Unchanged). If the raw file list changed (late segment, e.g. from `heatd migrate`, or recovery appended an end marker), the marker is removed and the day is rolled again completely.
- Schedule: `lake::spawn_background` runs `Sink::roll_pending` once at start (catch-up of every UTC date before today that has raw files and no matching marker) and then daily at 03:00 local time of the host (`secs_until_roll`). Today's (UTC) data is never rolled. Each rolled table appends an `events` row `roll_ok` (detail: date, table, rows, partitions); a failing table appends `roll_failed` (detail: date, table, error) and is retried at the next run.
- Memory: one day of one table is held in memory while rolling.

## Operational state (not data of record)

Kept minimal and local: `/data/heatd.duckdb` (existing `storage.rs`) stays as the small state store for watch ingest idempotency (`ingest_ids`, `Storage::claim_id(device, id) -> bool`). Last known values for the rules are kept in memory (rebuilt from the next reports after a restart; at most one 60 s poll interval of staleness). The legacy DuckDB tables `scans`, `readings`, `setpoints`, `predictions`, `device_state` are frozen: new code must not write them; `heatd migrate` exports them once. Nothing in this state is read by phase 2.

## `heatd migrate`

One-shot, run with heatd stopped (needs the DuckDB file). Reads the legacy tables read-only and writes the new raw segments (partition date = UTC date of the legacy `ts`), then writes `raw/.migrated_duckdb` and refuses to run again. The next serve start rolls the migrated days.

| legacy | new | mapping |
|---|---|---|
| scans (one row per AP) | watch_raw | ts, received_ts, device, sender=bssid, rssi, freq, channel derived from freq, batch_id=scan_id |
| scans.label (distinct per scan) | labels | source=watch, room=label, device |
| predictions | labels | source=model, confidence=probability, model_id=legacy |
| readings | attribute_reports | source=legacy, fw=legacy, dtype=float, v_f64=value, v_str=legacy attribute name, cluster/attribute from the known specs else 0/4294967295, mono_ns=0, value_raw empty |
| setpoints | decisions | rule=legacy, applied=true, proposed=true, setpoint_new=value, commit=legacy, inputs.extra={"reason":...} |
| device_state | events | kind=watch_state, source=legacy, detail JSON (device, received_ts, motion, power, queued, n_aps, away, id) |

## Tables

### attribute_reports

Producer: heatd Matter code (matter-controller reports). One row per attribute report of the wildcard subscription (source `subscription`), per 60 s core-attribute poll (`poll`), per explicit read (`read`), or migrated history (`legacy`). Partitioned by date and node.

```schema attribute_reports
ts: timestamp[us,UTC]
mono_ns: i64
node: u64
endpoint: u16
cluster: u32
attribute: u32
dtype: dict<utf8>
v_i64: i64?
v_f64: f64?
v_bool: bool?
v_str: utf8?
v_bin: binary?
value_raw: binary
source: dict<utf8>
fw: dict<utf8>
```

- `dtype` values: `int`, `uint`, `float`, `bool`, `string`, `bytes`, `null`, `struct`, `list`, `unknown`. The decoded value goes into exactly one of `v_i64` (int; uint if it fits), `v_f64`, `v_bool`, `v_str` (string; decimal text for uint above i64::MAX; JSON text for struct/list), `v_bin` (bytes); all null for `null`/`unknown`.
- `value_raw`: see "Abweichungen" below: for live Matter capture this is the exact text chip-tool printed for the value (UTF-8), not TLV; empty for legacy rows. Vendor-specific clusters/attributes (Eurotronic) are stored the same way without interpretation; anything undecodable has `dtype=unknown` and `value_raw`.
- `fw`: firmware/software version string of the node at report time (`unknown` if not known yet, `legacy` for migrated rows).

### events

Producers: matter-controller (device events, subscription lifecycle), rule core and exploration, presence logic, QA jobs, lake roll. Matter events (`kind=matter_event`, and device `startup`/`shutdown`) use the same value columns as attribute_reports plus `event_id`, `event_number`, `priority`, `device_ts`. Everything that is not a Matter event uses only `ts`, `mono_ns`, `kind`, `source` and the generic `detail` column (JSON object as text), plus `node` / `experiment_id` where applicable; therefore all Matter columns are nullable here (additive to the Matter event fields). `attribute` is kept for column compatibility with attribute_reports and is null for Matter events (they carry `event_id`). Partitioned by date and node (`node=none` for heatd-level events).

```schema events
ts: timestamp[us,UTC]
mono_ns: i64
kind: dict<utf8>
node: u64?
endpoint: u16?
cluster: u32?
attribute: u32?
event_id: u32?
event_number: u64?
priority: u8?
device_ts: timestamp[us,UTC]?
dtype: dict<utf8>?
v_i64: i64?
v_f64: f64?
v_bool: bool?
v_str: utf8?
v_bin: binary?
value_raw: binary?
source: dict<utf8>
fw: dict<utf8>?
experiment_id: utf8?
detail: utf8?
```

Explicit list of `kind` values (dictionary column; adding a kind is not a schema change but must be added to `lake::kind` and to this list):

| kind | meaning | source | node | detail JSON |
|---|---|---|---|---|
| matter_event | Matter event report from a device | subscription | yes | name, timestamp (device-printed event timestamp text) |
| subscription_lost | wildcard subscription of a node dropped (chip-tool log marker or interactive session ended) | heatd | yes | reason, silent_s, backoff_s (session restart delay, if the whole session ended) |
| resubscribed | subscription re-established (log marker, or first report after the loss) | heatd | yes | attempt, downtime_s, via (marker or report) |
| startup | heatd start (source heatd, node null) or device StartUp event (source subscription, node set) | heatd / subscription | no / yes | heatd: commit, schema_version |
| shutdown | heatd graceful stop, or device ShutDown event | heatd / subscription | no / yes | |
| fw_change | BasicInformation SoftwareVersion / SoftwareVersionString changed (triggers an automatic inventory re-run) | heatd | yes | old, new, old_version, new_version |
| experiment_start | exploration experiment begins | heatd | yes | type, setpoint, duration_min, room |
| experiment_end | experiment finished normally | heatd | yes | room |
| experiment_abort | experiment aborted (manual override, window open, frost, error) | heatd | yes | reason, room |
| ntp_drift | clock offset warning (>= 1 s), at start and hourly | heatd | no | offset_s, server |
| presence_arrive | user came home | heatd | no | source (watch_wifi, manual, ...), device |
| presence_leave | user left | heatd | no | source, device |
| roll_ok | a table/day was rolled to Parquet | heatd | no | date, table, rows, partitions |
| roll_failed | a table/day failed to roll | heatd | no | date, table, error |
| poll_failed | 60 s core-attribute poll got no response (backoff applies) | heatd | yes | error, consecutive, next_poll_s |
| watch_state | watch heartbeat state (migrated legacy device_state; new code may keep emitting it) | watch / legacy | no | device, motion, power, queued, n_aps, away, id |

`experiment_id` links experiment_* events to `decisions.experiment_id`. `source` values: `subscription`, `poll`, `read`, `heatd`, `watch`, `legacy`.

### decisions

Producer: rule core, one row per decision before the write to the device, including decisions that are proposed but not applied. Partitioned by date and node.

```schema decisions
ts: timestamp[us,UTC]
room: dict<utf8>
node: u64
rule: dict<utf8>
inputs: struct<local_temp: f64?, outdoor_temp: f64?, schedule_setpoint: f64?, frost_setpoint: f64?, demand_pct: f64?, battery_pct: f64?, prediction_age_min: f64?, contact_age_min: f64?, occupancy: utf8?, room_estimate: utf8?, window_open: bool?, away: bool?, system_mode: utf8?, extra: utf8?>
setpoint_old: f64?
setpoint_new: f64?
mode: dict<utf8>?
exploration: bool
experiment_id: utf8?
proposed: bool
applied: bool
policy_id: utf8?
commit: dict<utf8>
```

- `rule`: `schedule`, `presence`, `window`, `frost`, `manual`, `exploration`, `policy`, `legacy` (migrated). The away rule (all rooms 18 C when the watch is absent) is `rule=presence` with `inputs.occupancy=away`, `inputs.away=true`.
- `inputs`: every value the rule saw (null if not available): temperatures in C, `demand_pct`, `battery_pct`, ages in minutes, `occupancy` (`room`/`away`/`unknown`), `room_estimate`, flags, `system_mode`, `extra` JSON for anything else.
- `setpoint_old` / `setpoint_new` in C, `mode` = target SystemMode text, `proposed` = a rule/policy suggested it, `applied` = it was written to the device, `commit` = heatd commit (`lake::COMMIT`).
- Manual interventions (setpoint changed at the device, detected as a report without own decision) are `rule=manual` here and additionally a `labels` row with `source=override`.

### labels

Producers: /watch/label and alias /api/label (source `watch`), manual entry (`manual`), rule core for overrides (`override`), room model (`model`). Partitioned by date.

```schema labels
ts: timestamp[us,UTC]
from_ts: timestamp[us,UTC]?
to_ts: timestamp[us,UTC]?
room: dict<utf8>
source: dict<utf8>
device: dict<utf8>?
confidence: f32?
model_id: utf8?
probs: utf8?
```

- `ts`: time the label was recorded; `from_ts`/`to_ts`: validity window of window labels from /api/label (null for point labels). `confidence` and `probs` (JSON object room to probability) are filled for `model`; `model_id` identifies the model. `device`: watch/device the label refers to, if any.

### watch_raw

Producer: /watch/measurement and aliases /api/scan, /api/scans. One row per access point of a Wi-Fi scan. Partitioned by date.

```schema watch_raw
ts: timestamp[us,UTC]
device_ts: timestamp[us,UTC]?
sender: dict<utf8>
rssi: i8
channel: u8?
batch_id: u64
device: dict<utf8>
freq: u16?
received_ts: timestamp[us,UTC]?
```

- `ts`: measurement time as sent by the watch, `device_ts`: raw watch clock if reported separately, `received_ts`: heatd receive time. `sender` = BSSID (lowercase), `batch_id` = grouping id of one scan/upload batch, `device` = watch id, `freq` MHz, `channel` sent or derived from freq.

### weather

Producer: Open-Meteo job, hourly. Partitioned by date.

```schema weather
ts: timestamp[us,UTC]
kind: dict<utf8>
issued_ts: timestamp[us,UTC]?
fetched_ts: timestamp[us,UTC]
temp_c: f32?
wind_ms: f32?
radiation_wm2: f32?
cloud_pct: f32?
precip_mm: f32?
```

- `kind`: `obs` (observation, `issued_ts` null) or `fc` (forecast, `issued_ts` = model run time). `ts` = hour the values refer to, `fetched_ts` = fetch time. Units: C, m/s, W/m2, %, mm. After an outage the last 48 h are backfilled as `obs` rows.

### meta

Producer: config loader at start and on change. Versioned key/value pairs; the latest row per key with `valid_from <= t` is valid at time t. Keys: `rooms` (JSON list), `adjacency` (JSON list of room pairs), `heat_source` (JSON: type, operating hours/night shutdown if known), `coordinates` (JSON lat/lon). A new row is only written when the value changed. Partitioned by date.

```schema meta
ts: timestamp[us,UTC]
key: dict<utf8>
value: utf8
valid_from: timestamp[us,UTC]
```

## Abweichungen

Deviations from `docs/phase1-spec.md` section 1 (Matter capture), all caused by chip-tool v1.5.1.0 being the only Matter client:

- **No raw TLV.** chip-tool prints decoded values only. The encoded report payload is not reachable from its stdout (interactive mode with `--trace_decode` / `--trace-to json:` would write the decoded message tree, and the raw bytes only appear in secure-channel debug logs before decryption, which are neither stable nor attributable to a single attribute). Therefore `attribute_reports.value_raw` and `events.value_raw` hold the exact printed value text as UTF-8 bytes: the lines chip-tool printed after the `Endpoint: ... Cluster: ... Attribute|Event ...` header, with the log prefix removed and indentation and newlines kept (example: `  BatPercentRemaining: 190`, structs and lists multi-line). `dtype` keeps the decoded type (`int`, `uint`, `float`, `bool`, `string`, `list`, `struct`, `null`); it does not mark the raw column as text, because every live row has text there. Phase 2 must not parse `value_raw` as TLV; legacy rows have it empty.
- **`uint` is rare.** chip-tool prints no signedness, so every integer that fits `i64` is `int`; `uint` is used only above `i64::MAX` (decimal text in `v_str`). Octet strings are not distinguished from strings (no `bytes` rows from live capture; `v_bin` stays null).
- **Unknown and vendor data.** Vendor clusters or attributes (ids above `0xFFFF`, i.e. with vendor prefix, or in `0xFC00..0xFFF7`) and anything chip-tool prints without a name (`TLV`, `Unknown`) are stored with `dtype=unknown`, all `v_*` null and the printed text in `value_raw`, uninterpreted.
- **Event device timestamp.** `device_ts` is only set when chip-tool prints an epoch timestamp (`Epoch ...` in ms). Devices usually report system time (ms since boot), which cannot be mapped to UTC; that text is kept in `detail.timestamp` and `device_ts` is null.
- **Subscription lifecycle** is derived from chip-tool log lines (`Subscription Liveness timeout`, `Will try to resubscribe`, `SubscribeResponse is received`; matched case-insensitively in `matter-controller/src/capture.rs`) and from the interactive session ending (exponential backoff 1 s to 60 s). These strings are not yet verified against a live chip-tool 1.5.1.0 log; as a safety net heatd treats the first report after a loss as the resubscribe (`via=report`). Wildcard attribute and event subscriptions are two chip-tool subscriptions per node, so a loss may be logged once per subscription state change, not per subscription.
- **Poll** runs through the interactive chip-tool session (one `any read-by-id` line with the five core paths, reusing the established CASE session) every 60 s per node; responses are tagged `source=poll`. After a failed poll the next one is delayed 120 s, 240 s, ... up to 960 s (`poll_failed` events); it resets after a success. No poll runs while an automatic inventory is in progress.
- **Automatic inventory** runs a separate one-shot chip-tool process next to the interactive session (the CLI `heatd inventory` needs the storage lock and heatd stopped; the automatic run does not). Output: `inventory/<node>_<fw>.json`, `.md` and raw logs under `inventory/raw/` in `inventory_dir` (default `inventory`), `fw` = SoftwareVersionString, else SoftwareVersion.

## Public API

`crates/heatd/src/lake.rs`: `Table`, row structs (`AttributeReport`, `Event`, `Decision` with `DecisionInputs`, `Label`, `WatchRaw`, `Weather`, `Meta`), trait `Row`, `LakeConfig`, `Sink` (`open`, `append`, `append_many`, `append_on`, `tick`, `flush`, `close`, `roll_day`, `roll_pending`), `kind::*`, `spawn_background`. `AppState.sink` carries the shared `Sink`.
