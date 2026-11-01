#include "matter.h"
#include <string.h>
#include "mbedtls/ccm.h"
#include "mbedtls/ecdsa.h"
#include "mbedtls/md.h"
#include "mbedtls/sha256.h"

void mc_wipe(void *p, size_t n)
{
    volatile uint8_t *v = p;
    while (n--) *v++ = 0;
}

bool mc_ct_eq(const uint8_t *a, const uint8_t *b, size_t n)
{
    uint8_t d = 0;
    for (size_t i = 0; i < n; i++) d |= a[i] ^ b[i];
    return d == 0;
}

static void put_le(uint8_t *o, uint64_t v, int n)
{
    for (int i = 0; i < n; i++) o[i] = (uint8_t)(v >> (8 * i));
}

static uint64_t get_le(const uint8_t *p, int n)
{
    uint64_t v = 0;
    for (int i = 0; i < n; i++) v |= (uint64_t)p[i] << (8 * i);
    return v;
}

static void tlv_put(tlv_w_t *w, const void *d, size_t n)
{
    if (w->err || w->len + n > w->cap) {
        w->err = true;
        return;
    }
    if (n) memcpy(w->b + w->len, d, n);
    w->len += n;
}

static void tlv_ctl(tlv_w_t *w, int tag, uint8_t type)
{
    uint8_t h[2] = {(uint8_t)((tag >= 0 ? 0x20 : 0x00) | type), (uint8_t)tag};
    tlv_put(w, h, tag >= 0 ? 2 : 1);
}

void tlv_w_init(tlv_w_t *w, uint8_t *b, size_t cap)
{
    w->b = b;
    w->cap = cap;
    w->len = 0;
    w->err = false;
}

void tlv_w_start(tlv_w_t *w, int tag, uint8_t type)
{
    tlv_ctl(w, tag, type == TLV_STRUCT ? 0x15 : type == TLV_ARRAY ? 0x16 : 0x17);
}

void tlv_w_end(tlv_w_t *w)
{
    uint8_t e = 0x18;
    tlv_put(w, &e, 1);
}

void tlv_w_uint(tlv_w_t *w, int tag, uint64_t v)
{
    int sz = v <= 0xFF ? 0 : v <= 0xFFFF ? 1 : v <= 0xFFFFFFFFu ? 2 : 3;
    uint8_t b[8];
    tlv_ctl(w, tag, (uint8_t)(4 + sz));
    put_le(b, v, 1 << sz);
    tlv_put(w, b, (size_t)1 << sz);
}

void tlv_w_sint(tlv_w_t *w, int tag, int64_t v)
{
    int sz = (v >= -128 && v <= 127) ? 0 : (v >= -32768 && v <= 32767) ? 1 : (v >= INT32_MIN && v <= INT32_MAX) ? 2 : 3;
    uint8_t b[8];
    tlv_ctl(w, tag, (uint8_t)sz);
    put_le(b, (uint64_t)v, 1 << sz);
    tlv_put(w, b, (size_t)1 << sz);
}

void tlv_w_bool(tlv_w_t *w, int tag, bool v)
{
    tlv_ctl(w, tag, v ? 9 : 8);
}

void tlv_w_bytes(tlv_w_t *w, int tag, const uint8_t *d, size_t n)
{
    uint8_t b[2];
    int sz = n <= 0xFF ? 0 : 1;
    tlv_ctl(w, tag, (uint8_t)(16 + sz));
    put_le(b, n, 1 << sz);
    tlv_put(w, b, (size_t)1 << sz);
    tlv_put(w, d, n);
}

void tlv_r_init(tlv_r_t *r, const uint8_t *p, size_t n)
{
    r->p = p;
    r->end = p + n;
}

static const int k_tagsz[8] = {0, 1, 2, 4, 2, 4, 6, 8};

