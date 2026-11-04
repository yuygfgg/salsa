#!/bin/sh
set -eu

ROOT=$(CDPATH= cd -- "$(dirname -- "$0")" && pwd)
REPO=$(CDPATH= cd -- "$ROOT/../../.." && pwd)
BIN=${SALSA_AUDIOD_BIN:-"$REPO/target/release/salsa-audiod"}
LABEL=dev.salsa.audio.audiod
PREFIX="$HOME/Library/Application Support/Salsa"
INSTALLED="$PREFIX/salsa-audiod"
PLIST="$HOME/Library/LaunchAgents/$LABEL.plist"
LOG_DIR="$HOME/Library/Logs/Salsa"
CONFIG=${SALSA_AUDIOD_CONFIG:-"$PREFIX/audiod.json"}
LOAD=0

usage() {
    cat >&2 <<EOF
usage: $0 [options]

Installs the service binary and LaunchAgent. Output device and render settings
are configured separately with `salsa-audiod configure`.

  --config PATH   configuration file (default: $CONFIG)
  --load          load the LaunchAgent now (explicit action)
  --bin PATH      service binary (default: target/release/salsa-audiod)
EOF
    exit 2
}

while [ $# -gt 0 ]; do
    case "$1" in
        --config) CONFIG=$2; shift 2 ;;
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

mkdir -p "$PREFIX" "$LOG_DIR" "$(dirname "$PLIST")"
cp "$BIN" "$INSTALLED"
chmod 0755 "$INSTALLED"
codesign --force --sign - "$INSTALLED"
codesign --verify --strict "$INSTALLED"

SA_LABEL=$LABEL SA_BINARY=$INSTALLED SA_CONFIG=$CONFIG \
SA_LOAD=$LOAD SA_LOG_DIR=$LOG_DIR SA_PLIST=$PLIST python3 - <<'PY'
import os
import plistlib

log = os.path.join(os.environ["SA_LOG_DIR"], "audiod.log")
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
    "StandardOutPath": log,
    "StandardErrorPath": log,
}
with open(os.environ["SA_PLIST"], "wb") as handle:
    plistlib.dump(document, handle)
PY

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
printf '%s\n' "installed agent:   $PLIST"
printf '%s\n' "command:           $LINK"
printf '%s\n' "config:            $CONFIG"
printf '%s\n' "log:               $LOG_DIR/audiod.log"
case ":$PATH:" in
    *":$(dirname "$LINK"):"*) ;;
    *) printf 'add %s to PATH to run `salsa-audiod` directly\n' "$(dirname "$LINK")" ;;
esac

if [ ! -f "$CONFIG" ]; then
    printf '%s\n' "no configuration yet; choose an output first:"
    printf '%s\n' "  $INSTALLED configure"
fi

if [ "$LOAD" -eq 1 ]; then
    if [ ! -f "$CONFIG" ]; then
        printf '%s\n' "--load requested but $CONFIG does not exist; the service will exit until it is configured" >&2
    fi
    launchctl bootout "gui/$(id -u)/$LABEL" 2>/dev/null || true
    sleep 0.3
    launchctl bootstrap "gui/$(id -u)" "$PLIST" 2>/dev/null || {
        sleep 0.5
        launchctl bootstrap "gui/$(id -u)" "$PLIST"
    }
    printf '%s\n' "loaded $LABEL"
else
    printf '%s\n' "not loaded (explicit action):"
    printf '%s\n' "  launchctl bootstrap gui/$(id -u) \"$PLIST\""
fi
printf '%s\n' "after changing settings, restart with:"
printf '%s\n' "  launchctl kickstart -k gui/$(id -u)/$LABEL"
