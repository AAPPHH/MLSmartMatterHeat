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

Nimmt zwei Bodyformen an (Unterscheidung: Feld `samples` vorhanden oder nicht).

1. Phase-1-Form (unverändert): `{"device", "ts" (ms), "device_ts"?, "id"?, "batch_id"?, "measurements"|"aps": [{"sender"|"bssid", "rssi", "channel"?, "freq"?}]}`. Idempotenz über `id`. Antwort `{"stored": n, "batch_id": b}`, bei Wiederholung `{"stored": 0, "duplicate": true}`.
2. Langformat (Phase 1b), Body wie `measurement_example.json`: `{"device","batch_id","sent_ts","clock_offset_ms","app_version","samples":[...]}`.

```json
{"device": "pixel-watch", "batch_id": 1761580800001, "sent_ts": 1761580805000, "clock_offset_ms": -120, "app_version": "2.5",
 "samples": [
  {"elapsed_ns": 123456789000, "ts": 1761580800000, "sensor_type": "rssi", "sensor_name": "wifi", "value": -61, "unit": "dBm", "sender": "aa:bb:cc:00:00:01", "channel": 6, "freq": 2437},
  {"elapsed_ns": 123456790000, "ts": 1761580800000, "sensor_type": "hr", "sensor_name": "com.google.sensor.heart_rate", "value": 64, "unit": "bpm"}]}
```

`batch_id` ist eine Zahl (i64) oder ein String, stabil über Wiederholungen; `(device, batch_id)` ist der Idempotenzschlüssel. `sent_ts` ist die Uhrzeit (Epoch ms) beim Bilden des Batches, `clock_offset_ms = Wanduhr_ms - elapsedRealtime_ms` zum selben Zeitpunkt; damit gilt für jedes Sample `ts ≈ elapsed_ns / 1e6 + clock_offset_ms`. Optional: `battery_pct_per_h` (Akkuverbrauch der Uhr in Prozentpunkten pro Stunde, gleitend über mindestens 1 h ohne Laden, fehlt sonst).

Jedes Sample: `elapsed_ns` (`SensorEvent.timestamp` bzw. bei WLAN-Scans `ScanResult.timestamp`; fehlt bei alten Einträgen), `ts` (Epoch ms; mehr als 5 min in der Zukunft wird auf die Eingangszeit gesetzt), `sensor_type`, `sensor_name`, `value`, `unit`; optional `sender` (BSSID klein), `channel`, `freq` (MHz). Unbekannter `sensor_type`, `rssi` ohne `sender` oder nicht endlicher `value`: `400` mit `{"error": ...}`.

| sensor_type | Quelle | unit | Takt |
|---|---|---|---|
| `temp` | Hauttemperatur (`com.google.sensor.skin_temperature`, Typ 65555) | `C` | 60 s, Hardware-Batching (nicht weckend) |
| `hr` | Herzfrequenz, ein Wert je Weckfenster (Sensor höchstens 20 s registriert, erster gültiger Wert) | `bpm` | höchstens 1/60 s, nur bei ohnehin stattfindenden Wachphasen (Scan-Takt, Bildschirm an) |
| `motion` | Varianz des Betrags der Beschleunigung über ein 10-s-Fenster (Accelerometer nur im Weckfenster) | `m2/s4` | höchstens 1/60 s, wie `hr` |
| `offbody` | Low-Latency-Off-Body, Rohwert von Android: `1` = Uhr am Handgelenk, `0` = abgelegt | `bool` | bei Änderung |
| `battery` | Akkustand | `%` | je Batch und bei Lade-Wechsel |
| `charging` | Ladeflag (`1` = lädt) | `bool` | je Batch und bei Lade-Wechsel |
| `rssi` | WLAN-Scan, eine Zeile je Access Point (`sender`, `freq`, `value` = RSSI; alle APs eines Scans tragen dasselbe `ts`) | `dBm` | ereignisgesteuert wie bisher |

Die Zeilen landen im Langformat in `watch_raw` (siehe `docs/data-foundation.md`).

### Scan-Form (WLAN-Batch mit Scan-Feldern)

Trägt ein Langformat-Body `rssi`-Samples und mindestens eines der Felder `id`, `label`, `motion`, `power`, `away`, `bedtime`, `queued` (Bedeutung wie bei `/api/scan`), behandelt heatd ihn als Scan und durchläuft den vollständigen Phase-1-Pfad (derselbe Code wie `/api/scan`, nur ohne zweite `watch_raw`-Schreibung): `scans`/`device_state`/`predictions` (Idempotenz zusätzlich über `id`), Präsenz und Away-Übergänge, Label (`label` schreibt das Stay-Label), Vorhersage des alten Modells als Rückfallebene, Raum-Matcher. Die Antwort enthält zusätzlich `room`, `probability`, `rooms`, `aps` (Attribution wie bei `/api/scan`) und bei einer bereits gesehenen `id` `duplicate: true`. `ts` des Scans ist das größte `ts` der `rssi`-Samples. Die Uhr sendet Scans selbst über `/api/scan(s)` (ein Request je Rückstau); die Scan-Form ist für Clients gedacht, die nur einen Endpunkt sprechen (`MeasureBatch.fromScan`).

