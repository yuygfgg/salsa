#!/usr/bin/env python3
"""Summarize a clock_probe CSV."""

import csv
import json
import sys

import numpy as np


def main():
    if len(sys.argv) != 3:
        print("usage: summarize_clock.py INPUT.csv OUTPUT.json", file=sys.stderr)
        return 2
    rows = list(csv.DictReader(open(sys.argv[1])))
    if not rows:
        print("no rows", file=sys.stderr)
        return 1
    host = np.array([int(r["hal_host_ns"]) for r in rows], dtype=np.int64)
    hal = np.array([int(r["hal_sample"]) for r in rows], dtype=np.int64)
    delta = np.array([float(r["delta"]) for r in rows])
    duration = (host[-1] - host[0]) / 1e9
    summary = {
        "rows": len(rows),
        "duration_s": duration,
        "delta_min_frames": float(delta.min()),
        "delta_max_frames": float(delta.max()),
        "delta_mean_frames": float(delta.mean()),
        "hal_rate_fps": float((hal[-1] - hal[0]) / duration),
        "seeds": sorted({r["shm_seed"] for r in rows}),
        "generations": sorted({r["generation"] for r in rows}),
    }
    json.dump(summary, open(sys.argv[2], "w"), indent=2)
    print(json.dumps(summary, indent=2))
    return 0


if __name__ == "__main__":
    raise SystemExit(main())
