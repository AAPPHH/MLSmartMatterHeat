#include <math.h>
#include <stdio.h>
#include <string.h>
#include "../main/watchdog_core.h"
#include "cJSON.h"

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

static wd_state_t fresh(bool armed)
{
    wd_config_t c;
    wd_config_defaults(&c);
    c.armed = armed;
    wd_state_t st;
    wd_state_init(&st, &c);
    return st;
}

static void test_defaults_and_token(void)
{
    wd_config_t c;
    wd_config_defaults(&c);
    CHECK(wd_config_valid(&c));
    CHECK(c.timeout_s == 600 && c.setpoint_cc == 1800 && c.node_count == 4 && c.nodes[0] == 5 && c.nodes[3] == 8 && c.endpoint == 1 && !c.armed);
    CHECK(wd_token_ok("s3cret", "s3cret"));
    CHECK(!wd_token_ok("s3cret", "s3cre"));
    CHECK(!wd_token_ok("s3cret", "s3cretx"));
    CHECK(!wd_token_ok("s3cret", "S3cret"));
    CHECK(!wd_token_ok("s3cret", NULL));
    CHECK(!wd_token_ok("s3cret", ""));
    CHECK(!wd_token_ok("", ""));
    CHECK(!wd_token_ok(NULL, "x"));
}

static void test_parse_heartbeat(void)
{
    wd_config_t c;
    wd_config_defaults(&c);
    CHECK(wd_parse_heartbeat("{\"ts\":1730000000000,\"timeout_s\":600,\"setpoint_c\":18.0,\"nodes\":[5,6,7,8],\"endpoint\":1}", &c) == WD_OK);
    CHECK(c.timeout_s == 600 && c.setpoint_cc == 1800 && c.node_count == 4 && c.endpoint == 1);
    CHECK(wd_parse_heartbeat("{\"timeout_s\":900,\"setpoint_c\":17.5,\"nodes\":[5,9],\"endpoint\":2}", &c) == WD_OK);
    CHECK(c.timeout_s == 900 && c.setpoint_cc == 1750 && c.node_count == 2 && c.nodes[1] == 9 && c.endpoint == 2);
    wd_config_t keep = c;
    CHECK(wd_parse_heartbeat("{\"ts\":1}", &c) == WD_OK);
    CHECK(wd_config_equal(&c, &keep));
    CHECK(wd_parse_heartbeat("not json", &c) == WD_E_JSON);
    CHECK(wd_parse_heartbeat("[1,2]", &c) == WD_E_JSON);
    CHECK(wd_parse_heartbeat("{\"timeout_s\":\"600\"}", &c) == WD_E_FIELD);
    CHECK(wd_parse_heartbeat("{\"timeout_s\":10}", &c) == WD_E_RANGE);
    CHECK(wd_parse_heartbeat("{\"timeout_s\":999999}", &c) == WD_E_RANGE);
    CHECK(wd_parse_heartbeat("{\"setpoint_c\":40}", &c) == WD_E_RANGE);
    CHECK(wd_parse_heartbeat("{\"setpoint_c\":1}", &c) == WD_E_RANGE);
    CHECK(wd_parse_heartbeat("{\"nodes\":[]}", &c) == WD_E_RANGE);
    CHECK(wd_parse_heartbeat("{\"nodes\":[1,2,3,4,5,6,7,8,9]}", &c) == WD_E_RANGE);
    CHECK(wd_parse_heartbeat("{\"nodes\":[0]}", &c) == WD_E_RANGE);
    CHECK(wd_parse_heartbeat("{\"nodes\":[\"5\"]}", &c) == WD_E_FIELD);
    CHECK(wd_parse_heartbeat("{\"nodes\":5}", &c) == WD_E_FIELD);
    CHECK(wd_parse_heartbeat("{\"endpoint\":0}", &c) == WD_E_RANGE);
    CHECK(wd_parse_heartbeat("{\"timeout_s\":600,\"setpoint_c\":99}", &c) == WD_E_RANGE);
    CHECK(wd_config_equal(&c, &keep));
}

static void test_parse_mode_and_hex(void)
{
    bool armed = false;
    CHECK(wd_parse_mode("{\"mode\":\"armed\"}", &armed) == WD_OK && armed);
    CHECK(wd_parse_mode("{\"mode\":\"dry_run\"}", &armed) == WD_OK && !armed);
    CHECK(wd_parse_mode("{\"mode\":\"live\"}", &armed) == WD_E_RANGE);
    CHECK(wd_parse_mode("{\"mode\":1}", &armed) == WD_E_FIELD);
    CHECK(wd_parse_mode("{", &armed) == WD_E_JSON);
    uint8_t b[4];
    size_t n = 0;
    CHECK(wd_hex_decode("0aFf10", b, 4, &n) == 0 && n == 3 && b[0] == 0x0a && b[1] == 0xff && b[2] == 0x10);
    CHECK(wd_hex_decode("abc", b, 4, &n) != 0);
    CHECK(wd_hex_decode("zz", b, 4, &n) != 0);
    CHECK(wd_hex_decode("0102030405", b, 4, &n) != 0);
}

