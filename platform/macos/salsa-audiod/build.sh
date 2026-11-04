#!/bin/sh
set -eu

ROOT=$(CDPATH= cd -- "$(dirname -- "$0")" && pwd)
REPO=$(CDPATH= cd -- "$ROOT/../../.." && pwd)

cargo build --release -p salsa-audiod --manifest-path "$REPO/Cargo.toml"

BIN="$REPO/target/release/salsa-audiod"
codesign --force --sign - "$BIN"
codesign --verify --strict "$BIN"

printf '%s\n' "$BIN"
shasum -a 256 "$BIN"
