#include "watchdog_core.h"
#include <math.h>
#include <stdio.h>
#include <stddef.h>
#include <stdint.h>
#include <stdlib.h>
#include <string.h>
#include "cJSON.h"

#define MAX_NODE_ID 9007199254740991.0

void wd_config_defaults(wd_config_t *cfg)
{
    memset(cfg, 0, sizeof(*cfg));
    cfg->armed = 0;
    cfg->endpoint = 1;
    cfg->node_count = 4;
    cfg->timeout_s = 600;
    cfg->setpoint_cc = 1800;
    for (int i = 0; i < 4; i++) {
        cfg->nodes[i] = 5 + i;
    }
}

bool wd_config_valid(const wd_config_t *cfg)
{
    if (cfg->timeout_s < 60 || cfg->timeout_s > 86400) return false;
    if (cfg->setpoint_cc < 500 || cfg->setpoint_cc > 3000) return false;
    if (cfg->endpoint < 1 || cfg->node_count < 1 || cfg->node_count > WD_MAX_NODES) return false;
    for (int i = 0; i < cfg->node_count; i++) {
        if (cfg->nodes[i] < 1 || cfg->nodes[i] > (uint64_t)MAX_NODE_ID) return false;
    }
    return true;
}

bool wd_config_equal(const wd_config_t *a, const wd_config_t *b)
{
    if (a->armed != b->armed || a->endpoint != b->endpoint || a->node_count != b->node_count) return false;
    if (a->timeout_s != b->timeout_s || a->setpoint_cc != b->setpoint_cc) return false;
    return memcmp(a->nodes, b->nodes, sizeof(uint64_t) * a->node_count) == 0;
}

static void reset_nodes(wd_state_t *st)
{
    memset(st->node_state, 0, sizeof(st->node_state));
    memset(st->node_tried, 0, sizeof(st->node_tried));
    memset(st->node_due_s, 0, sizeof(st->node_due_s));
    memset(st->node_attempts, 0, sizeof(st->node_attempts));
    memset(st->node_err, 0, sizeof(st->node_err));
}

void wd_state_init(wd_state_t *st, const wd_config_t *cfg)
{
    memset(st, 0, sizeof(*st));
    st->cfg = *cfg;
}

bool wd_token_ok(const char *expected, const char *provided)
{
    if (!expected || !provided || expected[0] == '\0') return false;
    size_t le = strlen(expected), lp = strlen(provided);
    unsigned char diff = (unsigned char)(le != lp);
    for (size_t i = 0; i < le; i++) {
        diff |= (unsigned char)(expected[i] ^ provided[i < lp ? i : 0]);
    }
    return diff == 0;
}

static bool number_field(const cJSON *root, const char *key, double *out, wd_err_t *err)
{
    const cJSON *it = cJSON_GetObjectItemCaseSensitive(root, key);
    if (!it) return false;
    if (!cJSON_IsNumber(it)) {
        *err = WD_E_FIELD;
        return false;
    }
    *out = it->valuedouble;
    return true;
}

