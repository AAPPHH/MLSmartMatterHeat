# Phase 1: Datengrundlage (Logging, Speicherschicht, Explorationsmodus)

Ziel: heatd zeichnet ab sofort alles auf, was Thermostate, Uhr, Regelkern und
Wetter liefern – roh, append-only, lieber zu viel als zu wenig. Phase 2
(training/) liest ausschließlich die hier definierten Parquet-Tabellen und
DuckDB-Views; dieses Schema ist der Vertrag. Schreibe zuerst
docs/data-foundation.md mit den Arrow-Schemata, dann implementiere.

## 1. Matter-Erfassung
- Pro kommissioniertem Node eine Wildcard-Subscription (alle Endpoints, alle
  Cluster, alle Attribute und Events), min_interval 1 s, max_interval 300 s.
  Jeden Report unverändert speichern: TLV-Rohbytes plus dekodierter Wert.
- Zusätzlich alle 60 s ein Poll der Kernattribute (LocalTemperature,
  OccupiedHeatingSetpoint, PIHeatingDemand, SystemMode, BatPercentRemaining)
  als Heartbeat, Quelle `poll`.
- Subscription-Abriss, Resubscribe, StartUp-/ShutDown-Events, Firmware-
  Wechsel als Zeilen in `events`. Reconnect mit exponentiellem Backoff.
- Vendor-spezifische Cluster und Attribute (Eurotronic) ohne Interpretation
  mitschreiben; alles Unbekannte landet mit dtype `unknown` und Rohbytes.
- Befehl `heatd inventory <node>`: Wildcard-Read, Ausgabe
  inventory/<node>_<fw>.json (Cluster, Attribute, Zugriffsrechte, aktuelle
  Werte, Vendor-Cluster roh) plus Markdown-Tabelle für Menschen. Wird nach
  jedem Firmware-Wechsel automatisch wiederholt.

## 2. Regelkern-Logging
- Jede Regelentscheidung vor dem Schreiben ans Gerät nach `decisions`:
  ts, room, node, rule (schedule|presence|window|frost|manual|exploration|
  policy), inputs (Struct mit allen gesehenen Werten), setpoint_old,
  setpoint_new, mode, exploration, experiment_id, proposed, applied,
  policy_id, commit.
- Manuelle Eingriffe (Sollwertänderung am Gerät, erkannt über Report ohne
  eigene Entscheidung) als rule=manual plus Zeile in `labels` mit
  source=override.
- Uhr-Endpunkte: POST /watch/measurement nimmt Rohmessungen (ts, sender,
  rssi, channel, batch_id) nach `watch_raw`; POST /watch/label nimmt
  Raum-Labels nach `labels`. Modellausgaben des Raum-Modells (Raum,
  Wahrscheinlichkeiten, model_id) ebenfalls nach `labels`, source=model.

## 3. Kontext
- Open-Meteo stündlich für Koordinaten aus der Config: Temperatur, Wind,
  Globalstrahlung, Bewölkung, Niederschlag; Beobachtung (kind=obs) und
  Vorhersage (kind=fc, mit issued_ts) getrennt nach `weather`. Bei Ausfall
  Nachholen der letzten 48 h.
- `meta` aus der Config, versioniert: Raumliste, Nachbarschaft als Liste von
  Paaren, Heizquelle (Typ, Betriebszeiten/Nachtabschaltung falls bekannt),
  Koordinaten. Keine Geometrie erforderlich.

## 4. Speicherschicht (Arrow/Parquet)
- Heißer Pfad: Arrow IPC Stream pro Tabelle und Tag unter
  /data/raw/<table>/<date>.arrows; Flush alle 30 s oder 500 Zeilen, fsync
  nach jedem Batch. Beim Start unvollständige Dateien bis zum letzten gültigen
  Batch recovern.
- Kalter Pfad: Job 03:00 rollt den Vortag nach
  /data/parquet/<table>/date=<d>/node=<n>/part.parquet (Zstd, Dictionary auf
  String-Spalten, Row-Groups 64k). Zeilenzahl verifizieren, erst dann
  Marker `.rolled` schreiben. Rohdateien nie löschen.
