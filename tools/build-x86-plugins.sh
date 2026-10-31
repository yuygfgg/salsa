#!/usr/bin/env bash
# Build the salsa plugins for x86_64 and stage them next to the Dolby runtime.
set -euo pipefail

SCRIPT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
# shellcheck source=x86-interop-env.sh
source "$SCRIPT_DIR/x86-interop-env.sh"

cd "$SALSA_REPO_ROOT"
"$SALSA_X86_CARGO" build --release \
    -p gst-eac3joc -p gst-objectaudio -p gst-truehd \
    --target x86_64-apple-darwin

mkdir -p "$SALSA_X86_PLUGINS"
for name in libgstsalsaeac3joc.dylib libgstsalsaobjectaudio.dylib libgstsalsatruehd.dylib; do
    source_file="target/x86_64-apple-darwin/release/$name"
    cp -f "$source_file" "$SALSA_X86_PLUGINS/$name"
    # The plugin references @rpath/lib/libgstreamer-1.0.0.dylib; point that at
    # the Reference Player runtime through the space-free symlink.
    if ! otool -l "$SALSA_X86_PLUGINS/$name" | grep -q "$SALSA_DRP_LINK"; then
        install_name_tool -add_rpath "$SALSA_DRP_LINK" "$SALSA_X86_PLUGINS/$name"
    fi
done

echo "Staged x86_64 plugins in $SALSA_X86_PLUGINS" >&2