static bool tlv_step(const uint8_t **pp, const uint8_t *end, int *depth)
{
    const uint8_t *p = *pp;
    if (p >= end) return false;
    uint8_t ctl = *p++;
    uint8_t type = ctl & 31;
    size_t ts = (size_t)k_tagsz[ctl >> 5];
    if ((size_t)(end - p) < ts) return false;
    p += ts;
    size_t vl = 0;
    if (type <= 7) vl = (size_t)1 << (type & 3);
    else if (type == 10) vl = 4;
    else if (type == 11) vl = 8;
    else if (type >= 12 && type <= 19) {
        size_t ls = (size_t)1 << (type & 3);
        if ((size_t)(end - p) < ls || ls > 2) return false;
        vl = ls + (size_t)get_le(p, (int)ls);
    } else if (type == 24) {
        if (--*depth < 0) return false;
    } else if (type >= 21 && type <= 23) {
        (*depth)++;
    } else if (type != 8 && type != 9 && type != 20) {
        return false;
    }
    if ((size_t)(end - p) < vl) return false;
    *pp = p + vl;
    return true;
}

bool tlv_next(tlv_r_t *r, tlv_e_t *e)
{
    const uint8_t *p = r->p;
    if (!p || p >= r->end) return false;
    uint8_t ctl = *p++;
    uint8_t type = ctl & 31;
    size_t ts = (size_t)k_tagsz[ctl >> 5];
    memset(e, 0, sizeof(*e));
    if ((size_t)(r->end - p) < ts || type == 24) goto fail;
    e->tag = (ctl >> 5) == 1 ? p[0] : (ctl >> 5) == 0 ? -1 : -2;
    p += ts;
    if (type <= 3 || (type >= 4 && type <= 7)) {
        size_t vl = (size_t)1 << (type & 3);
        if ((size_t)(r->end - p) < vl) goto fail;
        uint64_t v = get_le(p, (int)vl);
        p += vl;
        if (type <= 3) {
            e->type = TLV_SINT;
            int sh = 64 - 8 * (int)vl;
            e->s = (int64_t)(v << sh) >> sh;
        } else {
            e->type = TLV_UINT;
            e->u = v;
        }
    } else if (type == 8 || type == 9) {
        e->type = TLV_BOOL;
        e->u = type == 9;
    } else if (type == 10 || type == 11) {
        e->type = TLV_FLOAT;
        size_t vl = type == 10 ? 4 : 8;
        if ((size_t)(r->end - p) < vl) goto fail;
        p += vl;
    } else if (type >= 12 && type <= 19) {
        size_t ls = (size_t)1 << (type & 3);
        if (ls > 2 || (size_t)(r->end - p) < ls) goto fail;
        size_t n = (size_t)get_le(p, (int)ls);
        p += ls;
        if ((size_t)(r->end - p) < n) goto fail;
        e->type = type < 16 ? TLV_UTF8 : TLV_BYTES;
        e->d = p;
        e->n = n;
        p += n;
    } else if (type == 20) {
        e->type = TLV_NULL;
    } else if (type >= 21 && type <= 23) {
        int depth = 1;
        const uint8_t *q = p;
        const uint8_t *last = p;
        while (depth > 0) {
            last = q;
            if (!tlv_step(&q, r->end, &depth)) goto fail;
        }
        e->type = type == 21 ? TLV_STRUCT : type == 22 ? TLV_ARRAY : TLV_LIST;
        e->d = p;
        e->n = (size_t)(last - p);
        p = q;
    } else {
        goto fail;
    }
    r->p = p;
    return true;
fail:
    r->p = NULL;
    return false;
}

bool tlv_find(const uint8_t *p, size_t n, int tag, tlv_e_t *e)
{
    tlv_r_t r;
    tlv_r_init(&r, p, n);
    while (tlv_next(&r, e)) {
        if (e->tag == tag) return true;
    }
    return false;
}

int mc_hdr_encode(const mc_hdr_t *h, uint8_t *out, size_t cap)
{
    size_t n = 8 + (h->has_src ? 8 : 0) + (h->has_dst ? 8 : 0);
    if (cap < n) return -1;
    out[0] = (uint8_t)((h->has_src ? 0x04 : 0) | (h->has_dst ? 0x01 : 0));
    put_le(out + 1, h->sid, 2);
    out[3] = h->sflags;
    put_le(out + 4, h->ctr, 4);
    size_t o = 8;
    if (h->has_src) {
        put_le(out + o, h->src, 8);
        o += 8;
    }
    if (h->has_dst) put_le(out + o, h->dst, 8);
    return (int)n;
}

