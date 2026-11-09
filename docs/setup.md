# Einrichtung: Border Router, Minix, Commissioning

Anleitung für den Aufbau von Grund auf. Überblick und Architektur: [README](../README.md).

## 1. Border Router flashen (Windows, einmalig)

Voraussetzung ist ESP-IDF v5.5.4 unter `C:\clones\esp\esp-idf` (`install.ps1 esp32s3,esp32h2`). Das USB-C-Kabel muss am **Main Board (ESP32-S3)** stecken, nicht am H2.

```powershell
git submodule update --init
.\firmware\build.ps1 -Variant wifi -Port COM7      # oder -Variant ethernet
```

Das Skript baut zuerst das RCP-Image (`ot_rcp`, ESP32-H2), dann den Border Router (ESP32-S3), packt das RCP-Image hinein und flasht. Beim ersten Boot schreibt der S3 das RCP-Image selbst auf den H2.

**WLAN-Variante (aktiv):** Die Zugangsdaten stehen in `firmware/br/sdkconfig.wifi.local`; die Datei ist gitignored:

```
CONFIG_EXAMPLE_WIFI_SSID="MyHomeWiFi"
CONFIG_EXAMPLE_WIFI_PASSWORD="..."
```

Beim Boot verbindet sich der Border Router mit dem 2,4-GHz-WLAN und bildet automatisch ein Thread-Netz mit zufälligem Netzwerkschlüssel (Name und Kanal siehe `ot dataset active`). Der Leader-Status kommt etwa 45 s nach dem Boot.

**Ethernet-Variante:** `-Variant ethernet`, Kabel in die Fritzbox. Ohne Link startet Thread nicht.

Dataset sichern; es geht nie ins Repo:

```powershell
cargo run --release -p heatd -- thread dataset COM7   # schreibt secrets/thread_dataset.hex
```

OpenThread-Befehle brauchen in dieser Konsole das Präfix `ot`, zum Beispiel `ot state` → `leader` und `ot br state` → `running`. Beim Öffnen des COM-Ports nicht DTR/RTS setzen, sonst startet das Board neu; `heatd` macht das richtig.

## 2. Minix einrichten (Ubuntu 24.04)

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

## 3. Commissioning

Pro Thermostat, Node-IDs wie in `heatd.toml` (Beispiel: 5 Wohnzimmer, 6 Schlafzimmer, 7 Bad):

Der laufende `heatd` hält eine Sperre (`heatd.lock`) auf dem chip-tool-Speicher; `commission`, `read` und `set` brauchen dieselbe Sperre. Deshalb zuerst stoppen: ein `docker compose exec` in den laufenden Container scheitert mit "stop heatd first".

Docker:

```bash
cd ~/MLSmartMatterHeat/deploy/docker
docker compose stop heatd                                       # Pflicht: gibt die Sperre frei, `unless-stopped` startet ihn danach nicht von selbst
docker compose run --rm heatd commission 5 <pairing-code>          # 11-stelliger Code oder "MT:..." vom Thermostat
docker compose run --rm heatd read 5                            # Ist- und Solltemperatur
docker compose run --rm heatd set 5 21.5
docker compose up -d heatd
```

systemd:

```bash
sudo systemctl stop heatd                                # Pflicht: sonst "stop heatd first" (Lock auf dem chip-tool-Speicher)
./target/release/heatd commission 5 <pairing-code>          # 11-stelliger Code oder "MT:..." vom Thermostat
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
