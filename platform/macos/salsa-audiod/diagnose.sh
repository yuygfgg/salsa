#!/bin/sh
set -eu

LABEL=dev.salsa.audio.audiod
PREFIX="$HOME/Library/Application Support/Salsa"
INSTALLED="$PREFIX/salsa-audiod"
PLIST="$HOME/Library/LaunchAgents/$LABEL.plist"
HAL=/Library/Audio/Plug-Ins/HAL/SalsaHAL.driver
SHM=${SALSA_SHM_NAME:-/salsa-hal-carrier}
CONFIG=${SALSA_AUDIOD_CONFIG:-"$PREFIX/audiod.json"}
STATUS=0

printf '%s\n' "== service =="
if [ -x "$INSTALLED" ]; then
    printf 'ok   %s\n' "$INSTALLED"
    shasum -a 256 "$INSTALLED"
    codesign -dv "$INSTALLED" 2>&1 | sed -n '1,6p'
else
    printf 'FAIL %s is missing\n' "$INSTALLED"
    STATUS=1
fi

if [ -f "$PLIST" ]; then
    printf 'ok   %s\n' "$PLIST"
    plutil -lint "$PLIST"
    plutil -p "$PLIST" | sed -n '1,40p'
else
    printf 'FAIL %s is missing\n' "$PLIST"
    STATUS=1
fi

printf '%s\n' "== configuration =="
if [ -f "$CONFIG" ]; then
    printf 'ok   %s\n' "$CONFIG"
    if [ -x "$INSTALLED" ]; then
        "$INSTALLED" configure --show || STATUS=1
    fi
else
    printf 'info %s does not exist; run `%s configure`\n' "$CONFIG" "$INSTALLED"
fi

printf '%s\n' "== launchd =="
if launchctl print "gui/$(id -u)/$LABEL" >/dev/null 2>&1; then
    printf 'ok   %s is loaded\n' "$LABEL"
    launchctl print "gui/$(id -u)/$LABEL" | sed -n '1,20p'
else
    printf 'info %s is not loaded\n' "$LABEL"
fi
pgrep -fl salsa-audiod || printf 'info no salsa-audiod process\n'

printf '%s\n' "== HAL driver =="
if [ -d "$HAL" ]; then
    printf 'ok   %s\n' "$HAL"
    codesign --verify --strict "$HAL" && printf 'ok   HAL signature\n'
    strings "$HAL/Contents/MacOS/SalsaHAL" | grep -E '^salsa-atmos' | sort -u
    printf '     (one fixed-format build at a time; see platform/macos/salsa-hal/README.md)\n'
else
    printf 'FAIL %s is missing\n' "$HAL"
    STATUS=1
fi

printf '%s\n' "== carrier ring =="
if python3 - "$SHM" <<'PY'
import ctypes
import os
import sys

libc = ctypes.CDLL(None, use_errno=True)
name = sys.argv[1].encode()
fd = libc.shm_open(name, os.O_RDWR, 0)
if fd < 0:
    raise SystemExit(1)
libc.close(fd)
PY
then
    printf 'ok   %s exists\n' "$SHM"
else
    printf 'info %s does not exist (is the HAL installed and a client playing?)\n' "$SHM"
fi

printf '%s\n' "== audio devices =="
if [ -x "$INSTALLED" ]; then
    OBJC_PRINT_DUPLICATE_CLASSES=NO "$INSTALLED" devices || true
else
    system_profiler SPAudioDataType 2>/dev/null | sed -n '1,80p' || true
fi

printf '%s\n' "== mpv =="
if command -v mpv >/dev/null 2>&1; then
    mpv --version | sed -n '1p'
    printf '     required flags: --ao=coreaudio_exclusive --audio-device=coreaudio_exclusive/<UID> --audio-spdif=eac3,truehd\n'
else
    printf 'info mpv is not on PATH\n'
fi

exit "$STATUS"
