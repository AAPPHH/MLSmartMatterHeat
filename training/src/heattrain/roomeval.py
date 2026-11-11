import copy
import json
import math
import time
from datetime import datetime, timezone
from pathlib import Path
import numpy as np
from heattrain.roombatch import Grid, norm_cdf_np, run_batch
from heattrain.roomfit import FoldFit, Truth, base_doc, calibrate_air, episodes, fit_shock, shock_defaults, tune_windows, unusable_rooms
from heattrain.roomobs import evidence_blocks
from heattrain.thermal import build_rooms, load_devices, load_events, load_lake

KINDS = ("persistence", "imm_anchored", "imm_oracle", "imm_raw", "kf_anchored")
STRATA = ("steady", "heating_start", "heating", "setpoint_change", "cooling", "airing")


def rmse(x):
    x = np.asarray(x, float)
    return float(np.sqrt(np.mean(x ** 2))) if len(x) else None


def build_data(config, src):
    cfg = config["thermal"]
    reports, weather, meta = load_lake(src, cfg["exclude_sources"])
    rooms, skipped = build_rooms(reports, weather, meta, cfg, load_events(src), load_devices(src))
    if not rooms:
        raise SystemExit(f"no usable rooms: {skipped}")
    return rooms, skipped


def run_range(grid, docs, shocks, house, start, stop, record=True):
    bh, recs = run_batch(grid.window(start, stop), docs, shocks, house, record=record)
    pad = lambda x: np.concatenate([np.full(start, np.nan), x, np.full(grid.n - stop, np.nan)])
    full = [{r: {k: pad(v) for k, v in d.items()} for r, d in rec.items()} for rec in recs] if record else None
    return bh, full, [[dict(e, step=e["step"] + start) for e in bh.resets(g)] for g in range(len(shocks))]


def strata(rf, js, spans_mask):
    c, n = rf.c, rf.n
    u, sp, y = np.nan_to_num(c["u"]), c["sp"], c["y"]
    lead = lambda x, a, b: x[np.clip(np.arange(a, b), 0, n - 1)]
    out = []
    for j in js:
        w = lead(u, j - 30, j + 1)
        spd = np.nanmax(lead(sp, j - 30, j + 1)) - np.nanmin(lead(sp, j - 30, j + 1)) if np.isfinite(lead(sp, j - 30, j + 1)).any() else 0.0
        ya, yb = lead(y, j - 5, j + 1), lead(y, j - 35, j - 29)
        dT = np.nanmean(ya) - np.nanmean(yb) if np.isfinite(ya).any() and np.isfinite(yb).any() else 0.0
        ahead = u[j:j + 20].mean() if j < n else 0.0
        if spans_mask[j]:
            out.append("airing")
        elif spd >= 0.4:
            out.append("setpoint_change")
        elif w[-1] - w.min() >= 0.1:
            out.append("heating_start")
        elif ahead > 0.02:
            out.append("heating")
        elif w.max() < 0.02 and dT <= -0.25:
            out.append("cooling")
        else:
            out.append("steady")
    return np.array(out)


class Skill:
    def __init__(self, Hs, floor):
        self.Hs, self.floor = Hs, floor
        self.e, self.lp, self.n_room = {}, {}, {}

    def add(self, room, H, errs, st, lp):
        for k, v in errs.items():
            self.e.setdefault((H, k), []).append(v)
            self.e.setdefault((room, H, k), []).append(v)
            if H in (20, 60):
                for s in STRATA:
                    self.e.setdefault((H, s, k), []).append(v[st == s])
        for k, v in lp.items():
            self.lp.setdefault((H, k), []).append(v)

    def block(self, key):
        m = {k: rmse(np.concatenate(self.e[key + (k,)])) if key + (k,) in self.e else None for k in KINDS}
        n = int(sum(len(x) for x in self.e.get(key + ("persistence",), [])))
        skill = lambda k: 1.0 - m[k] / m["persistence"] if n and m["persistence"] and m[k] is not None else None
        return {"n": n, **m, "skill": skill("imm_anchored"), "oracle_skill": skill("imm_oracle")}

    def report(self, rooms):
        out = {"pooled": {}, "rooms": {r: {} for r in rooms}}
        for H in self.Hs:
            b = self.block((H,))
            b["floor_rmse"] = self.floor
            b["dist_floor"] = {k: math.sqrt(max(b[k] ** 2 - self.floor ** 2, 0.0)) if b[k] else None for k in ("persistence", "imm_anchored")}
            if (H, "imm_anchored") in self.lp:
                b["interval_logscore"] = {k: float(np.mean(np.concatenate(self.lp[(H, k)]))) for k in ("persistence", "imm_anchored")}
            out["pooled"][f"H{H}"] = b
            for r in rooms:
                out["rooms"][r][f"H{H}"] = self.block((r, H))
            if H in (20, 60):
                out["pooled"][f"strata_H{H}"] = {s: self.block((H, s)) for s in STRATA}
        return out


