#!/usr/bin/env bash
# Validate gst-plugin-oamd (oamdcapsfeature / oamdserialize / dlbdapvr) on the
# salsa decoder + salsaobjectaudioadapt stream, following the file layout of
# gst-plugin-oamd/atmos_tools.py.
#
#   tools/test-atmos-tools-adapter.sh /path/to/stream.ec3
#   tools/test-atmos-tools-adapter.sh /path/to/master.mlp
#
# The Dolby Reference Player is x86_64-only, so this builds the salsa plugins
# for x86_64 and runs under Rosetta like tools/test-dolby-interop.sh.
set -euo pipefail

SCRIPT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
# shellcheck source=x86-interop-env.sh
source "$SCRIPT_DIR/x86-interop-env.sh"

STREAM="${1:-${SALSA_TEST_EC3:-}}"
if [[ -z "$STREAM" || ! -f "$STREAM" ]]; then
    echo "usage: $0 /path/to/stream.ec3|master.mlp" >&2
    exit 1
fi
"$SCRIPT_DIR/build-x86-plugins.sh"

WORK="$SALSA_X86_CACHE/atmos-tools-adapter"
rm -rf "$WORK"
mkdir -p "$WORK/ref" "$WORK/ours"
trap 'status=$?; if [[ $status -eq 0 && "${SALSA_KEEP_INTEROP:-0}" != "1" ]]; then rm -rf "$WORK"; fi' EXIT
ATMOS_TOOLS="$SALSA_REPO_ROOT/../gst-plugin-oamd/atmos_tools.py"
if [[ ! -f "$ATMOS_TOOLS" ]]; then
    echo "atmos_tools.py not found at $ATMOS_TOOLS" >&2
    exit 1
fi

read -r HEADER < <(xxd -l 8 -p "$STREAM")
case "$HEADER" in
    0b77*) CODEC=eac3 ;;
    *)     CODEC=truehd ;;
esac
echo "== codec: $CODEC =="

export GST_PLUGIN_PATH="$SALSA_X86_PLUGINS:$SALSA_GST_PLUGIN_OAMD_PATH"
export GST_REGISTRY="$WORK/ours-registry.bin"
rm -f "$GST_REGISTRY"

echo "== reference: atmos_tools.py --mode decode =="
PYTHONUNBUFFERED=1 GST_REGISTRY_FORK=no GST_REGISTRY="$WORK/ref-registry.bin" \
    python3 "$ATMOS_TOOLS" -i "$STREAM" -o "$WORK/ref/out" --mode decode

echo "== salsa: decoder + salsaobjectaudioadapt + gst-plugin-oamd =="
if [[ "$CODEC" == "eac3" ]]; then
    DECODER=(salsaeac3jocparse ! salsaeac3jocdec)
else
    DECODER=(salsatruehdparse ! salsatruehdatmosdec)
fi
"$SALSA_GST_LAUNCH" -e filesrc location="$STREAM" blocksize=1048576 ! \
    "${DECODER[@]}" ! salsaobjectaudioadapt direction=to-dolby ! \
    tee name=t \
    t. ! queue ! oamdcapsfeature remove=true ! audioconvert ! \
        audio/x-raw,format=S24LE,layout=interleaved ! filesink location="$WORK/ours/out.pcm" \
    t. ! queue ! oamdserialize ! filesink location="$WORK/ours/out.metadata"

OBJECT_COUNT="$(python3 - "$WORK/ours/out.metadata" <<'PY'
import re, sys
ids = set()
for line in open(sys.argv[1]):
    m = re.match(r"^\s*-\s*ID:\s*(\d+)\s*$", line)
    if m and int(m.group(1)) >= 10:
        ids.add(int(m.group(1)))
print(len(ids))
PY
)"
CHANNELS=$((OBJECT_COUNT + 1))
echo "  detected $OBJECT_COUNT objects -> $CHANNELS channels"

ffmpeg -y -v error -f s24le -ar 48k -ac "$CHANNELS" -i "$WORK/ours/out.pcm" \
    -c:a copy "$WORK/ours/out.caf"

python3 - "$WORK" "$CHANNELS" <<'PY'
import pathlib, re, sys
work = pathlib.Path(sys.argv[1])
channels = int(sys.argv[2])

