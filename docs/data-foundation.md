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

Every Arrow schema (IPC stream, Parquet footer key/value) carries `schema_version` (integer as string, currently `2`), `heatd_commit` (git short hash from the build; env `HEATD_COMMIT` overrides, e.g. Docker build arg; fallback `unknown`) and `table`. Parquet files carry the metadata of the heatd build that rolled them; a raw segment carries that of the build that wrote it.

Additive migration rules: columns are only ever appended at the end of the schema, as nullable; nothing is renamed, retyped, reordered or removed; every addition bumps `schema_version` (v2: `watch_raw` columns `elapsed_ns` .. `unit`, new tables `watch_sensors` and `watch_inference`). The struct `decisions.inputs` is frozen in v1 (new inputs go into `inputs.extra` as JSON, or become a new top-level nullable column). New dictionary values (event kinds, rules, sources) are not schema changes. The roller conforms older raw segments to the current schema (missing columns become null) and fails the table (`roll_failed`) if a raw segment has a column the running build does not know or a changed type; it never drops data. Schema changes requested by phase 2 go through `docs/schema-requests.md`.

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

Kept minimal and local: `/data/heatd.duckdb` (existing `storage.rs`) stays as the small state store for watch ingest idempotency (`ingest_ids`, `Storage::claim_id(device, id) -> bool`). Last known values for the rules are kept in memory (rebuilt from the next reports after a restart; at most one 60 s poll interval of staleness). The legacy DuckDB tables `scans`, `readings`, `setpoints`, `predictions`, `device_state` are legacy: phase 2 must not read them, but the current rules still read presence from them and `storage.rs`, `rules.rs` and `relabel` still write them next to the lake (dual write, until the rules read the lake). `heatd migrate` exports them once and only up to the first heatd `startup` event, so that dual-written rows are not duplicated. The lake additionally gets `watch_state` events (one per `/api/scan`, with device, received_ts, motion, power, queued, n_aps, away, id) and in-scan room labels (`labels`, source `watch`).

## `heatd migrate`

One-shot, run with heatd stopped (needs the DuckDB file). Run it before the first start of the build that writes the lake. It first reads every legacy table completely (nullable columns as options), takes the cutoff (`ts` of the first `startup` event with source `heatd` in `raw/events`, else the oldest `watch_raw` row, else no cutoff) and only then appends the rows older than the cutoff. It reads the legacy tables read-only and writes the new raw segments (partition date = UTC date of the legacy `ts`), then writes `raw/.migrated_duckdb` and refuses to run again. The next serve start rolls the migrated days.

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
- `value_raw`: see "Abweichungen" below: for live Matter capture this is the exact text chip-tool printed for the value (UTF-8), not TLV; empty for legacy rows. Vendor-specific clusters/attributes (Eurotronic) are stored the same way without interpretation; anything undecodable has `dtype=unknown` and `value_raw`. In the inventory, chip-tool prints "Don't know how to log ..." for values it cannot decode; those records carry the placeholder `<not decoded by chip-tool, see raw log>` and the value only exists in the `[DMG]` dump of the raw log.
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
| matcher_drift | 7-day abstain rate of a watch matcher rose by more than 50 % against the previous 7 days (checked daily with the report) | heatd | no | matcher, current, previous, rows_current, rows_previous |
| occupancy_end | a room stopped counting as occupied for heating (hold elapsed or away); ts = end | heatd | no | room, from_ms (start, epoch ms), minutes |
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