static void hexstr(char *dst, size_t bytes, char c)
{
    for (size_t i = 0; i < bytes * 2; i++) dst[i] = c;
    dst[bytes * 2] = '\0';
}

static void test_parse_credentials(void)
{
    static char key[WD_KEY_LEN * 2 + 1], ipk[WD_IPK_LEN * 2 + 1], cert[201], body[2048];
    hexstr(key, WD_KEY_LEN, 'a');
    hexstr(ipk, WD_IPK_LEN, '1');
    hexstr(cert, 100, 'b');
    snprintf(body, sizeof(body), "{\"fabric_id\":\"0x1\",\"node_id\":\"1B2D0001\",\"rcac\":\"%s\",\"icac\":\"%s\",\"noc\":\"%s\",\"key\":\"%s\",\"ipk\":\"%s\"}", cert, cert, cert, key, ipk);
    static wd_creds_t cr;
    CHECK(wd_parse_credentials(body, &cr) == WD_OK);
    CHECK(cr.fabric_id == 1 && cr.node_id == 0x1B2D0001ULL && cr.rcac_len == 100 && cr.icac_len == 100 && cr.noc_len == 100);
    CHECK(cr.key[0] == 0xaa && cr.ipk[15] == 0x11);
    snprintf(body, sizeof(body), "{\"fabric_id\":\"1\",\"node_id\":\"1B2D0001\",\"rcac\":\"%s\",\"noc\":\"%s\",\"key\":\"%s\",\"ipk\":\"%s\"}", cert, cert, key, ipk);
    CHECK(wd_parse_credentials(body, &cr) == WD_OK && cr.icac_len == 0);
    snprintf(body, sizeof(body), "{\"fabric_id\":\"1\",\"node_id\":\"1B2D0001\",\"rcac\":\"%s\",\"key\":\"%s\",\"ipk\":\"%s\"}", cert, key, ipk);
    CHECK(wd_parse_credentials(body, &cr) == WD_E_FIELD);
    hexstr(ipk, 8, '1');
    snprintf(body, sizeof(body), "{\"fabric_id\":\"1\",\"node_id\":\"1B2D0001\",\"rcac\":\"%s\",\"noc\":\"%s\",\"key\":\"%s\",\"ipk\":\"%s\"}", cert, cert, key, ipk);
    CHECK(wd_parse_credentials(body, &cr) == WD_E_FIELD);
    CHECK(wd_parse_credentials("nope", &cr) == WD_E_JSON);
}

static void test_no_trigger_with_heartbeats(void)
{
    wd_state_t st = fresh(true);
    for (uint32_t t = 0; t < 5000; t++) {
        if (t % 30 == 5) wd_on_heartbeat(&st, t);
        CHECK(wd_tick(&st, t).kind == WD_ACT_NONE);
    }
    CHECK(!st.triggered);
}

static void test_boot_trigger_dry_run(void)
{
    wd_state_t st = fresh(false);
    CHECK(wd_tick(&st, 599).kind == WD_ACT_NONE);
    wd_action_t a = wd_tick(&st, 600);
    CHECK(a.kind == WD_ACT_TRIGGERED && st.triggered && st.triggered_at_s == 600);
    for (uint32_t t = 601; t < 4000; t += 7) CHECK(wd_tick(&st, t).kind == WD_ACT_NONE);
    CHECK(st.triggered);
}

static void test_armed_sends_each_node_once_then_rearm(void)
{
    wd_state_t st = fresh(true);
    CHECK(wd_tick(&st, 600).kind == WD_ACT_TRIGGERED);
    bool seen[4] = {0};
    for (int k = 0; k < 4; k++) {
        wd_action_t a = wd_tick(&st, 601);
        CHECK(a.kind == WD_ACT_SEND && a.node_idx >= 0 && a.node_idx < 4);
        CHECK(!seen[a.node_idx]);
        seen[a.node_idx] = true;
        wd_report(&st, a.node_idx, true, NULL, 601);
    }
    CHECK(wd_tick(&st, 602).kind == WD_ACT_NONE);
    CHECK(wd_tick(&st, 5000).kind == WD_ACT_NONE);
    char buf[512];
    wd_status_json(&st, 5000, true, 41.5, "v1", true, NULL, buf, sizeof(buf));
    CHECK(strstr(buf, "\"triggered\":true") && strstr(buf, "\"node\":5,\"ok\":true,\"err\":null"));
    CHECK(wd_on_heartbeat(&st, 5001));
    CHECK(!st.triggered);
    CHECK(wd_tick(&st, 5002).kind == WD_ACT_NONE);
    CHECK(!wd_on_heartbeat(&st, 5003));
    CHECK(wd_tick(&st, 5003 + 600).kind == WD_ACT_TRIGGERED);
    CHECK(wd_tick(&st, 5003 + 601).kind == WD_ACT_SEND);
}

