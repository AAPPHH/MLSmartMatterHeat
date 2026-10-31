import json
from dataclasses import dataclass
from datetime import datetime
from pathlib import Path
from zoneinfo import ZoneInfo
import duckdb
import numpy as np
from scipy.optimize import curve_fit
from heattrain.thermal import DEMAND, SETPOINT, TEMP, ffill_at, interp, latest_meta, load_lake, read_arrows, weather_grid


@dataclass
class Room:
    name: str
    node: int
    s: np.ndarray
    sp: np.ndarray
    u: np.ndarray
    ueff: np.ndarray
    inferred: np.ndarray
    n_valid: int
    n_invalid: int


def runs(mask, gap=0):
    idx = np.flatnonzero(mask)
    out = []
    for i in idx:
        if out and i - out[-1][1] <= gap + 1:
            out[-1][1] = i + 1
        else:
            out.append([int(i), int(i) + 1])
    return [(a, b) for a, b in out]


def lag(x, k):
    return np.concatenate([np.full(k, np.nan), x[:-k]])


def load_events(path, kind):
    rows = []
    base = Path(path) / "parquet" / "events"
    if any(base.rglob("part.parquet")):
        con = duckdb.connect()
        rows += con.execute(f"SELECT epoch(ts), detail FROM read_parquet('{base.as_posix()}/**/part.parquet', hive_partitioning=false, union_by_name=true) WHERE kind::VARCHAR = '{kind}'").fetchall()
    t = read_arrows(path, "events")
    if t is not None:
        ts = t.column("ts").to_pylist()
        for i, (k, d) in enumerate(zip(t.column("kind").to_pylist(), t.column("detail").to_pylist())):
            if k == kind:
                rows.append((ts[i].timestamp(), d))
    seen, out = set(), []
    for ts, d in sorted(rows, key=lambda r: r[0]):
        key = (round(ts, 3), d)
        if key not in seen:
            seen.add(key)
            try:
                out.append((float(ts), json.loads(d) if d else {}))
            except ValueError:
                continue
    return out


