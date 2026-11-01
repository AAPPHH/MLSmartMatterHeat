#pragma once
#include <stdbool.h>
#include <stddef.h>
#include <stdint.h>
#include "watchdog_core.h"

#define MC_MTU 1280
#define MC_PUB_LEN 65
#define MC_SIG_LEN 64
#define MC_KEY_LEN 16
#define MC_TAG_LEN 16
#define MC_HASH_LEN 32
#define MC_RAND_LEN 32

#define TLV_UINT 1
#define TLV_SINT 2
#define TLV_BOOL 3
#define TLV_BYTES 4
#define TLV_UTF8 5
#define TLV_STRUCT 6
#define TLV_ARRAY 7
#define TLV_LIST 8
#define TLV_NULL 9
#define TLV_FLOAT 10

typedef struct {
    uint8_t *b;
    size_t cap;
    size_t len;
    bool err;
} tlv_w_t;

typedef struct {
    const uint8_t *p;
    const uint8_t *end;
} tlv_r_t;

typedef struct {
    uint8_t type;
    int tag;
    uint64_t u;
    int64_t s;
    const uint8_t *d;
    size_t n;
} tlv_e_t;

void tlv_w_init(tlv_w_t *w, uint8_t *b, size_t cap);
void tlv_w_start(tlv_w_t *w, int tag, uint8_t type);
void tlv_w_end(tlv_w_t *w);
void tlv_w_uint(tlv_w_t *w, int tag, uint64_t v);
void tlv_w_sint(tlv_w_t *w, int tag, int64_t v);
void tlv_w_bool(tlv_w_t *w, int tag, bool v);
void tlv_w_bytes(tlv_w_t *w, int tag, const uint8_t *d, size_t n);
void tlv_r_init(tlv_r_t *r, const uint8_t *p, size_t n);
bool tlv_next(tlv_r_t *r, tlv_e_t *e);
bool tlv_find(const uint8_t *p, size_t n, int tag, tlv_e_t *e);

typedef struct {
    uint8_t flags;
    uint8_t sflags;
    uint16_t sid;
    uint32_t ctr;
    bool has_src;
    bool has_dst;
    uint64_t src;
    uint64_t dst;
    size_t len;
} mc_hdr_t;

typedef struct {
    uint8_t xflags;
    uint8_t opcode;
    uint16_t xid;
    uint16_t proto;
    uint32_t ack;
    size_t len;
} mc_phdr_t;

#define XF_I 0x01
#define XF_A 0x02
#define XF_R 0x04
#define XF_SX 0x08
#define XF_V 0x10

int mc_hdr_encode(const mc_hdr_t *h, uint8_t *out, size_t cap);
int mc_hdr_decode(const uint8_t *in, size_t n, mc_hdr_t *h);
int mc_phdr_encode(const mc_phdr_t *h, uint8_t *out, size_t cap);
int mc_phdr_decode(const uint8_t *in, size_t n, mc_phdr_t *h);
void mc_nonce(uint8_t sflags, uint32_t ctr, uint64_t node, uint8_t out[13]);

int mc_hkdf(const uint8_t *salt, size_t slen, const uint8_t *ikm, size_t ilen, const uint8_t *info, size_t infolen, uint8_t *out, size_t olen);
#define MC_STR(s) (const uint8_t *)(s), (sizeof(s) - 1)
int mc_compressed_fabric_id(const uint8_t root_pub[MC_PUB_LEN], uint64_t fabric_id, uint8_t out[8]);
int mc_ipk_operational(const uint8_t epoch[MC_KEY_LEN], const uint8_t cfid[8], uint8_t out[MC_KEY_LEN]);
int mc_dest_id(const uint8_t ipk[MC_KEY_LEN], const uint8_t rnd[MC_RAND_LEN], const uint8_t root_pub[MC_PUB_LEN], uint64_t fabric_id, uint64_t node_id, uint8_t out[MC_HASH_LEN]);
int mc_ccm_seal(const uint8_t key[MC_KEY_LEN], const uint8_t nonce[13], const uint8_t *aad, size_t alen, uint8_t *buf, size_t n);
int mc_ccm_open(const uint8_t key[MC_KEY_LEN], const uint8_t nonce[13], const uint8_t *aad, size_t alen, uint8_t *buf, size_t n);
bool mc_ct_eq(const uint8_t *a, const uint8_t *b, size_t n);
void mc_wipe(void *p, size_t n);

typedef int (*mc_rng_fn)(void *ctx, uint8_t *b, size_t n);

int mc_ecdsa_sign(const uint8_t priv[32], const uint8_t *msg, size_t n, uint8_t sig[MC_SIG_LEN], mc_rng_fn rng, void *rctx);
int mc_pub_from_priv(const uint8_t priv[32], uint8_t pub[MC_PUB_LEN], mc_rng_fn rng, void *rctx);
int mc_ecdsa_verify(const uint8_t pub[MC_PUB_LEN], const uint8_t *msg, size_t n, const uint8_t sig[MC_SIG_LEN]);

typedef struct {
    const uint8_t *raw;
    size_t raw_len;
    tlv_e_t serial;
    tlv_e_t issuer;
    tlv_e_t subject;
    tlv_e_t exts;
    uint8_t sigalgo;
    uint32_t not_before;
    uint32_t not_after;
    uint8_t pubalgo;
    uint8_t curve;
    const uint8_t *pub;
    const uint8_t *sig;
    bool is_ca;
    bool has_node;
    bool has_fabric;
    uint64_t node_id;
    uint64_t fabric_id;
} mc_cert_t;

int mc_cert_parse(const uint8_t *tlv, size_t n, mc_cert_t *c);
int mc_cert_tbs_der(const mc_cert_t *c, uint8_t *out, size_t cap, size_t *len);
int mc_cert_verify(const mc_cert_t *c, const uint8_t issuer_pub[MC_PUB_LEN]);

typedef struct {
    void *ctx;
    int (*send)(void *ctx, const uint8_t *b, size_t n);
    int (*recv)(void *ctx, uint8_t *b, size_t max, uint32_t timeout_ms);
    uint32_t (*now_ms)(void *ctx);
    void (*rng)(void *ctx, uint8_t *b, size_t n);
} mc_io_t;

typedef struct {
    uint32_t idle_ms;
    uint32_t active_ms;
    uint32_t max_ms;
    uint8_t max_retx;
    uint32_t resp_ms;
} mc_mrp_t;

enum { MC_OK = 0, MC_E_ARG = -1, MC_E_TIMEOUT = -2, MC_E_PROTO = -3, MC_E_CERT = -4, MC_E_CRYPTO = -5, MC_E_PEER = -6, MC_E_STATUS = -7, MC_E_KEY = -8 };

void mc_mrp_from_txt(mc_mrp_t *m, uint32_t sii_ms, uint32_t sai_ms);
uint32_t mc_backoff_ms(uint32_t base_ms, uint8_t send_count, uint32_t max_ms, uint32_t rnd);

int mc_write_int16(const mc_io_t *io, const wd_creds_t *cr, uint64_t peer_node, const mc_mrp_t *mrp, uint8_t ep, uint32_t cluster, uint32_t attr, int16_t val, char *err, size_t err_len);