int mc_hdr_decode(const uint8_t *in, size_t n, mc_hdr_t *h)
{
    memset(h, 0, sizeof(*h));
    if (n < 8) return -1;
    h->flags = in[0];
    if ((in[0] >> 4) != 0 || (in[0] & 0x02)) return -1;
    h->sid = (uint16_t)get_le(in + 1, 2);
    h->sflags = in[3];
    if ((in[3] & 0xA0) || (in[3] & 0x03)) return -1;
    h->ctr = (uint32_t)get_le(in + 4, 4);
    size_t o = 8;
    if (in[0] & 0x04) {
        if (n < o + 8) return -1;
        h->has_src = true;
        h->src = get_le(in + o, 8);
        o += 8;
    }
    uint8_t dsiz = in[0] & 0x03;
    if (dsiz == 1) {
        if (n < o + 8) return -1;
        h->has_dst = true;
        h->dst = get_le(in + o, 8);
        o += 8;
    } else if (dsiz != 0) {
        return -1;
    }
    h->len = o;
    return (int)o;
}

int mc_phdr_encode(const mc_phdr_t *h, uint8_t *out, size_t cap)
{
    size_t n = 6 + ((h->xflags & XF_A) ? 4 : 0);
    if (cap < n) return -1;
    out[0] = h->xflags & (uint8_t)~(XF_V | XF_SX);
    out[1] = h->opcode;
    put_le(out + 2, h->xid, 2);
    put_le(out + 4, h->proto, 2);
    if (h->xflags & XF_A) put_le(out + 6, h->ack, 4);
    return (int)n;
}

int mc_phdr_decode(const uint8_t *in, size_t n, mc_phdr_t *h)
{
    memset(h, 0, sizeof(*h));
    if (n < 6) return -1;
    h->xflags = in[0];
    if (in[0] & (XF_SX | 0xE0)) return -1;
    h->opcode = in[1];
    h->xid = (uint16_t)get_le(in + 2, 2);
    size_t o = 4;
    if (in[0] & XF_V) {
        if (n < o + 4 || get_le(in + o, 2) != 0) return -1;
        o += 2;
    }
    h->proto = (uint16_t)get_le(in + o, 2);
    o += 2;
    if (in[0] & XF_A) {
        if (n < o + 4) return -1;
        h->ack = (uint32_t)get_le(in + o, 4);
        o += 4;
    }
    h->len = o;
    return (int)o;
}

void mc_nonce(uint8_t sflags, uint32_t ctr, uint64_t node, uint8_t out[13])
{
    out[0] = sflags;
    put_le(out + 1, ctr, 4);
    put_le(out + 5, node, 8);
}

static int hmac256(const uint8_t *key, size_t kl, const uint8_t *a, size_t al, const uint8_t *b, size_t bl, const uint8_t *c, size_t cl, uint8_t out[32])
{
    mbedtls_md_context_t m;
    mbedtls_md_init(&m);
    int r = mbedtls_md_setup(&m, mbedtls_md_info_from_type(MBEDTLS_MD_SHA256), 1);
    if (r == 0) r = mbedtls_md_hmac_starts(&m, key, kl);
    if (r == 0 && al) r = mbedtls_md_hmac_update(&m, a, al);
    if (r == 0 && bl) r = mbedtls_md_hmac_update(&m, b, bl);
    if (r == 0 && cl) r = mbedtls_md_hmac_update(&m, c, cl);
    if (r == 0) r = mbedtls_md_hmac_finish(&m, out);
    mbedtls_md_free(&m);
    return r;
}

int mc_hkdf(const uint8_t *salt, size_t slen, const uint8_t *ikm, size_t ilen, const uint8_t *info, size_t il, uint8_t *out, size_t olen)
{
    uint8_t zero[32] = {0};
    uint8_t prk[32], t[32];
    if (olen > 255 * 32) return MC_E_ARG;
    int r = hmac256(slen ? salt : zero, slen ? slen : 32, ikm, ilen, NULL, 0, NULL, 0, prk);
    size_t tl = 0;
    for (uint8_t i = 1; r == 0 && olen > 0; i++) {
        r = hmac256(prk, 32, t, tl, info, il, &i, 1, t);
        tl = 32;
        size_t c = olen < 32 ? olen : 32;
        memcpy(out, t, c);
        out += c;
        olen -= c;
    }
    mc_wipe(prk, sizeof(prk));
    mc_wipe(t, sizeof(t));
    return r ? MC_E_CRYPTO : MC_OK;
}