static void test_retry_backoff_and_giveup(void)
{
    wd_state_t st = fresh(true);
    st.cfg.node_count = 1;
    CHECK(wd_tick(&st, 600).kind == WD_ACT_TRIGGERED);
    uint32_t t = 600, last = 0, tries = 0;
    uint32_t gaps[40];
    while (t < 600 + WD_RETRY_WINDOW_S + 400) {
        wd_action_t a = wd_tick(&st, t);
        if (a.kind == WD_ACT_SEND) {
            if (tries) gaps[tries - 1] = t - last;
            last = t;
            tries++;
            wd_report(&st, 0, false, "timeout: no/resp", t);
        }
        t++;
    }
    CHECK(tries >= 8 && tries <= 12);
    CHECK(gaps[0] == 30 && gaps[1] == 60 && gaps[2] == 120 && gaps[3] == 240 && gaps[4] == WD_RETRY_MAX_DELAY_S);
    CHECK(last <= 600 + WD_RETRY_WINDOW_S);
    CHECK(st.node_state[0] == WD_NODE_GAVE_UP);
    char buf[512];
    wd_status_json(&st, t, false, 0, "v1", true, NULL, buf, sizeof(buf));
    CHECK(strstr(buf, "\"ok\":false,\"err\":\"timeout__no_resp\"") && strstr(buf, "\"die_temperature_c\":null"));
}

static void test_retry_stops_on_heartbeat(void)
{
    wd_state_t st = fresh(true);
    wd_tick(&st, 600);
    wd_action_t a = wd_tick(&st, 601);
    CHECK(a.kind == WD_ACT_SEND);
    wd_on_heartbeat(&st, 602);
    wd_report(&st, a.node_idx, false, "late", 603);
    CHECK(wd_tick(&st, 700).kind == WD_ACT_NONE);
    CHECK(wd_tick(&st, 1000).kind == WD_ACT_NONE);
}

static void test_dry_to_armed_during_outage(void)
{
    wd_state_t st = fresh(false);
    wd_tick(&st, 600);
    CHECK(wd_tick(&st, 3000).kind == WD_ACT_NONE);
    wd_set_armed(&st, true, 3000);
    wd_action_t a = wd_tick(&st, 3000);
    CHECK(a.kind == WD_ACT_SEND);
    wd_report(&st, a.node_idx, true, NULL, 3000);
    wd_set_armed(&st, false, 3010);
    CHECK(wd_tick(&st, 3011).kind == WD_ACT_NONE);
}

static void test_status_json_shape(void)
{
    wd_state_t st = fresh(false);
    char buf[512];
    size_t n = wd_status_json(&st, 12, true, 38.46, "fw1", false, NULL, buf, sizeof(buf));
    CHECK(n == strlen(buf));
    CHECK(strstr(buf, "\"mode\":\"dry_run\"") && strstr(buf, "\"triggered\":false") && strstr(buf, "\"triggered_at_uptime_s\":null"));
    CHECK(strstr(buf, "\"results\":[]") && strstr(buf, "\"die_temperature_c\":38.5") && strstr(buf, "\"uptime_s\":12") && strstr(buf, "\"fw\":\"fw1\"") && strstr(buf, "\"timeout_s\":600"));
    char small[16];
    wd_status_json(&st, 12, true, 38.46, "fw1", false, NULL, small, sizeof(small));
    CHECK(strlen(small) == 15);
    n = wd_heartbeat_json(&st, 30, buf, sizeof(buf));
    CHECK(n == strlen(buf) && strcmp(buf, "{\"ok\":true,\"mode\":\"dry_run\",\"age_s\":30}") == 0);
}

static void test_status_json_diag(void)
{
    wd_state_t st = fresh(true);
    st.triggered = true;
    for (int i = 0; i < st.cfg.node_count; i++) st.node_tried[i] = 1;
    wd_diag_t dg = {"task_wdt", 6, 3, 2, 8, true, 77, true};
    char buf[2048];
    size_t n = wd_status_json(&st, 900, true, 40.0, "fw1-long-version-string-0123456789", true, &dg, buf, sizeof(buf));
    CHECK(n == strlen(buf) && n < 1200);
    CHECK(strstr(buf, "\"reset_reason\":\"task_wdt\"") && strstr(buf, "\"reset_reason_code\":6") && strstr(buf, "\"wifi_restarts\":3") && strstr(buf, "\"wifi_disc_reason\":8"));
    CHECK(strstr(buf, "\"state_restored\":true") && strstr(buf, "\"wifi_ip\":true") && strstr(buf, "\"noip_s\":77") && strstr(buf, "\"conn_level\":2"));
    dg.disc_reason = 0;
    wd_status_json(&st, 900, false, 0, "fw1", true, &dg, buf, sizeof(buf));
    CHECK(strstr(buf, "\"wifi_disc_reason\":null") && strstr(buf, "\"die_temperature_c\":null"));
}

