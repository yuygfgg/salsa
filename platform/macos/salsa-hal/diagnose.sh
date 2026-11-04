#!/bin/sh
set -eu

ROOT=$(CDPATH= cd -- "$(dirname -- "$0")" && pwd)
TARGET_DIR=/Library/Audio/Plug-Ins/HAL
CAPTURE=${SALSA_CAPTURE:-"$ROOT/build/salsa_carrier_capture"}
STATUS=0
FOUND=0

printf '%s\n' "== installed bundles =="
for name in SalsaHAL-EAC3.driver SalsaHAL-TrueHD.driver; do
    bundle="$TARGET_DIR/$name"
    if [ -d "$bundle" ]; then
        FOUND=1
        printf 'ok   %s\n' "$bundle"
        identifier=$(plutil -extract CFBundleIdentifier raw -o - \
            "$bundle/Contents/Info.plist" 2>/dev/null || printf 'unknown')
        printf '     bundle id: %s\n' "$identifier"
        if codesign --verify --strict "$bundle"; then
            printf 'ok   signature verifies\n'
        else
            printf 'FAIL signature does not verify\n'
            STATUS=1
        fi
        printf '     device identities: '
        strings "$bundle/Contents/MacOS/SalsaHAL" | grep -E '^salsa-atmos' | sort -u | tr '\n' ' '
        printf '\n'
        printf '     carrier ring: '
        strings "$bundle/Contents/MacOS/SalsaHAL" | grep -E '^/salsa-hal-carrier' | sort -u | tr '\n' ' '
        printf '\n'
    fi
done
if [ "$FOUND" -eq 0 ]; then
    printf 'FAIL no Salsa HAL driver is installed in %s\n' "$TARGET_DIR"
    STATUS=1
fi
printf '%s\n' "== build variants =="
for directory in build build-eac3 build-truehd build-native8-test build-pcm-test; do
    bundle="$ROOT/$directory/SalsaHAL.driver"
    if [ -d "$bundle" ]; then
        uid=$(strings "$bundle/Contents/MacOS/SalsaHAL" |
            grep -E '^salsa-atmos(-eac3|-truehd)?$' | head -n 1)
        if [ -f "$bundle/Contents/Info.plist" ]; then
            identifier=$(plutil -extract CFBundleIdentifier raw -o - \
                "$bundle/Contents/Info.plist" 2>/dev/null || printf 'unknown')
        else
            identifier=unknown
        fi
        printf '%-24s %-18s %s\n' "$directory" "${uid:-unknown}" "$identifier"
    fi
done

printf '%s\n' "== driver probes =="
PROBED=0
probe_one() {
    capture=$1
    uid=$2
    if [ -x "$capture" ]; then
        PROBED=1
        printf '%s\n' "-- $capture ($uid)"
        probe_output=$("$capture" --device "$uid" --probe 2>&1) || true
        printf '%s\n' "$probe_output"
        case "$probe_output" in
            *"input virtual"*)
                printf 'warning: the installed build exposes an input stream; reinstall the fixed output-only build\n' >&2
                ;;
        esac
    fi
}
probe_one "$CAPTURE" salsa-atmos
probe_one "$ROOT/build-eac3/salsa_carrier_capture" salsa-atmos-eac3
probe_one "$ROOT/build-truehd/salsa_carrier_capture" salsa-atmos-truehd
if [ "$PROBED" -eq 0 ]; then
    printf 'info capture tool not built: %s\n' "$CAPTURE"
fi

printf '%s\n' "== player settings =="
printf '%s\n' \
    "mpv --ao=coreaudio_exclusive \\" \
    "    --audio-device=coreaudio_exclusive/salsa-atmos-eac3 \\" \
    "    --audio-spdif=eac3 movie.mkv" \
    "" \
    "mpv --ao=coreaudio_exclusive \\" \
    "    --audio-device=coreaudio_exclusive/salsa-atmos-truehd \\" \
    "    --audio-spdif=truehd movie.mkv"

exit "$STATUS"
