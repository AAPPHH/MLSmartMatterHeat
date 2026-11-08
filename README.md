# 🔥 MLSmartMatterHeat

**Heizung, die weiß, in welchem Raum du bist.** Ohne Cloud, ohne Hersteller-App, alles im eigenen LAN.

Die Pixel Watch misst alle 30 s das WLAN-Umfeld. Ein kleines ML-Modell erkennt daraus den Raum. Ein Rust-Dienst auf dem Minix kombiniert das mit Uhrzeit und Zeitplänen und stellt die Matter-Thermostate über Thread.

```
 ⌚ Pixel Watch ──WLAN-Scan (BSSID+RSSI)──▶ 🖥 Minix Z300 · heatd (Rust)
                                              │  DuckDB · Regeln · ONNX
                                              │  chip-tool (Matter)
                                              ▼
                    🏠 Heimnetz (IPv6) ◀──▶ 📡 ESP Thread Border Router
                                              │  ESP32-S3 + ESP32-H2 (RCP)
                                              ▼  Thread 802.15.4
                                   🌡 Comet Zero  🌡 Comet Zero  🌡 Comet Zero
```

| Teil | Wo | Was |
|---|---|---|
| `firmware/` | ESP Thread BR Board v1.2 | esp-thread-br `basic_thread_border_router`, WLAN- oder Ethernet-Backbone, RCP-Image wird beim Boot automatisch auf den H2 geflasht |
| `crates/matter-controller` | Rust | Trait `MatterController`: Commissioning BLE→Thread, Thermostat lesen/schreiben, Subscriptions. Backends: `ChipToolController` (echt) und `MockController` (Tests) |
| `crates/heatd` | Rust, Minix | HTTP-API für die Uhr, DuckDB-Zeitreihen, Regelwerk, ONNX-Inferenz (`ort`), CLI für Commissioning und Thread |
| `wear/` | Kotlin, Pixel Watch | App **HeatScan**: Foreground-Service scannt alle 30 s, Material-3-Oberfläche (Raum-Labeln, Fensterfrage, Boost, Alarme) und Kachel mit Temperaturen |
| `ml/` | Python (uv) | `heatml`: Raumklassifikator auf dem RSSI-Vektor → ONNX, Platz für das Fenster-offen-Modell |
| `contract/` | – | Gemeinsamer Datenvertrag: Scan-JSON, Feature-Vektor, DuckDB-Schema |

---

## 1 · Border Router flashen (Windows, einmalig)

Voraussetzung ist ESP-IDF v5.5.4 unter `C:\clones\esp\esp-idf` (`install.ps1 esp32s3,esp32h2`). Das USB-C-Kabel muss am **Main Board (ESP32-S3)** stecken, nicht am H2.

```powershell
git submodule update --init
.\firmware\build.ps1 -Variant wifi -Port COM7      # oder -Variant ethernet
```

Das Skript baut zuerst das RCP-Image (`ot_rcp`, ESP32-H2), dann den Border Router (ESP32-S3), packt das RCP-Image hinein und flasht. Beim ersten Boot schreibt der S3 das RCP-Image selbst auf den H2.

**WLAN-Variante (aktiv):** Die Zugangsdaten stehen in `firmware/br/sdkconfig.wifi.local`; die Datei ist gitignored:

```
CONFIG_EXAMPLE_WIFI_SSID="FRITZ!Box 1234 AB"
CONFIG_EXAMPLE_WIFI_PASSWORD="..."
```

Beim Boot verbindet sich der Border Router mit dem 2,4-GHz-WLAN und bildet automatisch ein Thread-Netz mit zufälligem Netzwerkschlüssel. Das Netz heißt `thread-net-1`, Kanal 26. Der Leader-Status kommt etwa 45 s nach dem Boot.

**Ethernet-Variante:** `-Variant ethernet`, Kabel in die Fritzbox. Ohne Link startet Thread nicht.

Dataset sichern; es geht nie ins Repo:

```powershell
cargo run --release -p heatd -- thread dataset COM7   # schreibt secrets/thread_dataset.hex
```

OpenThread-Befehle brauchen in dieser Konsole das Präfix `ot`, zum Beispiel `ot state` → `leader` und `ot br state` → `running`. Beim Öffnen des COM-Ports nicht DTR/RTS setzen, sonst startet das Board neu; `heatd` macht das richtig.

## 2 · Minix einrichten (Ubuntu 24.04)

### Docker (empfohlen)

`heatd`, `chip-tool` (gepinnt auf connectedhomeip `v1.5.1.0`, im Image gebaut, kein Snap) und die Produktions-PAA-Zertifikate (gleicher Tag) liegen in einem Image. Auf dem Host bleiben nur Dinge, die der Kernel oder BlueZ liefern:

Port-Vorprüfung auf dem Minix vor dem ersten `up`: chip-tool braucht UDP 5550 und 5353 (Matter, mDNS), `heatd` TCP 8080; mit `network_mode: host` konkurriert das direkt mit anderen Diensten auf dem Host.

```bash
ss -lntup | grep -E ':(8080|5550|5353)\b'    # leere Ausgabe = frei; 5353 teilt sich avahi meist über SO_REUSEPORT, das ist unkritisch
```

Bei Konflikt auf 8080: `listen` in `heatd.toml` (Vorlage `deploy/docker/heatd.docker.toml`) und `HEATD_PORT` in `deploy/docker/.env` auf denselben neuen Port setzen (der Healthcheck liest `HEATD_PORT`, Standard 8080), und die Uhr/App auf den neuen Port zeigen lassen. Ist 5550 oder 5353 belegt, muss der andere Dienst weichen; chip-tool lässt sich nicht umlegen.

```bash
sudo apt install -y bluez && sudo systemctl enable --now bluetooth
sudo usermod -aG bluetooth $USER                                          # Pflicht: D-Bus erlaubt BlueZ-Zugriff nur dieser Gruppe (neu einloggen)
sudo cp deploy/99-thread-ra.conf /etc/sysctl.d/ && sudo sysctl --system   # Routen ins Thread-Netz annehmen (Interface-Name wlo1 in der Datei prüfen)
git clone https://github.com/AAPPHH/MLSmartMatterHeat.git ~/MLSmartMatterHeat && cd ~/MLSmartMatterHeat
cp deploy/docker/heatd.docker.toml heatd.toml                             # Räume, Node-IDs, Zeitpläne anpassen
mkdir -p data models chip-storage secrets && scp <pc>:.../secrets/thread_dataset.hex secrets/
scp <pc>:.../models/room_model.onnx models/                               # optional, ohne Modell liefert /api/scan keinen Raum
cp deploy/docker/.env.example deploy/docker/.env                          # HEATD_UID/GID (id -u, id -g), HEATD_PORT, NINJA_JOBS eintragen
```

