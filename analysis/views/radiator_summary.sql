-- radiator_pairs / radiator_summary: Zusammenfassung je Heizkoerper (room, node) ueber alle Zeilen der Tabelle step_responses (Fenster = --from..--to, Report: 14 Tage).
-- Braucht die Tabelle step_responses (von dynamics::open_dynamics befuellt).
-- ACHTUNG: Das Thermostat misst direkt am Heizkoerper (kein Raumthermometer); Anstiegsraten und Zeiten sind deshalb optimistisch (zu schnell/zu hoch) gegenueber der echten Raumluft.
-- radiator_pairs: Aufheizspruenge zweier Raeume mit Start <= 60 min auseinander, aehnlicher Ventiloeffnung (<= 15 Prozentpunkte) und Aussentemperatur (<= 3 K); ratio = rate_per_k(room) / rate_per_k(peer).
-- radiator_summary: Mediane je room/node; rel_rate_vs_peers = Median der Paar-Verhaeltnisse (> 1 schneller als die Nachbarraeume bei aehnlichem Ventil; Hinweis auf hydraulischen Abgleich/Vorlauftemperatur, kein Beweis).
CREATE OR REPLACE VIEW radiator_pairs AS
SELECT a.room, a.node, b.room AS peer, a.rate_per_k / b.rate_per_k AS ratio
FROM step_responses a JOIN step_responses b
  ON a.room <> b.room AND a.direction = 'heat_up' AND b.direction = 'heat_up'
  AND abs(epoch_us(a.t_start) - epoch_us(b.t_start)) <= 3600000000
  AND abs(a.initial_valve_pct - b.initial_valve_pct) <= 15 AND abs(a.outdoor_temp - b.outdoor_temp) <= 3
WHERE a.rate_per_k > 0 AND b.rate_per_k > 0;
CREATE OR REPLACE VIEW radiator_summary AS
WITH pairs AS (SELECT room, node, median(ratio) AS rel_rate_vs_peers, count(*) AS pair_count FROM radiator_pairs GROUP BY room, node)
SELECT s.room, s.node, count(*) AS steps, count(*) FILTER (WHERE s.direction = 'heat_up') AS heat_up_steps,
  median(s.dead_time_min) AS median_dead_time_min,
  median(s.rate_per_k) FILTER (WHERE s.direction = 'heat_up') AS median_rate_per_k,
  median(s.rate_per_valve) FILTER (WHERE s.direction = 'heat_up') AS median_rate_per_valve,
  median(s.time_to_target_min) AS median_settle_min,
  median(s.overshoot_c) AS median_overshoot_c,
  max(s.overshoot_c) AS max_overshoot_c,
  p.rel_rate_vs_peers, coalesce(p.pair_count, 0) AS pair_count,
  'Thermostat misst am Heizkoerper, Raten optimistisch' AS note
FROM step_responses s LEFT JOIN pairs p ON p.room = s.room AND p.node = s.node
GROUP BY s.room, s.node, p.rel_rate_vs_peers, p.pair_count;