int mc_compressed_fabric_id(const uint8_t root_pub[MC_PUB_LEN], uint64_t fabric_id, uint8_t out[8])
{
    uint8_t salt[8];
    for (int i = 0; i < 8; i++) salt[i] = (uint8_t)(fabric_id >> (56 - 8 * i));
    return mc_hkdf(salt, 8, root_pub + 1, 64, MC_STR("CompressedFabric"), out, 8);
}

int mc_ipk_operational(const uint8_t epoch[MC_KEY_LEN], const uint8_t cfid[8], uint8_t out[MC_KEY_LEN])
{
    return mc_hkdf(cfid, 8, epoch, MC_KEY_LEN, MC_STR("GroupKey v1.0"), out, MC_KEY_LEN);
}

int mc_dest_id(const uint8_t ipk[MC_KEY_LEN], const uint8_t rnd[MC_RAND_LEN], const uint8_t root_pub[MC_PUB_LEN], uint64_t fabric_id, uint64_t node_id, uint8_t out[MC_HASH_LEN])
{
    uint8_t m[MC_RAND_LEN + MC_PUB_LEN + 16];
    memcpy(m, rnd, MC_RAND_LEN);
    memcpy(m + MC_RAND_LEN, root_pub, MC_PUB_LEN);
    put_le(m + MC_RAND_LEN + MC_PUB_LEN, fabric_id, 8);
    put_le(m + MC_RAND_LEN + MC_PUB_LEN + 8, node_id, 8);
    return hmac256(ipk, MC_KEY_LEN, m, sizeof(m), NULL, 0, NULL, 0, out) ? MC_E_CRYPTO : MC_OK;
}

int mc_ccm_seal(const uint8_t key[MC_KEY_LEN], const uint8_t nonce[13], const uint8_t *aad, size_t alen, uint8_t *buf, size_t n)
{
    mbedtls_ccm_context c;
    mbedtls_ccm_init(&c);
    int r = mbedtls_ccm_setkey(&c, MBEDTLS_CIPHER_ID_AES, key, 128);
    if (r == 0) r = mbedtls_ccm_encrypt_and_tag(&c, n, nonce, 13, aad, alen, buf, buf, buf + n, MC_TAG_LEN);
    mbedtls_ccm_free(&c);
    return r ? MC_E_CRYPTO : MC_OK;
}

int mc_ccm_open(const uint8_t key[MC_KEY_LEN], const uint8_t nonce[13], const uint8_t *aad, size_t alen, uint8_t *buf, size_t n)
{
    if (n < MC_TAG_LEN) return MC_E_CRYPTO;
    mbedtls_ccm_context c;
    mbedtls_ccm_init(&c);
    int r = mbedtls_ccm_setkey(&c, MBEDTLS_CIPHER_ID_AES, key, 128);
    if (r == 0) r = mbedtls_ccm_auth_decrypt(&c, n - MC_TAG_LEN, nonce, 13, aad, alen, buf, buf, buf + n - MC_TAG_LEN, MC_TAG_LEN);
    mbedtls_ccm_free(&c);
    return r ? MC_E_CRYPTO : MC_OK;
}

int mc_ecdsa_sign(const uint8_t priv[32], const uint8_t *msg, size_t n, uint8_t sig[MC_SIG_LEN], mc_rng_fn rng, void *rctx)
{
    mbedtls_ecp_group g;
    mbedtls_mpi d, r, s;
    uint8_t h[32];
    mbedtls_ecp_group_init(&g);
    mbedtls_mpi_init(&d);
    mbedtls_mpi_init(&r);
    mbedtls_mpi_init(&s);
    int rc = mbedtls_ecp_group_load(&g, MBEDTLS_ECP_DP_SECP256R1);
    if (rc == 0) rc = mbedtls_sha256(msg, n, h, 0);
    if (rc == 0) rc = mbedtls_mpi_read_binary(&d, priv, 32);
    if (rc == 0) rc = mbedtls_ecdsa_sign(&g, &r, &s, &d, h, 32, rng, rctx);
    if (rc == 0) rc = mbedtls_mpi_write_binary(&r, sig, 32);
    if (rc == 0) rc = mbedtls_mpi_write_binary(&s, sig + 32, 32);
    mbedtls_ecp_group_free(&g);
    mbedtls_mpi_free(&d);
    mbedtls_mpi_free(&r);
    mbedtls_mpi_free(&s);
    return rc ? MC_E_CRYPTO : MC_OK;
}

