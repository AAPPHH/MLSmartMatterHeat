#include "watchdog.h"
#include <stdio.h>
#include <string.h>
#include <sys/param.h>
#include "driver/temperature_sensor.h"
#include "esp_attr.h"
#include "esp_app_desc.h"
#include "esp_cpu_utils.h"
#include "esp_debug_helpers.h"
#include "esp_memory_utils.h"
#include "esp_private/panic_internal.h"
#include "esp_event.h"
#include "esp_http_server.h"
#include "esp_heap_caps.h"
#include "esp_log.h"
#include "esp_ota_ops.h"
#include "esp_openthread.h"
#include "esp_openthread_lock.h"
#include "esp_netif.h"
#include "esp_system.h"
#include "esp_task_wdt.h"
#include "esp_wifi.h"
#include "openthread/instance.h"
#include "openthread/thread.h"
#include "esp_timer.h"
#include "freertos/FreeRTOS.h"
#include "freertos/semphr.h"
#include "freertos/task.h"
#include "mbedtls/platform_util.h"
#include "xtensa_context.h"
#include "nvs.h"
#include "sdkconfig.h"
#include "watchdog_core.h"
#include "watchdog_matter.h"

#if CONFIG_OPENTHREAD_BR_START_WEB
#error "br_watchdog serves its own httpd on port 80; disable CONFIG_OPENTHREAD_BR_START_WEB"
#endif

#define TAG "br_watchdog"
#define NVS_NS "br_wd"
#define NVS_KEY_CFG "cfg"
#define NVS_KEY_CREDS "creds"
#define CFG_MAGIC 0x57443031u
#define BODY_MAX 4096
#define STATUS_MAX 2048
#define HEAP_MAX 1536
#define WD_TASK_STACK 14336
#define OTA_CHUNK 4096
#define OTA_VALIDATE_GOOD_S 60
#define OTA_VALIDATE_DEADLINE_S 600
#define HDR_TOKEN "X-Heatd-Token"
#define OT_PROBE_S 5
#define SUP_TASK_STACK 3072
#define OTA_MAX_TIMEOUTS 12
#define CONN_BASE_S (CONFIG_BR_CONN_REBOOT_MINUTES == 0 ? 0 : MAX(CONFIG_BR_CONN_REBOOT_MINUTES, 11) * 60)

typedef struct {
    uint32_t magic;
    wd_config_t cfg;
} cfg_blob_t;

static wd_state_t s_state;
static wd_creds_t s_creds;
static bool s_have_creds;
static SemaphoreHandle_t s_lock;
static temperature_sensor_handle_t s_tsens;
static httpd_handle_t s_httpd;
static volatile uint32_t s_ticks;
static volatile uint32_t s_stack_free_min = WD_TASK_STACK;
static RTC_NOINIT_ATTR wd_rtc_t s_rtc;
static RTC_NOINIT_ATTR wd_panic_t s_panic;
static RTC_NOINIT_ATTR wd_trace_t s_trace;
static DRAM_ATTR char s_twdt[sizeof(((wd_panic_t *)0)->twdt)];
static wd_conn_t s_conn;
static wd_diag_t s_diag;
static uint32_t s_wifi_restarts;
static TaskHandle_t s_sup_task;
static volatile uint32_t s_last_tick_s;
static volatile bool s_send_active;
static volatile uint32_t s_send_since_s;
static volatile bool s_ot_seen;
static volatile bool s_probe_pending;
static volatile uint32_t s_disc_count;
static volatile uint32_t s_disc_reason;

typedef void (*ot_task_t)(void *);
esp_err_t esp_openthread_task_queue_post(ot_task_t task, void *arg);

extern void __real_esp_panic_handler(panic_info_t *info);

static uint32_t uptime_s(void)
{
    return (uint32_t)(esp_timer_get_time() / 1000000);
}

void br_watchdog_stage(wd_stage_t stage)
{
    uint32_t hw = (uint32_t)uxTaskGetStackHighWaterMark(NULL) * sizeof(StackType_t);
    if (hw < s_stack_free_min) s_stack_free_min = hw;
    wd_trace_mark(&s_trace, stage, (uint32_t)(esp_timer_get_time() / 1000), hw);
}

