#!/bin/sh
set -eu

ROOT=$(CDPATH= cd -- "$(dirname -- "$0")" && pwd)
BUNDLE=${1:-/Library/Audio/Plug-Ins/HAL/SalsaHAL.driver}
CAPTURE=${SALSA_CAPTURE:-"$ROOT/build/salsa_carrier_capture"}
STATUS=0

printf '%s\n' "== installed bundle =="
if [ -d "$BUNDLE" ]; then
    printf 'ok   %s\n' "$BUNDLE"
    if codesign --verify --strict "$BUNDLE"; then
        printf 'ok   signature verifies\n'
    else
        printf 'FAIL signature does not verify\n'
        STATUS=1
    fi
    printf '%s\n' "device identities:"
    strings "$BUNDLE/Contents/MacOS/SalsaHAL" | grep -E '^salsa-atmos' | sort -u
    printf '%s\n' "Only one format variant is installed at a time."
else
    printf 'FAIL %s is missing\n' "$BUNDLE"
    STATUS=1
fi

printf '%s\n' "== build variants =="
for directory in build build-eac3 build-truehd build-native8-test build-pcm-test; do
    bundle="$ROOT/$directory/SalsaHAL.driver"
    if [ -d "$bundle" ]; then
        uid=$(strings "$bundle/Contents/MacOS/SalsaHAL" |
            grep -E '^salsa-atmos(-eac3|-truehd)?$' | head -n 1)
        printf '%-24s %s\n' "$directory" "${uid:-unknown}"
    fi
done

printf '%s\n' "== driver probe =="
if [ -x "$CAPTURE" ]; then
    probe_output=$("$CAPTURE" --probe 2>&1) || STATUS=1
    printf '%s\n' "$probe_output"
    case "$probe_output" in
        *"input virtual"*)
            printf 'warning: the installed build exposes an input stream; reinstall the fixed output-only build\n' >&2
            ;;
    esac
else
    printf 'info capture tool not built: %s\n' "$CAPTURE"
fi

printf '%s\n' "== player settings =="
printf '%s\n' \
    "mpv --ao=coreaudio_exclusive \\" \
    "    --audio-device=coreaudio_exclusive/<UID> \\" \
    "    --audio-spdif=eac3,truehd movie.mkv" \
    "" \
    "Use salsa-atmos-eac3 for the fixed E-AC-3 build and" \
    "salsa-atmos-truehd for the fixed TrueHD build."

exit "$STATUS"