static void test_restore_keeps_timer_across_reboots(void)
{
    wd_state_t st = fresh(true);
    wd_conn_t c;
    wd_conn_init(&c, 0, 0);
    wd_on_heartbeat(&st, 10);
    wd_rtc_t r;
    wd_rtc_pack(&r, &st, 400, WD_NO_WALL, &c, 0, 0);
    CHECK(wd_rtc_valid(&r) && r.hb_age_s == 390 && !r.triggered);
    wd_state_t st2 = fresh(true);
    wd_state_restore(&st2, &r, 15, WD_NO_WALL, false);
    CHECK(wd_age_s(&st2, 15) == 390 + WD_REBOOT_GAP_S);
    CHECK(wd_tick(&st2, 15).kind == WD_ACT_NONE);
    CHECK(wd_tick(&st2, 15 + 600 - 390 - WD_REBOOT_GAP_S).kind == WD_ACT_TRIGGERED);
    wd_state_t cur = fresh(true);
    wd_state_restore(&cur, &r, 15, WD_NO_WALL, false);
    for (int k = 0; k < 40 && wd_age_s(&cur, 35) < 600; k++) {
        wd_rtc_pack(&r, &cur, 35, WD_NO_WALL, &c, 0, 0);
        cur = fresh(true);
        wd_state_restore(&cur, &r, 15, WD_NO_WALL, false);
    }
    CHECK(wd_age_s(&cur, 35) >= 600);
    CHECK(wd_tick(&cur, 35).kind == WD_ACT_TRIGGERED);
    wd_on_heartbeat(&cur, 40);
    CHECK(wd_age_s(&cur, 40) == 0 && cur.hb_carry_s == 0);
}

static void test_restore_triggered_resumes_window(void)
{
    wd_state_t st = fresh(true);
    wd_conn_t c;
    wd_conn_init(&c, 0, 0);
    wd_tick(&st, 600);
    wd_action_t a = wd_tick(&st, 600);
    CHECK(a.kind == WD_ACT_SEND);
    wd_report(&st, a.node_idx, false, "timeout", 600);
    wd_rtc_t r;
    wd_rtc_pack(&r, &st, 1500, WD_NO_WALL, &c, 1, 8);
    CHECK(r.triggered && r.trig_age_s == 900 && r.disc_reason == 8 && r.wifi_restarts == 1);
    wd_state_t st2 = fresh(true);
    wd_state_restore(&st2, &r, 20, WD_NO_WALL, false);
    CHECK(st2.triggered && wd_since_trigger_s(&st2, 20) == 900 + WD_REBOOT_GAP_S);
    CHECK(wd_tick(&st2, 20).kind == WD_ACT_SEND);
    CHECK(wd_tick(&st2, 20 + WD_RETRY_WINDOW_S - 900 - WD_REBOOT_GAP_S + 1).kind == WD_ACT_NONE);
    CHECK(st2.node_state[0] == WD_NODE_GAVE_UP);
    wd_on_heartbeat(&st2, 5000);
    CHECK(!st2.triggered && st2.trig_carry_s == 0 && st2.node_state[0] == WD_NODE_PENDING);
}

static void test_rtc_validation(void)
{
    wd_state_t st = fresh(false);
    wd_conn_t c;
    wd_conn_init(&c, 1, 0);
    wd_rtc_t r;
    wd_rtc_pack(&r, &st, 50, WD_NO_WALL, &c, 2, 0);
    CHECK(wd_rtc_valid(&r) && r.conn_level == 1);
    r.hb_age_s ^= 1;
    CHECK(!wd_rtc_valid(&r));
    wd_rtc_t z;
    memset(&z, 0, sizeof(z));
    CHECK(!wd_rtc_valid(&z));
    memset(&z, 0xA5, sizeof(z));
    CHECK(!wd_rtc_valid(&z));
}

static void test_conn_backoff(void)
{
    wd_conn_t c;
    uint32_t base = 900;
    CHECK(wd_conn_limit_s(0, base) == 900 && wd_conn_limit_s(1, base) == 3600 && wd_conn_limit_s(2, base) == 21600 && wd_conn_limit_s(200, base) == 21600);
    wd_conn_init(&c, 0, 100);
    CHECK(!wd_conn_due(&c, 100 + 899, base) && wd_conn_due(&c, 100 + 900, base));
    wd_conn_update(&c, false, 500);
    CHECK(c.since_s == 100);
    wd_conn_init(&c, 1, 5);
    CHECK(!wd_conn_due(&c, 5 + 3599, base) && wd_conn_due(&c, 5 + 3600, base));
    wd_conn_init(&c, 2, 5);
    CHECK(!wd_conn_due(&c, 5 + 21599, base) && wd_conn_due(&c, 5 + 21600, base));
    wd_conn_update(&c, true, 1000);
    CHECK(c.has_ip && !wd_conn_due(&c, 1000000, base) && c.level == 2);
    wd_conn_update(&c, true, 1000 + WD_CONN_STABLE_S - 1);
    CHECK(c.level == 2);
    wd_conn_update(&c, true, 1000 + WD_CONN_STABLE_S);
    CHECK(c.level == 0);
    wd_conn_init(&c, 2, 0);
    wd_conn_update(&c, true, 100);
    wd_conn_update(&c, false, 200);
    CHECK(c.level == 2 && c.since_s == 200 && !wd_conn_due(&c, 200 + 900, base));
}

