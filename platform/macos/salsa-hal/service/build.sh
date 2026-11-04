#!/bin/sh
set -eu

ROOT=$(CDPATH= cd -- "$(dirname -- "$0")" && pwd)
OUT="$ROOT/../build"
SDK=$(xcrun --show-sdk-path)

mkdir -p "$OUT"

clang -O2 -Wall -Wextra -isysroot "$SDK" \
    -o "$OUT/salsa_audiod" "$ROOT/salsa_audiod.c" \
    -framework CoreAudio -framework CoreFoundation -framework IOKit

clang -O2 -Wall -Wextra -isysroot "$SDK" \
    -o "$OUT/loopback_capture" "$ROOT/loopback_capture.c" \
    -framework CoreAudio -framework CoreFoundation -framework IOKit

clang -O2 -Wall -Wextra -isysroot "$SDK" \
    -o "$OUT/clock_probe" "$ROOT/clock_probe.c" \
    -framework CoreAudio -framework CoreFoundation -framework IOKit

printf '%s\n' "$OUT/salsa_audiod"
printf '%s\n' "$OUT/loopback_capture"
printf '%s\n' "$OUT/clock_probe"