def interval_logscore(y, mu, sd, grid):
    sd = max(sd, 1e-3)
    p = norm_cdf_np((y + 0.5 * grid - mu) / sd) - norm_cdf_np((y - 0.5 * grid - mu) / sd)
    return np.log(np.maximum(p, 1e-12))


def fold_skill(fold, rfs, docs, rec, spans, skill, rc, grid_q):
    phi = math.exp(-1.0 / fold["shock"]["tau_l"]) if rc["eval"]["decay_lam"] else 1.0
    for r, rf in rfs.items():
        p = rf.params(np.asarray(fold["th"][r])[None])
        start = (rec[r]["m"], rec[r]["a"], rec[r]["lam"], rec[r]["p_supply"])
        test = (rf.js >= fold["cut"]) & (rf.js < fold["hi"])
        train = rf.js + max(rf.Hs) < fold["cut"]
        e_te, raw_te = rf.errors(p, test, docs[r], start, phi, True)
        e_or, _ = rf.errors(p, test, docs[r], start, phi)
        kf, _ = rf.errors(p, test, docs[r], causal=True)
        e_tr, _ = rf.errors(p, train, docs[r], start, phi, True)
        st = strata(rf, rf.js[test], spans[r])
        y, js_te, Pc = rf.c["y"], rf.js[test], rf.Pc[test]
        for H in rf.Hs:
            tr_ok = rf.valid[H][train] & rf.sp_ok[train] & np.isfinite(e_tr[H][:, 0])
            sd_i = rmse(e_tr[H][tr_ok, 0]) or 1.0
            sd_p = rmse((y[rf.js[train] + H] - rf.Pc[train])[tr_ok]) or 1.0
            ok = rf.valid[H][test] & rf.sp_ok[test] & np.isfinite(e_te[H][:, 0]) & np.isfinite(e_or[H][:, 0]) & np.isfinite(kf[H][:, 0])
            yh = y[js_te + H][ok]
            errs = {"persistence": (y[js_te + H] - Pc)[ok], "imm_anchored": e_te[H][ok, 0], "imm_oracle": e_or[H][ok, 0], "imm_raw": raw_te[H][ok, 0], "kf_anchored": kf[H][ok, 0]}
            lp = {"persistence": interval_logscore(yh, Pc[ok], sd_p, grid_q), "imm_anchored": interval_logscore(yh, yh - errs["imm_anchored"], sd_i, grid_q)}
            skill.add(r, H, errs, st[ok], lp)


def flag_series(pw, level):
    return np.nan_to_num(pw) > level


def live_episodes(truth, room, merge_s):
    out = []
    for t in sorted(t for t, r in truth.live if r == room):
        if out and t - out[-1][1] <= merge_s:
            out[-1] = (out[-1][0], t)
        else:
            out.append((t, t))
    return out


def touching(eps, a, b):
    return [e for e in eps if e[0] <= b and e[1] >= a]


