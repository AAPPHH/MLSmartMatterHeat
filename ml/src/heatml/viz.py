import json
from datetime import datetime, timedelta, timezone
from pathlib import Path
import duckdb
import numpy as np
import plotly.graph_objects as go
from plotly.subplots import make_subplots
from sklearn.cluster import HDBSCAN
from heatml.data import Snapshot

VIZ = {
    "floor": -100.0, "span": 70.0, "min_ap_scans": 3,
    "umap": {"n_components": 3, "n_neighbors": 15, "min_dist": 0.1, "metric": "euclidean", "random_state": 0},
    "hdbscan": {"min_cluster_size": 5, "min_samples": 3},
    "symbols": ["circle", "diamond", "square", "cross", "x", "circle-open", "diamond-open", "square-open"],
    "noise": "#5A5A5A",
    "palette": ["#E8710A", "#6CB4FF", "#F5C542", "#B48CFF", "#4DD0B5", "#F28BB4", "#9BD45A", "#FF6B6B"],
}


class Embedding:
    def __init__(self, scans, config=VIZ):
        self.config = config
        self.scans = scans
        counts = {}
        for s in scans:
            for b in s.aps:
                counts[b] = counts.get(b, 0) + 1
        self.bssids = sorted(b for b, n in counts.items() if n >= config["min_ap_scans"])
        self.x = np.array([[(s.aps.get(b, config["floor"]) - config["floor"]) / config["span"] for b in self.bssids] for s in scans]).reshape(len(scans), len(self.bssids))

    def fit(self):
        import umap
        n = len(self.scans)
        if n < 5 or not self.bssids:
            self.xyz, self.clusters = np.zeros((n, 3)), np.full(n, -1)
            return self
        params = {**self.config["umap"], "n_neighbors": min(self.config["umap"]["n_neighbors"], n - 1)}
        self.xyz = umap.UMAP(**params).fit_transform(self.x)
        hp = self.config["hdbscan"]
        self.clusters = HDBSCAN(min_cluster_size=min(hp["min_cluster_size"], n), min_samples=min(hp["min_samples"], n)).fit_predict(self.xyz)
        return self

    def figure(self):
        cfg, fig = self.config, go.Figure()
        labels = sorted({s.label or "ohne Label" for s in self.scans})
        symbol = {l: cfg["symbols"][i % len(cfg["symbols"])] for i, l in enumerate(labels)}
        for c in sorted(set(self.clusters.tolist())):
            for l in labels:
                idx = [i for i, s in enumerate(self.scans) if self.clusters[i] == c and (s.label or "ohne Label") == l]
                if not idx:
                    continue
                name = f"{'Rauschen' if c < 0 else f'Cluster {c}'} · {l}"
                color = cfg["noise"] if c < 0 else cfg["palette"][c % len(cfg["palette"])]
                text = [f"{datetime.fromtimestamp(self.scans[i].ts):%d.%m. %H:%M}<br>Label: {l}<br>{name.split(' · ')[0]}<br>{len(self.scans[i].aps)} WLANs" for i in idx]
                fig.add_trace(go.Scatter3d(x=self.xyz[idx, 0], y=self.xyz[idx, 1], z=self.xyz[idx, 2], mode="markers", name=name, text=text, hoverinfo="text",
                                           marker={"size": 5, "color": color, "symbol": symbol[l], "opacity": 0.9, "line": {"width": 0}}))
        n_clusters = len({c for c in self.clusters.tolist() if c >= 0})
        fig.update_layout(title=f"WLAN-Messungen · UMAP 3D · {len(self.scans)} Scans · {len(self.bssids)} WLANs · {n_clusters} Cluster (Farbe: HDBSCAN, Form: Label)",
                          template="plotly_dark", scene={"xaxis_title": "UMAP 1", "yaxis_title": "UMAP 2", "zaxis_title": "UMAP 3"}, legend={"itemsizing": "constant"}, margin={"l": 0, "r": 0, "t": 50, "b": 0})
        return fig


def build(snapshot, out, config=VIZ):
    out = Path(out)
    out.parent.mkdir(parents=True, exist_ok=True)
    Embedding(Snapshot(snapshot).scans(labelled_only=False), config).fit().figure().write_html(out, include_plotlyjs=True)
    return out


USAGE = {
    "valve_cluster": 513, "valve_attribute": 8, "max_age_min": 6, "low_pct": 10.0, "high_pct": 50.0,
    "palette": ["#E8710A", "#6CB4FF", "#F5C542", "#B48CFF", "#4DD0B5", "#F28BB4", "#9BD45A", "#FF6B6B"],
    "heat_scale": "YlOrRd",
}


