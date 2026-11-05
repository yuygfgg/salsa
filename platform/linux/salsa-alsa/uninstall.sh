#!/bin/sh
set -eu

MODE=system

usage() {
    cat >&2 <<EOF
usage: sudo $0 [--system]
       $0 --user

Removes the Salsa ALSA PCM plug-in and its virtual device definitions.
EOF
    exit 2
}

while [ $# -gt 0 ]; do
    case "$1" in
        --user) MODE=user; shift ;;
        --system) MODE=system; shift ;;
        -h|--help) usage ;;
        *) usage ;;
    esac
done

if [ "$MODE" = system ]; then
    if [ "$(id -u)" -ne 0 ]; then
        printf 'system uninstall must run as root: sudo %s\n' "$0" >&2
        exit 1
    fi
    LIBDIR=$(pkg-config --variable=libdir alsa)/alsa-lib
    rm -f "$LIBDIR/libasound_module_pcm_salsa.so"
    rm -f /etc/alsa/conf.d/60-salsa.conf
    printf 'removed %s/libasound_module_pcm_salsa.so\n' "$LIBDIR"
    printf 'removed /etc/alsa/conf.d/60-salsa.conf\n'
else
    DATA=${XDG_DATA_HOME:-"$HOME/.local/share"}
    CONFIG=${XDG_CONFIG_HOME:-"$HOME/.config"}
    RC="$CONFIG/alsa/asoundrc"
    rm -f "$DATA/salsa/libasound_module_pcm_salsa.so"
    if [ -f "$RC" ]; then
        tmp="$RC.salsa.tmp"
        awk '
            /^# BEGIN SALSA ALSA PCM/ { skip = 1; next }
            /^# END SALSA ALSA PCM/ { skip = 0; next }
            !skip { print }
        ' "$RC" > "$tmp"
        mv "$tmp" "$RC"
    fi
    printf 'removed %s/salsa/libasound_module_pcm_salsa.so\n' "$DATA"
    printf 'updated %s\n' "$RC"
fi
