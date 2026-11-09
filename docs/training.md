# Uhr und Training

HeatScan-App, Etiketten, Raum-Matcher und thermisches Modell. Überblick: [README](../README.md).

## HeatScan-App

```bash
sh wear/build.sh                                         # Tests + signierte Release-APK (Docker)
adb connect <uhr-ip>:<port> && adb install -r wear/app/build/outputs/apk/release/app-release.apk
```

Die APK liegt auch im GitHub-Release. In der App erlaubst du **genauen Standort** (für WLAN-Scans) und **Geräte in der Nähe / LAN-Zugriff** (Android 17, sonst erreicht die Uhr den Minix nicht). Dann prüfst du die Server-URL (`http://192.168.178.29:8090`; auf dem Minix belegt gluetun 8080), tippst auf Start und fügst die Kachel **HeatScan** hinzu. Die Statuszeile meldet Probleme direkt, etwa „Standort aus“, „WLAN aus“ oder „LAN-Zugriff fehlt“.

Ab Version 2.4 misst die Uhr zusätzlich Hauttemperatur, Puls, Bewegung (Varianz), Trageerkennung und Akku und schickt sie gebündelt (alle 5 min oder 50 Samples) an `POST /watch/measurement`, zusammen mit dem nächsten Scan-Post (kein eigener Flush). Ab 2.5 läuft der Puls-Sensor nur kurz (höchstens 20 s, erster gültiger Wert) und die Beschleunigung 10 s in ohnehin stattfindenden Wachphasen (Scan-Takt, Bildschirm an); Hauttemperatur und Trageerkennung bleiben hardware-gebatcht. Scans gehen wieder gebündelt an `/api/scans`. Auf Wear OS 6 fragt die App nach `READ_HEART_RATE`/`READ_SKIN_TEMPERATURE` in einem ersten und `READ_HEALTH_DATA_IN_BACKGROUND` in einem zweiten Schritt; der Dienst läuft zusätzlich mit Typ `health`, sobald eine Gesundheitsberechtigung erteilt ist (Statuszeile `bg=ok|fehlt`). Dafür auf der Uhr unter Einstellungen > Apps > HeatScan > Berechtigungen **Körpersensoren** (Puls, `READ_HEART_RATE`) und **Hauttemperatur** (`READ_SKIN_TEMPERATURE`) erlauben (Wear OS 6, SDK 36 und höher; die App fragt beim Start). Fehlt eine Berechtigung oder der Sensor, wird nur dieser Kanal übersprungen; die Statuszeile zeigt es unter `sensors=` (z. B. `hr=ok,skin=Recht verweigert`). Antwortet ein älteres heatd mit 404/405, bleiben die Sensor-Batches in der Warteschlange und gehen nach einem Update ein.

**Training:**
1. Die Uhr fragt per Benachrichtigung „Wo bist du?“ (5 Raumknöpfe, Vermutung zuerst). Ein Tipp etikettiert die Scans ab der Frage und startet einen Aufenthalt: weitere Scans im selben Raum werden ohne Rückfrage etikettiert (höchstens 2 h). Wechselst du den Raum (Bewegung, danach zwei Scans mit anderem WLAN-Fingerabdruck oder abweichender Vorhersage), fragt die Uhr einmal mit Vibration „Raum gewechselt? Wo bist du jetzt?“ und etikettiert rückwirkend ab dem ersten abweichenden Scan. Vibration höchstens alle 3 min, nachts (22–7 Uhr), unterwegs und ohne Uhr am Handgelenk nur lautlos. Zusätzlich geht Antippen in der App; es entstehen keine zusätzlichen Scans. Ab Version 4.0 (Material 3 Expressive, `wear/`): Die Kachel zeigt je Raum Ist-/Solltemperatur, offene Fenster und den ersten Alarm (Stand aus `GET /api/state`, geladen nur beim Öffnen von App oder Kachel) und öffnet die App. Eine `window_prompt` erzeugt eine Benachrichtigung „Fenster offen?“ mit einer Vibration und den Aktionen Ja (Primäraktion, auch per Double Pinch), Nein und Weiß nicht; jede Antwort, auch Weiß nicht, Wischen und Ablauf nach 20 min (ohne zusätzlichen Weckruf erst beim nächsten Antwort-Eingang oder App-Start erkannt, mit dem Ablaufzeitpunkt als `ts`), geht als `POST /watch/window_label` mit `answer` (`yes`/`no`/`unknown`/`timeout`) und `via` in die Warteschlange. In der App meldet „Auf“/„Zu“ ein Fenster selbst (derselbe Endpunkt ohne `prompt_id`, wirkt wie `POST /api/window`). Alarme aus `alerts` kommen je `id` genau einmal als Benachrichtigung und stehen als Liste auf dem Startbildschirm. „Boost“ ruft `POST /api/boost` (5 min) und zeigt Entscheidung und Grund des Servers, der Zustand je Raum kommt aus `GET /api/boost`. Ab Version 4.3 stellt die Raumseite eine Temperatur von Hand vor: „–“/„+“ in 0,5-°C-Schritten, Dauer (1 Std., 2 Std., 4 Std., bis morgen 6 Uhr) und „setzen“ rufen `POST /api/setpoint` (Quelle `watch`); die aktive Vorgabe steht als „22 °C bis 18:40“ mit „Vorgabe beenden“ auf der Seite.
2. Snapshot ziehen und trainieren:
   ```bash
   curl -X POST http://localhost:8080/api/snapshot       # data/snapshot.duckdb
   cd ml && uv run python -m heatml train                # models/room_model.onnx + .json
   sudo systemctl restart heatd                          # Docker: docker compose restart heatd
   ```
3. Ab jetzt liefert `POST /api/scan` den Raum, und die Regeln heizen dort, wo du bist.