Das Thread-Dataset entsteht auf dem PC: `heatd thread form|dataset COMx` läuft dort (serielle Verbindung zum Border Router) und schreibt `secrets/thread_dataset.hex`; die Datei wird nur ins `secrets/` des Minix kopiert (siehe `scp` oben). Auf dem Minix wird dafür keine serielle Verbindung gebraucht.

Kurzweg: `bash deploy/docker/minix-up.sh` (Port 8090, Mock-Controller bis zum Commissioning) kopiert Konfiguration, Daten und Dataset und startet den Container. Sonst manuell: Image bauen, empfohlen auf dem PC und zum Minix schicken (schneller, schont den Minix-RAM; der chip-tool-Build braucht viel RAM und Zeit):

```bash
docker build -t heatd:local -f deploy/docker/Dockerfile .                 # im Repo-Root auf dem PC, Architektur muss zum Minix passen (x86-64)
docker save heatd:local | ssh <minix> docker load
ssh <minix> 'cd ~/MLSmartMatterHeat/deploy/docker && docker compose up -d --no-build'
```

Alternative, Build direkt auf dem Minix: `docker compose up -d --build`. Das dauert lange und braucht mehrere GB RAM; `NINJA_JOBS` in `.env` (Standard 4, bei knappem RAM 1 oder 2) begrenzt die Parallelität von ninja (beim PC-Build: `--build-arg NINJA_JOBS=8`).

```bash
cd ~/MLSmartMatterHeat/deploy/docker
curl http://localhost:${HEATD_PORT:-8080}/health
```

Wichtig an der Compose-Datei (`deploy/docker/compose.yaml`):

| Punkt | Grund |
|---|---|
| `network_mode: host` | Thread-Routen (IPv6, per RA vom Host gelernt), mDNS und Port 8080 für die Uhr brauchen den Netzstack des Hosts |
| `/run/dbus/system_bus_socket` | chip-tool spricht BlueZ per D-Bus für das BLE-Commissioning; es gibt keinen rohen HCI-Zugriff |
| `user: HEATD_UID:HEATD_GID` (aus `.env`) | D-Bus prüft die Gruppe `bluetooth` am Host-User, deshalb läuft der Container mit deiner UID statt einer festen; kein Root |
| `cap_drop: ALL` | `NET_ADMIN`/`NET_RAW` werden nicht gebraucht; BLE läuft über D-Bus, Routen setzt der Host |
| `heatd.toml` (ro), `data/`, `models/`, `chip-storage/`, `secrets/` | Konfiguration, DuckDB + Snapshot, ONNX-Modell, Matter-Fabric (Schlüssel, sichern), Thread-Dataset |

`chip-storage/` enthält die Matter-Fabric-Schlüssel; ohne dieses Verzeichnis müssen alle Thermostate neu gekoppelt werden. In `heatd.toml` stehen die Container-Pfade (`/data`, `/models`, `/chip-storage`, `/opt/paa-root-certs`); `listen` und `HEATD_PORT` in `.env` müssen übereinstimmen, sonst schlägt der Healthcheck fehl. `bypass_attestation = true` ist nur der Notausgang, falls die Attestierung trotzdem scheitert.

`heatd` beendet sich bei `docker stop` über SIGTERM sauber. Logs: `docker compose logs -f heatd`. Update: `git pull && docker compose up -d --build`. Neustart nach neuem Modell: `docker compose restart heatd`.

### Ausfallsicherheit: bei Minix-Ausfall alles auf 18 °C

Die Comet Zero haben weder Zeitplan noch Fallback: sie behalten den zuletzt geschriebenen Sollwert. Drei Schichten:

| Schicht | Wann | Was |
|---|---|---|
| A, heatd beim Beenden | SIGTERM/SIGINT (`docker stop`, Host-Shutdown) | schreibt `[failsafe] setpoint` (18,0) auf alle Knoten, max. `shutdown_timeout_s` (30 s) mit je 3 Versuchen, Ergebnis als Event `failsafe` im Lake; `stop_grace_period: 60s` in `compose.yaml` |
| B, Watchdog auf dem Host | `deploy/failsafe/heatd-failsafe.timer`, alle 5 min | `/health` auf 127.0.0.1:8090 dreimal hintereinander tot: einmal `docker compose restart heatd`; beim naechsten Lauf immer noch tot: heatd stoppen (falls der Container noch laeuft, heatd schreibt dabei selbst 18) und 18 °C per chip-tool auf alle Knoten schreiben, einmal je Ausfall; danach `docker compose up -d` |
| C, Obergrenze im Geraet | Start, nach Commissioning/Resubscribe, alle 6 h, bei jeder Abweichung | heatd schreibt `MaxHeatSetpointLimit` = `[failsafe] max_c` (22,0) auf alle Knoten; ein Boost hebt sie unmittelbar vor dem Boost-Schreiben nur so weit wie noetig an (24 °C, aufgerundet auf 0,5 °C, hoechstens `boost_max_c` 28,5) und senkt sie direkt nach dem naechsten Schreiben unter 22 wieder ab. Faellt der Minix komplett aus (Strom, Hardware), bleibt hoechstens ein Boost haengen, im Normalbetrieb nie ueber 22 °C |

Installation auf dem Minix (Image `heatd:local` und `paa/` muessen vorhanden sein):

```bash
sudo cp deploy/failsafe/heatd-failsafe.{service,timer} /etc/systemd/system/
sudo systemctl enable --now heatd-failsafe.timer
journalctl -t heatd-failsafe -n 20      # zusaetzlich /var/log/heatd-failsafe.log
```

Der Watchdog schreibt pro Knoten exakt diesen Befehl, in einem Einweg-Container mit denselben Volumes wie `compose.yaml` (nur wenn der heatd-Container nicht laeuft, wegen der Sperre auf `chip-storage/`):

```bash
docker run --rm --network host --user 1000:1000 --cap-drop ALL --security-opt no-new-privileges:true --entrypoint /usr/local/bin/chip-tool \
  -v ~/MLSmartMatterHeat/chip-storage:/chip-storage -v ~/MLSmartMatterHeat/paa:/opt/paa-root-certs:ro heatd:local \
  thermostat write occupied-heating-setpoint 1800 <node> 1 --storage-directory /chip-storage --paa-trust-store-path /opt/paa-root-certs
```