static void test_long_outage_no_reboot_loop(void)
{
    wd_state_t st = fresh(false);
    wd_conn_t c;
    uint32_t base = 900, boots = 0, up = 0, total = 0, horizon = 48 * 3600, first[3] = {0, 0, 0};
    wd_conn_init(&c, 0, 0);
    while (total < horizon) {
        total++;
        up++;
        if (!wd_wifi_restart_ok(&c, &st, up, base)) continue;
        if (boots < 3) first[boots] = total;
        boots++;
        wd_conn_init(&c, c.level + 1, 0);
        up = 0;
    }
    CHECK(first[0] == 900 && first[1] == 900 + 3600 && first[2] == 900 + 3600 + 21600);
    CHECK(boots == 2 + (horizon - 900 - 3600) / 21600);
}

static void test_heartbeat_vetoes_wifi_restart(void)
{
    wd_state_t st = fresh(true);
    wd_conn_t c;
    uint32_t base = 900;
    wd_conn_init(&c, 0, 0);
    wd_on_heartbeat(&st, 1000);
    CHECK(!wd_wifi_restart_ok(&c, &st, 1000 + WD_HB_VETO_S - 1, base));
    CHECK(wd_wifi_restart_ok(&c, &st, 1000 + WD_HB_VETO_S, base));
    CHECK(!wd_wifi_restart_ok(&c, &st, 1000 + WD_HB_VETO_S, 0));
    wd_conn_update(&c, true, 1100);
    CHECK(!wd_wifi_restart_ok(&c, &st, 100000, base));
    wd_state_t armed_triggered = fresh(true);
    wd_conn_t late;
    wd_conn_init(&late, 0, 0);
    wd_tick(&armed_triggered, 600);
    CHECK(armed_triggered.triggered && wd_wifi_restart_ok(&late, &armed_triggered, 1000, base));
}

static void test_restore_keeps_ok_nodes(void)
{
    wd_state_t st = fresh(true);
    wd_conn_t c;
    wd_conn_init(&c, 0, 0);
    wd_tick(&st, 600);
    for (int i = 0; i < 2; i++) {
        wd_action_t a = wd_tick(&st, 600);
        CHECK(a.kind == WD_ACT_SEND);
        wd_report(&st, a.node_idx, true, NULL, 600);
    }
    wd_report(&st, 2, false, "timeout", 600);
    wd_rtc_t r;
    wd_rtc_pack(&r, &st, 700, WD_NO_WALL, &c, 0, 0);
    CHECK(r.node_ok == 3);
    wd_state_t st2 = fresh(true);
    wd_state_restore(&st2, &r, 10, WD_NO_WALL, false);
    CHECK(st2.node_state[0] == WD_NODE_OK && st2.node_state[1] == WD_NODE_OK && st2.node_state[2] == WD_NODE_PENDING && st2.node_state[3] == WD_NODE_PENDING);
    wd_action_t a = wd_tick(&st2, 10);
    CHECK(a.kind == WD_ACT_SEND && a.node_idx == 2);
    wd_report(&st2, 2, true, NULL, 10);
    a = wd_tick(&st2, 10);
    CHECK(a.kind == WD_ACT_SEND && a.node_idx == 3);
    wd_report(&st2, 3, true, NULL, 10);
    CHECK(wd_tick(&st2, 11).kind == WD_ACT_NONE);
    wd_state_t st3 = fresh(true);
    st3.cfg.nodes[0] = 99;
    wd_state_restore(&st3, &r, 10, WD_NO_WALL, false);
    CHECK(st3.node_state[0] == WD_NODE_PENDING && st3.node_state[1] == WD_NODE_PENDING);
    wd_state_t quiet = fresh(true);
    wd_rtc_pack(&r, &quiet, 50, WD_NO_WALL, &c, 0, 0);
    CHECK(r.node_ok == 0 && !r.triggered);
}

static void test_armed_trigger_survives_reboot_loop(void)
{
    wd_state_t st = fresh(true);
    wd_conn_t c;
    wd_rtc_t r;
    wd_conn_init(&c, 0, 0);
    wd_on_heartbeat(&st, 0);
    uint32_t sent = 0;
    for (int boot = 0; boot < 60 && !sent; boot++) {
        wd_rtc_pack(&r, &st, 60, WD_NO_WALL, &c, 0, 0);
        st = fresh(true);
        wd_state_restore(&st, &r, 20, WD_NO_WALL, false);
        for (uint32_t t = 20; t < 80 && !sent; t++) {
            wd_action_t a = wd_tick(&st, t);
            if (a.kind == WD_ACT_SEND) sent = 1;
        }
    }
    CHECK(sent);
}

static void test_feed_ok(void)
{
    CHECK(wd_feed_ok(100, 99, false, 0, true));
    CHECK(wd_feed_ok(130, 100, false, 0, true));
    CHECK(!wd_feed_ok(131, 100, false, 0, true));
    CHECK(wd_feed_ok(1000, 100, true, 100, true));
    CHECK(wd_feed_ok(100 + WD_SEND_MAX_S, 100, true, 100, true));
    CHECK(!wd_feed_ok(101 + WD_SEND_MAX_S, 100, true, 100, true));
    CHECK(wd_feed_ok(WD_OT_BOOT_S, WD_OT_BOOT_S, false, 0, false));
    CHECK(!wd_feed_ok(WD_OT_BOOT_S + 1, WD_OT_BOOT_S + 1, false, 0, false));
}

