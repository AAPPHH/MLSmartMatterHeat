from collections import Counter
import numpy as np
from sklearn.cluster import AgglomerativeClustering
from heatml.data import Scan
from heatml.room import RoomClassifier, vectorize

UNKNOWN = "unbekannt"


class Profile:
    def __init__(self, x, m):
        self.x, self.m = x, m

    @classmethod
    def of(cls, vectors, f):
        v = np.asarray(vectors)
        m = v[:, f:].mean(axis=0)
        x = v[:, :f].sum(axis=0) / np.maximum(v[:, f:].sum(axis=0), 1e-9)
        return cls(x, m)


class PlaceClusterer:
    def __init__(self, cfg, room_cfg):
        self.cfg = cfg
        self.room_cfg = room_cfg
        self.bssids = []
        self.stays = []
        self.clusters = []
        self.names = {}

    def vocabulary(self, scans):
        counts = Counter(b for s in scans for b in s.aps)
        pairs = [(a, b) for a, b in zip(scans, scans[1:]) if b.ts - a.ts <= self.cfg["max_gap_s"]]
        flips = {b: np.mean([(b in x.aps) != (b in y.aps) for x, y in pairs]) if pairs else 0.0 for b in counts}
        keep = [b for b, n in counts.items() if n >= self.cfg["min_scan_fraction"] * len(scans) and n >= self.cfg["min_scans"]
                and flips[b] <= self.cfg["max_flip_rate"]]
        keep.sort(key=lambda b: (-counts[b], b))
        return sorted(keep[: self.cfg["max_features"]])

    def vectors(self, scans):
        r = self.room_cfg
        return np.stack([vectorize(s.aps, self.bssids, r["rssi_floor"], r["rssi_span"]) for s in scans])

    def distance(self, a, B):
        both = a.m * B.m
        one = a.m * (1 - B.m) + B.m * (1 - a.m)
        num = (both * np.abs(a.x - B.x) + one * self.cfg["miss_penalty"]).sum(axis=-1)
        return num / np.maximum(np.maximum(a.m, B.m).sum(axis=-1), 1e-9)

    def far(self, recent, v):
        return self.distance(recent, Profile(v[: len(self.bssids)], v[len(self.bssids):])) > self.cfg["jump_threshold"]

    def segment(self, scans, vectors):
        f, c = len(self.bssids), self.cfg
        stays, cur = [], [0]
        for i in range(1, len(scans)):
            recent = Profile.of(vectors[cur[-c["jump_window"]:]], f)
            confirmed = i + 1 == len(scans) or scans[i + 1].ts - scans[i].ts > c["max_gap_s"] or self.far(recent, vectors[i + 1])
            if scans[i].ts - scans[cur[-1]].ts > c["max_gap_s"] or (self.far(recent, vectors[i]) and confirmed):
                stays.append(cur)
                cur = []
            cur.append(i)
        stays.append(cur)
        return stays

    def cluster_stays(self, profiles):
        n = len(profiles)
        if n < 2:
            return np.zeros(n, dtype=int)
        X, M = np.stack([p.x for p in profiles]), np.stack([p.m for p in profiles])
        D = np.stack([self.distance(p, Profile(X, M)) for p in profiles])
        D = (D + D.T) / 2
        np.fill_diagonal(D, 0.0)
        return AgglomerativeClustering(n_clusters=None, metric="precomputed", linkage="average",
                                       distance_threshold=self.cfg["merge_threshold"]).fit_predict(D)

    def describe(self, scans, members, cid):
        c = self.cfg
        labels = Counter(scans[i].label for i in members if scans[i].label)
        name, purity, n_labels = UNKNOWN, 0.0, sum(labels.values())
        if labels:
            top, hits = labels.most_common(1)[0]
            purity = hits / n_labels
            if n_labels >= c["min_labels"] and purity >= c["min_purity"] and len(members) >= c["min_cluster_scans"]:
                name = top
        hours = Counter(int(scans[i].ts % 86400 // 3600) for i in members)
        return {"id": cid, "scans": len(members), "stays": 0, "labelled": n_labels, "name": name,
                "purity": round(purity, 3), "hours": dict(sorted(hours.items()))}

    def fit(self, scans):
        scans = sorted(scans, key=lambda s: s.ts)
        self.bssids = self.vocabulary(scans)
        f = len(self.bssids)
        vectors = self.vectors(scans)
        self.stays = self.segment(scans, vectors)
        assign = self.cluster_stays([Profile.of(vectors[s], f) for s in self.stays])
        self.clusters, self.names = [], {}
        for cid in sorted(set(assign)):
            stays = [s for s, a in zip(self.stays, assign) if a == cid]
            members = [i for s in stays for i in s]
            info = self.describe(scans, members, int(cid))
            info["stays"] = len(stays)
            self.clusters.append(info)
            for i in members:
                self.names[scans[i].scan_id] = info["name"]
        self.clusters.sort(key=lambda c: -c["scans"])
        return self

    def named(self, scans):
        return [Scan(s.scan_id, s.ts, self.names[s.scan_id], s.aps) for s in scans if self.names.get(s.scan_id, UNKNOWN) != UNKNOWN]

    def report(self):
        lines = [f"{len(self.clusters)} clusters, {len(self.stays)} stays, {len(self.bssids)} bssids"]
        for c in self.clusters:
            line = f"cluster {c['id']}: {c['name']} scans={c['scans']} stays={c['stays']} labelled={c['labelled']} purity={c['purity']}"
            if c["name"] == UNKNOWN:
                line += " hours=" + " ".join(f"{h}h:{n}" for h, n in c["hours"].items())
            lines.append(line)
        return "\n".join(lines)


class SemiSupervisedRoom:
    def __init__(self, cfg):
        self.cfg = cfg
        self.clusterer = PlaceClusterer(cfg["cluster"], cfg["room"])
        self.classifier = RoomClassifier(cfg["room"])

    def fit(self, scans):
        self.clusterer.fit(scans)
        named = self.clusterer.named(scans)
        if not named:
            raise SystemExit("no cluster could be named; label more scans")
        self.classifier.fit(named)
        return self
