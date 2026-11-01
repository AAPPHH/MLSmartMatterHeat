#include "matter.h"
#include <stdio.h>
#include <string.h>
#include "mbedtls/ecdh.h"
#include "mbedtls/sha256.h"

#define SC_PROTO 0x0000
#define IM_PROTO 0x0001
#define OP_ACK 0x10
#define OP_SIGMA1 0x30
#define OP_SIGMA2 0x31
#define OP_SIGMA3 0x32
#define OP_STATUS 0x40
#define OP_WRITE_REQ 0x06
#define OP_WRITE_RSP 0x07
#define OP_STATUS_RSP 0x01
#define IM_REVISION 11
#define SEEN_N 8

typedef struct {
    const mc_io_t *io;
    const wd_creds_t *cr;
    const mc_mrp_t *mrp;
    char *err;
    size_t err_len;
    uint8_t tx[MC_MTU];
    uint8_t rx[MC_MTU];
    uint8_t pl[MC_MTU];
    uint8_t wk[MC_MTU];
    uint8_t wk2[MC_MTU];
    size_t txlen;
    bool secured;
    bool xi;
    bool acked;
    bool pend_ack;
    uint16_t lsid;
    uint16_t psid;
    uint16_t xid;
    uint8_t enc_key[MC_KEY_LEN];
    uint8_t dec_key[MC_KEY_LEN];
    uint8_t ipk[MC_KEY_LEN];
    uint8_t eph_d[32];
    uint8_t eph_pub[MC_PUB_LEN];
    uint32_t ctr;
    uint32_t last_ctr;
    uint32_t pend_ctr;
    uint32_t seen[SEEN_N];
    uint8_t nseen;
    uint64_t local_node;
    uint64_t peer_node;
    uint64_t eph_node;
    uint32_t base_ms;
    uint32_t active_ms;
    mc_phdr_t rph;
    const uint8_t *rpl;
    size_t rpl_len;
} mc_client_t;

static mc_client_t s_c;

static int rng_cb(void *p, unsigned char *b, size_t n)
{
    const mc_io_t *io = ((mc_client_t *)p)->io;
    io->rng(io->ctx, b, n);
    return 0;
}

static uint32_t rnd32(mc_client_t *c)
{
    uint32_t v;
    rng_cb(c, (uint8_t *)&v, 4);
    return v;
}

static int fail(mc_client_t *c, int rc, const char *fmt, unsigned a)
{
    if (c->err && c->err_len && !c->err[0]) snprintf(c->err, c->err_len, fmt, a);
    return rc;
}

static uint32_t clampu(uint32_t v, uint32_t lo, uint32_t hi)
{
    return v < lo ? lo : v > hi ? hi : v;
}

void mc_mrp_from_txt(mc_mrp_t *m, uint32_t sii_ms, uint32_t sai_ms)
{
    m->idle_ms = sii_ms ? clampu(sii_ms, 2000, 60000) : 3000;
    m->active_ms = sai_ms ? clampu(sai_ms, 1000, 10000) : 1500;
    m->max_ms = 60000;
    m->max_retx = 5;
    m->resp_ms = 20000;
}

uint32_t mc_backoff_ms(uint32_t base_ms, uint8_t send_count, uint32_t max_ms, uint32_t rnd)
{
    uint64_t i = (uint64_t)base_ms * 1127 / 1024;
    for (int k = 1; k < send_count; k++) i = i * 16 / 10;
    i = i * (1024 + (rnd & 255)) / 1024;
    return i > max_ms ? max_ms : (uint32_t)i;
}

static int send_raw(mc_client_t *c)
{
    return c->io->send(c->io->ctx, c->tx, c->txlen) < 0 ? MC_E_PROTO : MC_OK;
}