static void test_u64_str(void)
{
    char b[24];
    CHECK(wd_u64_str(0, b, sizeof(b)) == 1 && !strcmp(b, "0"));
    CHECK(wd_u64_str(8, b, sizeof(b)) == 1 && !strcmp(b, "8"));
    CHECK(wd_u64_str(UINT64_MAX, b, sizeof(b)) == 20 && !strcmp(b, "18446744073709551615"));
    CHECK(wd_u64_str(9007199254740991ull, b, 6) == 16 && !strcmp(b, "90071"));
    wd_state_t st = fresh(true);
    st.cfg.nodes[0] = 9007199254740991ull;
    wd_tick(&st, 600);
    wd_report(&st, 0, false, "timeout", 600);
    char buf[512];
    wd_status_json(&st, 601, false, 0, "v", true, NULL, buf, sizeof(buf));
    CHECK(strstr(buf, "{\"node\":9007199254740991,\"ok\":false,\"err\":\"timeout\"}"));
}

static void reboot(wd_state_t *st, const wd_conn_t *c, uint32_t up, uint32_t wall, uint32_t down_s, bool armed, bool abnormal)
{
    wd_rtc_t r;
    wd_rtc_pack(&r, st, up, wall, c, 0, 0);
    CHECK(wd_rtc_valid(&r));
    *st = fresh(armed);
    wd_state_restore(st, &r, 5, wall + down_s, abnormal);
}

static wd_state_t triggered_sending(wd_conn_t *c, int *node)
{
    wd_state_t st = fresh(true);
    wd_conn_init(c, 0, 0);
    wd_tick(&st, 600);
    wd_action_t a = wd_tick(&st, 600);
    CHECK(a.kind == WD_ACT_SEND && st.sending);
    *node = a.node_idx;
    return st;
}

static void test_crash_isolates_node(void)
{
    wd_conn_t c;
    int n;
    wd_state_t st = triggered_sending(&c, &n);
    CHECK(n == 0);
    reboot(&st, &c, 610, 1000, 3, true, true);
    CHECK(st.node_crashes[0] == 1 && st.node_state[0] == WD_NODE_PENDING && !strcmp(st.node_err[0], "send_crash"));
    bool seen[4] = {false, false, false, false};
    for (uint32_t t = 5; t < 9; t++) {
        wd_action_t a = wd_tick(&st, t);
        if (a.kind == WD_ACT_SEND) {
            seen[a.node_idx] = true;
            wd_report(&st, a.node_idx, true, NULL, t);
        }
    }
    CHECK(!seen[0] && seen[1] && seen[2] && seen[3]);
    reboot(&st, &c, 8, 2000, 3, true, false);
    CHECK(st.node_crashes[0] == 1 && st.node_state[1] == WD_NODE_OK && st.node_state[3] == WD_NODE_OK);
    CHECK(wd_tick(&st, 5 + 113).kind == WD_ACT_NONE);
    wd_action_t a = wd_tick(&st, 5 + 114);
    CHECK(a.kind == WD_ACT_SEND && a.node_idx == 0);
    reboot(&st, &c, 119, 3000, 3, true, true);
    CHECK(st.node_crashes[0] == WD_CRASH_MAX && st.node_state[0] == WD_NODE_GAVE_UP && !strcmp(st.node_err[0], "send_crash"));
    for (int i = 1; i < 4; i++) CHECK(st.node_state[i] == WD_NODE_OK);
    CHECK(wd_tick(&st, 10000).kind == WD_ACT_NONE);
    char buf[1024];
    wd_diag_t dg = {"panic", 4, 0, 0, 0, false, 0, true};
    wd_status_json(&st, 30, false, 0, "v", true, &dg, buf, sizeof(buf));
    CHECK(strstr(buf, "\"err\":\"send_crash\"") && strstr(buf, "\"send_crashes\":2") && strstr(buf, "\"ok\":false"));
    wd_on_heartbeat(&st, 6000);
    CHECK(!st.triggered && st.node_crashes[0] == 0 && st.global_crashes == 0);
}

static void test_crash_count_decays_after_report(void)
{
    wd_conn_t c;
    int n;
    wd_state_t st = triggered_sending(&c, &n);
    reboot(&st, &c, 610, 1000, 3, true, true);
    CHECK(st.node_crashes[0] == 1);
    for (int i = 1; i < 4; i++) wd_report(&st, i, true, NULL, 5);
    wd_action_t a = wd_tick(&st, 5 + WD_CRASH_BACKOFF_S);
    CHECK(a.kind == WD_ACT_SEND && a.node_idx == 0);
    wd_report(&st, 0, false, "timeout", 5 + WD_CRASH_BACKOFF_S);
    CHECK(st.node_crashes[0] == 0);
    reboot(&st, &c, 200, 2000, 3, true, true);
    CHECK(st.node_crashes[0] == 1 && st.node_state[0] == WD_NODE_PENDING);
}

