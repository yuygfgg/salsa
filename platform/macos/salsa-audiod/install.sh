#!/bin/sh
set -eu

ROOT=$(CDPATH= cd -- "$(dirname -- "$0")" && pwd)
REPO=$(CDPATH= cd -- "$ROOT/../../.." && pwd)
BIN=${SALSA_AUDIOD_BIN:-"$REPO/target/release/salsa-audiod"}
LABEL=dev.salsa.audio.audiod
PREFIX="$HOME/Library/Application Support/Salsa"
INSTALLED="$PREFIX/salsa-audiod"
LOG_DIR="$HOME/Library/Logs/Salsa"
LOAD=0

usage() {
    cat >&2 <<EOF
usage: $0 [options]

Installs the service binary and the E-AC-3 and TrueHD LaunchAgents. Each agent
has its own configuration and can be configured with
`salsa-audiod configure --agent eac3|truehd`.

  --load          load both LaunchAgents now (explicit action)
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

mkdir -p "$PREFIX" "$LOG_DIR" "$HOME/Library/LaunchAgents"
cp "$BIN" "$INSTALLED"
chmod 0755 "$INSTALLED"
codesign --force --sign - "$INSTALLED"
codesign --verify --strict "$INSTALLED"

write_plist() {
    label=$1
    config=$2
    plist=$3
    log=$4
    SA_LABEL="$label" SA_BINARY="$INSTALLED" SA_CONFIG="$config" \
    SA_LOG="$log" SA_PLIST="$plist" python3 - <<'PY'
import os
import plistlib

document = {
    "Label": os.environ["SA_LABEL"],
    "ProgramArguments": [
        os.environ["SA_BINARY"], "run",
        "--config", os.environ["SA_CONFIG"],
    ],
    "RunAtLoad": True,
    "KeepAlive": {"SuccessfulExit": False},
    "ProcessType": "Interactive",
    "ThrottleInterval": 5,
    "StandardOutPath": os.environ["SA_LOG"],
    "StandardErrorPath": os.environ["SA_LOG"],
}
with open(os.environ["SA_PLIST"], "wb") as handle:
    plistlib.dump(document, handle)
PY
}

for profile in eac3 truehd; do
    profile_config="$PREFIX/audiod-$profile.json"
    profile_label="$LABEL.$profile"
    profile_plist="$HOME/Library/LaunchAgents/$profile_label.plist"
    if [ ! -f "$profile_config" ]; then
        "$INSTALLED" configure --agent "$profile" --config "$profile_config" \
            --device system --layout 7.1.4 --no-declare-latency
    fi
    write_plist "$profile_label" "$profile_config" "$profile_plist" \
        "$LOG_DIR/audiod-$profile.log"
    printf 'installed agent:   %s\n' "$profile_plist"
    printf 'profile config:    %s\n' "$profile_config"
done

LINK=
if command -v brew >/dev/null 2>&1; then
    BREW_BIN=$(brew --prefix)/bin
    if [ -w "$BREW_BIN" ]; then
        LINK="$BREW_BIN/salsa-audiod"
    fi
fi
if [ -z "$LINK" ] && [ -w /usr/local/bin ]; then
    LINK=/usr/local/bin/salsa-audiod
fi
if [ -z "$LINK" ]; then
    mkdir -p "$HOME/.local/bin"
    LINK="$HOME/.local/bin/salsa-audiod"
fi
ln -sf "$INSTALLED" "$LINK"

printf '%s\n' "installed service: $INSTALLED"
printf '%s\n' "command:           $LINK"
printf '%s\n' "log dir:           $LOG_DIR"

if [ "$LOAD" -eq 1 ]; then
    for profile in eac3 truehd; do
        launchctl bootout "gui/$(id -u)/$LABEL.$profile" 2>/dev/null || true
    done
    sleep 0.3
    for profile in eac3 truehd; do
        profile_label="$LABEL.$profile"
        profile_plist="$HOME/Library/LaunchAgents/$profile_label.plist"
        launchctl bootstrap "gui/$(id -u)" "$profile_plist" 2>/dev/null || {
            sleep 0.5
            launchctl bootstrap "gui/$(id -u)" "$profile_plist"
        }
        printf '%s\n' "loaded $profile_label"
    done
else
    printf '%s\n' "not loaded (explicit action):"
    printf '%s\n' "  launchctl bootstrap gui/$(id -u) \"$HOME/Library/LaunchAgents/$LABEL.eac3.plist\""
    printf '%s\n' "  launchctl bootstrap gui/$(id -u) \"$HOME/Library/LaunchAgents/$LABEL.truehd.plist\""
fi
printf '%s\n' "after changing a profile, restart it with:"
printf '%s\n' "  salsa-audiod configure --agent eac3 --restart"
printf '%s\n' "  salsa-audiod configure --agent truehd --restart"