static int send_msg(mc_client_t *c, uint16_t proto, uint8_t op, const uint8_t *payload, size_t len, bool reliable)
{
    mc_phdr_t ph = {0};
    ph.xflags = (uint8_t)((c->xi ? XF_I : 0) | (reliable ? XF_R : 0) | (c->pend_ack ? XF_A : 0));
    ph.opcode = op;
    ph.xid = c->xid;
    ph.proto = proto;
    ph.ack = c->pend_ctr;
    int pn = mc_phdr_encode(&ph, c->pl, sizeof(c->pl));
    if (pn < 0 || len + (size_t)pn + 64 > MC_MTU) return MC_E_ARG;
    if (len) memcpy(c->pl + pn, payload, len);
    mc_hdr_t h = {0};
    h.sid = c->secured ? c->psid : 0;
    h.ctr = c->ctr;
    h.has_src = !c->secured;
    h.src = c->eph_node;
    int hl = mc_hdr_encode(&h, c->tx, sizeof(c->tx));
    size_t n = (size_t)pn + len;
    memcpy(c->tx + hl, c->pl, n);
    if (c->secured) {
        uint8_t nonce[13];
        mc_nonce(h.sflags, h.ctr, c->local_node, nonce);
        if (mc_ccm_seal(c->enc_key, nonce, c->tx, (size_t)hl, c->tx + hl, n)) return MC_E_CRYPTO;
        n += MC_TAG_LEN;
    }
    c->txlen = (size_t)hl + n;
    c->last_ctr = c->ctr++;
    if (c->pend_ack) c->pend_ack = false;
    return send_raw(c);
}

static bool seen_before(mc_client_t *c, uint32_t ctr)
{
    for (uint8_t i = 0; i < c->nseen && i < SEEN_N; i++) {
        if (c->seen[i] == ctr) return true;
    }
    c->seen[c->nseen++ % SEEN_N] = ctr;
    return false;
}

static int recv_one(mc_client_t *c, uint32_t timeout_ms)
{
    uint32_t t0 = c->io->now_ms(c->io->ctx);
    for (;;) {
        uint32_t el = c->io->now_ms(c->io->ctx) - t0;
        if (el >= timeout_ms) return 0;
        int n = c->io->recv(c->io->ctx, c->rx, sizeof(c->rx), timeout_ms - el);
        if (n < 0) return MC_E_PROTO;
        if (n == 0) return 0;
        mc_hdr_t h;
        int hl = mc_hdr_decode(c->rx, (size_t)n, &h);
        if (hl < 0) continue;
        size_t pn = (size_t)n - (size_t)hl;
        if (c->secured) {
            uint8_t nonce[13];
            if (h.sid != c->lsid) continue;
            mc_nonce(h.sflags, h.ctr, h.has_src ? h.src : c->peer_node, nonce);
            if (mc_ccm_open(c->dec_key, nonce, c->rx, (size_t)hl, c->rx + hl, pn)) continue;
            pn -= MC_TAG_LEN;
        } else {
            if (h.sid != 0 || (h.has_dst && h.dst != c->eph_node)) continue;
        }
        mc_phdr_t ph;
        int phl = mc_phdr_decode(c->rx + hl, pn, &ph);
        if (phl < 0 || ph.xid != c->xid || ((ph.xflags & XF_I) != 0) == c->xi) continue;
        if (seen_before(c, h.ctr)) {
            if (ph.xflags & XF_R) {
                uint32_t sc = c->last_ctr, pc = c->pend_ctr;
                size_t sl = c->txlen;
                bool pa = c->pend_ack;
                uint8_t keep[MC_MTU];
                memcpy(keep, c->tx, sl);
                c->pend_ack = true;
                c->pend_ctr = h.ctr;
                send_msg(c, SC_PROTO, OP_ACK, NULL, 0, false);
                memcpy(c->tx, keep, sl);
                c->txlen = sl;
                c->last_ctr = sc;
                c->pend_ack = pa;
                c->pend_ctr = pc;
            }
            continue;
        }
        c->base_ms = c->active_ms;
        if ((ph.xflags & XF_A) && ph.ack == c->last_ctr) c->acked = true;
        if (ph.xflags & XF_R) {
            c->pend_ack = true;
            c->pend_ctr = h.ctr;
        }
        c->rph = ph;
        c->rpl = c->rx + hl + phl;
        c->rpl_len = pn - (size_t)phl;
        if (ph.proto == SC_PROTO && ph.opcode == OP_ACK) return 2;
        return 1;
    }
}

static void new_exchange(mc_client_t *c, bool initiator)
{
    c->xid = (uint16_t)(rnd32(c) | 1);
    c->xi = initiator;
    c->nseen = 0;
    c->pend_ack = false;
    c->acked = false;
}

