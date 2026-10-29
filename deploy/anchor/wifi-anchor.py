import json, re, subprocess, time, urllib.request
out = subprocess.run(["iw", "dev", "wlo1", "scan"], capture_output=True, text=True, timeout=60).stdout
aps, cur = [], None
for line in out.splitlines():
    m = re.match(r"BSS ([0-9a-f:]{17})", line)
    if m:
        cur = {"bssid": m.group(1)}
        aps.append(cur)
    elif cur is not None and (f := re.match(r"\s+freq: (\d+)", line)):
        cur["freq"] = int(float(f.group(1)))
    elif cur is not None and (s := re.match(r"\s+signal: (-?[\d.]+) dBm", line)):
        cur["rssi"] = round(float(s.group(1)))
aps = [a for a in aps if "rssi" in a and "freq" in a]
body = json.dumps({"ts": int(time.time() * 1000), "device": "minix-anchor", "motion": "still", "aps": aps}).encode()
req = urllib.request.Request("http://127.0.0.1:8090/api/scan", body, {"Content-Type": "application/json"})
print(urllib.request.urlopen(req, timeout=20).read().decode()[:200])
