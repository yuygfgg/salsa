#!/bin/sh
set -eu
ROOT=$(CDPATH= cd -- "$(dirname -- "$0")/.." && pwd)
SDK=$(xcrun --show-sdk-path)
SHM_NAME="/salsa-hal-test-carrier-$$"
mkdir -p "$ROOT/build"
for mode in native pcm frame native8 eac3 truehd; do
    case "$mode" in
        native) set -- ;;
        pcm) set -- -DSALSA_HAL_PCM_TEST ;;
        frame) set -- -DSALSA_HAL_FRAME_PACKETS ;;
        native8) set -- -DSALSA_HAL_FIXED_CHANNELS=8 ;;
        eac3)
            set -- -DSALSA_HAL_FIXED_CHANNELS=2 \
                '-DSALSA_HAL_DEVICE_UID="salsa-atmos-eac3"' \
                '-DSALSA_HAL_DEVICE_NAME="Salsa Atmos E-AC-3"' \
                '-DSALSA_HAL_BOX_UID="salsa-atmos-eac3-box"' \
                '-DSALSA_HAL_MODEL_UID="salsa-atmos-eac3-model"'
            ;;
        truehd)
            set -- -DSALSA_HAL_FIXED_CHANNELS=8 \
                '-DSALSA_HAL_DEVICE_UID="salsa-atmos-truehd"' \
                '-DSALSA_HAL_DEVICE_NAME="Salsa Atmos TrueHD"' \
                '-DSALSA_HAL_BOX_UID="salsa-atmos-truehd-box"' \
                '-DSALSA_HAL_MODEL_UID="salsa-atmos-truehd-model"'
            ;;
    esac
    clang -O1 -g -fsanitize=address,undefined -Wall -Wextra -Wno-unused-parameter \
        -isysroot "$SDK" "$@" -USALSA_HAL_OUTPUT_ONLY -DSALSA_CARRIER_SHM_NAME=\"$SHM_NAME\" "$ROOT/tools/test_driver.c" \
        -framework CoreAudio -framework CoreFoundation -framework IOKit -framework AudioUnit \
        -o "$ROOT/build/test-driver-$mode"
    printf 'mode: %s\n' "$mode"
    "$ROOT/build/test-driver-$mode"
done

SHM_NAME="/salsa-hal-test-output-$$"
clang -O1 -g -fsanitize=address,undefined -Wall -Wextra -Wno-unused-parameter \
    -isysroot "$SDK" -DSALSA_HAL_FIXED_CHANNELS=2 -DSALSA_HAL_OUTPUT_ONLY=1 \
    -DSALSA_CARRIER_SHM_NAME=\"$SHM_NAME\" "$ROOT/tools/test_driver.c" \
    -framework CoreAudio -framework CoreFoundation -framework IOKit -framework AudioUnit \
    -o "$ROOT/build/test-driver-output-only"
printf 'mode: %s\n' output-only
"$ROOT/build/test-driver-output-only"
