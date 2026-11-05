#!/bin/sh
set -eu

ROOT=$(CDPATH= cd -- "$(dirname -- "$0")" && pwd)
REPO=$(CDPATH= cd -- "$ROOT/../../.." && pwd)

cargo build --release -p salsa-audiod --manifest-path "$REPO/Cargo.toml" -j 1

printf '%s\n' "$REPO/target/release/salsa-audiod"
