#pragma once
#include <stddef.h>
#include <stdint.h>
#include "esp_err.h"
#include "sdkconfig.h"
#include "watchdog_core.h"

#ifdef CONFIG_BR_WATCHDOG_MATTER
#define WD_MATTER_BUILT 1
#else
#define WD_MATTER_BUILT 0
#endif

void br_watchdog_stage(wd_stage_t stage);
esp_err_t wd_matter_write_setpoint(const wd_creds_t *creds, uint64_t node, uint8_t endpoint, int32_t centi_c, char *err, size_t err_len);