static void IRAM_ATTR twdt_collect(void *opaque, const char *msg)
{
    if (strncmp(msg, "Task watchdog got", 17) == 0) return;
    if (msg[0] == '\n') wd_append_clean(s_twdt, sizeof(s_twdt), s_twdt[0] ? "," : "");
    else wd_append_clean(s_twdt, sizeof(s_twdt), msg);
}

void IRAM_ATTR esp_task_wdt_isr_user_handler(void)
{
    s_twdt[0] = '\0';
    esp_task_wdt_print_triggered_tasks(twdt_collect, NULL, NULL);
    wd_append_clean(s_twdt, sizeof(s_twdt), " | running:");
    for (int c = 0; c < CONFIG_FREERTOS_NUMBER_OF_CORES; c++) {
        TaskHandle_t t = xTaskGetCurrentTaskHandleForCore(c);
        wd_append_clean(s_twdt, sizeof(s_twdt), " ");
        wd_append_clean(s_twdt, sizeof(s_twdt), t ? pcTaskGetName(t) : "");
    }
}

void IRAM_ATTR __wrap_esp_panic_handler(panic_info_t *info)
{
    uint32_t prev = s_panic.magic == WD_PANIC_MAGIC && s_panic.sum == wd_fnv(&s_panic, offsetof(wd_panic_t, sum)) ? s_panic.count : 0;
    memset(&s_panic, 0, sizeof(s_panic));
    s_panic.count = prev + 1;
    s_panic.uptime_s = (uint32_t)(esp_timer_get_time() / 1000000);
    s_panic.exception = (uint8_t)info->exception;
    s_panic.core = (uint8_t)info->core;
    s_panic.pc = (uint32_t)info->addr;
    const XtExcFrame *f = info->frame;
    if (f) {
        s_panic.cause = f->exccause;
        s_panic.excvaddr = f->excvaddr;
        esp_backtrace_frame_t bt = {.pc = f->pc, .sp = f->a1, .next_pc = f->a0, .exc_frame = f};
        bool ok = esp_stack_ptr_is_sane(bt.sp) && esp_ptr_executable((void *)esp_cpu_process_stack_pc(bt.pc));
        for (int i = 0; ok && i < WD_BT_FRAMES; i++) {
            s_panic.bt[i] = esp_cpu_process_stack_pc(bt.pc);
            if (bt.next_pc == 0 || !esp_backtrace_get_next_frame(&bt)) break;
        }
    }
    TaskHandle_t th = xTaskGetCurrentTaskHandleForCore(info->core);
    wd_append_clean(s_panic.task, sizeof(s_panic.task), th ? pcTaskGetName(th) : "");
    wd_append_clean(s_panic.reason, sizeof(s_panic.reason), info->reason);
    wd_append_clean(s_panic.details, sizeof(s_panic.details), g_panic_abort ? g_panic_abort_details : "");
    wd_append_clean(s_panic.twdt, sizeof(s_panic.twdt), s_twdt);
    wd_panic_seal(&s_panic);
    __real_esp_panic_handler(info);
}

static void cfg_save(const wd_config_t *cfg)
{
    nvs_handle_t h;
    cfg_blob_t blob = {CFG_MAGIC, *cfg};
    if (nvs_open(NVS_NS, NVS_READWRITE, &h) != ESP_OK) return;
    if (nvs_set_blob(h, NVS_KEY_CFG, &blob, sizeof(blob)) == ESP_OK) nvs_commit(h);
    nvs_close(h);
}

static void load_nvs(wd_config_t *cfg)
{
    wd_config_defaults(cfg);
    nvs_handle_t h;
    if (nvs_open(NVS_NS, NVS_READONLY, &h) != ESP_OK) return;
    cfg_blob_t blob;
    size_t len = sizeof(blob);
    if (nvs_get_blob(h, NVS_KEY_CFG, &blob, &len) == ESP_OK && len == sizeof(blob) && blob.magic == CFG_MAGIC && wd_config_valid(&blob.cfg)) {
        *cfg = blob.cfg;
    }
    len = sizeof(s_creds);
    s_have_creds = nvs_get_blob(h, NVS_KEY_CREDS, &s_creds, &len) == ESP_OK && len == sizeof(s_creds);
    if (s_have_creds && (s_creds.rcac_len == 0 || s_creds.rcac_len > WD_CERT_MAX || s_creds.icac_len > WD_CERT_MAX || s_creds.noc_len == 0 || s_creds.noc_len > WD_CERT_MAX)) {
        mbedtls_platform_zeroize(&s_creds, sizeof(s_creds));
        s_have_creds = false;
    }
    nvs_close(h);
    if (!WD_MATTER_BUILT) cfg->armed = 0;
}

