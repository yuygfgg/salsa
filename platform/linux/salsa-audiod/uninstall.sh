#!/bin/sh
set -eu

DATA=${XDG_DATA_HOME:-"$HOME/.local/share"}
CONFIG=${XDG_CONFIG_HOME:-"$HOME/.config"}
UNIT_DIR="$CONFIG/systemd/user"
INSTALLED="$DATA/salsa/salsa-audiod"
PURGE=0

if [ "${1:-}" = "--purge" ]; then
    PURGE=1
    shift
fi

for profile in eac3 truehd; do
    systemctl --user disable --now "salsa-audiod-$profile.service" 2>/dev/null || true
    rm -f "$UNIT_DIR/salsa-audiod-$profile.service"
    printf 'removed %s/salsa-audiod-%s.service\n' "$UNIT_DIR" "$profile"
done
systemctl --user daemon-reload 2>/dev/null || true

if [ -L "$HOME/.local/bin/salsa-audiod" ] &&
    [ "$(readlink "$HOME/.local/bin/salsa-audiod")" = "$INSTALLED" ]; then
    rm -f "$HOME/.local/bin/salsa-audiod"
fi
rm -f "$INSTALLED"

if [ "$PURGE" -eq 1 ]; then
    rm -f "$CONFIG/salsa/audiod-eac3.json" "$CONFIG/salsa/audiod-truehd.json"
fi
rmdir "$DATA/salsa" 2>/dev/null || true

printf '%s\n' "removed $INSTALLED"
printf '%s\n' "the ALSA plug-in is separate: platform/linux/salsa-alsa/uninstall.sh"
