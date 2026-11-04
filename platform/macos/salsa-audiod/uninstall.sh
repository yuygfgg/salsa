#!/bin/sh
set -eu

LABEL=dev.salsa.audio.audiod
PREFIX="$HOME/Library/Application Support/Salsa"
INSTALLED="$PREFIX/salsa-audiod"
LOG_DIR="$HOME/Library/Logs/Salsa"

PURGE=0
if [ "${1:-}" = "--purge" ]; then
    PURGE=1
    shift
fi

for profile in eac3 truehd; do
    launchctl bootout "gui/$(id -u)/$LABEL.$profile" 2>/dev/null || true
done

BREW_LINK=
if command -v brew >/dev/null 2>&1; then
    BREW_LINK="$(brew --prefix)/bin/salsa-audiod"
fi
for link in "$BREW_LINK" /usr/local/bin/salsa-audiod "$HOME/.local/bin/salsa-audiod"; do
    if [ -L "$link" ] && [ "$(readlink "$link")" = "$INSTALLED" ]; then
        rm -f "$link"
    fi
done

for profile in eac3 truehd; do
    rm -f "$HOME/Library/LaunchAgents/$LABEL.$profile.plist"
    printf 'removed %s\n' "$HOME/Library/LaunchAgents/$LABEL.$profile.plist"
done
rm -f "$INSTALLED"

if [ "$PURGE" -eq 1 ]; then
    rm -rf "$LOG_DIR"
    rm -f "$PREFIX/audiod-eac3.json" "$PREFIX/audiod-truehd.json"
fi
rmdir "$PREFIX" 2>/dev/null || true

printf '%s\n' "removed $INSTALLED"
printf '%s\n' "the HAL driver is separate: sudo platform/macos/salsa-hal/uninstall.sh"
