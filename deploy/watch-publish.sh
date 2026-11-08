#!/usr/bin/env bash
set -euo pipefail
cd "$(dirname "$0")/.."
M=${MINIX:-aapphh@192.168.178.29}
DEST=${WATCH_UPDATE_DIR:-/home/aapphh/MLSmartMatterHeat/data/watch_update}
APK=${APK:-wear/app/build/outputs/apk/release/app-release.apk}
[ -f "$APK" ] || { echo "APK fehlt: $APK" >&2; exit 1; }
CODE=; NAME=
if command -v aapt2 >/dev/null 2>&1; then
  BADGING=$(aapt2 dump badging "$APK" 2>/dev/null | grep '^package:' || true)
  CODE=$(sed -n "s/.*versionCode='\([0-9]*\)'.*/\1/p" <<<"$BADGING")
  NAME=$(sed -n "s/.*versionName='\([^']*\)'.*/\1/p" <<<"$BADGING")
fi
if [ -z "$CODE" ] || [ -z "$NAME" ] && command -v apkanalyzer >/dev/null 2>&1; then
  CODE=$(apkanalyzer manifest version-code "$APK" 2>/dev/null || true)
  NAME=$(apkanalyzer manifest version-name "$APK" 2>/dev/null || true)
fi
if [ -z "$CODE" ] || [ -z "$NAME" ]; then
  CODE=$(sed -n 's/^[[:space:]]*versionCode = \([0-9]*\).*/\1/p' wear/app/build.gradle.kts | head -1)
  NAME=$(sed -n 's/^[[:space:]]*versionName = "\([^"]*\)".*/\1/p' wear/app/build.gradle.kts | head -1)
fi
[ -n "$CODE" ] && [ -n "$NAME" ] || { echo "versionCode/versionName nicht ermittelbar" >&2; exit 1; }
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