static const char *fw_version(void)
{
    return esp_app_get_description()->version;
}

static bool read_temp(double *c)
{
    float v;
    if (!s_tsens || temperature_sensor_get_celsius(s_tsens, &v) != ESP_OK) return false;
    *c = v;
    return true;
}

static esp_err_t send_json(httpd_req_t *req, const char *status, const char *json)
{
    httpd_resp_set_status(req, status);
    httpd_resp_set_type(req, "application/json");
    return httpd_resp_sendstr(req, json);
}

static bool authorized(httpd_req_t *req)
{
    char tok[96] = "";
    size_t len = httpd_req_get_hdr_value_len(req, HDR_TOKEN);
    if (len == 0 || len >= sizeof(tok) || httpd_req_get_hdr_value_str(req, HDR_TOKEN, tok, sizeof(tok)) != ESP_OK) tok[0] = '\0';
    if (wd_token_ok(CONFIG_BR_WATCHDOG_TOKEN, tok)) return true;
    send_json(req, "401 Unauthorized", "{\"ok\":false,\"err\":\"unauthorized\"}");
    return false;
}

static char *read_body(httpd_req_t *req)
{
    if (req->content_len == 0 || req->content_len >= BODY_MAX) {
        send_json(req, "400 Bad Request", "{\"ok\":false,\"err\":\"body_size\"}");
        return NULL;
    }
    char *buf = malloc(req->content_len + 1);
    if (!buf) {
        send_json(req, "500 Internal Server Error", "{\"ok\":false,\"err\":\"oom\"}");
        return NULL;
    }
    size_t got = 0;
    while (got < req->content_len) {
        int r = httpd_req_recv(req, buf + got, req->content_len - got);
        if (r <= 0) {
            free(buf);
            send_json(req, "400 Bad Request", "{\"ok\":false,\"err\":\"recv\"}");
            return NULL;
        }
        got += (size_t)r;
    }
    buf[got] = '\0';
    return buf;
}

static esp_err_t bad_request(httpd_req_t *req, wd_err_t e)
{
    const char *msg = e == WD_E_JSON ? "{\"ok\":false,\"err\":\"json\"}" : e == WD_E_RANGE ? "{\"ok\":false,\"err\":\"range\"}" : "{\"ok\":false,\"err\":\"field\"}";
    return send_json(req, "400 Bad Request", msg);
}

static esp_err_t heartbeat_handler(httpd_req_t *req)
{
    if (!authorized(req)) return ESP_OK;
    char *body = read_body(req);
    if (!body) return ESP_OK;
    xSemaphoreTake(s_lock, portMAX_DELAY);
    wd_config_t next = s_state.cfg;
    wd_err_t e = wd_parse_heartbeat(body, &next);
    free(body);
    if (e != WD_OK) {
        xSemaphoreGive(s_lock);
        return bad_request(req, e);
    }
    uint32_t now = uptime_s();
    uint32_t age = wd_age_s(&s_state, now);
    bool changed = !wd_config_equal(&next, &s_state.cfg);
    s_state.cfg = next;
    if (wd_on_heartbeat(&s_state, now)) ESP_LOGW(TAG, "heartbeat back, watchdog re-armed");
    char out[96];
    wd_heartbeat_json(&s_state, age, out, sizeof(out));
    wd_config_t persist = s_state.cfg;
    xSemaphoreGive(s_lock);
    if (changed) cfg_save(&persist);
    return send_json(req, "200 OK", out);
}

