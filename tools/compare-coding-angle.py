#!/usr/bin/env python3
"""Compare coding angles at fixed core targets or a matched total bitrate.

Build salsa-spatial-encode and the render_segment example in release mode.
Use a Python environment with NumPy, SciPy, soundfile, and Zimtohrli.
The report stores commands, binary hashes, rate searches, and measurements.
Temporary WAV files are removed after each layout.
"""

from __future__ import annotations

import argparse
from concurrent.futures import ThreadPoolExecutor
import hashlib
import importlib.metadata
import importlib.util
import json
import math
import os
from pathlib import Path
import subprocess
import time

import numpy as np
from scipy.signal import correlate, correlation_lags
import soundfile as sf


ROOT = Path(__file__).resolve().parents[1]
RATE = 48000
CORE_RATES = [120000, 96000, 120000, 84000]
RATE_FLAGS = ["--front-bitrate", "--surround-bitrate", "--aux-bitrate", "--center-bitrate"]
spec = importlib.util.spec_from_file_location("spatial_metrics", ROOT / "tools/compare-spatial-codecs.py")
metrics = importlib.util.module_from_spec(spec)
spec.loader.exec_module(metrics)


def write_json(path, value):
    temporary = path.with_suffix(".tmp")
    temporary.write_text(json.dumps(value, indent=2, allow_nan=False) + "\n")
    temporary.replace(path)


def digest(path):
    with path.open("rb") as source:
        return hashlib.file_digest(source, "sha256").hexdigest()


def run_encode(args, start, rho, tag, rates, headroom_db=None):
    directory = args.work / f"s{start:g}-r{rho:g}"
    directory.mkdir(parents=True, exist_ok=True)
    stem = directory / tag
    output = stem.with_suffix(".ogg")
    report_path = stem.with_suffix(".json")
    parameters = {"rates": rates, "headroom_db": headroom_db}
    parameter_path = directory / f"{tag}.parameters.json"
    command = [str(args.encoder), str(args.master), "--out", str(output),
               "--reference-start", str(round(start * RATE)),
               "--max-frames", str(round(args.seconds * RATE)),
               "--ceiling-horizontal-gain", str(rho),
               "--lfe-bitrate", "24000", "--beta", "1e-6", "--no-progress"]
    for flag, rate in zip(RATE_FLAGS, rates, strict=True):
        command += [flag, str(rate)]
    if headroom_db is not None:
        command += ["--headroom-db", str(headroom_db)]
    cached = (output.exists() and report_path.exists() and parameter_path.exists()
              and json.loads(parameter_path.read_text()) == parameters)
    started = time.monotonic()
    if not cached:
        environment = dict(os.environ, RAYON_NUM_THREADS=str(args.threads))
        with report_path.open("w") as report, stem.with_suffix(".log").open("w") as log:
            subprocess.run(command, stdout=report, stderr=log, env=environment, check=True)
        write_json(parameter_path, parameters)
    report = json.loads(report_path.read_text())
    if report["duration_samples"] != round(args.seconds * RATE):
        raise ValueError(f"Incomplete encode: {output}")
    actual_rate = output.stat().st_size * 8 * RATE / report["duration_samples"]
    if abs(actual_rate - report["total_bitrate"]) > 1e-6:
        raise ValueError(f"File rate differs from the encoder report: {output}")
    result = {"command": command, "stream": str(output), "core_targets": rates,
              "requested_headroom_db": headroom_db, "elapsed_seconds": time.monotonic() - started,
              "cached": cached, "stream_sha256": digest(output),
              "report": {key: value for key, value in report.items() if key != "per_tile_metadata"}}
    print(f"encoded start={start:g} rho={rho:g} {tag}: {actual_rate / 1000:.3f} kbps, "
          f"headroom={report['headroom_gain']:.6f}", flush=True)
    return result


