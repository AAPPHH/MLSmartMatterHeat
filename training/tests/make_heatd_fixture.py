import json
import shutil
import sys
import tempfile
from pathlib import Path
import numpy as np
from conftest import SYNTH, layout, make_lake
from heattrain.__main__ import CONFIG
from heattrain.watchroom import train

OUT = Path(__file__).resolve().parents[2] / "crates" / "heatd" / "tests" / "fixtures" / "watchroom_trained"


def samples(meta):
    means = layout()
    rng = np.random.default_rng(11)
    out = []
    for room in meta["rooms"]:
        for _ in range(3):
            aps = [[s, int(round(m + rng.normal(0, 1.5)))] for s, m in zip([f"aa:bb:cc:00:00:{i:02x}" for i in range(SYNTH["n_aps"])], means[room]) if m >= SYNTH["sensitivity"]]
            out.append({"room": room, "aps": aps})
    return out


def main():
    with tempfile.TemporaryDirectory() as tmp:
        lake = make_lake(Path(tmp) / "lake", 8)
        version = train(dict(CONFIG, eval_dir=Path(tmp) / "eval"), lake, Path(tmp) / "models", version="py1")
        shutil.rmtree(OUT, ignore_errors=True)
        shutil.copytree(version, OUT / "py1")
        (OUT / "samples.json").write_text(json.dumps(samples(json.loads((version / "model.json").read_text()))))


main()