- Schemata (Arrow), Schema-Version und heatd-Commit als Schema-Metadaten:
  attribute_reports: ts timestamp[us,UTC], mono_ns i64, node u64,
    endpoint u16, cluster u32, attribute u32, dtype dict<utf8>,
    v_i64 i64?, v_f64 f64?, v_bool bool?, v_str utf8?, v_bin binary?,
    value_raw binary, source dict<utf8>, fw dict<utf8>
  events: wie oben plus event_id u32, event_number u64, priority u8,
    device_ts timestamp?
  decisions: siehe Abschnitt 2
  labels: ts, room dict, source dict (watch|manual|override|model),
    confidence f32?, model_id utf8?
  watch_raw: ts, device_ts, sender dict, rssi i8, channel u8?, batch_id u64
  weather: ts, kind dict, issued_ts, Mess-Spalten f32
  meta: key/value mit valid_from
- Migrationen nur additiv. Jede Tabelle hat ein Schema-Testfile.

## 5. Explorationsmodus
- Konfigurierbar pro Raum: enabled, p_per_day, comfort_min, comfort_max,
  blackout-Zeiten (z. B. Schlafzimmer 22–7 Uhr aus).
- Experimente: Sollwertsprung ±1–2 °C für 60–180 min; Nachtabsenkung bis
  comfort_min; Ventil 0 % oder 100 % für 30 min (falls PIHeatingDemand
  schreibbar, sonst über Sollwert-Extrema). Zufällige Startzeit, nie unter
  Frostschutz, nie bei Fenster-offen-Erkennung.
- Jedes Experiment: experiment_id, Start/Ende in `events`, alle
  Entscheidungen mit exploration=true. Abbruch bei manuellem Eingriff,
  Abbruch wird geloggt.

## 6. Qualitätssicherung
- NTP-Drift-Check beim Start und stündlich, Warnung in `events` ab 1 s.
- Täglicher Report (Markdown nach /data/reports/<date>.md und Log): Reports
  pro Node, längste Lücke, Batterie, RSSI/LQI-Trend, Labels pro Raum,
  fehlende Wetterstunden, Anzahl Experimente, Roll-Status.
- /health erweitert um Subscription-Status pro Node und Alter des letzten
  Reports; /metrics mit denselben Werten.

## 7. Analyse-Schnittstelle
- DuckDB-View pro Raum auf 1-min-Raster: Ist, Soll, Ventil %, Außentemp,
  Strahlung, Wind, Anwesenheit, Nachbarraum-Temperaturen, Fenster-Flag,
  Explorations-Flag, Lückenmaske. Definition als SQL-Datei unter
  analysis/views/, Test gegen Beispiel-Parquet.
- Hilfsskript `heatd export --room <r> --from --to` schreibt den View als
  Parquet.

## Regeln
- Keine Vorverarbeitung im heißen Pfad, keine Aggregation, keine Retention.
- Kein Zugriff aus training/ auf /data/raw; training/ liest nur Parquet.
- Schemaänderungen aus Phase 2 kommen über docs/schema-requests.md.

## Abnahme
- Alle Tabellen schreiben und rollen verlustfrei (Testlauf mit
  Testthermostaten, Zeilenzahlen stimmen).
- Inventory für jedes echte Gerät im Repo.
- Explorationsmodus erzeugt in 24 h Testlauf mindestens ein Experiment pro
  aktiviertem Raum, korrekt markiert.
- Täglicher Report läuft, DuckDB-View liefert für einen Testtag ein
  vollständiges 1-min-Raster.

## Ergänzungen des Nutzers (2026-10-29)
- Abwesenheit (Uhr sieht das Heim-WLAN nicht, mit Schutz gegen "Uhr liegt
  zuhause/lädt"): alle Räume auf 18 °C, unabhängig vom Zeitplan.
- Anwesenheitsmuster lernen: Kommen/Gehen ab sofort als `events`
  (presence_arrive / presence_leave, mit Quelle) protokollieren. Später
  (eigene Phase, nach 2–3 Wochen Daten): Wahrscheinlichkeit "zuhause" je
  Wochentag und 15-min-Slot, Vorheizen vor erwarteter Rückkehr
  (anfangs 60 min Vorlauf, später aus dem thermischen Modell).
- Die bestehenden Uhr-Endpunkte /api/scan, /api/scans und /api/label bleiben
  als Aliase erhalten (installierte Uhr-App), schreiben aber in die neuen
  Tabellen watch_raw/labels. /api/label liefert Zeitfenster (from/to):
  labels bekommt dafür additiv from_ts/to_ts.
