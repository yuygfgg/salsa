#!/bin/sh
set -eu

ROOT=$(CDPATH= cd -- "$(dirname -- "$0")" && pwd)
COMMON=$(CDPATH= cd -- "$ROOT/../../common" && pwd)
OUT="$ROOT/build"
CC=${CC:-cc}

mkdir -p "$OUT"

$CC -shared -fPIC -DPIC -O2 -Wall -Wextra \
    -I "$COMMON" \
    -o "$OUT/libasound_module_pcm_salsa.so" \
    "$ROOT/src/pcm_salsa.c" \
    $(pkg-config --cflags --libs alsa)

printf '%s\n' "$OUT/libasound_module_pcm_salsa.so"
