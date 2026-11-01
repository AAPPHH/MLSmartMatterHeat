#!/usr/bin/env bash
set -euo pipefail
cd "$(dirname "$0")"
IDF=${IDF_PATH:-/c/clones/esp/esp-idf}
TMP=${TMPDIR:-/tmp}
CC=${CC:-gcc}
MBED=$IDF/components/mbedtls/mbedtls
MBLIB=$TMP/mbedtls_host
MBFILES="platform aes bignum bignum_core bignum_mod bignum_mod_raw ccm cipher cipher_wrap constant_time ecdh ecdsa ecp ecp_curves md platform_util sha256 asn1parse asn1write"
MBFLAGS="-std=c11 -O1 -I$MBED/include -I$MBED/library -DMBEDTLS_CONFIG_FILE=\"$PWD/mbedtls_host_config.h\" -w"
mkdir -p "$MBLIB"
for f in $MBFILES; do
    [ "$MBLIB/$f.o" -nt "$MBED/library/$f.c" ] || $CC $MBFLAGS -c "$MBED/library/$f.c" -o "$MBLIB/$f.o"
done
$CC -std=c11 -Wall -Wextra -Werror -I"$IDF/components/json/cJSON" ../main/watchdog_core.c "$IDF/components/json/cJSON/cJSON.c" test_watchdog_core.c -lm -o "$TMP/wd_core_test"
"$TMP/wd_core_test"
MATTER_SRC="../main/matter_proto.c ../main/matter_client.c"
$CC -std=c11 -Wall -Wextra -Werror -I"$MBED/include" -DMBEDTLS_CONFIG_FILE="\"$PWD/mbedtls_host_config.h\"" -I"$IDF/components/json/cJSON" $MATTER_SRC ../main/watchdog_core.c "$IDF/components/json/cJSON/cJSON.c" test_matter.c "$MBLIB"/*.o -lm -o "$TMP/matter_test"
"$TMP/matter_test"
