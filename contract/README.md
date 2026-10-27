# Datenvertrag Uhr ↔ heatd ↔ ML

## `POST /api/scan`

Body wie `scan_example.json`: `ts` Millisekunden seit Epoch (Uhrzeit der Uhr), `device` frei wählbarer Name, `label` Raumlabel während der Trainingsphase oder `null`, `aps` alle sichtbaren WLANs (`bssid` klein geschrieben mit `:`, `rssi` dBm, `freq` MHz, 2,4 und 5 GHz ungefiltert). Antwort `200` mit `{"stored": <anzahl aps>, "room": <vorhergesagter raum oder null>}`.

## Feature-Vektor

Feste BSSID-Reihenfolge `bssids` aus dem Training. Für BSSID `i`: `x_i = clamp((rssi - rssi_floor) / rssi_span, 0, 1)` falls sichtbar, sonst `0`; Maske `m_i = 1` falls sichtbar, sonst `0`. Eingabe `float32 [1, 2F] = concat(x, m)`. Unbekannte BSSIDs werden ignoriert. Beispiel: `features_example.json` angewendet auf `scan_example.json`.

## Modell

`room_model.onnx` + Sidecar `room_model.json`: `{"version": 1, "input_name": "input", "bssids": [...], "labels": [...], "rssi_floor": -100.0, "rssi_span": 70.0}`. Ausgaben: `label` (`int64 [N]`, Index in `labels`) und `probabilities` (`float32 [N, C]`), ohne ZipMap. Testmodell: `crates/heatd/tests/fixtures/room_model.{onnx,json}`, sagt für `scan_example.json` `wohnzimmer` voraus.