static void test_crash_attribution_window(void)
{
    wd_conn_t c;
    int n;
    wd_state_t st = fresh(true);
    wd_conn_init(&c, 0, 0);
    wd_tick(&st, 600);
    reboot(&st, &c, 610, 1000, 3, true, true);
    CHECK(st.global_crashes == 1);
    for (int i = 0; i < 4; i++) CHECK(st.node_crashes[i] == 0);
    CHECK(wd_tick(&st, 5).kind == WD_ACT_SEND);
    st = triggered_sending(&c, &n);
    wd_report(&st, 0, false, "timeout", 605);
    reboot(&st, &c, 620, 1000, 3, true, true);
    CHECK(st.node_crashes[0] == 1 && st.global_crashes == 1);
    st = triggered_sending(&c, &n);
    wd_report(&st, 0, false, "timeout", 605);
    reboot(&st, &c, 605 + WD_ATTRIB_S + 1, 1000, 3, true, true);
    CHECK(st.node_crashes[0] == 0 && st.global_crashes == 1);
}

static void test_one_send_charged_once(void)
{
    wd_conn_t c;
    int n;
    wd_state_t st = triggered_sending(&c, &n);
    wd_report(&st, 0, false, "no_address", 605);
    reboot(&st, &c, 630, 1000, 3, true, true);
    CHECK(st.node_crashes[0] == 1 && st.node_state[0] == WD_NODE_PENDING);
    reboot(&st, &c, 30, 1100, 3, true, true);
    CHECK(st.node_crashes[0] == 1 && st.node_state[0] == WD_NODE_PENDING && st.global_crashes == 2);
}

static void test_normal_reboot_adds_no_count(void)
{
    wd_conn_t c;
    int n;
    wd_state_t st = triggered_sending(&c, &n);
    reboot(&st, &c, 610, 1000, 3, true, false);
    CHECK(st.global_crashes == 0 && st.node_crashes[0] == 0 && st.node_state[0] == WD_NODE_PENDING);
    CHECK(wd_tick(&st, 5).kind == WD_ACT_SEND);
    reboot(&st, &c, 20, 1100, 3, true, false);
    CHECK(st.global_crashes == 0 && st.node_crashes[0] == 0 && wd_tick(&st, 5).kind == WD_ACT_SEND);
}

static void test_global_crash_backoff_persists(void)
{
    wd_conn_t c;
    wd_conn_init(&c, 0, 0);
    wd_state_t st = fresh(true);
    wd_tick(&st, 600);
    uint32_t wall = 1000;
    for (int i = 0; i < WD_GLOBAL_CRASH_MAX; i++) {
        reboot(&st, &c, 5, wall, 3, true, true);
        wall += 8;
    }
    CHECK(st.global_crashes == WD_GLOBAL_CRASH_MAX);
    CHECK(wd_tick(&st, 5).kind == WD_ACT_NONE);
    reboot(&st, &c, 5, wall, 3, true, false);
    wall += 8;
    CHECK(wd_tick(&st, 5).kind == WD_ACT_NONE && wd_tick(&st, 5 + WD_GLOBAL_BACKOFF_S - 30).kind == WD_ACT_NONE);
    wd_action_t a = wd_tick(&st, 5 + WD_GLOBAL_BACKOFF_S);
    CHECK(a.kind == WD_ACT_SEND);
    wd_report(&st, a.node_idx, true, NULL, 5 + WD_GLOBAL_BACKOFF_S);
    CHECK(st.global_crashes == 0 && wd_tick(&st, 5 + WD_GLOBAL_BACKOFF_S).kind == WD_ACT_SEND);
}

static void test_boot_loop_post_send_bounded(void)
{
    wd_conn_t c;
    wd_conn_init(&c, 0, 0);
    wd_state_t st = fresh(true);
    uint32_t base = 0;
    int sends = 0;
    wd_tick(&st, 600);
    for (uint32_t t = 600; t < 600 + WD_RETRY_WINDOW_S + 200; t++) {
        uint32_t u = t - base;
        wd_action_t a = wd_tick(&st, u);
        if (a.kind != WD_ACT_SEND) continue;
        sends++;
        wd_report(&st, a.node_idx, false, "timeout", u);
        wd_rtc_t r;
        wd_rtc_pack(&r, &st, u + 1, t + 1, &c, 0, 0);
        st = fresh(true);
        wd_state_restore(&st, &r, 5, t + 5, true);
        base = t + 5 - 5;
        t += 4;
    }
    CHECK(sends >= 5 && sends <= WD_GLOBAL_CRASH_MAX + WD_RETRY_WINDOW_S / WD_GLOBAL_BACKOFF_S + 1);
}