Die Knoten stehen in `heatd.toml` (`nodes = [..]`, Fallback 5 6 7 8, ueberschreibbar mit `FAILSAFE_NODES`). Wartung ohne Watchdog: `sudo touch /var/lib/heatd-failsafe/maintenance`, danach wieder loeschen. Der Zustand der Schichten steht in `/api/state` unter `failsafe` (Obergrenze je Knoten wie zuletzt geschrieben, letzter Shutdown, letzte Watchdog-Ausloesung aus `data/failsafe/watchdog.json`) und im Tagesbericht. Lehnt ein Geraet das Schreiben der Obergrenze ab, wird das geloggt und heatd arbeitet weiter; erneuter Versuch alle `retry_min` (10 min).

### systemd (Alternative)

```bash
sudo apt install -y build-essential pkg-config libssl-dev bluez
curl https://sh.rustup.rs -sSf | sh
sudo snap install chip-tool && sudo snap connect chip-tool:process-control
sh deploy/install-paa.sh                  # Produktions-PAA-Zertifikate, sonst scheitert die Attestierung
sudo cp deploy/99-thread-ra.conf /etc/sysctl.d/ && sudo sysctl --system   # Routen ins Thread-Netz annehmen
git clone https://github.com/AAPPHH/MLSmartMatterHeat.git ~/MLSmartMatterHeat && cd ~/MLSmartMatterHeat
cargo build --release -p heatd
cp heatd.example.toml heatd.toml          # Räume, Node-IDs, Zeitpläne anpassen
mkdir -p secrets && scp <pc>:.../secrets/thread_dataset.hex secrets/
```

`[chip_tool]` in `heatd.toml`: `storage_dir` bleibt leer, dann nimmt der Snap seinen eigenen Ordner `~/snap/chip-tool/common`. `paa_trust_store` zeigt auf die Zertifikate von `install-paa.sh`. `bypass_attestation = true` ist nur der Notausgang, falls die Attestierung trotzdem scheitert.

Im Betrieb hält `heatd` **einen** dauerhaften `chip-tool interactive`-Prozess. Darüber laufen eine Subscription pro Thermostat (Ist-Temperatur, Sollwert, Modus) und alle Schreibbefehle; stirbt der Prozess, startet er mit Backoff neu.

```bash
sudo cp deploy/heatd.service /etc/systemd/system/ && sudo systemctl enable --now heatd
curl http://localhost:8080/health
```

## 3 · Commissioning am Freitag 🗓

Pro Thermostat, Node-IDs wie in `heatd.toml` (Beispiel: 5 Wohnzimmer, 6 Schlafzimmer, 7 Bad):

Der laufende `heatd` hält eine Sperre (`heatd.lock`) auf dem chip-tool-Speicher; `commission`, `read` und `set` brauchen dieselbe Sperre. Deshalb zuerst stoppen: ein `docker compose exec` in den laufenden Container scheitert mit "stop heatd first".

Docker:

```bash
cd ~/MLSmartMatterHeat/deploy/docker
docker compose stop heatd                                       # Pflicht: gibt die Sperre frei, `unless-stopped` startet ihn danach nicht von selbst
docker compose run --rm heatd commission 5 34970112332          # 11-stelliger Code oder "MT:..." vom Thermostat
docker compose run --rm heatd read 5                            # Ist- und Solltemperatur
docker compose run --rm heatd set 5 21.5
docker compose up -d heatd
```

systemd:

```bash
sudo systemctl stop heatd                                # Pflicht: sonst "stop heatd first" (Lock auf dem chip-tool-Speicher)
./target/release/heatd commission 5 34970112332          # 11-stelliger Code oder "MT:..." vom Thermostat
./target/release/heatd read 5                            # Ist- und Solltemperatur
./target/release/heatd set 5 21.5
sudo systemctl start heatd
```

**Inventar:** Direkt nach dem Commissioning liest `inventory` alle Attribute und Ereignisse eines Thermostats (chip-tool Wildcard-Reads) und legt sie in `inventory_dir` (Standard `inventory/`, Docker `/data/inventory`) ab: `<node>_<firmware>.json`, `<node>_<firmware>.md` (je Endpoint und Cluster eine Tabelle mit Wert und Schreibbarkeit, dazu Features, Befehle, Batterie, Thread-Nachbarn mit RSSI/LQI, Zähler und Ereignisse) sowie die Rohlogs unter `inventory/raw/`. `<firmware>` ist die SoftwareVersionString des Geräts. Herstellerspezifische Cluster (ab 0xFC00 oder mit Vendor-Präfix) bleiben als Einträge erhalten und sind mit "vendor" markiert; chip-tool kennt ihre Werte aber nicht ("Don't know how to log"), deshalb steht dort `<not decoded by chip-tool, see raw log>`, den Rohwert findest du nur im Rohlog. Der Befehl braucht die Sperre, also `heatd` vorher stoppen und `controller = "chip-tool"` (mit `controller = "mock"` bricht er ab):

```bash
cd ~/MLSmartMatterHeat/deploy/docker
docker compose stop heatd
docker compose run --rm heatd inventory 5                       # schreibt /data/inventory/5_<firmware>.json und .md
docker compose up -d heatd
```

systemd: `./target/release/heatd inventory 5`. Jeder der beiden chip-tool-Aufrufe (Attribute, Events) darf bis zu `3 × timeout_s` dauern (`--timeout` wird entsprechend gesetzt). Endet chip-tool mit Fehler oder läuft in den Timeout, werden trotzdem die Rohlogs unter `inventory/raw/` geschrieben (bei null geparsten Attributen als `<node>_failed_<zeit>.*`) und die Fehler stehen im Inventar; ein schlafendes Thermostat braucht eventuell einen zweiten Versuch. Im laufenden `heatd` wird das Inventar nach einem Firmware-Wechsel (BasicInformation SoftwareVersion oder SoftwareVersionString) automatisch neu erzeugt. Damit nie zwei chip-tool-Prozesse auf demselben Storage laufen, hält `heatd` dafür die interaktive Sitzung kurz an und startet sie danach neu (Subscriptions bauen sich neu auf, einige Minuten ohne Reports sind möglich). Sichere `/chip-storage` vor dem ersten Deploy (`cp -a chip-storage chip-storage.bak`), die Fabric-Schlüssel liegen dort.

