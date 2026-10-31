#!/usr/bin/env bash
# Verify that the workspace has no standalone `oamd` package: the upstream
# parser is merged into `objectaudio-oamd`.
set -euo pipefail

SCRIPT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
ROOT="$(cd "$SCRIPT_DIR/.." && pwd)"

sources="$(
    cargo metadata --manifest-path "$ROOT/Cargo.toml" --format-version 1 \
        | python3 -c '
import json, sys
meta = json.load(sys.stdin)
for package in meta["packages"]:
    if package["name"] == "oamd":
        print(package.get("source") or package["manifest_path"])
'
)"
count="$(printf '%s\n' "$sources" | sed '/^$/d' | wc -l | tr -d ' ')"
if [[ "$count" != "0" ]]; then
    echo "expected no standalone oamd package, found $count:" >&2
    printf '%s\n' "$sources" >&2
    exit 1
fi
echo "no standalone oamd package; parser lives in objectaudio-oamd"

hits="$(
    grep -rn --include='*.rs' --include='Cargo.toml' \
        -E '(^|[^a-zA-Z_])use oamd::' "$ROOT/crates" \
        | grep -v '/objectaudio-oamd/' || true
)"
if [[ -n "$hits" ]]; then
    echo "crates/* must use objectaudio-oamd, not oamd directly" >&2
    printf '%s\n' "$hits" >&2
    exit 1
fi
echo "no direct oamd imports above objectaudio-oamd"
