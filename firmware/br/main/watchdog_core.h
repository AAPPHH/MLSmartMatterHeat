#pragma once
#include <stdbool.h>
#include <stddef.h>
#include <stdint.h>

#define WD_MAX_NODES 8
#define WD_RETRY_WINDOW_S 1800
#define WD_RETRY_MAX_DELAY_S 300
#define WD_ERR_LEN 24
#define WD_CERT_MAX 400
#define WD_KEY_LEN 97
#define WD_IPK_LEN 16
#define WD_REBOOT_GAP_S 30
#define WD_HB_VETO_S 300
#define WD_CONN_STABLE_S 600
#define WD_CONN_LEVELS 3
#define WD_SEND_MAX_S 2400
#define WD_STALL_S 30
#define WD_OT_BOOT_S 600
#define WD_CRASH_MAX 2
#define WD_CRASH_BACKOFF_S 120
#define WD_BT_FRAMES 8
#define WD_ATTRIB_S 300
#define WD_GLOBAL_CRASH_MAX 4
#define WD_GLOBAL_BACKOFF_S 600
#define WD_NO_WALL UINT32_MAX
#define WD_NO_NODE 0xFF
#define WD_TRACE_MAGIC 0x57445443u
#define WD_PANIC_MAGIC 0x57445042u
#define WD_RTC_MAGIC 0x57445256u

typedef enum { WD_OK = 0, WD_E_JSON, WD_E_FIELD, WD_E_RANGE } wd_err_t;
typedef enum { WD_ACT_NONE = 0, WD_ACT_TRIGGERED, WD_ACT_SEND } wd_action_kind_t;
typedef enum { WD_NODE_PENDING = 0, WD_NODE_OK, WD_NODE_GAVE_UP } wd_node_state_t;

typedef struct {
    uint8_t armed;
    uint8_t endpoint;
    uint8_t node_count;
    uint32_t timeout_s;
    int32_t setpoint_cc;
    uint64_t nodes[WD_MAX_NODES];
} wd_config_t;

typedef struct {
    wd_config_t cfg;
    uint32_t last_hb_s;
    uint32_t hb_carry_s;
    bool triggered;
    uint32_t triggered_at_s;
    uint32_t trig_carry_s;
    uint8_t node_state[WD_MAX_NODES];
    uint8_t node_tried[WD_MAX_NODES];
    uint8_t sending;
    uint8_t last_node;
    uint32_t last_send_s;
    uint32_t send_carry_s;
    uint8_t node_crashes[WD_MAX_NODES];
    uint8_t global_crashes;
    uint32_t global_due_s;
    uint32_t node_due_s[WD_MAX_NODES];
    uint8_t node_attempts[WD_MAX_NODES];
    char node_err[WD_MAX_NODES][WD_ERR_LEN];
} wd_state_t;

typedef struct {
    wd_action_kind_t kind;
    int node_idx;
} wd_action_t;

typedef struct {
    uint32_t magic;
    uint32_t hb_age_s;
    uint32_t trig_age_s;
    uint32_t wifi_restarts;
    uint32_t disc_reason;
    uint32_t nodes_hash;
    uint32_t wall_s;
    uint32_t since_send_s;
    uint32_t global_wait_s;
    uint32_t node_wait_s[WD_MAX_NODES];
    uint8_t node_crashes[WD_MAX_NODES];
    uint8_t triggered;
    uint8_t conn_level;
    uint8_t node_ok;
    uint8_t sending;
    uint8_t last_node;
    uint8_t global_crashes;
    uint8_t pad[2];
    uint32_t sum;
} wd_rtc_t;

typedef enum { WD_ST_IDLE = 0, WD_ST_START, WD_ST_CREDS, WD_ST_LOOKUP, WD_ST_CONNECT, WD_ST_TX, WD_ST_RXWAIT, WD_ST_CRYPTO, WD_ST_DONE, WD_ST_COUNT } wd_stage_t;

typedef struct {
    uint32_t magic;
    uint32_t seq;
    uint32_t start_ms;
    uint32_t node_lo;
    uint32_t stack_min;
    uint32_t stage_ms[WD_ST_COUNT];
    uint16_t seen;
    uint16_t tx;
    uint16_t rx;
    uint8_t stage;
    uint8_t pad;
    uint32_t sum;
} wd_trace_t;

typedef struct {
    uint32_t magic;
    uint32_t count;
    uint32_t uptime_s;
    uint32_t pc;
    uint32_t excvaddr;
    uint32_t cause;
    uint32_t a0;
    uint32_t bt[WD_BT_FRAMES];
    uint8_t exception;
    uint8_t core;
    uint8_t pad[2];
    char task[16];
    char reason[32];
    char details[48];
    char twdt[112];
    uint32_t sum;
} wd_panic_t;