def match_rate(args, start, rho, initial, common_gain, target_bps=None):
    attempts = [initial]
    directory = args.work / f"s{start:g}-r{rho:g}"
    history = sorted(directory.glob("match-*.parameters.json"), key=lambda path: int(path.name.split("-")[1].split(".")[0]))
    for parameter_path in history:
        tag = parameter_path.name.removesuffix(".parameters.json")
        parameters = json.loads(parameter_path.read_text())
        attempts.append(run_encode(args, start, rho, tag,
                                   parameters["rates"], parameters["headroom_db"]))
    target_bps = args.target_bps if target_bps is None else target_bps
    eligible = [item for item in attempts if item["report"]["headroom_gain"] == common_gain]
    target = target_bps or initial["report"]["total_bitrate"]
    current = min(eligible, key=lambda item: abs(item["report"]["total_bitrate"] - target)) if eligible else initial
    headroom_db = 20 * math.log10(common_gain)
    for iteration in range(len(attempts), len(attempts) + 7):
        rate = current["report"]["total_bitrate"]
        same_gain = current["report"]["headroom_gain"] == common_gain
        if same_gain and (target_bps is None or abs(rate - target_bps) <= args.tolerance_bps):
            return {"attempts": attempts, "selected": current, "matched": True, "target_bps": target_bps}
        rates = current["core_targets"]
        if target_bps is not None:
            points = [(sum(item["core_targets"]), item["report"]["total_bitrate"])
                      for item in attempts if item["report"]["headroom_gain"] == common_gain]
            below = [point for point in points if point[1] < target_bps]
            above = [point for point in points if point[1] > target_bps]
            brackets = [(low, high) for low in below for high in above if low[0] < high[0]]
            core_total = sum(rates) + target_bps - rate
            if brackets:
                low, high = min(brackets, key=lambda pair: pair[1][0] - pair[0][0])
                if high[0] - low[0] < 100:
                    break
                core_total = low[0] + (target_bps - low[1]) * (high[0] - low[0]) / (high[1] - low[1])
            rates = [max(6000, round(value * core_total / sum(rates))) for value in rates]
        current = run_encode(args, start, rho, f"match-{iteration}", rates, headroom_db)
        attempts.append(current)
    below = [item for item in attempts if item["report"]["headroom_gain"] == common_gain
             and item["report"]["total_bitrate"] <= target]
    if not below:
        raise RuntimeError(f"No encode below the rate budget: start={start}, rho={rho}")
    selected = max(below, key=lambda item: item["report"]["total_bitrate"])
    return {"attempts": attempts, "selected": selected, "matched": False, "target_bps": target_bps}


def render(args, source, layout, output, start=None):
    if layout == "binaural":
        stage = (f'salsahrtfrender personalized-headphone="{args.headphone}" '
                 "filterbank=1 profile=mid object-delay=0 tail-seconds=0 output-gain=1")
    else:
        stage = f"salsaoar layout={layout} limiter-enable=false"
    command = [str(args.renderer), f"{source} ! identity name=object_pcm ! {stage}", str(output)]
    if start is not None:
        command += [str(start), str(start + args.seconds)]
    command += ["--warp-mode", "3"]
    with output.with_suffix(".log").open("w") as log:
        subprocess.run(command, stdout=log, stderr=subprocess.STDOUT, check=True)
    audio, rate = sf.read(output, dtype="float64", always_2d=True)
    frames = round(args.seconds * RATE)
    if rate != RATE or len(audio) < frames or audio.shape[1] != len(metrics.LAYOUTS[layout].split()):
        raise ValueError(f"Unexpected render shape: {output}: {audio.shape}, rate={rate}")
    return audio[:frames], {"command": command, "frames_written": len(audio), "frames_measured": frames}


def measure(reference, candidate, labels, zim):
    result = metrics.signal_metrics(reference, candidate, labels)
    groups = {
        "full_range": [i for i, label in enumerate(labels) if label != "LFE"],
        "height": [i for i, label in enumerate(labels) if label.startswith(("Lt", "Rt"))],
        "ground": [i for i, label in enumerate(labels) if label != "LFE" and not label.startswith(("Lt", "Rt"))],
    }
    result["energy"] = {}
    for name, indices in groups.items():
        energy = float(np.sum(reference[:, indices] ** 2))
        error = float(np.sum((reference[:, indices] - candidate[:, indices]) ** 2))
        result["energy"][name] = {"reference": energy, "error": error}
        result[f"{name}_error_db"] = 10 * math.log10(max(error, 1e-30) / energy) if energy > 1e-20 else None
    indices = groups["full_range"]
    span = slice(RATE, min(len(reference), 10 * RATE))
    a = reference[span, indices].sum(axis=1)
    b = candidate[span, indices].sum(axis=1)
    correlations = correlate(b, a, method="fft")
    lags = correlation_lags(len(b), len(a))
    near = np.abs(lags) <= 256
    result["residual_lag_samples"] = int(lags[near][np.argmax(correlations[near])])
    result["delay_samples"] = 0
    result["reference_peak"] = float(np.max(np.abs(reference)))
    result["candidate_peak"] = float(np.max(np.abs(candidate)))
    if zim is not None:
        result.update(metrics.zimtohrli_metrics(reference, candidate, labels, zim, 10 ** (-3 / 20)))
    return result


