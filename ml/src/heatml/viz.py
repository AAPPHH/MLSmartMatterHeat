from datetime import datetime
from pathlib import Path
import numpy as np
import plotly.graph_objects as go
from plotly.subplots import make_subplots
from sklearn.decomposition import PCA
from heatml.data import Snapshot

VIZ = {
    "title": "Heat Dashboard",
    "bg": "#0d0d0d", "surface": "#1a1a19", "ink": "#f0efec", "ink2": "#c3c2b7", "muted": "#898781", "grid": "#2c2c2a", "axis": "#383835",
    "cats": ["#3987e5", "#d95926", "#199e70", "#c98500", "#d55181", "#008300", "#9085e9", "#e66767"],
    "unlabelled": "#6b6a66", "seq": ["#184f95", "#256abf", "#3987e5", "#6da7ec", "#9ec5f4", "#cde2fb"],
    "away": "#c98500", "ok": "#199e70",
    "test_until": "2026-10-28 06:00:00", "floor": -100, "top_aps": 20, "fingerprint_aps": 12,
    "constant_max_unique": 3, "state_pad_s": 3600,
    "sections": {
        "timeline": "Zeitachse", "heat": "WLAN-Heatmap", "fingerprint": "Raum-Fingerabdrücke",
        "map": "Karte der Messungen", "thermostat": "Thermostat-Verlauf",
    },
}


def band(freq):
    return "2.4 GHz" if freq < 3000 else "5 GHz" if freq < 5925 else "6 GHz"


class Data:
    def __init__(self, path, config):
        snap = Snapshot(path)
        self.scan_ids, self.ts, self.labels, self.freq, self.aps, self.counts = [], [], [], {}, {}, {}
        for sid, ts, label, bssid, rssi, freq in snap.query("SELECT scan_id, ts, label, bssid, rssi, freq FROM scans ORDER BY ts, scan_id"):
            if sid not in self.aps:
                self.scan_ids.append(sid)
                self.ts.append(ts)
                self.labels.append(label)
                self.aps[sid] = {}
            self.aps[sid][bssid] = max(rssi, self.aps[sid].get(bssid, rssi))
            self.freq[bssid] = freq
        self.rooms = sorted({l for l in self.labels if l})
        self.state = snap.query("SELECT ts, motion, power, n_aps, away FROM device_state ORDER BY ts")
        self.readings = snap.query("SELECT ts, node, value FROM readings WHERE attribute = 'local_temperature' ORDER BY ts")
        self.setpoints = snap.query("SELECT ts, node, room, value FROM setpoints ORDER BY ts")
        for ap in self.aps.values():
            for b in ap:
                self.counts[b] = self.counts.get(b, 0) + 1
        self.top = sorted(self.counts, key=lambda b: -self.counts[b])
        if self.ts:
            lo, hi, pad = min(self.ts), max(self.ts), config["state_pad_s"]
            self.state = [r for r in self.state if (lo - r[0]).total_seconds() < pad and (r[0] - hi).total_seconds() < pad]

    def color(self, label, config):
        return config["unlabelled"] if not label else config["cats"][self.rooms.index(label) % len(config["cats"])]

    def ap_name(self, bssid):
        return f"{bssid[-8:]} · {band(self.freq[bssid])}"

    def matrix(self, bssids, fill):
        return np.array([[self.aps[s].get(b, fill) for s in self.scan_ids] for b in bssids], dtype=float).reshape(len(bssids), len(self.scan_ids))