static int xchg(mc_client_t *c, uint16_t proto, uint8_t op, const uint8_t *payload, size_t len, const char *stage)
{
    c->acked = false;
    int rc = send_msg(c, proto, op, payload, len, true);
    if (rc) return fail(c, rc, "send_failed", 0);
    uint8_t k = 0;
    uint32_t next = c->io->now_ms(c->io->ctx) + mc_backoff_ms(c->base_ms, 0, c->mrp->max_ms, rnd32(c));
    uint32_t resp = 0;
    for (;;) {
        uint32_t now = c->io->now_ms(c->io->ctx);
        if (c->acked && !resp) resp = now + c->mrp->resp_ms;
        uint32_t dl = c->acked ? resp : next;
        if ((int32_t)(now - dl) >= 0) {
            if (c->acked || k >= c->mrp->max_retx) {
                if (c->err && c->err_len && !c->err[0]) snprintf(c->err, c->err_len, "timeout_%s", stage);
                return MC_E_TIMEOUT;
            }
            k++;
            if (send_raw(c)) return MC_E_PROTO;
            next = now + mc_backoff_ms(c->base_ms, k, c->mrp->max_ms, rnd32(c));
            continue;
        }
        int r = recv_one(c, dl - now);
        if (r < 0) return r;
        if (r == 1) return MC_OK;
    }
}

static int status_report(mc_client_t *c, const char *stage)
{
    if (c->rph.opcode != OP_STATUS || c->rpl_len < 8) return fail(c, MC_E_PROTO, "proto_status", 0);
    unsigned gen = (unsigned)(c->rpl[0] | c->rpl[1] << 8);
    unsigned code = (unsigned)(c->rpl[6] | c->rpl[7] << 8);
    if (gen == 0 && code == 0) return MC_OK;
    if (c->err && c->err_len && !c->err[0]) {
        if (c->rph.proto == SC_PROTO && code == 1) snprintf(c->err, c->err_len, "case_nosharedroot");
        else if (c->rph.proto == SC_PROTO && code == 2) snprintf(c->err, c->err_len, "case_invalidparam");
        else if (c->rph.proto == SC_PROTO && code == 4) snprintf(c->err, c->err_len, "case_busy");
        else snprintf(c->err, c->err_len, "%s_st%u_%u", stage, gen & 0xFF, code & 0xFFFF);
    }
    return MC_E_PEER;
}

static void hash_clone_final(const mbedtls_sha256_context *h, uint8_t out[32])
{
    mbedtls_sha256_context t;
    mbedtls_sha256_init(&t);
    mbedtls_sha256_clone(&t, h);
    mbedtls_sha256_finish(&t, out);
    mbedtls_sha256_free(&t);
}

static bool bytes_len(const tlv_e_t *e, size_t n)
{
    return e->type == TLV_BYTES && e->n == n;
}

static int check_chain(mc_client_t *c, const mc_cert_t *rcac, const uint8_t *icac, size_t icac_len, const uint8_t *noc, size_t noc_len, mc_cert_t *noc_out, bool is_peer)
{
    mc_cert_t ic, nc;
    const uint8_t *issuer = rcac->pub;
    if (mc_cert_parse(noc, noc_len, &nc)) return fail(c, MC_E_CERT, "cert_noc_parse", 0);
    if (icac_len) {
        if (mc_cert_parse(icac, icac_len, &ic) || !ic.is_ca) return fail(c, MC_E_CERT, "cert_icac_parse", 0);
        if (mc_cert_verify(&ic, rcac->pub)) return fail(c, MC_E_CERT, "cert_icac_sig", 0);
        if (ic.has_fabric && ic.fabric_id != c->cr->fabric_id) return fail(c, MC_E_CERT, "cert_icac_fabric", 0);
        issuer = ic.pub;
    }
    if (nc.is_ca || mc_cert_verify(&nc, issuer)) return fail(c, MC_E_CERT, "cert_noc_sig", 0);
    if (!nc.has_fabric || nc.fabric_id != c->cr->fabric_id) return fail(c, MC_E_CERT, "cert_fabric", 0);
    if (!nc.has_node || nc.node_id != (is_peer ? c->peer_node : c->cr->node_id)) return fail(c, MC_E_CERT, "cert_node", 0);
    *noc_out = nc;
    return MC_OK;
}

