# Phase 1b: Uhr als Template-Matcher (Raum jetzt, Temperatur später)

Quelle: Addendum des Nutzers vom 2026-10-29, angepasst an das, was die Pixel
Watch 4 einer normalen App erlaubt (geprüft per `dumpsys sensorservice`).

## Umfang dieser Runde
- Datenerfassung auf der Uhr komplett (inkl. Temperatur-Signalen), damit der
  Temperatur-Matcher später Daten vorfindet.
- Raum-Matcher komplett: Enrollment, Matching, Rejection, Re-Enrollment,
  Laufzeit in heatd, Evaluation.
- Temperatur-Matcher: NICHT in dieser Runde. Es gibt keine Raumthermometer
  (Ground Truth fehlt); nur Tabellen/Views so anlegen, dass er später
  andocken kann (`watch_inference.matcher = temp` bleibt vorgesehen).

## Abweichungen vom Addendum (verbindlich)
- Temperatursensoren: nur `com.google.sensor.skin_temperature` (Typ 65555,
  Recht `android.permission.health.READ_SKIN_TEMPERATURE`). Gehäuse-, IMU-,
  Magnetometer- und Druck-Thermistor sind `signature|privileged` und für uns
  nicht lesbar. Kein Umgebungstemperatursensor vorhanden.
- Weitere Sensoren: Heart Rate (`READ_HEART_RATE`), Low-Latency-Off-Body
  (kein Recht), Accelerometer, Akku/Ladeflag aus BatteryManager.
- "BLE-Scan" im Addendum = bestehende WLAN-Scan-Logik (Bluetooth-Ortung hat
  der Nutzer verworfen). WLAN-Scans bleiben ereignisgesteuert (Doze blockiert
  `startScan`), nicht 1/60 s.
- 1 Sample/60 s für Haut-Temperatur, HR, Off-Body, Bewegung über Hardware-
  Batching (`samplingPeriodUs` 60 s, `maxReportLatencyUs` 5 min, Non-Wakeup-
  Sensoren), keine zusätzlichen Alarme. Akku-Budget < 3 %/Tag Mehrverbrauch,
  Fallback 1/120 s. Batches an POST /watch/measurement alle 5 min oder
  50 Samples, beim Raum-Tipp sofort.

## Datenerfassung (Watch-App)
- `SensorManager.getSensorList(TYPE_ALL)` beim Start und bei Änderung an
  POST /watch/sensors (name, type, vendor, resolution, maxRange,
  app_version).
- Je Sample `elapsedRealtimeNanos`, je Batch Wall-Clock und
  `clock_offset_ms`. Keine Verarbeitung auf der Uhr.
- Labels: Raum-Tipp → POST /watch/label (ts, room, source=watch); die
  bestehenden /api/*-Endpunkte bleiben als Aliase.

## heatd: Tabellen (additiv zu docs/data-foundation.md)
- `watch_raw` erweitert um: elapsed_ns i64?, clock_offset_ms i32?,
  sensor_type dict (rssi|temp|hr|offbody|motion|battery|charging),
  sensor_name dict?, value f32?, unit dict?. Bestehende Spalten
  (sender, rssi, channel, freq, batch_id, …) bleiben; WLAN-Zeilen bekommen
  sensor_type=rssi und value=rssi.
- `watch_sensors`: ts, sensor_name, android_type i32, vendor, resolution,
  max_range, app_version.
- `watch_inference`: ts, matcher dict (room|temp), template_id, result_room
  dict?, result_temp f32?, confidence f32, abstained bool, reason dict?,
  distance f32?, state dict (rest|sleep|active|offbody|charging),
  features_hash, model_version. Strikt getrennt von `labels`.

## Feature-Layer (DuckDB-Views unter analysis/views/)
- `watch_minute`: 1-min-Raster aller Uhr-Signale; abgeleitet: Minuten seit
  letztem On-Body-Wechsel, Minuten seit Ladeende, HR-Mittel/Std der letzten
  10 min, Bewegungsmaß der letzten 10 min, Tageszeit sin/cos, Zustand.
- `watch_room_calib`: watch_minute + RSSI-Vektor (letzter frischer Scan der
  Minute) + Label-Raum (Labels mit from_ts/to_ts-Fenstern berücksichtigen).

## Zustandsklassifikation
- Regelbasiert aus HR, Bewegung, Off-Body, Ladeflag, Tageszeit:
  rest | sleep | active | offbody | charging; Schwellen in Config; Ergebnis je
  Minute in watch_minute. Templates sind pro Zustand getrennt (Fallback
  "all", wenn ein Zustand zu wenig Daten hat).

## Matcher 1: Raum (RSSI-Fingerprinting)
- Enrollment: pro Raum Template aus gelabelten RSSI-Vektoren (fehlende
  Sender −100 dBm); Klassifikator kNN oder Gradient Boosting auf dem
  Fingerprint-Vektor; zeitliche Glättung (HMM/Forward-Filter) mit
  Übergangswahrscheinlichkeiten aus der Nachbarschaft in `meta`.
- Matching: je Minute (und je frischem Scan) Wahrscheinlichkeit pro Raum;
  Ausgabe nur bei max_p ≥ τ_room und Margin zum Zweiten ≥ δ_room.
- Rejection: sonst abstained=true; heatd behält die letzte sichere Zuordnung
  bis T_hold Minuten, danach „unbekannt“ (kein Anwesenheits-Boost).
- Re-Enrollment: neue Labels fließen ein; wöchentliches Neutraining;
  Drift-Alarm (Event), wenn die Abstain-Quote über 7 Tage um > 50 % steigt.

## Laufzeit in heatd (`watchmatch`)
- Modelle als ONNX plus Sidecar-JSON, Version aus Config; Inferenz auf dem
  Minix, Ergebnis nach `watch_inference`.
- Fallback bei fehlendem Modell oder Fehler: alles abstained, Anwesenheit nur
  aus manuellen Labels und Heim-WLAN.
- /health und täglicher Report: Abstain-Quote je Matcher und Zustand, Labels
  je Raum, Akkuverbrauch der Uhr, Sensor-Sample-Zahlen.

## Evaluation (training/eval/)
- Accuracy und Macro-F1 je Raum mit zeitlich getrenntem Split (Tage);
  Coverage = 1 − Abstain-Quote; beide Zahlen immer zusammen berichten.
- Vergleich gegen Baseline „kein Matcher“.

## Abnahme (Raum)
- Watch-App 7 Tage mit < 3 %/Tag Mehrverbrauch, keine Batch-Verluste.
- Raum-Matcher Macro-F1 ≥ 0,85 bei Coverage ≥ 0,8 auf Hold-out-Tagen (sobald
  genug Labels vorhanden sind; bis dahin Pipeline mit synthetischen Daten
  nachweisen).
- Matcher in heatd angebunden, Abstain-Pfad getestet (Modell entfernt →
  Anwesenheit nur aus Labels, kein Absturz).