wd_err_t wd_parse_heartbeat(const char *body, wd_config_t *cfg)
{
    cJSON *root = cJSON_Parse(body);
    if (!root || !cJSON_IsObject(root)) {
        cJSON_Delete(root);
        return WD_E_JSON;
    }
    wd_config_t next = *cfg;
    wd_err_t err = WD_OK;
    double v;
    if (number_field(root, "timeout_s", &v, &err)) {
        if (v < 60 || v > 86400) err = WD_E_RANGE;
        else next.timeout_s = (uint32_t)v;
    }
    if (err == WD_OK && number_field(root, "setpoint_c", &v, &err)) {
        if (v < 5.0 || v > 30.0) err = WD_E_RANGE;
        else next.setpoint_cc = (int32_t)llround(v * 100.0);
    }
    if (err == WD_OK && number_field(root, "endpoint", &v, &err)) {
        if (v < 1 || v > 254) err = WD_E_RANGE;
        else next.endpoint = (uint8_t)v;
    }
    const cJSON *nodes = cJSON_GetObjectItemCaseSensitive(root, "nodes");
    if (err == WD_OK && nodes) {
        int n = cJSON_IsArray(nodes) ? cJSON_GetArraySize(nodes) : 0;
        if (!cJSON_IsArray(nodes)) err = WD_E_FIELD;
        else if (n < 1 || n > WD_MAX_NODES) err = WD_E_RANGE;
        for (int i = 0; err == WD_OK && i < n; i++) {
            const cJSON *e = cJSON_GetArrayItem(nodes, i);
            if (!cJSON_IsNumber(e)) err = WD_E_FIELD;
            else if (e->valuedouble < 1 || e->valuedouble > MAX_NODE_ID) err = WD_E_RANGE;
            else next.nodes[i] = (uint64_t)e->valuedouble;
        }
        if (err == WD_OK) next.node_count = (uint8_t)n;
    }
    cJSON_Delete(root);
    if (err != WD_OK) return err;
    if (!wd_config_valid(&next)) return WD_E_RANGE;
    *cfg = next;
    return WD_OK;
}

wd_err_t wd_parse_mode(const char *body, bool *armed)
{
    cJSON *root = cJSON_Parse(body);
    if (!root || !cJSON_IsObject(root)) {
        cJSON_Delete(root);
        return WD_E_JSON;
    }
    const cJSON *m = cJSON_GetObjectItemCaseSensitive(root, "mode");
    wd_err_t err = WD_OK;
    if (!cJSON_IsString(m)) err = WD_E_FIELD;
    else if (strcmp(m->valuestring, "armed") == 0) *armed = true;
    else if (strcmp(m->valuestring, "dry_run") == 0) *armed = false;
    else err = WD_E_RANGE;
    cJSON_Delete(root);
    return err;
}

int wd_hex_decode(const char *s, uint8_t *out, size_t max, size_t *n)
{
    size_t len = strlen(s);
    if (len % 2 || len / 2 > max) return -1;
    for (size_t i = 0; i < len; i += 2) {
        unsigned v = 0;
        for (int k = 0; k < 2; k++) {
            char c = s[i + k];
            unsigned d;
            if (c >= '0' && c <= '9') d = c - '0';
            else if (c >= 'a' && c <= 'f') d = c - 'a' + 10;
            else if (c >= 'A' && c <= 'F') d = c - 'A' + 10;
            else return -1;
            v = (v << 4) | d;
        }
        out[i / 2] = (uint8_t)v;
    }
    *n = len / 2;
    return 0;
}

static bool hex_u64(const cJSON *root, const char *key, uint64_t *out)
{
    const cJSON *it = cJSON_GetObjectItemCaseSensitive(root, key);
    if (!cJSON_IsString(it) || !it->valuestring[0]) return false;
    const char *s = it->valuestring;
    if (s[0] == '0' && (s[1] == 'x' || s[1] == 'X')) s += 2;
    if (!s[0] || strlen(s) > 16) return false;
    char *end;
    *out = strtoull(s, &end, 16);
    return *end == '\0';
}

static bool hex_blob(const cJSON *root, const char *key, uint8_t *out, size_t max, uint16_t *len, size_t exact, bool required)
{
    const cJSON *it = cJSON_GetObjectItemCaseSensitive(root, key);
    if (!it) return !required;
    size_t n;
    if (!cJSON_IsString(it) || wd_hex_decode(it->valuestring, out, max, &n) != 0) return false;
    if (exact ? n != exact : n == 0) return false;
    if (len) *len = (uint16_t)n;
    return true;
}