### Haltbarkeit, Warteschlange, Antwort

Die Zeilen sind auf der Platte (Flush mit fsync), bevor der Schlüssel vorgemerkt und `200` gesendet wird; bei Speicherfehler antwortet heatd `500`, ohne den Schlüssel vorzumerken, die Uhr sendet denselben Batch erneut. Antwort `200` mit `{"stored": <anzahl>, "batch_id": <id>}`, bei Wiederholung `{"stored": 0, "duplicate": true, "batch_id": <id>}`. Die Uhr schließt Batches nach 5 Minuten oder 50 Samples, beim Raum-Tipp sofort; sie liegen in der dauerhaften Warteschlange (kein Verlust bei Netzfehler oder Neustart; nur der noch offene Batch, höchstens 5 Minuten, geht bei einem harten Absturz verloren) und werden mit dem nächsten Scan-Post gesendet, ohne eigenen Flush. Einträge für `/watch/measurement` und `/watch/sensors` bleiben bei `404/405` in der Warteschlange (zurückgestellt) und gehen nach einem Server-Update ein; `400/422` verwirft den Eintrag.

## `POST /watch/sensors`

Sensorinventar beim Start und bei Änderung (Sensorliste oder `app_version`): `{"device","ts"?: ms,"app_version","sensors":[{"name"|"sensor_name","android_type"|"type","string_type","vendor","resolution","max_range"|"maxRange"}]}` (Tabelle `watch_sensors`). Die Uhr sendet zusätzlich `wake_up`, `min_delay_us`, `max_delay_us`, `fifo_max`, `reporting_mode`; heatd ignoriert sie bisher. Antwort `{"stored": n}` nach Flush auf Platte, `400` bei leerer Liste, `500` bei Speicherfehler.

## `POST /watch/label`

`{"device","ts","from","to","room","source":"watch"}` mit den Regeln von `/api/label` (`room` entspricht `label`). Die Uhr fällt bei `400/404/405/422` auf `/api/label` zurück.

## `POST /watch/window_label`

Antwort der Uhr auf eine Fensterfrage (`window_prompt`) oder manueller Eintrag: `{"device","ts","room","prompt_id"?,"label_id"?,"answer"?,"open"?,"via"?}` (Alias `/api/window_label`). `answer` ist `yes`, `no`, `unknown`, `timeout` oder `voided`, `via` eines aus `notification`, `page`, `self`, `swipe`, `expiry`, `replaced`, `undo`; beides wird in der Label-Zeile (Quelle `window`, Spalten `answer`, `via`) gespeichert. Alte Clients senden nur `open`: ohne `answer` gilt `open` true/false als `yes`/`no` (altes Verhalten), ohne beides `400`; widerspricht `open` einem `yes`/`no`, `400`; unbekannter `answer` `422`, unbekanntes `via` `400`. Nur `yes` und `no` setzen `window_open`, ändern den Fensterzustand (manuell: öffnen/schließen, mit `prompt_id`: `no` schließt ein automatisch geöffnetes Fenster) und zählen im `window_episode` als Bestätigung bzw. Verneinung. `unknown` und `timeout` werden mit `window_open` null gespeichert, beenden nur die offene Frage und bestätigen nie; `open` ist dort optional. `voided` nimmt die früheren Antworten derselben `prompt_id` (und das eigene `label_id`) zurück wie `/watch/label/void` (je eine `void`-Zeile, manuelles Öffnen wird rückgängig gemacht, Episoden-Abgleich ohne diese Antwort); es braucht `prompt_id` oder `label_id`. Wiederholung desselben `label_id` je Gerät: `{"stored": 0, "duplicate": true}`. Antwort sonst `{"stored": 1, "answer", "manual", "window_open": [Räume]}`.

## `POST /api/label`

