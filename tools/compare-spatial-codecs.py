#!/usr/bin/env python3
"""Render codec outputs and compare all supported speaker layouts.

Use the miniconda Python environment that supplies NumPy, soundfile, SciPy,
ViSQOL, Zimtohrli, and SPAUQ. The render command uses the existing
render_segment example.
"""

from __future__ import annotations

import argparse
import json
import subprocess
from pathlib import Path


LAYOUTS = {
    "2.0": "L R",
    "3.1": "L R C LFE",
    "4.0": "L R Lrs Rrs",
    "5.1": "L R C LFE Ls Rs",
    "7.1": "L R C LFE Lrs Rrs Ls Rs",
    "5.1.2": "L R C LFE Ls Rs Ltm Rtm",
    "5.1.4": "L R C LFE Ls Rs Ltf Rtf Ltr Rtr",
    "7.1.2": "L R C LFE Lrs Rrs Ls Rs Ltm Rtm",
    "7.1.4": "L R C LFE Lrs Rrs Ls Rs Ltf Rtf Ltr Rtr",
    "9.1.4": "L R C LFE Lrs Rrs Ls Rs Ltf Rtf Ltr Rtr Lw Rw",
    "9.1.6": "L R C LFE Lrs Rrs Ls Rs Ltf Rtf Ltr Rtr Ltm Rtm Lw Rw",
    "binaural": "L R",
}
RATE = 48000


def write_json(path, value):
    path.write_text(json.dumps(value, indent=2, allow_nan=False) + "\n")


def render(args):
    args.output.mkdir(parents=True, exist_ok=True)
    jobs = []
    for layout in args.layouts:
        if layout == "binaural":
            stage = (
                f'salsahrtfrender personalized-headphone="{args.headphone}" '
                "filterbank=1 profile=mid object-delay=0 tail-seconds=5 output-gain=1"
            )
        else:
            stage = f"salsaoar layout={layout} limiter-enable=false"
        output = args.output / f"{args.tag}-{layout}.wav"
        command = [str(args.renderer.resolve()), f"{args.source} ! {stage}", str(output)]
        if args.start is not None:
            command += [str(args.start), str(args.stop)]
        if args.pcm_override:
            command += ["--pcm-override", str(args.pcm_override)]
        if args.warp_mode is not None:
            command += ["--warp-mode", str(args.warp_mode)]
        with output.with_suffix(".log").open("w") as log:
            subprocess.run(command, stdout=log, stderr=subprocess.STDOUT, check=True)
        jobs.append({"layout": layout, "labels": LAYOUTS[layout].split(), "command": command})
        print(output.name, flush=True)
    write_json(args.output / f"{args.tag}-renders.json", jobs)


def signal_metrics(reference, candidate, labels):
    import numpy as np

    energy = np.sum(reference * reference, axis=0)
    candidate_energy = np.sum(candidate * candidate, axis=0)
    cross = np.sum(reference * candidate, axis=0)
    error = np.sum((candidate - reference) ** 2, axis=0)

    def ratio(numerator, denominator):
        if denominator <= 1e-20:
            return None
        return float(10 * np.log10(max(numerator, 1e-30) / denominator))

    def group(indices):
        return ratio(error[indices].sum(), energy[indices].sum()) if indices else None

    full_range = [i for i, name in enumerate(labels) if name != "LFE"]
    height = [i for i, name in enumerate(labels) if name.startswith(("Lt", "Rt"))]
    result = {
        "error_db": group(list(range(len(labels)))),
        "full_range_error_db": group(full_range),
        "height_error_db": group(height),
        "level_delta_db": ratio(candidate_energy.sum(), energy.sum()),
        "per_channel": {},
        "per_second_full_range_error_db": [],
    }
    for i, name in enumerate(labels):
        correlation = cross[i] / np.sqrt(energy[i] * candidate_energy[i]) if energy[i] * candidate_energy[i] > 1e-30 else None
        result["per_channel"][name] = {
            "error_db": ratio(error[i], energy[i]),
            "reference_rms_dbfs": ratio(energy[i], len(reference)),
            "level_delta_db": ratio(candidate_energy[i], energy[i]),
            "correlation": float(correlation) if correlation is not None else None,
        }
    for start in range(0, len(reference), RATE):
        r = reference[start:start + RATE, full_range]
        c = candidate[start:start + RATE, full_range]
        result["per_second_full_range_error_db"].append(ratio(np.sum((r - c) ** 2), np.sum(r * r)))
    return result


