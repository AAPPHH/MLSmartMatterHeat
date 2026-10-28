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
| `wear/` | Kotlin, Pixel Watch | App **HeatScan**: Foreground-Service scannt alle 30 s, Kachel und Knöpfe zum Raum-Labeln |
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

Image bauen, empfohlen auf dem PC und zum Minix schicken (schneller, schont den Minix-RAM; der chip-tool-Build braucht viel RAM und Zeit):

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

Der Minix spricht das Thermostat per Bluetooth an (`hci0`), übergibt das Thread-Dataset, und danach läuft alles über den Border Router. Das Thermostat muss dabei frisch zurückgesetzt und im Pairing-Modus sein und nah am Minix liegen. Das Commissioning darf bis zu 180 s dauern (`commission_timeout_s`).

Nach dem ersten Thermostat startest du `heatd` und prüfst mit `journalctl -u heatd -f` (Docker: `docker compose logs -f heatd`) und `curl localhost:8080/api/state`, ob Temperaturen ankommen. Das ist der erste echte Test des dauerhaften chip-tool-Prozesses.

## 4 · Uhr & Training ⌚

```bash
sh wear/build.sh                                         # Tests + signierte Release-APK (Docker)
adb connect <uhr-ip>:<port> && adb install -r wear/app/build/outputs/apk/release/app-release.apk
```

Die APK liegt auch im GitHub-Release. In der App erlaubst du **genauen Standort** (für WLAN-Scans) und **Geräte in der Nähe / LAN-Zugriff** (Android 17, sonst erreicht die Uhr den Minix nicht). Dann prüfst du die Server-URL (`http://192.168.178.29:8080`), tippst auf Start und fügst die Kachel **HeatScan** hinzu. Die Statuszeile meldet Probleme direkt, etwa „Standort aus“, „WLAN aus“ oder „LAN-Zugriff fehlt“.

**Training:**
1. In jedem Raum ein paar Mal 10–20 min verbringen und vorher auf der Uhr den Raum antippen; danach „kein Label“.
2. Snapshot ziehen und trainieren:
   ```bash
   curl -X POST http://localhost:8080/api/snapshot       # data/snapshot.duckdb
   cd ml && uv run python -m heatml train                # models/room_model.onnx + .json
   sudo systemctl restart heatd                          # Docker: docker compose restart heatd
   ```
3. Ab jetzt liefert `POST /api/scan` den Raum, und die Regeln heizen dort, wo du bist.

Feature-Auswahl: Nur stabile BSSIDs kommen ins Modell, also solche, die in genügend Scans eines Raums sichtbar sind und nicht ständig auftauchen und verschwinden. Fehlende BSSIDs werden maskiert. Schwellen stehen in `CONFIG` in `ml/src/heatml/__main__.py`.

## Regeln 🧠

| Priorität | Bedingung | Sollwert |
|---|---|---|
| 1 | Fenster offen (`POST /api/window`) | `frost_setpoint` |
| 2 | Zeitplan greift (auch über Mitternacht) | Zeitplan-Wert |
| – | sonst | `default_setpoint` |
| + | Uhr im Raum, jünger als `presence_timeout_minutes` | `+ presence_boost` |
| ↓ | Anwesenheit veraltet | max. `eco_setpoint` |

Die Regeln laufen alle 60 s. Geschrieben wird nur, wenn der neue Wert ≥ 0,5 °C vom zuletzt gesetzten abweicht.

## API

| Methode | Pfad | |
|---|---|---|
| `POST` | `/api/scan` | Scan der Uhr → `{stored, room}` |
| `GET` | `/api/state` | Räume, Temperaturen, Sollwerte, letzter Raum je Gerät |
| `POST` | `/api/snapshot` | DuckDB-Snapshot für das Training |
| `POST` | `/api/window` | `{room, open}` |
| `GET` | `/health` | |

Die API hat keine Authentifizierung und gehört nur ins LAN.

## Entwickeln 🧪

```bash
cargo test --workspace && cargo clippy --workspace --all-targets -- -D warnings   # ohne Hardware, Mock-Controller
cd ml && uv run pytest
sh wear/build.sh
```

`controller = "mock"` in `heatd.toml` lässt den ganzen Dienst ohne Thermostate laufen. Mit `heatml.synth` lassen sich realistische Fake-Scans erzeugen.
