# Datenvertrag Uhr ↔ heatd ↔ ML

## `POST /api/scan`

Body wie `scan_example.json`: `ts` Millisekunden seit Epoch (Uhrzeit der Uhr), `device` frei wählbarer Name, `label` Raumlabel während der Trainingsphase oder `null`, `aps` alle sichtbaren WLANs (`bssid` klein geschrieben mit `:`, `rssi` dBm, `freq` MHz, 2,4 und 5 GHz ungefiltert). Antwort `200` mit `{"stored": <anzahl aps>, "room": <vorhergesagter raum oder null>}`.

Optionale Felder (abwärtskompatibel): `motion` (`"moving"` | `"still"` | `null`), `queued` (`true`, wenn der Scan später aus dem Puffer hochgeladen wird; `ts` bleibt der Originalzeitpunkt), `power` (`"normal"` | `"saver"` | `"offbody"` | `"doze"` | `null`), `away` (`true`, wenn das Heim-WLAN nicht sichtbar ist). `aps` darf leer sein oder fehlen: das ist ein Heartbeat (`heartbeat_example.json`); er erzeugt keine Vorhersage, Antwort `{"stored": 0, "room": null}`. Unbekannte Werte für `motion`/`power` werden mit `422` abgelehnt.

### Idempotenz (`id`)

Optionales Feld `id` (String) bei Scans und Heartbeats, einzeln und im Batch: eindeutig pro Gerät und stabil über Wiederholungen, von der Uhr beim Einreihen als `<device>-<persistierter monotoner Zähler>` erzeugt (z. B. `pixel-watch-000123`, unabhängig vom Boot). `(device, id)` ist der Idempotenzschlüssel: eine bereits gesehene `id` wird mit `200` angenommen, aber nicht erneut gespeichert (weder `scans`, `device_state` noch `predictions`). Einzelantwort dann `{"stored": 0, "room": ..., "duplicate": true}`; `duplicate` fehlt sonst. Einträge ohne `id` verhalten sich wie bisher. Jeder Eintrag wird atomar in einer Transaktion gespeichert, ein Batch darf deshalb komplett wiederholt werden.

### Zeitstempel in der Zukunft

Liegt `ts` mehr als 5 Minuten nach der Eingangszeit (Uhrenabweichung), setzt heatd `ts` auf `received_at` und protokolliert es.

### Zuordnung der Schätzung (Attribution)

Antworten von `/api/scan` und `/api/scans` tragen zusätzlich (für den Scan, der den gemeldeten `room` lieferte; alte Felder unverändert): `probability` (Wahrscheinlichkeit des gemeldeten Raums oder `null`), `rooms` (alle Labels, nach `p` absteigend: `{"room", "p", "parts": [{"bssid", "share"}]}`) und `aps` (`{"bssid", "rssi", "freq"}` für jede in `parts` genannte BSSID, nach Gesamtanteil über alle Räume absteigend). Ohne Modell, bei leerem Scan oder Heartbeat: `null`, `[]`, `[]`.

Berechnung per Okklusion, modellunabhängig: pro BSSID des Scans aus dem Vokabular wird das Modell ohne diese BSSID erneut ausgeführt (ein gebündelter ONNX-Lauf); `drop_c = max(0, full_c - without_c)`. Pro Raum `c` sind `parts` die BSSIDs mit `drop_c > 0`, `share = full_c * drop_c / sum(drop_c)`, die Anteile summieren sich also zu `p`. Ist die Summe 0, ist `parts` leer (die ganze Wahrscheinlichkeit ist unerklärt).

## `POST /api/scans`

Batch für gepufferte Scans: `{"scans": [...]}`, Elemente wie bei `/api/scan` (`batch_example.json`). Antwort `200` mit `{"stored": <aps gesamt>, "accepted": <anzahl scans>, "duplicates": <anzahl wiederholter ids>, "room": <raum der Vorhersage mit dem größten ts im Batch oder null>}`. Die Reihenfolge der Elemente ist beliebig.

## `POST /watch/measurement` (Sensor- und WLAN-Messungen der Uhr)

