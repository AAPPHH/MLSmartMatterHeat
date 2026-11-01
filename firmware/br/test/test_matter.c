#include <stdio.h>
#include <stdlib.h>
#include <string.h>
#include "../main/matter.h"
#include "vectors.h"

static int failures;
static int checks;

#define CHECK(c)                                              \
    do {                                                      \
        checks++;                                             \
        if (!(c)) {                                           \
            failures++;                                       \
            printf("FAIL %s:%d %s\n", __FILE__, __LINE__, #c); \
        }                                                     \
    } while (0)

static void hex(const char *s, uint8_t *o)
{
    for (; s[0] && s[1]; s += 2) {
        unsigned v;
        sscanf(s, "%2x", &v);
        *o++ = (uint8_t)v;
    }
}

static int test_rng(void *c, uint8_t *b, size_t n)
{
    (void)c;
    for (size_t i = 0; i < n; i++) b[i] = (uint8_t)rand();
    return 0;
}

static void test_tlv(void)
{
    uint8_t b[64];
    tlv_w_t w;
    tlv_w_init(&w, b, sizeof(b));
    tlv_w_start(&w, -1, TLV_STRUCT);
    tlv_w_uint(&w, 0, 42);
    tlv_w_sint(&w, 1, -17);
    tlv_w_end(&w);
    uint8_t exp[] = {0x15, 0x24, 0x00, 0x2a, 0x20, 0x01, 0xef, 0x18};
    CHECK(!w.err && w.len == sizeof(exp) && !memcmp(b, exp, sizeof(exp)));
    tlv_r_t r;
    tlv_e_t e, in;
    tlv_r_init(&r, b, w.len);
    CHECK(tlv_next(&r, &e) && e.type == TLV_STRUCT && e.tag == -1 && e.n == 6 && r.p == r.end);
    CHECK(tlv_find(e.d, e.n, 0, &in) && in.type == TLV_UINT && in.u == 42);
    CHECK(tlv_find(e.d, e.n, 1, &in) && in.type == TLV_SINT && in.s == -17);
    CHECK(!tlv_find(e.d, e.n, 2, &in));
    tlv_w_init(&w, b, sizeof(b));
    tlv_w_sint(&w, 4, 1800);
    tlv_w_uint(&w, 5, 0x1234567890ULL);
    tlv_w_bool(&w, 6, true);
    tlv_w_bytes(&w, 7, (const uint8_t *)"abc", 3);
    uint8_t exp2[] = {0x21, 0x04, 0x08, 0x07, 0x27, 0x05, 0x90, 0x78, 0x56, 0x34, 0x12, 0x00, 0x00, 0x00, 0x29, 0x06, 0x30, 0x07, 0x03, 'a', 'b', 'c'};
    CHECK(w.len == sizeof(exp2) && !memcmp(b, exp2, sizeof(exp2)));
    tlv_r_init(&r, b, w.len);
    CHECK(tlv_next(&r, &e) && e.type == TLV_SINT && e.s == 1800 && e.tag == 4);
    CHECK(tlv_next(&r, &e) && e.type == TLV_UINT && e.u == 0x1234567890ULL);
    CHECK(tlv_next(&r, &e) && e.type == TLV_BOOL && e.u == 1);
    CHECK(tlv_next(&r, &e) && e.type == TLV_BYTES && e.n == 3 && !memcmp(e.d, "abc", 3));
    CHECK(!tlv_next(&r, &e));
    uint8_t trunc[] = {0x15, 0x24, 0x00, 0x2a};
    tlv_r_init(&r, trunc, sizeof(trunc));
    CHECK(!tlv_next(&r, &e) && r.p == NULL);
    uint8_t nest[] = {0x15, 0x35, 0x01, 0x24, 0x02, 0x07, 0x18, 0x24, 0x03, 0x09, 0x18};
    tlv_r_init(&r, nest, sizeof(nest));
    CHECK(tlv_next(&r, &e) && e.n == sizeof(nest) - 2);
    CHECK(tlv_find(e.d, e.n, 3, &in) && in.u == 9 && tlv_find(e.d, e.n, 1, &in) && in.type == TLV_STRUCT && in.n == 3);
}

static void test_headers(void)
{
    uint8_t b[32];
    mc_hdr_t h = {0}, d;
    h.sid = 0xA1B2;
    h.ctr = 0x12345678;
    h.has_src = true;
    h.src = 0x1122334455667788ULL;
    int n = mc_hdr_encode(&h, b, sizeof(b));
    uint8_t exp[] = {0x04, 0xb2, 0xa1, 0x00, 0x78, 0x56, 0x34, 0x12, 0x88, 0x77, 0x66, 0x55, 0x44, 0x33, 0x22, 0x11};
    CHECK(n == 16 && !memcmp(b, exp, 16));
    CHECK(mc_hdr_decode(b, n, &d) == 16 && d.sid == 0xA1B2 && d.ctr == 0x12345678 && d.has_src && d.src == h.src && !d.has_dst);
    h.has_src = false;
    h.has_dst = true;
    h.dst = 7;
    n = mc_hdr_encode(&h, b, sizeof(b));
    CHECK(n == 16 && b[0] == 0x01 && mc_hdr_decode(b, n, &d) == 16 && d.has_dst && d.dst == 7);
    b[0] = 0x10;
    CHECK(mc_hdr_decode(b, n, &d) < 0);
    b[0] = 0x03;
    CHECK(mc_hdr_decode(b, n, &d) < 0);
    b[0] = 0x00;
    b[3] = 0x80;
    CHECK(mc_hdr_decode(b, n, &d) < 0);
    CHECK(mc_hdr_decode(b, 7, &d) < 0);
    mc_phdr_t p = {XF_I | XF_R | XF_A, 0x30, 0xBEEF, 0, 0xCAFEBABE, 0}, q;
    n = mc_phdr_encode(&p, b, sizeof(b));
    uint8_t expp[] = {0x07, 0x30, 0xef, 0xbe, 0x00, 0x00, 0xbe, 0xba, 0xfe, 0xca};
    CHECK(n == 10 && !memcmp(b, expp, 10));
    CHECK(mc_phdr_decode(b, n, &q) == 10 && q.xflags == 7 && q.opcode == 0x30 && q.xid == 0xBEEF && q.proto == 0 && q.ack == 0xCAFEBABE);
    uint8_t vend[] = {0x10, 0x01, 0x01, 0x00, 0x34, 0x12, 0x01, 0x00};
    CHECK(mc_phdr_decode(vend, sizeof(vend), &q) < 0);
    uint8_t nc[13];
    mc_nonce(0x00, 0x11223344, 0x8877665544332211ULL, nc);
    uint8_t expn[] = {0x00, 0x44, 0x33, 0x22, 0x11, 0x11, 0x22, 0x33, 0x44, 0x55, 0x66, 0x77, 0x88};
    CHECK(!memcmp(nc, expn, 13));
}

static void test_crypto(void)
{
    uint8_t ikm[22], salt[13], info[10], okm[42], exp[42];
    memset(ikm, 0x0b, 22);
    for (int i = 0; i < 13; i++) salt[i] = (uint8_t)i;
    for (int i = 0; i < 10; i++) info[i] = (uint8_t)(0xf0 + i);
    hex("3cb25f25faacd57a90434f64d0362f2a2d2d0a90cf1a5a4c5db02d56ecc4c5bf34007208d5b887185865", exp);
    CHECK(mc_hkdf(salt, 13, ikm, 22, info, 10, okm, 42) == 0 && !memcmp(okm, exp, 42));
    uint8_t exp3[42];
    hex("8da4e775a563c18f715f802a063c5a31b8a11f5c5ee1879ec3454e5f3c738d2d9d201395faa4b61a96c8", exp3);
    CHECK(mc_hkdf(NULL, 0, ikm, 22, NULL, 0, okm, 42) == 0 && !memcmp(okm, exp3, 42));

    uint8_t root[65], cf[8], expc[8];
    hex("044a9f42b1ca4840d37292bbc7f6a7e11e22200c976fc900dbc98a7a383a641cb8254a2e56d4e295a847943b4e3897c4a773e930277b4d9fbede8a052686bfacfa", root);
    CHECK(mc_compressed_fabric_id(root, 0x2906C908D115D362ULL, cf) == 0);
    hex("87e1b004e235a130", expc);
    CHECK(!memcmp(cf, expc, 8));
    uint8_t epoch[16], ipk[16], expi[16];
    hex("235bf7e62823d358dca4ba50b1535f4b", epoch);
    hex("a6f5306baf6d050af23ba4bd6b9dd960", expi);
    CHECK(mc_ipk_operational(epoch, cf, ipk) == 0 && !memcmp(ipk, expi, 16));
    uint8_t opk[16], rnd[32], dst[32], expd[32];
    hex("9bc61cd9c62a2df6d64dfcaa9dc472d4", opk);
    hex("7e171231568dfa17206b3accf8faec2f4d21b580113196f47c7c4deb810a73dc", rnd);
    hex("dc35dd5fc9134cc5544538c9c3fc4297c1ec3370c839136a80e10796451d4c53", expd);
    CHECK(mc_dest_id(opk, rnd, root, 0x2906C908D115D362ULL, 0xCD5544AA7B13EF14ULL, dst) == 0 && !memcmp(dst, expd, 32));
    CHECK(mc_dest_id(opk, rnd, root, 0x2906C908D115D362ULL, 0xCD5544AA7B13EF15ULL, dst) == 0 && memcmp(dst, expd, 32));

    uint8_t key[16] = {0}, nonce[13] = {1, 2, 3}, buf[48 + 16], aad[8] = {9, 8, 7};
    memset(buf, 0x5a, 48);
    uint8_t orig[48];
    memcpy(orig, buf, 48);
    CHECK(mc_ccm_seal(key, nonce, aad, 8, buf, 48) == 0 && memcmp(buf, orig, 48));
    uint8_t t[64];
    memcpy(t, buf, 64);
    CHECK(mc_ccm_open(key, nonce, aad, 8, buf, 64) == 0 && !memcmp(buf, orig, 48));
    t[63] ^= 1;
    CHECK(mc_ccm_open(key, nonce, aad, 8, t, 64) != 0);
    CHECK(mc_ct_eq(key, key, 16) && !mc_ct_eq(key, nonce, 13));

    uint8_t priv[32], sig[64];
    memcpy(priv, V_Node01_01_PrivateKey, 32);
    mc_cert_t c;
    CHECK(mc_cert_parse(V_Node01_01_Chip, sizeof(V_Node01_01_Chip), &c) == 0);
    uint8_t derived[65];
    CHECK(mc_pub_from_priv(priv, derived, test_rng, NULL) == 0 && !memcmp(derived, c.pub, 65));
    priv[3] ^= 1;
    CHECK(mc_pub_from_priv(priv, derived, test_rng, NULL) == 0 && memcmp(derived, c.pub, 65));
    priv[3] ^= 1;
    CHECK(mc_ecdsa_sign(priv,(const uint8_t *)"hello", 5, sig, test_rng, NULL) == 0);
    CHECK(mc_ecdsa_verify(c.pub, (const uint8_t *)"hello", 5, sig) == 0);
    CHECK(mc_ecdsa_verify(c.pub, (const uint8_t *)"hellp", 5, sig) != 0);
}

static size_t der_tbs_off(const uint8_t *d, size_t *len)
{
    size_t o = (d[1] & 0x80) ? 2 + (d[1] & 0x7f) : 2;
    size_t l = d[o + 1];
    size_t h = 2;
    if (l & 0x80) {
        h = 2 + (l & 0x7f);
        l = (l & 0x7f) == 1 ? d[o + 2] : (size_t)(d[o + 2] << 8 | d[o + 3]);
    }
    *len = h + l;
    return o;
}

static void check_cert(const uint8_t *tlv, size_t tn, const uint8_t *der, const uint8_t *issuer_pub)
{
    mc_cert_t c;
    uint8_t tbs[640];
    size_t n, dl;
    CHECK(mc_cert_parse(tlv, tn, &c) == 0);
    CHECK(mc_cert_tbs_der(&c, tbs, sizeof(tbs), &n) == 0);
    size_t o = der_tbs_off(der, &dl);
    CHECK(n == dl && !memcmp(tbs, der + o, dl));
    int vr = mc_cert_verify(&c, issuer_pub ? issuer_pub : c.pub);
    if (vr) printf("verify fail serial0=%02x\n", c.serial.d[0]);
    CHECK(vr == 0);
}

static void test_certs(void)
{
    mc_cert_t root, ica, node, node2;
    CHECK(mc_cert_parse(V_Root01_Chip, sizeof(V_Root01_Chip), &root) == 0);
    CHECK(mc_cert_parse(V_ICA01_Chip, sizeof(V_ICA01_Chip), &ica) == 0);
    CHECK(mc_cert_parse(V_Node01_01_Chip, sizeof(V_Node01_01_Chip), &node) == 0);
    CHECK(mc_cert_parse(V_Node01_02_Chip, sizeof(V_Node01_02_Chip), &node2) == 0);
    check_cert(V_Root01_Chip, sizeof(V_Root01_Chip), V_Root01_DER, NULL);
    check_cert(V_ICA01_Chip, sizeof(V_ICA01_Chip), V_ICA01_DER, root.pub);
    check_cert(V_Node01_01_Chip, sizeof(V_Node01_01_Chip), V_Node01_01_DER, ica.pub);
    check_cert(V_Node01_02_Chip, sizeof(V_Node01_02_Chip), V_Node01_02_DER, root.pub);
    CHECK(node.has_node && node.node_id == 0xDEDEDEDE00010001ULL && node.has_fabric && node.fabric_id == 0xFAB000000000001DULL);
    CHECK(node2.node_id == 0xDEDEDEDE00010002ULL && !node.is_ca && ica.is_ca && root.is_ca);
    CHECK(mc_cert_verify(&node, root.pub) != 0);
    CHECK(mc_cert_verify(&node, node2.pub) != 0);
    uint8_t bad[sizeof(V_Node01_01_Chip)];
    memcpy(bad, V_Node01_01_Chip, sizeof(bad));
    bad[40] ^= 1;
    mc_cert_t b;
    if (mc_cert_parse(bad, sizeof(bad), &b) == 0) CHECK(mc_cert_verify(&b, ica.pub) != 0);
    CHECK(mc_cert_parse(V_Node01_01_Chip, 100, &b) != 0);
    CHECK(mc_cert_parse(V_Node01_01_DER, sizeof(V_Node01_01_DER), &b) != 0);
}

int main(void)
{
    test_tlv();
    test_headers();
    test_crypto();
    test_certs();
    printf("test_matter: %d checks, %d failures\n", checks, failures);
    return failures ? 1 : 0;
}
