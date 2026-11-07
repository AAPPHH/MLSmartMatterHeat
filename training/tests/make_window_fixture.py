import shutil
import sys
import tempfile
from pathlib import Path
from heattrain.__main__ import CONFIG
from heattrain.windowml import parity_csv, window_bundle

OUT = Path(__file__).resolve().parents[2] / "crates" / "heatd" / "tests" / "fixtures"
SPAN = ["2026-11-07T02:00:00+00:00", "2026-11-07T11:00:00+00:00"]


def main(lake):
    with tempfile.TemporaryDirectory() as tmp:
        out, bundle, _, _, grid = window_bundle(CONFIG, lake, tmp, version="parity")
        shutil.copy(out / "bundle.json", OUT / "window_bundle.json")
        (OUT / "window_parity.csv").write_text(parity_csv(bundle, grid, grid.span(*SPAN)))


if __name__ == "__main__":
    main(sys.argv[1] if len(sys.argv) > 1 else CONFIG["window"]["lake"])
