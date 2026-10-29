-- room_delta / room_delta_daily: Soll/Ist-Abweichung je Raum und Minute bzw. Tag. Braucht die View room_minute.
-- Optional: SET VARIABLE delta_to_us = <epoch us>; schneidet Minuten ab diesem Zeitpunkt ab (Standard: to_us).
-- room_delta: delta = actual - setpoint (C, positiv = zu warm), valve_pct, Flags. delta_ok = Ist und Soll ohne Luecke (gap_mask Bit 1,2), valve_ok = Ventil ohne Luecke (Bit 4).
-- room_delta_daily (UTC-Tag): Minuten mit Luecke zaehlen nirgends mit; Exploration-Minuten werden getrennt gezaehlt (exploration_minutes, exploration_mean_abs_delta) und fliessen nicht in die Normalwerte.
--   minutes: Minuten mit gueltigem Delta ohne Exploration; band_share: Anteil mit |delta| <= 0.5 C; heating_minutes: Ventil > 0 %; gap_minutes: Minuten ohne gueltiges Delta.
CREATE OR REPLACE VIEW room_delta AS
SELECT room, t, actual, setpoint, actual - setpoint AS delta, valve_pct, setpoint_cmd, presence, window_open, exploration, gap_mask,
  gap_mask & 3 = 0 AS delta_ok, gap_mask & 4 = 0 AS valve_ok
FROM room_minute
WHERE epoch_us(t) < coalesce(getvariable('delta_to_us'), getvariable('to_us'));
CREATE OR REPLACE VIEW room_delta_daily AS
SELECT room, CAST(date_trunc('day', t) AS DATE) AS day,
  count(*) FILTER (WHERE delta_ok AND NOT exploration) AS minutes,
  avg(delta) FILTER (WHERE delta_ok AND NOT exploration) AS mean_delta,
  avg(abs(delta)) FILTER (WHERE delta_ok AND NOT exploration) AS mean_abs_delta,
  quantile_cont(abs(delta), 0.9) FILTER (WHERE delta_ok AND NOT exploration) AS p90_abs_delta,
  avg((abs(delta) <= 0.5)::INTEGER) FILTER (WHERE delta_ok AND NOT exploration) AS band_share,
  count(*) FILTER (WHERE valve_ok AND valve_pct > 0 AND NOT exploration) AS heating_minutes,
  count(*) FILTER (WHERE delta_ok AND exploration) AS exploration_minutes,
  avg(abs(delta)) FILTER (WHERE delta_ok AND exploration) AS exploration_mean_abs_delta,
  count(*) FILTER (WHERE NOT delta_ok) AS gap_minutes,
  count(*) AS grid_minutes
FROM room_delta GROUP BY room, day;