typedef struct {
    uint8_t level;
    bool has_ip;
    uint32_t since_s;
} wd_conn_t;

typedef struct {
    const char *reset_name;
    int reset_code;
    uint32_t wifi_restarts;
    uint32_t conn_level;
    int disc_reason;
    bool has_ip;
    uint32_t noip_s;
    bool restored;
} wd_diag_t;

typedef struct {
    uint64_t fabric_id;
    uint64_t node_id;
    uint16_t rcac_len;
    uint16_t icac_len;
    uint16_t noc_len;
    uint8_t ipk[WD_IPK_LEN];
    uint8_t key[WD_KEY_LEN];
    uint8_t rcac[WD_CERT_MAX];
    uint8_t icac[WD_CERT_MAX];
    uint8_t noc[WD_CERT_MAX];
} wd_creds_t;

size_t wd_u64_str(uint64_t v, char *buf, size_t n);

#define WD_INLINE static inline __attribute__((always_inline))

WD_INLINE uint32_t wd_fnv(const void *p, size_t n)
{
    const uint8_t *b = p;
    uint32_t h = 2166136261u;
    for (size_t i = 0; i < n; i++) {
        h = (h ^ b[i]) * 16777619u;
    }
    return h;
}

WD_INLINE void wd_append_clean(char *dst, size_t n, const char *src)
{
    size_t o = 0;
    while (o + 1 < n && dst[o]) o++;
    for (size_t i = 0; src && src[i] && o + 1 < n; i++) {
        char c = src[i];
        dst[o++] = c >= 32 && c < 127 && c != '"' && c != '\\' ? c : '_';
    }
    if (n) dst[o] = '\0';
}

WD_INLINE void wd_panic_seal(wd_panic_t *p)
{
    p->magic = WD_PANIC_MAGIC;
    p->sum = wd_fnv(p, offsetof(wd_panic_t, sum));
}

void wd_config_defaults(wd_config_t *cfg);
bool wd_config_valid(const wd_config_t *cfg);
bool wd_config_equal(const wd_config_t *a, const wd_config_t *b);
void wd_state_init(wd_state_t *st, const wd_config_t *cfg);
bool wd_token_ok(const char *expected, const char *provided);
wd_err_t wd_parse_heartbeat(const char *body, wd_config_t *cfg);
wd_err_t wd_parse_mode(const char *body, bool *armed);
wd_err_t wd_parse_credentials(const char *body, wd_creds_t *out);
int wd_hex_decode(const char *s, uint8_t *out, size_t max, size_t *n);
uint32_t wd_age_s(const wd_state_t *st, uint32_t now_s);
bool wd_on_heartbeat(wd_state_t *st, uint32_t now_s);
void wd_set_armed(wd_state_t *st, bool armed, uint32_t now_s);
wd_action_t wd_tick(wd_state_t *st, uint32_t now_s);
void wd_report(wd_state_t *st, int node_idx, bool ok, const char *err, uint32_t now_s);
size_t wd_status_json(const wd_state_t *st, uint32_t now_s, bool have_temp, double die_c, const char *fw, bool armed_capable, const wd_diag_t *dg, char *buf, size_t n);
uint32_t wd_since_trigger_s(const wd_state_t *st, uint32_t now_s);
void wd_rtc_pack(wd_rtc_t *r, const wd_state_t *st, uint32_t now_s, uint32_t wall_s, const wd_conn_t *c, uint32_t wifi_restarts, uint32_t disc_reason);
bool wd_rtc_valid(const wd_rtc_t *r);
void wd_state_restore(wd_state_t *st, const wd_rtc_t *r, uint32_t now_s, uint32_t wall_s, bool abnormal);
void wd_conn_init(wd_conn_t *c, uint8_t level, uint32_t now_s);
void wd_conn_update(wd_conn_t *c, bool has_ip, uint32_t now_s);
uint32_t wd_conn_limit_s(uint8_t level, uint32_t base_s);
bool wd_conn_due(const wd_conn_t *c, uint32_t now_s, uint32_t base_s);
bool wd_wifi_restart_ok(const wd_conn_t *c, const wd_state_t *st, uint32_t now_s, uint32_t base_s);
bool wd_feed_ok(uint32_t now_s, uint32_t last_tick_s, bool send_active, uint32_t send_since_s, bool ot_seen);
bool wd_panic_valid(const wd_panic_t *p);
bool wd_trace_valid(const wd_trace_t *t);
void wd_trace_begin(wd_trace_t *t, uint64_t node, uint32_t now_ms);
void wd_trace_mark(wd_trace_t *t, wd_stage_t stage, uint32_t now_ms, uint32_t stack_free);
size_t wd_blackbox_json(const wd_panic_t *p, const wd_trace_t *t, char *buf, size_t n);
size_t wd_heartbeat_json(const wd_state_t *st, uint32_t age_s, char *buf, size_t n);
