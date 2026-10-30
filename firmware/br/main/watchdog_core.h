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
    bool triggered;
    uint32_t triggered_at_s;
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
size_t wd_status_json(const wd_state_t *st, uint32_t now_s, bool have_temp, double die_c, const char *fw, bool armed_capable, char *buf, size_t n);
size_t wd_heartbeat_json(const wd_state_t *st, uint32_t age_s, char *buf, size_t n);