static esp_err_t status_handler(httpd_req_t *req)
{
    char *out = malloc(STATUS_MAX);
    if (!out) return send_json(req, "500 Internal Server Error", "{\"ok\":false,\"err\":\"oom\"}");
    double c = 0;
    bool have = read_temp(&c);
    xSemaphoreTake(s_lock, portMAX_DELAY);
    wd_diag_t dg = s_diag;
    dg.wifi_restarts = s_wifi_restarts;
    dg.conn_level = s_conn.level;
    dg.has_ip = s_conn.has_ip;
    dg.noip_s = s_conn.has_ip ? 0 : uptime_s() - s_conn.since_s;
    dg.disc_reason = (int)s_disc_reason;
    size_t n = wd_status_json(&s_state, uptime_s(), have, c, fw_version(), WD_MATTER_BUILT && s_have_creds, &dg, out, STATUS_MAX);
    xSemaphoreGive(s_lock);
    esp_err_t r = n < STATUS_MAX ? send_json(req, "200 OK", out) : send_json(req, "500 Internal Server Error", "{\"ok\":false,\"err\":\"status_size\"}");
    free(out);
    return r;
}

static esp_err_t mode_handler(httpd_req_t *req)
{
    if (!authorized(req)) return ESP_OK;
    char *body = read_body(req);
    if (!body) return ESP_OK;
    bool armed = false;
    wd_err_t e = wd_parse_mode(body, &armed);
    free(body);
    if (e != WD_OK) return bad_request(req, e);
    xSemaphoreTake(s_lock, portMAX_DELAY);
    if (armed && !(WD_MATTER_BUILT && s_have_creds)) {
        xSemaphoreGive(s_lock);
        return send_json(req, "501 Not Implemented", WD_MATTER_BUILT ? "{\"ok\":false,\"err\":\"no_credentials\"}" : "{\"ok\":false,\"err\":\"matter_not_built\"}");
    }
    wd_set_armed(&s_state, armed, uptime_s());
    wd_config_t persist = s_state.cfg;
    char out[96];
    wd_heartbeat_json(&s_state, wd_age_s(&s_state, uptime_s()), out, sizeof(out));
    xSemaphoreGive(s_lock);
    cfg_save(&persist);
    ESP_LOGW(TAG, "mode set to %s", armed ? "armed" : "dry_run");
    return send_json(req, "200 OK", out);
}

static esp_err_t credentials_handler(httpd_req_t *req)
{
    if (!authorized(req)) return ESP_OK;
    char *body = read_body(req);
    if (!body) return ESP_OK;
    wd_creds_t *cr = calloc(1, sizeof(*cr));
    if (!cr) {
        free(body);
        return send_json(req, "500 Internal Server Error", "{\"ok\":false,\"err\":\"oom\"}");
    }
    wd_err_t e = wd_parse_credentials(body, cr);
    mbedtls_platform_zeroize(body, req->content_len);
    free(body);
    if (e != WD_OK) {
        free(cr);
        return bad_request(req, e);
    }
    nvs_handle_t h;
    esp_err_t r = nvs_open(NVS_NS, NVS_READWRITE, &h);
    if (r == ESP_OK) {
        r = nvs_set_blob(h, NVS_KEY_CREDS, cr, sizeof(*cr));
        if (r == ESP_OK) r = nvs_commit(h);
        nvs_close(h);
    }
    if (r == ESP_OK) {
        xSemaphoreTake(s_lock, portMAX_DELAY);
        s_creds = *cr;
        s_have_creds = true;
        xSemaphoreGive(s_lock);
    }
    mbedtls_platform_zeroize(cr, sizeof(*cr));
    free(cr);
    if (r != ESP_OK) return send_json(req, "500 Internal Server Error", "{\"ok\":false,\"err\":\"nvs\"}");
    ESP_LOGI(TAG, "operational credentials stored");
    return send_json(req, "200 OK", "{\"ok\":true}");
}

static esp_err_t ambient_handler(httpd_req_t *req)
{
    double c;
    char out[64];
    if (!read_temp(&c)) return send_json(req, "503 Service Unavailable", "{\"die_temperature_c\":null}");
    snprintf(out, sizeof(out), "{\"die_temperature_c\":%.1f}", c);
    return send_json(req, "200 OK", out);
}

