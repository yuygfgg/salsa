#!/usr/bin/env bash
# Build a minimal macOS .app around the Salsa Studio binary.
#
# The Salsa elements are linked into the binary, so only an optional Rosella
# model is bundled. GStreamer and FFmpeg are still resolved from the host
# installation (Homebrew or the system framework). Run from anywhere:
#
#   crates/apps/salsa-gui/scripts/bundle-macos.sh
#   SALSA_MODEL=/path/to/model.personalized_headphone crates/apps/salsa-gui/scripts/bundle-macos.sh
#
set -euo pipefail

ROOT="$(cd "$(dirname "$0")/../../../.." && pwd)"
APP_NAME="Salsa Studio"
APP="$ROOT/target/release/$APP_NAME.app"
VERSION="0.1.0"

echo "==> Building Salsa Studio (release)"
cargo build --release --manifest-path "$ROOT/Cargo.toml" -p salsa-gui

echo "==> Assembling $APP"
rm -rf "$APP"
mkdir -p "$APP/Contents/MacOS" "$APP/Contents/Resources"
cp "$ROOT/target/release/salsa-gui" "$APP/Contents/MacOS/salsa-gui"
cp "$ROOT/crates/apps/salsa-gui/bundle/Info.plist" "$APP/Contents/Info.plist"
printf 'APPL????' > "$APP/Contents/PkgInfo"

if [[ -n "${SALSA_MODEL:-}" ]]; then
  cp "$SALSA_MODEL" "$APP/Contents/Resources/"
fi

if command -v codesign >/dev/null 2>&1; then
  echo "==> Ad-hoc code signing"
  codesign --force --sign - "$APP"
fi

echo "==> Built $APP ($VERSION)"
