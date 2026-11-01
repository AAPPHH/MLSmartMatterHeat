import os, re, socket, struct, sys, time, hashlib
from cryptography.hazmat.primitives import hashes
from cryptography.hazmat.primitives.asymmetric import ec
from cryptography.hazmat.primitives.serialization import Encoding, PublicFormat
from cryptography.hazmat.primitives.asymmetric.utils import decode_dss_signature, encode_dss_signature
from cryptography.hazmat.primitives.ciphers.aead import AESCCM
from cryptography.hazmat.primitives.kdf.hkdf import HKDF

CFG = {
    "port": int(os.environ["RESP_PORT"]),
    "scenario": os.environ.get("RESP_SCENARIO", "ok"),
    "result": os.environ["RESP_RESULT"],
    "vectors": os.environ["RESP_VECTORS"],
    "epoch_ipk": bytes.fromhex("235bf7e62823d358dca4ba50b1535f4b"),
    "late_s": 0.6,
    "idle_s": 8.0,
}


def vectors():
    text = open(CFG["vectors"]).read()
    out = {}
    for m in re.finditer(r"static const uint8_t (\w+)\[\d+\] = \{([^}]*)\};", text):
        out[m.group(1)] = bytes(int(x, 16) for x in m.group(2).split(","))
    return out


def w_ctl(tag, t):
    return bytes([(0x20 if tag is not None else 0) | t]) + (bytes([tag]) if tag is not None else b"")


def t_struct(tag, body):
    return w_ctl(tag, 0x15) + body + b"\x18"


def t_array(tag, body):
    return w_ctl(tag, 0x16) + body + b"\x18"


def t_list(tag, body):
    return w_ctl(tag, 0x17) + body + b"\x18"


def t_bytes(tag, d):
    return w_ctl(tag, 0x10) + bytes([len(d)]) + d if len(d) < 256 else w_ctl(tag, 0x11) + struct.pack("<H", len(d)) + d


def t_uint(tag, v):
    for sz, code in ((1, 4), (2, 5), (4, 6), (8, 7)):
        if v < 1 << (8 * sz):
            return w_ctl(tag, code) + v.to_bytes(sz, "little")


def t_sint(tag, v):
    for sz, code in ((1, 0), (2, 1), (4, 2), (8, 3)):
        if -(1 << (8 * sz - 1)) <= v < 1 << (8 * sz - 1):
            return w_ctl(tag, code) + v.to_bytes(sz, "little", signed=True)


def t_parse(buf):
    p = 0
    items = []
    while p < len(buf):
        ctl = buf[p]
        p += 1
        tc, t = ctl >> 5, ctl & 31
        tag = None
        if tc == 1:
            tag = buf[p]
            p += 1
        elif tc:
            raise ValueError("tag form")
        if t <= 3:
            sz = 1 << t
            v = int.from_bytes(buf[p:p + sz], "little", signed=True)
            p += sz
        elif t <= 7:
            sz = 1 << (t - 4)
            v = int.from_bytes(buf[p:p + sz], "little")
            p += sz
        elif t in (8, 9):
            v = t == 9
        elif 12 <= t <= 19:
            ls = 1 << (t & 3)
            n = int.from_bytes(buf[p:p + ls], "little")
            p += ls
            v = buf[p:p + n]
            p += n
        elif 21 <= t <= 23:
            depth, q = 1, p
            while depth:
                c2 = buf[q]
                q += 1
                tc2, t2 = c2 >> 5, c2 & 31
                q += 1 if tc2 == 1 else 0
                if t2 <= 3:
                    q += 1 << t2
                elif t2 <= 7:
                    q += 1 << (t2 - 4)
                elif 12 <= t2 <= 19:
                    ls = 1 << (t2 & 3)
                    n = int.from_bytes(buf[q:q + ls], "little")
                    q += ls + n
                elif 21 <= t2 <= 23:
                    depth += 1
                elif t2 == 24:
                    depth -= 1
            v = buf[p:q - 1]
            p = q
        else:
            raise ValueError("type %d" % t)
        items.append((tag, v))
    return items


def field(buf, tag):
    for t, v in t_parse(buf):
        if t == tag:
            return v
    return None


def hkdf(salt, ikm, info, n):
    return HKDF(algorithm=hashes.SHA256(), length=n, salt=salt or None, info=info).derive(ikm)


def sha(*parts):
    h = hashlib.sha256()
    for p in parts:
        h.update(p)
    return h.digest()


def ecdsa_sign(priv, data):
    r, s = decode_dss_signature(priv.sign(data, ec.ECDSA(hashes.SHA256())))
    return r.to_bytes(32, "big") + s.to_bytes(32, "big")