static esp_err_t heap_handler(httpd_req_t *req)
{
    char out[HEAP_MAX];
    unsigned sup = s_sup_task ? (unsigned)uxTaskGetStackHighWaterMark(s_sup_task) * sizeof(StackType_t) : 0;
    wd_panic_t pn = s_panic;
    wd_trace_t tr = s_trace;
    int n = snprintf(out, sizeof(out), "{\"free\":%u,\"min_free\":%u,\"largest_block\":%u,\"wd_stack_size\":%u,\"wd_stack_free_min\":%u,\"sup_stack_size\":%u,\"sup_stack_free_min\":%u,", (unsigned)esp_get_free_heap_size(), (unsigned)esp_get_minimum_free_heap_size(), (unsigned)heap_caps_get_largest_free_block(MALLOC_CAP_8BIT), (unsigned)WD_TASK_STACK, (unsigned)s_stack_free_min, (unsigned)SUP_TASK_STACK, sup);
    n += (int)wd_blackbox_json(&pn, &tr, out + n, sizeof(out) - (size_t)n);
    if (n + 2 > (int)sizeof(out)) return send_json(req, "500 Internal Server Error", "{\"ok\":false,\"err\":\"heap_size\"}");
    strcpy(out + n, "}");
    return send_json(req, "200 OK", out);
}

static volatile bool s_ota_busy;

static void reboot_task(void *arg)
{
    vTaskDelay(pdMS_TO_TICKS(1500));
    esp_restart();
}

static esp_err_t ota_fail(httpd_req_t *req, const char *status, const char *err)
{
    char out[64];
    snprintf(out, sizeof(out), "{\"ok\":false,\"err\":\"%s\"}", err);
    s_ota_busy = false;
    return send_json(req, status, out);
}

static esp_err_t ota_handler(httpd_req_t *req)
{
    if (!authorized(req)) return ESP_OK;
    if (s_ota_busy) return send_json(req, "409 Conflict", "{\"ok\":false,\"err\":\"busy\"}");
    s_ota_busy = true;
    const esp_partition_t *part = esp_ota_get_next_update_partition(NULL);
    if (!part || req->content_len == 0 || req->content_len > part->size) return ota_fail(req, "400 Bad Request", part ? "size" : "no_slot");
    char *buf = malloc(OTA_CHUNK);
    if (!buf) return ota_fail(req, "500 Internal Server Error", "oom");
    esp_ota_handle_t h;
    if (esp_ota_begin(part, req->content_len, &h) != ESP_OK) {
        free(buf);
        return ota_fail(req, "500 Internal Server Error", "begin");
    }
    size_t got = 0;
    const char *err = NULL;
    int timeouts = 0;
    while (got < req->content_len && !err) {
        int r = httpd_req_recv(req, buf, MIN(OTA_CHUNK, req->content_len - got));
        if (r == HTTPD_SOCK_ERR_TIMEOUT) {
            if (++timeouts >= OTA_MAX_TIMEOUTS) err = "stalled";
            continue;
        }
        timeouts = 0;
        if (r <= 0) err = "recv";
        else if (esp_ota_write(h, buf, (size_t)r) != ESP_OK) err = "write";
        else got += (size_t)r;
    }
    free(buf);
    if (err) {
        esp_ota_abort(h);
        return ota_fail(req, "400 Bad Request", err);
    }
    if (esp_ota_end(h) != ESP_OK) return ota_fail(req, "400 Bad Request", "image_invalid");
    esp_app_desc_t nd;
    if (esp_ota_get_partition_description(part, &nd) != ESP_OK || strcmp(nd.project_name, esp_app_get_description()->project_name) != 0) return ota_fail(req, "400 Bad Request", "wrong_project");
    if (esp_ota_set_boot_partition(part) != ESP_OK) return ota_fail(req, "500 Internal Server Error", "set_boot");
    ESP_LOGW(TAG, "ota: %u bytes written to %s, version %s, rebooting", (unsigned)got, part->label, nd.version);
    char out[128];
    snprintf(out, sizeof(out), "{\"ok\":true,\"bytes\":%u,\"slot\":\"%s\",\"version\":\"%s\"}", (unsigned)got, part->label, nd.version);
    esp_err_t sr = send_json(req, "200 OK", out);
    xTaskCreate(reboot_task, "ota_reboot", 2048, NULL, 5, NULL);
    return sr;
}

static bool thread_attached(void)
{
    otInstance *inst = esp_openthread_get_instance();
    if (!otInstanceIsInitialized(inst) || !esp_openthread_lock_acquire(pdMS_TO_TICKS(500))) return false;
    otDeviceRole role = otThreadGetDeviceRole(inst);
    esp_openthread_lock_release();
    return role == OT_DEVICE_ROLE_CHILD || role == OT_DEVICE_ROLE_ROUTER || role == OT_DEVICE_ROLE_LEADER;
}

