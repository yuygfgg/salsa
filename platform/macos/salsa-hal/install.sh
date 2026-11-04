#!/bin/sh
set -eu

ROOT=$(CDPATH= cd -- "$(dirname -- "$0")" && pwd)
TARGET_DIR=/Library/Audio/Plug-Ins/HAL
BUNDLES=

usage() {
    cat >&2 <<EOF
usage: sudo $0 [BUNDLE ...]

Installs the Salsa HAL driver bundles. The E-AC-3 and TrueHD variants have
distinct bundle identifiers, so they coexist:

  (no arguments)         install build-eac3 and build-truehd
  BUNDLE ...             one or more already-built codec bundles
EOF
    exit 2
}

while [ $# -gt 0 ]; do
    case "$1" in
        -h|--help) usage ;;
        -*) usage ;;
        *) BUNDLES="$BUNDLES $1" ;;
    esac
    shift
done

if [ -z "$BUNDLES" ]; then
    BUNDLES="$ROOT/build-eac3/SalsaHAL.driver $ROOT/build-truehd/SalsaHAL.driver"
fi

if [ "$(id -u)" -ne 0 ]; then
    printf '%s\n' "install.sh must run as root: sudo $0" >&2
    exit 1
fi

install_bundle() {
    bundle=$1
    if [ ! -d "$bundle" ]; then
        printf 'build the requested bundle first: %s\n' "$bundle" >&2
        exit 1
    fi
    identifier=$(plutil -extract CFBundleIdentifier raw -o - \
        "$bundle/Contents/Info.plist" 2>/dev/null) || {
        printf 'cannot read CFBundleIdentifier from %s\n' "$bundle" >&2
        exit 1
    }
    case "$identifier" in
        *eac3*) name=SalsaHAL-EAC3.driver ;;
        *truehd*) name=SalsaHAL-TrueHD.driver ;;
        *)
            printf 'not a Salsa codec bundle: %s (%s)\n' "$bundle" "$identifier" >&2
            exit 1
            ;;
    esac
    target="$TARGET_DIR/$name"
    codesign --verify --strict "$bundle"
    rm -rf "$target"
    cp -R "$bundle" "$target"
    chown -R root:wheel "$target"
    chmod -R a+rX,go-w "$target"
    printf 'installed %s (%s)\n' "$target" "$identifier"
}

for bundle in $BUNDLES; do
    install_bundle "$bundle"
done

printf '%s\n' "restarting coreaudiod"
killall coreaudiod || true
