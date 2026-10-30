import copy
import json
import shutil
import tempfile
from dataclasses import replace
from pathlib import Path
import numpy as np
import onnxruntime as ort
from heattrain.__main__ import CONFIG
from heattrain.thermal import RoomData, kalman, step_terms
from heattrain.thermalml import build_bundle, jsonable, env_from, feature_matrix, local_hour, optimum_start, origin_state, rollout, boost_policy, recorded, sensor_of
from test_thermal import simulate

OUT = Path(__file__).resolve().parents[2] / "crates" / "heatd" / "testdata"
VERSION = "20261030T000000Z"
FIXED_T_OFF = (0, 10, 30)


def base_params(tcfg, **over):
    p = {n: v[0] for n, v in tcfg["params"].items() if n != "c"}
    p.update(over)
    return p


def physics_cases(tcfg):
    out = []
    rc2 = base_params(tcfg, a1=8.0, a2=2.0, b1=0.6, b2=0.5, b3=0.04, b4=0.3, w=0.7, ts=55.0, ts_night=48.0, c_wall=0.1)
    rc1 = base_params(tcfg, a1=3.0, b2=0.4, b3=0.03, b4=0.2, ts=50.0, ts_night=44.0, c_wall=0.1)
    for struct, p in (("rc2", rc2), ("rc1", rc1)):
        for night in (False, True):
            for u in (0.0, 0.6, 1.0):
                x = [24.0, 19.5] if struct == "rc2" else [0.0, 19.5]
                a = [float(z) for z in step_terms(p, struct, u, 6.0, 3.0, 250.0, night, {"wall": (np.float64(1.0), np.float64(19.0))}, 60.0 / 3600)]
                nx = [a[0] * x[0] + a[1] * x[1] + a[4], a[2] * x[0] + a[3] * x[1] + a[5]]
                w = p["w"] if struct == "rc2" else 0.0
                out.append({"structure": struct, "params": p, "dt_s": 60.0, "night": night, "u": u, "tout": 6.0, "wind": 3.0, "rad": 250.0, "nb": {"wall": [1.0, 19.0]}, "x": x, "next": nx, "sensor_next": sensor_of(w, *nx)})
    return out, rc2, rc1


def kalman_cases(tcfg, rc2, rc1):
    n = 60
    rng = np.random.default_rng(5)
    u = np.clip(np.concatenate([np.zeros(20), np.ones(25), np.full(15, 0.3)]), 0, 1)
    y = 19.0 + np.cumsum(rng.normal(0.02, 0.05, n)) + 0.15 * np.cumsum(u) / 10
    y = np.round(y, 2)
    y[rng.random(n) < 0.6] = np.nan
    y[0] = 19.0
    night = np.zeros(n, bool)
    night[:10] = True
    out = []
    for struct, p in (("rc2", rc2), ("rc1", rc1)):
        d = RoomData("k", 1, 1_790_000_000.0, 60.0, y.copy(), y.copy(), u, np.full(n, np.nan), np.full(n, 6.0), np.full(n, 3.0), np.full(n, 100.0), night,
                     {"wall": (np.full(n, 1.0), np.full(n, 19.0))}, {})
        ks, v, s, xs, ps = kalman(p, d, struct, tcfg, collect="full")
        out.append({"structure": struct, "params": p, "cfg": {k: tcfg[k] for k in ("q_r", "q_a", "noise_r", "p0")}, "dt_s": 60.0, "y": y, "u": u, "night": night, "tout": d.tout, "wind": d.wind, "rad": d.rad,
                    "nb": {"wall": [1.0, 19.0]}, "x_post": xs, "p_post": ps, "innovation_steps": ks, "innovations": v, "innovation_var": s})
    return out


def feature_cases(cfg, ctx, out_dir, rng):
    lat = ort.InferenceSession(str(out_dir / "latent.onnx"))
    res = ort.InferenceSession(str(out_dir / "residual.onnx"))
    names, lat_names = ctx["names"], ctx["lat_names"]
    out = []
    for R in [r for r in ctx["rooms"] if r.valid]:
        for j in np.linspace(0, len(R.ks) - 1, 6).astype(int):
            k = R.ks[j]
            raw = {"S": R.yc[k - 60:k + 1][::-1].copy(), "V": R.d.u[k - 240:k + 1][::-1].copy(), "hour": R.hour[k], "outdoor_c": R.d.tout[k], "radiation_wm2": R.d.rad[k], "others_heating": R.others[k],
                   "phys_sensor_pred": R.pred1[k], "phys_air": R.kair[k]}
            if j % 2:
                raw.update(ambient_nvme_c=41.5, ambient_pkg_w=7.2, ambient_die_c=55.0)
            ctx1 = {kk: (np.asarray(v, float)[None, :] if kk in ("S", "V") else np.array([v], float)) for kk, v in raw.items()}
            F = feature_matrix(cfg, names, ctx1, R.name)
            Fl = F[:, ctx["nets"]["latent"].cols]
            out.append({"room": R.name, "ctx": {kk: v for kk, v in raw.items()}, "residual_vector": F[0], "latent_vector": Fl[0],
                        "room_temp": float(lat.run(None, {"x": Fl})[0][0, 0]), "delta": float(res.run(None, {"x": F})[0][0, 0])})
    return out