def ids(path):
    found = set()
    for line in path.read_text().splitlines():
        m = re.match(r"^\s*-\s*ID:\s*(\d+)\s*$", line)
        if m:
            found.add(int(m.group(1)))
    return found

def events(path):
    return sum(1 for line in path.read_text().splitlines()
               if re.match(r"^\s*-\s*ID:\s*\d+\s*$", line))

ours = work / "ours/out.metadata"
ref = work / "ref/out.metadata"
ours_ids, ref_ids = ids(ours), ids(ref)
ours_events, ref_events = events(ours), events(ref)
print(f"  metadata events: ours={ours_events} ref={ref_events}")
print(f"  object IDs:       ours={sorted(ours_ids)} ref={sorted(ref_ids)}")
if ours_ids != ref_ids:
    raise SystemExit("object ID sets differ")
if abs(ours_events - ref_events) > max(32, ref_events // 200):
    raise SystemExit(f"event count differs by too much: {ours_events} vs {ref_events}")

template = pathlib.Path(work / "ours/out.atmos")
object_ids = sorted({*range(10, 25), *[i for i in ours_ids if i >= 10]})
content = f"""
version: 0.5.1
presentations:
  - type: home
    simplified: false
    metadata: out.metadata
    audio: out.caf
    offset: 0
    fps: 24.0
    scBedConfiguration: [3]
    creationTool: YUYGFGG
    creationToolVersion: 0.1.0
    downmixType_5to2: LoRo_Stereo
    51-to-20_LsRs90degPhaseShift: false
    warpMode: warping
    bedInstances:
      - channels:
          - channel: LFE
            ID: 3
    objects:
{chr(10).join(f"      - ID: {i}" for i in object_ids)}
"""
template.write_text(content)
print(f"  .atmos:           {template}")
print(f"  CAF channels:     {channels}")
PY

if ! cmp -s "$WORK/ref/out.atmos" "$WORK/ours/out.atmos"; then
    echo "generated .atmos manifest differs from atmos_tools.py" >&2
    diff -u "$WORK/ref/out.atmos" "$WORK/ours/out.atmos" | head -40 >&2
    exit 1
fi
echo "  .atmos manifest:  identical to atmos_tools.py"

echo "== reference binaural =="
PYTHONUNBUFFERED=1 GST_REGISTRY_FORK=no GST_REGISTRY="$WORK/ref-registry.bin" \
    python3 "$ATMOS_TOOLS" -i "$STREAM" -o "$WORK/ref/binaural.wav" --mode binaural

echo "== salsa binaural through adapter + dlbdapvr =="
"$SALSA_GST_LAUNCH" -e filesrc location="$STREAM" blocksize=1048576 ! \
    "${DECODER[@]}" ! salsaobjectaudioadapt direction=to-dolby ! \
    oamdcapsfeature float=true ! dlbdapvr ! wavenc ! \
    filesink location="$WORK/ours/binaural.wav"

python3 - "$WORK" <<'PY'
import pathlib, subprocess, sys
import numpy as np
work = pathlib.Path(sys.argv[1])
def decode(path):
    out = subprocess.check_output(
        ["ffmpeg", "-v", "error", "-i", str(path), "-f", "f32le", "-ac", "2", "-"],
        stderr=subprocess.DEVNULL)
    return np.frombuffer(out, dtype="<f4").reshape(-1, 2).astype(np.float64)
a = decode(work / "ours/binaural.wav")
b = decode(work / "ref/binaural.wav")
n = min(len(a), len(b)); a, b = a[:n], b[:n]
print(f"  binaural frames:  ours={len(a)} ref={len(b)}")
for c in range(2):
    x, y = a[:, c], b[:, c]
    gain = np.dot(x, y) / max(np.dot(x, x), 1e-30)
    err = np.sum((x * gain - y) ** 2)
    snr = 10 * np.log10(np.sum(y * y) / max(err, 1e-30))
    corr = np.corrcoef(x, y)[0, 1]
    print(f"  ch{c}: corr={corr:.6f} gain={20*np.log10(abs(gain)+1e-30):+.2f} dB "
          f"snr_after_gain={snr:.2f} dB")
    if corr < 0.99:
        raise SystemExit("binaural waveform correlation below 0.99")
PY

echo "atmos_tools.py + gst-plugin-oamd + salsa adapter: passed" >&2
