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
MBCFG="-I$MBED/include -DMBEDTLS_CONFIG_FILE=\"$PWD/mbedtls_host_config.h\""
$CC -std=c11 -Wall -Wextra -Werror $MBCFG -I"$IDF/components/json/cJSON" $MATTER_SRC ../main/watchdog_core.c "$IDF/components/json/cJSON/cJSON.c" test_matter.c "$MBLIB"/*.o -lm -o "$TMP/matter_test"
"$TMP/matter_test"
LIBS=""
case "$(uname -s)" in MINGW*|MSYS*|CYGWIN*) LIBS="-lws2_32";; esac
$CC -std=gnu11 -Wall -Wextra -Werror $MBCFG $MATTER_SRC test_loopback.c "$MBLIB"/*.o $LIBS -o "$TMP/matter_loopback"
PORT=${LB_PORT:-47500}
lb() {
    local name=$1 expect=$2 peer=${3:-DEDEDEDE00010002} envs=${4:-LB_NONE=1}
    PORT=$((PORT + 1))
    rm -f "$TMP/lb_result"
    RESP_PORT=$PORT RESP_SCENARIO=$name RESP_RESULT=$TMP/lb_result RESP_VECTORS=$PWD/vectors.h python -I ref_responder.py &
    local rp=$!
    sleep 1
    env $envs "$TMP/matter_loopback" $PORT "$expect" "$peer" || { echo "LOOPBACK FAIL $name"; kill $rp 2>/dev/null || true; return 1; }
    if [ "$expect" = ok ]; then
        wait $rp
        [ "$(cat "$TMP/lb_result")" = "1/0x201/0x12=1800" ] || { echo "LOOPBACK FAIL $name: responder saw $(cat "$TMP/lb_result")"; return 1; }
    else
        kill $rp 2>/dev/null || true
        wait $rp 2>/dev/null || true
    fi
    echo "loopback $name ok"
}
lb ok ok
lb drop_first ok
lb late ok
lb dup ok
lb bad_node case_nosharedroot DEDEDEDE00010003
lb ok case_nosharedroot DEDEDEDE00010002 LB_BAD_IPK=1
lb busy case_busy
lb bad_sig sig_sigma2
lb write_fail write_status_87
lb ok key_mismatch DEDEDEDE00010002 LB_BAD_KEY=1
lb silent timeout_sigma1
CL="$TMP/matter_loopback"
if command -v cygpath >/dev/null 2>&1; then CL=$(cygpath -w "$CL.exe"); fi
(cd ../matter_client && C_LOOPBACK="$CL" cargo test --release)
