import copy
import json
import math
import numpy as np
from heattrain.__main__ import CONFIG
from heattrain.roombatch import Grid
from heattrain.roomeval import Skill, interval_logscore, merge_windows, rmse, window_metrics
from heattrain.roomfit import Truth, calibrate_air, episodes, label_loss, load_live_alarms

RC = CONFIG["roommodel"]


class G:
    def __init__(self, names, n, ya=None):
        self.names, self.n, self.dt = names, n, 60.0
        self.times = 1_790_000_000.0 + 60.0 * np.arange(n)
        self.cols = {r: {"ya": ya or []} for r in names}

    def span_mask(self, spans):
        m = np.zeros(self.n, bool)
        for a, b in spans:
            m[np.searchsorted(self.times, a):np.searchsorted(self.times, b, side="right")] = True
        return m


def truth(gt, labels=(), live=()):
    t = object.__new__(Truth)
    t.rc, t.gt, t.labels, t.live = RC, list(gt), list(labels), list(live)
    return t


def rec(n, pw):
    return {"r": {"p_window": np.asarray(pw, float), "lam": np.ones(n)}}


def test_episodes_need_a_minimum_length():
    t = np.arange(20.0)
    flag = np.zeros(20, bool)
    flag[2:4] = flag[10:16] = True
    assert episodes(flag, t) == [(2.0, 3.0), (10.0, 15.0)]
    assert episodes(flag, t, 5) == [(10.0, 15.0)]
    flag[:] = True
    assert episodes(flag, t, 5) == [(0.0, 19.0)]


def test_window_metrics_counts_hits_false_episodes_and_live_alarms():
    g = G(["r"], 600)
    t0 = g.times[0]
    gt = [(t0 + 300 * 60.0, t0 + 330 * 60.0, ["r"])]
    pw = np.zeros(600)
    pw[305:315] = 0.9
    pw[100:104] = 0.9
    pw[450:460] = 0.9
    live = [(t0 + 310 * 60.0, "r"), (t0 + 100 * 60.0, "r"), (t0 + 200 * 60.0, "r")]
    m = window_metrics(g, rec(600, pw), truth(gt, live=live), 0, 600, RC)
    assert m["opens"] == 1 and m["hits"] == 1 and m["latency_min"] == [5.0]
    assert m["false_episodes"] == 1
    assert m["live_hits"] == 1 and m["live_false"] == 2
    s = merge_windows([m, m])
    assert s["hits"] == 2 and abs(s["false_per_room_day"] - 2 / (1200 / 1440.0)) < 1e-9


def test_label_loss_rewards_peaks_inside_events_and_charges_false_episodes():
    g = G(["r"], 600)
    t0 = g.times[0]
    gt = [(t0 + 300 * 60.0, t0 + 330 * 60.0, ["r"])]
    quiet = np.zeros(600)
    hit = quiet.copy()
    hit[305:312] = 0.9
    false = hit.copy()
    false[50:60] = 0.9
    t = truth(gt)
    a, b, c = (label_loss(g, rec(600, x), t, RC) for x in (quiet, hit, false))
    assert b < a and abs((c - b) - RC["shock"]["label"]["fa_cost"]) < 1e-9


def test_interval_logscore_prefers_the_right_centre_and_is_bounded():
    y = np.array([20.0, 20.5])
    good, bad = interval_logscore(y, y, 0.2, 0.5), interval_logscore(y, y + 3.0, 0.2, 0.5)
    assert (good > bad).all() and (bad >= math.log(1e-12) - 1e-9).all()


def test_skill_pools_errors_and_reports_floor_distance():
    s = Skill([20], 0.2)
    rng = np.random.default_rng(0)
    for room in ("a", "b"):
        e = rng.normal(0, 0.5, 200)
        s.add(room, 20, {"persistence": 2 * e, "imm_anchored": e, "imm_raw": e, "kf_anchored": e}, np.array(["steady"] * 200), {"persistence": np.zeros(200), "imm_anchored": np.ones(200)})
    rep = s.report(["a", "b"])
    p = rep["pooled"]["H20"]
    assert p["n"] == 400 and abs(p["skill"] - 0.5) < 1e-9 and p["dist_floor"]["imm_anchored"] < p["imm_anchored"]
    assert rep["pooled"]["strata_H20"]["steady"]["n"] == 400 and rep["rooms"]["a"]["H20"]["n"] == 200
    assert rmse([]) is None


def test_air_calibration_returns_offset_and_robust_noise_and_skips_short_series():
    n = 400
    g = G(["r", "q"], n)
    a = 20.0 + 0.1 * np.sin(np.arange(n) / 20.0)
    rng = np.random.default_rng(1)
    g.cols["r"]["ya"] = [a - 1.2 + rng.normal(0, 0.1, n)]
    ys = np.full(n, np.nan)
    ys[:10] = 19.0
    g.cols["q"]["ya"] = [ys]
    out = calibrate_air(g, {}, RC, 0, n, {"r": {"a": a}, "q": {"a": a}})
    assert set(out) == {"r"} and abs(out["r"][0]["offset"] + 1.2) < 0.05 and out["r"][0]["noise"] >= RC["evidence"]["air"]["noise"]


def test_live_alarms_read_auto_window_open_events_only(tmp_path):
    import pyarrow as pa
    import pyarrow.ipc as ipc
    (tmp_path / "raw" / "events").mkdir(parents=True)
    rows = {"ts": [1.0, 2.0, 3.0], "kind": ["window_open", "window_open", "window_close"], "detail": [json.dumps({"room": "bad", "source": "auto"}), json.dumps({"room": "bad", "source": "manual"}), json.dumps({"room": "bad", "source": "auto"})]}
    tbl = pa.table({"ts": pa.array([1_790_000_000_000_000 + int(x) for x in rows["ts"]], pa.timestamp("us", tz="UTC")), "kind": rows["kind"], "detail": rows["detail"]})
    with open(tmp_path / "raw" / "events" / "a.arrows", "wb") as f:
        with ipc.new_stream(f, tbl.schema) as w:
            w.write_table(tbl)
    out = load_live_alarms(tmp_path)
    assert len(out) == 1 and out[0][1] == "bad"
