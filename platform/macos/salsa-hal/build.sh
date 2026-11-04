#!/bin/sh
set -eu

ROOT=$(CDPATH= cd -- "$(dirname -- "$0")" && pwd)
MODE=${MODE:-native}

if [ "$MODE" = both ]; then
    MODE=eac3 "$0"
    MODE=truehd "$0"
    exit 0
fi

# Each installed codec variant needs its own CoreAudio device identity. Reusing
# the bundle identifier or the CFPlugIn factory UUID makes coreaudiod load only
# one of the two bundles; reusing the carrier SHM name makes them overwrite each
# other's ring. Keep all three per-variant.
BUNDLE_ID=dev.salsa.audio.SalsaHAL
BUNDLE_NAME="Salsa Atmos"
FACTORY_UUID=A7CE48BD-0FD0-4D56-92DC-E39CE7D12A64

case "$MODE" in
    native) BUILD="$ROOT/build"; set -- ;;
    pcm) BUILD="$ROOT/build-pcm-test"; set -- -DSALSA_HAL_PCM_TEST ;;
    frame) BUILD="$ROOT/build-frame-test"; set -- -DSALSA_HAL_FRAME_PACKETS ;;
    native8) BUILD="$ROOT/build-native8-test"; set -- -DSALSA_HAL_FIXED_CHANNELS=8 ;;
    eac3)
        BUILD="$ROOT/build-eac3"
        BUNDLE_ID=dev.salsa.audio.SalsaHAL.eac3
        BUNDLE_NAME="Salsa Atmos E-AC-3"
        FACTORY_UUID=A7CE48BD-0FD0-4D56-92DC-E39CE7D12A64
        set -- -DSALSA_HAL_FIXED_CHANNELS=2 -DSALSA_HAL_OUTPUT_ONLY=1 \
            '-DSALSA_HAL_DEVICE_UID="salsa-atmos-eac3"' \
            '-DSALSA_HAL_DEVICE_NAME="Salsa Atmos E-AC-3"' \
            '-DSALSA_HAL_BOX_UID="salsa-atmos-eac3-box"' \
            '-DSALSA_HAL_MODEL_UID="salsa-atmos-eac3-model"' \
            '-DSALSA_CARRIER_SHM_NAME="/salsa-hal-carrier-eac3"'
        ;;
    truehd)
        BUILD="$ROOT/build-truehd"
        BUNDLE_ID=dev.salsa.audio.SalsaHAL.truehd
        BUNDLE_NAME="Salsa Atmos TrueHD"
        FACTORY_UUID=B8DF59CE-1FE1-4E67-A3ED-F4ADF8E23B75
        set -- -DSALSA_HAL_FIXED_CHANNELS=8 -DSALSA_HAL_OUTPUT_ONLY=1 \
            '-DSALSA_HAL_DEVICE_UID="salsa-atmos-truehd"' \
            '-DSALSA_HAL_DEVICE_NAME="Salsa Atmos TrueHD"' \
            '-DSALSA_HAL_BOX_UID="salsa-atmos-truehd-box"' \
            '-DSALSA_HAL_MODEL_UID="salsa-atmos-truehd-model"' \
            '-DSALSA_CARRIER_SHM_NAME="/salsa-hal-carrier-truehd"'
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
    "-DSALSA_HAL_BUNDLE_ID=\"$BUNDLE_ID\"" \
    -o "$BUNDLE/Contents/MacOS/SalsaHAL" "$ROOT/src/SalsaHAL.c" \
    -framework CoreAudio -framework CoreFoundation -framework IOKit -framework AudioUnit

clang -O2 -Wall -Wextra -isysroot "$SDK" \
    -o "$BUILD/salsa_carrier_capture" "$ROOT/tools/salsa_carrier_capture.c" \
    -framework CoreAudio -framework CoreFoundation

SANSA_BUNDLE_ID="$BUNDLE_ID" SANSA_BUNDLE_NAME="$BUNDLE_NAME" \
SANSA_FACTORY_UUID="$FACTORY_UUID" \
python3 - "$ROOT/Info.plist" "$BUNDLE/Contents/Info.plist" <<'PY'
import os
import plistlib
import sys

source, destination = sys.argv[1:3]
with open(source, "rb") as handle:
    document = plistlib.load(handle)

bundle_id = os.environ["SANSA_BUNDLE_ID"]
bundle_name = os.environ["SANSA_BUNDLE_NAME"]
factory = os.environ["SANSA_FACTORY_UUID"]

document["CFBundleIdentifier"] = bundle_id
document["CFBundleName"] = bundle_name
document["CFBundleDisplayName"] = bundle_name
document["CFPlugInFactories"] = {factory: "SalsaHAL_Create"}
for key, values in document.get("CFPlugInTypes", {}).items():
    document["CFPlugInTypes"][key] = [factory for _ in values]

with open(destination, "wb") as handle:
    plistlib.dump(document, handle)
PY

codesign --force --sign - "$BUNDLE"
codesign --verify --strict "$BUNDLE"

printf '%s\n' "$BUNDLE"
printf '%s\n' "$BUILD/salsa_carrier_capture"