def window_metrics(grid, rec, truth, lo, hi, rc):
    ev, hit_s = rc["eval"], rc["shock"]["hit_min"] * 60.0
    names, spans = grid.names, truth.spans(grid.names)
    t = grid.times
    res = {"opens": 0, "hits": 0, "latency_min": [], "missed": [], "false_episodes": 0, "valid_min": 0, "denials": 0, "denials_flagged": 0, "per_room": {}, "live_opens": 0, "live_hits": 0, "live_false": 0, "live_denials_flagged": 0}
    for r in names:
        tt = t[lo:hi]
        eps = episodes(flag_series(rec[r]["p_window"][lo:hi], ev["pw_flag"]), tt, ev["min_episode_min"])
        leps = live_episodes(truth, r, rc["hold_min"] * 60.0)
        valid = int(np.isfinite(rec[r]["lam"][lo:hi]).sum())
        outside = lambda e: not any(x <= e[0] <= y for x, y in spans[r])
        false = sum(1 for e in eps if outside(e))
        live_false = sum(1 for e in leps if t[lo] <= e[0] < t[hi - 1] and outside(e))
        for room, a in truth.opens([r], t[lo], t[hi - 1]):
            mine = touching(eps, a, a + hit_s)
            res["opens"] += 1
            res["live_opens"] += 1
            res["live_hits"] += int(bool(touching(leps, a, a + hit_s)))
            if mine:
                res["hits"] += 1
                res["latency_min"].append(max(mine[0][0] - a, 0.0) / 60.0)
            else:
                res["missed"].append([r, a])
        for room, a in truth.denials([r], t[lo], t[hi - 1]):
            res["denials"] += 1
            res["denials_flagged"] += int(bool(touching(eps, a - hit_s, a + hit_s)))
            res["live_denials_flagged"] += int(bool(touching(leps, a - hit_s, a + hit_s)))
        res["false_episodes"] += false
        res["valid_min"] += valid
        res["live_false"] += live_false
        res["per_room"][r] = {"false": false, "valid_min": valid, "live_false": live_false}
    return res


def merge_windows(parts):
    out = {k: 0 for k in ("opens", "hits", "false_episodes", "valid_min", "denials", "denials_flagged", "live_opens", "live_hits", "live_false", "live_denials_flagged")}
    out.update({"latency_min": [], "missed": [], "per_room": {}})
    for p in parts:
        for k in out:
            if k in ("latency_min", "missed"):
                out[k] += p[k]
            elif k == "per_room":
                for r, v in p[k].items():
                    o = out[k].setdefault(r, {"false": 0, "valid_min": 0, "live_false": 0})
                    for kk in o:
                        o[kk] += v[kk]
            else:
                out[k] += p[k]
    days = lambda m: max(m / 1440.0, 1e-9)
    out["false_per_room_day"] = out["false_episodes"] / days(out["valid_min"])
    out["live_false_per_room_day"] = out["live_false"] / days(out["valid_min"])
    for r, o in out["per_room"].items():
        o["false_per_day"], o["live_false_per_day"] = o["false"] / days(o["valid_min"]), o["live_false"] / days(o["valid_min"])
    return out


class Variants:
    def __init__(self, rooms, full, rc):
        self.rooms, self.full, self.rc = {d.room: d for d in rooms}, full, rc
        self.blocks = {d.room: evidence_blocks(d.ext, rc["evidence"]) for d in rooms}
        ss = [int(np.nonzero(np.isfinite(a))[0][0]) for c in full.cols.values() for a in c["ya"] if np.isfinite(a).any()]
        self.sensor_start = min(ss) if ss else None

    def docs(self, base, parts, cal=None, effort=None):
        out = copy.deepcopy(base)
        for r, d in out.items():
            b = self.blocks[r]
            if effort:
                d["effort"] = dict(effort)
            if "air" in parts and cal and r in cal:
                d["air"] = [{"node": a["node"], **c} for a, c in zip(b.get("air", []), cal[r])]
            if "gas" in parts:
                d.update({k: b[k] for k in ("co2", "hum") if k in b})
        return out


def make_folds(full, variants, rc):
    ev = rc["eval"]
    cuts = [int(full.n * f) for f in ev["folds"]]
    ss = variants.sensor_start
    if ss is not None and full.n - ss >= ev["min_sensor_min"]:
        sc = ss + int((full.n - ss) * ev["sensor_split"])
        cuts.append(sc)
    cuts = sorted(set(cuts))
    return [{"cut": c, "hi": h, "sensor": ss is not None and c >= ss} for c, h in zip(cuts, cuts[1:] + [full.n])]


def run_fold(fold, full, ff, truth, rc, warm):
    docs0, th, info = ff.run(fold["cut"], warm, ext=False)
    best, nll, trace = fit_shock(full.window(max(0, fold["cut"] - int(rc["shock"]["days"] * 1440)), fold["cut"]), docs0, rc, None, truth)
    fold.update(th=th, info=info, docs0=docs0, shock=shock_defaults(rc, **best), nll=nll)
    return th


