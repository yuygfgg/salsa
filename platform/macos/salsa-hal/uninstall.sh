#!/bin/sh
set -eu

TARGET_DIR=/Library/Audio/Plug-Ins/HAL
REMOVED=0

if [ "$(id -u)" -ne 0 ]; then
    printf '%s\n' "uninstall.sh must run as root: sudo $0" >&2
    exit 1
fi

for name in SalsaHAL-EAC3.driver SalsaHAL-TrueHD.driver; do
    target="$TARGET_DIR/$name"
    if [ -d "$target" ]; then
        rm -rf "$target"
        printf 'removed %s\n' "$target"
        REMOVED=1
    fi
done

if [ "$REMOVED" -eq 0 ]; then
    printf '%s\n' "no Salsa HAL driver bundles were installed"
    exit 0
fi

printf '%s\n' "restarting coreaudiod"
killall coreaudiod || true