int mc_ecdsa_verify(const uint8_t pub[MC_PUB_LEN], const uint8_t *msg, size_t n, const uint8_t sig[MC_SIG_LEN])
{
    mbedtls_ecp_group g;
    mbedtls_ecp_point q;
    mbedtls_mpi r, s;
    uint8_t h[32];
    mbedtls_ecp_group_init(&g);
    mbedtls_ecp_point_init(&q);
    mbedtls_mpi_init(&r);
    mbedtls_mpi_init(&s);
    int rc = mbedtls_ecp_group_load(&g, MBEDTLS_ECP_DP_SECP256R1);
    if (rc == 0) rc = mbedtls_sha256(msg, n, h, 0);
    if (rc == 0) rc = mbedtls_ecp_point_read_binary(&g, &q, pub, MC_PUB_LEN);
    if (rc == 0) rc = mbedtls_ecp_check_pubkey(&g, &q);
    if (rc == 0) rc = mbedtls_mpi_read_binary(&r, sig, 32);
    if (rc == 0) rc = mbedtls_mpi_read_binary(&s, sig + 32, 32);
    if (rc == 0) rc = mbedtls_ecdsa_verify(&g, h, 32, &q, &r, &s);
    mbedtls_ecp_group_free(&g);
    mbedtls_ecp_point_free(&q);
    mbedtls_mpi_free(&r);
    mbedtls_mpi_free(&s);
    return rc ? MC_E_CRYPTO : MC_OK;
}

int mc_cert_parse(const uint8_t *tlv, size_t n, mc_cert_t *c)
{
    memset(c, 0, sizeof(*c));
    tlv_r_t r;
    tlv_e_t top, e;
    tlv_r_init(&r, tlv, n);
    if (!tlv_next(&r, &top) || top.type != TLV_STRUCT || top.tag != -1 || r.p != tlv + n) return MC_E_CERT;
    c->raw = tlv;
    c->raw_len = n;
    bool got[12] = {false};
    tlv_r_init(&r, top.d, top.n);
    while (tlv_next(&r, &e)) {
        if (e.tag < 1 || e.tag > 11) continue;
        got[e.tag] = true;
        switch (e.tag) {
        case 1: c->serial = e; break;
        case 2: c->sigalgo = (uint8_t)e.u; break;
        case 3: c->issuer = e; break;
        case 4: c->not_before = (uint32_t)e.u; break;
        case 5: c->not_after = (uint32_t)e.u; break;
        case 6: c->subject = e; break;
        case 7: c->pubalgo = (uint8_t)e.u; break;
        case 8: c->curve = (uint8_t)e.u; break;
        case 9:
            if (e.n != MC_PUB_LEN || e.d[0] != 4) return MC_E_CERT;
            c->pub = e.d;
            break;
        case 10: c->exts = e; break;
        case 11:
            if (e.n != MC_SIG_LEN) return MC_E_CERT;
            c->sig = e.d;
            break;
        }
    }
    if (!r.p) return MC_E_CERT;
    for (int i = 1; i <= 11; i++) {
        if (!got[i]) return MC_E_CERT;
    }
    if (c->serial.type != TLV_BYTES || c->serial.n == 0 || c->serial.n > 20 || c->sigalgo != 1 || c->pubalgo != 1 || c->curve != 1 || c->issuer.type != TLV_LIST || c->subject.type != TLV_LIST || c->exts.type != TLV_LIST) return MC_E_CERT;
    tlv_r_init(&r, c->subject.d, c->subject.n);
    while (tlv_next(&r, &e)) {
        if (e.tag >= 0 && (e.tag & 0x7F) == 17 && e.type == TLV_UINT) {
            c->has_node = true;
            c->node_id = e.u;
        } else if (e.tag >= 0 && (e.tag & 0x7F) == 21 && e.type == TLV_UINT) {
            c->has_fabric = true;
            c->fabric_id = e.u;
        }
    }
    if (!r.p) return MC_E_CERT;
    tlv_r_init(&r, c->exts.d, c->exts.n);
    while (tlv_next(&r, &e)) {
        if (e.tag == 1 && e.type == TLV_STRUCT) {
            tlv_e_t ca;
            c->is_ca = tlv_find(e.d, e.n, 1, &ca) && ca.type == TLV_BOOL && ca.u;
        }
    }
    return r.p ? MC_OK : MC_E_CERT;
}