Body wie `measurement_example.json`: `{"device","batch_id","sent_ts","clock_offset_ms","app_version","samples":[...]}`. `batch_id` ist eine Zahl (i64), stabil über Wiederholungen; `(device, batch_id)` ist der Idempotenzschlüssel (eine wiederholte `batch_id` wird mit `200` angenommen, aber nicht erneut gespeichert). `sent_ts` ist die Uhrzeit (Epoch ms) beim Bilden des Batches, `clock_offset_ms = Wanduhr_ms - elapsedRealtime_ms` zum selben Zeitpunkt; damit gilt für jedes Sample `ts ≈ elapsed_ns / 1e6 + clock_offset_ms`. Optional: `battery_pct_per_h` (Akkuverbrauch der Uhr in Prozentpunkten pro Stunde, gleitend über mindestens 1 h ohne Laden, fehlt sonst) und, nur bei WLAN-Batches, `id`, `motion`, `power`, `away`, `queued` mit der Bedeutung wie bei `/api/scan` (heatd schreibt dafür `device_state`).

Jedes Sample: `elapsed_ns` (`SensorEvent.timestamp` bzw. bei WLAN-Scans `ScanResult.timestamp`; fehlt bei alten Einträgen), `ts` (Epoch ms), `sensor_type`, `sensor_name`, `value`, `unit`; optional `sender` (BSSID klein), `channel`, `freq` (MHz).

| sensor_type | Quelle | unit | Takt |
|---|---|---|---|
| `temp` | Hauttemperatur (`com.google.sensor.skin_temperature`, Typ 65555) | `C` | 60 s, Hardware-Batching |
| `hr` | Herzfrequenz | `bpm` | 60 s, Hardware-Batching |
| `motion` | Varianz des Betrags der Beschleunigung je 60-s-Intervall (Accelerometer 1 Hz, Hardware-Batching) | `m2/s4` | 60 s |
| `offbody` | Low-Latency-Off-Body (`1` = Uhr nicht am Arm) | `bool` | bei Änderung |
| `battery` | Akkustand | `%` | je Batch und bei Lade-Wechsel |
| `charging` | Ladeflag | `bool` | je Batch und bei Lade-Wechsel |
| `rssi` | WLAN-Scan, eine Zeile je Access Point (`sender`, `freq`, `value` = RSSI) | `dBm` | ereignisgesteuert wie bisher |

Batches schließt die Uhr nach 5 Minuten oder 50 Samples, beim Raum-Tipp sofort. Sie liegen in der dauerhaften Warteschlange (kein Verlust bei Netzfehler oder Neustart; nur der noch offene Batch, höchstens 5 Minuten, geht bei einem harten Absturz verloren). Antwort `200` mit `{"stored": <anzahl>, "batch_id": <id>}`, bei Wiederholung `{"stored": 0, "duplicate": true}`. Scans mit Access Points schickt die Uhr nur noch hierhin (Antwort darf wie bei `/api/scan` `room`, `rooms`, `probability` enthalten); Heartbeats ohne APs und Away-Einträge gehen weiter an `/api/scan(s)`. Antwortet der Server mit `400/404/405/422`, fällt die Uhr eine Stunde lang auf `/api/scan` zurück. Einträge für `/watch/measurement` und `/watch/sensors` bleiben bei `400/404/405/422` in der Warteschlange (zurückgestellt) und gehen nach einem Server-Update ein.

## `POST /watch/sensors`

Sensorinventar beim Start und bei Änderung (Sensorliste oder `app_version`): `{"device","ts","app_version","sensors":[{"name","android_type","string_type","vendor","resolution","max_range"}]}` (Tabelle `watch_sensors`).

## `POST /watch/label`

`{"device","ts","from","to","room","source":"watch"}` mit den Regeln von `/api/label` (`room` entspricht `label`). Die Uhr fällt bei `400/404/405/422` auf `/api/label` zurück.

## `POST /api/label`