def summarize(report):
    """Give each segment equal weight; also report pooled signal errors."""
    summary = {}
    for layout in report["settings"]["layouts"]:
        summary[layout] = {}
        for rho in report["settings"]["rhos"]:
            if rho == 0:
                continue
            pairs = [(segment["layouts"][layout]["candidates"]["0"],
                      segment["layouts"][layout]["candidates"][f"{rho:g}"])
                     for segment in report["segments"].values() if layout in segment["layouts"]]
            result = {"segments": len(pairs), "positive_delta": "Candidate improves over baseline"}
            for group in ("full_range", "height", "ground"):
                key = f"{group}_error_db"
                active = [(baseline, candidate) for baseline, candidate in pairs if baseline[key] is not None]
                if not active:
                    continue
                delta = [baseline[key] - candidate[key] for baseline, candidate in active]
                baseline_error = sum(baseline["energy"][group]["error"] for baseline, _ in active)
                candidate_error = sum(candidate["energy"][group]["error"] for _, candidate in active)
                result[group] = {"baseline_mean_error_db": float(np.mean([b[key] for b, _ in active])),
                                 "candidate_mean_error_db": float(np.mean([c[key] for _, c in active])),
                                 "mean_delta_db": float(np.mean(delta)),
                                 "pooled_delta_db": 10 * math.log10(max(baseline_error, 1e-30) / max(candidate_error, 1e-30)),
                                 "min_delta_db": min(delta), "max_delta_db": max(delta),
                                 "improved_segments": sum(value > 0 for value in delta)}
            if pairs and "zimtohrli_distance_mean" in pairs[0][0]:
                key = "zimtohrli_distance_mean"
                delta = [b[key] - c[key] for b, c in pairs]
                result["zimtohrli"] = {
                    "baseline_mean_distance": float(np.mean([b[key] for b, _ in pairs])),
                    "candidate_mean_distance": float(np.mean([c[key] for _, c in pairs])),
                    "mean_distance_reduction": float(np.mean(delta)),
                    "mean_mos_increase": float(np.mean([c["zimtohrli_mos_mean"] - b["zimtohrli_mos_mean"] for b, c in pairs])),
                    "improved_segments": sum(value > 0 for value in delta),
                }
            summary[layout][f"{rho:g}"] = result
    return summary


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("master", type=Path)
    parser.add_argument("--work", type=Path, required=True)
    parser.add_argument("--report", type=Path, required=True)
    parser.add_argument("--starts", type=float, nargs="+", required=True)
    parser.add_argument("--seconds", type=float, required=True)
    parser.add_argument("--rhos", type=float, nargs="+", required=True)
    parser.add_argument("--layouts", nargs="+", choices=metrics.LAYOUTS, required=True)
    parser.add_argument("--target-bps", type=float)
    parser.add_argument("--tolerance-bps", type=float, default=1000)
    parser.add_argument("--zimtohrli", action="store_true")
    parser.add_argument("--headphone", type=Path)
    parser.add_argument("--jobs", type=int, default=2)
    parser.add_argument("--threads", type=int, default=6)
    parser.add_argument("--encoder", type=Path, default=ROOT / "target/release/salsa-spatial-encode")
    parser.add_argument("--renderer", type=Path, default=ROOT / "target/release/examples/render_segment")
    args = parser.parse_args()
    if (args.seconds <= 1 or any(start < 0 for start in args.starts)
            or any(not 0 <= rho < 1 for rho in args.rhos) or 0 not in args.rhos
            or args.jobs < 1 or args.threads < 1 or args.tolerance_bps <= 0
            or (args.target_bps is not None and args.target_bps <= 0)):
        parser.error("Use valid ranges and include rho=0 as the baseline")
    if "binaural" in args.layouts and args.headphone is None:
        parser.error("Binaural rendering requires --headphone")
    args.work.mkdir(parents=True, exist_ok=True)
    args.report.parent.mkdir(parents=True, exist_ok=True)
    settings = {key: str(value.resolve()) if isinstance(value, Path) else value for key, value in vars(args).items()}
    settings.update({"encoder_sha256": digest(args.encoder), "renderer_sha256": digest(args.renderer),
                     "sample_rate": RATE, "core_targets": CORE_RATES, "lfe_target": 24000,
                     "beta": 1e-6, "warp_mode": 3, "metric_gain_db": -3,
                     "alignment": "Source clock; zero extra delay; no gain fit; startup included",
                     "headroom": "Minimum automatic gain across the tested angles for each segment",
                     "versions": {name: importlib.metadata.version(name) for name in ("numpy", "scipy", "soundfile")}})
    zim = None
    if args.zimtohrli:
        import zimtohrli
        zim = zimtohrli.Pyohrli()
        if zim.sample_rate != RATE:
            raise ValueError("Zimtohrli sample rate differs")
        settings["versions"]["zimtohrli"] = importlib.metadata.version("zimtohrli")
        settings["zimtohrli"] = {"num_rotators": zim.num_rotators, "parameters": "Installed binding defaults"}
    work_settings = args.work / "settings.json"
    if work_settings.exists() and json.loads(work_settings.read_text()) != settings:
        raise ValueError("Work directory belongs to different experiment settings")
    write_json(work_settings, settings)
    report = {"settings": settings, "segments": {}}
    if args.report.exists():
        report = json.loads(args.report.read_text())
        if report["settings"] != settings:
            raise ValueError("Report belongs to different experiment settings")
    with ThreadPoolExecutor(max_workers=args.jobs) as pool:
        for start in args.starts:
            segment = report["segments"].setdefault(f"{start:g}", {"encodes": {}, "layouts": {}})
            if all(layout in segment["layouts"] for layout in args.layouts):
                continue
            if not segment["encodes"]:
                initial = list(pool.map(lambda rho: run_encode(args, start, rho, "auto", CORE_RATES), args.rhos))
                common_gain = min(item["report"]["headroom_gain"] for item in initial)
                selected = list(pool.map(lambda pair: match_rate(args, start, pair[0], pair[1], common_gain), zip(args.rhos, initial)))
                # A metadata mode change can leave a gap around the nominal rate.
                # Lower the shared target, then match every angle to that target.
                for _ in range(3):
                    if all(item["matched"] for item in selected):
                        break
                    pair_target = min(item["selected"]["report"]["total_bitrate"] for item in selected)
                    print(f"shared rate start={start:g}: {pair_target / 1000:.3f} kbps", flush=True)
                    selected = list(pool.map(lambda pair: match_rate(args, start, pair[0], pair[1], common_gain, pair_target), zip(args.rhos, initial)))
                if not all(item["matched"] for item in selected):
                    raise RuntimeError(f"Cannot match a common total rate: start={start}")
                segment["encodes"] = {f"{rho:g}": result for rho, result in zip(args.rhos, selected, strict=True)}
                segment["common_headroom_gain"] = common_gain
                segment["pair_target_bps"] = selected[0]["target_bps"]
                write_json(args.report, report)
            for layout in args.layouts:
                if layout in segment["layouts"]:
                    continue
                reference_path = args.work / f"s{start:g}-{layout}-reference.wav"
                reference, ref_render = render(args, f'salsadamfsrc location="{args.master}"', layout, reference_path, start)
                outputs = {}
                for rho in args.rhos:
                    candidate_path = args.work / f"s{start:g}-{layout}-r{rho:g}.wav"
                    stream = segment["encodes"][f"{rho:g}"]["selected"]["stream"]
                    candidate, cand_render = render(args, f'filesrc location="{stream}" ! salsaspatialparse ! salsaspatialdec', layout, candidate_path)
                    outputs[f"{rho:g}"] = measure(reference, candidate, metrics.LAYOUTS[layout].split(), zim)
                    outputs[f"{rho:g}"]["render"] = cand_render
                    candidate_path.unlink()
                    print(f"measured start={start:g} {layout} rho={rho:g}: "
                          f"error={outputs[f'{rho:g}']['full_range_error_db']:.3f} dB", flush=True)
                segment["layouts"][layout] = {"reference_render": ref_render, "candidates": outputs}
                write_json(args.report, report)
                reference_path.unlink()
                del reference, candidate
    report["summary"] = summarize(report)
    write_json(args.report, report)
    print(f"complete: {args.report}", flush=True)


if __name__ == "__main__":
    main()