**Matter-Erfassung:** Pro Node abonniert `heatd` per chip-tool alle Attribute und Events (`any subscribe-by-id` / `any subscribe-event-by-id`, `subscribe_min_s` = 30 bis `subscribe_max_s` = 300 s; das schont die Batterie des schlafenden Thermostats; ein manueller Eingriff am Gerät fällt daher bis zu 30 s später auf) und schreibt jeden Report unverändert in die Lake-Tabellen `attribute_reports` und `events` (siehe `docs/data-foundation.md`, Abschnitt "Abweichungen": Rohwert ist der von chip-tool gedruckte Text, nicht TLV). Zusätzlich fragt ein Heartbeat-Poll LocalTemperature, OccupiedHeatingSetpoint, PIHeatingDemand, SystemMode und BatPercentRemaining ab (`source=poll`), aber nur für einen Node, von dem in den letzten `poll_silence_s` = 300 s kein Report kam; bei Fehlschlägen wird die Pause bis auf 16 Minuten verdoppelt und ein `poll_failed`-Event geschrieben. Abriss und Wiederaufbau der Subscription, StartUp/ShutDown und Firmware-Wechsel landen als Zeilen in `events`.

Im Betrieb liegen zusätzlich folgende Werte im Speicher (`/api/state`) und in `attribute_reports`: `pi_heating_demand`, `thermostat_running_state`, `local_temperature_calibration`, Batterie (`bat_voltage` in mV, `bat_percent_remaining` in %, `bat_charge_level`), `routing_role` und RSSI/LQI des Thread-Parents (`thread_parent_rssi`, `thread_parent_last_rssi`, `thread_parent_lqi`). Die Subscriptions sind volle Wildcards (höchstens zwei pro Node: Attribute und Events), fehlende optionale Cluster stören sie nicht; Fehlerzeilen von chip-tool werden einmal im Log gemeldet.

Der Minix spricht das Thermostat per Bluetooth an (`hci0`), übergibt das Thread-Dataset, und danach läuft alles über den Border Router. Das Thermostat muss dabei frisch zurückgesetzt und im Pairing-Modus sein und nah am Minix liegen. Das Commissioning darf bis zu 180 s dauern (`commission_timeout_s`).

Nach dem ersten Thermostat startest du `heatd` und prüfst mit `journalctl -u heatd -f` (Docker: `docker compose logs -f heatd`) und `curl localhost:8080/api/state`, ob Temperaturen ankommen. Das ist der erste echte Test des dauerhaften chip-tool-Prozesses.

## 4 · Uhr & Training ⌚

```bash
sh wear/build.sh                                         # Tests + signierte Release-APK (Docker)
adb connect <uhr-ip>:<port> && adb install -r wear/app/build/outputs/apk/release/app-release.apk
```

Die APK liegt auch im GitHub-Release. In der App erlaubst du **genauen Standort** (für WLAN-Scans) und **Geräte in der Nähe / LAN-Zugriff** (Android 17, sonst erreicht die Uhr den Minix nicht). Dann prüfst du die Server-URL (`http://192.168.178.29:8090`; auf dem Minix belegt gluetun 8080), tippst auf Start und fügst die Kachel **HeatScan** hinzu. Die Statuszeile meldet Probleme direkt, etwa „Standort aus“, „WLAN aus“ oder „LAN-Zugriff fehlt“.

Ab Version 2.4 misst die Uhr zusätzlich Hauttemperatur, Puls, Bewegung (Varianz), Trageerkennung und Akku und schickt sie gebündelt (alle 5 min oder 50 Samples) an `POST /watch/measurement`, zusammen mit dem nächsten Scan-Post (kein eigener Flush). Ab 2.5 läuft der Puls-Sensor nur kurz (höchstens 20 s, erster gültiger Wert) und die Beschleunigung 10 s in ohnehin stattfindenden Wachphasen (Scan-Takt, Bildschirm an); Hauttemperatur und Trageerkennung bleiben hardware-gebatcht. Scans gehen wieder gebündelt an `/api/scans`. Auf Wear OS 6 fragt die App nach `READ_HEART_RATE`/`READ_SKIN_TEMPERATURE` in einem ersten und `READ_HEALTH_DATA_IN_BACKGROUND` in einem zweiten Schritt; der Dienst läuft zusätzlich mit Typ `health`, sobald eine Gesundheitsberechtigung erteilt ist (Statuszeile `bg=ok|fehlt`). Dafür auf der Uhr unter Einstellungen > Apps > HeatScan > Berechtigungen **Körpersensoren** (Puls, `READ_HEART_RATE`) und **Hauttemperatur** (`READ_SKIN_TEMPERATURE`) erlauben (Wear OS 6, SDK 36 und höher; die App fragt beim Start). Fehlt eine Berechtigung oder der Sensor, wird nur dieser Kanal übersprungen; die Statuszeile zeigt es unter `sensors=` (z. B. `hr=ok,skin=Recht verweigert`). Antwortet ein älteres heatd mit 404/405, bleiben die Sensor-Batches in der Warteschlange und gehen nach einem Update ein.

**Training:**
1. Die Uhr fragt per Benachrichtigung „Wo bist du?“ (5 Raumknöpfe, Vermutung zuerst). Ein Tipp etikettiert die Scans ab der Frage und startet einen Aufenthalt: weitere Scans im selben Raum werden ohne Rückfrage etikettiert (höchstens 2 h). Wechselst du den Raum (Bewegung, danach zwei Scans mit anderem WLAN-Fingerabdruck oder abweichender Vorhersage), fragt die Uhr einmal mit Vibration „Raum gewechselt? Wo bist du jetzt?“ und etikettiert rückwirkend ab dem ersten abweichenden Scan. Vibration höchstens alle 3 min, nachts (22–7 Uhr), unterwegs und ohne Uhr am Handgelenk nur lautlos. Zusätzlich geht Antippen in der App; es entstehen keine zusätzlichen Scans. Ab Version 4.0 (Material 3 Expressive, `wear/`): Die Kachel zeigt je Raum Ist-/Solltemperatur, offene Fenster und den ersten Alarm (Stand aus `GET /api/state`, geladen nur beim Öffnen von App oder Kachel) und öffnet die App. Eine `window_prompt` erzeugt eine Benachrichtigung „Fenster offen?“ mit einer Vibration und den Aktionen Ja (Primäraktion, auch per Double Pinch), Nein und Weiß nicht; jede Antwort, auch Weiß nicht, Wischen und Ablauf nach 20 min (ohne zusätzlichen Weckruf erst beim nächsten Antwort-Eingang oder App-Start erkannt, mit dem Ablaufzeitpunkt als `ts`), geht als `POST /watch/window_label` mit `answer` (`yes`/`no`/`unknown`/`timeout`) und `via` in die Warteschlange. In der App meldet „Auf“/„Zu“ ein Fenster selbst (derselbe Endpunkt ohne `prompt_id`, wirkt wie `POST /api/window`). Alarme aus `alerts` kommen je `id` genau einmal als Benachrichtigung und stehen als Liste auf dem Startbildschirm. „Boost“ ruft `POST /api/boost` (5 min) und zeigt Entscheidung und Grund des Servers, der Zustand je Raum kommt aus `GET /api/boost`.
2. Snapshot ziehen und trainieren:
   ```bash
   curl -X POST http://localhost:8080/api/snapshot       # data/snapshot.duckdb
   cd ml && uv run python -m heatml train                # models/room_model.onnx + .json
   sudo systemctl restart heatd                          # Docker: docker compose restart heatd
   ```