class HeatingUsage:
    def __init__(self, lake_dir, config=USAGE):
        self.lake, self.config = Path(lake_dir), config
        self.rooms = self.room_nodes()
        self.minutes = self.valve_minutes()

    def parquet(self, table):
        files = sorted(self.lake.glob(f"parquet/{table}/date=*/**/part.parquet"))
        return "[" + ", ".join("'" + f.as_posix().replace("'", "''") + "'" for f in files) + "]" if files else None

    def query(self, sql):
        con = duckdb.connect()
        try:
            return con.execute(sql).fetchall()
        finally:
            con.close()

    def room_nodes(self):
        files = self.parquet("meta")
        if not files:
            return {}
        rows = self.query(f"SELECT arg_max(value, ts) FROM read_parquet({files}, union_by_name = true) WHERE key = 'rooms'")
        rooms = json.loads(rows[0][0]) if rows and rows[0][0] else []
        return {r["name"]: [int(n) for n in r.get("nodes", [])] for r in rooms if r.get("nodes")}

    def valve_minutes(self):
        files = self.parquet("attribute_reports")
        if not files or not self.rooms:
            return {}
        c = self.config
        rows = self.query(
            f"SELECT node, epoch_us(ts) // 1000000, coalesce(v_f64, v_i64::DOUBLE) FROM read_parquet({files}, union_by_name = true, hive_partitioning = true) "
            f"WHERE cluster = {c['valve_cluster']} AND attribute = {c['valve_attribute']} AND (v_f64 IS NOT NULL OR v_i64 IS NOT NULL) ORDER BY node, ts")
        if not rows:
            return {}
        first = min(r[1] for r in rows) // 60
        last = max(r[1] for r in rows) // 60 + c["max_age_min"] + 1
        grid = (np.arange(first, last) + 1) * 60 - 1
        per_node = {}
        for node, ts, value in rows:
            per_node.setdefault(node, ([], []))
            per_node[node][0].append(ts)
            per_node[node][1].append(value)
        series = {}
        for node, (ts, value) in per_node.items():
            ts, value = np.array(ts), np.array(value, dtype=float)
            idx = np.searchsorted(ts, grid, side="right") - 1
            ok = (idx >= 0) & (grid - ts[np.maximum(idx, 0)] <= c["max_age_min"] * 60) & (value[np.maximum(idx, 0)] >= 0) & (value[np.maximum(idx, 0)] <= 100)
            series[node] = np.where(ok, value[np.maximum(idx, 0)], np.nan)
        out = {}
        for room, nodes in self.rooms.items():
            stack = np.array([series[n] for n in nodes if n in series])
            if stack.size and not np.all(np.isnan(stack)):
                count = (~np.isnan(stack)).sum(axis=0)
                out[room] = (first, np.where(count > 0, np.nansum(stack, axis=0) / np.maximum(count, 1), np.nan))
        return out

    def days(self):
        if not self.minutes:
            return []
        lo = min(first for first, _ in self.minutes.values())
        hi = max(first + len(v) for first, v in self.minutes.values())
        day = lambda m: datetime.fromtimestamp(m * 60, timezone.utc).date()
        return [day(lo) + timedelta(days=i) for i in range((day(hi - 1) - day(lo)).days + 1)]

    def tables(self):
        days = self.days()
        day_index = {d: i for i, d in enumerate(days)}
        hours = {r: np.zeros((len(days), 24)) for r in self.minutes}
        mins = {r: np.zeros((len(days), 2)) for r in self.minutes}
        c = self.config
        for room, (first, v) in self.minutes.items():
            for i in np.flatnonzero(~np.isnan(v)):
                t = datetime.fromtimestamp((first + int(i)) * 60, timezone.utc)
                d = day_index.get(t.date())
                if d is None:
                    continue
                hours[room][d, t.hour] += v[i] / 100 / 60
                mins[room][d, 0] += v[i] > c["low_pct"]
                mins[room][d, 1] += v[i] > c["high_pct"]
        return days, hours, mins

    def figure(self):
        cfg = self.config
        days, hours, mins = self.tables()
        rooms = sorted(hours)
        labels = [d.strftime("%d.%m.") for d in days]
        total = sum((h.sum() for h in hours.values()), 0.0)
        fig = make_subplots(rows=len(rooms) + 2, cols=1, specs=[[{"type": "xy"}]] * (len(rooms) + 1) + [[{"type": "table"}]],
                            subplot_titles=["Ventilstunden je Tag und Heizkoerper"] + [f"{r}: Ventilstunden je Tag x Stunde (UTC)" for r in rooms] + ["Summen"],
                            row_heights=[2] + [1] * len(rooms) + [1 + len(rooms) / 2], vertical_spacing=0.04)
        for i, room in enumerate(rooms):
            fig.add_trace(go.Bar(x=labels, y=hours[room].sum(axis=1), name=room, marker={"color": cfg["palette"][i % len(cfg["palette"])]}), row=1, col=1)
            fig.add_trace(go.Heatmap(z=hours[room], x=list(range(24)), y=labels, colorscale=cfg["heat_scale"], zmin=0, zmax=1, colorbar={"title": "h", "len": 0.1} if i == 0 else None, showscale=i == 0,
                                     hovertemplate="%{y} %{x}:00<br>%{z:.2f} Ventilstunden<extra>" + room + "</extra>"), row=i + 2, col=1)
            fig.update_yaxes(type="category", autorange="reversed", row=i + 2, col=1)
        fig.update_layout(barmode="stack")
        rows = [[r, f"{hours[r].sum():.2f}", f"{mins[r][:, 0].sum():.0f}", f"{mins[r][:, 1].sum():.0f}", f"{100 * hours[r].sum() / total:.0f} %" if total else "-"] for r in rooms]
        rows.append(["Haus gesamt", f"{total:.2f}", f"{sum(m[:, 0].sum() for m in mins.values()):.0f}", f"{sum(m[:, 1].sum() for m in mins.values()):.0f}", "100 %" if total else "-"])
        fig.add_trace(go.Table(header={"values": ["Heizkoerper", "Ventilstunden", f"Min > {cfg['low_pct']:.0f} %", f"Min > {cfg['high_pct']:.0f} %", "Anteil Haus"]}, cells={"values": list(map(list, zip(*rows)))}), row=len(rooms) + 2, col=1)
        fig.update_layout(title=f"Heizbetrieb · {len(days)} Tage · {len(rooms)} Heizkoerper · Ventilstunden = Summe PIHeatingDemand-Anteil x Zeit", template="plotly_dark", height=300 * (len(rooms) + 2), legend={"orientation": "h"})
        return fig


def build_usage(lake_dir, out, config=USAGE):
    out = Path(out)
    out.parent.mkdir(parents=True, exist_ok=True)
    HeatingUsage(lake_dir, config).figure().write_html(out, include_plotlyjs=True)
    return out
