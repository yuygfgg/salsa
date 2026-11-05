#!/bin/sh
set -eu

ROOT=$(CDPATH= cd -- "$(dirname -- "$0")" && pwd)
CONF="$ROOT/salsa.conf.in"
LIB=${SALSA_ALSA_LIB:-"$ROOT/build/libasound_module_pcm_salsa.so"}
MODE=system
DEST=

usage() {
    cat >&2 <<EOF
usage: sudo $0 [--system] [--lib PATH]
       $0 --user [--lib PATH]

Installs the Salsa ALSA PCM plug-in and defines the salsa_atmos_eac3 and
salsa_atmos_truehd virtual output devices.

  --user          install under \$HOME and update ~/.config/alsa/asoundrc
  --system        install under the ALSA libdir and /etc/alsa/conf.d (default)
  --lib PATH      plug-in shared library (default: platform/linux/salsa-alsa/build)
EOF
    exit 2
}

while [ $# -gt 0 ]; do
    case "$1" in
        --user) MODE=user; shift ;;
        --system) MODE=system; shift ;;
        --lib) LIB=$2; shift 2 ;;
        -h|--help) usage ;;
        *) usage ;;
    esac
done

if [ ! -f "$LIB" ]; then
    printf 'plug-in not found: %s\nrun %s/build.sh first\n' "$LIB" "$ROOT" >&2
    exit 1
fi

if [ "$MODE" = system ]; then
    if [ "$(id -u)" -ne 0 ]; then
        printf 'system install must run as root: sudo %s\n' "$0" >&2
        printf 'use --user for a per-user install without root\n' >&2
        exit 1
    fi
    LIBDIR=$(pkg-config --variable=libdir alsa)/alsa-lib
    DEST="$LIBDIR/libasound_module_pcm_salsa.so"
    mkdir -p "$LIBDIR"
    install -m 0644 "$LIB" "$DEST"
    mkdir -p /etc/alsa/conf.d
    sed "s|@SALSA_ALSA_LIB@|$DEST|g" "$CONF" > /etc/alsa/conf.d/60-salsa.conf
    printf 'installed %s\n' "$DEST"
    printf 'installed /etc/alsa/conf.d/60-salsa.conf\n'
else
    DATA=${XDG_DATA_HOME:-"$HOME/.local/share"}
    CONFIG=${XDG_CONFIG_HOME:-"$HOME/.config"}
    DEST="$DATA/salsa/libasound_module_pcm_salsa.so"
    RC="$CONFIG/alsa/asoundrc"
    mkdir -p "$DATA/salsa" "$CONFIG/alsa"
    install -m 0755 "$LIB" "$DEST"
    BEGIN='# BEGIN SALSA ALSA PCM'
    END='# END SALSA ALSA PCM'
    if [ -f "$RC" ]; then
        tmp="$RC.salsa.tmp"
        awk -v begin="$BEGIN" -v end="$END" '
            $0 == begin { skip = 1; next }
            $0 == end { skip = 0; next }
            !skip { print }
        ' "$RC" > "$tmp"
    else
        : > "$RC"
        tmp="$RC.salsa.tmp"
        : > "$tmp"
    fi
    {
        printf '%s\n' "$BEGIN"
        sed "s|@SALSA_ALSA_LIB@|$DEST|g" "$CONF"
        printf '%s\n' "$END"
    } >> "$tmp"
    mv "$tmp" "$RC"
    printf 'installed %s\n' "$DEST"
    printf 'updated %s\n' "$RC"
fi
printf '%s\n' "virtual devices: salsa_atmos_eac3, salsa_atmos_truehd"