3. Ab jetzt liefert `POST /api/scan` den Raum, und die Regeln heizen dort, wo du bist.

Feature-Auswahl: Nur stabile BSSIDs kommen ins Modell, also solche, die in genügend Scans eines Raums sichtbar sind und nicht ständig auftauchen und verschwinden. Fehlende BSSIDs werden maskiert. Schwellen stehen in `CONFIG` in `ml/src/heatml/__main__.py`.

## Datenlake, Export & Konfiguration 🗄

`heatd` schreibt alle Rohdaten als Arrow-Streams nach `<data_dir>/raw/` und rollt sie täglich um 03:00 in Parquet (`<data_dir>/parquet/`, Marker `.rolled`). Schema: `docs/data-foundation.md`.

```bash
heatd migrate                                                   # einmalig, VOR dem ersten Start dieses Builds: alte DuckDB-Daten in die Lake übernehmen
heatd inventory 5                                               # Geräteinventar (siehe oben), heatd vorher stoppen
heatd export --room wohnzimmer --from 2026-10-01 --to 2026-10-20   # 1-min-Raster (analysis/views/room_minute.sql) als Parquet, --to inklusive
heatd report [2026-10-20]                                       # Tagesreport nach <data_dir>/reports/<datum>.md (läuft automatisch um 03:10)
```

`migrate` übernimmt nur Zeilen vor dem ersten `startup`-Event von `heatd` in `raw/events` (ohne dieses Event vor dem ältesten `watch_raw`-Zeitstempel), liest vorher alle Tabellen vollständig ein und schreibt danach; so entstehen keine Doppelzeilen, weil der laufende Build DuckDB und Lake parallel beschreibt.

Docker: `docker compose run --rm heatd export --room wohnzimmer --from ... --to ...` schreibt nach `/data/exports/`.

Neue Schlüssel in `heatd.toml` (Vorlagen in `heatd.example.toml`, Docker: `deploy/docker/heatd.docker.toml`):

| Schlüssel | Bedeutung |
|---|---|
| `latitude`, `longitude` | schaltet den stündlichen Open-Meteo-Abruf (Wetter, `weather_url` überschreibt den Endpunkt) und `coordinates` in `meta` ein; ohne beide aus |
| `away_setpoint`, `away_guard_minutes` | Sollwert bei Abwesenheit (18 °C) und Zeit, die die getragene Uhr das WLAN fehlen muss (20 min) |
| `occupied_setpoint` / `sleep_room` | Sollwert des belegten Raums (21) und Schlafzimmer, das bei Schlafen den Zeitplanwert behält (z. B. `schlafzimmer`; leer = keine Ausnahme) |
| `manual_hold_minutes` | ändert jemand den Sollwert am Thermostat, hält `heatd` diesen Wert genau so lange (60 Minuten, auch wenn sich Zeitplan oder Anwesenheit ändern; nur offenes Fenster und Abwesenheit gehen vor), setzt dann sofort den Systemwert und läuft weiter; Entscheidung `rule=manual` und Label `override` einmal, ein laufendes Experiment bricht ab |
| `poll_silence_s` | Heartbeat-Poll pro Node nur, wenn so lange (300 s) kein Report kam |
| `exploration_seed` | Seed für die zufälligen Startzeiten der Experimente |
| `[rooms.exploration]` | je Raum `enabled` (Standard `false`), `p_per_day`, `comfort_min`, `comfort_max`, `blackout = [{from, to}]` |
| `neighbours` | Nachbarräume: `{ a, b, kind = "wall" oder "ceiling" oder "open", partial }` (bei `ceiling` liegt `a` über `b`), alte Paare `["a","b"]` gelten weiter (Art unbekannt); die Paare liefern die Nachbartemperaturen im Raster, die Arten stehen als `links` in `meta` |
| `[heat_source]` | `kind`, `scope`, `efficiency_class`, `flow_temp_controllable`, `night_setback_controllable`, `operating_hours`, `night_shutdown` als `meta` |
| `[building]`, `rooms.exterior` | `floor`, `roof_rooms`; Außentüren je Raum (`exterior = ["front_door"]`) als `meta` `building`. Räume ohne Thermostat (`nodes` weglassen, z. B. der Flur) sind erlaubt |
| `ntp_server` | SNTP-Drift-Check (Event ab 1 s), leer schaltet ihn aus |
| `inventory_dir` | Ablage der Inventardateien |

`/health` zeigt je Node Subscription-Status und Alter des letzten Reports (`degraded` bei Verlust), `/metrics` dieselben Werte für Prometheus.

## Regeln 🧠

| Priorität | Bedingung | Sollwert |
|---|---|---|
| 1 | Fenster offen (`POST /api/window`) | `frost_setpoint` |
| 2 | Uhr abwesend (Schutz: `away_guard_minutes`, Uhr muss getragen worden sein) | `away_setpoint` (18 °C) in allen Räumen, ohne Zeitplan |
| 3 | Laufendes Explorationsexperiment (`rooms.exploration`) | Experiment-Sollwert |
| 4 | Raum, in dem du bist, außer `sleep_room`, solange die Uhr auf Schlafen steht | `occupied_setpoint` (21 °C), immer genau dieser Wert |
| 5 | Zeitplan greift (auch über Mitternacht), auch im `sleep_room` bei Schlafen | Zeitplan-Wert |
| – | sonst | `default_setpoint` |

