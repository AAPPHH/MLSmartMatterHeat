#!/bin/sh
set -e
cd "$(dirname "$0")"
mkdir -p ../secrets
[ $# -eq 0 ] && set -- testDebugUnitTest assembleRelease
MSYS_NO_PATHCONV=1 exec docker run --rm \
  -v "$(pwd):/work/wear" \
  -v "$(cd ../secrets && pwd):/work/secrets" \
  -v heatscan-gradle:/root/.gradle \
  -v heatscan-android:/root/.android \
  -w /work/wear \
  ghcr.io/cirruslabs/android-sdk:36 \
  sh ./gradlew --console=plain "$@"
