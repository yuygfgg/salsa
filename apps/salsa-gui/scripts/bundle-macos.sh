#!/usr/bin/env bash
# Build a minimal macOS .app around the Salsa Studio binary.
#
# It bundles the Salsa plugin dylibs and an optional Rosella model. GStreamer
# and FFmpeg are still resolved from the host installation (Homebrew or the
# system framework). Run from anywhere:
#
#   apps/salsa-gui/scripts/bundle-macos.sh
#   SALSA_MODEL=/path/to/model.personalized_headphone apps/salsa-gui/scripts/bundle-macos.sh
#
set -euo pipefail

ROOT="$(cd "$(dirname "$0")/../../.." && pwd)"
APP_NAME="Salsa Studio"
APP="$ROOT/apps/salsa-gui/target/release/$APP_NAME.app"
VERSION="0.1.0"

echo "==> Building GStreamer plugins (release)"
cargo build --release --manifest-path "$ROOT/Cargo.toml" \
  -p gst-damf -p gst-eac3joc -p gst-objectaudio -p gst-truehd

echo "==> Building Salsa Studio (release)"
cargo build --release --manifest-path "$ROOT/apps/salsa-gui/Cargo.toml"

echo "==> Assembling $APP"
rm -rf "$APP"
mkdir -p "$APP/Contents/MacOS" "$APP/Contents/Resources/gstreamer-1.0"
cp "$ROOT/apps/salsa-gui/target/release/salsa-gui" "$APP/Contents/MacOS/salsa-gui"
cp "$ROOT/apps/salsa-gui/bundle/Info.plist" "$APP/Contents/Info.plist"
printf 'APPL????' > "$APP/Contents/PkgInfo"

for library in \
  libgstsalsadamf.dylib \
  libgstsalsaeac3joc.dylib \
  libgstsalsatruehd.dylib \
  libgstsalsaobjectaudio.dylib
do
  cp "$ROOT/target/release/$library" "$APP/Contents/Resources/gstreamer-1.0/"
done

if [[ -n "${SALSA_MODEL:-}" ]]; then
  cp "$SALSA_MODEL" "$APP/Contents/Resources/"
fi

if command -v codesign >/dev/null 2>&1; then
  echo "==> Ad-hoc code signing"
  for library in "$APP"/Contents/Resources/gstreamer-1.0/*.dylib; do
    codesign --force --sign - "$library"
  done
  codesign --force --sign - "$APP"
fi

echo "==> Built $APP ($VERSION)"
