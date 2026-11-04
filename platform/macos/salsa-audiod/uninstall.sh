#!/bin/sh
set -eu

LABEL=dev.salsa.audio.audiod
PREFIX="$HOME/Library/Application Support/Salsa"
INSTALLED="$PREFIX/salsa-audiod"
PLIST="$HOME/Library/LaunchAgents/$LABEL.plist"

PURGE=0
if [ "${1:-}" = "--purge" ]; then
    PURGE=1
    shift
fi

launchctl bootout "gui/$(id -u)/$LABEL" 2>/dev/null || true
BREW_LINK=
if command -v brew >/dev/null 2>&1; then
    BREW_LINK="$(brew --prefix)/bin/salsa-audiod"
fi
for link in "$BREW_LINK" /usr/local/bin/salsa-audiod "$HOME/.local/bin/salsa-audiod"; do
    if [ -L "$link" ] && [ "$(readlink "$link")" = "$INSTALLED" ]; then
        rm -f "$link"
    fi
done
rm -f "$PLIST"
rm -f "$INSTALLED"
if [ "$PURGE" -eq 1 ]; then
    rm -rf "$HOME/Library/Logs/Salsa"
    rm -f "$PREFIX/audiod.json"
fi
rmdir "$PREFIX" 2>/dev/null || true

printf '%s\n' "removed $INSTALLED"
printf '%s\n' "removed $PLIST"
printf '%s\n' "the HAL driver is separate: sudo platform/macos/salsa-hal/uninstall.sh"
