#!/usr/bin/env bash
set -euo pipefail
ROOT=${ROOT:-$HOME/MLSmartMatterHeat}
STORAGE=${STORAGE:-$ROOT/chip-storage}
PAA=${PAA:-$ROOT/paa}
TOKEN_FILE=${TOKEN_FILE:-$ROOT/secrets/br_watchdog_token}
CREDS_FILE=${CREDS_FILE:-$ROOT/secrets/br_watchdog_creds.json}
BR_URL=${BR_URL:-http://192.168.178.33}
NODES=${NODES:-"5 6 7 8"}
ENDPOINT=${ENDPOINT:-1}
CONTROLLER_NODE_ID=${CONTROLLER_NODE_ID:-0x1B2D0001}
ISSUER_INDEX=${ISSUER_INDEX:-0}
FABRIC_ID=${FABRIC_ID:-}
IPK_HEX=${IPK_HEX:-74656d706f726172792069706b203031}
HEATD_IMAGE=${HEATD_IMAGE:-heatd:local}
CHIP_CERT_IMAGE=${CHIP_CERT_IMAGE:-chipcert:local}
CHIP_CERT_BIN=${CHIP_CERT_BIN:-}
STEP=${STEP:-all}
APPLY=${APPLY:-0}
TOOL_TIMEOUT=${TOOL_TIMEOUT:-120}
TRIES=${TRIES:-3}

umask 077
WORK=$(mktemp -d)
trap 'rm -rf "$WORK"' EXIT
mkdir -p "$WORK/storage"
cp -a "$STORAGE"/. "$WORK/storage"/

die() { echo "provision: $*" >&2; exit 1; }
want() { [ "$STEP" = all ] || [ "$STEP" = "$1" ]; }

kvs() {
    local v
    v=$(grep -ihm1 "^$1=" "$WORK"/storage/*.ini | head -1 | cut -d= -f2- | tr -d '\r\n') || true
    [ -n "$v" ] || die "key $1 not found in $STORAGE/*.ini (present: $(grep -ioh '^example[a-z]*[0-9a-f]*=' "$WORK"/storage/*.ini | tr '
' ' '))"
    printf '%s' "$v"
}

cert() {
    if [ -n "$CHIP_CERT_BIN" ]; then
        (cd "$WORK" && "$CHIP_CERT_BIN" "$@")
    else
        docker run --rm -u "$(id -u):$(id -g)" -v "$WORK:/work" -w /work "$CHIP_CERT_IMAGE" "$@"
    fi
}

tool() {
    docker run --rm --network host -u "$(id -u):$(id -g)" -v "$WORK/storage:/chip-storage" -v "$PAA:/opt/paa-root-certs:ro" --entrypoint chip-tool "$HEATD_IMAGE" \
        "$@" --storage-directory /chip-storage --paa-trust-store-path /opt/paa-root-certs --timeout "$TOOL_TIMEOUT"
}

retry() {
    local i
    for i in $(seq "$TRIES"); do
        "$@" && return 0
        echo "provision: attempt $i/$TRIES failed: $1 $2 $3" >&2
        sleep 5
    done
    return 1
}

kvs_file() {
    kvs "$1" | base64 -d > "$WORK/$2"
    [ -s "$WORK/$2" ] || die "key $1 is empty or not base64"
}

ec_der_hex() {
    python3 -I - "$1" <<'PY'
import sys
raw = open(sys.argv[1], "rb").read()
assert len(raw) == 97 and raw[0] == 4, "ExampleOpCredsICAKey is not a 97 byte serialized P256 keypair"
pub, priv = raw[:65], raw[65:]
print((bytes.fromhex("30770201010420") + priv + bytes.fromhex("a00a06082a8648ce3d030107a144034200") + pub).hex())
PY
}

make_credentials() {
    local fab=$FABRIC_ID nid
    nid=$(printf '%X' "$((CONTROLLER_NODE_ID))")
    kvs_file "ExampleOpCredsICAKey$ISSUER_INDEX" ica_key.bin
    kvs_file "ExampleCAIntermediateCert$ISSUER_INDEX" icac.der
    kvs_file "ExampleCARootCert$ISSUER_INDEX" rcac.der
    if [ -z "$fab" ]; then
        local i found=()
        for i in 1 2 3 4 5; do
            grep -iqm1 "^f/$i/n=" "$WORK"/storage/*.ini || continue
            kvs_file "f/$i/n" noc_probe.tlv
            cert convert-cert -p noc_probe.tlv noc_probe.pem
            found+=("$(openssl x509 -in "$WORK/noc_probe.pem" -noout -subject | grep -o '1\.3\.6\.1\.4\.1\.37244\.1\.5 *= *[0-9A-Fa-f]*' | sed 's/.*= *//')")
        done
        [ "${#found[@]}" -eq 1 ] && [ -n "${found[0]}" ] || die "fabric id not unique (${found[*]:-none}); set FABRIC_ID=<hex>"
        fab=${found[0]}
    fi
    openssl ecparam -name prime256v1 -genkey -noout -out "$WORK/ctl_key.pem"
    openssl ec -in "$WORK/ctl_key.pem" -outform DER -out "$WORK/ctl_key.der" 2>/dev/null
    cert convert-cert -x rcac.der rcac.hex
    cert convert-cert -x icac.der icac.hex
    ec_der_hex "$WORK/ica_key.bin" > "$WORK/ica_key.hex"
    chmod 600 "$WORK/ica_key.hex"
    cert gen-cert --type n --subject-chip-id "$nid" --subject-fab-id "$fab" --ca-key ica_key.hex --ca-cert icac.der --key ctl_key.pem --lifetime 4294967295 --out noc.hex --out-format chip-hex
    cert convert-cert -p noc.hex noc_check.pem
    cert convert-cert -p rcac.der rcac.pem
    cert convert-cert -p icac.der icac.pem
    cat "$WORK/rcac.pem" "$WORK/icac.pem" > "$WORK/chain.pem"
    openssl verify -no_check_time -partial_chain -CAfile "$WORK/chain.pem" "$WORK/noc_check.pem" >/dev/null 2>&1 || echo "provision: warning, openssl could not verify the NOC against root and ICA (Matter cert extensions); check the CASE result on the BR" >&2
    python3 -I - "$WORK" "$fab" "$nid" "$IPK_HEX" <<'PY'
import json, subprocess, sys
w, fab, nid, ipk = sys.argv[1:5]
rd = lambda n: open(f"{w}/{n}").read().strip().lower()
der = open(f"{w}/ctl_key.der", "rb").read()
priv = der[7:39]
pub = der[-65:]
assert der[:7] == bytes.fromhex("30740201010420") or der[:7] == bytes.fromhex("30770201010420"), "unexpected key DER"
assert pub[0] == 4 and len(priv) == 32
body = {"fabric_id": fab, "node_id": nid, "rcac": rd("rcac.hex"), "icac": rd("icac.hex"), "noc": rd("noc.hex"), "key": (pub + priv).hex(), "ipk": ipk}
for k in ("rcac", "icac", "noc"):
    assert len(body[k]) // 2 <= 400, k
assert len(ipk) == 32
json.dump(body, open(f"{w}/credentials.json", "w"))
PY
    install -m 600 "$WORK/credentials.json" "$CREDS_FILE"
    echo "credentials ready in $CREDS_FILE: fabric $fab, controller node 0x$nid"
}

PARSE_ACL=$(cat <<'PY'
import json, re, sys
def parse(text):
    entries, stack = None, []
    root = {}
    stack = [(-1, root)]
    for raw in text.splitlines():
        m = re.search(r"\[TOO\]\s?(.*)$", raw)
        if not m:
            continue
        line = m.group(1).rstrip()
        s = line.strip()
        if not s or s in ("}", "},", "]"):
            continue
        ind = len(line) - len(line.lstrip())
        while len(stack) > 1 and stack[-1][0] >= ind:
            stack.pop()
        cont = stack[-1][1]
        km = re.match(r"\[(\d+)\]:\s*(.*)$", s)
        if km:
            val = km.group(2)
            if val == "{":
                d = {}
                cont.append(d)
                stack.append((ind, d))
            else:
                cont.append(int(val) if re.fullmatch(r"-?\d+", val) else val)
            continue
        kv = re.match(r"([A-Za-z]+):\s*(.*)$", s)
        if not kv or not isinstance(cont, dict):
            continue
        k, val = kv.groups()
        lm = re.fullmatch(r"(\d+) entries", val)
        if lm:
            lst = []
            cont[k] = lst
            stack.append((ind, lst))
        elif val == "null":
            cont[k] = None
        else:
            cont[k] = int(val) if re.fullmatch(r"-?\d+", val) else val
    return root, text
def main():
    text = sys.stdin.read()
    root, _ = parse(text)
    acl = root.get("ACL")
    n = re.search(r"ACL:\s*(\d+) entries", text)
    if acl is None or n is None or len(acl) != int(n.group(1)):
        sys.exit("cannot parse ACL read output")
    out = []
    for e in acl:
        for k in ("Privilege", "AuthMode", "FabricIndex"):
            if not isinstance(e.get(k), int):
                sys.exit("ACL entry without " + k)
        subj = e.get("Subjects")
        tg = e.get("Targets")
        out.append({"fabricIndex": e["FabricIndex"], "privilege": e["Privilege"], "authMode": e["AuthMode"],
                    "subjects": [int(x) for x in subj] if subj is not None else None,
                    "targets": [{"cluster": t.get("Cluster"), "endpoint": t.get("Endpoint"), "deviceType": t.get("DeviceType")} for t in tg] if tg is not None else None})
    json.dump(out, sys.stdout)
main()
PY
)

PLAN_ACL=$(cat <<'PY'
import json, sys
old = json.load(open(sys.argv[1]))
node, ep = int(sys.argv[2], 0), int(sys.argv[3])
if not any(e["privilege"] == 5 for e in old):
    sys.exit("existing ACL has no Administer entry, refusing to touch it")
fab = old[0]["fabricIndex"]
new = {"fabricIndex": fab, "privilege": 3, "authMode": 2, "subjects": [node], "targets": [{"cluster": 513, "endpoint": ep, "deviceType": None}]}
if any(e["privilege"] == 3 and e["authMode"] == 2 and e["subjects"] == [node] and e["targets"] == new["targets"] for e in old):
    print("present")
else:
    json.dump(old + [new], open(sys.argv[4], "w"))
    print("append")
PY
)

CHECK_ACL=$(cat <<'PY'
import json, sys
old = json.load(open(sys.argv[1]))
now = json.load(open(sys.argv[2]))
node = int(sys.argv[3], 0)
key = lambda e: json.dumps(e, sort_keys=True)
missing = [e for e in old if key(e) not in {key(x) for x in now}]
mine = [e for e in now if e["privilege"] == 3 and e["subjects"] == [node]]
if missing or not mine or len(now) != len(old) + 1:
    sys.exit("verify failed: missing=%d mine=%d" % (len(missing), len(mine)))
PY
)

read_acl() {
    tool accesscontrol read acl "$1" 0 > "$WORK/acl_$1.raw" 2>&1 || return 1
    python3 -I -c "$PARSE_ACL" < "$WORK/acl_$1.raw" > "$2" || { tail -40 "$WORK/acl_$1.raw" >&2; return 1; }
}

write_acl() {
    tool accesscontrol write acl "$(cat "$2")" "$1" 0 > "$WORK/aclw_$1.raw" 2>&1
}

grant_acl() {
    local node=$1 nid=$((CONTROLLER_NODE_ID))
    retry read_acl "$node" "$WORK/old_$node.json" || die "node $node: ACL read failed"
    local plan
    plan=$(python3 -I -c "$PLAN_ACL" "$WORK/old_$node.json" "$nid" "$ENDPOINT" "$WORK/new_$node.json") || die "node $node: $plan"
    if [ "$plan" = present ]; then echo "node $node: entry already present"; return 0; fi
    echo "node $node: existing $(python3 -I -c 'import json,sys;print(len(json.load(open(sys.argv[1]))))' "$WORK/old_$node.json") entries, appending Operate on cluster 0x0201 endpoint $ENDPOINT for $CONTROLLER_NODE_ID"
    if [ "$APPLY" != 1 ]; then echo "node $node: DRY run, would write $(cat "$WORK/new_$node.json")"; return 0; fi
    retry write_acl "$node" "$WORK/new_$node.json" || die "node $node: ACL write failed"
    if ! { retry read_acl "$node" "$WORK/now_$node.json" && python3 -I -c "$CHECK_ACL" "$WORK/old_$node.json" "$WORK/now_$node.json" "$nid"; }; then
        echo "node $node: verification failed, restoring previous ACL" >&2
        retry write_acl "$node" "$WORK/old_$node.json" || echo "node $node: RESTORE FAILED, previous ACL is in $WORK/old_$node.json, copying to /tmp/acl_old_$node.json" >&2
        cp "$WORK/old_$node.json" "/tmp/acl_old_$node.json"
        die "node $node: ACL verification failed"
    fi
    echo "node $node: ACL ok"
}

push_credentials() {
    [ -s "$CREDS_FILE" ] || die "$CREDS_FILE missing, run STEP=cert first"
    [ -r "$TOKEN_FILE" ] || die "token file $TOKEN_FILE missing"
    if [ "$APPLY" != 1 ]; then echo "DRY run, would POST credentials ($(wc -c < "$CREDS_FILE") bytes) to $BR_URL/watchdog/credentials"; return 0; fi
    printf 'X-Heatd-Token: %s\n' "$(tr -d '\r\n' < "$TOKEN_FILE")" > "$WORK/hdr"
    curl -fsS --max-time 20 -X POST -H "Content-Type: application/json" -H @"$WORK/hdr" --data-binary @"$CREDS_FILE" "$BR_URL/watchdog/credentials"
    echo
}

want cert && make_credentials
if want acl; then
    for n in $NODES; do grant_acl "$n"; done
fi
want push && push_credentials
[ "$APPLY" = 1 ] || echo "provision: nothing was written (DRY run). Repeat with APPLY=1."
