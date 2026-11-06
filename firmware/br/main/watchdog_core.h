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
#define WD_RTC_MAGIC 0x57445254u

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
    uint8_t triggered;
    uint8_t conn_level;
    uint8_t node_ok;
    uint8_t pad;
    uint32_t sum;
} wd_rtc_t;

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
void wd_rtc_pack(wd_rtc_t *r, const wd_state_t *st, uint32_t now_s, const wd_conn_t *c, uint32_t wifi_restarts, uint32_t disc_reason);
bool wd_rtc_valid(const wd_rtc_t *r);
void wd_state_restore(wd_state_t *st, const wd_rtc_t *r, uint32_t now_s);
void wd_conn_init(wd_conn_t *c, uint8_t level, uint32_t now_s);
void wd_conn_update(wd_conn_t *c, bool has_ip, uint32_t now_s);
uint32_t wd_conn_limit_s(uint8_t level, uint32_t base_s);
bool wd_conn_due(const wd_conn_t *c, uint32_t now_s, uint32_t base_s);
bool wd_wifi_restart_ok(const wd_conn_t *c, const wd_state_t *st, uint32_t now_s, uint32_t base_s);
bool wd_feed_ok(uint32_t now_s, uint32_t last_tick_s, bool send_active, uint32_t send_since_s, bool ot_seen);
size_t wd_heartbeat_json(const wd_state_t *st, uint32_t age_s, char *buf, size_t n);