static void put_sigma_certs(tlv_w_t *w, const wd_creds_t *cr)
{
    tlv_w_bytes(w, 1, cr->noc, cr->noc_len);
    if (cr->icac_len) tlv_w_bytes(w, 2, cr->icac, cr->icac_len);
}

static int case_connect(mc_client_t *c, const mc_cert_t *rcac)
{
    const wd_creds_t *cr = c->cr;
    mbedtls_ecp_group g;
    mbedtls_mpi d, z;
    mbedtls_ecp_point q, pp;
    mbedtls_sha256_context hash;
    uint8_t shared[32], i_rand[MC_RAND_LEN], dest[MC_HASH_LEN], resp_pub[MC_PUB_LEN], salt[16 + 32 + MC_PUB_LEN + 32], k[MC_KEY_LEN], digest[32], keys[48];
    static const uint8_t n2[13] = {'N', 'C', 'A', 'S', 'E', '_', 'S', 'i', 'g', 'm', 'a', '2', 'N'};
    static const uint8_t n3[13] = {'N', 'C', 'A', 'S', 'E', '_', 'S', 'i', 'g', 'm', 'a', '3', 'N'};
    int rc = MC_E_CRYPTO, r;
    mbedtls_ecp_group_init(&g);
    mbedtls_mpi_init(&d);
    mbedtls_mpi_init(&z);
    mbedtls_ecp_point_init(&q);
    mbedtls_ecp_point_init(&pp);
    mbedtls_sha256_init(&hash);
    size_t ol;
    if (mbedtls_ecp_group_load(&g, MBEDTLS_ECP_DP_SECP256R1) || mbedtls_ecp_gen_keypair(&g, &d, &q, rng_cb, c) || mbedtls_ecp_point_write_binary(&g, &q, MBEDTLS_ECP_PF_UNCOMPRESSED, &ol, c->eph_pub, MC_PUB_LEN) || mbedtls_mpi_write_binary(&d, c->eph_d, 32)) {
        fail(c, rc, "crypto_ephkey", 0);
        goto done;
    }
    rng_cb(c, i_rand, sizeof(i_rand));
    do {
        c->lsid = (uint16_t)rnd32(c);
    } while (c->lsid == 0);
    if (mc_dest_id(c->ipk, i_rand, rcac->pub, cr->fabric_id, c->peer_node, dest)) {
        fail(c, rc, "crypto_destid", 0);
        goto done;
    }
    tlv_w_t w;
    tlv_w_init(&w, c->wk, sizeof(c->wk));
    tlv_w_start(&w, -1, TLV_STRUCT);
    tlv_w_bytes(&w, 1, i_rand, sizeof(i_rand));
    tlv_w_uint(&w, 2, c->lsid);
    tlv_w_bytes(&w, 3, dest, sizeof(dest));
    tlv_w_bytes(&w, 4, c->eph_pub, MC_PUB_LEN);
    tlv_w_end(&w);
    mbedtls_sha256_starts(&hash, 0);
    mbedtls_sha256_update(&hash, c->wk, w.len);
    c->secured = false;
    c->eph_node = ((uint64_t)rnd32(c) << 32) | rnd32(c);
    c->ctr = (rnd32(c) & 0x0FFFFFFF) + 1;
    new_exchange(c, true);
    if ((r = xchg(c, SC_PROTO, OP_SIGMA1, c->wk, w.len, "sigma1"))) {
        rc = r;
        goto done;
    }
    if (c->rph.proto != SC_PROTO) {
        rc = fail(c, MC_E_PROTO, "proto_sigma2", 0);
        goto done;
    }
    if (c->rph.opcode == OP_STATUS) {
        rc = status_report(c, "sigma1");
        goto done;
    }
    tlv_e_t top, e_rand, e_sid, e_pub, e_enc, e_par;
    tlv_r_t rd;
    tlv_r_init(&rd, c->rpl, c->rpl_len);
    if (c->rph.opcode != OP_SIGMA2 || !tlv_next(&rd, &top) || top.type != TLV_STRUCT || !tlv_find(top.d, top.n, 1, &e_rand) || !bytes_len(&e_rand, MC_RAND_LEN) || !tlv_find(top.d, top.n, 2, &e_sid) || e_sid.type != TLV_UINT || e_sid.u > 0xFFFF || !tlv_find(top.d, top.n, 3, &e_pub) || !bytes_len(&e_pub, MC_PUB_LEN) || !tlv_find(top.d, top.n, 4, &e_enc) || e_enc.type != TLV_BYTES || e_enc.n <= MC_TAG_LEN || e_enc.n > sizeof(c->wk)) {
        rc = fail(c, MC_E_PROTO, "proto_sigma2", 0);
        goto done;
    }
    c->psid = (uint16_t)e_sid.u;
    memcpy(resp_pub, e_pub.d, MC_PUB_LEN);
    if (tlv_find(top.d, top.n, 5, &e_par) && e_par.type == TLV_STRUCT) {
        tlv_e_t sai;
        if (tlv_find(e_par.d, e_par.n, 2, &sai) && sai.type == TLV_UINT && sai.u) c->active_ms = clampu((uint32_t)sai.u, c->active_ms, 10000);
    }
    memcpy(salt, c->ipk, 16);
    memcpy(salt + 16, e_rand.d, 32);
    memcpy(salt + 48, resp_pub, MC_PUB_LEN);
    hash_clone_final(&hash, salt + 48 + MC_PUB_LEN);
    mbedtls_sha256_update(&hash, c->rpl, c->rpl_len);
    if (mbedtls_ecp_point_read_binary(&g, &pp, resp_pub, MC_PUB_LEN) || mbedtls_ecp_check_pubkey(&g, &pp) || mbedtls_ecdh_compute_shared(&g, &z, &pp, &d, rng_cb, c) || mbedtls_mpi_write_binary(&z, shared, 32)) {
        rc = fail(c, MC_E_CRYPTO, "crypto_ecdh", 0);
        goto done;
    }
    memcpy(c->wk, e_enc.d, e_enc.n);
    size_t en = e_enc.n;
    if (mc_hkdf(salt, sizeof(salt), shared, 32, MC_STR("Sigma2"), k, MC_KEY_LEN) || mc_ccm_open(k, n2, NULL, 0, c->wk, en)) {
        rc = fail(c, MC_E_CRYPTO, "crypto_sigma2", 0);
        goto done;
    }
    tlv_e_t tb, t_noc, t_ic, t_sig;
    tlv_r_init(&rd, c->wk, en - MC_TAG_LEN);
    bool has_ic;
    if (!tlv_next(&rd, &tb) || tb.type != TLV_STRUCT || !tlv_find(tb.d, tb.n, 1, &t_noc) || t_noc.type != TLV_BYTES || !tlv_find(tb.d, tb.n, 3, &t_sig) || !bytes_len(&t_sig, MC_SIG_LEN)) {
        rc = fail(c, MC_E_PROTO, "proto_sigma2_tbe", 0);
        goto done;
    }
    has_ic = tlv_find(tb.d, tb.n, 2, &t_ic) && t_ic.type == TLV_BYTES;
    mc_cert_t pn;
    if ((r = check_chain(c, rcac, has_ic ? t_ic.d : NULL, has_ic ? t_ic.n : 0, t_noc.d, t_noc.n, &pn, true))) {
        rc = r;
        goto done;
    }
    tlv_w_init(&w, c->wk2, sizeof(c->wk2));
    tlv_w_start(&w, -1, TLV_STRUCT);
    tlv_w_bytes(&w, 1, t_noc.d, t_noc.n);
    if (has_ic) tlv_w_bytes(&w, 2, t_ic.d, t_ic.n);
    tlv_w_bytes(&w, 3, resp_pub, MC_PUB_LEN);
    tlv_w_bytes(&w, 4, c->eph_pub, MC_PUB_LEN);
    tlv_w_end(&w);
    if (w.err || mc_ecdsa_verify(pn.pub, c->wk2, w.len, t_sig.d)) {
        rc = fail(c, MC_E_CERT, "sig_sigma2", 0);
        goto done;
    }
    tlv_w_init(&w, c->wk2, sizeof(c->wk2));
    tlv_w_start(&w, -1, TLV_STRUCT);
    put_sigma_certs(&w, cr);
    tlv_w_bytes(&w, 3, c->eph_pub, MC_PUB_LEN);
    tlv_w_bytes(&w, 4, resp_pub, MC_PUB_LEN);
    tlv_w_end(&w);
    uint8_t sig[MC_SIG_LEN];
    if (w.err || mc_ecdsa_sign(cr->key + MC_PUB_LEN, c->wk2, w.len, sig, rng_cb, c)) {
        rc = fail(c, MC_E_CRYPTO, "crypto_sign", 0);
        goto done;
    }
    tlv_w_init(&w, c->wk, sizeof(c->wk) - MC_TAG_LEN);
    tlv_w_start(&w, -1, TLV_STRUCT);
    put_sigma_certs(&w, cr);
    tlv_w_bytes(&w, 3, sig, sizeof(sig));
    tlv_w_end(&w);
    if (w.err) {
        rc = fail(c, MC_E_ARG, "arg_sigma3_size", 0);
        goto done;
    }
    memcpy(salt, c->ipk, 16);
    hash_clone_final(&hash, salt + 16);
    if (mc_hkdf(salt, 48, shared, 32, MC_STR("Sigma3"), k, MC_KEY_LEN) || mc_ccm_seal(k, n3, NULL, 0, c->wk, w.len)) {
        rc = fail(c, MC_E_CRYPTO, "crypto_sigma3", 0);
        goto done;
    }
    size_t tbe = w.len + MC_TAG_LEN;
    tlv_w_init(&w, c->wk2, sizeof(c->wk2));
    tlv_w_start(&w, -1, TLV_STRUCT);
    tlv_w_bytes(&w, 1, c->wk, tbe);
    tlv_w_end(&w);
    if (w.err) {
        rc = fail(c, MC_E_ARG, "arg_sigma3_size", 0);
        goto done;
    }
    mbedtls_sha256_update(&hash, c->wk2, w.len);
    if ((r = xchg(c, SC_PROTO, OP_SIGMA3, c->wk2, w.len, "sigma3"))) {
        rc = r;
        goto done;
    }
    if ((r = status_report(c, "sigma3"))) {
        rc = r;
        goto done;
    }
    if (c->pend_ack) {
        send_msg(c, SC_PROTO, OP_ACK, NULL, 0, false);
        c->pend_ack = false;
    }
    memcpy(salt, c->ipk, 16);
    hash_clone_final(&hash, digest);
    memcpy(salt + 16, digest, 32);
    if (mc_hkdf(salt, 48, shared, 32, MC_STR("SessionKeys"), keys, sizeof(keys))) {
        rc = fail(c, MC_E_CRYPTO, "crypto_keys", 0);
        goto done;
    }
    memcpy(c->enc_key, keys, MC_KEY_LEN);
    memcpy(c->dec_key, keys + MC_KEY_LEN, MC_KEY_LEN);
    c->secured = true;
    c->ctr = (rnd32(c) & 0x0FFFFFFF) + 1;
    rc = MC_OK;
done:
    mc_wipe(shared, sizeof(shared));
    mc_wipe(k, sizeof(k));
    mc_wipe(keys, sizeof(keys));
    mc_wipe(salt, sizeof(salt));
    mc_wipe(c->eph_d, sizeof(c->eph_d));
    mbedtls_ecp_group_free(&g);
    mbedtls_mpi_free(&d);
    mbedtls_mpi_free(&z);
    mbedtls_ecp_point_free(&q);
    mbedtls_ecp_point_free(&pp);
    mbedtls_sha256_free(&hash);
    return rc;
}