typedef struct {
    uint8_t *b;
    size_t cap;
    size_t pos;
    bool err;
} der_t;

static void der_raw(der_t *d, const uint8_t *p, size_t n)
{
    if (n > d->pos) {
        d->err = true;
        return;
    }
    d->pos -= n;
    memcpy(d->b + d->pos, p, n);
}

static void der_b(der_t *d, uint8_t v)
{
    der_raw(d, &v, 1);
}

static size_t der_mark(const der_t *d)
{
    return d->cap - d->pos;
}

static void der_wrap(der_t *d, uint8_t tag, size_t mark)
{
    size_t n = der_mark(d) - mark;
    if (n < 128) {
        der_b(d, (uint8_t)n);
    } else if (n < 256) {
        der_b(d, (uint8_t)n);
        der_b(d, 0x81);
    } else {
        der_b(d, (uint8_t)(n & 255));
        der_b(d, (uint8_t)(n >> 8));
        der_b(d, 0x82);
    }
    der_b(d, tag);
}

static void der_tlv(der_t *d, uint8_t tag, const uint8_t *p, size_t n)
{
    size_t m = der_mark(d);
    der_raw(d, p, n);
    der_wrap(d, tag, m);
}

static const uint8_t OID_SIG[] = {0x2A, 0x86, 0x48, 0xCE, 0x3D, 0x04, 0x03, 0x02};
static const uint8_t OID_EC[] = {0x2A, 0x86, 0x48, 0xCE, 0x3D, 0x02, 0x01};
static const uint8_t OID_P256[] = {0x2A, 0x86, 0x48, 0xCE, 0x3D, 0x03, 0x01, 0x07};
static const uint8_t OID_EXT[6][3] = {{0, 0, 0}, {0x55, 0x1D, 0x13}, {0x55, 0x1D, 0x0F}, {0x55, 0x1D, 0x25}, {0x55, 0x1D, 0x0E}, {0x55, 0x1D, 0x23}};
static const uint8_t OID_DC[] = {0x09, 0x92, 0x26, 0x89, 0x93, 0xF2, 0x2C, 0x64, 0x01, 0x19};
static const uint8_t OID_PURPOSE[] = {0x2B, 0x06, 0x01, 0x05, 0x05, 0x07, 0x03, 0};
static const uint8_t PURPOSE_LAST[7] = {0, 1, 2, 3, 4, 8, 9};

static void der_oid(der_t *d, const uint8_t *o, size_t n)
{
    der_tlv(d, 0x06, o, n);
}

static bool dn_oid(uint8_t id, uint8_t *o, size_t *n)
{
    static const uint8_t at[] = {0, 3, 4, 5, 6, 7, 8, 10, 11, 12, 41, 42, 43, 44, 46, 65};
    if (id >= 1 && id <= 15) {
        o[0] = 0x55;
        o[1] = 0x04;
        o[2] = at[id];
        *n = 3;
    } else if (id == 16) {
        memcpy(o, OID_DC, sizeof(OID_DC));
        *n = sizeof(OID_DC);
    } else if (id >= 17 && id <= 22) {
        static const uint8_t m[] = {0x2B, 0x06, 0x01, 0x04, 0x01, 0x82, 0xA2, 0x7C, 0x01};
        memcpy(o, m, sizeof(m));
        o[sizeof(m)] = (uint8_t)(id - 16);
        *n = sizeof(m) + 1;
    } else {
        return false;
    }
    return true;
}

static void der_hex(uint8_t *o, uint64_t v, int digits)
{
    for (int i = 0; i < digits; i++) o[i] = (uint8_t)"0123456789ABCDEF"[(v >> (4 * (digits - 1 - i))) & 15];
}