Nachträgliches Etikettieren (`label_example.json`): `{"device": <string>, "from": <epoch ms>, "to": <epoch ms>, "label": <string>}`. `label` muss `^[a-z_]{1,32}$` entsprechen, `from <= to` und `to - from <= 3600000`, sonst `400` mit `{"error": ...}`. heatd setzt `scans.label = label` für alle Zeilen des Geräts mit `ts` in `[from, to]` (beide Grenzen inklusive, `ts` wie bei `/api/scan` als Millisekunden nach TIMESTAMP gewandelt). Überschreiben ist gewollt: die letzte Korrektur gewinnt. Antwort `200` mit `{"labelled": <anzahl verschiedener scan_id>}`; idempotent. Raum-IDs der Uhr bleiben `kueche`, `wohnzimmer`, `flur`, `bad`, `schlafzimmer`.

Wie die Uhr etikettiert: Scans tragen `label` nur, solange ein bestätigter Aufenthalt („Stay“) läuft, sonst `null`. Ein Tipp auf einen der fünf Raumknöpfe der „Wo bist du?“-Benachrichtigung (oder in App/Kachel) erzeugt ein Label-Ereignis `[Zeit des Scans, der gefragt hat, jetzt]` und startet einen Stay mit dem WLAN-Fingerabdruck dieses Scans (gemittelt über bis zu 5 Scans). Danach werden Scans ohne Rückfrage direkt mit dem Stay-Raum etikettiert, höchstens 2 h lang. Raumwechsel erkennt die Uhr, wenn nach Bewegung zwei Scans in Folge vom Fingerabdruck abweichen (mittlere Differenz > 8 dB über die APs ab −85 dBm, fehlend = −100 dBm, oder Überlappung der AP-Mengen < 0,5) oder die Server-Vorhersage (`probability` >= 0,6) zweimal hintereinander vom Stay-Raum abweicht. Dann endet der Stay, die Uhr fragt mit Vibration „Raum gewechselt? Wo bist du jetzt?“, und die Antwort etikettiert ab dem ersten abweichenden Scan. Vibration höchstens alle 3 min, nie zwischen 22 und 7 Uhr, unterwegs oder ohne Uhr am Handgelenk. Antwortet der Server auf `/api/label` mit `404` (altes Image), bleibt das Ereignis in der Warteschlange und wird beim nächsten Flush erneut gesendet.

## Präsenz

Jeder Scan und Heartbeat schreibt eine Zeile in `device_state`. Vorhersagen tragen den Scan-`ts` (nicht die Eingangszeit), spät hochgeladene Scans werden also nie "frisch". Der belegte Raum ist die Vorhersage mit dem größten `ts` des Geräts und gilt, solange (i) der jüngste `device_state` (größter `ts`) `motion = still` meldet und der letzte Kontakt (`received_at`) jünger als `still_contact_timeout_minutes` (180) ist oder (ii) die Vorhersage jünger als `presence_timeout_minutes` (30) ist. Ist `power` `saver`/`offbody` oder der Kontakt älter als `still_contact_timeout_minutes`, ist die Präsenz unbekannt: Zeitplan ohne Boost, kein Eco. Eco gilt nur, wenn der jüngste Zustand `away = true` meldet.

## Feature-Vektor

Feste BSSID-Reihenfolge `bssids` aus dem Training. Für BSSID `i`: `x_i = clamp((rssi - rssi_floor) / rssi_span, 0, 1)` falls sichtbar, sonst `0`; Maske `m_i = 1` falls sichtbar, sonst `0`. Eingabe `float32 [1, 2F] = concat(x, m)`. Unbekannte BSSIDs werden ignoriert. Beispiel: `features_example.json` angewendet auf `scan_example.json`.

## Modell

`room_model.onnx` + Sidecar `room_model.json`: `{"version": 1, "input_name": "input", "bssids": [...], "labels": [...], "rssi_floor": -100.0, "rssi_span": 70.0}`. Ausgaben: `label` (`int64 [N]`, Index in `labels`) und `probabilities` (`float32 [N, C]`), ohne ZipMap. Testmodell: `crates/heatd/tests/fixtures/room_model.{onnx,json}`, sagt für `scan_example.json` `wohnzimmer` voraus.

