#include "watchdog_matter.h"
#include <string.h>

#ifdef CONFIG_BR_WATCHDOG_MATTER
#error "BR_WATCHDOG_MATTER needs the esp-matter controller integration, not implemented: see deploy/br-watchdog/README.md section Gap"
#else
esp_err_t wd_matter_write_setpoint(const wd_creds_t *creds, uint64_t node, uint8_t endpoint, int32_t centi_c, char *err, size_t err_len)
{
    (void)creds;
    (void)node;
    (void)endpoint;
    (void)centi_c;
    strlcpy(err, "matter_not_built", err_len);
    return ESP_ERR_NOT_SUPPORTED;
}
#endif