static void validate_task(void *arg)
{
    esp_ota_img_states_t st;
    if (esp_ota_get_state_partition(esp_ota_get_running_partition(), &st) != ESP_OK || st != ESP_OTA_IMG_PENDING_VERIFY) {
        vTaskDelete(NULL);
        return;
    }
    uint32_t good = 0, last_ticks = s_ticks;
    for (uint32_t t = 0; t < OTA_VALIDATE_DEADLINE_S; t++) {
        vTaskDelay(pdMS_TO_TICKS(1000));
        bool ok = s_ticks != last_ticks && s_httpd && thread_attached();
        last_ticks = s_ticks;
        good = ok ? good + 1 : 0;
        if (good >= OTA_VALIDATE_GOOD_S) {
            esp_ota_mark_app_valid_cancel_rollback();
            ESP_LOGW(TAG, "ota: new image marked valid");
            vTaskDelete(NULL);
            return;
        }
    }
    ESP_LOGE(TAG, "ota: new image never reached watchdog + Thread for %us, rolling back", OTA_VALIDATE_GOOD_S);
    esp_ota_mark_app_invalid_rollback_and_reboot();
    vTaskDelete(NULL);
}

static const httpd_uri_t s_uris[] = {
    {.uri = "/watchdog/heartbeat", .method = HTTP_POST, .handler = heartbeat_handler},
    {.uri = "/watchdog/status", .method = HTTP_GET, .handler = status_handler},
    {.uri = "/watchdog/mode", .method = HTTP_POST, .handler = mode_handler},
    {.uri = "/watchdog/credentials", .method = HTTP_POST, .handler = credentials_handler},
    {.uri = "/ambient", .method = HTTP_GET, .handler = ambient_handler},
    {.uri = "/ota", .method = HTTP_POST, .handler = ota_handler},
    {.uri = "/watchdog/heap", .method = HTTP_GET, .handler = heap_handler},
};
#define URI_COUNT (sizeof(s_uris) / sizeof(s_uris[0]))

static void start_http(void)
{
    httpd_config_t cfg = HTTPD_DEFAULT_CONFIG();
    cfg.max_uri_handlers = URI_COUNT;
    cfg.max_open_sockets = 3;
    cfg.lru_purge_enable = true;
    cfg.stack_size = 8192;
    esp_err_t r = httpd_start(&s_httpd, &cfg);
    if (r != ESP_OK) {
        ESP_LOGE(TAG, "httpd_start failed: %s", esp_err_to_name(r));
        return;
    }
    for (size_t i = 0; i < URI_COUNT; i++) {
        esp_err_t e = httpd_register_uri_handler(s_httpd, &s_uris[i]);
        if (e != ESP_OK) ESP_LOGE(TAG, "register %s failed: %s", s_uris[i].uri, esp_err_to_name(e));
    }
}

static void log_dry_run(const wd_state_t *st)
{
    char nodes[64] = "";
    size_t o = 0;
    for (int i = 0; i < st->cfg.node_count && o < sizeof(nodes) - 12; i++) {
        char nb[24];
        wd_u64_str(st->cfg.nodes[i], nb, sizeof(nb));
        o += snprintf(nodes + o, sizeof(nodes) - o, "%s%s", i ? "," : "", nb);
    }
    ESP_LOGW(TAG, "no heartbeat for %us: would send %.1f to nodes %s (dry_run)", (unsigned)st->cfg.timeout_s, st->cfg.setpoint_cc / 100.0, nodes);
}

static void rtc_store_locked(uint32_t now)
{
    wd_rtc_t r;
    wd_rtc_pack(&r, &s_state, now, &s_conn, s_wifi_restarts, s_disc_reason);
    s_rtc = r;
}