def tuned_trials(fold, full, docs, truth, rc, house):
    grid_lw = sorted(set(rc["shock"]["lambda_w_grid"]) | {rc["shock"]["lambda_w"]})
    shocks = [dict(fold["shock"], lambda_w=lw) for lw in grid_lw]
    start = max(0, fold["cut"] - int(rc["eval"]["warm_days"] * 1440))
    bh, recs, resets = run_range(full, docs, shocks, house, start, fold["hi"])
    cut_grid = full.window(start, fold["cut"])
    tr = {r: {k: v[start:fold["cut"]] for k, v in d.items()} for r, d in recs[0].items()}
    inside = lambda t: cut_grid.times[0] <= t < cut_grid.times[-1]
    lw, lj, tuning = tune_windows(cut_grid, tr, rc, [g for g in truth.gt if inside(g[0])], [x for x in truth.labels if inside(x[0])])
    gi = int(np.argmin([abs(x - lw) for x in grid_lw]))
    return recs[0], recs[gi], lw, lj, resets[gi], tuning


def evaluate_variant(folds, full, ff, variants, truth, rc, house, parts, effort=None, shock=None):
    skill = Skill(list(ff.rfs[next(iter(ff.rfs))].Hs), rc["eval"]["floor"])
    wparts, resets, per_fold = [], 0, []
    for f in folds:
        f = dict(f, shock=shock_defaults(rc, **shock)) if shock else f
        cal = None
        if "air" in parts and variants.sensor_start is not None and f["cut"] > variants.sensor_start:
            ref_docs = f["docs0"]
            _, rref, _ = run_range(full, ref_docs, [f["shock"]], house, max(0, variants.sensor_start - 600), f["cut"])
            cal = calibrate_air(full, ref_docs, rc, variants.sensor_start, f["cut"], rref[0])
        docs = variants.docs(f["docs0"], parts, cal, effort)
        rec_a, rec_w, lw, lj, rs, tuning = tuned_trials(f, full, docs, truth, rc, house)
        spans = truth.spans(full.names)
        spans_mask = {r: full.span_mask(spans[r]) for r in full.names}
        fold_skill(f, ff.rfs, docs, rec_a, spans_mask, skill, rc, house["trv_grid"] or 0.5)
        wparts.append(window_metrics(full, rec_w, truth, f["cut"], f["hi"], rc))
        n_r = sum(1 for e in rs if f["cut"] <= e["step"] < f["hi"])
        resets += n_r
        per_fold.append({"cut": f["cut"], "hi": f["hi"], "lambda_w": lw, "resets": n_r, "shock": {k: f["shock"][k] for k in ("q_calm", "q_jump", "p_jump", "tau_l")}, "tuning_refused": bool(tuning and tuning.get("refused"))})
    rep = skill.report(list(full.names))
    return {"skill": rep, "window": merge_windows(wparts), "resets": resets, "folds": per_fold}


def score(rep):
    p = rep["skill"]["pooled"]
    return 0.5 * sum(p[h]["skill"] or 0.0 for h in ("H20", "H60"))


def rolling_origin(full, ff, variants, truth, rc, house):
    folds, warm = make_folds(full, variants, rc), None
    for f in folds:
        warm = run_fold(f, full, ff, truth, rc, warm)
    sensor = [f for f in folds if f["sensor"]]
    ev = lambda fs, parts=(), effort=None, shock=None: evaluate_variant(fs, full, ff, variants, truth, rc, house, parts, effort, shock)
    out = {"trv": ev(folds)}
    sel = rc["evidence"]["select"]
    if not sel["enabled"]:
        return folds, out, {"parts": ["air", "gas"], "effort": rc["evidence"]["effort"]}
    for i, sh in enumerate(rc["eval"]["shock_variants"]):
        out[f"shock_{i}"] = ev(folds, shock=sh)
        out[f"shock_{i}"]["shock_fixed"] = sh
    cands = [(f"effort_{e['sigma']}_{e['steady_u']}", folds, "trv", (), e) for e in sel["effort"]]
    if sensor:
        out["trv_sensor"] = ev(sensor)
        cands += [("air", sensor, "trv_sensor", ("air",), None), ("air_gas", sensor, "trv_sensor", ("air", "gas"), None)]
    for key, fs, ref, parts, effort in cands:
        out[key] = ev(fs, parts, effort)
        out[key]["gain"] = score(out[key]) - score(out[ref])
        out[key]["accepted"] = bool(out[key]["gain"] >= sel["min_gain"] and out[key]["resets"] <= out[ref]["resets"] + sel["max_resets_up"])
    best = lambda pre: max([k for k, v in out.items() if v.get("accepted") and (k.startswith("effort_") == pre)], key=lambda k: out[k]["gain"], default=None)
    be, bs = best(True), best(False)
    chosen = {"parts": ["air"] + (["gas"] if bs == "air_gas" else []) if bs else [], "effort": next(e for e in sel["effort"] if f"effort_{e['sigma']}_{e['steady_u']}" == be) if be else None}
    return folds, out, chosen