static void der_dn(der_t *d, const tlv_e_t *list)
{
    tlv_e_t el[16];
    int n = 0;
    tlv_r_t r;
    tlv_r_init(&r, list->d, list->n);
    while (n < 16 && tlv_next(&r, &el[n])) n++;
    if (!r.p || r.p != r.end) {
        d->err = true;
        return;
    }
    size_t m0 = der_mark(d);
    for (int i = n - 1; i >= 0; i--) {
        const tlv_e_t *e = &el[i];
        uint8_t id = (uint8_t)(e->tag & 0x7F);
        uint8_t oid[12], str[16];
        size_t ol;
        if (e->tag < 0 || !dn_oid(id, oid, &ol)) {
            d->err = true;
            return;
        }
        size_t ms = der_mark(d);
        if (id >= 17 && id <= 21) {
            if (e->type != TLV_UINT) d->err = true;
            der_hex(str, e->u, 16);
            der_tlv(d, 0x0C, str, 16);
        } else if (id == 22) {
            if (e->type != TLV_UINT) d->err = true;
            der_hex(str, e->u, 8);
            der_tlv(d, 0x0C, str, 8);
        } else {
            if (e->type != TLV_UTF8) d->err = true;
            der_tlv(d, id == 16 ? 0x16 : (e->tag & 0x80) ? 0x13 : 0x0C, e->d, e->n);
        }
        der_oid(d, oid, ol);
        der_wrap(d, 0x30, ms);
        der_wrap(d, 0x31, ms);
    }
    der_wrap(d, 0x30, m0);
}

static void der_time(der_t *d, uint32_t secs)
{
    char s[16];
    int y = 9999, mo = 12, dd = 31, hh = 23, mi = 59, ss = 59;
    if (secs) {
        int64_t z = (int64_t)(secs / 86400) + 10957 + 719468;
        int64_t era = z / 146097;
        int64_t doe = z - era * 146097;
        int64_t yoe = (doe - doe / 1460 + doe / 36524 - doe / 146096) / 365;
        int64_t doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
        int64_t mp = (5 * doy + 2) / 153;
        dd = (int)(doy - (153 * mp + 2) / 5 + 1);
        mo = (int)(mp < 10 ? mp + 3 : mp - 9);
        y = (int)(yoe + era * 400 + (mo <= 2));
        uint32_t t = secs % 86400;
        hh = (int)(t / 3600);
        mi = (int)(t % 3600 / 60);
        ss = (int)(t % 60);
    }
    int vals[6] = {y, mo, dd, hh, mi, ss};
    size_t o = 0;
    bool gen = y >= 2050;
    for (int i = 0; i < 6; i++) {
        int w = (i == 0 && gen) ? 4 : 2;
        int v = (i == 0 && !gen) ? y % 100 : vals[i];
        for (int k = w - 1; k >= 0; k--) {
            int p = 1;
            for (int j = 0; j < k; j++) p *= 10;
            s[o++] = (char)('0' + (v / p) % 10);
        }
    }
    s[o++] = 'Z';
    der_tlv(d, gen ? 0x18 : 0x17, (const uint8_t *)s, o);
}