## Phase 1b: `POST /watch/measurement`, `POST /watch/sensors`, Raum-Matcher

### `POST /watch/measurement`

Nimmt zwei Bodyformen an (Unterscheidung: Feld `samples` vorhanden oder nicht).

1. Phase-1-Form (unverändert): `{"device", "ts" (ms), "device_ts"?, "id"?, "batch_id"?, "measurements"|"aps": [{"sender"|"bssid", "rssi", "channel"?, "freq"?}]}`. Idempotenz über `id`. Antwort `{"stored": n, "batch_id": b}`, bei Wiederholung `{"stored": 0, "duplicate": true}`.
2. Langformat (Phase 1b):

```json
{"device": "pixel-watch", "batch_id": 1761580800001, "sent_ts": 1761580805000, "clock_offset_ms": -120, "app_version": "1.4.0",
 "samples": [
  {"elapsed_ns": 123456789000, "ts": 1761580800000, "sensor_type": "rssi", "sensor_name": "wifi", "value": -61, "unit": "dBm", "sender": "aa:bb:cc:00:00:01", "channel": 6, "freq": 2437},
  {"elapsed_ns": 123456790000, "ts": 1761580800000, "sensor_type": "hr", "sensor_name": "com.google.sensor.heart_rate", "value": 64, "unit": "bpm"}]}
```

`ts` in Millisekunden seit Epoch (Wanduhr der Uhr), `elapsed_ns` = `elapsedRealtimeNanos` des Samples, `clock_offset_ms` = Wanduhr minus Uhrenzeit des Batches. `sensor_type`: `rssi` (Pflicht: `sender`; `value` = RSSI in dBm; alle APs eines Scans tragen dasselbe `ts`), `temp` (Hauttemperatur, °C), `hr` (bpm), `offbody` (1 = am Handgelenk, 0 = abgelegt), `motion` (Bewegungsmaß, m/s² Abweichung von der Schwerkraft), `battery` (%), `charging` (1/0). Andere Werte: `400` mit `{"error": ...}`. `ts` mehr als 5 min in der Zukunft wird auf die Eingangszeit gesetzt.

Idempotenz: `(device, batch_id)` ist der Schlüssel (`batch_id` Zahl oder String). Ein bereits gespeicherter Batch wird mit `200` und `{"stored": 0, "duplicate": true, "batch_id": b}` angenommen und nicht erneut gespeichert. Haltbarkeit wie in Phase 1: die Zeilen sind auf der Platte (Flush mit fsync), bevor der Schlüssel vorgemerkt und `200` gesendet wird; bei Speicherfehler antwortet heatd `500`, ohne den Schlüssel vorzumerken, die Uhr sendet denselben Batch erneut. Antwort sonst `{"stored": n, "batch_id": b}`. Die Zeilen landen im Langformat in `watch_raw` (siehe `docs/data-foundation.md`).

### `POST /watch/sensors`

Sensorinventar der Uhr, beim Start und bei Änderung: `{"app_version": "1.4.0", "ts"?: ms, "sensors": [{"name"|"sensor_name", "type"|"android_type": 65555, "vendor", "resolution", "maxRange"|"max_range"}]}`. Antwort `{"stored": n}` nach Flush auf Platte, `400` bei leerer Liste, `500` bei Speicherfehler. Ziel: Tabelle `watch_sensors`.

### Raum-Matcher (`[watchmatch]` in `heatd.toml`)