Ein manueller Eingriff am Thermostat (`manual_hold_minutes`) hält den Wert des Geräts genau 60 Minuten und geht allem außer offenem Fenster und Abwesenheit vor. Frost (`frost_setpoint`) ist der Wert bei offenem Fenster.

Der belegte Raum ist der Raum des Matchers, solange er sicher ist (nicht abstained, innerhalb von `t_hold_min`, Alter höchstens `presence_timeout_minutes`). Fehlt er, gilt das bestätigte Stay-Label der Uhr (Scan-Label oder `/api/label`), wenn es jünger als `presence_timeout_minutes` ist und die Uhr seither keine Bewegung gemeldet hat. Räume ohne Node (z. B. `flur`) bekommen keine Aktion. Schlafen heißt: Zustand `sleep` aus dem Watch-Zustand oder das Feld `bedtime: true`, das die Uhr aus dem Bettzeit-Modus sendet. `presence_boost` wird nicht mehr verwendet (Schlüssel bleibt erlaubt, ohne Wirkung). Entscheidungen tragen `rule=presence`, `reason` = `occupied` oder `sleep`, sowie `source` (`matcher` oder `stay_label`) und `sleep` in `inputs.extra`.

Nach einem Neustart baut heatd die Anwesenheit beim Start aus dem auf, was es ohnehin speichert, damit nicht bis zum nächsten Upload der Uhr (oft 20-30 Minuten) nur "vermutlich zuhause" gilt: Tracker (Weg/Zurück, inklusive `away_guard_minutes`) aus `device_state` der letzten `[presence] tracker_lookback_h` Stunden, Matcher-Raum, Stay-Label und Schlafzustand (Hr/Bewegung/Offbody/Ladegerät) aus Lake (`watch_inference`, `labels`, `watch_state`, `watch_raw`) der letzten `presence_timeout_minutes`, und die Raum-Belegung (`occupancy_enter_min` rechnet ab dem ersten Treffer des Raums). Ältere Daten werden ignoriert, Abwesenheit bleibt bis `still_contact_timeout_minutes` erhalten. `tracker_lookback_h` (Standard 72) muss die längste Ruhephase während der Abwesenheit abdecken, sonst startet der Tracker ohne die Bewegung, die das Weg bestätigt, und meldet zuhause. Stay-Labels werden nach ihrem Ende (`to`) einsortiert, nicht nach dem Upload, und gelöschte Labels (void) werden ignoriert, unabhängig von der Upload-Reihenfolge. Abschalten mit `[presence] restore = false`.

Die Regeln laufen alle 60 s. Geschrieben wird nur, wenn der neue Wert ≥ 0,5 °C vom zuletzt gesetzten abweicht. Die Entscheidung wird vor dem Geräte-Schreiben auf Platte geschrieben (fsync). Berichte, die kurz nach einem eigenen Schreiben noch den alten oder einen vom Gerät gerundeten Wert zeigen (2 × `timeout_s`), und Poll-Antworten gelten nie als manueller Eingriff. Ein echter manueller Eingriff am Thermostat wird genau `manual_hold_minutes` (60) gehalten; danach schreibt `heatd` sofort den Systemwert (ohne Ratenlimit).

Anhebungs-Budget: Damit die Thermostate nicht dauernd umschalten, darf jeder Raum pro rollender Stunde nur `raise_budget_per_hour` (Standard 1, 0 = unbegrenzt) Sollwert-Anhebungen bekommen. Eine Anhebung ist jedes Schreiben, dessen Wert über dem liegt, was das Gerät meldet bzw. heatd zuletzt geschrieben hat. Senkungen (Fenster-Frost, Abwesenheit, Boost-Ende, Zeitplan/Anwesenheit nach unten) sind immer sofort und kosten nichts. Ist das Budget aufgebraucht, wird die Anhebung aufgeschoben (Entscheidung mit `reason=raise_budget`, `deferred=true`, einmal je Wert) und sobald ein Slot frei wird, mit dem dann aktuellen Wunschwert geschrieben; es gibt keine Warteschlange alter Werte. Der Boost auf 24 °C zählt als Anhebung (er ersetzt die gleichzeitige Anhebung auf den Zielwert, beides zusammen kostet eine). Boost-Zyklen und Explorationsexperimente starten nicht, solange das Budget des Raums aufgebraucht ist (laufende werden nicht abgebrochen). Frei, aber in `inputs.extra.budget` als `exempt_*` protokolliert: das Zurückschreiben nach Neustart oder Border-Router-Failsafe, wenn das Gerät noch auf dem Failsafe-Wert (`[failsafe] setpoint` bzw. `[br_watchdog] setpoint_c`) steht (je Knoten das erste Schreiben nach dem Start), das Nachschreiben eines unveränderten Ziels durch die Sollwert-Prüfung (`[node_health]`), die Obergrenze bei `no_supply` und die Rückkehr auf das vorherige Ziel nach Fenster-Schließen bzw. PI-Reset (`exempt_window_close`), weil sie nur heatds eigene Frost-Senkung zurücknimmt; sie verbraucht kein Budget. Sichtbar in `/api/state` (`rooms[].raise_budget`: `limit`, `used`, `next_free_ts`, `deferred_setpoint`), Prometheus (`heatd_raise_budget_limit`, `heatd_raise_budget_used`, `heatd_raise_next_free_seconds`, `heatd_raise_deferred`, `heatd_raises_deferred_total`) und im Tagesbericht (Abschnitt "Anhebungs-Budget je Raum").

## Boost (Aufheizen) 🚀

Geboostet wird nur in zwei Szenarien. Morgens: die Uhr meldet Aufwachen (Schlafzustand oder Bettzeit endet) zwischen 05:00 und 11:00, solange jemand da ist; Räume, deren Wunsch-Sollwert dann um mindestens 1 K steigt. Heimkehr: Abwesenheit endet nach mindestens `[boost] homecoming_min_away_min` (60) Minuten (kuerzer: `kein Boost: nur N min weg`); Räume, die vom Abwesenheits-Sollwert um mindestens 1 K steigen. Raumwechsel, Zeitplan-Stufen und jede andere Anhebung boosten nie. Der Boost schreibt 24 °C, hält 5 bis 20 Minuten (Ventil offen) und schreibt dann den Zielwert des Raums. Nach einem offenen Fenster (eigener Raum oder Haus-Kanal) boostet heatd automatisch nicht mehr, solange es weniger als `[boost] after_window_min` (180) Minuten her ist, weil Heizkoerper und Waende noch warm sind (von Hand per `POST /api/boost` bleibt es erlaubt, mit Hinweis). Abbruch bei Sensor ≥ 24 °C, Fenster auf, Abwesenheit, manueller Änderung am Gerät oder Absenkung des Ziels. Das Anhebungs-Budget (1 pro Raum und Stunde) gilt, der Boost ist die Anhebung, die Rückkehr ist eine freie Senkung.

