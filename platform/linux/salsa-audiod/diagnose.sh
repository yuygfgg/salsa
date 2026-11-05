#!/bin/sh
set -eu

DATA=${XDG_DATA_HOME:-"$HOME/.local/share"}
CONFIG=${XDG_CONFIG_HOME:-"$HOME/.config"}
INSTALLED="$DATA/salsa/salsa-audiod"
STATUS=0

printf '%s\n' "== service binary =="
if [ -x "$INSTALLED" ]; then
    printf 'ok   %s\n' "$INSTALLED"
else
    printf 'FAIL %s is not installed\n' "$INSTALLED"
    STATUS=1
fi

printf '%s\n' "== configurations =="
for profile in eac3 truehd; do
    path="$CONFIG/salsa/audiod-$profile.json"
    if [ -f "$path" ]; then
        printf 'ok   %s\n' "$path"
        sed 's/^/     /' "$path"
    else
        printf 'info %s does not exist\n' "$path"
    fi
done

printf '%s\n' "== systemd user units =="
for profile in eac3 truehd; do
    unit="salsa-audiod-$profile.service"
    if systemctl --user is-enabled --quiet "$unit" 2>/dev/null; then
        state=$(systemctl --user is-active "$unit" 2>/dev/null || true)
        printf 'ok   %s (%s)\n' "$unit" "${state:-inactive}"
    else
        printf 'info %s is not enabled\n' "$unit"
    fi
done

printf '%s\n' "== ALSA virtual devices =="
if aplay -L 2>/dev/null | grep -q 'salsa_atmos'; then
    aplay -L 2>/dev/null | grep -A1 'salsa_atmos' | sed 's/^/     /'
else
    printf 'info the Salsa ALSA devices are not in the current ALSA configuration\n'
fi

printf '%s\n' "== shared-memory carriers =="
for name in salsa-hal-carrier-eac3 salsa-hal-carrier-truehd; do
    path="/dev/shm/$name"
    if [ -e "$path" ]; then
        printf 'ok   %s\n' "$path"
    else
        printf 'info %s does not exist yet (start a player first)\n' "$path"
    fi
done

printf '%s\n' "== player settings =="
printf '%s\n' \
    "mpv --ao=alsa --audio-device=alsa/salsa_atmos_eac3 --audio-spdif=eac3 movie.mkv" \
    "mpv --ao=alsa --audio-device=alsa/salsa_atmos_truehd --audio-spdif=truehd movie.mlp"

exit "$STATUS"