static void watchdog_task(void *arg)
{
    for (;;) {
        vTaskDelay(pdMS_TO_TICKS(1000));
        wd_creds_t *cr = NULL;
        xSemaphoreTake(s_lock, portMAX_DELAY);
        uint32_t now = uptime_s();
        wd_action_t act = wd_tick(&s_state, now);
        if (act.kind == WD_ACT_SEND) rtc_store_locked(now);
        wd_state_t snap = s_state;
        if (act.kind == WD_ACT_SEND && s_have_creds) {
            cr = malloc(sizeof(*cr));
            if (cr) memcpy(cr, &s_creds, sizeof(*cr));
        }
        xSemaphoreGive(s_lock);
        s_ticks++;
        s_last_tick_s = uptime_s();
        if (act.kind == WD_ACT_TRIGGERED) {
            if (snap.cfg.armed) ESP_LOGW(TAG, "no heartbeat for %us: sending %.1f to %u nodes", (unsigned)snap.cfg.timeout_s, snap.cfg.setpoint_cc / 100.0, (unsigned)snap.cfg.node_count);
            else log_dry_run(&snap);
        }
        if (act.kind != WD_ACT_SEND) continue;
        char err[WD_ERR_LEN] = "";
        esp_err_t r = ESP_ERR_INVALID_STATE;
        if (cr) {
            wd_trace_begin(&s_trace, snap.cfg.nodes[act.node_idx], (uint32_t)(esp_timer_get_time() / 1000));
            s_send_since_s = uptime_s();
            s_send_active = true;
            r = wd_matter_write_setpoint(cr, snap.cfg.nodes[act.node_idx], snap.cfg.endpoint, snap.cfg.setpoint_cc, err, sizeof(err));
            s_send_active = false;
            s_last_tick_s = uptime_s();
            mbedtls_platform_zeroize(cr, sizeof(*cr));
            free(cr);
            br_watchdog_stage(WD_ST_DONE);
        } else {
            strlcpy(err, "no_credentials", sizeof(err));
        }
        xSemaphoreTake(s_lock, portMAX_DELAY);
        wd_report(&s_state, act.node_idx, r == ESP_OK, err, uptime_s());
        rtc_store_locked(uptime_s());
        xSemaphoreGive(s_lock);
        char nb[24];
        wd_u64_str(snap.cfg.nodes[act.node_idx], nb, sizeof(nb));
        ESP_LOGW(TAG, "node %s write %s %s", nb, r == ESP_OK ? "ok" : "failed", err);
    }
}

static const char *reset_name(esp_reset_reason_t r)
{
    switch (r) {
    case ESP_RST_POWERON: return "poweron";
    case ESP_RST_EXT: return "external";
    case ESP_RST_SW: return "software";
    case ESP_RST_PANIC: return "panic";
    case ESP_RST_INT_WDT: return "int_wdt";
    case ESP_RST_TASK_WDT: return "task_wdt";
    case ESP_RST_WDT: return "other_wdt";
    case ESP_RST_DEEPSLEEP: return "deepsleep";
    case ESP_RST_BROWNOUT: return "brownout";
    case ESP_RST_USB: return "usb";
    case ESP_RST_JTAG: return "jtag";
    case ESP_RST_EFUSE: return "efuse";
    case ESP_RST_PWR_GLITCH: return "power_glitch";
    case ESP_RST_CPU_LOCKUP: return "cpu_lockup";
    default: return "unknown";
    }
}

static bool reset_crashed(esp_reset_reason_t r)
{
    return r == ESP_RST_PANIC || r == ESP_RST_INT_WDT || r == ESP_RST_TASK_WDT || r == ESP_RST_WDT;
}

static bool rtc_load(void)
{
    esp_reset_reason_t rr = esp_reset_reason();
    s_diag.reset_code = (int)rr;
    s_diag.reset_name = reset_name(rr);
    bool ok = rr != ESP_RST_POWERON && wd_rtc_valid(&s_rtc);
    if (!ok) memset(&s_rtc, 0, sizeof(s_rtc));
    if (rr == ESP_RST_POWERON) {
        memset(&s_panic, 0, sizeof(s_panic));
        memset(&s_trace, 0, sizeof(s_trace));
    }
    s_diag.restored = ok;
    s_wifi_restarts = s_rtc.wifi_restarts;
    s_disc_reason = s_rtc.disc_reason;
    return ok;
}

static bool sta_has_ip(void)
{
    esp_netif_t *nif = esp_netif_get_handle_from_ifkey("WIFI_STA_DEF");
    esp_netif_ip_info_t ip;
    return !nif || (esp_netif_get_ip_info(nif, &ip) == ESP_OK && ip.ip.addr != 0);
}

