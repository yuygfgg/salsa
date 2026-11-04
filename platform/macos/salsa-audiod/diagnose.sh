#!/bin/sh
set -eu

LABEL=dev.salsa.audio.audiod
PREFIX="$HOME/Library/Application Support/Salsa"
INSTALLED="$PREFIX/salsa-audiod"
HAL_DIR=/Library/Audio/Plug-Ins/HAL
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

check_agent() {
    name=$1
    config=$2
    label="$LABEL.$name"
    plist="$HOME/Library/LaunchAgents/$label.plist"
    printf '%s\n' "== agent: $name =="
    if [ -f "$plist" ]; then
        printf 'ok   %s\n' "$plist"
        plutil -lint "$plist"
        plutil -p "$plist" | sed -n '1,20p'
    else
        printf 'FAIL %s is missing\n' "$plist"
        STATUS=1
    fi
    if [ -f "$config" ]; then
        printf 'ok   %s\n' "$config"
        if [ -x "$INSTALLED" ]; then
            "$INSTALLED" configure --agent "$name" --config "$config" --show || STATUS=1
        fi
    else
        printf 'info %s does not exist\n' "$config"
    fi
    if launchctl print "gui/$(id -u)/$label" >/dev/null 2>&1; then
        printf 'ok   %s is loaded\n' "$label"
        launchctl print "gui/$(id -u)/$label" | sed -n '1,12p'
    else
        printf 'info %s is not loaded\n' "$label"
    fi
}

check_agent eac3 "$PREFIX/audiod-eac3.json"
check_agent truehd "$PREFIX/audiod-truehd.json"

pgrep -fl salsa-audiod || printf 'info no salsa-audiod process\n'

printf '%s\n' "== HAL drivers =="
FOUND=0
for name in SalsaHAL-EAC3.driver SalsaHAL-TrueHD.driver; do
    driver="$HAL_DIR/$name"
    if [ -d "$driver" ]; then
        FOUND=1
        printf 'ok   %s\n' "$driver"
        codesign --verify --strict "$driver" && printf 'ok   HAL signature\n'
        strings "$driver/Contents/MacOS/SalsaHAL" |
            grep -E '^salsa-atmos|^/salsa-hal-carrier' | sort -u | sed 's/^/     /'
    fi
done
if [ "$FOUND" -eq 0 ]; then
    printf 'FAIL no Salsa HAL driver is installed\n'
    STATUS=1
fi

printf '%s\n' "== carrier rings =="
for shm in /salsa-hal-carrier-eac3 /salsa-hal-carrier-truehd; do
    if python3 - "$shm" <<'PY'
import ctypes
import os
import sys

libc = ctypes.CDLL(None, use_errno=True)
fd = libc.shm_open(sys.argv[1].encode(), os.O_RDWR, 0)
if fd < 0:
    raise SystemExit(1)
libc.close(fd)
PY
    then
        printf 'ok   %s exists\n' "$shm"
    else
        printf 'info %s does not exist (is the HAL installed and a client playing?)\n' "$shm"
    fi
done

printf '%s\n' "== audio devices =="
if [ -x "$INSTALLED" ]; then
    OBJC_PRINT_DUPLICATE_CLASSES=NO "$INSTALLED" devices || true
else
    system_profiler SPAudioDataType 2>/dev/null | sed -n '1,80p' || true
fi

printf '%s\n' "== mpv =="
if command -v mpv >/dev/null 2>&1; then
    mpv --version | sed -n '1p'
    printf '%s\n' \
        "E-AC-3: --ao=coreaudio_exclusive --audio-device=coreaudio_exclusive/salsa-atmos-eac3 --audio-spdif=eac3" \
        "TrueHD: --ao=coreaudio_exclusive --audio-device=coreaudio_exclusive/salsa-atmos-truehd --audio-spdif=truehd"
else
    printf 'info mpv is not on PATH\n'
fi

exit "$STATUS"