Ein frischer Scan (Alter höchstens `scan_fresh_s`, Standard 180 s; über `/watch/measurement` oder `/api/scan`) wird mit dem ONNX-Modell aus `model_dir/<model_version>/` klassifiziert (`model_version = "latest"`: neuestes Verzeichnis nach `created` in `model.json`; stündliche Prüfung, Hot-Reload ohne Neustart). Pro Scan eine Zeile in `watch_inference` (`matcher=room`). Ausgabe nur bei `max_p >= tau_room` und Abstand zum Zweiten `>= delta_room`; sonst `abstained` mit Grund `low_confidence`, `low_margin`, `no_model`, `no_scan` (Minute ohne Scan) oder `unknown_state`. Die Anwesenheitsregeln nutzen den Matcher-Raum nur, wenn er nicht abstained; sonst gilt die letzte sichere Zuordnung bis `t_hold_min`, danach „unbekannt“ (kein Anwesenheits-Boost, Abwesenheitslogik unverändert). Ohne Modell oder bei ungültigem Modell: alles `no_model`, Anwesenheit wie vor Phase 1b (Vorhersage des alten Modells, Labels, Heim-WLAN), kein Absturz.

Zustand je Scan (Schwellen `[watchmatch.state]`: `hr_rest_max`, `hr_active_min`, `motion_active_min`, `sleep_hours`): `charging`, `offbody`, `sleep` (im Schlaffenster, HR-Mittel der letzten 10 min <= `hr_rest_max`, Bewegung < `motion_active_min`), `active` (HR >= `hr_active_min` oder Bewegung >= `motion_active_min`), sonst `rest`. Modell-Datei je Zustand aus `states`, Fallback `all`; `offbody`/`charging` ohne eigenen Eintrag: `unknown_state`.

Modellvertrag `models/watchroom/<model_version>/`: `model.json` (`model_version`, `created`, `matcher: "room"`, `rooms`, `senders` in Merkmalsreihenfolge, `sentinel_dbm: -100`, `scale {floor: -100, span: 70}`, `states {<zustand|all>: <onnx>}`, `tau_room`, `delta_room`, `t_hold_min`, `smoothing {kind: "hmm", stay_prob, transition {von: {nach: p}}}`, `metrics`, `train_window`, `n_labels`) und je Zustand eine ONNX-Datei: Eingabe `x` `float32 [N, len(senders)]` mit `x = clip((rssi - floor) / span, 0, 1)` (fehlender Sender = `sentinel_dbm`), Ausgabe `probabilities` `float32 [N, len(rooms)]` (Softmax-normiert, Reihenfolge `rooms`, ohne ZipMap). heatd glättet mit einem Vorwärtsschritt des HMM (pro Minute seit dem letzten Scan ein Übergangsschritt, Selbstübergang `stay_prob`). Testmodell: `crates/heatd/tests/fixtures/watchroom/v1/` (Generator `make_fixture.py` daneben).

## DuckDB (`heatd`)

```sql
CREATE TABLE scans (scan_id BIGINT, ts TIMESTAMP, received_at TIMESTAMP, device VARCHAR, label VARCHAR, bssid VARCHAR, rssi INTEGER, freq INTEGER);
CREATE TABLE readings (ts TIMESTAMP, node UBIGINT, endpoint USMALLINT, attribute VARCHAR, value DOUBLE);
CREATE TABLE setpoints (ts TIMESTAMP, node UBIGINT, room VARCHAR, value DOUBLE, reason VARCHAR);
CREATE TABLE predictions (ts TIMESTAMP, device VARCHAR, room VARCHAR, probability DOUBLE);
CREATE TABLE device_state (ts TIMESTAMP, received_at TIMESTAMP, device VARCHAR, motion VARCHAR, power VARCHAR, queued BOOLEAN, n_aps INTEGER, away BOOLEAN, id VARCHAR);
CREATE TABLE ingest_ids (device VARCHAR, id VARCHAR, PRIMARY KEY (device, id));
```

`scans` im Langformat: eine Zeile pro AP, `scan_id` gruppiert einen Scan, `ts` aus dem Body (UTC), `label` NULL außerhalb der Trainingsphase. `readings.attribute` ist der Name aus dem Thermostat-Cluster (`local_temperature`, `occupied_heating_setpoint`, `system_mode`, …), Temperaturen in °C. Python liest nie die Live-Datei, sondern den Snapshot, den `POST /api/snapshot` per `COPY FROM DATABASE` nach `data/snapshot.duckdb` schreibt.