wd_err_t wd_parse_credentials(const char *body, wd_creds_t *out)
{
    cJSON *root = cJSON_Parse(body);
    if (!root || !cJSON_IsObject(root)) {
        cJSON_Delete(root);
        return WD_E_JSON;
    }
    memset(out, 0, sizeof(*out));
    bool ok = hex_u64(root, "fabric_id", &out->fabric_id) && out->fabric_id != 0;
    ok = ok && hex_u64(root, "node_id", &out->node_id) && out->node_id != 0;
    ok = ok && hex_blob(root, "rcac", out->rcac, WD_CERT_MAX, &out->rcac_len, 0, true);
    ok = ok && hex_blob(root, "icac", out->icac, WD_CERT_MAX, &out->icac_len, 0, false);
    ok = ok && hex_blob(root, "noc", out->noc, WD_CERT_MAX, &out->noc_len, 0, true);
    ok = ok && hex_blob(root, "key", out->key, WD_KEY_LEN, NULL, WD_KEY_LEN, true);
    ok = ok && hex_blob(root, "ipk", out->ipk, WD_IPK_LEN, NULL, WD_IPK_LEN, true);
    cJSON_Delete(root);
    if (!ok) {
        memset(out, 0, sizeof(*out));
        return WD_E_FIELD;
    }
    return WD_OK;
}

static uint32_t sat_add(uint32_t a, uint32_t b)
{
    return a > UINT32_MAX - b ? UINT32_MAX : a + b;
}

uint32_t wd_age_s(const wd_state_t *st, uint32_t now_s)
{
    return sat_add(now_s >= st->last_hb_s ? now_s - st->last_hb_s : 0, st->hb_carry_s);
}

uint32_t wd_since_trigger_s(const wd_state_t *st, uint32_t now_s)
{
    return sat_add(now_s >= st->triggered_at_s ? now_s - st->triggered_at_s : 0, st->trig_carry_s);
}

bool wd_on_heartbeat(wd_state_t *st, uint32_t now_s)
{
    bool rearmed = st->triggered;
    st->last_hb_s = now_s;
    st->hb_carry_s = 0;
    if (rearmed) {
        st->triggered = false;
        st->triggered_at_s = 0;
        st->trig_carry_s = 0;
        reset_nodes(st);
    }
    return rearmed;
}

void wd_set_armed(wd_state_t *st, bool armed, uint32_t now_s)
{
    bool was = st->cfg.armed;
    st->cfg.armed = armed;
    if (armed && !was && st->triggered) {
        st->triggered_at_s = now_s;
        st->trig_carry_s = 0;
        reset_nodes(st);
    }
}

wd_action_t wd_tick(wd_state_t *st, uint32_t now_s)
{
    wd_action_t act = {WD_ACT_NONE, -1};
    if (!st->triggered) {
        if (wd_age_s(st, now_s) < st->cfg.timeout_s) return act;
        st->triggered = true;
        st->triggered_at_s = now_s;
        st->trig_carry_s = 0;
        reset_nodes(st);
        act.kind = WD_ACT_TRIGGERED;
        return act;
    }
    if (!st->cfg.armed) return act;
    for (int i = 0; i < st->cfg.node_count; i++) {
        if (st->node_state[i] != WD_NODE_PENDING || st->node_due_s[i] > now_s) continue;
        if (wd_since_trigger_s(st, now_s) > WD_RETRY_WINDOW_S) {
            st->node_state[i] = WD_NODE_GAVE_UP;
            continue;
        }
        act.kind = WD_ACT_SEND;
        act.node_idx = i;
        return act;
    }
    return act;
}

void wd_report(wd_state_t *st, int i, bool ok, const char *err, uint32_t now_s)
{
    if (!st->triggered || i < 0 || i >= st->cfg.node_count) return;
    st->node_tried[i] = 1;
    st->node_attempts[i]++;
    if (ok) {
        st->node_state[i] = WD_NODE_OK;
        st->node_err[i][0] = '\0';
        return;
    }
    size_t k = 0;
    for (; err && err[k] && k < WD_ERR_LEN - 1; k++) {
        char c = err[k];
        st->node_err[i][k] = (c >= 'a' && c <= 'z') || (c >= 'A' && c <= 'Z') || (c >= '0' && c <= '9') || c == '_' || c == '-' ? c : '_';
    }
    st->node_err[i][k] = '\0';
    uint32_t shift = st->node_attempts[i] < 5 ? st->node_attempts[i] : 5;
    uint32_t delay = 15u << shift;
    if (delay > WD_RETRY_MAX_DELAY_S) delay = WD_RETRY_MAX_DELAY_S;
    st->node_due_s[i] = now_s + delay;
}

