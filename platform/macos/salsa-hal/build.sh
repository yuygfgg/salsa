#!/bin/sh
set -eu

ROOT=$(CDPATH= cd -- "$(dirname -- "$0")" && pwd)
MODE=${MODE:-native}
case "$MODE" in
    native) BUILD="$ROOT/build"; set -- ;;
    pcm) BUILD="$ROOT/build-pcm-test"; set -- -DSALSA_HAL_PCM_TEST ;;
    frame) BUILD="$ROOT/build-frame-test"; set -- -DSALSA_HAL_FRAME_PACKETS ;;
    native8) BUILD="$ROOT/build-native8-test"; set -- -DSALSA_HAL_FIXED_CHANNELS=8 ;;
    eac3)
        BUILD="$ROOT/build-eac3"
        set -- -DSALSA_HAL_FIXED_CHANNELS=2 -DSALSA_HAL_OUTPUT_ONLY=1 \
            '-DSALSA_HAL_DEVICE_UID="salsa-atmos-eac3"' \
            '-DSALSA_HAL_DEVICE_NAME="Salsa Atmos E-AC-3"' \
            '-DSALSA_HAL_BOX_UID="salsa-atmos-eac3-box"' \
            '-DSALSA_HAL_MODEL_UID="salsa-atmos-eac3-model"'
        ;;
    truehd)
        BUILD="$ROOT/build-truehd"
        set -- -DSALSA_HAL_FIXED_CHANNELS=8 -DSALSA_HAL_OUTPUT_ONLY=1 \
            '-DSALSA_HAL_DEVICE_UID="salsa-atmos-truehd"' \
            '-DSALSA_HAL_DEVICE_NAME="Salsa Atmos TrueHD"' \
            '-DSALSA_HAL_BOX_UID="salsa-atmos-truehd-box"' \
            '-DSALSA_HAL_MODEL_UID="salsa-atmos-truehd-model"'
        ;;
    *) printf 'unknown mode: %s\n' "$MODE" >&2; exit 2 ;;
esac
BUNDLE="$BUILD/SalsaHAL.driver"
SDK=$(xcrun --show-sdk-path)

rm -rf "$BUNDLE"
mkdir -p "$BUNDLE/Contents/MacOS" "$BUNDLE/Contents/Resources"

clang -bundle -fPIC -O2 -Wall -Wextra -Wno-unused-parameter \
    -isysroot "$SDK" \
    "$@" -DSALSA_HAL_LATENCY_FRAMES="${LATENCY_FRAMES:-0}" \
    -o "$BUNDLE/Contents/MacOS/SalsaHAL" "$ROOT/src/SalsaHAL.c" \
    -framework CoreAudio -framework CoreFoundation -framework IOKit -framework AudioUnit

clang -O2 -Wall -Wextra -isysroot "$SDK" \
    -o "$BUILD/salsa_carrier_capture" "$ROOT/tools/salsa_carrier_capture.c" \
    -framework CoreAudio -framework CoreFoundation

cp "$ROOT/Info.plist" "$BUNDLE/Contents/Info.plist"
codesign --force --sign - "$BUNDLE"
codesign --verify --strict "$BUNDLE"

printf '%s\n' "$BUNDLE"
printf '%s\n' "$BUILD/salsa_carrier_capture"
