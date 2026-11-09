# Raummodell (ein Modell, eine Kostenfunktion, zuerst im Schatten)

Erweitert `training/src/heattrain/thermal.py` (rc2-Kalman: x1 Heizkoerper/Wandmasse, x2 Luft, Nachbarkopplungen c_k, Ventil-Waermeeintrag). Keine neue Modelldatei. Online-Port: `crates/heatd/src/room_model.rs`. In diesem Schritt steuern Fensterdetektor (`windowml.py`/`window_model.rs`) und Boost-Dosierung (`boostlearn.py`/`boostlearn.rs`) weiter; das neue Modell laeuft nur mit und schreibt nichts an Thermostate oder Uhr.

## Zustaende und Gleichungen (Schritt h = 1 min, je Raum i)

Zustand x = [m, a, lambda]: m Masse (Heizkoerper+Waende), a Luft, lambda Verlustverhaeltnis (1 = normal, Fenster = 2..6). Gemeinsam fuer alle Raeume: s in {0,1}, Kessel liefert / liefert nicht.

- m' = m + h[ -(a1 u s + a2) m + a2 a + a1 u s ts ]   (u = Ventilanteil 0..1, ts = Vorlauf-Zieltemperatur, Tag/Nacht wie bisher)
- a' = a + h[ b1 m - (b1 + lambda amb + cn) a + lambda amb tout + b4 rad/100 + cs ],  amb = b2 + b3 wind  (cn, cs: Nachbarkopplungen wie `step_terms`)
- lambda' = 1 + phi (lambda - 1) + eta, phi = exp(-h/tau_l); begrenzt auf [0.5, 8]
- Die bisherigen Parameter (a1, a2, b1..b4, w, ts, ts_night, c_k) kommen unveraendert aus `fit_room`. Mit lambda = 1, s = 1 muss der neue Filter den alten `kalman` auf 1e-9 treffen (Regressionstest).

## Beobachtungen

1. Sensor y = w m + (1-w) a, Rauschen R = (0.5 K)^2 / 12 plus `noise_r` (0,5-K-Raster als Intervall-Messung behandelt).
2. Ventil-PI-Bedarf (Cluster 513, Attribut 8): u_obs = g_i (sp - a) + e, nur wenn 0 < u_obs < 1 (ungesaettigt), Rauschen sigma_u. g_i = `valve_gain` des Raums. Er liefert die Luft feiner als 0,5 K und zeigt, dass der Thermostat "mehr braucht". Saettigung wird uebersprungen, nicht als Wert gewertet.

## Schaetzung

- Verlustsprung: IMM mit zwei EKF-Zweigen je Raum und Kesselhypothese. Zweig "ruhig": eta klein. Zweig "Sprung": eta gross (Std q_J). Uebergang ruhig->Sprung mit p_jump je Minute, Sprung->ruhig mit 1/hold_j. Bilinear lambda*(tout-a) wird per Jacobi linearisiert (EKF, 3x3 in `[f64;9]`, kein Matrixcrate).
- Kessel: gemeinsame 2-Zustands-Markovkette w_s. Je Minute und Kesselhypothese s laeuft jeder Raum seine IMM-Baenke (4 Filter je Raum, 16 insgesamt). Die Raumlikelihoods L_i(s) werden multipliziert: w_s <- (P w)_s * prod_i L_i(s). Raumzustaende = mit w_s gemischt. Ein Kesselstillstand erklaert alle Raeume mit einem Wechsel; vier Fenster kosten p_jump^4. Ist u = 0 in allen Raeumen, sind die L_i gleich und w_s bleibt beim Prior.
- Ausgabe je Raum und Minute: lambda (Mittel, Std), m, a, P(Fenster) = P(lambda > lambda_w) aus dem Mischungs-CDF, P(Versorgung) = w_1, Innovationen (Sensor, Ventil).
- Online-Neuschaetzung der Parameter gibt es nicht. Der Filter schaetzt nur Zustaende; Parameter kommen aus dem Trainer.

## Datenluecken und Filterreset