static void test_dry_run_never_sends_after_restore(void)
{
    wd_conn_t c;
    int n;
    wd_state_t st = triggered_sending(&c, &n);
    for (int k = 0; k < 6; k++) {
        reboot(&st, &c, 10, 1000 + 20 * k, 3, false, true);
        CHECK(st.global_crashes == 0 && st.node_crashes[0] == 0);
        for (uint32_t t = 5; t < 5 + 3 * WD_RETRY_WINDOW_S; t += 50) CHECK(wd_tick(&st, t).kind == WD_ACT_NONE);
    }
}

static void test_blackbox(void)
{
    wd_trace_t t;
    memset(&t, 0xA5, sizeof(t));
    CHECK(!wd_trace_valid(&t));
    wd_trace_mark(&t, WD_ST_TX, 5, 100);
    CHECK(!wd_trace_valid(&t));
    wd_trace_begin(&t, 8, 1000);
    CHECK(wd_trace_valid(&t) && t.seq == 1 && t.stage == WD_ST_START);
    wd_trace_mark(&t, WD_ST_LOOKUP, 1010, 9000);
    wd_trace_mark(&t, WD_ST_TX, 1500, 4000);
    wd_trace_mark(&t, WD_ST_RXWAIT, 1501, 8000);
    wd_trace_mark(&t, WD_ST_CRYPTO, 1900, 3000);
    wd_trace_mark(&t, WD_ST_TX, 2400, 5000);
    CHECK(wd_trace_valid(&t) && t.tx == 2 && t.rx == 1 && t.stack_min == 3000 && t.stage_ms[WD_ST_TX] == 1400);
    wd_panic_t p;
    memset(&p, 0, sizeof(p));
    CHECK(!wd_panic_valid(&p));
    p.count = 2;
    p.exception = 4;
    p.cause = 28;
    p.pc = 0x42009555;
    p.a0 = 0x4200bbbb;
    p.bt[0] = 0x42009555;
    p.bt[1] = 0x4200aaaa;
    wd_append_clean(p.task, sizeof(p.task), "br_watch");
    wd_append_clean(p.task, sizeof(p.task), "dog\"x\n-and-more-than-fifteen");
    wd_append_clean(p.twdt, sizeof(p.twdt), "\n - br_supervisor (CPU 0)");
    CHECK(!strcmp(p.task, "br_watchdog_x_-"));
    wd_panic_seal(&p);
    char buf[1024];
    size_t n = wd_blackbox_json(&p, &t, buf, sizeof(buf));
    CHECK(n < sizeof(buf) && strstr(buf, "\"exception\":\"fault\"") && strstr(buf, "\"pc\":\"0x42009555\"") && strstr(buf, "\"a0\":\"0x4200bbbb\"") && strstr(buf, "\"backtrace\":[\"0x42009555\",\"0x4200aaaa\"]"));
    CHECK(strstr(buf, "\"stage\":\"tx\"") && strstr(buf, "\"tx\":2,\"rx\":1,\"stack_min\":3000") && strstr(buf, "\"lookup\":10") && strstr(buf, "\"twdt\":\"_ - br_supervisor (CPU 0)\""));
    char full[1100];
    snprintf(full, sizeof(full), "{%s}", buf);
    cJSON *j = cJSON_Parse(full);
    CHECK(j && cJSON_GetObjectItem(cJSON_GetObjectItem(j, "last_send"), "seq")->valueint == 1);
    cJSON_Delete(j);
    p.pc ^= 1;
    CHECK(!wd_panic_valid(&p));
    wd_blackbox_json(&p, NULL, buf, sizeof(buf));
    CHECK(!strcmp(buf, "\"last_panic\":null,\"last_send\":null"));
    char small[20];
    wd_panic_seal(&p);
    n = wd_blackbox_json(&p, &t, small, sizeof(small));
    CHECK(n > sizeof(small) && strlen(small) == sizeof(small) - 1);
    wd_trace_begin(&t, 9, 5000);
    CHECK(t.seq == 2 && t.tx == 0 && t.stack_min == 0);
}

int main(void)
{
    test_blackbox();
    test_crash_isolates_node();
    test_crash_count_decays_after_report();
    test_crash_attribution_window();
    test_one_send_charged_once();
    test_normal_reboot_adds_no_count();
    test_global_crash_backoff_persists();
    test_boot_loop_post_send_bounded();
    test_dry_run_never_sends_after_restore();
    test_u64_str();
    test_defaults_and_token();
    test_parse_heartbeat();
    test_parse_mode_and_hex();
    test_parse_credentials();
    test_no_trigger_with_heartbeats();
    test_boot_trigger_dry_run();
    test_armed_sends_each_node_once_then_rearm();
    test_retry_backoff_and_giveup();
    test_retry_stops_on_heartbeat();
    test_dry_to_armed_during_outage();
    test_status_json_shape();
    test_status_json_diag();
    test_restore_keeps_timer_across_reboots();
    test_restore_triggered_resumes_window();
    test_rtc_validation();
    test_conn_backoff();
    test_long_outage_no_reboot_loop();
    test_heartbeat_vetoes_wifi_restart();
    test_restore_keeps_ok_nodes();
    test_armed_trigger_survives_reboot_loop();
    test_feed_ok();
    printf("%d checks, %d failed\n", checks, failures);
    return failures ? 1 : 0;
}
