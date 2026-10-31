#!/usr/bin/env bash
# Verify the external gst-plugin-oamd DAMF export chain.
#
#   tools/test-damf-export.sh [stream.ec3|stream.mlp] [--binaural] [--cavern] [--clean]
#
# The stream comes from the first argument or $SALSA_TEST_STREAM. The script:
#   1. runs gst-plugin-oamd/atmos_tools.py --mode decode;
#   2. checks .atmos / .metadata / .caf / .pcm and the manifest references;
#   3. compares .pcm and .metadata with a direct Dolby decode;
#   4. verifies the CAF decodes back to the same PCM;
#   5. optionally runs the binaural mode and a Cavern DAMF render.
#
# gst-plugin-oamd is used as-is; this script never modifies it.
set -euo pipefail

SCRIPT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
REPO_ROOT="$(cd "$SCRIPT_DIR/.." && pwd)"
OAMD_REPO="${SALSA_GST_PLUGIN_OAMD_REPO:-$REPO_ROOT/../gst-plugin-oamd}"
DRP_APP="${SALSA_DRP_APP:-/Applications/Dolby/Dolby Reference Player.app}"
DRP_RESOURCES="${SALSA_DRP_RESOURCES:-$DRP_APP/Contents/Frameworks/GStreamer.framework/Versions/1_16/Resources}"
WORK="${SALSA_DAMF_WORK:-/tmp/salsa-damf}"
OAMD_PLUGIN_PATH="${SALSA_GST_PLUGIN_OAMD_PATH:-$OAMD_REPO/build/cmake-x86_64/reference-player-plugins}"

RUN_BINAURAL=0
RUN_CAVERN=0
CLEAN=0
STREAM=""
for arg in "$@"; do
    case "$arg" in
        --binaural) RUN_BINAURAL=1 ;;
        --cavern) RUN_CAVERN=1 ;;
        --clean) CLEAN=1 ;;
        -*) echo "Unknown option: $arg" >&2; exit 2 ;;
        *) STREAM="$arg" ;;
    esac
done

if [[ ! -f "$OAMD_REPO/atmos_tools.py" ]]; then
    echo "gst-plugin-oamd not found at $OAMD_REPO" >&2
    echo "Set SALSA_GST_PLUGIN_OAMD_REPO to its checkout." >&2
    exit 1
fi
if [[ ! -d "$OAMD_PLUGIN_PATH" ]]; then
    echo "gst-plugin-oamd build not found at $OAMD_PLUGIN_PATH" >&2
    exit 1
fi
if [[ ! -d "$DRP_RESOURCES" ]]; then
    echo "Dolby Reference Player runtime not found at $DRP_RESOURCES" >&2
    exit 1
fi

STREAM="${STREAM:-${SALSA_TEST_STREAM:-}}"
if [[ -z "$STREAM" || ! -f "$STREAM" ]]; then
    echo "Pass a raw E-AC-3 or TrueHD stream." >&2
    exit 1
fi

mkdir -p "$WORK"
STEM="$(basename "$STREAM")"
BASE="$WORK/${STEM%.*}"
rm -f "$BASE".atmos "$BASE".metadata "$BASE".caf "$BASE".pcm "$BASE".direct.pcm \
    "$BASE".direct.metadata "$BASE".roundtrip.pcm "$BASE".binaural.wav \
    "$BASE".atmos.audio "$BASE".atmos.metadata "$BASE".cavern_5.1.wav

CODEC="$(python3 - "$STREAM" <<'PY'
import sys
data = open(sys.argv[1], 'rb').read(4096)
if data.startswith(b'\x0b\x77'):
    print('eac3')
elif b'\xf8\x72\x6f\xba' in data:
    print('truehd')
else:
    print('unknown')
PY
)"
if [[ "$CODEC" == unknown ]]; then
    echo "$STREAM is not a raw E-AC-3 or TrueHD stream." >&2
    exit 1
fi

echo "== gst-plugin-oamd =="
if git -C "$OAMD_REPO" rev-parse --short HEAD >/dev/null 2>&1; then
    echo "  commit: $(git -C "$OAMD_REPO" rev-parse --short HEAD)"
fi
echo "  plugin: $OAMD_PLUGIN_PATH"
echo "  stream: $STREAM ($CODEC)"

echo "== DAMF export (atmos_tools.py --mode decode) =="
OAMD_PLUGIN_PATH="$OAMD_PLUGIN_PATH" \
    python3 "$OAMD_REPO/atmos_tools.py" -i "$STREAM" --mode decode -o "$BASE"

