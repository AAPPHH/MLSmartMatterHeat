import json, sys, base64, urllib.request, urllib.parse, pathlib, re
out = pathlib.Path(sys.argv[1]); base = "https://on.dcl.csa-iot.org/dcl/pki"
get = lambda u: json.load(urllib.request.urlopen(u, timeout=30))
roots = get(f"{base}/root-certificates")["approvedRootCertificates"]["certs"]
n = 0
for r in roots:
    u = f"{base}/certificates/{urllib.parse.quote(r['subject'], safe='')}/{urllib.parse.quote(r['subjectKeyId'], safe='')}"
    try:
        for c in get(u)["approvedCertificates"]["certs"]:
            pem = c["pemCert"]; der = base64.b64decode("".join(l for l in pem.strip().splitlines() if "CERTIFICATE" not in l))
            name = re.sub(r"[^A-Za-z0-9]+", "_", (c.get("subjectAsText") or r["subjectKeyId"]))[:80] + "_" + r["subjectKeyId"].replace(":", "")[:8]
            (out / f"dcld_{name}.der").write_bytes(der); n += 1
    except Exception as e:
        print("skip", r["subjectKeyId"], e)
print("roots", len(roots), "written", n)
print([p.name for p in out.glob("*urotronic*")])
