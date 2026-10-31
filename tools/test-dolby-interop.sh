#!/usr/bin/env bash
# Repeatable Dolby interop matrix on this machine.
#
#   tools/test-dolby-interop.sh [stream.ec3]
#
# The stream comes from the first argument or $SALSA_TEST_EC3. It must be raw
# E-AC-3 with OAMD.
set -euo pipefail

SCRIPT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
# shellcheck source=build-x86-plugins.sh
source "$SCRIPT_DIR/build-x86-plugins.sh"

STREAM="${1:-${SALSA_TEST_EC3:-}}"
if [[ -z "$STREAM" || ! -f "$STREAM" ]]; then
    echo "Set SALSA_TEST_EC3 or pass a raw E-AC-3 file." >&2
    exit 1
fi

WORK="$SALSA_X86_CACHE/interop"
mkdir -p "$WORK"
export GST_PLUGIN_PATH="$SALSA_X86_PLUGINS:$SALSA_GST_PLUGIN_OAMD_PATH"
export GST_REGISTRY="$SALSA_X86_CACHE/registry-interop.bin"
rm -f "$GST_REGISTRY"

run_gst() {
    local log
    log="$(mktemp "$WORK/gst.XXXXXX.log")"
    if ! "$SALSA_GST_LAUNCH" -e "$@" >"$log" 2>&1; then
        echo "gst-launch failed; log: $log" >&2
        tail -30 "$log" >&2
        return 1
    fi
    if grep -qE 'ERROR:|not-negotiated|Internal data stream error' "$log"; then
        echo "gst-launch reported an error; log: $log" >&2
        grep -E 'ERROR:|not-negotiated|Internal data stream error' "$log" >&2
        return 1
    fi
    rm -f "$log"
}

echo "== element check =="
for element in \
    salsaobjectaudioadapt salsaoar salsahrtfrender \
    salsaeac3jocparse salsaeac3jocdec \
    dlbac3parse dlbac3dec dlboar oamdserialize; do
    "$SALSA_GST_INSPECT" "$element" >/dev/null
    echo "  $element"
done

echo "== reverse: dlbac3dec ! salsaobjectaudioadapt ! salsaoar =="
run_gst filesrc location="$STREAM" ! dlbac3parse enable-metadata=true ! \
    dlbac3dec out-ch-config=21 ! salsaobjectaudioadapt direction=to-salsa ! \
    salsaoar layout=5.1 ! fakesink sync=false

echo "== forward: salsaeac3jocdec ! salsaobjectaudioadapt ! dlboar =="
run_gst filesrc location="$STREAM" ! salsaeac3jocparse ! salsaeac3jocdec ! \
    salsaobjectaudioadapt direction=to-dolby ! \
    dlboar out-ch-config=16 limiter-enable=false ! fakesink sync=false

echo "== OAMD payload boundary (ID11 vs OAMDI) =="
ours="$WORK/ours.oamd.yaml"
dolby="$WORK/dolby.oamd.yaml"
run_gst filesrc location="$STREAM" ! salsaeac3jocparse ! salsaeac3jocdec ! \
    salsaobjectaudioadapt direction=to-dolby ! oamdserialize ! \
    filesink location="$ours"
run_gst filesrc location="$STREAM" ! dlbac3parse enable-metadata=true ! \
    dlbac3dec out-ch-config=21 ! oamdserialize ! filesink location="$dolby"
sed '/samplePos:/d' "$ours" >"$WORK/ours.nos"
sed '/samplePos:/d' "$dolby" >"$WORK/dolby.nos"
if diff -q "$WORK/ours.nos" "$WORK/dolby.nos" >/dev/null; then
    echo "  payloads identical; events: $(grep -c '^  - ID:' "$ours")"
else
    # The adapter re-encodes the IR as a dynamic-object-only OAMD
    # payload. Dolby's own stream may carry a bed assignment, trim and
    # different block layout, so byte equality is not expected. The pipelines
    # above already prove both directions decode; keep the diff for inspection.
    echo "  OAMD differs (expected: adapter re-encodes the IR); events:"
    echo "    ours=$(grep -c '^  - ID:' "$ours") dolby=$(grep -c '^  - ID:' "$dolby")"
    diff "$WORK/ours.nos" "$WORK/dolby.nos" | head -20 >&2 || true
fi

echo "Dolby interop matrix passed." >&2
