from datetime import datetime
from pathlib import Path
import numpy as np
import plotly.graph_objects as go
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
