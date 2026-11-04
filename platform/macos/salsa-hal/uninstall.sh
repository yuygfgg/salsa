#!/bin/sh
set -eu

TARGET=/Library/Audio/Plug-Ins/HAL/SalsaHAL.driver

if [ "$(id -u)" -ne 0 ]; then
    printf '%s\n' "uninstall.sh must run as root: sudo $0" >&2
    exit 1
fi

rm -rf "$TARGET"
printf '%s\n' "removed $TARGET"
printf '%s\n' "restarting coreaudiod"
killall coreaudiod || true