for extension in atmos metadata caf pcm; do
    if [[ ! -s "$BASE.$extension" ]]; then
        echo "missing or empty: $BASE.$extension" >&2
        exit 1
    fi
done
if ! grep -q "metadata: $(basename "$BASE").metadata" "$BASE.atmos"; then
    echo "manifest does not reference the metadata sidecar" >&2
    exit 1
fi
if ! grep -q "audio: $(basename "$BASE").caf" "$BASE.atmos"; then
    echo "manifest does not reference the CAF audio" >&2
    exit 1
fi
EVENTS="$(grep -c '^  - ID:' "$BASE.metadata")"
if [[ "$EVENTS" -eq 0 ]]; then
    echo "metadata has no events" >&2
    exit 1
fi
echo "  manifest: $BASE.atmos"
echo "  metadata events: $EVENTS"

echo "== direct decode comparison =="
export GST_PLUGIN_SCANNER="$DRP_RESOURCES/libexec/gst-plugin-scanner"
export GST_PLUGIN_PATH="$OAMD_PLUGIN_PATH"
export GST_PLUGIN_SYSTEM_PATH_1_0="$DRP_RESOURCES/lib/gstreamer-1.0"
export GST_REGISTRY_FORK=no
export GST_REGISTRY="$WORK/registry.bin"
GST_LAUNCH="$DRP_RESOURCES/bin/gst-launch-1.0"
if [[ "$CODEC" == eac3 ]]; then
    DECODE=(dlbac3parse enable-metadata=true ! dlbac3dec out-ch-config=21)
else
    DECODE=(dlbtruehdparse align-major-sync=false ! dlbtruehddec out-ch-config=21 presentation=16)
fi
"$GST_LAUNCH" -q filesrc location="$STREAM" ! "${DECODE[@]}" \
    ! oamdcapsfeature remove=true ! audioconvert \
    ! audio/x-raw,format=S24LE,layout=interleaved \
    ! filesink location="$BASE.direct.pcm"
"$GST_LAUNCH" -q filesrc location="$STREAM" ! "${DECODE[@]}" \
    ! oamdserialize ! filesink location="$BASE.direct.metadata"
cmp "$BASE.pcm" "$BASE.direct.pcm"
cmp "$BASE.metadata" "$BASE.direct.metadata"
echo "  PCM and metadata are identical to the direct Dolby decode"

echo "== CAF round-trip =="
ffmpeg -y -v error -i "$BASE.caf" -f s24le -acodec pcm_s24le "$BASE.roundtrip.pcm"
cmp "$BASE.pcm" "$BASE.roundtrip.pcm"
echo "  CAF decodes back to the same 24-bit PCM"

if [[ "$RUN_BINAURAL" -eq 1 ]]; then
    echo "== binaural mode =="
    OAMD_PLUGIN_PATH="$OAMD_PLUGIN_PATH" \
        python3 "$OAMD_REPO/atmos_tools.py" -i "$STREAM" --mode binaural \
        -o "$BASE.binaural.wav"
    [[ -s "$BASE.binaural.wav" ]]
    echo "  $BASE.binaural.wav"
fi

if [[ "$RUN_CAVERN" -eq 1 ]]; then
    echo "== Cavern DAMF read-back =="
    # Cavern 2.1.0 expects the standard DAMF sidecar names. atmos_tools.py
    # writes <stem>.metadata / <stem>.caf and references them in the manifest;
    # create the sidecar aliases without touching gst-plugin-oamd.
    ln -sf "$(basename "$BASE").metadata" "$BASE.atmos.metadata"
    ln -sf "$(basename "$BASE").caf" "$BASE.atmos.audio"
    if command -v aq >/dev/null 2>&1; then
        aq render "$BASE.atmos" --layout 5.1 --out "$BASE.cavern_5.1.wav"
        [[ -s "$BASE.cavern_5.1.wav" ]]
        echo "  $BASE.cavern_5.1.wav"
    else
        echo "  aq not found; skipping (DAMF sidecars are present)" >&2
    fi
fi

rm -f "$BASE.direct.pcm" "$BASE.direct.metadata" "$BASE.roundtrip.pcm"
if [[ "$CLEAN" -eq 1 ]]; then
    rm -f "$BASE.pcm" "$BASE.caf" "$BASE.atmos.audio" "$BASE.atmos.metadata"
fi

echo "DAMF export verification passed."
du -h "$BASE".atmos "$BASE".metadata 2>/dev/null || true
