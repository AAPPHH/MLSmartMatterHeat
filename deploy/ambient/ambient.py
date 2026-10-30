import glob, json, os, sys, time, urllib.request

CONFIG = {
    "heatd_url": "http://127.0.0.1:8090/api/ambient",
    "interval_s": 60,
    "sysfs": "/sys",
    "br_http": {"enabled": False, "url": "http://192.168.178.33/ambient", "json_path": ["die_temperature_c"], "timeout_s": 5},
}


def read_text(path):
    with open(path) as f:
        return f.read().strip()


def nvme_c(cfg):
    for hwmon in sorted(glob.glob(os.path.join(cfg["sysfs"], "class/hwmon/hwmon*"))):
        try:
            if read_text(os.path.join(hwmon, "name")) == "nvme":
                return int(read_text(os.path.join(hwmon, "temp1_input"))) / 1000.0
        except (OSError, ValueError):
            continue
    return None


class PackagePower:
    def __init__(self, cfg):
        self.dir = os.path.join(cfg["sysfs"], "class/powercap/intel-rapl:0")
        self.last = None

    def read(self):
        try:
            energy = int(read_text(os.path.join(self.dir, "energy_uj")))
            span = int(read_text(os.path.join(self.dir, "max_energy_range_uj")))
        except (OSError, ValueError):
            self.last = None
            return None
        now = time.monotonic()
        previous, self.last = self.last, (energy, now)
        if previous is None or now <= previous[1]:
            return None
        delta = energy - previous[0]
        if delta < 0:
            delta += span
        return delta / 1e6 / (now - previous[1])


def br_die_c(cfg):
    br = cfg["br_http"]
    if not br["enabled"]:
        return None
    try:
        with urllib.request.urlopen(br["url"], timeout=br["timeout_s"]) as r:
            value = json.loads(r.read().decode())
        for key in br["json_path"]:
            value = value[key]
        return float(value)
    except (OSError, ValueError, KeyError, TypeError, IndexError):
        return None


def post(cfg, source, readings):
    readings = [r for r in readings if r["value"] is not None]
    if not readings:
        return
    body = json.dumps({"source": source, "ts": int(time.time() * 1000), "readings": readings}).encode()
    req = urllib.request.Request(cfg["heatd_url"], body, {"Content-Type": "application/json"})
    try:
        urllib.request.urlopen(req, timeout=10).read()
    except OSError as e:
        print(f"ambient post {source} failed: {e}", file=sys.stderr, flush=True)


def sample(cfg, power):
    post(cfg, "minix", [{"sensor": "nvme_c", "value": nvme_c(cfg), "unit": "C"}, {"sensor": "pkg_w", "value": power.read(), "unit": "W"}])
    post(cfg, "br", [{"sensor": "die_c", "value": br_die_c(cfg), "unit": "C"}])


def main(cfg=CONFIG):
    power = PackagePower(cfg)
    while True:
        started = time.monotonic()
        sample(cfg, power)
        time.sleep(max(1.0, cfg["interval_s"] - (time.monotonic() - started)))


if __name__ == "__main__":
    main()