def criteria(rc, hold, final_resets, flags, edges):
    c, p = rc["criteria"], hold["trv"]["skill"]["pooled"]
    w = hold["trv"]["window"]
    return {"S1": {"pass": bool(p["H20"]["skill"] >= c["s1_h20"] and p["H60"]["skill"] >= c["s1_h60"]), "basis": "causal", "h20": p["H20"]["skill"], "h60": p["H60"]["skill"], "oracle_valve": {"h20": p["H20"]["oracle_skill"], "h60": p["H60"]["oracle_skill"]}},
            "S2": {"pass": bool(w["false_per_room_day"] < c["s2"] and w["false_per_room_day"] < w["live_false_per_room_day"]), "stretch": bool(w["false_per_room_day"] < c["s2_stretch"]), "false_per_room_day": w["false_per_room_day"], "live_false_per_room_day": w["live_false_per_room_day"]},
            "S3": {"pass": bool(w["hits"] > w["live_hits"]), "hits": w["hits"], "live_hits": w["live_hits"], "opens": w["opens"]},
            "S4": {"pass": bool(not flags and not edges and max(final_resets.values()) < c["s4_resets"]), "resets_per_room": final_resets, "at_bound": flags, "shock_at_grid_edge": edges}}


def fit_bundle(config, rooms, src, version=None):
    t0 = time.time()
    cfg, rc = config["thermal"], config["roommodel"]
    house = rc["house"]
    full = Grid(rooms, {d.room: {"c": dict.fromkeys(d.nb, 0.0)} for d in rooms})
    truth = Truth(src, rc)
    ff, variants = FoldFit(rooms, full, cfg, rc), Variants(rooms, full, rc)
    folds, hold, chosen = rolling_origin(full, ff, variants, truth, rc, house)
    t_fold = time.time() - t0
    docs0, th, info = ff.run(full.n, folds[-1]["th"], ext=False)
    lo = max(0, full.n - int(rc["shock"]["days"] * 1440))
    best, nll, trace = fit_shock(full.window(lo, full.n), docs0, rc, None, truth)
    shock = shock_defaults(rc, **best)
    cal = None
    if "air" in chosen["parts"] and variants.sensor_start is not None:
        _, rref, _ = run_range(full, docs0, [shock], house, max(0, variants.sensor_start - 600), full.n)
        cal = calibrate_air(full, docs0, rc, variants.sensor_start, full.n, rref[0])
    docs = variants.docs(docs0, chosen["parts"], cal, chosen["effort"])
    bad = unusable_rooms(full, docs, rc)
    for r in bad:
        docs[r]["unusable"] = True
    run, recs = run_batch(full, docs, [shock], house, record=True)
    lw, lj, tuning = tune_windows(full, recs[0], rc, truth.gt, truth.labels)
    shock = dict(shock, lambda_w=lw, lambda_jump=lj)
    run, recs = run_batch(full, docs, [shock], house, record=True)
    rec, resets = recs[0], run.resets(0)
    by_room = {r: sum(1 for x in resets if x["room"] == r) for r in full.names}
    flags = [f"{r}: at bound {','.join(i['at_bound'])}" for r, i in info.items() if i["at_bound"]]
    unconverged = [f"{r}: fit not converged" for r, i in info.items() if not i["converged"]]
    flags_all = flags + unconverged + [f"{r}: fit unusable, {'; '.join(w)}" for r, w in bad.items()] + ([tuning["warning"]] if tuning and tuning.get("refused") else [])
    version = version or datetime.now(timezone.utc).strftime("%Y%m%dT%H%M%SZ")
    house_out = dict(house, night_hours=cfg["night_hours"], timezone=cfg["timezone"])
    bundle = {"kind": "room_model", "version": version, "dt_s": full.dt, "rooms": docs, "shock": {k: shock[k] for k in ("tau_l", "q_calm", "q_jump", "p_jump", "hold_min", "lambda_jump", "lambda_w", "p_l0")}, "house": house_out}
    pooled = {k: v for k, v in hold["trv"]["skill"]["pooled"].items()}
    holdout = {"pooled": pooled, "rooms": hold["trv"]["skill"]["rooms"], "folds": hold["trv"]["folds"], "window": hold["trv"]["window"], "variants": {k: {"gain": v.get("gain"), "accepted": v.get("accepted"), "resets": v["resets"], "pooled": {h: {"skill": v["skill"]["pooled"][h]["skill"], "oracle_skill": v["skill"]["pooled"][h]["oracle_skill"], "n": v["skill"]["pooled"][h]["n"]} for h in ("H5", "H20", "H60")}, "window": {k2: v["window"][k2] for k2 in ("hits", "opens", "false_per_room_day", "live_false_per_room_day")}, "shock_fixed": v.get("shock_fixed"), "resets_per_fold": [f["resets"] for f in v["folds"]]} for k, v in hold.items()}, "chosen": chosen}
    by = lambda key: {r: sum(1 for x in resets if x["room"] == r and x["cause"] == key) for r in full.names}
    detection = merge_windows([window_metrics(full, rec, truth, 0, full.n, rc)])
    report = {"version": version, "flags": flags_all, "holdout": holdout, "criteria": criteria(rc, hold, by_room, flags, [k for k, v in rc["shock"]["grid"].items() if best[k] in (min(v), max(v))]), "fit": info, "nll": nll, "trace": [{"shock": t, "nll": s} for t, s in trace], "tuning": tuning, "detection": detection,
              "resets": len(resets), "reset_list": reset_list(full, resets, truth, rec), "reset_by_room": by_room, "reset_causes": {c: by(c) for c in sorted({x["cause"] for x in resets})}, "ground_truth_events": len(truth.gt), "labels": len(truth.labels), "live_alarms": len(truth.live),
              "loglik": float(run.loglik[0]), "minutes": full.n, "sensor_start": variants.sensor_start, "masked_minutes": {d.room: d.info["n_masked"] for d in rooms}, "seconds": {"folds": t_fold, "total": time.time() - t0},
              "params": {n: {**d["params"], "c": d["c"], "valve_gain": d["valve_gain"], "sigma_u": d["sigma_u"], "valve_obs": d["valve_obs"]} for n, d in docs.items()}}
    return bundle, report