static void der_ext(der_t *d, const tlv_e_t *e)
{
    size_t m0 = der_mark(d);
    size_t m1;
    if (e->tag < 1 || e->tag > 5) {
        d->err = true;
        return;
    }
    m1 = der_mark(d);
    if (e->tag == 1) {
        tlv_e_t ca, pl;
        bool has_pl = e->type == TLV_STRUCT && tlv_find(e->d, e->n, 2, &pl) && pl.type == TLV_UINT;
        bool is_ca = e->type == TLV_STRUCT && tlv_find(e->d, e->n, 1, &ca) && ca.type == TLV_BOOL && ca.u;
        size_t ms = der_mark(d);
        if (has_pl) {
            uint8_t v[2] = {0, (uint8_t)pl.u};
            if (pl.u > 127) d->err = true;
            der_tlv(d, 0x02, v + 1, 1);
        }
        if (is_ca) {
            uint8_t t = 0xFF;
            der_tlv(d, 0x01, &t, 1);
        }
        der_wrap(d, 0x30, ms);
    } else if (e->tag == 2) {
        uint16_t v = (uint16_t)e->u;
        uint8_t b[3];
        size_t n = 0;
        uint8_t rev[2];
        for (int i = 0; i < 2; i++) {
            uint8_t x = (uint8_t)(v >> (8 * i)), y = 0;
            for (int k = 0; k < 8; k++) y |= (uint8_t)(((x >> k) & 1) << (7 - k));
            rev[i] = y;
        }
        n = (v >> 8) ? 2 : 1;
        uint8_t unused = 0;
        if (v) {
            while (!((rev[n - 1] >> unused) & 1)) unused++;
        }
        b[0] = unused;
        memcpy(b + 1, rev, n);
        if (e->type != TLV_UINT) d->err = true;
        der_tlv(d, 0x03, b, n + 1);
    } else if (e->tag == 3) {
        tlv_e_t el[8];
        int n = 0;
        tlv_r_t r;
        tlv_r_init(&r, e->d, e->n);
        while (n < 8 && tlv_next(&r, &el[n])) n++;
        size_t ms = der_mark(d);
        for (int i = n - 1; i >= 0; i--) {
            uint8_t o[8];
            memcpy(o, OID_PURPOSE, 8);
            if (el[i].u < 1 || el[i].u > 6) d->err = true;
            else o[7] = PURPOSE_LAST[el[i].u];
            der_oid(d, o, 8);
        }
        der_wrap(d, 0x30, ms);
    } else if (e->tag == 4) {
        der_tlv(d, 0x04, e->d, e->n);
    } else {
        size_t ms = der_mark(d);
        der_tlv(d, 0x80, e->d, e->n);
        der_wrap(d, 0x30, ms);
    }
    der_wrap(d, 0x04, m1);
    if (e->tag <= 3) {
        uint8_t t = 0xFF;
        der_tlv(d, 0x01, &t, 1);
    }
    der_oid(d, OID_EXT[e->tag], 3);
    der_wrap(d, 0x30, m0);
}

int mc_cert_tbs_der(const mc_cert_t *c, uint8_t *out, size_t cap, size_t *len)
{
    der_t d = {out, cap, cap, false};
    size_t m0 = der_mark(&d);
    tlv_e_t el[8];
    int n = 0;
    tlv_r_t r;
    tlv_r_init(&r, c->exts.d, c->exts.n);
    while (n < 8 && tlv_next(&r, &el[n])) n++;
    if (!r.p || r.p != r.end) return MC_E_CERT;
    size_t me = der_mark(&d);
    for (int i = n - 1; i >= 0; i--) der_ext(&d, &el[i]);
    der_wrap(&d, 0x30, me);
    der_wrap(&d, 0xA3, me);
    size_t ms = der_mark(&d);
    size_t mk = der_mark(&d);
    der_raw(&d, c->pub, MC_PUB_LEN);
    der_b(&d, 0);
    der_wrap(&d, 0x03, mk);
    size_t ma = der_mark(&d);
    der_oid(&d, OID_P256, sizeof(OID_P256));
    der_oid(&d, OID_EC, sizeof(OID_EC));
    der_wrap(&d, 0x30, ma);
    der_wrap(&d, 0x30, ms);
    der_dn(&d, &c->subject);
    size_t mv = der_mark(&d);
    der_time(&d, c->not_after);
    der_time(&d, c->not_before);
    der_wrap(&d, 0x30, mv);
    der_dn(&d, &c->issuer);
    size_t mg = der_mark(&d);
    der_oid(&d, OID_SIG, sizeof(OID_SIG));
    der_wrap(&d, 0x30, mg);
    der_tlv(&d, 0x02, c->serial.d, c->serial.n);
    static const uint8_t ver[] = {0xA0, 0x03, 0x02, 0x01, 0x02};
    der_raw(&d, ver, sizeof(ver));
    der_wrap(&d, 0x30, m0);
    if (d.err) return MC_E_CERT;
    *len = cap - d.pos;
    memmove(out, out + d.pos, *len);
    return MC_OK;
}

int mc_cert_verify(const mc_cert_t *c, const uint8_t issuer_pub[MC_PUB_LEN])
{
    uint8_t tbs[640];
    size_t n;
    int r = mc_cert_tbs_der(c, tbs, sizeof(tbs), &n);
    if (r) return r;
    return mc_ecdsa_verify(issuer_pub, tbs, n, c->sig) ? MC_E_CERT : MC_OK;
}
