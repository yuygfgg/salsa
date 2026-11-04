#!/bin/sh
set -eu

ROOT=$(CDPATH= cd -- "$(dirname -- "$0")" && pwd)
BUNDLE=${1:-"$ROOT/build/SalsaHAL.driver"}
TARGET=/Library/Audio/Plug-Ins/HAL/SalsaHAL.driver

if [ "$(id -u)" -ne 0 ]; then
    printf '%s\n' "install.sh must run as root: sudo $0" >&2
    exit 1
fi

if [ ! -d "$BUNDLE" ]; then
    printf 'build the requested bundle first: %s\n' "$BUNDLE" >&2
    exit 1
fi
codesign --verify --strict "$BUNDLE"
rm -rf "$TARGET"
cp -R "$BUNDLE" "$TARGET"
chown -R root:wheel "$TARGET"
chmod -R a+rX,go-w "$TARGET"

printf '%s\n' "installed $TARGET"
printf '%s\n' "restarting coreaudiod"
killall coreaudiod || true