- Ausfallmaske im Fit (neu in `thermal.py`): Minuten, in denen alle Knoten schweigen oder heatd stand (Luecken in `attribute_reports` ueber alle Knoten, `startup`/`shutdown`-Events, Boost-Holdout-Fall mit 13 h Ausfall), Neustartluecken und veraltete Knoten (`stale.after_s`) sind Messluecken: nur Vorhersageschritt, keine Innovation, kein Gewicht im Fit. Gemessene Zeit nach Luecke > `reset_gap_min`: Neustart des Raumzustands aus dem Sensor mit P0.
- Filterreset: jede nicht endliche Zahl, jede Kovarianz mit negativem Eigenwert, lambda ausserhalb der Grenzen oder Innovation > `reset_sigma` Standardabweichungen wiederholt: Zustand neu aus Sensor, Event `filter_reset` (`detail`: room, cause, ts, vorheriger Zustand). Nie stilles Weiterrechnen mit NaN. Der Tagesbericht zaehlt Resets je Raum.

## Kostenfunktion (alle paar Minuten, je Raum, Horizont 60 min)

Kosten(c) in Minuten Heizen = Energie + cold_cost_min * Kaelte + raise_cost_min * [c > aktueller Sollwert]

- Energie = Summe u_k s_k (Ventilminuten) im Rollout. Rollout wie `ThermalModel.predict`: u_k = clip(g (c - Sensor_k), 0, 1) mit Ventilkern/Verzoegerung des Heizkoerpers (25-70 min, ueber m).
- Kaelte = Summe max(0, Ziel_k - a_k) * anwesend_k [K min]; anwesend_k = Ziel laut Zeitplan gilt und Nutzer ist da oder wird erwartet (Praesenz, Uhr-Schlaf, Zeitplan).
- Strafe nur auf Sollwert-Erhoehungen, die heatd befiehlt; Senkungen kosten nichts. Nicht auf Ventilbewegungen.
- Erwartungswert ueber Modi: Kosten = (1-P) (Energie + Kaelte + Anheben | lambda = Mittel) + P(Fenster) (Energie + Anheben | lambda = lambda_J), jeweils gewichtet ueber P(Versorgung); Kaelte zaehlt nur im Modus "zu", denn bei offenem Fenster hat der Nutzer frische Luft statt Waerme gewaehlt, also gilt bei P(Fenster) nahe 1 nur noch Energie und Heizen sinkt immer (bei P = 0,5 waehlt der Planer die Probe).
- Kandidaten c: Frost, aktueller Sollwert, Ziel, Ziel +/- Raster 0,5 K, Boost (24 C, d min, nur wenn die Boost-Tore offen sind), Probe(d).
- Harte Regeln (einzige): Prognose-Spitze im Modus "ruhig" <= Ziel + 0,5 K sonst Kandidat verworfen; Boost nur 05-11 Wake oder >= 60 min weg; kein Boost <= 180 min nach Fenster (bleibt vorerst); BR-Failsafe; Knoten-Selbstheilung.
- Zwei Regler in `heatd.toml` `[room_model]`: `raise_cost_min` (Vorgabe 15), `cold_cost_min` (Vorgabe 1,0: 1 K zu kalt eine Stunde = 60 Heizminuten). Daneben nur Betriebsparameter: `enabled`, `shadow`, `tick_min` (5), `model_dir`, `reload_min`.

## Fenster, Probe, Boost, Kessel als Abfragen