static int write_attr(mc_client_t *c, uint8_t ep, uint32_t cluster, uint32_t attr, int16_t val)
{
    tlv_w_t w;
    tlv_w_init(&w, c->wk, sizeof(c->wk));
    tlv_w_start(&w, -1, TLV_STRUCT);
    tlv_w_bool(&w, 0, false);
    tlv_w_bool(&w, 1, false);
    tlv_w_start(&w, 2, TLV_ARRAY);
    tlv_w_start(&w, -1, TLV_STRUCT);
    tlv_w_start(&w, 1, TLV_LIST);
    tlv_w_uint(&w, 2, ep);
    tlv_w_uint(&w, 3, cluster);
    tlv_w_uint(&w, 4, attr);
    tlv_w_end(&w);
    tlv_w_sint(&w, 2, val);
    tlv_w_end(&w);
    tlv_w_end(&w);
    tlv_w_uint(&w, 255, IM_REVISION);
    tlv_w_end(&w);
    if (w.err) return fail(c, MC_E_ARG, "arg_write", 0);
    new_exchange(c, true);
    int r = xchg(c, IM_PROTO, OP_WRITE_REQ, c->wk, w.len, "write");
    if (r) return r;
    if (c->rph.proto != IM_PROTO) return fail(c, MC_E_PROTO, "proto_write", 0);
    tlv_r_t rd;
    tlv_e_t top, e;
    tlv_r_init(&rd, c->rpl, c->rpl_len);
    if (!tlv_next(&rd, &top) || top.type != TLV_STRUCT) return fail(c, MC_E_PROTO, "proto_write", 0);
    if (c->rph.opcode == OP_STATUS_RSP) {
        unsigned st = tlv_find(top.d, top.n, 0, &e) ? (unsigned)e.u : 0xFF;
        return fail(c, MC_E_STATUS, "write_status_%02x", st);
    }
    if (c->rph.opcode != OP_WRITE_RSP || !tlv_find(top.d, top.n, 0, &e) || e.type != TLV_ARRAY) return fail(c, MC_E_PROTO, "proto_write", 0);
    tlv_r_t ar;
    tlv_e_t item;
    int count = 0;
    tlv_r_init(&ar, e.d, e.n);
    while (tlv_next(&ar, &item)) {
        tlv_e_t st, code;
        if (item.type != TLV_STRUCT || !tlv_find(item.d, item.n, 1, &st) || st.type != TLV_STRUCT || !tlv_find(st.d, st.n, 0, &code)) return fail(c, MC_E_PROTO, "proto_write", 0);
        count++;
        if (code.u != 0) return fail(c, MC_E_STATUS, "write_status_%02x", (unsigned)code.u);
    }
    if (!ar.p || count == 0) return fail(c, MC_E_PROTO, "proto_write", 0);
    if (c->pend_ack) {
        send_msg(c, IM_PROTO, OP_ACK, NULL, 0, false);
        c->pend_ack = false;
    }
    return MC_OK;
}

