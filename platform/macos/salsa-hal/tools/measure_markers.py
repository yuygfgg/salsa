#!/usr/bin/env python3
"""Measure scheduled click markers from a loopback capture."""

import argparse
import csv
import json
import pathlib

import numpy as np


def load_capture(path, rate, channels):
    raw = np.fromfile(path, dtype="<f4")
    if raw.size % channels:
        raw = raw[:-(raw.size % channels)]
    frames = raw.reshape(-1, channels)
    energy = np.sum(frames * frames, axis=1)
    window = max(1, int(rate * 0.010))
    envelope = np.convolve(energy, np.ones(window) / window, mode="same")
    threshold = max(float(envelope.max()) * 0.05, 1e-8)
    above = envelope > threshold
    starts = []
    i = 0
    while i < len(above):
        if above[i]:
            j = i
            while j < len(above) and above[j]:
                j += 1
            starts.append(i)
            i = j
        else:
            i += 1
    return frames, starts, threshold


def read_markers(path):
    rows = []
    with open(path, newline="") as f:
        for row in csv.DictReader(f):
            rows.append({
                "sync": int(row["sync"]),
                "carrier_sample": int(row["carrier_sample"]),
                "host_ns": int(row["host_ns"]),
                "latency_frames": int(row["latency_frames"]),
                "kind": int(row["kind"]),
            })
    return rows


def nearest(values, target):
    if not values:
        return None
    return min(values, key=lambda v: abs(v - target))


def main():
    ap = argparse.ArgumentParser()
    ap.add_argument("--capture", required=True)
    ap.add_argument("--meta", required=True)
    ap.add_argument("--markers", required=True)
    ap.add_argument("--out", required=True)
    args = ap.parse_args()

    meta = json.load(open(args.meta))
    rate = float(meta["rate"])
    first_host_ns = int(meta["first_host_ns"])
    markers = read_markers(args.markers)
    frames, starts, threshold = load_capture(args.capture, rate, int(meta.get("channels", 2)))

    for marker in markers:
        # Detection indices and expected positions are relative to file frame 0.
        sample = (marker["host_ns"] - first_host_ns) * rate / 1e9
        marker["expected_sample"] = sample

    offsets = []
    for marker in markers:
        detected = nearest(starts, marker["expected_sample"])
        marker["detected_sample"] = detected
        if detected is not None:
            offsets.append(detected - marker["expected_sample"])
    median_offset = float(np.median(offsets)) if offsets else 0.0
    residuals = [o - median_offset for o in offsets]
    max_abs_residual = max((abs(r) for r in residuals), default=0.0)

    pair_markers = [m for m in markers if m["kind"] in (0, 1)]
    pair_ms = None
    pair_error_ms = None
    if len(pair_markers) >= 2 and pair_markers[0]["kind"] == 0 and pair_markers[1]["kind"] == 1:
        a = pair_markers[0]["detected_sample"]
        b = pair_markers[1]["detected_sample"]
        if a is not None and b is not None:
            pair_ms = (b - a) * 1000.0 / rate
            declared_ms = (pair_markers[1]["host_ns"] - pair_markers[0]["host_ns"]) / 1e6
            pair_error_ms = pair_ms - declared_ms

    refs = [m for m in markers if m["kind"] in (0, 2) and m["detected_sample"] is not None]
    drift_ms = None
    if len(refs) >= 2:
        observed = (refs[-1]["detected_sample"] - refs[0]["detected_sample"]) * 1000.0 / rate
        expected_ms = (refs[-1]["host_ns"] - refs[0]["host_ns"]) / 1e6
        drift_ms = observed - expected_ms

    summary = {
        "capture": str(pathlib.Path(args.capture)),
        "rate": rate,
        "frames": int(len(frames)),
        "duration_s": len(frames) / rate,
        "threshold": threshold,
        "click_count": len(starts),
        "marker_count": len(markers),
        "declared_latency_frames": markers[0]["latency_frames"] if markers else None,
        "declared_latency_ms": (markers[0]["latency_frames"] * 1000.0 / 192000.0) if markers else None,
        "pair_ms": pair_ms,
        "pair_error_ms": pair_error_ms,
        "median_offset_samples": median_offset,
        "max_abs_residual_samples": max_abs_residual,
        "drift_ms": drift_ms,
        "click_starts": starts,
        "markers": markers,
    }
    with open(args.out, "w") as f:
        json.dump(summary, f, indent=2)
    print(json.dumps({k: summary[k] for k in (
        "click_count", "marker_count", "declared_latency_frames",
        "declared_latency_ms", "pair_ms", "pair_error_ms",
        "median_offset_samples", "max_abs_residual_samples", "drift_ms")}, indent=2))
    return 0


if __name__ == "__main__":
    raise SystemExit(main())
