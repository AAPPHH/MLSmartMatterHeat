#!/usr/bin/env bash
set -euo pipefail
cd "$(dirname "$0")/.."
M=${MINIX:-aapphh@192.168.178.29}
DEST=${WATCH_UPDATE_DIR:-/home/aapphh/MLSmartMatterHeat/data/watch_update}
APK=${APK:-wear/app/build/outputs/apk/release/app-release.apk}
[ -f "$APK" ] || { echo "APK fehlt: $APK" >&2; exit 1; }
IMAGE=${WEAR_IMAGE:-ghcr.io/cirruslabs/android-sdk:36}
FP=${FINGERPRINT:-secrets/wear-release.sha256}
[ -s "$FP" ] || { echo "Fingerabdruck fehlt: $FP" >&2; exit 1; }
APKDIR=$(cd "$(dirname "$APK")" && pwd)
sdk() {
  local tool=$1; shift
  if command -v "$tool" >/dev/null 2>&1; then "$tool" "$@"; return; fi
  MSYS_NO_PATHCONV=1 docker run --rm -v "$APKDIR:/apk:ro" "$IMAGE" sh -c 'exec "$(ls -d "$ANDROID_HOME"/build-tools/*/ | tail -1)$0" "$@"' "$tool" "$@"
}
APKIN=$(command -v aapt2 >/dev/null 2>&1 && echo "$APK" || echo "/apk/$(basename "$APK")")
BADGING=$(sdk aapt2 dump badging "$APKIN" | grep '^package:') || BADGING=
CODE=$(sed -n "s/.*versionCode='\([0-9]*\)'.*/\1/p" <<<"$BADGING")
NAME=$(sed -n "s/.*versionName='\([^']*\)'.*/\1/p" <<<"$BADGING")
[ -n "$CODE" ] && [ -n "$NAME" ] || { echo "versionCode/versionName nicht aus der APK lesbar" >&2; exit 1; }
APKSIGNIN=$(command -v apksigner >/dev/null 2>&1 && echo "$APK" || echo "/apk/$(basename "$APK")")
CERTS=$(sdk apksigner verify --print-certs "$APKSIGNIN") || { echo "APK-Signatur ungueltig" >&2; exit 1; }
DIGESTS=$(sed -n 's/^Signer #[0-9]* certificate SHA-256 digest: //p' <<<"$CERTS" | tr 'A-F' 'a-f')
[ "$(wc -l <<<"$DIGESTS")" -eq 1 ] && [ "$DIGESTS" = "$(tr -d '[:space:]' < "$FP" | tr 'A-F' 'a-f')" ] || { echo "APK ist nicht mit dem Release-Zertifikat signiert" >&2; exit 1; }
SHA=$(sha256sum "$APK" | cut -d' ' -f1)
SIZE=$(wc -c < "$APK" | tr -d ' ')
T=$(mktemp -d)
trap 'rm -rf "$T"' EXIT
printf '{"version_code":%s,"version_name":"%s","sha256":"%s","size":%s}\n' "$CODE" "$NAME" "$SHA" "$SIZE" > "$T/update.json"
ssh "$M" "mkdir -p '$DEST'"
scp -q "$APK" "$M:$DEST/.update.apk.tmp"
ssh "$M" "mv '$DEST/.update.apk.tmp' '$DEST/update.apk'"
scp -q "$T/update.json" "$M:$DEST/.update.json.tmp"
ssh "$M" "mv '$DEST/.update.json.tmp' '$DEST/update.json'"
echo "published $NAME ($CODE) $SIZE bytes sha256 $SHA"