static void close_session(mc_client_t *c)
{
    static const uint8_t sr[8] = {0, 0, 0, 0, 0, 0, 3, 0};
    new_exchange(c, true);
    send_msg(c, SC_PROTO, OP_STATUS, sr, sizeof(sr), false);
}

int mc_write_int16(const mc_io_t *io, const wd_creds_t *cr, uint64_t peer_node, const mc_mrp_t *mrp, uint8_t ep, uint32_t cluster, uint32_t attr, int16_t val, char *err, size_t err_len)
{
    mc_client_t *c = &s_c;
    memset(c, 0, sizeof(*c));
    c->io = io;
    c->cr = cr;
    c->mrp = mrp;
    c->err = err;
    c->err_len = err_len;
    c->peer_node = peer_node;
    c->local_node = cr->node_id;
    c->base_ms = mrp->idle_ms;
    c->active_ms = mrp->active_ms;
    if (err_len) err[0] = '\0';
    mc_cert_t rcac, own;
    uint8_t cf[8], chk[MC_PUB_LEN];
    int rc;
    if (mc_cert_parse(cr->rcac, cr->rcac_len, &rcac) || !rcac.is_ca) {
        rc = fail(c, MC_E_CERT, "cert_rcac", 0);
    } else if (check_chain(c, &rcac, cr->icac_len ? cr->icac : NULL, cr->icac_len, cr->noc, cr->noc_len, &own, false)) {
        rc = MC_E_CERT;
    } else if (cr->key[0] != 4 || !mc_ct_eq(cr->key, own.pub, MC_PUB_LEN) || mc_pub_from_priv(cr->key + MC_PUB_LEN, chk, rng_cb, c) || !mc_ct_eq(chk, own.pub, MC_PUB_LEN)) {
        rc = fail(c, MC_E_KEY, "key_mismatch", 0);
    } else if (mc_compressed_fabric_id(rcac.pub, cr->fabric_id, cf) || mc_ipk_operational(cr->ipk, cf, c->ipk)) {
        rc = fail(c, MC_E_CRYPTO, "crypto_ipk", 0);
    } else {
        rc = case_connect(c, &rcac);
        if (rc == MC_OK) {
            rc = write_attr(c, ep, cluster, attr, val);
            close_session(c);
        }
    }
    if (err_len && !err[0] && rc) snprintf(err, err_len, "error_%d", rc);
    mc_wipe(c, sizeof(*c));
    mc_wipe(chk, sizeof(chk));
    return rc;
}