Die Dosis kommt aus dem gelernten Modell (`/models/boost/<Zeitstempel>/bundle.json`, neuestes gültiges Bundle, Neuladen alle `[boost] reload_min`): höchstens 10 % über der stärksten bekannten Dosis in ähnlicher Lage (eigener Raum, Kontextvektor), 5 Minuten ohne ähnliche Lage, und nur so lang, dass das dem Boost zuzurechnende Risiko P_Boost(Spitze > Ziel + 0,5 K) − P_ohne_Boost(Spitze > Ziel + 0,5 K) höchstens 10 % beträgt (ohne Boost = nur der Thermostat). Ohne Bundle gibt es keinen Boost. Nach jedem Boost misst heatd 60 Minuten die Spitze und schreibt ein `boost_cycle`-Event; der Trainer-Timer (alle 10 Minuten) lernt danach neu. `POST /api/boost` löst einen Boost von Hand über denselben Weg aus (Szenario `manual`, Dosis `min(minuten, 20)`, ohne Angabe 5 Minuten, gleiche 24-°C-Grenzen und Budget; kein Bundle → 409 mit Grund). Alle manuellen Eingaben laufen über eine Intent-Schicht (`Boost`, `WindowOpen`, `WindowClose`); ein späterer Thermostat-Tastendruck (kurzer lokaler Sollwert-Schub, in `rules::on_report` erkannt) baut nur den Intent und ruft dieselbe Funktion. Zustand in `/api/state` (`rooms[].boost`: Szenario, erlaubt, gewählt, Grund in einem Satz), `/api/boost`, Prometheus `heatd_boost_*` und im Tagesbericht („Aufheizen: Boost nach Szenario“, inklusive verweigerter Boosts). Details: `docs/boost.md`.


## Fenster-offen-Erkennung 🪟

heatd erkennt offene Fenster mit einem gelernten Physik-Residuum-Modell (Detektor `v3`; die alten Schwellen-Detektoren v1/v2 und `window-replay` sind entfernt). Der Fühler des Comet Zero sitzt am Heizkörper, ein schließendes Ventil kühlt ihn wie ein offenes Fenster ab. Das Modell kennt deshalb das Ventil.

- **Modell**: je Raum eine Ridge-Regression, die die Fühleränderung der nächsten 20 min aus dem eigenen Ventilverlauf (Mittel über 20 min, gefilterte Werte mit tau 4/12/40 min, Ventil jetzt und vor 20 min), Sollwert, Außentemperatur, Strahlung, Wind und Minuten seit offenem Ventil vorhersagt. Evidenz = standardisiertes negatives Residuum z je Raum (auf 6 begrenzt). Hausscore = Summe der positiven z mit Gewicht 1 (Erdgeschoss) bzw. 0,3 (Schlafzimmer). Alarm, wenn der Score den Schwellwert des Bundles überschreitet (aus 0,5 Episoden pro Tag auf unmarkierten Daten abgeleitet, Stand der ersten Auswertung 8,0). Ein einzelner Raum erreicht höchstens 6 und löst allein nie aus, es müssen mindestens zwei Räume zusammenkommen. Offline erkannte das Modell das Fenster vom 2026-11-07 (Bad und Flur) 25 min nach dem Öffnen bei 0,4 Fehlalarmen pro Tag und 0 Alarmen bei Kesselausfall.
- **Bundle**: `models/window/<version>/bundle.json` (nächtlich vom Trainingscontainer geschrieben, `python -m heattrain window`, nur wenn das feste Kriterium erfüllt ist). heatd lädt das neueste beim Start und prüft alle `reload_min` = 10 min auf eine neuere Version. Fehlt das Bundle oder ist es ungültig, wird einmal geloggt, es gibt keine Erkennung und die Heizung läuft unverändert weiter.
- **Eingaben minutengenau**: Aus den Berichten `local_temperature`, `pi_heating_demand` und `occupied_heating_setpoint` jedes Knotens wird pro Minute der letzte gültige Wert übernommen (Temperatur höchstens 10 min, Ventil 30 min, Sollwert 6 h alt), dazu die letzte Wetterbeobachtung (Außentemperatur, Wind, Strahlung, höchstens 180 min alt, keine Interpolation). Fehlt ein Wert, ist die Minute nicht bewertbar. Ein fehlendes Ventil gilt nie als geschlossen. Nach einem Neustart wird der Verlauf aus dem Lake nachgespielt (ohne Wetter, die ersten 20 min danach sind deshalb nicht bewertbar), nach einer Ventillücke dauert es 141 min bis zur nächsten Bewertung.
- **Kessel-Veto**: Solange irgendein Raum `no_supply` meldet (Event `supply`, Zeitpunkt der Meldung), und 60 min danach gibt es keinen Alarm.
- **Wirkung**: Beim Alarm bekommen die Erdgeschossräume mit Thermostat (nicht in `building.roof_rooms`) Frost (`frost_setpoint`), deren z mindestens `room_z` = 1,0 beträgt; trägt keiner so viel bei, alle Erdgeschossräume mit Thermostat. Experimente werden in offenen Räumen nicht gestartet und laufende abgebrochen. Events `window_open` (Detail: Detektor `v3`, Räume, z je Raum, Hausscore, Schwellwert, Bundle-Version, Quelle `auto`).
- **Zu** nach spätestens `hold_min` = 20 min (`window_close`, Grund `timeout`), früher mit `POST /api/window {open: false}` (Grund `manual`) oder mit der Antwort "zu" der Uhr auf die Fensterfrage (Grund `label`). Danach öffnet derselbe Raum `rearm_min` = 30 min lang nicht automatisch neu.
- **Kein Alarm** (Event `window_suppressed`, Grund `rearm`, `setpoint`, `manual_hold` oder `away`) für einen Raum, der gerade geschlossen wurde, dessen Sollwert innerhalb von `setpoint_guard_min` = 20 min gesenkt wurde, der im manuellen Hold ist oder wenn niemand zu Hause ist (Abwesenheit). Je Grund wird höchstens alle 15 min geloggt.
- **Rückfragen an die Uhr**: Bei jedem automatischen Öffnen steht (höchstens ein offener, `prompt_expire_min` = 20 min gültiger) `window_prompt {id, room, question: open}` in den Antworten von `/api/scan` und `/api/scans`, solange eine Uhr zu Hause ist (Raum mit dem höchsten z). Antwort: `POST /watch/window_label {device, ts, room, answer, via, prompt_id, label_id}` (Label-Quelle `window`; `answer` = yes, no, unknown, timeout oder voided, alte Clients senden nur `open`); "yes" ändert nichts, "no" schließt das automatisch geöffnete Fenster, `unknown`/`timeout` beenden nur die Frage und bestätigen nie, `voided` nimmt die früheren Antworten der Frage zurück (Details in `contract/README.md`). Ohne `prompt_id` gilt der Eintrag als manuell und wirkt wie `POST /api/window`. Zurücknehmen über `/watch/label/void`. Pro Episode landet 25 min nach Ende (`label_grace_min`) ein `window_episode`-Event im Lake, der Tagesbericht fasst es zusammen.
- Ein manuell geöffnetes Fenster (`POST /api/window`) schließt nach spätestens `manual_max_min` = 12 h von selbst (`manual_timeout`).
- `POST /api/window {"room": "...", "open": true|false}` überschreibt manuell. Zustand: `/api/state` (je Raum Zustand, z, Modellversion), `/metrics` (`heatd_window_z`, `heatd_window_house_score`), Tagesbericht. Einstellungen unter `[window]` in der Konfiguration (alte Schlüssel der v1/v2-Zeit werden ignoriert).