size_t wd_heartbeat_json(const wd_state_t *st, uint32_t age_s, char *buf, size_t n)
{
    int w = snprintf(buf, n, "{\"ok\":true,\"mode\":\"%s\",\"age_s\":%u}", st->cfg.armed ? "armed" : "dry_run", (unsigned)age_s);
    return w < 0 ? 0 : (size_t)w;
}

#define APPEND(...)                                                 \
    do {                                                            \
        int w_ = snprintf(buf + o, o < n ? n - o : 0, __VA_ARGS__); \
        if (w_ > 0) o += (size_t)w_;                                \
    } while (0)

size_t wd_status_json(const wd_state_t *st, uint32_t now_s, bool have_temp, double die_c, const char *fw, bool armed_capable, const wd_diag_t *dg, char *buf, size_t n)
{
    size_t o = 0;
    APPEND("{\"mode\":\"%s\",\"armed_capable\":%s,\"last_heartbeat_age_s\":%u,\"timeout_s\":%u,\"setpoint_c\":%.2f,\"endpoint\":%u,\"triggered\":%s,\"triggered_at_uptime_s\":",
           st->cfg.armed ? "armed" : "dry_run", armed_capable ? "true" : "false", (unsigned)wd_age_s(st, now_s), (unsigned)st->cfg.timeout_s,
           st->cfg.setpoint_cc / 100.0, (unsigned)st->cfg.endpoint, st->triggered ? "true" : "false");
    if (st->triggered) APPEND("%u", (unsigned)st->triggered_at_s);
    else APPEND("null");
    APPEND(",\"results\":[");
    bool first = true;
    for (int i = 0; st->triggered && i < st->cfg.node_count; i++) {
        if (!st->node_tried[i] && st->node_state[i] != WD_NODE_GAVE_UP) continue;
        APPEND("%s{\"node\":%llu,\"ok\":%s,\"err\":", first ? "" : ",", (unsigned long long)st->cfg.nodes[i], st->node_state[i] == WD_NODE_OK ? "true" : "false");
        if (st->node_err[i][0]) APPEND("\"%s\"", st->node_err[i]);
        else APPEND("null");
        APPEND("}");
        first = false;
    }
    APPEND("],\"die_temperature_c\":");
    if (have_temp) APPEND("%.1f", die_c);
    else APPEND("null");
    APPEND(",\"uptime_s\":%u,\"fw\":\"%s\"", (unsigned)now_s, fw ? fw : "");
    if (dg) {
        APPEND(",\"reset_reason\":\"%s\",\"reset_reason_code\":%d,\"state_restored\":%s,\"wifi_restarts\":%u,\"conn_level\":%u,\"wifi_ip\":%s,\"noip_s\":%u,\"wifi_disc_reason\":",
               dg->reset_name ? dg->reset_name : "unknown", dg->reset_code, dg->restored ? "true" : "false", (unsigned)dg->wifi_restarts, (unsigned)dg->conn_level, dg->has_ip ? "true" : "false", (unsigned)dg->noip_s);
        if (dg->disc_reason > 0) APPEND("%d", dg->disc_reason);
        else APPEND("null");
    }
    APPEND("}");
    if (n) buf[o < n ? o : n - 1] = '\0';
    return o;
}

static uint32_t nodes_hash(const wd_config_t *cfg)
{
    uint32_t h = 2166136261u;
    for (int i = 0; i < cfg->node_count; i++) {
        for (int k = 0; k < 8; k++) {
            h = (h ^ (uint8_t)(cfg->nodes[i] >> (8 * k))) * 16777619u;
        }
    }
    return h;
}

