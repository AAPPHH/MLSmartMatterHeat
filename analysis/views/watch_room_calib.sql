-- watch_room_calib: watch_minute plus the Wi-Fi fingerprint of the minute and the room label, the training input of the room matcher.
-- Needs the same setup as watch_minute (create that view first, then this file) and the lake table labels as a view.
-- Columns: all columns of watch_minute, and
--   scan_ts    timestamp of the last scan inside the minute (null if the minute has no scan; such rows carry no fingerprint)
--   n_aps      number of access-point rows of that scan
--   rssi       list of {sender, rssi} of that scan sorted by sender (rows of rssi samples with the same ts form one scan; missing senders are -100 dBm by convention of the model)
--   label_room / label_source   room of the label that applies to the minute, null if none. Labels with source watch or manual only (model and override rows are not ground truth).
--              A window label (from_ts, to_ts) applies to every minute it overlaps, a point label (from_ts null) to the minute of its ts; on overlap the label recorded last wins.
CREATE OR REPLACE VIEW watch_room_calib AS
WITH
scans AS (
  SELECT epoch_us(ts) // 60000000 AS m, epoch_us(ts) AS us, sender, rssi::INTEGER AS rssi FROM watch_raw WHERE device IS DISTINCT FROM 'minix-anchor' AND coalesce(sensor_type, 'rssi') = 'rssi'
),
last_scan AS (SELECT m, max(us) AS scan_us FROM scans GROUP BY m),
fingerprint AS (
  SELECT s.m, l.scan_us, count(*) AS n_aps, list({'sender': s.sender, 'rssi': s.rssi} ORDER BY s.sender, s.rssi) AS rssi
  FROM scans s JOIN last_scan l ON s.m = l.m AND s.us = l.scan_us GROUP BY s.m, l.scan_us
),
windows AS (
  SELECT room, source, epoch_us(ts) AS rec_us, coalesce(epoch_us(from_ts), epoch_us(ts)) AS f, coalesce(epoch_us(to_ts), epoch_us(ts)) AS e FROM labels WHERE source IN ('watch', 'manual')
),
minutes AS (SELECT epoch_us(t) AS t_us, epoch_us(t) // 60000000 AS m FROM watch_minute),
applied AS (
  SELECT m.m, arg_max(w.room, w.rec_us) AS label_room, arg_max(w.source, w.rec_us) AS label_source
  FROM minutes m JOIN windows w ON w.f <= m.t_us + 59999999 AND w.e >= m.t_us GROUP BY m.m
)
SELECT w.*, make_timestamp(f.scan_us) AS scan_ts, f.n_aps, f.rssi, a.label_room, a.label_source
FROM watch_minute w
LEFT JOIN fingerprint f ON f.m = epoch_us(w.t) // 60000000
LEFT JOIN applied a ON a.m = epoch_us(w.t) // 60000000
ORDER BY w.t;
