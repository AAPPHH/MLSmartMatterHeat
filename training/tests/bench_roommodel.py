import copy
import sys
import time
import numpy as np
from heattrain.__main__ import CONFIG
from heattrain.roomfit import fit_bundle
from test_thermal import simulate

HOURS = int(sys.argv[2]) if len(sys.argv) > 2 else 48


def rooms(hours=HOURS, n=4):
    out = []
    for i, name in enumerate(("wohnzimmer", "kueche", "bad", "schlafzimmer")[:n]):
        d, _, _ = simulate(hours=hours, seed=i, report_p=0.4)
        d.room, d.node = name, 5 + i
        d.sp = np.full(d.n, 21.0)
        d.yv = np.where(np.isfinite(d.y), d.u, np.nan)
        d.bad = np.zeros(d.n, bool)
        d.info["neighbour_nodes"] = {}
        d.info["n_masked"] = 0
        out.append(d)
    return out


if __name__ == "__main__":
    config = copy.deepcopy(CONFIG)
    rs = rooms()
    t = time.perf_counter()
    bundle, report = fit_bundle(config, rs, sys.argv[1] if len(sys.argv) > 1 else ".", version="20261101T000000Z")
    print(f"fit_bundle {len(rs)} rooms x {HOURS} h: {time.perf_counter() - t:.1f} s, shock {bundle['shock']}, nll {report['nll']:.6f}, resets {report['resets']}")
