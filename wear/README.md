# HeatScan (Pixel Watch)

Build: `sh wear/build.sh` (Docker, Tests + signierte Release-APK unter `wear/app/build/outputs/apk/release/app-release.apk`).

## Selbst-Update (ab 4.1.0)

Die App prueft nur, wenn die Uhr laedt (`ACTION_POWER_CONNECTED` im laufenden Scan-Dienst) und der Dienst das Heim-WLAN gebunden hat (dasselbe Netz wie `Remote`; ohne WLAN passiert nichts, kein Bluetooth, kein Standardnetz). Hoechstens eine Pruefung alle 6 h, nach einem Fehler ein Wiederholungsversuch pro Ladevorgang (nach 10 min, ueber den Handler des Dienstes, kein Alarm).

Ablauf: `GET /watch/update` liefert `{"version_code","version_name","sha256","size"}` oder 404. Ist `version_code` groesser als die installierte, laedt die App `GET /watch/update.apk` ins Cache-Verzeichnis, prueft Groesse und SHA-256 und installiert per `PackageInstaller` (`MODE_FULL_INSTALL`, ab API 31 ohne Nutzerbestaetigung). Verlangt Android doch eine Bestaetigung, erscheint eine leise Benachrichtigung "Update bereit"; ein Tipp startet den Bestaetigungsdialog. Die Datei im Cache wird nach jedem Versuch geloescht. Ergebnisse stehen im Log (`update_none`, `update_current`, `update_committed`, `update_status`, `update_failed`).

Serverseite: heatd liefert `update.json` und `update.apk` aus `[watch].update_dir` (Standard `/data/watch_update`).

Voraussetzungen:

- Android erlaubt die Installation ohne Nutzeraktion nur, wenn die App selbst als Installer eingetragen ist (installer of record) und der Update-Lauf dieselbe Signatur hat. Die Erstinstallation daher einmalig so:

```
adb push app-release.apk /data/local/tmp/app-release.apk
adb shell pm install -r -i de.aapphh.heatscan /data/local/tmp/app-release.apk
```

  `pm install -i <installer>` setzt den Installer; `adb install -r -i de.aapphh.heatscan app-release.apk` reicht die Option bei aktuellen platform-tools an `pm` durch. Ein normales `adb install` traegt `com.android.shell` ein, dann bleibt es bei der Bestaetigungs-Benachrichtigung.
- Neue APKs muessen mit demselben Zertifikat signiert sein (`wear/keystore.properties`), sonst lehnt Android das Update ab.
- Die Berechtigung `REQUEST_INSTALL_PACKAGES` steht im Manifest; auf der Uhr muss "Unbekannte Apps installieren" fuer HeatScan erlaubt sein.
