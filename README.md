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

**WLAN-Variante:** Der Border Router öffnet beim ersten Start den Hotspot `ESP-ThreadBR-XXXX`. Verbinde dich mit dem Handy, öffne `http://192.168.4.1` und trag dein 2,4-GHz-WLAN ein. Die Daten landen nur im NVS des Boards. Danach bildet er automatisch ein Thread-Netz mit zufälligem Netzwerkschlüssel.

**Ethernet-Variante:** Kabel in die Fritzbox, fertig. Ohne Link startet Thread nicht.

Dataset sichern; es geht nie ins Repo:

```powershell
cargo run --release -p heatd -- thread dataset COM7   # schreibt secrets/thread_dataset.hex
```

Prüfen in der seriellen Konsole: `state` → `leader`.

## 2 · Minix einrichten (Ubuntu 24.04)

```bash
sudo apt install -y build-essential pkg-config libssl-dev bluez
curl https://sh.rustup.rs -sSf | sh
sudo snap install chip-tool && sudo snap connect chip-tool:process-control
sudo cp deploy/99-thread-ra.conf /etc/sysctl.d/ && sudo sysctl --system   # Routen ins Thread-Netz annehmen
git clone https://github.com/AAPPHH/MLSmartMatterHeat.git ~/MLSmartMatterHeat && cd ~/MLSmartMatterHeat
cargo build --release -p heatd
cp heatd.example.toml heatd.toml          # Räume, Node-IDs, Zeitpläne anpassen
mkdir -p secrets && scp <pc>:.../secrets/thread_dataset.hex secrets/
```

In `heatd.toml` setzt du `[chip_tool] storage_dir = "/home/aapphh/snap/chip-tool/common"`, weil der Snap nur dort schreiben darf.

```bash
sudo cp deploy/heatd.service /etc/systemd/system/ && sudo systemctl enable --now heatd
curl http://localhost:8080/health
```

## 3 · Commissioning am Freitag 🗓

Pro Thermostat, Node-IDs wie in `heatd.toml` (Beispiel: 5 Wohnzimmer, 6 Schlafzimmer, 7 Bad):

```bash
sudo systemctl stop heatd                                # chip-tool-Speicher nicht parallel nutzen
./target/release/heatd commission 5 34970112332          # 11-stelliger Code oder "MT:..." vom Thermostat
./target/release/heatd read 5                            # Ist- und Solltemperatur
./target/release/heatd set 5 21.5
sudo systemctl start heatd
```

Der Minix spricht das Thermostat per Bluetooth an (`hci0`), übergibt das Thread-Dataset, und danach läuft alles über den Border Router. Das Thermostat muss dabei frisch zurückgesetzt und im Pairing-Modus sein und nah am Minix liegen.

## 4 · Uhr & Training ⌚

```bash
sh wear/build.sh                                         # Tests + signierte Release-APK (Docker)
adb connect <uhr-ip>:<port> && adb install -r wear/app/build/outputs/apk/release/app-release.apk
```

Die APK liegt auch im GitHub-Release. In der App gibst du die Standortberechtigung frei, prüfst die Server-URL (`http://192.168.178.29:8080`) und tippst auf Start. Dann fügst du die Kachel **HeatScan** hinzu.

**Training:**
1. In jedem Raum ein paar Mal 10–20 min verbringen und vorher auf der Uhr den Raum antippen; danach „kein Label“.
2. Snapshot ziehen und trainieren:
   ```bash
   curl -X POST http://localhost:8080/api/snapshot       # data/snapshot.duckdb
   cd ml && uv run python -m heatml train                # models/room_model.onnx + .json
   sudo systemctl restart heatd
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