## API

| Methode | Pfad | |
|---|---|---|
| `POST` | `/api/scan` | Scan der Uhr → `{stored, room}` |
| `GET` | `/api/state` | Räume, Temperaturen, Sollwerte, letzter Raum je Gerät |
| `POST` | `/watch/measurement` | `{device, ts, measurements:[{sender, rssi, freq oder channel}]}` nach `watch_raw` |
| `POST` | `/watch/label` | wie `/api/label`, nach `labels` |
| `GET` | `/watch/update` | App-Update der Uhr: `{version_code, version_name, sha256, size}` oder 404 (nichts veröffentlicht oder Prüfsumme/Größe passt nicht) |
| `GET` | `/watch/update.apk` | die APK dazu (`application/vnd.android.package-archive`); Dateien `update.json` + `update.apk` in `[watch] update_dir` (Standard `/data/watch_update`, im Docker-Mount `data/`), veröffentlicht mit `deploy/watch-publish.sh` auf dem PC |
| `POST` | `/api/snapshot` | DuckDB-Snapshot für das Training |
| `POST` | `/api/window` | `{room, open}` |
| `POST` | `/api/boost` | `{room, minutes?}` manueller Boost (Szenario `manual`) über denselben Sicherheitsweg, 200 mit Entscheidung oder 409 mit `reason` |
| `GET` | `/api/boost` | Boost-Zustand je Raum (Szenario, erlaubt, gewählt, Grund) |
| `POST` | `/watch/window_label` | `{device, ts, room, answer, via, open?, prompt_id, label_id}` |
| `GET` | `/health` | |

Die API hat keine Authentifizierung und gehört nur ins LAN.

## Raum-Abgleich trainieren (Phase 1b) 🧭

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

Wöchentliches Neutraining extern, z. B. systemd-Timer auf dem Minix (nicht deployt):

```
0 4 * * 0  cd /opt/MLSmartMatterHeat/training && uv run python -m heattrain watchroom /data /models/watchroom
```

## Thermisches Raummodell (virtuelles Raumthermometer) 🌡

Je Thermostat ein Grey-Box-Modell (Radiator `T_r`, Raumluft `T_a`, Sensor `y = w*T_r + (1-w)*T_a`): Der Heizkörper wird vom Ventil `u` (PIHeatingDemand, Werte über 100 gelten als fehlend) mit effektiver Vorlauftemperatur `T_s` geheizt, die Luft koppelt an Radiator, Außentemperatur, Wind, Strahlung und Nachbarräume (`links` aus `meta`). Fit per Kalman-Filter-Likelihood (scipy, Parameter positiv und beschränkt, 1-Minuten-Raster); Parameter-Unsicherheit aus der Jacobi-Matrix, Fallback auf 1 Zustand, wenn 2 Zustände nicht identifizierbar oder auf dem Hold-out-Block nicht besser sind.

```bash
cd training
uv run python -m heattrain thermal /data /models        # <data_root> (parquet/) oder Roh-Ordner mit *.arrows
```

Ausgabe: `<models_dir>/thermal/<datum>/<raum>.json` (Parameter, Grenzen, relative Standardabweichung, Zeitkonstanten, RMSE auf dem Hold-out-Block gegen Persistenz, Datenspanne, Warnungen). Python: `estimate_room_air` liefert die gefilterte Raumluft samt Unsicherheit, `predict` und `ThermalModel.lead_time` rechnen Vorlaufzeiten für geplante Ventilstellung oder Solltemperatur. Die Quelle `legacy` wird ausgeschlossen (Config `exclude_sources`). Mit weniger als etwa einem Tag Daten sind die Parameter schwach bestimmt; die Warnungen im JSON sagen, welche.

## Entwickeln 🧪

```bash
cargo test --workspace && cargo clippy --workspace --all-targets -- -D warnings   # ohne Hardware, Mock-Controller
cd ml && uv run pytest
cd training && uv run pytest
sh wear/build.sh
```

`controller = "mock"` in `heatd.toml` lässt den ganzen Dienst ohne Thermostate laufen. Mit `heatml.synth` lassen sich realistische Fake-Scans erzeugen.

Debug-Aktionen der Uhr (`adb shell am broadcast -n de.aapphh.heatscan.debug/de.aapphh.heatscan.DebugReceiver -a de.aapphh.heatscan.<AKTION>`, Debug-Build; beim Release-Build Paket `de.aapphh.heatscan`, nur mit DUMP-Recht): `STATUS`, `START`, `CONFIG`, `PAUSE`, `FLUSH` und `DEMO`. `DEMO` schreibt Beispieldaten (Küche 82 %, fünf WLANs) in den Speicher, damit man die Datenseite ohne Modell sieht; eine echte Server-Antwort mit Räumen überschreibt sie wieder.