class Dashboard:
    def __init__(self, path, config=VIZ):
        self.c = config
        self.d = Data(path, config)
        self.first = True

    def layout(self, fig, height=440, **kw):
        c = self.c
        fig.update_layout(
            paper_bgcolor=c["surface"], plot_bgcolor=c["surface"], font=dict(color=c["ink2"], family="system-ui, Segoe UI, sans-serif", size=12),
            height=height, margin=dict(l=70, r=20, t=40, b=40), hoverlabel=dict(bgcolor=c["bg"], font_color=c["ink"]),
            legend=dict(orientation="h", y=1.1, x=0, font=dict(color=c["ink2"])), **kw)
        fig.update_xaxes(gridcolor=c["grid"], linecolor=c["axis"], zeroline=False)
        fig.update_yaxes(gridcolor=c["grid"], linecolor=c["axis"], zeroline=False)
        return fig

    def empty(self, text):
        fig = go.Figure()
        fig.add_annotation(text=text, showarrow=False, font=dict(color=self.c["muted"], size=14))
        fig.update_xaxes(visible=False)
        fig.update_yaxes(visible=False)
        return self.layout(fig, height=160)

    def html(self, fig):
        out = fig.to_html(full_html=False, include_plotlyjs=True if self.first else False, config={"displaylogo": False, "responsive": True})
        self.first = False
        return out

    def testline(self, fig):
        t = self.c["test_until"]
        fig.add_shape(type="line", x0=t, x1=t, y0=0, y1=1, yref="paper", line=dict(color=self.c["muted"], dash="dot", width=1))
        fig.add_annotation(x=t, y=1, yref="paper", text="Testlabels bis hier", showarrow=False, xanchor="right", yanchor="bottom", font=dict(color=self.c["muted"], size=11))

    def kpis(self):
        d = self.d
        n = len(d.ts)
        last = max(d.ts) if n else None
        today = sum(1 for t in d.ts if t.date() == datetime.now().date())
        per_room = [(r, sum(1 for l in d.labels if l == r)) for r in d.rooms]
        share = " · ".join(f"{r} {100 * k / n:.0f}%" for r, k in per_room) if n and per_room else "keine Labels"
        unl = f"ohne Label {100 * sum(1 for l in d.labels if not l) / n:.0f}%" if n and per_room else ""
        tiles = [
            ("Messungen gesamt", f"{n}", ""), ("Messungen heute", f"{today}", ""),
            ("Gelabelte Anteile pro Raum", share, unl), ("Letzte Messung", last.strftime("%d.%m. %H:%M:%S") if n else "-", ""),
            ("WLANs gesehen", f"{len(d.counts)}", ""),
        ]
        return "".join(f'<div class="tile"><div class="k">{k}</div><div class="v{" sm" if len(v) > 14 else ""}">{v}</div><div class="s">{s}</div></div>' for k, v, s in tiles)

    def timeline(self):
        d, c = self.d, self.c
        if not d.ts:
            return self.empty("Keine Messungen vorhanden")
        fig = make_subplots(rows=2, cols=1, shared_xaxes=True, row_heights=[0.8, 0.2], vertical_spacing=0.06)
        for label, name in [(r, r) for r in d.rooms] + [(None, "ohne Label")]:
            idx = [i for i, l in enumerate(d.labels) if l == label]
            if idx:
                fig.add_trace(go.Scatter(
                    x=[d.ts[i] for i in idx], y=[len(d.aps[d.scan_ids[i]]) for i in idx], mode="markers", name=name,
                    marker=dict(color=d.color(label, c), size=8, line=dict(color=c["surface"], width=1)),
                    hovertemplate="%{x|%d.%m. %H:%M:%S}<br>" + name + "<br>%{y} WLANs<extra></extra>"), row=1, col=1)
        fig.update_yaxes(title_text="WLANs je Messung", row=1, col=1)
        if d.state:
            fig.add_trace(go.Scatter(
                x=[r[0] for r in d.state], y=[1] * len(d.state), mode="markers", name="Heartbeat", showlegend=False,
                marker=dict(symbol="square", size=9, color=[c["away"] if r[4] else c["ok"] for r in d.state]),
                customdata=[[r[1] or "-", r[2] or "-", "ja" if r[4] else "nein"] for r in d.state],
                hovertemplate="%{x|%d.%m. %H:%M:%S}<br>Bewegung: %{customdata[0]}<br>Energie: %{customdata[1]}<br>abwesend: %{customdata[2]}<extra>Heartbeat</extra>"), row=2, col=1)
        else:
            fig.add_annotation(text="keine Heartbeats", showarrow=False, xref="x2 domain", yref="y2 domain", x=0.5, y=0.5, font=dict(color=c["muted"]))
        fig.update_yaxes(visible=False, row=2, col=1)
        self.testline(fig)
        return self.layout(fig, height=480)

    def heat(self):
        d, c = self.d, self.c
        top = d.top[: c["top_aps"]]
        if not top:
            return self.empty("Keine WLAN-Daten")
        times = [t.strftime("%d.%m. %H:%M") for t in d.ts]
        tick = list(range(0, len(times), max(1, len(times) // 10)))
        n = len(c["seq"]) - 1
        fig = go.Figure(go.Heatmap(
            z=d.matrix(top, np.nan), x=list(range(len(times))), y=[d.ap_name(b) for b in top], customdata=np.tile(times, (len(top), 1)),
            colorscale=[[i / n, col] for i, col in enumerate(c["seq"])], zmin=-95, zmax=-35, ygap=2, hoverongaps=False,
            colorbar=dict(title="dBm", tickfont=dict(color=c["ink2"])), hovertemplate="%{y}<br>%{customdata}<br>%{z} dBm<extra></extra>"))
        fig.update_yaxes(autorange="reversed")
        fig.update_xaxes(tickmode="array", tickvals=tick, ticktext=[times[i] for i in tick], title_text="Messung (zeitlich sortiert, Luecken ausgeblendet)")
        return self.layout(fig, height=max(360, 22 * len(top) + 120))

    def fingerprint(self):
        d, c = self.d, self.c
        if not d.rooms:
            return self.empty("Keine gelabelten Messungen")
        by_room = {r: [d.aps[s] for s, l in zip(d.scan_ids, d.labels) if l == r] for r in d.rooms}
        seen = {b: sum(1 for a in sum(by_room.values(), []) if b in a) for b in d.counts}
        aps = [b for b in sorted(seen, key=lambda b: -seen[b]) if seen[b]][: c["fingerprint_aps"]]
        fig = go.Figure()
        for r in d.rooms:
            vals = [np.mean([a[b] for a in by_room[r] if b in a]) if any(b in a for a in by_room[r]) else None for b in aps]
            share = [100 * sum(1 for a in by_room[r] if b in a) / len(by_room[r]) for b in aps]
            fig.add_trace(go.Bar(
                x=[d.ap_name(b) for b in aps], y=vals, name=f"{r} (n={len(by_room[r])})", marker=dict(color=d.color(r, c), cornerradius=3, line=dict(color=c["surface"], width=1)),
                customdata=share, hovertemplate="%{x}<br>" + r + ": %{y:.1f} dBm<br>gesehen in %{customdata:.0f}% der Messungen<extra></extra>"))
        fig.update_layout(barmode="group", bargap=0.25)
        fig.update_yaxes(title_text="mittlere Signalstaerke (dBm)", range=[-100, -20])
        return self.layout(fig)

    def pcamap(self):
        d, c = self.d, self.c
        if len(d.ts) < 3 or len(d.top) < 2:
            return self.empty("Zu wenige Messungen fuer die Karte")
        x = d.matrix(d.top, c["floor"]).T
        p = PCA(2).fit(x)
        xy = p.transform(x)
        fig = go.Figure()
        for label, name in [(None, "ohne Label")] + [(r, r) for r in d.rooms]:
            idx = [i for i, l in enumerate(d.labels) if l == label]
            if idx:
                fig.add_trace(go.Scatter(
                    x=xy[idx, 0], y=xy[idx, 1], mode="markers", name=name, customdata=[d.ts[i].strftime("%d.%m. %H:%M:%S") for i in idx],
                    marker=dict(color=d.color(label, c), size=9 if label else 6, opacity=0.9 if label else 0.55, line=dict(color=c["surface"], width=1)),
                    hovertemplate="%{customdata}<br>" + name + "<extra></extra>"))
        ev = p.explained_variance_ratio_
        fig.update_xaxes(title_text=f"Hauptkomponente 1 ({100 * ev[0]:.0f}%)")
        fig.update_yaxes(title_text=f"Hauptkomponente 2 ({100 * ev[1]:.0f}%)")
        return self.layout(fig, height=520)

    def thermostat(self):
        d, c = self.d, self.c
        if not d.readings and not d.setpoints:
            return self.empty("Keine Thermostat-Daten"), ""
        room = {n: r for _, n, r, _ in d.setpoints}
        nodes = sorted({r[1] for r in d.readings} | set(room))
        fig = go.Figure()
        for i, n in enumerate(nodes):
            col, name = c["cats"][i % len(c["cats"])], room.get(n) or f"Node {n}"
            rd = [(t, v) for t, nn, v in d.readings if nn == n]
            sp = [(t, v) for t, nn, _, v in d.setpoints if nn == n]
            if rd:
                fig.add_trace(go.Scatter(x=[t for t, _ in rd], y=[v for _, v in rd], mode="lines", name=f"{name} Ist", line=dict(color=col, width=2),
                                         hovertemplate="%{x|%d.%m. %H:%M}<br>" + name + " Ist %{y:.1f} °C<extra></extra>"))
            if sp:
                fig.add_trace(go.Scatter(x=[t for t, _ in sp], y=[v for _, v in sp], mode="lines", name=f"{name} Soll", line=dict(color=col, width=2, dash="dot", shape="hv"),
                                         hovertemplate="%{x|%d.%m. %H:%M}<br>" + name + " Soll %{y:.1f} °C<extra></extra>"))
        fig.update_yaxes(title_text="Temperatur (°C)")
        uniq = [len({v for _, nn, v in d.readings if nn == n}) for n in nodes]
        mock = not d.readings or max(uniq, default=0) <= c["constant_max_unique"]
        note = '<p class="note">Testdaten bis Commissioning: Werte stammen vom Mock-Controller und sind (nahezu) konstant.</p>' if mock else ""
        return self.layout(fig), note

    def render(self, out):
        c = self.c
        s = c["sections"]
        th, note = self.thermostat()
        parts = [
            ("timeline", self.timeline(), "Jeder Punkt ist eine Messung, Farbe = Raum-Label (grau = ohne Label). Das Band darunter zeigt die Heartbeats der Uhr (orange = abwesend); Luecken = keine Meldung.", ""),
            ("heat", self.heat(), "Die haeufigsten WLANs (Zeilen) ueber alle Messungen (Spalten). Helle Felder = starkes Signal, leere Felder = nicht gesehen.", ""),
            ("fingerprint", self.fingerprint(), "Mittlere Signalstaerke je WLAN und Raum. Je unterschiedlicher die Balken, desto besser sind die Raeume trennbar.", ""),
            ("map", self.pcamap(), "Jede Messung als Punkt (2D-PCA ueber alle WLANs, fehlend = -100 dBm). Getrennte Farbwolken = Raeume unterscheidbar.", ""),
            ("thermostat", th, "Gemessene Temperatur (Linie) und Solltemperatur (gepunktet) je Thermostat.", note),
        ]
        body = "".join(f'<section><h2>{s[k]}</h2><p class="d">{desc}</p>{extra}<div class="card">{self.html(f)}</div></section>' for k, f, desc, extra in parts)
        css = (f"body{{margin:0;background:{c['bg']};color:{c['ink']};font-family:system-ui,Segoe UI,sans-serif}}main{{max-width:1200px;margin:0 auto;padding:24px 16px}}"
               f"h1{{font-size:26px;margin:0 0 4px}}h2{{font-size:18px;margin:32px 0 4px}}.d{{color:{c['ink2']};margin:0 0 10px;font-size:14px}}"
               f".note{{background:{c['surface']};border-left:3px solid {c['away']};padding:8px 12px;margin:0 0 10px;font-size:14px}}"
               f".tiles{{display:grid;grid-template-columns:repeat(auto-fit,minmax(190px,1fr));gap:12px;margin-top:16px}}"
               f".tile,.card{{background:{c['surface']};border-radius:10px;padding:14px 16px}}.card{{padding:8px;overflow:hidden}}"
               f".k{{color:{c['muted']};font-size:12px;text-transform:uppercase;letter-spacing:.04em}}.v{{font-size:30px;font-weight:600;margin-top:4px}}.v.sm{{font-size:16px;line-height:1.5}}.s{{color:{c['muted']};font-size:12px}}")
        page = (f'<!doctype html><html lang="de"><head><meta charset="utf-8"><meta name="viewport" content="width=device-width,initial-scale=1"><title>{c["title"]}</title><style>{css}</style></head>'
                f'<body><main><h1>{c["title"]}</h1><div class="tiles">{self.kpis()}</div>{body}</main></body></html>')
        Path(out).write_text(page, encoding="utf-8")
        return Path(out)


def build(snapshot, out, config=VIZ):
    return Dashboard(snapshot, config).render(out)