class Boiler:
    def __init__(self, cfg, reports, weather, meta, supply_events=()):
        self.cfg, self.tz = cfg, ZoneInfo(cfg["timezone"])
        self.supply_events = list(supply_events)
        series = {}
        for ts, node, attr, v in reports:
            series.setdefault((node, attr), []).append((ts, v))
        series = {k: tuple(np.array(x) for x in zip(*sorted(v))) for k, v in series.items()}
        names = {n: r["name"] for r in latest_meta(meta).get("rooms", []) for n in r.get("nodes", [])}
        nodes = sorted({n for n, a in series if a == TEMP})
        times = np.concatenate([series[(n, TEMP)][0] for n in nodes])
        self.grid = np.arange(np.floor(times.min() / 60) * 60, times.max() + 1e-6, cfg["dt_s"])
        self.tout = weather_grid(weather, self.grid)[0]
        self.rooms = [self.room(n, names.get(n, f"node{n}"), series) for n in nodes if (n, DEMAND) in series]
        self.day, self.mod = self.local(self.grid)
        self.night_id = self.local(self.grid - 43200.0)[0]
        h = self.mod // 60
        self.night = (h >= cfg["night_hours"][0]) | (h < cfg["night_hours"][1])

    def room(self, node, name, series):
        c, g = self.cfg, self.grid
        t, v = series[(node, TEMP)]
        v = v / 100.0
        ok = (v > c["temp_valid"][0]) & (v < c["temp_valid"][1])
        t, v = t[ok], v[ok]
        s = interp(t, v, g)
        j = np.searchsorted(t, g)
        prev, nxt = t[np.clip(j - 1, 0, len(t) - 1)], t[np.clip(j, 0, len(t) - 1)]
        s[((nxt - prev) > c["max_gap_s"]) | (g < t[0]) | (g > t[-1])] = np.nan
        dt, dv = series[(node, DEMAND)]
        raw = ffill_at(dt, dv, g)
        raw = np.where(g < dt[0], np.nan, raw)
        invalid = raw > 100
        u = np.where(invalid, np.nan, raw / 100.0)
        sp = ffill_at(*series[(node, SETPOINT)], g) / 100.0 if (node, SETPOINT) in series else np.full(len(g), np.nan)
        wants = (sp - s) >= c["invalid_wants_k"]
        ueff = np.where(invalid, np.where(wants, 1.0, 0.0), u)
        return Room(name, node, s, sp, u, np.where(np.isnan(raw), np.nan, ueff), invalid, int((~invalid & np.isfinite(raw)).sum()), int(invalid.sum()))

    def local(self, grid):
        keys = (grid // 900).astype(np.int64)
        base = {}
        for k in np.unique(keys):
            d = datetime.fromtimestamp(int(k) * 900, self.tz)
            base[k] = (d.date().isoformat(), d.hour * 60 + d.minute)
        day = np.array([base[k][0] for k in keys])
        mod = np.array([base[k][1] for k in keys]) + (grid - keys * 900) / 60.0
        return day, mod

    def hhmm(self, i):
        return datetime.fromtimestamp(float(self.grid[i]), self.tz).strftime("%H:%M") if 0 <= i < len(self.grid) else None

    def iso(self, i):
        return datetime.fromtimestamp(float(self.grid[i]), self.tz).isoformat(timespec="minutes")

    def detect(self):
        c = self.cfg
        W = c["warm_window_min"]
        self.sup_runs, self.ns_runs = {}, {}
        for r in self.rooms:
            with np.errstate(invalid="ignore"):
                rise = r.s - lag(r.s, W)
                warm = rise >= c["warm_rise_k"]
            for a, b in runs(r.inferred & warm, c["merge_gap_min"]):
                r.ueff[max(a - W, 0):b] = 1.0
            with np.errstate(invalid="ignore"):
                opn = r.ueff >= c["open_frac"]
            r.rise = rise
            r.open = opn
            r.supply = opn & warm
            self.sup_runs[r.name] = []
            for a, b in runs(r.supply, c["merge_gap_min"]):
                lo = max(a - W, 0)
                seg = r.s[lo:a + 1]
                on = lo + len(seg) - 1 - int(np.nanargmin(seg[::-1])) if np.isfinite(seg).any() else a
                self.sup_runs[r.name].append({"start": a, "end": b, "onset": on})
            self.ns_runs[r.name] = []
            for a, b in runs(opn, c["merge_gap_min"]):
                hit = np.flatnonzero(r.supply[a:b])
                if len(hit):
                    b = max(a, a + int(hit[0]) - c["warm_window_min"])
                if b - a < c["nosupply_min"]:
                    continue
                s = r.s[a:b]
                if not np.isfinite(s).sum() > 0.8 * len(s):
                    continue
                gap = np.nanmedian(r.sp[a:b] - s)
                if np.nanmax(s - np.minimum.accumulate(np.nan_to_num(s, nan=np.inf))) < c["flat_k"] and (not np.isfinite(gap) or gap >= c["wants_min_k"]) and not r.inferred[a:b].any():
                    self.ns_runs[r.name].append({"start": a, "end": b, "gap_k": None if not np.isfinite(gap) else float(gap), "u_mean": float(np.nanmean(r.u[a:b])), "s_mean": float(np.nanmean(s))})
        n = len(self.grid)
        sup = np.zeros(n, bool)
        for r in self.rooms:
            sup |= r.supply
        self.sup_any = np.zeros(n, bool)
        for a, b in runs(sup, c["merge_gap_min"] + c["supply_hold_min"]):
            self.sup_any[a:b] = True
        self.ns_any = np.zeros(n, bool)
        for lst in self.ns_runs.values():
            for e in lst:
                self.ns_any[e["start"]:e["end"]] = True
        self.ns_any &= ~self.sup_any
        self.observed = np.zeros(n, bool)
        for r in self.rooms:
            self.observed |= r.open

    def fmt(self, a, b):
        return [self.hhmm(a), self.hhmm(b - 1)]

    def windows(self):
        out = {}
        for d in sorted(set(self.day)):
            m = self.day == d
            idx = np.flatnonzero(m)
            sup = [dict(zip(("von", "bis"), self.fmt(a, b)), minuten=b - a) for a, b in runs(self.sup_any & m, 0)]
            ns = []
            for name, lst in self.ns_runs.items():
                ns += [dict(zip(("von", "bis"), self.fmt(e["start"], e["end"])), raum=name, minuten=e["end"] - e["start"], sensor_mittel_c=round(e["s_mean"], 1), ventil_mittel_pct=round(100 * e["u_mean"])) for e in lst if m[e["start"]]]
            out[d] = {"minuten_daten": int(m.sum()), "minuten_ventil_offen": int((self.observed & m).sum()), "minuten_lieferung": int((self.sup_any & m).sum()), "minuten_ohne_lieferung": int((self.ns_any & m).sum()), "lieferfenster": sup, "ohne_lieferung": ns}
        return out

    def setback(self):
        c = self.cfg
        nights = []
        for nid in sorted(set(self.night_id)):
            g = np.flatnonzero(self.night_id == nid)
            starts = [(e["start"], name) for name, lst in self.ns_runs.items() for e in lst if self.night_id[e["start"]] == nid and self.night[e["start"]]]
            rec = {"nacht": nid, "beginn_bestaetigt": None, "beginn_nach": None, "ende": None, "dauer_h": None, "raeume": []}
            if starts:
                first, name = min(starts)
                before = np.flatnonzero(self.sup_any[g[0]:first]) + g[0]
                rec["beginn_bestaetigt"], rec["beginn_nach"] = self.hhmm(first), self.hhmm(int(before[-1]) + 1) if len(before) else None
                ons = [(e["onset"], n) for n, lst in self.sup_runs.items() for e in lst if e["onset"] > first and self.night_id[e["onset"]] == nid]
                if ons:
                    end, _ = min(ons)
                    rec["ende"], rec["dauer_h"] = self.hhmm(end), round((end - first) / 60, 1)
                    rec["_i"] = (first, end)
                    rec["raeume"] = self.recovery(end)
                rec["_first"] = first
            nights.append(rec)
        confirmed = [n for n in nights if n["beginn_bestaetigt"]]

        def spread(key):
            vals = []
            for n in confirmed:
                i = n["_first"] if key == "beginn" else n.get("_i", (None, None))[1]
                if i is not None:
                    vals.append((self.mod[i] + 720) % 1440 - 720 if key == "beginn" else self.mod[i])
            if not vals:
                return None
            m = float(np.mean(vals)) % 1440
            return {"n": len(vals), "mittel": f"{int(m // 60):02d}:{int(m % 60):02d}", "std_min": round(float(np.std(vals)), 1) if len(vals) > 1 else None}

        res = {"beginn": spread("beginn"), "ende": spread("ende"), "belastbar": len(confirmed) >= c["min_nights"]}
        for n in nights:
            n.pop("_first", None)
            n.pop("_i", None)
        return dict(res, naechte=nights)

    def recovery(self, start):
        c, out = self.cfg, []
        for r in self.rooms:
            a = np.flatnonzero(np.isfinite(r.s[start:]) & np.isfinite(r.sp[start:]))
            if not len(a):
                continue
            seg_s, seg_sp = r.s[start:], r.sp[start:]
            hit = np.flatnonzero(seg_s >= seg_sp - c["recovery_tol_k"])
            hit = hit[hit < c["recovery_max_min"]]
            end = min(start + c["recovery_max_min"], len(r.s))
            out.append({"raum": r.name, "minuten": int(hit[0]) if len(hit) else None, "sensor_max_c": round(float(np.nanmax(r.s[start:end])), 1), "sollwert_c": round(float(np.nanmedian(r.sp[start:end])), 1)})
        return out

    def fit(self, r, ev):
        c = self.cfg
        a = ev["onset"]
        limit = min(a + c["fit_max_min"], len(r.s))
        seg = r.s[a:limit]
        if not np.isfinite(seg).all() or len(seg) < c["fit_min_points"] * 3:
            return None
        cut = len(seg)
        run_max = np.maximum.accumulate(seg)
        peak = np.flatnonzero(run_max - seg >= c["fall_k"])
        if len(peak):
            cut = int(peak[0])
        closed = np.flatnonzero(r.ueff[a:limit] < 0.5)
        closed = closed[closed > c["fit_min_points"] * 3]
        if len(closed):
            cut = min(cut, int(closed[0]))
        seg, ue = seg[:cut], r.ueff[a:a + cut]
        if len(seg) < c["fit_min_points"] * 3 or seg.max() - seg[0] < c["fit_min_rise_k"] or np.nanmedian(ue[:max(10, len(ue) // 2)]) < c["full_open"]:
            return None
        t = np.arange(len(seg)) * 1.0
        try:
            p, _ = curve_fit(lambda x, inf, tau: inf - (inf - seg[0]) * np.exp(-x / tau), t, seg, p0=[seg.max() + 1, 15.0], bounds=([seg.max() - 0.5, c["tau_bounds"][0]], [seg.max() + c["max_extrapolation_k"], c["tau_bounds"][1]]))
        except (RuntimeError, ValueError):
            return None
        inf, tau = float(p[0]), float(p[1])
        rmse = float(np.sqrt(np.mean((inf - (inf - seg[0]) * np.exp(-t / tau) - seg) ** 2)))
        plateau = bool(seg[-1] - seg[max(0, len(seg) - 11)] < c["plateau_k"] and inf - seg[-1] < c["plateau_gap_k"])
        k = c["sensor_coupling"]
        t0 = float(seg[0])
        base = {"raum": r.name, "onset": self.iso(a), "aussen_c": round(float(self.tout[a]), 1), "t0_c": round(t0, 1), "tinf_c": round(inf, 1), "tau_min": round(tau, 1), "rate0_kph": round(60 * (inf - t0) / tau, 1), "rmse_k": round(rmse, 2), "minuten": len(seg), "plateau": plateau, "ventil": "abgeleitet" if r.inferred[a:a + len(seg)].mean() > 0.5 else "gemessen", "sensor_max_c": round(float(seg.max()), 1)}
        base["vorlauf_min_c"] = round(float(seg.max()), 1)
        if plateau:
            base.update(vorlauf_c=round(t0 + (inf - t0) / k, 1), vorlauf_von_c=round(t0 + (inf - t0) / c["coupling_range"][1], 1), vorlauf_bis_c=round(t0 + (inf - t0) / c["coupling_range"][0], 1))
        return base

    def ref_flow(self, tout):
        d = self.cfg["curve_ref"]
        phi = np.clip((d["indoor"] - tout) / (d["indoor"] - d["design_out"]), 0.02, 1.5)
        excess = ((d["flow"] + d["ret"]) / 2 - d["indoor"]) * phi ** (1 / d["n"])
        return float(d["indoor"] + excess + (d["flow"] - d["ret"]) * phi / 2)

    def flow(self):
        c = self.cfg
        events = [e for r in self.rooms for ev in self.sup_runs[r.name] if (e := self.fit(r, ev))]
        full = [e for e in events if "vorlauf_c" in e]
        for e in full:
            e["referenz_c"] = round(self.ref_flow(e["aussen_c"]), 1)
            e["abweichung_k"] = round(e["vorlauf_c"] - e["referenz_c"], 1)
        curve = None
        if len(full) >= c["min_curve_events"]:
            x, y = np.array([e["aussen_c"] for e in full]), np.array([e["vorlauf_c"] for e in full])
            if x.max() - x.min() >= c["min_curve_span_k"]:
                p = np.polyfit(x, y, 1)
                curve = {"steigung_k_pro_k": round(float(p[0]), 2), "achsenabschnitt_c": round(float(p[1]), 1), "n": len(x), "aussen_spanne_c": [round(float(x.min()), 1), round(float(x.max()), 1)], "wetterfuehrung": bool(p[0] <= -c["curve_slope_min"])}
        dev = [e["abweichung_k"] for e in full]
        verdict = None
        if len(dev) >= c["min_verdict_events"]:
            m = float(np.median(dev))
            verdict = "zu hoch" if m > c["flow_high_k"] else "zu niedrig" if m < -c["flow_low_k"] else "plausibel"
        return {"ereignisse": events, "anzahl_mit_plateau": len(full), "kurve": curve, "bewertung": verdict, "annahmen": {"sensor_kopplung": c["sensor_coupling"], "kopplung_spanne": c["coupling_range"], "referenzkurve": c["curve_ref"]}}

    def hydraulics(self):
        c = self.cfg
        groups = []
        for ev in sorted((dict(ev, room=r) for r in self.rooms for ev in self.sup_runs[r.name]), key=lambda e: e["onset"]):
            if groups and ev["onset"] - groups[-1][0]["onset"] <= c["group_min"]:
                groups[-1].append(ev)
            else:
                groups.append([ev])
        delays = {r.name: [] for r in self.rooms}
        events = []
        for g in groups:
            first = g[0]["onset"]
            if len({e["room"].name for e in g}) < 2:
                continue
            row = {"start": self.iso(first), "raeume": {}}
            for e in g:
                r = e["room"]
                o = e["onset"]
                while o > 0 and r.open[o - 1]:
                    o -= 1
                if r.name in row["raeume"]:
                    continue
                d = max(0, e["onset"] - max(first, o))
                delays[r.name].append(d)
                row["raeume"][r.name] = int(d)
            events.append(row)
        power, p_rows = {}, []
        for r in self.rooms:
            with np.errstate(invalid="ignore", divide="ignore"):
                m = r.supply & (r.ueff >= c["power_valve_min"]) & ~r.inferred
                v = (r.rise * 60.0 / c["warm_window_min"])[m] / r.u[m]
            power[r.name] = {"kph_pro_ventil": round(float(np.median(v)), 1) if m.sum() >= c["power_min_minutes"] else None, "minuten": int(m.sum())}
        vals = [p["kph_pro_ventil"] for p in power.values() if p["kph_pro_ventil"] is not None]
        med = float(np.median(vals)) if vals else None
        for name, p in power.items():
            p["relativ"] = round(p["kph_pro_ventil"] / med, 2) if med and p["kph_pro_ventil"] is not None else None
        rooms = {}
        for r in self.rooms:
            tot = r.n_valid + r.n_invalid
            share = r.n_invalid / tot if tot else 0.0
            rooms[r.name] = {"verzoegerung_min": float(np.mean(delays[r.name])) if delays[r.name] else None, "verzoegerung_n": len(delays[r.name]), "ventil_ungueltig_anteil": round(share, 2), "leistung": power[r.name]}
            rooms[r.name]["verzoegerung_min"] = None if rooms[r.name]["verzoegerung_min"] is None else round(rooms[r.name]["verzoegerung_min"], 1)
        return {"raeume": rooms, "anlaufereignisse": events, "median_leistung_kph": med}

    def cycling(self):
        c, out = self.cfg, {}
        w, lo, hi = c["cycle_window_min"], c["cycle_period_min"][0], c["cycle_period_min"][1]
        for r in self.rooms:
            res = {"fenster": 0, "mit_schwingung": 0, "perioden_min": [], "amplituden_k": []}
            for a in range(0, len(r.s) - w, c["cycle_step_min"]):
                s, u = r.s[a:a + w], r.u[a:a + w]
                if not np.isfinite(s).all() or not (np.nan_to_num(u) >= c["open_frac"]).all():
                    continue
                res["fenster"] += 1
                x = s - np.convolve(np.pad(s, (c["cycle_trend_min"] // 2,) * 2, mode="edge"), np.ones(c["cycle_trend_min"]) / c["cycle_trend_min"], mode="valid")[:w]
                f = np.fft.rfftfreq(w, 1.0)
                X = np.fft.rfft(x * np.hanning(w))
                band = (f >= 1 / hi) & (f <= 1 / lo)
                tot = float((np.abs(X[1:]) ** 2).sum())
                if tot <= 0 or not band.any():
                    continue
                frac = float((np.abs(X[band]) ** 2).sum() / tot)
                Xb = np.where(band, X, 0)
                xb = np.fft.irfft(Xb, w)
                sig = float(np.std(xb / np.maximum(np.hanning(w), 0.5)))
                core = xb[w // 6:-w // 6]
                crossings = int(((core[:-1] < 0) & (core[1:] >= 0)).sum())
                if frac >= c["cycle_frac_min"] and sig >= c["cycle_sigma_min_k"] and crossings >= c["cycle_min_crossings"]:
                    res["mit_schwingung"] += 1
                    k = np.flatnonzero(band)[np.argmax(np.abs(X[band]))]
                    res["perioden_min"].append(round(1 / f[k], 1))
                    res["amplituden_k"].append(round(2 * sig, 2))
            out[r.name] = {"fenster": res["fenster"], "mit_schwingung": res["mit_schwingung"], "periode_median_min": float(np.median(res["perioden_min"])) if res["perioden_min"] else None, "amplitude_median_k": float(np.median(res["amplituden_k"])) if res["amplituden_k"] else None}
        return {"raeume": out, "sensor_aufloesung_k": 0.5}

    def episodes_vs_heatd(self):
        ends = [e for e in self.supply_events if e[1].get("state") == "end"]
        return {"heatd_ereignisse": len(self.supply_events), "heatd_phasen_beendet": len(ends), "eigene_phasen": sum(len(v) for v in self.ns_runs.values())}

    def run(self):
        self.detect()
        c = self.cfg
        hours = len(self.grid) * c["dt_s"] / 3600
        doc = {"daten": {"von": self.iso(0), "bis": self.iso(len(self.grid) - 1), "stunden": round(hours, 1), "tage": round(hours / 24, 2), "raeume": {r.name: {"node": r.node, "ventil_ungueltig_anteil": round(r.n_invalid / max(r.n_valid + r.n_invalid, 1), 3)} for r in self.rooms}},
               "lieferung": self.windows(), "nachtabsenkung": self.setback(), "vorlauf": self.flow(), "hydraulik": self.hydraulics(), "takten": self.cycling(), "heatd_abgleich": self.episodes_vs_heatd()}
        doc["belastbar"] = {"tage": hours / 24 >= c["min_days"], "naechte": doc["nachtabsenkung"]["belastbar"], "vorlauf": doc["vorlauf"]["kurve"] is not None}
        doc["befunde"] = self.findings(doc)
        doc["tagesbericht"] = {d: self.daily(d, v, doc) for d, v in doc["lieferung"].items()}
        return doc

    def findings(self, doc):
        c, out = self.cfg, []
        for name, r in doc["hydraulik"]["raeume"].items():
            s = r["ventil_ungueltig_anteil"]
            if s >= c["invalid_warn"]:
                out.append({"thema": "geraet", "stufe": "hoch" if s >= c["invalid_crit"] else "mittel", "text": f"{name}: {s * 100:.0f} % der Ventilwerte sind ungueltig (134 = nicht verfuegbar). Das Thermostat meldet seinen Ventilstand nicht verlaesslich; Analyse dort nur ueber Temperaturverlauf. Batterie, Funkverbindung und Firmware pruefen."})
            if r["leistung"]["relativ"] is not None and not (c["power_rel"][0] <= r["leistung"]["relativ"] <= c["power_rel"][1]):
                out.append({"thema": "hydraulik", "stufe": "niedrig", "text": f"{name}: Heizleistung je Ventilanteil {r['leistung']['relativ']:.1f}x des Medians der Raeume ({r['leistung']['kph_pro_ventil']} K/h). Abweichung kann Radiatorgroesse, Luftvolumen oder Durchfluss sein (moegliche Abgleich-Frage)."})
            if r["verzoegerung_min"] is not None and r["verzoegerung_n"] >= 1 and r["verzoegerung_min"] >= c["delay_flag_min"]:
                out.append({"thema": "hydraulik", "stufe": "niedrig", "text": f"{name}: warmes Wasser kommt im Mittel {r['verzoegerung_min']:.0f} min nach dem ersten Heizkoerper an (n={r['verzoegerung_n']}); Hinweis auf langen Strang oder fehlenden hydraulischen Abgleich."})
        nos = sum(d["minuten_ohne_lieferung"] for d in doc["lieferung"].values())
        if nos:
            out.append({"thema": "versorgung", "stufe": "mittel", "text": f"{nos} min mit offenem Ventil, aber ohne Temperaturanstieg am Heizkoerper (keine Waermelieferung)."})
        v = doc["vorlauf"]
        if v["bewertung"] and v["bewertung"] != "plausibel":
            out.append({"thema": "vorlauf", "stufe": "mittel", "text": f"Geschaetzte Vorlauftemperatur wirkt {v['bewertung']} fuer die Aussentemperatur (Annahmen im Bericht)."})
        for name, t in doc["takten"]["raeume"].items():
            if t["fenster"] >= c["cycle_min_windows"] and t["mit_schwingung"] / t["fenster"] >= c["cycle_share_flag"]:
                out.append({"thema": "takten", "stufe": "niedrig", "text": f"{name}: Temperaturschwingung mit {t['periode_median_min']} min Periode in {t['mit_schwingung']} von {t['fenster']} Fenstern, Hinweis auf Brennertakten."})
        return out

    def daily(self, d, v, doc):
        lines = ["", "## Kesselversorgung (Analyse)", ""]
        if v["lieferfenster"]:
            lines.append("Waermelieferung erkannt: " + ", ".join(f"{w['von']}-{w['bis']}" for w in v["lieferfenster"]) + ".")
        else:
            lines.append("Keine Waermelieferung erkannt (kein Ventil offen mit Temperaturanstieg).")
        for w in v["ohne_lieferung"]:
            lines.append(f"- {w['raum']} {w['von']}-{w['bis']}: Ventil {w['ventil_mittel_pct']} % offen, aber kein Anstieg ({w['sensor_mittel_c']} C am Heizkoerper)")
        nacht = [n for n in doc["nachtabsenkung"]["naechte"] if n["nacht"] == d]
        for n in nacht:
            if n["beginn_bestaetigt"]:
                lines.append(f"Nachtabsenkung ab {n['beginn_bestaetigt']}" + (f", Ende {n['ende']}" if n["ende"] else ", Ende nicht beobachtet") + ".")
        return "\n".join(lines) + "\n"


def sec(title, body):
    return f"\n## {title}\n\n{body}\n"


def markdown(doc):
    d, L = doc["daten"], []
    L.append(f"# Kessel-Diagnose\n\nZeitraum {d['von']} bis {d['bis']} ({d['tage']} Tage, {d['stunden']} h). Alles hier ist aus den Heizkoerper-Thermostaten abgeleitet (Temperatur am Heizkoerper, Ventilstellung), nicht am Kessel gemessen.")
    b = doc["belastbar"]
    L.append(sec("Datenlage", f"- Genug Tage fuer Aussagen: {'ja' if b['tage'] else 'NEIN, viel zu wenig Daten'}\n- Genug Naechte fuer Absenkzeiten: {'ja' if b['naechte'] else 'nein'}\n- Genug Aufheizvorgaenge fuer Vorlauf/Heizkurve: {'ja' if b['vorlauf'] else 'nein'}\n\nAlle Zahlen sind Schaetzungen aus wenigen Ereignissen und nur als Hinweis zu lesen."))
    L.append(sec("Befunde", "\n".join(f"- [{f['stufe']}] {f['text']}" for f in doc["befunde"]) or "Keine Auffaelligkeiten erkannt."))
    rows = ["| Tag | Daten h | Ventil offen h | Lieferung h | offen ohne Lieferung h | Lieferfenster |", "|---|---|---|---|---|---|"]
    for day, v in doc["lieferung"].items():
        rows.append(f"| {day} | {v['minuten_daten'] / 60:.1f} | {v['minuten_ventil_offen'] / 60:.1f} | {v['minuten_lieferung'] / 60:.1f} | {v['minuten_ohne_lieferung'] / 60:.1f} | {', '.join(w['von'] + '-' + w['bis'] for w in v['lieferfenster']) or '-'} |")
    ns = [f"- {day} {w['raum']} {w['von']}-{w['bis']} ({w['minuten']} min, Ventil {w['ventil_mittel_pct']} %, Heizkoerper {w['sensor_mittel_c']} C)" for day, v in doc["lieferung"].items() for w in v["ohne_lieferung"]]
    L.append(sec("Wann liefert der Kessel", "Lieferung = mindestens ein Ventil offen und der Heizkoerper wird waermer. Ohne Lieferung = Ventil offen, Heizkoerper bleibt kalt. Wenn alle Ventile zu sind, ist nichts erkennbar.\n\n" + "\n".join(rows) + ("\n\nPhasen ohne Lieferung:\n" + "\n".join(ns) if ns else "")))
    nb = doc["nachtabsenkung"]
    nr = ["| Nacht | Letzte Lieferung | Erste Phase ohne Lieferung | Wieder Lieferung | Dauer h |", "|---|---|---|---|---|"]
    for n in nb["naechte"]:
        nr.append(f"| {n['nacht']} | {n['beginn_nach'] or '-'} | {n['beginn_bestaetigt'] or '-'} | {n['ende'] or '-'} | {n['dauer_h'] or '-'} |")
    rec = ["| Nacht | Raum | Minuten bis Sollwert | Sensor max C | Sollwert C |", "|---|---|---|---|---|"]
    rec += [f"| {n['nacht']} | {r['raum']} | {'schon erreicht' if r['minuten'] == 0 else r['minuten'] if r['minuten'] is not None else 'nicht erreicht'} | {r['sensor_max_c']} | {r['sollwert_c']} |" for n in nb["naechte"] for r in n["raeume"]]
    sp = ""
    for k, t in (("beginn", "Beginn"), ("ende", "Ende")):
        if nb[k]:
            sp += f"\n- {t}: Mittel {nb[k]['mittel']}, Streuung {str(nb[k]['std_min']) + ' min' if nb[k]['std_min'] is not None else 'nicht berechenbar (nur 1 Nacht)'}, n={nb[k]['n']}"
    L.append(sec("Nachtabsenkung und Morgen-Aufheizen", "Der Beginn liegt zwischen der letzten beobachteten Lieferung und der ersten beobachteten Phase ohne Lieferung (Ventile sind abends meist zu, deshalb kaum genauer messbar). Sensor sitzt am Heizkoerper und liegt ueber der Raumluft, Sollwert wird daher frueh 'erreicht'.\n\n" + "\n".join(nr) + sp + ("\n\nAufheizen:\n\n" + "\n".join(rec) if len(rec) > 2 else "")))
    v = doc["vorlauf"]
    vr = ["| Raum | Beginn | Aussen C | Start C | Plateau C | Zeitkonst. min | Rate K/h | Vorlauf-Schaetzung C | Referenz C |", "|---|---|---|---|---|---|---|---|---|"]
    for e in v["ereignisse"]:
        vr.append(f"| {e['raum']} | {e['onset']} | {e['aussen_c']} | {e['t0_c']} | {e['tinf_c']}{'' if e['plateau'] else ' (nicht erreicht)'} | {e['tau_min']} | {e['rate0_kph']} | {str(e.get('vorlauf_von_c', '-')) + '-' + str(e.get('vorlauf_bis_c', '-')) if e['plateau'] else 'mind. ' + str(e['sensor_max_c'])} | {e.get('referenz_c', '-')} |")
    k = v["kurve"]
    kt = f"Heizkurve: Steigung {k['steigung_k_pro_k']} K je K Aussentemperatur ueber {k['n']} Ereignisse (Aussen {k['aussen_spanne_c'][0]} bis {k['aussen_spanne_c'][1]} C); {'wettergefuehrt' if k['wetterfuehrung'] else 'keine erkennbare Wetterfuehrung'}." if k else "Heizkurve nicht bestimmbar (zu wenige Aufheizvorgaenge oder zu kleine Aussentemperatur-Spanne)."
    a = v["annahmen"]
    L.append(sec("Vorlauftemperatur (Schaetzung)", f"Methode: Beim Anlaufen der Lieferung bei fast offenem Ventil steigt die Temperatur am Heizkoerper-Thermostat exponentiell auf ein Plateau. Das Plateau ist der Messwert; die Vorlauftemperatur ist daraus nur hochgerechnet unter der Annahme, dass der Sensor {a['sensor_kopplung'] * 100:.0f} % (Spanne {a['kopplung_spanne'][0] * 100:.0f}-{a['kopplung_spanne'][1] * 100:.0f} %) des Abstands zwischen Raum und Wasser erreicht. Referenz = typische Heizkurve fuer Altbau 70/55 bei -12 C (Annahme).\n\n" + "\n".join(vr) + f"\n\n{kt}\n\nBewertung: {v['bewertung'] or 'nicht bewertbar'}"))
    h = doc["hydraulik"]
    hr = ["| Raum | Heizleistung K/h je Ventil | relativ | Verzoegerung min (n) | Ventilwerte ungueltig |", "|---|---|---|---|---|"]
    for n, r in h["raeume"].items():
        hr.append(f"| {n} | {r['leistung']['kph_pro_ventil'] if r['leistung']['kph_pro_ventil'] is not None else '-'} | {r['leistung']['relativ'] if r['leistung']['relativ'] is not None else '-'} | {r['verzoegerung_min'] if r['verzoegerung_min'] is not None else '-'} ({r['verzoegerung_n']}) | {r['ventil_ungueltig_anteil'] * 100:.0f} % |")
    L.append(sec("Heizkoerper und Hydraulik", "Heizleistung = Temperaturanstieg am Heizkoerper je Stunde geteilt durch Ventilanteil (nur gemessene Ventilwerte). Verzoegerung = Minuten, die ein Heizkoerper nach dem ersten warm wird, obwohl sein Ventil schon offen war. Beides haengt auch von Heizkoerper-Groesse und Raum ab, ist also nur ein Hinweis auf hydraulischen Abgleich.\n\n" + "\n".join(hr)))
    t = doc["takten"]["raeume"]
    tr = ["| Raum | Fenster | mit Schwingung | Periode min | Amplitude K |", "|---|---|---|---|---|"] + [f"| {n} | {r['fenster']} | {r['mit_schwingung']} | {r['periode_median_min'] or '-'} | {r['amplitude_median_k'] or '-'} |" for n, r in t.items()]
    L.append(sec("Brenner-Takten", "Gesucht werden Schwingungen mit 5-30 min Periode am Heizkoerper bei offenem Ventil (60-min-Fenster). Der Sensor meldet nur in 0.5-K-Schritten und etwa jede Minute, schnelle kleine Schwingungen sind damit kaum sichtbar.\n\n" + "\n".join(tr)))
    L.append(sec("Was wir zusaetzlich braeuchten", "- Anlege-Temperaturfuehler (Clamp-on, z. B. DS18B20 oder NTC) am Vorlauf und Ruecklauf direkt am Kessel: ersetzt die Vorlauf-Schaetzung durch eine Messung und liefert die Spreizung.\n- Brennerlaufzeit ueber Stromklemme (Strom-Sensor an der Brennerzufuhr) oder Vibrationssensor am Brenner: macht Takten und Lieferfenster messbar.\n- Oeltank-Fuellstand (Ultraschall- oder Druckfuehler): Verbrauch und Wirkungsgrad aus Oelmenge je Heizstunde.\n- Aussen-/Kellertemperatur am Kessel und ein Fuehler am Heizungs-Hauptventil/Mischer: zeigt, ob die Absenkung vom Regler oder vom Kessel kommt.\n- Raumlufttemperatur je Raum (Thermostat-Sensor sitzt am Heizkoerper): trennt Raumtemperatur von Heizkoerpertemperatur."))
    return "\n".join(L)


def html(boiler, doc, path):
    g = boiler.grid
    step = 5
    x = [datetime.fromtimestamp(float(t), boiler.tz).strftime("%Y-%m-%d %H:%M") for t in g[::step]]
    clean = lambda a: [None if not np.isfinite(v) else round(float(v), 2) for v in a[::step]]
    temps = [{"x": x, "y": clean(r.s), "name": r.name, "mode": "lines", "yaxis": "y"} for r in boiler.rooms] + [{"x": x, "y": clean(boiler.tout), "name": "aussen", "mode": "lines", "line": {"dash": "dot", "color": "gray"}}]
    shapes = [{"type": "rect", "xref": "x", "yref": "paper", "x0": boiler.iso(a).replace("T", " "), "x1": boiler.iso(b - 1).replace("T", " "), "y0": 0, "y1": 1, "fillcolor": col, "opacity": 0.15, "line": {"width": 0}} for mask, col in ((boiler.sup_any, "green"), (boiler.ns_any, "red")) for a, b in runs(mask, 0)]
    valves = [{"x": x, "y": clean(r.ueff * 100), "name": r.name, "mode": "lines", "line": {"shape": "hv"}} for r in boiler.rooms]
    ev = doc["vorlauf"]["ereignisse"]
    tt = np.linspace(-10, 20, 31)
    flow = [{"x": [e["aussen_c"] for e in ev if "vorlauf_c" in e], "y": [e["vorlauf_c"] for e in ev if "vorlauf_c" in e], "text": [e["raum"] for e in ev if "vorlauf_c" in e], "mode": "markers", "name": "Schaetzung"}, {"x": tt.tolist(), "y": [boiler.ref_flow(float(t)) for t in tt], "mode": "lines", "name": "Referenzkurve"}]
    hy = doc["hydraulik"]["raeume"]
    bars = [{"x": list(hy), "y": [r["leistung"]["kph_pro_ventil"] for r in hy.values()], "type": "bar", "name": "K/h je Ventil"}, {"x": list(hy), "y": [r["verzoegerung_min"] for r in hy.values()], "type": "bar", "name": "Verzoegerung min"}]
    figs = [("Temperatur am Heizkoerper (gruen = Lieferung, rot = offen ohne Lieferung)", temps, {"shapes": shapes, "yaxis": {"title": "C"}}), ("Ventil (inkl. abgeleitet bei ungueltig)", valves, {"yaxis": {"title": "%"}}), ("Vorlauf-Schaetzung vs. Aussentemperatur", flow, {"xaxis": {"title": "Aussen C"}, "yaxis": {"title": "Vorlauf C"}}), ("Heizleistung und Verzoegerung je Raum", bars, {"barmode": "group"})]
    body = "".join(f'<h3>{t}</h3><div id="f{i}" style="height:380px"></div>' for i, (t, _, _) in enumerate(figs))
    js = "".join(f"Plotly.newPlot('f{i}',{json.dumps(d)},{json.dumps(dict(l, margin={'t': 20}))});" for i, (_, d, l) in enumerate(figs))
    Path(path).write_text(f'<!doctype html><html lang="de"><head><meta charset="utf-8"><title>Kessel-Diagnose</title><script src="https://cdn.plot.ly/plotly-2.35.2.min.js"></script></head><body style="font-family:sans-serif;margin:16px">{body}<script>{js}</script></body></html>', encoding="utf-8")


def boiler(config, src, out_dir):
    cfg = config["boiler"]
    reports, weather, meta = load_lake(src)
    b = Boiler(cfg, reports, weather, meta, load_events(src, "supply"))
    doc = b.run()
    out = Path(out_dir)
    out.mkdir(parents=True, exist_ok=True)
    (out / "boiler.json").write_text(json.dumps(doc, indent=1, ensure_ascii=False, default=lambda o: o.item()), encoding="utf-8")
    (out / "boiler.md").write_text(markdown(doc), encoding="utf-8")
    html(b, doc, out / "boiler.html")
    return doc, out