def perceptual_metrics(reference, candidate, labels, directory, api):
    import numpy as np
    import soundfile as sf
    from spauq.core.metrics import spauq_eval

    directory.mkdir(parents=True, exist_ok=True)
    active = [i for i, label in enumerate(labels) if label != "LFE" and np.mean(reference[:, i] ** 2) > 1e-10]
    pairs = []
    for i in active:
        ref_path = directory / f"{labels[i]}-reference.wav"
        cand_path = directory / f"{labels[i]}-candidate.wav"
        sf.write(ref_path, reference[:, i], RATE, subtype="FLOAT")
        sf.write(cand_path, candidate[:, i], RATE, subtype="FLOAT")
        pairs.append((str(ref_path), str(cand_path)))
    scores = api.measure_batch(pairs, parallel=True, max_workers=2)
    channels = {}
    for i, score in zip(active, scores, strict=True):
        if isinstance(score, Exception):
            raise RuntimeError(f"ViSQOL failed for {labels[i]}: {score}")
        mos, similarity = float(score.moslqo), float(score.vnsim)
        if not np.isfinite([mos, similarity]).all():
            raise ValueError(f"ViSQOL returned a non-finite score for {labels[i]}")
        channels[labels[i]] = {"moslqo": mos, "vnsim": similarity}
    spatial = []
    for start in range(0, len(reference), 10 * RATE):
        r = reference[start:start + 10 * RATE, active]
        c = candidate[start:start + 10 * RATE, active]
        if len(r) < RATE // 2:
            continue
        values = spauq_eval(reference=r.T.astype(np.float32), estimate=c.T.astype(np.float32), fs=RATE,
                            align_mode=None, scale_mode=None, verbose=False)
        spatial.append({key: float(np.median(value)) for key, value in values.items() if key in ("SSR", "SRR")})
    return {
        "visqol_mean": float(np.mean([v["moslqo"] for v in channels.values()])),
        "visqol_min": float(min(v["moslqo"] for v in channels.values())),
        "visqol_channels": channels,
        "spauq_chunks": spatial,
        "spauq_ssr": float(np.median([v["SSR"] for v in spatial])),
        "spauq_srr": float(np.median([v["SRR"] for v in spatial])),
    }


def zimtohrli_metrics(reference, candidate, labels, api, gain):
    import numpy as np
    import zimtohrli

    channels = {}
    for i, label in enumerate(labels):
        if label == "LFE" or np.mean(reference[:, i] ** 2) <= 1e-10:
            continue
        a = np.asarray(reference[:, i] * gain, dtype=np.float32)
        b = np.asarray(candidate[:, i] * gain, dtype=np.float32)
        if max(np.max(np.abs(a)), np.max(np.abs(b))) > 1.0:
            raise ValueError("Zimtohrli input exceeds full scale; select a common --metric-gain-db")
        distance = float(api.distance(a, b))
        mos = float(zimtohrli.mos_from_zimtohrli(distance))
        if not np.isfinite([distance, mos]).all():
            raise ValueError(f"Zimtohrli returned a non-finite score for {label}")
        channels[label] = {"distance": distance, "mos": mos}
    if not channels:
        raise ValueError("Zimtohrli has no active full-range reference channel")
    return {
        "zimtohrli_distance_mean": float(np.mean([v["distance"] for v in channels.values()])),
        "zimtohrli_mos_mean": float(np.mean([v["mos"] for v in channels.values()])),
        "zimtohrli_channels": channels,
    }


