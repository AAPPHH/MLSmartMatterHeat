# Datenvertrag Uhr ↔ heatd ↔ ML

## `POST /api/scan`

Body wie `scan_example.json`: `ts` Millisekunden seit Epoch (Uhrzeit der Uhr), `device` frei wählbarer Name, `label` Raumlabel während der Trainingsphase oder `null`, `aps` alle sichtbaren WLANs (`bssid` klein geschrieben mit `:`, `rssi` dBm, `freq` MHz, 2,4 und 5 GHz ungefiltert). Antwort `200` mit `{"stored": <anzahl aps>, "room": <vorhergesagter raum oder null>}`.

Optionale Felder (abwärtskompatibel): `motion` (`"moving"` | `"still"` | `null`), `queued` (`true`, wenn der Scan später aus dem Puffer hochgeladen wird; `ts` bleibt der Originalzeitpunkt), `power` (`"normal"` | `"saver"` | `"offbody"` | `"doze"` | `null`), `away` (`true`, wenn das Heim-WLAN nicht sichtbar ist). `aps` darf leer sein oder fehlen: das ist ein Heartbeat (`heartbeat_example.json`); er erzeugt keine Vorhersage, Antwort `{"stored": 0, "room": null}`. Unbekannte Werte für `motion`/`power` werden mit `422` abgelehnt.

### Idempotenz (`id`)

Optionales Feld `id` (String) bei Scans und Heartbeats, einzeln und im Batch: eindeutig pro Gerät und stabil über Wiederholungen, von der Uhr beim Einreihen als `<device>-<persistierter monotoner Zähler>` erzeugt (z. B. `pixel-watch-000123`, unabhängig vom Boot). `(device, id)` ist der Idempotenzschlüssel: eine bereits gesehene `id` wird mit `200` angenommen, aber nicht erneut gespeichert (weder `scans`, `device_state` noch `predictions`). Einzelantwort dann `{"stored": 0, "room": ..., "duplicate": true}`; `duplicate` fehlt sonst. Einträge ohne `id` verhalten sich wie bisher. Jeder Eintrag wird atomar in einer Transaktion gespeichert, ein Batch darf deshalb komplett wiederholt werden.

### Zeitstempel in der Zukunft

Liegt `ts` mehr als 5 Minuten nach der Eingangszeit (Uhrenabweichung), setzt heatd `ts` auf `received_at` und protokolliert es.

## `POST /api/scans`

Batch für gepufferte Scans: `{"scans": [...]}`, Elemente wie bei `/api/scan` (`batch_example.json`). Antwort `200` mit `{"stored": <aps gesamt>, "accepted": <anzahl scans>, "duplicates": <anzahl wiederholter ids>, "room": <raum der Vorhersage mit dem größten ts im Batch oder null>}`. Die Reihenfolge der Elemente ist beliebig.

## Präsenz

Jeder Scan und Heartbeat schreibt eine Zeile in `device_state`. Vorhersagen tragen den Scan-`ts` (nicht die Eingangszeit), spät hochgeladene Scans werden also nie "frisch". Der belegte Raum ist die Vorhersage mit dem größten `ts` des Geräts und gilt, solange (i) der jüngste `device_state` (größter `ts`) `motion = still` meldet und der letzte Kontakt (`received_at`) jünger als `still_contact_timeout_minutes` (180) ist oder (ii) die Vorhersage jünger als `presence_timeout_minutes` (30) ist. Ist `power` `saver`/`offbody` oder der Kontakt älter als `still_contact_timeout_minutes`, ist die Präsenz unbekannt: Zeitplan ohne Boost, kein Eco. Eco gilt nur, wenn der jüngste Zustand `away = true` meldet.

## Feature-Vektor

Feste BSSID-Reihenfolge `bssids` aus dem Training. Für BSSID `i`: `x_i = clamp((rssi - rssi_floor) / rssi_span, 0, 1)` falls sichtbar, sonst `0`; Maske `m_i = 1` falls sichtbar, sonst `0`. Eingabe `float32 [1, 2F] = concat(x, m)`. Unbekannte BSSIDs werden ignoriert. Beispiel: `features_example.json` angewendet auf `scan_example.json`.

## Modell

`room_model.onnx` + Sidecar `room_model.json`: `{"version": 1, "input_name": "input", "bssids": [...], "labels": [...], "rssi_floor": -100.0, "rssi_span": 70.0}`. Ausgaben: `label` (`int64 [N]`, Index in `labels`) und `probabilities` (`float32 [N, C]`), ohne ZipMap. Testmodell: `crates/heatd/tests/fixtures/room_model.{onnx,json}`, sagt für `scan_example.json` `wohnzimmer` voraus.

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