- Fenster offen: lambda hoch -> Heizen teuer und wirkungslos -> Frost gewinnt. Kein Detektor, kein Timer.
- "Noch offen?" (Probe): Kandidat Probe(d) = Ziel fuer d min, dann Frost. Wert der Information: EVOI(d) = rho(d) * (C0 - sum_m P_m min_c C(c|m)) - Probekosten(d). C0 = bester Kandidat ohne Information, rho(d) = Trennschaerfe der Modi nach d min (Sensor/Ventil-Unterschied der beiden Rollouts relativ zum Rauschen, 1 - 2*Phi(-d'/2)). Probe wird gewaehlt, wenn max_d EVOI > 0; Dauer und Haeufigkeit folgen aus der Rechnung (P bleibt hoch, Prior zerfaellt mit hold_j).
- Boost: Kandidat Boost(d), d aus {5,...,20}; gewaehlt nur wenn guenstigster Weg zum Ziel und Tore offen. Ersetzt spaeter `boostlearn` (Dosis, d_safe, p_max).
- no_supply: w_1 < 0,5 -> Rollout ohne Waerme; Sollwert-Anheben bringt nichts, also kein Anheben (Kosten sind bei gleicher Kaelte hoeher). Ersetzt spaeter den Deckel/PI-Reset in `supply_step`.

## Bundle (Trainer -> heatd)

`/models/room_model/<YYYYmmddTHHMMSSZ>/bundle.json`, `kind: room_model`, `version`. Nur Zeitstempelordner, der neueste gueltige gewinnt, Rest wie bei `window_model::try_load`. Inhalt: `dt_s`, `rooms.<name>`: `struct`, `params` (a1,a2,b1,b2,b3,b4,w,ts,ts_night), `c` je Kopplungsart, `neighbours` (Raumnamen je Art), `valve_gain`, `sigma_u`, `noise_r`, `p0`, `q_r`, `q_a`; `shock`: `tau_l`, `q_calm`, `q_jump`, `p_jump`, `hold_min`, `lambda_jump`, `lambda_w`; `house`: `p_supply_switch` je Richtung, `reset_gap_min`, `reset_sigma`, `night_hours`, `timezone`. Der Lader lehnt NaN, p ausserhalb (0,1), lambda_w <= 1 ab. Trainer: `python -m heattrain roommodel <lake> <models_dir>` (neues Modul nur wenn `thermal.py` ~1000 Zeilen ueberschreitet; sonst dort). Parameter der Schockschicht: p_jump/hold_j/q_J per Maximum-Likelihood auf ausfallbereinigten Daten ohne bestaetigte Fenster; lambda_J, lambda_w so, dass bestaetigte Fenster (ground truth + Uhr-Antworten "ja") erkannt und "nein"-Antworten nicht erkannt werden (Kompromiss in der Trainerausgabe, nicht Teil des Bundles).

## Rust-Port und Paritaet

`room_model.rs`: `Bundle::parse/load`, `RoomFilter` (IMM-EKF, `step(inputs) -> Out`), `House` (Kessel-Kette), `Bank::step(ts, inputs)`, `decide(room, ...) -> Shadow`. Eingaben je Minute: Sensor (Raster), Ventil (cl 513 attr 8), Sollwert, Aussen/Wind/Strahlung (Wetterfeed wie `window.rs`), Nachbartemperaturen, Praesenz. Fixture `crates/heatd/testdata/room_model_parity.json` aus `training/tests/make_room_model_fixture.py`: 3 Raeume x 1500 Minuten inkl. Fensterspruenge, Kesselstopp, Messluecke, Reset; Ausgaben (a, m, lambda, P(Fenster), P(Versorgung), gewaehlter Sollwert, Kostenterme) auf 1e-6. Rust-Test liest dieselbe Datei.

## Schattenprotokoll

Neue additive Tabelle `shadow_decisions` (nicht `decisions`, damit Soll/Ist-, Budget- und Dynamikauswertungen unberuehrt bleiben): ts, room, setpoint_live (was heatd gerade will), setpoint_shadow, raise (Bool), probe_min, boost_min, cost_energy, cost_cold, cost_raise, cost_live (Kosten des Live-Sollwerts im Modell), lambda, lambda_std, mass_c, air_c, p_supply, p_window, p_window_live (alter Detektor offen), model_version, why (ein deutscher Satz), z.B. `Schlafzimmer: Verlust 3x normal -> Fenster offen -> Kälte zählt nicht, Heizen wäre Verlust -> 8 °C, spart ca. 40 min Heizen`. Zeile bei Wechsel des Schatten-Sollwerts oder alle 30 min, nicht jede Minute. Event `filter_reset` in `events`.

## Tagesbericht (Abschnitt "Raummodell Schatten")

- 20-min-Prognosefehler je Raum (RMSE Sensor, Ursprung = Schattenzeile) gegen Persistenz (letzte tatsaechliche Messung, nicht Filterschaetzung); daneben nur zur Groessenordnung das eigene Residuum des Live-Fensterdetektors (keine 20-min-Prognose). Bei N < 10 als unsicher markiert.
- Fenstererkennung: P(Fenster) > 0,5 gegen `ground_truth/windows.jsonl` (Treffer binnen 20 min) und gegen Uhr-Antworten in `labels` (source `window`, answer ja/nein): Treffer, Fehlalarme, Latenz; Hinweis kleine Fallzahl (N ausweisen, keine Prozente unter N=10).
- Versorgung: Schatten-Kesselstillstaende (w_1 < 0,5 laenger 20 min) gegen vorhandene `supply`-Events (start/end).
- Schatten-Erhoehungen (Schatten-Sollwert steigt gegenueber der vorigen Schattenzeile des Raums) gegen tatsaechliche Erhoehungen je Raum und Tag; Summe geschaetzter Heizminuten Schatten gegen live (Modellrechnung, kein Messwert).
- Filterresets je Raum. Energie und Komfort gelten erst nach Umschalten als messbar, nicht aus dem Schatten.

## Regeln, die spaetere Schritte entfernen

- Fenster: `[window]` hold_min, rearm_min, room_z, setpoint_guard_min, Schwellen/Hausschwelle, `window_model.rs`/`windowml.py`-Ridge-Detektor, Suppression-Logik (`window.rs::alarm`); Fensterfrage-Timer (`prompt_*`) wird Probe, Uhr-Antworten bleiben als Etiketten.
- Versorgung: `[supply]` valve_pct, min_min, flat_k_per_h, recover_*, cap_k, learn_days, overshoot_*, reset_*; `supply_step`, `pi_reset`, `Windows::supply_tick`.
- Boost: `boostlearn.rs/.py` (Dosis, Thompson, d_safe, p_max), `[boost]` trigger_window_min, min_rise_k, measure_min, arm_timeout_min, wind_default, resume_cap_min, Dosis 5..20 und 24-C-Festwert.
- Budget: `raise_budget_per_hour`, `Budget` in `rules.rs` (ersetzt durch raise_cost_min). `after_window_min` bleibt.
- Bleiben: morning_hours, homecoming_min_away_min, after_window_min, Failsafe, BR-Watchdog, `node_health`, `min_write_interval_min`, Schutz vor Ziel + 0,5 K.

## Umsetzung Schritt 1 (nur Schatten)

Code: Filter, Ausfallmaske und Planer in `training/src/heattrain/thermal.py` (`RoomFilter`, `House`, `plan`, `build_rooms` mit `events`), Anpassung/Export in `training/src/heattrain/roomfit.py` (`python -m heattrain roommodel <lake> <models_dir>`, in `training/nightly-container.sh` und `deploy/pc-train/nightly.sh`), Online-Port `crates/heatd/src/room_model.rs` (Bundle, Filter, Planer), Schattenanbindung `crates/heatd/src/room_shadow.rs` (Eingaben aus `on_report`, Minutentakt, Zeilen, `/api/state` Block `room_model`, Reload). Hook `room_model_step` in `rules.rs` nimmt `&[Desired]` und kann nichts schreiben; ein Test vergleicht Thermostat-Schreibzugriffe und `decisions`-Zeilen mit und ohne Modell. Die Dateien sind getrennt, weil `thermal.py` und `room_model.rs` sonst ueber ~1000 Zeilen gewachsen waeren; das Modell selbst bleibt in `thermal.py`.

Abweichungen und Festlegungen gegenueber dem Entwurf:

- Ventilbeobachtung: `u = g (sp - Sensor) + e` mit Sensor = w m + (1-w) a, nicht gegen die reine Luft. Der Thermostat regelt auf seinen Sensor und `valve_gain` ist so gefittet; gegen die Luft waere die Beobachtung um g w (m - a) verschoben und zog im Test die Luftschaetzung auf den Sensorwert.
- Komfort (`Kaelte`) wird am Sensor gemessen (Ziel gilt dem Thermostat), die harte Regel "nie ueber Ziel + 0,5 K" ebenfalls am Sensor im Modus "ruhig"; wenn der Sensor schon hoeher liegt, gilt dessen aktueller Wert als Grenze. Frost bleibt immer zulaessig.
- Neustart/Reset-Zustand (`balance_seed`): Luft und Masse aus der Gleichgewichtsbilanz bei lambda = 1 (Sensor, Aussen, Nachbarn), nicht m = a = Sensor; sonst starteten Resets mit falscher Luft/Masse-Aufteilung und liefen in Folgeresets. Der Regressionstest gegen den alten `kalman` setzt `balance_seed = 0`, `r_grid = 0`, kein Sprung, kein Kesselwechsel.
- Reset-Hygiene (`reset_gap_min`, `reset_sigma`, `reset_n`) sind Filterparameter, keine Regelgrenzen. Ursachen im Event `filter_reset`: `nan`, `psd`, `lambda` (Mittel ausserhalb 0,5..8), `innovation`, `gap`, `restart_gap` (Dienst stand laenger als 30 min). Eingaben ohne Wetter/Ventil frieren den Filter ein (kein Schritt), die Luecke zaehlt bis `reset_gap_min`.
- Kessel- und Sprungparameter: `p_up`, `p_down`, `p_supply0` sind feste Annahmen in `CONFIG["roommodel"]["house"]` (nicht gefittet); `q_calm`, `p_jump`, `q_jump` per Maximum-Likelihood-Gittersuche auf den letzten `shock.days` Tagen ohne bestaetigte Fenster (ground truth und Ja-Antworten, +-30 min) und ohne Ausfallminuten; `lambda_w` per Gitter gegen ground truth/Antworten, `lambda_jump` Median des geschaetzten lambda in bestaetigten Fenstern. Die Gittersuche ist koordinatenweise (`passes` Durchlaeufe); `roomfit.BatchHouse` rechnet dafuer alle Gitterpunkte (Produktgitter bis `batch_max`) und alle Raeume in einem numpy-Durchlauf und spielt die Koordinatensuche exakt aus dem Cache nach; `House` in `thermal.py` bleibt die skalare Referenz (Test vergleicht beide inkl. Resets). Ein einziger Abschlusslauf ohne Maske liefert Resets, Likelihood und Prognosefehler, da `lambda_w`/`lambda_jump` den Filter nicht beeinflussen.
- Probe: EVOI-Probenkosten sind der Mehrverbrauch der Probe gegenueber dem besten Kandidaten ueber die ersten d Minuten, nach unten bei 0 begrenzt (sonst wirkten kurzfristig guenstigere Proben als Wert).
- Eingaben im Daemon wie im Training: Ventil und Wetter gelten mit dem letzten Wert weiter (der Filter rechnet durch Luecken weiter, nur die Ventilbeobachtung faellt weg); ohne je gesehene Aussentemperatur ruht der Schatten und meldet `shadow_state` = `no_weather`. Ein Bundle-Reload setzt alle Raeume mit `filter_reset` cause `reload` zurueck.
- `[room_model]` in `heatd.toml` kennt nur `enabled` (Vorgabe aus), `model_dir`, `raise_cost_min`, `cold_cost_min`; Schatten ist fest (kein Schalter), Takt 5 min, Reload 10 min, Zeilen mindestens alle 30 min als Konstanten.
- Zusatzspalten in `shadow_decisions`: `sensor_c`, `forecast_c` (20-min-Prognose bei Live-Sollwert), `resid_live_k` (Residuum des alten Detektors, Prognose minus Messung) und `kind`; sie machen den Prognosefehler im Tagesbericht ohne Neuberechnung messbar. Zeilen entstehen auch beim Wechsel von P(Fenster) ueber/unter 0,5.
- Boost-Tore im Schatten: 05-11 Uhr nach Wachwerden oder Rueckkehr nach >= `homecoming_min_away_min` Abwesenheit (Fenster der gleichen Laenge), kein Fenster seit `after_window_min`, kein eigenes P(Fenster) >= 0,5 und Kessel p >= 0,5.