static uint32_t rtc_sum(const wd_rtc_t *r)
{
    const uint8_t *p = (const uint8_t *)r;
    uint32_t h = 2166136261u;
    for (size_t i = 0; i < offsetof(wd_rtc_t, sum); i++) {
        h = (h ^ p[i]) * 16777619u;
    }
    return h;
}

void wd_rtc_pack(wd_rtc_t *r, const wd_state_t *st, uint32_t now_s, const wd_conn_t *c, uint32_t wifi_restarts, uint32_t disc_reason)
{
    memset(r, 0, sizeof(*r));
    r->magic = WD_RTC_MAGIC;
    r->hb_age_s = wd_age_s(st, now_s);
    r->triggered = st->triggered;
    r->trig_age_s = st->triggered ? wd_since_trigger_s(st, now_s) : 0;
    r->wifi_restarts = wifi_restarts;
    r->disc_reason = disc_reason;
    r->conn_level = c->level;
    r->nodes_hash = nodes_hash(&st->cfg);
    for (int i = 0; st->triggered && i < st->cfg.node_count; i++) {
        if (st->node_state[i] == WD_NODE_OK) r->node_ok |= (uint8_t)(1u << i);
    }
    r->sum = rtc_sum(r);
}

bool wd_rtc_valid(const wd_rtc_t *r)
{
    return r->magic == WD_RTC_MAGIC && r->sum == rtc_sum(r);
}

void wd_state_restore(wd_state_t *st, const wd_rtc_t *r, uint32_t now_s)
{
    st->last_hb_s = now_s;
    st->hb_carry_s = sat_add(r->hb_age_s, WD_REBOOT_GAP_S);
    if (!r->triggered) return;
    st->triggered = true;
    st->triggered_at_s = now_s;
    st->trig_carry_s = sat_add(r->trig_age_s, WD_REBOOT_GAP_S);
    reset_nodes(st);
    if (r->nodes_hash != nodes_hash(&st->cfg)) return;
    for (int i = 0; i < st->cfg.node_count; i++) {
        if (!(r->node_ok & (1u << i))) continue;
        st->node_state[i] = WD_NODE_OK;
        st->node_tried[i] = 1;
        st->node_attempts[i] = 1;
    }
}

void wd_conn_init(wd_conn_t *c, uint8_t level, uint32_t now_s)
{
    c->level = level;
    c->has_ip = false;
    c->since_s = now_s;
}

void wd_conn_update(wd_conn_t *c, bool has_ip, uint32_t now_s)
{
    if (has_ip != c->has_ip) {
        c->has_ip = has_ip;
        c->since_s = now_s;
    } else if (has_ip && now_s - c->since_s >= WD_CONN_STABLE_S) {
        c->level = 0;
    }
}

uint32_t wd_conn_limit_s(uint8_t level, uint32_t base_s)
{
    static const uint32_t mult[WD_CONN_LEVELS] = {1, 4, 24};
    return base_s * mult[level < WD_CONN_LEVELS ? level : WD_CONN_LEVELS - 1];
}

bool wd_conn_due(const wd_conn_t *c, uint32_t now_s, uint32_t base_s)
{
    return !c->has_ip && now_s - c->since_s >= wd_conn_limit_s(c->level, base_s);
}

bool wd_wifi_restart_ok(const wd_conn_t *c, const wd_state_t *st, uint32_t now_s, uint32_t base_s)
{
    return base_s > 0 && wd_conn_due(c, now_s, base_s) && wd_age_s(st, now_s) >= WD_HB_VETO_S;
}

bool wd_feed_ok(uint32_t now_s, uint32_t last_tick_s, bool send_active, uint32_t send_since_s, bool ot_seen)
{
    if (!ot_seen && now_s > WD_OT_BOOT_S) return false;
    if (send_active) return now_s - send_since_s <= WD_SEND_MAX_S;
    return now_s - last_tick_s <= WD_STALL_S;
}