def rollout_cases(cfg, tcfg, ctx, target=21.0):
    out = []
    nets = ctx["nets"]
    R = next(r for r in ctx["rooms"] if r.valid)
    ks = np.array([R.d.n // 2])
    x0, S0, V0 = origin_state(cfg, R, ks)
    x0[:], S0[:], V0[:] = 17.0, 17.0, 0.0
    env = env_from(R, ks, cfg["horizon_min"])
    env["others"][:] = 1.0
    H = cfg["horizon_min"]
    for variant, kw in (("grey_box", {}), ("residual", {"latent": nets["latent"], "residual": nets["residual"]})):
        r = optimum_start(cfg, tcfg, R.doc, R.name, ctx["names"], x0, S0, V0, env, target, **kw)
        fixed = []
        for T in FIXED_T_OFF:
            o = rollout(cfg, tcfg, R.doc, R.name, ctx["names"], x0, S0, V0, env, boost_policy(np.array([float(T)]), target, R.doc["valve_gain"]), H, **kw)
            fixed.append({"t_off_min": T, "peak": o["air"].max(), "air": o["air"][0], "sensor": o["sensor"][0], "u": o["u"][0]})
        out.append({"variant": variant, "room": R.name, "target": target, "boost_setpoint": cfg["boost_setpoint"], "horizon_min": H, "toff_max": cfg["toff_max"], "overshoot_tol": cfg["overshoot_tol"], "feedback": cfg["residual_feedback"],
                    "x0": x0[0], "S0": S0[0], "V0": V0[0], "env": {"tout": env["tout"][0], "wind": env["wind"][0], "rad": env["rad"][0], "night": env["night"][0], "hour": env["hour"][0], "others": env["others"][0],
                                                                  "nb": {k: [n[0], s[0]] for k, (n, s) in env["nb"].items()}},
                    "t_off_min": r["t_off_min"], "predicted_peak_c": r["predicted_peak_c"], "air": r["air"], "sensor": r["sensor"], "peaks": r["peaks"], "fixed": fixed})
    return out


def main():
    conf = copy.deepcopy(CONFIG)
    conf["thermalml"].update(xgb_latent={"n_estimators": 25, "max_depth": 3, "learning_rate": 0.1}, xgb_residual={"n_estimators": 20, "max_depth": 3, "learning_rate": 0.1}, min_residual_rows=300, min_latent_labels=100, parity_rows=200)
    rooms = []
    for i, n in enumerate(("wohnzimmer", "schlafzimmer")):
        d, _, _ = simulate(hours=120, seed=i, report_p=0.3)
        rooms.append(replace(d, room=n, node=i + 1, sp=np.full(d.n, 21.0)))
    d, _, _ = simulate(hours=6, seed=9)
    rooms.append(replace(d, room="bad", node=9, sp=np.full(d.n, 21.0)))
    tcfg = conf["thermal"]
    with tempfile.TemporaryDirectory() as tmp:
        out, bundle, ctx = build_bundle(conf, rooms, {}, Path(tmp), version=VERSION)
        cfg = dict(ctx["cfg"], residual_feedback=0.5)
        dst = OUT / "thermal_bundle"
        shutil.rmtree(dst, ignore_errors=True)
        dst.mkdir(parents=True)
        for f in ("bundle.json", "latent.onnx", "residual.onnx"):
            shutil.copy(out / f, dst / f)
        bj = json.loads((dst / "bundle.json").read_text())
        bj["residual"]["feedback"] = cfg["residual_feedback"]
        (dst / "bundle.json").write_text(json.dumps(bj, indent=1))
        phys, rc2, rc1 = physics_cases(tcfg)
        fx = {"tol": 1e-4, "bundle_dir": "thermal_bundle", "dt_s": 60, "physics": phys, "kalman": kalman_cases(tcfg, rc2, rc1), "features": feature_cases(cfg, ctx, out, np.random.default_rng(0)),
              "rollouts": rollout_cases(cfg, tcfg, ctx)}
    (OUT / "thermal_ml_parity.json").write_text(json.dumps(jsonable(fx), indent=None, separators=(",", ":")))


if __name__ == "__main__":
    main()