def measure(args):
    import numpy as np
    import soundfile as sf
    from scipy.signal import correlate, correlation_lags

    api = None
    if args.perceptual:
        from visqol import VisqolApi
        api = VisqolApi()
        api.create(mode="audio")
    zim = None
    if args.zimtohrli:
        import zimtohrli
        zim = zimtohrli.Pyohrli()
        if zim.sample_rate != RATE:
            raise ValueError("Zimtohrli sample rate differs from the render rate")
    report = {"sample_rate": RATE, "frames": args.frames, "reference": args.reference,
              "alignment": "Fixed candidate delay only; no gain fit or channel remapping.", "layouts": {}}
    if api:
        import importlib.metadata
        report["perceptual"] = {
            "visqol_version": importlib.metadata.version("visqol-python"),
            "spauq_version": importlib.metadata.version("spauq"),
            "visqol_mode": "audio; default internal SPL matching and alignment",
            "spauq_settings": "10-second blocks; alignment and scaling disabled",
        }
    if zim:
        import importlib.metadata
        report["zimtohrli"] = {
            "version": importlib.metadata.version("zimtohrli"),
            "bands": zim.num_rotators,
            "common_gain_db": args.metric_gain_db,
            "parameters": "Installed C++ binding defaults; no per-candidate normalization.",
        }
    suffix = "perceptual" if api else "zimtohrli" if zim else "signal"
    destination = args.report or args.output / f"{args.reference}-{suffix}.json"
    for layout in args.layouts:
        reference, rate = sf.read(args.output / f"{args.reference}-{layout}.wav", dtype="float64", always_2d=True)
        labels = LAYOUTS[layout].split()
        if rate != RATE or reference.shape[1] != len(labels) or len(reference) < args.frames:
            raise ValueError(f"invalid reference shape or rate for {layout}")
        reference = reference[:args.frames]
        outputs = {}
        for entry in args.candidate:
            tag, delay_text = entry.rsplit(":", 1)
            delay = int(delay_text)
            if delay < 0:
                raise ValueError("candidate delay must be nonnegative")
            candidate, rate = sf.read(args.output / f"{tag}-{layout}.wav", dtype="float64", always_2d=True)
            candidate = candidate[delay:delay + args.frames]
            if rate != RATE or candidate.shape != reference.shape:
                raise ValueError(f"candidate span or channel count differs: {tag} {layout}")
            result = signal_metrics(reference, candidate, labels)
            # Check the chosen fixed delay without changing the measurement.
            indices = [i for i, name in enumerate(labels) if name != "LFE"]
            stop = min(len(reference), 15 * RATE)
            a = reference[5 * RATE:stop, indices].sum(axis=1)
            b = candidate[5 * RATE:stop, indices].sum(axis=1)
            correlations = correlate(b, a, method="fft")
            lags = correlation_lags(len(b), len(a))
            near = np.abs(lags) <= 256
            result["residual_lag_samples"] = int(lags[near][np.argmax(correlations[near])])
            result["delay_samples"] = delay
            if api:
                result.update(perceptual_metrics(reference, candidate, labels, args.output / "metrics" / tag / layout, api))
            if zim:
                result.update(zimtohrli_metrics(reference, candidate, labels, zim, 10 ** (args.metric_gain_db / 20)))
            outputs[tag] = result
            print(f"{layout} {tag}: error {result['error_db']:.3f} dB", flush=True)
        report["layouts"][layout] = {"labels": labels, "candidates": outputs}
        write_json(destination, report)


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    commands = parser.add_subparsers(dest="command", required=True)
    render_parser = commands.add_parser("render")
    render_parser.add_argument("--source", required=True, help="GStreamer source and decoder pipeline")
    render_parser.add_argument("--tag", required=True)
    render_parser.add_argument("--renderer", type=Path, default=Path("target/release/examples/render_segment"))
    render_parser.add_argument("--headphone", type=Path, required=True)
    render_parser.add_argument("--pcm-override", type=Path)
    render_parser.add_argument("--warp-mode", type=int, choices=range(4))
    render_parser.add_argument("--start", type=float)
    render_parser.add_argument("--stop", type=float)
    measure_parser = commands.add_parser("measure")
    measure_parser.add_argument("--reference", required=True)
    measure_parser.add_argument("--candidate", action="append", required=True, help="TAG:DELAY_SAMPLES")
    measure_parser.add_argument("--frames", type=int, required=True)
    measure_parser.add_argument("--perceptual", action="store_true")
    measure_parser.add_argument("--zimtohrli", action="store_true")
    measure_parser.add_argument("--metric-gain-db", type=float, default=0.0,
                                help="Common gain for both Zimtohrli inputs; does not change waveform metrics")
    measure_parser.add_argument("--report", type=Path)
    for sub in [render_parser, measure_parser]:
        sub.add_argument("--output", type=Path, required=True)
        sub.add_argument("--layouts", nargs="+", choices=list(LAYOUTS), default=list(LAYOUTS))
    args = parser.parse_args()
    if args.command == "render":
        if (args.start is None) != (args.stop is None):
            parser.error("--start and --stop must be supplied together")
        render(args)
    else:
        if args.frames <= 5 * RATE:
            parser.error("measure requires more than five seconds")
        measure(args)


if __name__ == "__main__":
    main()
