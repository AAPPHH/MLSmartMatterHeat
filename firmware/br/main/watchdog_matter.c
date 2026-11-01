#include "watchdog_matter.h"
#include <string.h>

#ifndef CONFIG_BR_WATCHDOG_MATTER
esp_err_t wd_matter_write_setpoint(const wd_creds_t *creds, uint64_t node, uint8_t endpoint, int32_t centi_c, char *err, size_t err_len)
{
    (void)creds;
    (void)node;
    (void)endpoint;
    (void)centi_c;
    strlcpy(err, "matter_not_built", err_len);
    return ESP_ERR_NOT_SUPPORTED;
}
#else
#include <stdio.h>
#include <strings.h>
#include "esp_log.h"
#include "esp_openthread.h"
#include "esp_openthread_lock.h"
#include "esp_random.h"
#include "esp_timer.h"
#include "lwip/sockets.h"
#include "nvs.h"
#include "openthread/dns.h"
#include "openthread/ip6.h"
#include "openthread/srp_server.h"
#include "matter.h"

#define TAG "br_matter"
#define NVS_NS "br_wd"
#define MAX_ADDR 3
#define CLUSTER_THERMOSTAT 0x0201
#define ATTR_HEATING_SETPOINT 0x0012
#define MATTER_PORT_FALLBACK 5540

typedef struct {
    uint8_t addr[MAX_ADDR][16];
    uint8_t n;
    uint16_t port;
    uint32_t sii_ms;
    uint32_t sai_ms;
} peer_t;

typedef struct {
    uint8_t addr[16];
    uint16_t port;
    uint32_t sii_ms;
    uint32_t sai_ms;
} peer_cache_t;

typedef struct {
    int fd;
    struct sockaddr_in6 peer;
} net_t;

static int net_send(void *ctx, const uint8_t *b, size_t n)
{
    net_t *s = ctx;
    return send(s->fd, b, n, 0);
}

static int net_recv(void *ctx, uint8_t *b, size_t max, uint32_t ms)
{
    net_t *s = ctx;
    struct timeval tv = {.tv_sec = ms / 1000, .tv_usec = (ms % 1000) * 1000};
    setsockopt(s->fd, SOL_SOCKET, SO_RCVTIMEO, &tv, sizeof(tv));
    int r = recv(s->fd, b, max, 0);
    return r < 0 ? 0 : r;
}

static uint32_t net_now(void *ctx)
{
    (void)ctx;
    return (uint32_t)(esp_timer_get_time() / 1000);
}

static void net_rng(void *ctx, uint8_t *b, size_t n)
{
    (void)ctx;
    esp_fill_random(b, n);
}

static bool link_local(const uint8_t *a)
{
    return a[0] == 0xfe && (a[1] & 0xc0) == 0x80;
}

static void txt_u32(const uint8_t *txt, uint16_t len, const char *key, uint32_t *out)
{
    otDnsTxtEntryIterator it;
    otDnsTxtEntry e;
    otDnsInitTxtEntryIterator(&it, txt, len);
    while (otDnsGetNextTxtEntry(&it, &e) == OT_ERROR_NONE) {
        if (e.mKey && e.mValue && !strcmp(e.mKey, key) && e.mValueLength < 12) {
            char v[12];
            memcpy(v, e.mValue, e.mValueLength);
            v[e.mValueLength] = '\0';
            *out = (uint32_t)strtoul(v, NULL, 10);
        }
    }
}

static bool srp_lookup(const char *label, peer_t *p)
{
    bool found = false;
    memset(p, 0, sizeof(*p));
    esp_openthread_lock_acquire(portMAX_DELAY);
    otInstance *inst = esp_openthread_get_instance();
    for (const otSrpServerHost *h = otSrpServerGetNextHost(inst, NULL); h && !found; h = otSrpServerGetNextHost(inst, h)) {
        if (otSrpServerHostIsDeleted(h)) continue;
        for (const otSrpServerService *s = otSrpServerHostGetNextService(h, NULL); s; s = otSrpServerHostGetNextService(h, s)) {
            if (otSrpServerServiceIsDeleted(s) || strcasecmp(otSrpServerServiceGetInstanceLabel(s), label) != 0) continue;
            uint8_t cnt = 0;
            const otIp6Address *a = otSrpServerHostGetAddresses(h, &cnt);
            for (uint8_t i = 0; a && i < cnt && p->n < MAX_ADDR; i++) {
                if (!otIp6IsAddressUnspecified(&a[i]) && !link_local(a[i].mFields.m8)) memcpy(p->addr[p->n++], a[i].mFields.m8, 16);
            }
            p->port = otSrpServerServiceGetPort(s);
            uint16_t tl = 0;
            const uint8_t *txt = otSrpServerServiceGetTxtData(s, &tl);
            if (txt) {
                txt_u32(txt, tl, "SII", &p->sii_ms);
                txt_u32(txt, tl, "SAI", &p->sai_ms);
            }
            found = p->n > 0 && p->port != 0;
            break;
        }
    }
    esp_openthread_lock_release();
    return found;
}