def last_lambda(lam, step):
    seen = lam[max(step - 30, 0):step]
    seen = seen[np.isfinite(seen)]
    return float(seen[-1]) if len(seen) else None


def reset_list(full, resets, truth, rec):
    opens = truth.opens(full.names)
    out = []
    for x in resets:
        t = float(full.times[x["step"]])
        near = [(a - t) / 60.0 for r, a in opens if r == x["room"]]
        out.append({"room": x["room"], "cause": x["cause"], "time": datetime.fromtimestamp(t, timezone.utc).strftime("%Y-%m-%dT%H:%MZ"), "lambda_before": last_lambda(rec[x["room"]]["lam"], x["step"]), "min_to_next_open": min(near, key=lambda d: abs(d)) if near else None})
    return out


def window_model_sigma(models_dir):
    base = Path(models_dir) / "window"
    out = {}
    for d in sorted(base.glob("*Z"), reverse=True) if base.is_dir() else []:
        f = d / "bundle.json"
        if f.is_file():
            b = json.loads(f.read_text())
            out = {r: m["sigma"] for r, m in b.get("rooms", {}).items() if "sigma" in m}
            break
    return out


def roommodel(config, src, models_dir):
    rooms, skipped = build_data(config, src)
    bundle, report = fit_bundle(config, rooms, src)
    out = Path(models_dir) / "room_model" / bundle["version"]
    out.mkdir(parents=True, exist_ok=True)
    (out / "bundle.json").write_text(json.dumps(bundle, indent=1, default=float))
    report["skipped"] = skipped
    report["window_model_sigma"] = window_model_sigma(models_dir)
    (out / "report.json").write_text(json.dumps(report, indent=1, default=float))
    return out, bundle, report
