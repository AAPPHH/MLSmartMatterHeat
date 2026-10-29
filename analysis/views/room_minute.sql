-- room_minute: one row per room and UTC minute on a gap-free 1-minute grid [from_ts, to_ts).
-- Needs: the lake tables as views named attribute_reports, events, decisions, weather, meta
-- (CREATE VIEW x AS SELECT * FROM read_parquet(['/data/parquet/x/date=*/**/part.parquet'], union_by_name = true)),
-- and two variables in epoch microseconds: SET VARIABLE from_us = 1793145600000000; SET VARIABLE to_us = 1793232000000000; (minute aligned, UTC).
-- `heatd export` and `report::open_lake` set all of this up. Read partitions from (from - 2 days) to (to + 1 day): partition dates are ingest dates.
-- Values are carried forward (last report at or before the end of the minute). Matter units: centi-degrees for 0x0201/0x0000 and 0x0012, percent for 0x0008; legacy rows hold v_f64 already in the final unit.
-- Columns (t is the UTC minute start as a timestamp without time zone):
--   actual            room mean of LocalTemperature (C) over the room's nodes
--   setpoint          room mean of reported OccupiedHeatingSetpoint (C); setpoint_cmd = last applied decision setpoint_new
--   valve_pct         room mean of PIHeatingDemand (%)
--   outdoor_temp / radiation / wind   weather obs (C, W/m2 preceding-hour mean, m/s), last hourly observation
--   presence          true after presence_arrive, false after presence_leave, null before the first presence event
--   neighbour_temps   list of {room, temp} for rooms paired in meta `adjacency`; neighbour_temp_mean their mean
--   window_open       last decision inputs.window_open of the room (null if older than 1 h)
--   exploration       true between experiment_start and experiment_end/abort of any node of the room
--   gap_mask          bit 1 actual older than 6 min or missing, 2 setpoint, 4 valve_pct, 8 weather older than 3 h or missing
CREATE OR REPLACE VIEW room_minute AS
WITH
params AS (SELECT getvariable('from_us') // 60000000 AS m0, getvariable('to_us') // 60000000 AS m1),
current_meta AS (SELECT key, arg_max(value, ts) AS value FROM meta GROUP BY key),
rooms AS (
  SELECT json_extract_string(r.value, '$.name') AS room, n.value::UBIGINT AS node
  FROM current_meta, json_each(current_meta.value) r, json_each(json_extract(r.value, '$.nodes')) n
  WHERE current_meta.key = 'rooms'
),
adjacent AS (
  SELECT json_extract_string(p.value, '$[0]') AS room, json_extract_string(p.value, '$[1]') AS neighbour FROM current_meta, json_each(current_meta.value) p WHERE current_meta.key = 'adjacency'
  UNION
  SELECT json_extract_string(p.value, '$[1]'), json_extract_string(p.value, '$[0]') FROM current_meta, json_each(current_meta.value) p WHERE current_meta.key = 'adjacency'
),
grid AS (SELECT room, node, m * 60000000 AS t, m * 60000000 + 59999999 AS te FROM rooms, params, range(params.m0, params.m1) AS g(m)),
obs AS (
  SELECT node, epoch_us(ts) AS us,
    CASE attribute WHEN 0 THEN 'actual' WHEN 18 THEN 'setpoint' ELSE 'valve' END AS k,
    CASE WHEN attribute IN (0, 18) THEN coalesce(v_f64, v_i64 / 100.0) ELSE coalesce(v_f64, v_i64::DOUBLE) END AS v
  FROM attribute_reports
  WHERE cluster = 513 AND attribute IN (0, 8, 18) AND (v_f64 IS NOT NULL OR v_i64 IS NOT NULL)
),
node_min AS (
  SELECT g.room, g.t, g.te, a.v AS actual, g.te - a.us AS actual_age, s.v AS setpoint, g.te - s.us AS setpoint_age, v.v AS valve_pct, g.te - v.us AS valve_age
  FROM grid g
  ASOF LEFT JOIN (SELECT node, us, v FROM obs WHERE k = 'actual') a ON g.node = a.node AND g.te >= a.us
  ASOF LEFT JOIN (SELECT node, us, v FROM obs WHERE k = 'setpoint') s ON g.node = s.node AND g.te >= s.us
  ASOF LEFT JOIN (SELECT node, us, v FROM obs WHERE k = 'valve') v ON g.node = v.node AND g.te >= v.us
),
room_min AS (
  SELECT room, t, any_value(te) AS te, avg(actual) AS actual, avg(setpoint) AS setpoint, avg(valve_pct) AS valve_pct,
    coalesce(min(actual_age) > 360000000, true) AS gap_actual, coalesce(min(setpoint_age) > 360000000, true) AS gap_setpoint, coalesce(min(valve_age) > 360000000, true) AS gap_valve
  FROM node_min GROUP BY room, t
),
neighbours AS (
  SELECT a.room, r.t, list({'room': a.neighbour, 'temp': r.actual} ORDER BY a.neighbour) FILTER (WHERE r.actual IS NOT NULL) AS neighbour_temps, avg(r.actual) AS neighbour_temp_mean
  FROM adjacent a JOIN room_min r ON r.room = a.neighbour GROUP BY a.room, r.t
),
commands AS (
  SELECT room, epoch_us(ts) AS us, setpoint_new FROM decisions WHERE applied AND setpoint_new IS NOT NULL
),
windows AS (
  SELECT room, epoch_us(ts) AS us, inputs.window_open AS window_open FROM decisions WHERE inputs.window_open IS NOT NULL
),
experiments AS (
  SELECT r.room, epoch_us(e.ts) AS us, e.kind = 'experiment_start' AS active FROM events e JOIN (SELECT DISTINCT room, node FROM rooms) r ON e.node = r.node WHERE e.kind IN ('experiment_start', 'experiment_end', 'experiment_abort')
),
presence_events AS (
  SELECT epoch_us(ts) AS us, kind = 'presence_arrive' AS home FROM events WHERE kind IN ('presence_arrive', 'presence_leave')
),
weather_obs AS (
  SELECT epoch_us(ts) AS us, arg_max(temp_c, fetched_ts) AS temp_c, arg_max(radiation_wm2, fetched_ts) AS radiation_wm2, arg_max(wind_ms, fetched_ts) AS wind_ms FROM weather WHERE kind = 'obs' GROUP BY ts
)
SELECT
  m.room, make_timestamp(m.t) AS t, m.actual, m.setpoint, c.setpoint_new AS setpoint_cmd, m.valve_pct,
  w.temp_c AS outdoor_temp, w.radiation_wm2 AS radiation, w.wind_ms AS wind,
  p.home AS presence, n.neighbour_temps, n.neighbour_temp_mean,
  CASE WHEN m.te - win.us <= 3600000000 THEN win.window_open END AS window_open,
  coalesce(x.active, false) AS exploration,
  m.gap_actual::INTEGER | (m.gap_setpoint::INTEGER << 1) | (m.gap_valve::INTEGER << 2) | (coalesce(m.te - w.us > 10800000000, true)::INTEGER << 3) AS gap_mask
FROM room_min m
ASOF LEFT JOIN commands c ON m.room = c.room AND m.te >= c.us
ASOF LEFT JOIN windows win ON m.room = win.room AND m.te >= win.us
ASOF LEFT JOIN experiments x ON m.room = x.room AND m.te >= x.us
ASOF LEFT JOIN presence_events p ON m.te >= p.us
ASOF LEFT JOIN weather_obs w ON m.te >= w.us
LEFT JOIN neighbours n ON n.room = m.room AND n.t = m.t
ORDER BY m.room, m.t;
