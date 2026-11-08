# HeatScan (Pixel Watch)

Build: `sh wear/build.sh` (Docker, Tests + signierte Release-APK unter `wear/app/build/outputs/apk/release/app-release.apk`).

## Selbst-Update (ab 4.1.0)

Die App prueft nur, wenn die Uhr laedt (`ACTION_POWER_CONNECTED` im laufenden Scan-Dienst) und der Dienst das Heim-WLAN gebunden hat (dasselbe Netz wie `Remote`; ohne WLAN passiert nichts, kein Bluetooth, kein Standardnetz). Hoechstens eine Pruefung alle 6 h, nach einem Fehler ein Wiederholungsversuch pro Ladevorgang (nach 10 min, ueber den Handler des Dienstes, kein Alarm).

Ablauf: `GET /watch/update` liefert `{"version_code","version_name","sha256","size"}` oder 404. Ist `version_code` groesser als die installierte, laedt die App `GET /watch/update.apk` ins Cache-Verzeichnis, prueft Groesse und SHA-256 und installiert per `PackageInstaller` (`MODE_FULL_INSTALL`, ab API 31 ohne Nutzerbestaetigung). Verlangt Android doch eine Bestaetigung, erscheint eine leise Benachrichtigung "Update bereit"; ein Tipp startet den Bestaetigungsdialog. Eine Version, die fehlschlaegt (Status FAILURE oder zweimal Bestaetigung noetig), wird uebersprungen, bis eine hoehere veroeffentlicht ist. Der Download ist auf `size` Bytes begrenzt. Die Datei im Cache wird nach jedem Versuch geloescht. Ergebnisse stehen im Log (`update_none`, `update_current`, `update_committed`, `update_status`, `update_failed`).

Serverseite: heatd liefert `update.json` und `update.apk` aus `[watch].update_dir` (Standard `/data/watch_update`).

Voraussetzungen:

- Das Selbst-Update funktioniert, weil die App sich selbst aktualisiert: ab API 31 braucht ein Update derselben App (gleiche Signatur) mit `UPDATE_PACKAGES_WITHOUT_USER_ACTION` keine Bestaetigung, sofern der Installer of record dies zulaesst. Ein normales `adb install` reicht daher. Optional kann die Erstinstallation den Installer setzen, dann bleibt die Bestaetigung sicher aus:

```
adb push app-release.apk /data/local/tmp/app-release.apk
adb shell pm install -r -i de.aapphh.heatscan /data/local/tmp/app-release.apk
```

- Neue APKs muessen mit demselben Zertifikat signiert sein (`wear/keystore.properties`), sonst lehnt Android das Update ab.
- Heim-WLAN: `heatscan.home=<SSID>` in `wear/local.properties` (gitignored) setzt den Standardwert fuer das Heim-WLAN; ohne Eintrag `HomeWiFi`.
- Die Berechtigungen `REQUEST_INSTALL_PACKAGES` und `UPDATE_PACKAGES_WITHOUT_USER_ACTION` stehen im Manifest; auf der Uhr muss "Unbekannte Apps installieren" fuer HeatScan erlaubt sein.
