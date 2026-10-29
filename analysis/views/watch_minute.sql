-- watch_minute: one row per UTC minute on a gap-free 1-minute grid [from_ts, to_ts) with all watch signals (long-format rows of watch_raw) and the activity state.
-- Needs: the lake table watch_raw as a view (CREATE VIEW watch_raw AS SELECT * FROM read_parquet([...], union_by_name = true)) and the variables from_us / to_us (epoch microseconds, minute aligned, UTC).
-- `report::open_watch_views` sets everything up. Read partitions from (from - 2 days) to (to + 1 day): partition dates are ingest dates.
-- Optional variables (SET VARIABLE name = value), defaults in brackets, same meaning as heatd.toml [watchmatch.state]:
--   hr_rest_max [75], hr_active_min [100], motion_active_min [0.5], sleep_from_min [1380] / sleep_to_min [420] (minute of the local day; sleep window 23-7),
--   local_offset_min [0] (minutes to add to UTC for the local day used by sleep window and tod_sin/tod_cos; fixed offset, no DST handling).
-- Columns (t is the UTC minute start as a timestamp without time zone):
--   skin_temp  mean skin temperature (degC) of the minute, hr / motion mean of the minute, n_rssi number of access-point rows, battery_pct last battery value at or before the minute end (carried)
--   on_body    last offbody sample at or before the minute end (true = on body), null before the first sample; charging likewise
--   min_since_onbody_change   minutes since the last change of on_body (null if it never changed); min_since_charge_end minutes since the last charging -> not charging transition
--   hr_mean_10 / hr_std_10    mean / standard deviation of the hr samples of the last 10 minutes (including this one); motion_10 mean of the motion samples of the last 10 minutes
--   tod_sin / tod_cos         time of day of the local minute as sin / cos of 2*pi*minute/1440
--   state      charging | offbody | sleep | active | rest, rules in this order:
--              charging if charging; offbody if on_body is false; sleep if in the sleep window and hr_mean_10 <= hr_rest_max (or null) and motion_10 < motion_active_min (or null);
--              active if hr_mean_10 >= hr_active_min or motion_10 >= motion_active_min; else rest. heatd applies the same rules per scan (watchmatch.rs).
CREATE OR REPLACE VIEW watch_minute AS
WITH
params AS (
  SELECT getvariable('from_us') // 60000000 AS m0, getvariable('to_us') // 60000000 AS m1,
    coalesce(getvariable('hr_rest_max'), 75)::DOUBLE AS hr_rest_max, coalesce(getvariable('hr_active_min'), 100)::DOUBLE AS hr_active_min,
    coalesce(getvariable('motion_active_min'), 0.5)::DOUBLE AS motion_active_min, coalesce(getvariable('sleep_from_min'), 1380)::BIGINT AS sleep_from,
    coalesce(getvariable('sleep_to_min'), 420)::BIGINT AS sleep_to, coalesce(getvariable('local_offset_min'), 0)::BIGINT AS tz
),
sig AS (
  SELECT epoch_us(ts) AS us, epoch_us(ts) // 60000000 AS m, sensor_type, value::DOUBLE AS value FROM watch_raw WHERE device IS DISTINCT FROM 'minix-anchor' AND sensor_type IN ('temp', 'hr', 'offbody', 'motion', 'battery', 'charging') AND value IS NOT NULL
),
minute_agg AS (
  SELECT m,
    sum(value) FILTER (WHERE sensor_type = 'hr') AS hr_sum, sum(value * value) FILTER (WHERE sensor_type = 'hr') AS hr_sq, count(*) FILTER (WHERE sensor_type = 'hr') AS hr_n,
    sum(value) FILTER (WHERE sensor_type = 'motion') AS mo_sum, count(*) FILTER (WHERE sensor_type = 'motion') AS mo_n,
    avg(value) FILTER (WHERE sensor_type = 'temp') AS skin_temp
  FROM sig GROUP BY m
),
rssi_n AS (
  SELECT epoch_us(ts) // 60000000 AS m, count(*) AS n FROM watch_raw WHERE device IS DISTINCT FROM 'minix-anchor' AND coalesce(sensor_type, 'rssi') = 'rssi' GROUP BY 1
),
grid AS (SELECT m, m * 60000000 AS t, m * 60000000 + 59999999 AS te FROM params, range(params.m0 - 9, params.m1) AS g(m)),
windowed AS (
  SELECT g.m, g.t, g.te, a.skin_temp,
    a.hr_sum / nullif(a.hr_n, 0) AS hr, a.mo_sum / nullif(a.mo_n, 0) AS motion, coalesce(r.n, 0) AS n_rssi,
    sum(a.hr_sum) OVER w / nullif(sum(a.hr_n) OVER w, 0) AS hr_mean_10,
    CASE WHEN sum(a.hr_n) OVER w > 0 THEN sqrt(greatest(sum(a.hr_sq) OVER w / sum(a.hr_n) OVER w - power(sum(a.hr_sum) OVER w / sum(a.hr_n) OVER w, 2), 0)) END AS hr_std_10,
    sum(a.mo_sum) OVER w / nullif(sum(a.mo_n) OVER w, 0) AS motion_10
  FROM grid g LEFT JOIN minute_agg a ON a.m = g.m LEFT JOIN rssi_n r ON r.m = g.m
  WINDOW w AS (ORDER BY g.m ROWS BETWEEN 9 PRECEDING AND CURRENT ROW)
),
body_events AS (
  SELECT us, on_body, max(us) FILTER (WHERE changed) OVER (ORDER BY us ROWS BETWEEN UNBOUNDED PRECEDING AND CURRENT ROW) AS since_us
  FROM (SELECT us, value >= 0.5 AS on_body, lag(value >= 0.5) OVER (ORDER BY us) IS NOT NULL AND (value >= 0.5) IS DISTINCT FROM lag(value >= 0.5) OVER (ORDER BY us) AS changed FROM sig WHERE sensor_type = 'offbody')
),
charge_events AS (
  SELECT us, charging, max(us) FILTER (WHERE ended) OVER (ORDER BY us ROWS BETWEEN UNBOUNDED PRECEDING AND CURRENT ROW) AS ended_us
  FROM (SELECT us, value >= 0.5 AS charging, coalesce(lag(value >= 0.5) OVER (ORDER BY us), false) AND value < 0.5 AS ended FROM sig WHERE sensor_type = 'charging')
),
battery_events AS (SELECT us, value AS battery_pct FROM sig WHERE sensor_type = 'battery'),
joined AS (
  SELECT w.*, b.on_body, b.since_us, c.charging, c.ended_us, e.battery_pct
  FROM windowed w
  ASOF LEFT JOIN body_events b ON w.te >= b.us
  ASOF LEFT JOIN charge_events c ON w.te >= c.us
  ASOF LEFT JOIN battery_events e ON w.te >= e.us
),
classified AS (
  SELECT j.*, p.hr_rest_max, p.hr_active_min, p.motion_active_min, ((j.m + p.tz) % 1440 + 1440) % 1440 AS lmin, p.sleep_from, p.sleep_to
  FROM joined j, params p
)
SELECT
  make_timestamp(t) AS t, skin_temp, hr, motion, n_rssi, battery_pct, on_body, coalesce(charging, false) AS charging,
  CASE WHEN since_us IS NOT NULL THEN ((te - since_us) // 60000000)::BIGINT END AS min_since_onbody_change,
  CASE WHEN ended_us IS NOT NULL THEN ((te - ended_us) // 60000000)::BIGINT END AS min_since_charge_end,
  hr_mean_10, hr_std_10, motion_10,
  sin(2 * pi() * lmin / 1440) AS tod_sin, cos(2 * pi() * lmin / 1440) AS tod_cos,
  CASE
    WHEN coalesce(charging, false) THEN 'charging'
    WHEN on_body = false THEN 'offbody'
    WHEN (CASE WHEN sleep_from = sleep_to THEN false WHEN sleep_from < sleep_to THEN lmin >= sleep_from AND lmin < sleep_to ELSE lmin >= sleep_from OR lmin < sleep_to END)
      AND coalesce(hr_mean_10 <= hr_rest_max, true) AND coalesce(motion_10 < motion_active_min, true) THEN 'sleep'
    WHEN coalesce(hr_mean_10 >= hr_active_min, false) OR coalesce(motion_10 >= motion_active_min, false) THEN 'active'
    ELSE 'rest'
  END AS state
FROM classified
WHERE t >= (SELECT m0 * 60000000 FROM params)
ORDER BY t;
