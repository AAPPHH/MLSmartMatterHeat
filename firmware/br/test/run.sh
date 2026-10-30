#!/usr/bin/env bash
set -euo pipefail
cd "$(dirname "$0")"
IDF=${IDF_PATH:-/c/clones/esp/esp-idf}
OUT=${TMPDIR:-/tmp}/wd_core_test
${CC:-gcc} -std=c11 -Wall -Wextra -Werror -I"$IDF/components/json/cJSON" ../main/watchdog_core.c "$IDF/components/json/cJSON/cJSON.c" test_watchdog_core.c -lm -o "$OUT"
"$OUT"