static void cache_key(uint64_t node, char *key, size_t n)
{
    snprintf(key, n, "mc%x", (unsigned)node);
}

static void cache_save(uint64_t node, const peer_t *p)
{
    nvs_handle_t h;
    peer_cache_t c = {.port = p->port, .sii_ms = p->sii_ms, .sai_ms = p->sai_ms};
    char key[16];
    memcpy(c.addr, p->addr[0], 16);
    cache_key(node, key, sizeof(key));
    if (nvs_open(NVS_NS, NVS_READWRITE, &h) != ESP_OK) return;
    peer_cache_t old;
    size_t len = sizeof(old);
    if (nvs_get_blob(h, key, &old, &len) != ESP_OK || len != sizeof(old) || memcmp(&old, &c, sizeof(c)) != 0) {
        if (nvs_set_blob(h, key, &c, sizeof(c)) == ESP_OK) nvs_commit(h);
    }
    nvs_close(h);
}

static bool cache_load(uint64_t node, peer_t *p)
{
    nvs_handle_t h;
    peer_cache_t c;
    size_t len = sizeof(c);
    char key[16];
    cache_key(node, key, sizeof(key));
    if (nvs_open(NVS_NS, NVS_READONLY, &h) != ESP_OK) return false;
    bool ok = nvs_get_blob(h, key, &c, &len) == ESP_OK && len == sizeof(c) && c.port != 0;
    nvs_close(h);
    if (!ok) return false;
    memset(p, 0, sizeof(*p));
    memcpy(p->addr[0], c.addr, 16);
    p->n = 1;
    p->port = c.port;
    p->sii_ms = c.sii_ms;
    p->sai_ms = c.sai_ms;
    return true;
}

static esp_err_t try_peer(const wd_creds_t *cr, uint64_t node, const peer_t *p, int idx, int16_t value, uint8_t ep, char *err, size_t err_len)
{
    net_t n = {0};
    n.fd = socket(AF_INET6, SOCK_DGRAM, IPPROTO_UDP);
    if (n.fd < 0) {
        strlcpy(err, "socket", err_len);
        return ESP_FAIL;
    }
    n.peer.sin6_family = AF_INET6;
    n.peer.sin6_len = sizeof(n.peer);
    n.peer.sin6_port = htons(p->port ? p->port : MATTER_PORT_FALLBACK);
    memcpy(&n.peer.sin6_addr, p->addr[idx], 16);
    if (connect(n.fd, (struct sockaddr *)&n.peer, sizeof(n.peer)) != 0) {
        close(n.fd);
        strlcpy(err, "socket", err_len);
        return ESP_FAIL;
    }
    mc_io_t io = {&n, net_send, net_recv, net_now, net_rng};
    mc_mrp_t mrp;
    mc_mrp_from_txt(&mrp, p->sii_ms, p->sai_ms);
    int rc = mc_write_int16(&io, cr, node, &mrp, ep, CLUSTER_THERMOSTAT, ATTR_HEATING_SETPOINT, (int16_t)value, err, err_len);
    close(n.fd);
    return rc == MC_OK ? ESP_OK : rc == MC_E_TIMEOUT ? ESP_ERR_TIMEOUT : ESP_FAIL;
}

esp_err_t wd_matter_write_setpoint(const wd_creds_t *creds, uint64_t node, uint8_t endpoint, int32_t centi_c, char *err, size_t err_len)
{
    uint8_t cf[8];
    char label[40];
    peer_t p;
    err[0] = '\0';
    if (centi_c < -32768 || centi_c > 32767) {
        strlcpy(err, "range", err_len);
        return ESP_ERR_INVALID_ARG;
    }
    if (mc_creds_cfid(creds, cf)) {
        strlcpy(err, "cert_rcac", err_len);
        return ESP_FAIL;
    }
    snprintf(label, sizeof(label), "%02X%02X%02X%02X%02X%02X%02X%02X-%016llX", cf[0], cf[1], cf[2], cf[3], cf[4], cf[5], cf[6], cf[7], (unsigned long long)node);
    if (srp_lookup(label, &p)) {
        cache_save(node, &p);
    } else if (!cache_load(node, &p)) {
        strlcpy(err, "no_address", err_len);
        return ESP_ERR_NOT_FOUND;
    }
    esp_err_t r = ESP_FAIL;
    for (int i = 0; i < p.n && i < 2; i++) {
        r = try_peer(creds, node, &p, i, (int16_t)centi_c, endpoint, err, err_len);
        if (r != ESP_ERR_TIMEOUT) break;
        err[0] = '\0';
    }
    if (r == ESP_ERR_TIMEOUT && !err[0]) strlcpy(err, "timeout", err_len);
    ESP_LOGI(TAG, "node %llu: %s, heap min %u", (unsigned long long)node, r == ESP_OK ? "written" : err, (unsigned)esp_get_minimum_free_heap_size());
    return r;
}
#endif