static void net_event(void *arg, esp_event_base_t base, int32_t id, void *data)
{
    s_disc_reason = ((wifi_event_sta_disconnected_t *)data)->reason;
    s_disc_count++;
}

static void wifi_restart(void)
{
    uint32_t before = s_disc_count;
    esp_wifi_disconnect();
    for (int i = 0; i < 10 && s_disc_count == before; i++) {
        vTaskDelay(pdMS_TO_TICKS(500));
    }
    if (s_disc_count == before) esp_wifi_connect();
}

static void ot_probe(void *arg)
{
    if (!s_ot_seen) esp_task_wdt_add(NULL);
    esp_task_wdt_reset();
    s_ot_seen = true;
    s_probe_pending = false;
}

static void supervisor_task(void *arg)
{
    esp_task_wdt_add(NULL);
    uint32_t probe_s = 0;
    for (;;) {
        vTaskDelay(pdMS_TO_TICKS(1000));
        uint32_t now = uptime_s();
        if (wd_feed_ok(now, s_last_tick_s, s_send_active, s_send_since_s, s_ot_seen)) esp_task_wdt_reset();
        if (!s_probe_pending && now - probe_s >= OT_PROBE_S && otInstanceIsInitialized(esp_openthread_get_instance())) {
            s_probe_pending = true;
            probe_s = now;
            if (esp_openthread_task_queue_post(ot_probe, NULL) != ESP_OK) s_probe_pending = false;
        }
        bool ip = sta_has_ip();
        if (xSemaphoreTake(s_lock, pdMS_TO_TICKS(500)) != pdTRUE) continue;
        wd_conn_update(&s_conn, ip, now);
        bool go = wd_wifi_restart_ok(&s_conn, &s_state, now, CONN_BASE_S);
        if (go) {
            if (s_conn.level < WD_CONN_LEVELS - 1) s_conn.level++;
            s_conn.since_s = now;
            s_wifi_restarts++;
        }
        rtc_store_locked(now);
        xSemaphoreGive(s_lock);
        if (go) {
            ESP_LOGW(TAG, "no IP (wifi reason %u): WiFi restart #%u, Thread keeps running", (unsigned)s_disc_reason, (unsigned)s_wifi_restarts);
            wifi_restart();
        }
    }
}

void br_watchdog_start(void)
{
    wd_config_t cfg;
    s_lock = xSemaphoreCreateMutex();
    load_nvs(&cfg);
    wd_state_init(&s_state, &cfg);
    uint32_t boot_s = uptime_s();
    if (rtc_load()) wd_state_restore(&s_state, &s_rtc, boot_s, reset_crashed(esp_reset_reason()));
    wd_conn_init(&s_conn, s_rtc.conn_level < WD_CONN_LEVELS ? s_rtc.conn_level : 0, boot_s);
    s_last_tick_s = boot_s;
#if CONFIG_EXAMPLE_CONNECT_WIFI
    esp_event_handler_register(WIFI_EVENT, WIFI_EVENT_STA_DISCONNECTED, net_event, NULL);
#endif
    temperature_sensor_config_t tc = TEMPERATURE_SENSOR_CONFIG_DEFAULT(-10, 80);
    if (temperature_sensor_install(&tc, &s_tsens) != ESP_OK || temperature_sensor_enable(s_tsens) != ESP_OK) {
        ESP_LOGE(TAG, "temperature sensor unavailable");
        s_tsens = NULL;
    }
    if (strlen(CONFIG_BR_WATCHDOG_TOKEN) == 0) ESP_LOGE(TAG, "BR_WATCHDOG_TOKEN empty: all watchdog writes are rejected");
    start_http();
    xTaskCreate(watchdog_task, "br_watchdog", WD_TASK_STACK, NULL, 4, NULL);
    xTaskCreate(validate_task, "ota_validate", 3072, NULL, 3, NULL);
    xTaskCreate(supervisor_task, "br_supervisor", SUP_TASK_STACK, NULL, 6, &s_sup_task);
    ESP_LOGI(TAG, "started: mode %s, timeout %us, creds %s, reset %s%s, hb age %us", cfg.armed ? "armed" : "dry_run", (unsigned)cfg.timeout_s, s_have_creds ? "present" : "absent", s_diag.reset_name, s_diag.restored ? " (state restored)" : "", (unsigned)wd_age_s(&s_state, boot_s));
}