Feature-Auswahl: Nur stabile BSSIDs kommen ins Modell, also solche, die in genügend Scans eines Raums sichtbar sind und nicht ständig auftauchen und verschwinden. Fehlende BSSIDs werden maskiert. Schwellen stehen in `CONFIG` in `ml/src/heatml/__main__.py`.

## Raum-Abgleich trainieren

Das Training liest nur Parquet aus dem Datenlake (`<data_root>/parquet`, nie `raw/`) und schreibt ein versioniertes Modell nach `<models_dir>/<version>/` (`model.json` plus ein ONNX je Zustand, `all` ist der Fallback). heatd lädt es über `[watchmatch] model_dir`.

```bash
cd training
uv run python -m heattrain watchroom /data /models/watchroom    # trainieren + exportieren (Hold-out-Auswertung in model.json und eval/*.md)
uv run python -m heattrain eval /data /models/watchroom/<version>   # Modell auf Tagen nach dem Trainingsfenster prüfen
uv run pytest
```

Ablauf: gelabelte WLAN-Scans (Fenster-Labels mit `from_ts`/`to_ts` und Punkt-Labels) werden je Scan gruppiert, mit derselben Regel wie heatd (`state_rule` in `model.json`) einem Zustand zugeordnet (rest, sleep, active, offbody, charging; ohne Puls und Bewegung `rest`; `offbody` und `charging` werden nicht trainiert), kNN gegen Gradient Boosting per Tages-Block-CV gewählt und kalibriert. τ und δ maximieren Macro-F1 bei Coverage ≥ 0,8, abgestimmt auf dem geglätteten Belief (derselbe HMM-Schritt wie in heatd); die HMM-Übergänge kommen aus den `links` in `meta` (Wand/offen bevorzugt, ohne Meta gleichverteilt). Beim Export wird ONNX gegen sklearn geprüft (≤ 1e-5). Abweichung: HistGradientBoosting lässt sich mit skl2onnx 1.20 und onnx 1.23 nicht konvertieren, daher klassisches GradientBoosting.

Auf der heatd-Seite aktivierst du den Abgleich in `heatd.toml` (Docker: `deploy/docker/heatd.docker.toml`, `/models` ist ein Volume):

```toml
[watchmatch]
enabled = true
model_dir = "/models/watchroom"   # darin <version>/model.json + ONNX
model_version = "latest"          # oder fester Versionsname; "latest" prüft stündlich auf ein neueres Modell
scan_fresh_s = 180

[watchmatch.state]                # Zustandsregeln, gleiche Werte wie im Training (CONFIG in training/src/heattrain/__main__.py)
hr_rest_max = 75
hr_active_min = 100
motion_active_min = 0.5
sleep_hours = "23-7"
```

Ohne Modell schreibt heatd `no_model`-Zeilen in `watch_inference` und heizt wie vorher (kein Absturz). Beobachten: `/health` (Block `watch`), `/metrics` (`heatd_watch_*`) und der Tagesbericht (Abschnitt Watch, Event `matcher_drift`). Ablauf: Uhr sammelt Etiketten und WLAN, `heattrain watchroom` trainiert, Modellordner nach `/models/watchroom/<version>` legen, heatd lädt ohne Neustart.

Ende-zu-Ende-Fixture ohne Hardware: `cd training && PYTHONPATH=tests uv run python tests/make_heatd_fixture.py` erzeugt synthetische Daten, trainiert mit der echten Pipeline und schreibt das Modell nach `crates/heatd/tests/fixtures/watchroom_trained/`; der heatd-Test lädt es, prüft `watch_inference`-Zeilen und den `no_model`-Pfad nach Entfernen des Modellordners.

Nächtliches Neutraining auf dem Minix: `deploy/train/heattrain.timer` (03:30, Container `deploy/train/Dockerfile`, trainiert thermisches Modell, Raum-Matcher und Fenster-Modell, der Boost-Trainer läuft über `heattrain-boost.timer`); alternativ vom PC aus mit `deploy/pc-train/nightly.sh`.

## Thermisches Raummodell

Je Thermostat ein Grey-Box-Modell (Radiator `T_r`, Raumluft `T_a`, Sensor `y = w*T_r + (1-w)*T_a`): Der Heizkörper wird vom Ventil `u` (PIHeatingDemand, Werte über 100 gelten als fehlend) mit effektiver Vorlauftemperatur `T_s` geheizt, die Luft koppelt an Radiator, Außentemperatur, Wind, Strahlung und Nachbarräume (`links` aus `meta`). Fit per Kalman-Filter-Likelihood (scipy, Parameter positiv und beschränkt, 1-Minuten-Raster); Parameter-Unsicherheit aus der Jacobi-Matrix, Fallback auf 1 Zustand, wenn 2 Zustände nicht identifizierbar oder auf dem Hold-out-Block nicht besser sind.

```bash
cd training
uv run python -m heattrain thermal /data /models        # <data_root> (parquet/) oder Roh-Ordner mit *.arrows
```

Ausgabe: `<models_dir>/thermal/<datum>/<raum>.json` (Parameter, Grenzen, relative Standardabweichung, Zeitkonstanten, RMSE auf dem Hold-out-Block gegen Persistenz, Datenspanne, Warnungen). Python: `estimate_room_air` liefert die gefilterte Raumluft samt Unsicherheit, `predict` und `ThermalModel.lead_time` rechnen Vorlaufzeiten für geplante Ventilstellung oder Solltemperatur. Die Quelle `legacy` wird ausgeschlossen (Config `exclude_sources`). Mit weniger als etwa einem Tag Daten sind die Parameter schwach bestimmt; die Warnungen im JSON sagen, welche.

