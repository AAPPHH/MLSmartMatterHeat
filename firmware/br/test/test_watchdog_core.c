#include <math.h>
#include <stdio.h>
#include <string.h>
#include "../main/watchdog_core.h"

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
    wd_status_json(&st, 5000, true, 41.5, "v1", true, buf, sizeof(buf));
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
    wd_status_json(&st, t, false, 0, "v1", true, buf, sizeof(buf));
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
    size_t n = wd_status_json(&st, 12, true, 38.46, "fw1", false, buf, sizeof(buf));
    CHECK(n == strlen(buf));
    CHECK(strstr(buf, "\"mode\":\"dry_run\"") && strstr(buf, "\"triggered\":false") && strstr(buf, "\"triggered_at_uptime_s\":null"));
    CHECK(strstr(buf, "\"results\":[]") && strstr(buf, "\"die_temperature_c\":38.5") && strstr(buf, "\"uptime_s\":12") && strstr(buf, "\"fw\":\"fw1\"") && strstr(buf, "\"timeout_s\":600"));
    char small[16];
    wd_status_json(&st, 12, true, 38.46, "fw1", false, small, sizeof(small));
    CHECK(strlen(small) == 15);
    n = wd_heartbeat_json(&st, 30, buf, sizeof(buf));
    CHECK(n == strlen(buf) && strcmp(buf, "{\"ok\":true,\"mode\":\"dry_run\",\"age_s\":30}") == 0);
}

int main(void)
{
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
    printf("%d checks, %d failed\n", checks, failures);
    return failures ? 1 : 0;
}