- Append-only write protocol: each setpoint write produces two rows with the same `ts`, `room`, `node`, `rule`, `inputs` and `experiment_id`. The proposal row (`proposed=true, applied=false`) is persisted before the device write; after the attempt a result row follows (`applied=true` on success, `applied=false` on failure). `inputs.extra` is a JSON object: `phase` = `proposal` | `result` | `observed` (manual), `reason` = rule detail (`schedule`, `default`, `occupied`, `sleep`, `away`, `window_open`, `exploration`), proposal rows add `source` (`matcher`, `stay_label`) and `sleep` (bool), result rows add `ref_ts` (ts of the proposal) and `error` on failure. Consumers take the latest row per (`ts`, `node`). Unchanged setpoints (hysteresis, < 0.5 C) write no row. A sink error is logged and never aborts the tick or the other rooms; the device write still happens.
- `rule=manual` rows: `proposed=false, applied=true`, `setpoint_old` = last commanded value, `setpoint_new` = value reported by the device, `experiment_id` = the experiment it aborted (if any).
- `rule=exploration`: `exploration=true` and `experiment_id` set on all decisions of a running experiment; `rule=window` uses the frost setpoint.
- Setpoint precedence (final, evaluated per room every 60 s): (1) manual hold of a device change (`manual_hold_minutes`) blocks all writes to that node for exactly `manual_hold_minutes` (default 60; a schedule or presence change does not end it, window open and away do), then the system value is written immediately; (2) window open: `frost_setpoint`; (3) away (confirmed): `away_setpoint` everywhere; (4) running exploration experiment; (5) occupied room: exactly `occupied_setpoint` (21.0, replaces `presence_boost`, which is still accepted in the config but ignored), except the room named by `sleep_room` while the watch is asleep, which falls through to (6); (6) room schedule; (7) `default_setpoint`. Rooms without nodes get no action. Occupied room: the matcher room if it is not abstained and within `t_hold_min` (and `presence_timeout_minutes`), else the watch's confirmed stay label (scan `label` or `/api/label`, kept in memory per device, cleared by a later unlabeled scan with APs) if younger than `presence_timeout_minutes` and the device did not report `moving` after the label, else none. Sleep: classified watch state `sleep` (`watch_minute` rule) or `bedtime: true` in the latest `device_state` row (optional `bedtime` boolean on scan and heartbeat bodies and long-format `/watch/measurement` batches, stored in `device_state.bedtime` and the `watch_state` event detail). Decision rows: `rule=presence`, `reason` `occupied` or `sleep`, `inputs.extra.source` and `inputs.extra.sleep`.
- Away: the watch must report away continuously for `away_guard_minutes` (default 20) and must have been carried (a `moving` report since away began), so a watch lying at home or charging never triggers it. Contact and power checks (`still_contact_timeout_minutes`, saver/offbody) still apply. Confirmed leave is logged as `presence_leave` (ts = start of the absence), return as `presence_arrive`, detail `{source: watch_wifi, device}`. Away target is `away_setpoint` (default 18.0) for all rooms, except window-open (frost).
- Filter gegen staendiges An/Aus (Praezedenz unveraendert, die Filter sitzen zwischen Raumerkennung und Schreiben; Defaults in `heatd.docker.toml`):
  1. Belegung mit Einschaltverzoegerung: Ein Raum zaehlt fuer die Heizung erst als belegt (Punkt 5 der Praezedenz), wenn er `occupancy_enter_min` (10) Minuten ununterbrochen der erkannte Raum war. Ausnahme: ein expliziter Tipp (Aufenthaltslabel aus `/api/label`, `/watch/label` oder mitgefuehrtes Label, `source=stay_label`) macht den Raum sofort belegt. Noch nicht bestaetigte Raeume werden als Entscheidung `proposed=true, applied=false` mit `inputs.extra.reason=debounce` (und `wanted=occupied`) protokolliert, einmal je Aufenthalt.
  2. Nachlauf: Verlaesst die Person den Raum (anderer Raum, unbekannt, Aufenthalt beendet), behaelt der Raum `occupancy_exit_min` (45) Minuten lang `occupied_setpoint`; mehrere Raeume koennen gleichzeitig nachlaufen. Abwesend (`away`) beendet jeden Nachlauf sofort (`away_setpoint`), ebenso Schlaf im `sleep_room` (Plan-/Nachtwert sofort). Das Ende eines belegten Intervalls wird als Event `occupancy_end` (room, from_ms, minutes) protokolliert.
  3. Schreibbegrenzung je Node: hoechstens eine Sollwertaenderung alle `min_write_interval_min` (15) Minuten, gemessen ab dem letzten erfolgreichen Schreiben. Ausnahmen (sofort): Fenster offen/Frost, Wechsel von oder nach Abwesenheit, Exploration, Ende eines manuellen Holds, expliziter Tipp. Deadband: kein Schreiben, wenn `|neu - Geraetewert| < setpoint_deadband` (0,3 C; zusaetzlich bleibt die Hysterese von 0,5 C gegen den letzten Sollwert). Aufgeschobene Vorschlaege stehen als Entscheidung mit `proposed=true, applied=false` und `inputs.extra.reason=rate_limited` (`wanted` = eigentlicher Grund) im Lake, nur wenn sich der aufgeschobene Zielwert aendert, nicht bei jedem Tick.
  4. Tagesbericht: Abschnitt "Heizfilter" je Raum mit Anzahl Sollwert-Schreibvorgaengen, `rate_limited`- und `debounce`-Vorschlaegen sowie belegten Minuten (Summe der `occupancy_end`-Events des Tages).

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
elapsed_ns: i64?
clock_offset_ms: i32?
sensor_type: dict<utf8>?
sensor_name: dict<utf8>?
value: f32?
unit: dict<utf8>?
```

- `ts`: measurement time as sent by the watch, `device_ts`: raw watch clock if reported separately, `received_ts`: heatd receive time. `sender` = BSSID (lowercase), `batch_id` = grouping id of one scan/upload batch, `device` = watch id, `freq` MHz, `channel` sent or derived from freq.

- Phase 1b (schema v2, long format): one row per sample for every sensor. `sensor_type` is `rssi` (Wi-Fi access point, `value` = rssi), `temp` (skin temperature, degC), `hr` (bpm), `offbody` (Android raw value of the low-latency off-body sensor: 1 = on the wrist, 0 = off the wrist), `motion` (variance of the acceleration magnitude over a 10 s window, m2/s4), `battery` (percent), `charging` (1 = charging, 0 = not). `elapsed_ns` = `SystemClock.elapsedRealtimeNanos` of the sample on the watch, `clock_offset_ms` = wall clock minus device clock of the batch, `sensor_name` = Android sensor name, `unit` free text. Rows written before v2 and rows of `/api/scan` without the new fields have `sensor_type` null or `rssi`; treat null as `rssi`. For `sensor_type` other than `rssi` the required legacy columns carry placeholders: `sender` = empty string, `rssi` = 0, `channel`/`freq` null; filter by `sensor_type` before using `rssi`. All rssi rows of one scan carry the same `ts`.

### watch_sensors

Producer: `POST /watch/sensors`, the sensor inventory of the watch (`SensorManager.getSensorList`), written at app start and whenever it changes. Partitioned by date.

```schema watch_sensors
ts: timestamp[us,UTC]
sensor_name: dict<utf8>
android_type: i32
vendor: dict<utf8>?
resolution: f32?
max_range: f32?
app_version: dict<utf8>?
```

### watch_inference

Producer: heatd `watchmatch` module. One row per fresh Wi-Fi scan (matcher `room`) and one row `reason=no_scan` per minute gap. Strictly separate from `labels`. Partitioned by date. `matcher` is `room` (written now) or `temp` (reserved for the later temperature matcher).

```schema watch_inference
ts: timestamp[us,UTC]
matcher: dict<utf8>
template_id: utf8
result_room: dict<utf8>?
result_temp: f32?
confidence: f32
abstained: bool
reason: dict<utf8>?
distance: f32?
state: dict<utf8>
features_hash: utf8
model_version: dict<utf8>
```

- `ts`: scan time. `template_id`: state key of the model file used (`all`, `sleep`, ...), `none` without model. `result_room`: set only when not abstained. `confidence`: max probability after the HMM forward step (0 when no model). `reason` (abstained rows): `low_confidence`, `low_margin`, `no_model`, `no_scan`, `no_known_ap` (no sender of the model visible), `unknown_state` (`offbody`/`charging` never have templates). `distance`: reserved (null). `state`: `rest`, `sleep`, `active`, `offbody`, `charging`. `features_hash`: 16 hex characters, FNV-1a 64 over the sorted `sender,rssi` pairs of the scan (empty for `no_scan`). `model_version`: `model_version` of `model.json`, `none` without model.

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
- **Subscription interval** (battery on the sleepy Thread thermostat): `subscribe_min_s` = 30 s, `subscribe_max_s` = 300 s, two subscriptions per node (attribute wildcard, event wildcard). The 1 s minimum from the first draft is not used; no narrow 1 s subscription is added either, since a third subscription per node is more costly than the up to 30 s latency in noticing a manual change.
- **Poll** deviates from the 60 s spec: heatd checks every 60 s per node but only polls when no report (any source) arrived from that node in the last `poll_silence_s` = 300 s, so a subscription that delivers reports causes no extra device round trips. It runs through the interactive chip-tool session (one `any read-by-id` line with the five core paths, one endpoint per path as chip-tool requires, reusing the established CASE session); responses are tagged `source=poll`. Poll rows do not count as subscription liveness (`/health`, `resubscribed` events need a `source=subscription` report). After a failed poll the next one is delayed 120 s, 240 s, ... up to 960 s (`poll_failed` events); it resets after a success. No poll runs while an automatic inventory is in progress.
- **Attribute writes** (`OccupiedHeatingSetpoint`, `SystemMode`) in the daemon go through the interactive chip-tool session (existing CASE session, no new process and no fresh address resolution; the one-shot `AddressResolve ... Timeout` of a separate process is avoided). Each attempt keeps the `timeout_s` limit and the write-response parsing; a failed attempt (timeout, resolve error, session loss) is retried up to 3 times in the same session with 5 s and 10 s backoff, a device status rejection (`status = 0x..`) is not retried. The one-shot path remains only for CLI commands (`heatd set`), which keep the storage lock semantics.
- **Automatic inventory** pauses the interactive chip-tool session (the process is stopped and restarted afterwards, so subscriptions are re-established) and runs a one-shot chip-tool process on the same storage in that window, so two chip-tool processes never use the storage at the same time (the CLI `heatd inventory` needs the storage lock and heatd stopped). Raw logs are written even when chip-tool exits non-zero or times out (`--timeout` is passed). Output: `inventory/<node>_<fw>.json`, `.md` and raw logs under `inventory/raw/` in `inventory_dir` (default `inventory`), `fw` = SoftwareVersionString, else SoftwareVersion.

## Context, QA and analysis (heatd runtime)

- Weather (`crates/heatd/src/context.rs`): enabled only when `latitude` and `longitude` are set in heatd.toml (placeholders are commented out in heatd.example.toml). Hourly (5 min past the hour, retry every 10 min on failure) one Open-Meteo forecast-API call with `hourly=temperature_2m,wind_speed_10m,shortwave_radiation,cloud_cover,precipitation&wind_speed_unit=ms&timezone=UTC&timeformat=unixtime&past_days=2&forecast_days=2` (`weather_url` in config overrides the endpoint). Hours up to the current hour become `obs` rows, written once per hour (the newest `obs` ts is rebuilt from `raw/weather` after a restart), later hours become `fc` rows with `issued_ts` = fetch hour; this repeats every hour, so forecasts accumulate per issue time. `past_days=2` is the 48 h backfill: after an outage every missing observation hour of the last 48 h is written on the next successful fetch. Radiation and precipitation are Open-Meteo preceding-hour values, temperature, wind and cloud cover are instantaneous. Tests use the recorded response `crates/heatd/tests/fixtures/open_meteo_forecast.json`, never the network.
- Meta (`write_meta`, at start): `rooms` is a JSON list of `{name, nodes}` (nodes included so the analysis view can map node to room; rooms without thermostat have an empty list), `adjacency` the sorted unique room pairs from the config key `neighbours`, `links` (only when kinds are set) the list of `{a, b, kind (wall|ceiling|open), partial}` (for `ceiling`, `a` is above `b`), `building` `{floor, roof_rooms, exterior}` when set, `heat_source` `{type, scope, efficiency_class, flow_temp_controllable, night_setback_controllable, operating_hours, night_shutdown}` from `[heat_source]`, `coordinates` `{lat, lon}` only when set. A row is appended only when the value differs from the newest row of that key in `raw/meta`; `valid_from` is the write time.
- NTP: SNTP query (UDP 123) to `ntp_server` (default pool.ntp.org, empty disables) at start and hourly; offset = ((T2-T1)+(T3-T4))/2. `|offset| >= 1 s` writes an `ntp_drift` event and a log warning. The offset is kept in the status struct either way.
- Daily report: 03:10 local (after the 03:00 roll; at start, 2 minutes after boot, when yesterday's file is missing) writes `<data_dir>/reports/<date>.md` for the previous UTC date and logs one summary line. Sections: reports per node (count, subscription/poll split, first/last, longest gap, battery from BatPercentRemaining), link quality trend (Thread neighbour table regex over `v_str` of cluster 53 attribute 7, plus watch RSSI), labels per room and source, missing weather hours, experiments, events by kind, roll status per table from the `.rolled` markers. Rows are selected by `ts` in the UTC day from the Parquet partitions date-1 to date+1. `heatd report [YYYY-MM-DD]` regenerates one on demand (rolls the day first).
- `/health` returns `status` (`ok`, or `degraded` while a node is `lost`/`resubscribing`), `uptime_s`, per node `subscription`, `subscription_age_s`, `last_report_age_s`, `reports`, plus `ntp_offset_s`, `weather_age_s`, `last_roll`. `/metrics` is the same data as Prometheus text (`heatd_node_subscription`, `heatd_node_last_report_age_seconds`, `heatd_node_reports_total`, `heatd_ntp_offset_seconds`, `heatd_weather_last_fetch_age_seconds`, `heatd_weather_rows_total`, `heatd_last_roll_age_seconds`, `heatd_last_roll_ok`, `heatd_uptime_seconds`).
- Status struct (`crates/heatd/src/status.rs`, `AppState.status: Status`, cheap `Clone`): `status.node_report(node, ts_us)` on every attribute report of a node, `status.subscription(node, SubState::{Unknown,Subscribed,Resubscribing (reserved, never set),Lost}, ts_us)` on every subscription state change (the age is measured from the last change), `status.register_nodes(iter)` at start (done in `serve`). Others are written by the context jobs (`ntp_offset`, `weather_fetched`, `rolled`).
- Analysis view `analysis/views/room_minute.sql` (header documents all columns and the gap mask) is a DuckDB view `room_minute` on the gap-free UTC 1-minute grid per room. It reads the lake tables as views of the same names, so for ad-hoc work create those views over `read_parquet` and set `from_us` / `to_us` (see the file header). `heatd export --room <r> --from <date> --to <date> [--out path]` (`--to` inclusive) rolls pending past days, builds everything and writes `SELECT * FROM room_minute WHERE room = r` as Zstd Parquet (default `<data_dir>/exports/<room>_<from>_<to>.parquet`). The SQL file is compiled into the binary, so the Docker build copies `analysis/`. DuckDB is built with the `parquet` and `json` extensions (the bundled build has no network access to install them).
- Watch matcher (`crates/heatd/src/watchmatch.rs`, config `[watchmatch]` and `[watchmatch.state]`, contract in `contract/README.md`): disabled unless `enabled = true`. Model directory `model_dir/<model_version>/` with `model.json` and one ONNX file per activity state (`states`, fallback key `all`; `offbody`/`charging` only with an explicit key, else `unknown_state`); `model_version = "latest"` takes the directory with the newest `created` and is re-checked hourly (hot reload; an invalid newer directory is ignored and the old model stays). Per fresh Wi-Fi scan (`/watch/measurement` long or phase-1 format, or `/api/scan`; `ts` not older than `scan_fresh_s`, strictly newer than the previous scan of the device) heatd computes the room probabilities, applies one HMM forward step (`steps = clamp(round(minutes since the last scan), 1, 60)` applications of the transition matrix built from `smoothing`; after more than 60 minutes the prior is uniform; the classifier output is used as emission), decides with `tau_room` / `delta_room` and writes one `watch_inference` row (`matcher=room`). A minute tick writes one `no_scan` row when a device has been silent for 2 minutes. The rules take the matcher room only if it is not abstained, else the last confident room for `t_hold_min` minutes, else no room (`Presence.room = None`, no boost, away logic untouched). Without a loadable model every scan is `no_model` and the presence logic runs on the legacy prediction, labels and Wi-Fi away state. Activity state per scan: same rules as the `watch_minute` view with the thresholds of `[watchmatch.state]`.
- Watch drift alarm: the daily report job compares, per matcher, the abstain rate of the last 7 completed UTC days with the 7 days before (rows with reason `no_scan` excluded) and appends a `matcher_drift` event if it rose by more than 50 % (relative), by at least 5 percentage points, with at least 20 rows in each window.
- `/health` additionally returns `watch`: `model_version`, `abstain` (per matcher and state: `total`, `abstained`, `rate` since start), `samples` (per `sensor_type` since start) and `battery_pct_per_h` (watch drain over the last 24 h while not charging, null with less than 30 minutes of data). `/metrics` adds `heatd_watch_model_loaded`, `heatd_watch_inference_total`, `heatd_watch_abstained_total`, `heatd_watch_abstain_ratio` (labels `matcher`, `state`), `heatd_watch_samples_total{sensor_type}` and `heatd_watch_battery_percent_per_hour`. The daily report has a `Watch` section (abstain rate per matcher and state, abstain reasons, sensor sample counts, battery drain; labels per room are in the labels section).
- Soll/Ist und Nachregeln (`crates/heatd/src/dynamics.rs`, `analysis/views/room_delta.sql`, `analysis/views/radiator_summary.sql`): `room_delta` (Minute: delta = actual - setpoint, Ventil, Flags) und `room_delta_daily` (je Raum und UTC-Tag: mittleres Delta, mittlerer Betrag, P90 des Betrags, Anteil Minuten mit |delta| <= 0,5 C, Heizminuten mit Ventil > 0; nur Minuten ohne Luecke, Exploration getrennt gezaehlt). Die Tabelle `step_responses` wird in Rust aus `room_minute` und den angewandten `decisions` berechnet: ein Sprung ist eine Aenderung des gemeldeten Sollwerts um mindestens 0,5 C (Folgeaenderungen innerhalb von 3 min zaehlen als ein Sprung; `source` = Regel der passenden Entscheidung, `device` wenn keine). Je Sprung: Richtung (`heat_up`/`cool_down`), Sprunggroesse, Starttemperatur, Totzeit (min bis die Temperatur sich 0,1 C in die richtige Richtung bewegt), Anfangsrate (C/h, Regressionsgerade ueber die ersten 30 min nach der Totzeit, positiv = Richtung Ziel; `rate_per_k` je K Sprung, `rate_per_valve` je % Ventil), Zeit bis Ziel (innerhalb 0,3 C, leer wenn vor dem naechsten Sprung oder 6 h nicht erreicht), Ueberschwingen (max. C jenseits des Ziels in den 60 min danach), Ventil mittel/max waehrend des Einschwingens, Aussentemperatur und Strahlung zum Start, Anzahl gleichzeitig heizender anderer Raeume (30 min nach Start), Exploration/`experiment_id`. `radiator_summary` (Median je Raum/Knoten ueber das Fenster, im Report 14 Tage) und `radiator_pairs` vergleichen Aufheizspruenge verschiedener Raeume (Start <= 60 min auseinander, Ventil +-15 Punkte, Aussentemperatur +-3 K) als erster Hinweis auf hydraulischen Abgleich/Vorlauftemperatur. Das Thermostat misst am Heizkoerper, nicht in der Raummitte: Raten und Zeiten sind optimistisch. Der Tagesreport enthaelt den Abschnitt "Soll/Ist und Nachregeln". `heatd export --view step_responses|room_delta|room_delta_daily|radiator_summary|radiator_pairs [--room r] [--from d] [--to d] [--out p]` schreibt sie als Parquet (Standard: die letzten 14 Tage bis gestern).
- Analysis views `analysis/views/watch_minute.sql` and `analysis/views/watch_room_calib.sql` (headers document columns and variables): `watch_minute` is the gap-free UTC 1-minute grid of all watch signals with the derived features and the activity state (thresholds, sleep window and local offset are DuckDB variables, set by `report::open_watch_views`); `watch_room_calib` adds the Wi-Fi fingerprint of the minute (last scan) and the room label (labels with source `watch`/`manual`, window labels included).

## Public API

`crates/heatd/src/lake.rs`: `Table`, row structs (`AttributeReport`, `Event`, `Decision` with `DecisionInputs`, `Label`, `WatchRaw`, `WatchSensor`, `WatchInference`, `Weather`, `Meta`), trait `Row`, `LakeConfig`, `Sink` (`open`, `append`, `append_many`, `append_on`, `tick`, `flush`, `close`, `roll_day`, `roll_pending`), `kind::*`, `spawn_background`. `AppState.sink` carries the shared `Sink`.