Nachträgliches Etikettieren (`label_example.json`): `{"device": <string>, "from": <epoch ms>, "to": <epoch ms>, "label": <string>}`. `label` muss `^[a-z_]{1,32}$` entsprechen, `from <= to` und `to - from <= 3600000`, sonst `400` mit `{"error": ...}`. heatd setzt `scans.label = label` für alle Zeilen des Geräts mit `ts` in `[from, to]` (beide Grenzen inklusive, `ts` wie bei `/api/scan` als Millisekunden nach TIMESTAMP gewandelt). Überschreiben ist gewollt: die letzte Korrektur gewinnt. Antwort `200` mit `{"labelled": <anzahl verschiedener scan_id>}`; idempotent. Raum-IDs der Uhr bleiben `kueche`, `wohnzimmer`, `flur`, `bad`, `schlafzimmer` und `keiner`. `keiner` bedeutet „zu Hause, aber in keinem beheizten Raum“ (z. B. Terrasse): ein gültiges Label und Stay-Label (Scans tragen `label = "keiner"`), im Training eine reguläre Klasse. heatd gibt dafür keinem Raum `occupied_setpoint` (alle Räume folgen dem Zeitplan), zählt es aber nicht als abwesend: Eco gilt weiterhin nur bei `away = true`. Die Wahrscheinlichkeitsliste der Uhr (`rooms`) und die orange Vermutung zeigen `keiner` nie als Quadrant; in der „Wo bist du?“-Benachrichtigung, auf der Raumseite und in der Kachel ist „Keiner“ immer die letzte, getrennte Auswahl (Bestätigung „✓ Gespeichert: Kein Raum“). Der Trainings-HMM behandelt `keiner` standardmäßig als benachbart zu `flur` und `wohnzimmer` (`hmm.default_links` in der Trainingskonfiguration).

Wie die Uhr etikettiert: Scans tragen `label` nur, solange ein bestätigter Aufenthalt („Stay“) läuft, sonst `null`. Ein Tipp auf einen der fünf Raumknöpfe (oder „Keiner“) der „Wo bist du?“-Benachrichtigung (oder in App/Kachel) erzeugt ein Label-Ereignis `[Zeit des Scans, der gefragt hat, jetzt]` und startet einen Stay mit dem WLAN-Fingerabdruck dieses Scans (gemittelt über bis zu 5 Scans). Danach werden Scans ohne Rückfrage direkt mit dem Stay-Raum etikettiert, höchstens 2 h lang. Raumwechsel erkennt die Uhr, wenn nach Bewegung zwei Scans in Folge vom Fingerabdruck abweichen (mittlere Differenz > 8 dB über die APs ab −85 dBm, fehlend = −100 dBm, oder Überlappung der AP-Mengen < 0,5) oder die Server-Vorhersage (`probability` >= 0,6) zweimal hintereinander vom Stay-Raum abweicht. Dann endet der Stay, die Uhr fragt mit Vibration „Raum gewechselt? Wo bist du jetzt?“, und die Antwort etikettiert ab dem ersten abweichenden Scan. Vibration höchstens alle 3 min, nie zwischen 22 und 7 Uhr, unterwegs oder ohne Uhr am Handgelenk. Antwortet der Server auf `/api/label` mit `404` (altes Image), bleibt das Ereignis in der Warteschlange und wird beim nächsten Flush erneut gesendet.

## Präsenz

Jeder Scan und Heartbeat schreibt eine Zeile in `device_state`. Vorhersagen tragen den Scan-`ts` (nicht die Eingangszeit), spät hochgeladene Scans werden also nie "frisch". Der belegte Raum ist die Vorhersage mit dem größten `ts` des Geräts und gilt, solange (i) der jüngste `device_state` (größter `ts`) `motion = still` meldet und der letzte Kontakt (`received_at`) jünger als `still_contact_timeout_minutes` (180) ist oder (ii) die Vorhersage jünger als `presence_timeout_minutes` (30) ist. Fehlt ein sicherer Matcher-Raum (oder die Vorhersage ist älter als `presence_timeout_minutes`), gilt das Stay-Label des Scans, wenn es jünger als `presence_timeout_minutes` ist und der Stay nicht durch einen unetikettierten Scan oder eine `moving`-Meldung danach beendet wurde. Optional trägt jeder Scan, Heartbeat und Langformat-Body `bedtime` (bool, Bettzeit-Modus der Uhr; fehlt, wenn nicht lesbar); `true` zählt wie der Zustand `sleep`, das `sleep_room` behält dann seinen Zeitplanwert statt `occupied_setpoint`. Ist `power` `saver`/`offbody` oder der Kontakt älter als `still_contact_timeout_minutes`, ist die Präsenz unbekannt: Zeitplan ohne `occupied_setpoint`, kein Eco. Eco gilt nur, wenn der jüngste Zustand `away = true` meldet.

## Feature-Vektor

Feste BSSID-Reihenfolge `bssids` aus dem Training. Für BSSID `i`: `x_i = clamp((rssi - rssi_floor) / rssi_span, 0, 1)` falls sichtbar, sonst `0`; Maske `m_i = 1` falls sichtbar, sonst `0`. Eingabe `float32 [1, 2F] = concat(x, m)`. Unbekannte BSSIDs werden ignoriert. Beispiel: `features_example.json` angewendet auf `scan_example.json`.