def ecdsa_verify(pub, data, sig):
    pub.verify(encode_dss_signature(int.from_bytes(sig[:32], "big"), int.from_bytes(sig[32:], "big")), data, ec.ECDSA(hashes.SHA256()))


def pubkey(b):
    return ec.EllipticCurvePublicKey.from_encoded_point(ec.SECP256R1(), b)


class Responder:
    def __init__(self):
        v = vectors()
        self.noc = v["V_Node01_02_Chip"]
        self.priv = ec.derive_private_key(int.from_bytes(v["V_Node01_02_PrivateKey"], "big"), ec.SECP256R1())
        rc = v["V_Root01_Chip"]
        self.root_pub = field(field(rc, None), 9)
        noc_top = field(self.noc, None)
        subj = t_parse(field(noc_top, 6))
        self.node = dict(subj)[17]
        self.fabric = dict(subj)[21]
        self.node_id = self.node
        cf = hkdf(self.fabric.to_bytes(8, "big"), self.root_pub[1:], b"CompressedFabric", 8)
        self.ipk = hkdf(cf, CFG["epoch_ipk"], b"GroupKey v1.0", 16)
        self.sock = socket.socket(socket.AF_INET, socket.SOCK_DGRAM)
        self.sock.bind(("127.0.0.1", CFG["port"]))
        self.sock.settimeout(0.2)
        self.ctr = int.from_bytes(os.urandom(4), "little") & 0x0FFFFFFF
        self.secured = False
        self.dropped = False
        self.handled = set()
        self.written = None

    def parse(self, data):
        flags, sid, sflags, ctr = data[0], struct.unpack("<H", data[1:3])[0], data[3], struct.unpack("<I", data[4:8])[0]
        p = 8
        src = dst = None
        if flags & 4:
            src = struct.unpack("<Q", data[p:p + 8])[0]
            p += 8
        if flags & 3 == 1:
            dst = struct.unpack("<Q", data[p:p + 8])[0]
            p += 8
        return dict(flags=flags, sid=sid, sflags=sflags, ctr=ctr, src=src, dst=dst, hl=p)

    def parse_p(self, d):
        xf, op, xid, proto = d[0], d[1], struct.unpack("<H", d[2:4])[0], struct.unpack("<H", d[4:6])[0]
        p = 6
        ack = None
        if xf & 2:
            ack = struct.unpack("<I", d[p:p + 4])[0]
            p += 4
        return dict(xf=xf, op=op, xid=xid, proto=proto, ack=ack, body=d[p:])

    def send(self, addr, proto, op, xid, payload, reliable, ack=None, initiator=False):
        xf = (1 if initiator else 0) | (4 if reliable else 0) | (2 if ack is not None else 0)
        ph = bytes([xf, op]) + struct.pack("<HH", xid, proto) + (struct.pack("<I", ack) if ack is not None else b"") + payload
        self.ctr += 1
        if self.secured:
            hdr = bytes([0]) + struct.pack("<H", self.peer_sid) + bytes([0]) + struct.pack("<I", self.ctr)
            nonce = bytes([0]) + struct.pack("<I", self.ctr) + struct.pack("<Q", self.node_id)
            msg = hdr + AESCCM(self.r2i, tag_length=16).encrypt(nonce, ph, hdr)
        else:
            hdr = bytes([1]) + struct.pack("<H", 0) + bytes([0]) + struct.pack("<I", self.ctr) + struct.pack("<Q", self.peer_eph)
            msg = hdr + ph
        for _ in range(2 if CFG["scenario"] == "dup" else 1):
            self.sock.sendto(msg, addr)

    def status(self, general, code):
        return struct.pack("<HIH", general, 0, code)

    def sigma1(self, addr, h, p):
        body = p["body"]
        top = field(body, None)
        rnd, sid, dest, ipub = field(top, 1), field(top, 2), field(top, 3), field(top, 4)
        self.peer_eph = h["src"]
        self.peer_sid = sid
        self.xid = p["xid"]
        msg = rnd + self.root_pub + struct.pack("<QQ", self.fabric, self.node_id)
        import hmac
        want = hmac.new(self.ipk, msg, hashlib.sha256).digest()
        if CFG["scenario"] == "busy":
            self.send(addr, 0, 0x40, self.xid, self.status(1, 4), True, ack=h["ctr"])
            return
        if not hmac.compare_digest(want, dest):
            self.send(addr, 0, 0x40, self.xid, self.status(1, 1), True, ack=h["ctr"])
            return
        self.s1 = body
        self.init_pub = ipub
        self.eph = ec.generate_private_key(ec.SECP256R1())
        self.eph_pub = self.eph.public_key().public_bytes(Encoding.X962, PublicFormat.UncompressedPoint)
        self.shared = self.eph.exchange(ec.ECDH(), pubkey(ipub))
        rrand = os.urandom(32)
        tbs = t_struct(None, t_bytes(1, self.noc) + t_bytes(3, self.eph_pub) + t_bytes(4, ipub))
        sig = ecdsa_sign(self.priv, tbs)
        if CFG["scenario"] == "bad_sig":
            sig = bytes([sig[0] ^ 1]) + sig[1:]
        tbe = t_struct(None, t_bytes(1, self.noc) + t_bytes(3, sig) + t_bytes(4, os.urandom(16)))
        k = hkdf(self.ipk + rrand + self.eph_pub + sha(body), self.shared, b"Sigma2", 16)
        enc = AESCCM(k, tag_length=16).encrypt(b"NCASE_Sigma2N", tbe, None)
        self.s2 = t_struct(None, t_bytes(1, rrand) + t_uint(2, 0x4321) + t_bytes(3, self.eph_pub) + t_bytes(4, enc) + t_struct(5, t_uint(1, 500) + t_uint(2, 300)))
        self.local_sid = 0x4321
        if CFG["scenario"] == "late" and not self.handled_late:
            self.handled_late = True
            time.sleep(CFG["late_s"])
        self.send(addr, 0, 0x31, self.xid, self.s2, True, ack=h["ctr"])

    handled_late = False

    def sigma3(self, addr, h, p):
        top = field(p["body"], None)
        enc = field(top, 1)
        k = hkdf(self.ipk + sha(self.s1, self.s2), self.shared, b"Sigma3", 16)
        tbe = AESCCM(k, tag_length=16).decrypt(b"NCASE_Sigma3N", enc, None)
        t = field(tbe, None)
        noc, ic, sig = field(t, 1), field(t, 2), field(t, 3)
        ipub = field(field(noc, None), 9)
        tbs = t_struct(None, t_bytes(1, noc) + (t_bytes(2, ic) if ic else b"") + t_bytes(3, self.init_pub) + t_bytes(4, self.eph_pub))
        ecdsa_verify(pubkey(ipub), tbs, sig)
        self.init_node = dict(t_parse(field(field(noc, None), 6)))[17]
        keys = hkdf(self.ipk + sha(self.s1, self.s2, p["body"]), self.shared, b"SessionKeys", 48)
        self.i2r, self.r2i = keys[:16], keys[16:32]
        self.send(addr, 0, 0x40, self.xid, self.status(0, 0), True, ack=h["ctr"])
        self.secured = True
        self.sess_ctr = None

    def secure_msg(self, addr, h, p):
        body = p["body"]
        if p["proto"] == 0 and p["op"] == 0x10:
            return False
        if p["proto"] == 0 and p["op"] == 0x40:
            code = struct.unpack("<H", body[6:8])[0]
            return code == 3
        if p["proto"] == 1 and p["op"] == 6:
            top = field(body, None)
            item = field(field(top, 2), None)
            path = field(item, 1)
            ep, cl, at = [dict(t_parse(path))[i] for i in (2, 3, 4)]
            val = dict(t_parse(item))[2]
            self.written = "%d/0x%x/0x%x=%d" % (ep, cl, at, val)
            st = 0x87 if CFG["scenario"] == "write_fail" else 0
            rsp = t_struct(None, t_array(0, t_struct(None, t_list(0, t_uint(2, ep) + t_uint(3, cl) + t_uint(4, at)) + t_struct(1, t_uint(0, st)))) + t_uint(255, 11))
            self.send(addr, 1, 7, p["xid"], rsp, True, ack=h["ctr"])
        return False

    def run(self):
        deadline = time.time() + CFG["idle_s"]
        done = False
        while time.time() < deadline and not done:
            try:
                data, addr = self.sock.recvfrom(2048)
            except socket.timeout:
                continue
            if CFG["scenario"] == "silent":
                continue
            if CFG["scenario"] == "drop_first" and not self.dropped:
                self.dropped = True
                continue
            h = self.parse(data)
            try:
                if h["sid"] == 0:
                    if h["ctr"] in self.handled:
                        continue
                    self.handled.add(h["ctr"])
                    p = self.parse_p(data[h["hl"]:])
                    if p["op"] == 0x30:
                        self.sigma1(addr, h, p)
                    elif p["op"] == 0x32:
                        self.sigma3(addr, h, p)
                elif self.secured and h["sid"] == self.local_sid:
                    nonce = bytes([h["sflags"]]) + struct.pack("<I", h["ctr"]) + struct.pack("<Q", self.init_node)
                    pt = AESCCM(self.i2r, tag_length=16).decrypt(nonce, data[h["hl"]:], data[:h["hl"]])
                    done = self.secure_msg(addr, h, self.parse_p(pt))
            except Exception as e:
                print("responder error:", repr(e), file=sys.stderr)
        open(CFG["result"], "w").write(self.written or "none")


Responder().run()
