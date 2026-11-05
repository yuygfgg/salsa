#!/bin/sh
set -eu

ROOT=$(CDPATH= cd -- "$(dirname -- "$0")" && pwd)
REPO=$(CDPATH= cd -- "$ROOT/../../.." && pwd)
BIN=${SALSA_AUDIOD_BIN:-"$REPO/target/release/salsa-audiod"}
DATA=${XDG_DATA_HOME:-"$HOME/.local/share"}
CONFIG=${XDG_CONFIG_HOME:-"$HOME/.config"}
UNIT_DIR="$CONFIG/systemd/user"
PREFIX="$DATA/salsa"
INSTALLED="$PREFIX/salsa-audiod"
LOAD=0

usage() {
    cat >&2 <<EOF
usage: $0 [options]

Installs the service binary and the E-AC-3 and TrueHD user services. Each
service has its own configuration and can be configured with
\`salsa-audiod configure --agent eac3|truehd\`.

  --load          enable and start both services now
  --bin PATH      service binary (default: target/release/salsa-audiod)
EOF
    exit 2
}

while [ $# -gt 0 ]; do
    case "$1" in
        --load) LOAD=1; shift ;;
        --bin) BIN=$2; shift 2 ;;
        -h|--help) usage ;;
        *) usage ;;
    esac
done

if [ ! -x "$BIN" ]; then
    printf 'service binary not found: %s\n' "$BIN" >&2
    printf 'run %s/build.sh first\n' "$ROOT" >&2
    exit 1
fi

mkdir -p "$PREFIX" "$CONFIG/salsa" "$UNIT_DIR"
cp "$BIN" "$INSTALLED"
chmod 0755 "$INSTALLED"

write_unit() {
    profile=$1
    unit=$2
    config_path=$3
    cat > "$UNIT_DIR/$unit" <<EOF
[Unit]
Description=Salsa virtual IEC 61937 service ($profile)
After=pipewire.service pipewire-pulse.service pulseaudio.service

[Service]
Type=simple
ExecStart=$INSTALLED run --config $config_path
Restart=on-failure
RestartSec=5

[Install]
WantedBy=default.target
EOF
    printf 'installed unit:  %s/%s\n' "$UNIT_DIR" "$unit"
}

for profile in eac3 truehd; do
    profile_config="$CONFIG/salsa/audiod-$profile.json"
    if [ ! -f "$profile_config" ]; then
        "$INSTALLED" configure --agent "$profile" --config "$profile_config" \
            --device system --layout 7.1.4 --no-declare-latency
    fi
    write_unit "$profile" "salsa-audiod-$profile.service" "$profile_config"
    printf 'profile config:  %s\n' "$profile_config"
done

if [ -d "$HOME/.local/bin" ]; then
    ln -sf "$INSTALLED" "$HOME/.local/bin/salsa-audiod"
fi

printf '%s\n' "installed service: $INSTALLED"
if [ "$LOAD" -eq 1 ]; then
    systemctl --user daemon-reload
    systemctl --user enable --now salsa-audiod-eac3.service salsa-audiod-truehd.service
    printf '%s\n' "enabled and started both services"
else
    printf '%s\n' "not loaded (explicit action):"
    printf '%s\n' "  systemctl --user daemon-reload"
    printf '%s\n' "  systemctl --user enable --now salsa-audiod-eac3.service salsa-audiod-truehd.service"
fi
printf '%s\n' "after changing a profile, restart it with:"
printf '%s\n' "  salsa-audiod configure --agent eac3 --restart"
printf '%s\n' "  salsa-audiod configure --agent truehd --restart"
