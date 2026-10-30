import json
import sys
from pathlib import Path
import numpy as np
import onnx
from onnx import TensorProto, helper, numpy_helper
from heattrain.thermal import RoomData, ThermalModel, kalman

CFG = {"q_r": 0.5, "q_a": 0.05, "noise_r": 0.15, "p0": 1.0, "mc_samples": 0, "mc_seed": 0}
RC2 = {"a1": 2.0, "a2": 3.0, "b1": 0.5, "b2": 0.1, "b3": 0.01, "b4": 0.2, "w": 0.6, "ts": 45.0, "ts_night": 40.0, "c_wall": 0.05, "c_ceiling": 0.02}
RC1 = {"a1": 0.5, "b2": 0.15, "b3": 0.0, "b4": 0.1, "ts": 45.0, "ts_night": 40.0, "c_wall": 0.05, "c_ceiling": 0.02}


def doc(structure, params, gain):
    return {"room": "t", "structure": structure, "params": params, "valve_gain": gain, "night_hours": [22, 5], "names": [], "neighbours": {"wall": 2, "ceiling": 1}}


def scenario(structure, params):
    rng = np.random.default_rng(7)
    n = 120
    u = np.clip(np.concatenate([np.zeros(20), np.ones(40), 0.4 * np.ones(30), rng.uniform(0, 1, 30)]), 0, 1)
    y = 19.0 + np.cumsum(rng.normal(0.02, 0.05, n))
    y[rng.uniform(size=n) < 0.7] = np.nan
    y[0] = 19.0
    night = np.arange(n) >= 70
    tout = 5.0 + 0.01 * np.arange(n)
    wind = 2.0 + np.sin(np.arange(n) / 10)
    rad = 80.0 * np.abs(np.sin(np.arange(n) / 30))
    nb = {"wall": (np.full(n, 2.0), 2 * (19.5 + 0.01 * np.arange(n))), "ceiling": (np.ones(n), np.full(n, 18.0))}
    d = RoomData("t", 1, 0.0, 60.0, y, np.nan_to_num(y), u, np.full(n, np.nan), tout, wind, rad, night, nb)
    p = params
    ks, vs, ss, xs, ps = kalman(p, d, structure, CFG, collect=True)
    return {
        "u": u.tolist(), "y": [None if v != v else float(v) for v in y], "night": night.tolist(), "tout": tout.tolist(), "wind": wind.tolist(), "rad": rad.tolist(),
        "nb": {k: {"n": a.tolist(), "s": b.tolist()} for k, (a, b) in nb.items()},
        "x_after_update": xs.tolist(), "innov_idx": ks.tolist(), "innov": vs.tolist(), "innov_var": ss.tolist(), "p22": ps.tolist(),
    }


def predictions(structure, params, gain):
    model = ThermalModel(doc(structure, params, gain), CFG)
    nb = {"wall": (2.0, 39.0), "ceiling": (1.0, 18.0)}
    cases = []
    for name, kw in (
        ("u_const_day", {"u": 1.0, "night": False}),
        ("u_const_night", {"u": 0.5, "night": True}),
        ("setpoint_pi_day", {"setpoint": 28.0, "night": False}),
        ("setpoint_pi_night", {"setpoint": 21.0, "night": True}),
    ):
        out = model.predict((19.5, 19.0), 90, 4.0, wind=3.0, rad=120.0, nb=nb, **kw)
        cases.append({"name": name, "x0": [19.5, 19.0], "steps": 90, "tout": 4.0, "wind": 3.0, "rad": 120.0, "nb": {k: list(v) for k, v in nb.items()}, **{k: v for k, v in kw.items()}, "out": out.tolist()})
    return cases


LATENT = (["sensor_lag_0", "phys_air", "valve_lag_0", "room_t"], [0.3, 0.7, 0.0, 0.0], 0.0)
RESIDUAL = (["valve_lag_0", "phys_sensor_pred", "outdoor_c", "room_t"], [0.05, 0.0, 0.0, 0.0], -0.01)


def linear(path, weights, bias, output):
    w = numpy_helper.from_array(np.array(weights, dtype=np.float32).reshape(-1, 1), "w")
    b = numpy_helper.from_array(np.array([bias], dtype=np.float32), "b")
    graph = helper.make_graph(
        [helper.make_node("MatMul", ["x", "w"], ["m"]), helper.make_node("Add", ["m", "b"], [output])], "lin",
        [helper.make_tensor_value_info("x", TensorProto.FLOAT, [None, len(weights)])], [helper.make_tensor_value_info(output, TensorProto.FLOAT, [None, 1])], [w, b])
    model = helper.make_model(graph, opset_imports=[helper.make_opsetid("", 13)])
    model.ir_version = 9
    onnx.save(model, str(path))


def bundle(directory, rc2_doc):
    directory = Path(directory)
    directory.mkdir(parents=True, exist_ok=True)
    linear(directory / "latent.onnx", LATENT[1], LATENT[2], "room_temp")
    linear(directory / "residual.onnx", RESIDUAL[1], RESIDUAL[2], "delta")
    doc = {
        "version": directory.name, "created": "2026-01-01T00:00:00Z", "dt_s": 60, "history_min": 60, "lag_step_min": 5,
        "rooms": {"t": {"physics": rc2_doc, "sensor_offset_c": 0.4, "valid": True}}, "room_index": ["t"],
        "latent": {"file": "latent.onnx", "features": LATENT[0]}, "residual": {"file": "residual.onnx", "features": RESIDUAL[0]},
        "metrics": {"backtest": {"t": {"cycles": 5, "grey_box": {"rmse": 0.3}, "residual": {"rmse": 0.2}}}},
    }
    (directory / "bundle.json").write_text(json.dumps(doc, indent=1))
    inputs = {"sensor_lag_0": 20.5, "phys_air": 19.8, "valve_lag_0": 0.6, "phys_sensor_pred": 20.6, "outdoor_c": 4.0, "room_t": 1.0}
    row = lambda feats: np.array([[inputs[f] for f in feats[0]]], dtype=np.float32)
    lat = float((row(LATENT) @ np.array(LATENT[1], dtype=np.float32).reshape(-1, 1))[0, 0] + LATENT[2])
    res = float((row(RESIDUAL) @ np.array(RESIDUAL[1], dtype=np.float32).reshape(-1, 1))[0, 0] + RESIDUAL[2])
    return {"inputs": inputs, "latent": lat, "residual": res}


def main(target, bundle_dir=None):
    out = {"cfg": CFG, "models": {}}
    for structure, params, gain in (("rc2", RC2, 0.35), ("rc1", RC1, 0.5)):
        out["models"][structure] = {"doc": doc(structure, params, gain), "predict": predictions(structure, params, gain), "kalman": scenario(structure, params)}
    if bundle_dir:
        out["onnx"] = bundle(bundle_dir, out["models"]["rc2"]["doc"])
    Path(target).write_text(json.dumps(out, indent=None, default=float))


if __name__ == "__main__":
    main(*sys.argv[1:])