## Modell

`room_model.onnx` + Sidecar `room_model.json`: `{"version": 1, "input_name": "input", "bssids": [...], "labels": [...], "rssi_floor": -100.0, "rssi_span": 70.0}`. Ausgaben: `label` (`int64 [N]`, Index in `labels`) und `probabilities` (`float32 [N, C]`), ohne ZipMap. Testmodell: `crates/heatd/tests/fixtures/room_model.{onnx,json}`, sagt für `scan_example.json` `wohnzimmer` voraus.

## Phase 1b: Raum-Matcher

### Raum-Matcher (`[watchmatch]` in `heatd.toml`)

Ein frischer Scan (Alter höchstens `scan_fresh_s`, Standard 180 s; über `/watch/measurement` oder `/api/scan`) wird mit dem ONNX-Modell aus `model_dir/<model_version>/` klassifiziert (`model_version = "latest"`: neuestes Verzeichnis nach `created` in `model.json`; stündliche Prüfung, Hot-Reload ohne Neustart). Pro Scan eine Zeile in `watch_inference` (`matcher=room`). Ausgabe nur bei `max_p >= tau_room` und Abstand zum Zweiten `>= delta_room`; sonst `abstained` mit Grund `low_confidence`, `low_margin`, `no_model`, `no_scan` (Minute ohne Scan), `no_known_ap` oder `unknown_state`. Die Anwesenheitsregeln nutzen den Matcher-Raum nur, wenn er nicht abstained; sonst gilt die letzte sichere Zuordnung bis `t_hold_min`, danach „unbekannt“ (kein Anwesenheits-Boost, Abwesenheitslogik unverändert). Ohne Modell oder bei ungültigem Modell: alles `no_model`, Anwesenheit wie vor Phase 1b (Vorhersage des alten Modells, Labels, Heim-WLAN; läuft über denselben Scan-Pfad), kein Absturz.

Zustand je Scan, eine Regel für heatd und Training: Schwellen und Fenster stehen als `state_rule` in `model.json` (`hr_rest_max`, `hr_active_min`, `motion_active_min`, `sleep_hours` [von, bis] in vollen Stunden, `window_s` = 600, `stale_s` = 600); fehlt `state_rule`, gelten die Werte aus `[watchmatch.state]` (Standard 70/90/0,5, 23-7). Reihenfolge: `charging` (letzter Ladeflag-Wert jünger als `stale_s` und 1), `offbody` (letzter Off-Body-Wert jünger als `stale_s` und 0; Scans tragen `power = offbody` als frisches Signal), `rest` (weder HR- noch Bewegungswert in den letzten `window_s`), `sleep` (im Schlaffenster, HR-Mittel <= `hr_rest_max`, Bewegung < `motion_active_min`), `active` (HR-Mittel >= `hr_active_min` oder Bewegung >= `motion_active_min`), sonst `rest`. Ältere Ladeflag- und Off-Body-Werte zählen als unbekannt (am Handgelenk, nicht ladend). Modell-Datei je Zustand aus `states`, Fallback `all`. `offbody` und `charging` bekommen nie ein Template (das Training schließt sie aus): `unknown_state`. Enthält ein Scan keinen Sender aus `senders`, ist die Ausgabe `no_known_ap` (Training und heatd identisch), ohne die Glättung zu verändern.

Modellvertrag `models/watchroom/<model_version>/`: `model.json` (`model_version`, `created`, `matcher: "room"`, `rooms`, `senders` in Merkmalsreihenfolge, `sentinel_dbm: -100`, `scale {floor: -100, span: 70}`, `states {<zustand|all>: <onnx>}`, `tau_room`, `delta_room`, `t_hold_min`, `smoothing {kind: "hmm", stay_prob, transition {von: {nach: p}}}`, `metrics`, `train_window`, `n_labels`) und je Zustand eine ONNX-Datei: Eingabe `x` `float32 [N, len(senders)]` mit `x = clip((rssi - floor) / span, 0, 1)` (fehlender Sender = `sentinel_dbm`), Ausgabe `probabilities` `float32 [N, len(rooms)]` (Softmax-normiert, Reihenfolge `rooms`, ohne ZipMap). heatd glättet mit einem Vorwärtsschritt des HMM (pro Minute seit dem letzten Scan ein Übergangsschritt, höchstens 60, danach Neustart mit gleichverteiltem Prior, Selbstübergang `stay_prob`, Belief mindestens 1e-4); die Auswertung im Training rechnet denselben Schritt, und `tau_room`/`delta_room` werden auf dem geglätteten Belief abgestimmt. Testmodell: `crates/heatd/tests/fixtures/watchroom/v1/` (Generator `make_fixture.py` daneben).

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
