#!/bin/sh
set -eu

HERE=$(CDPATH= cd -- "$(dirname -- "$0")" && pwd)
ROOT=$(CDPATH= cd -- "$HERE/.." && pwd)
WORKSPACE=$(CDPATH= cd -- "$ROOT/../../../.." && pwd)
OUT="$ROOT/fixtures"

EAC3_SOURCE=${1:-"$WORKSPACE/atmos_opensource.ec3"}
TRUEHD_SOURCE=${2:-"$WORKSPACE/16ch.mlp"}

mkdir -p "$OUT"

ffmpeg -y -v error -t 20 -i "$EAC3_SOURCE" -c:a copy "$OUT/eac3_20s.ec3"
ffmpeg -y -v error -t 20 -i "$TRUEHD_SOURCE" -c:a copy -f truehd "$OUT/truehd_20s.mlp"
ffmpeg -y -v error -i "$OUT/eac3_20s.ec3" -c:a copy -f spdif "$OUT/eac3_20s.spdif"
ffmpeg -y -v error -i "$OUT/truehd_20s.mlp" -c:a copy -f spdif "$OUT/truehd_20s.spdif"

python3 "$HERE/inspect_spdif.py" "$OUT/eac3_20s.spdif" "$OUT/truehd_20s.spdif"
