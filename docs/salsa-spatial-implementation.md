# salsa-spatial implementation

This document describes the current software and bitstream profile.
The [codec design](salsa-spatial.md) defines the algorithms and their
mathematical model. The [quality evaluation](salsa-spatial-quality.md)
contains the current measurements.

## Profile

| Property | Current value |
|---|---|
| Sample rate | 48 kHz |
| Object limit | 128 non-LFE objects |
| Core | Seven full-range channels and a separate LFE |
| Transport | IAMF 1.1, one 5.1.2 layer |
| Opus streams | Three stereo and two mono streams, mapping family 0 |
| Opus packet | 20 ms |
| STFT | 1,920 samples, 240-sample hop, square-root Hann window |
| Parameter bands | 24 |
| Parameter tile | 100 ms |
| Regular anchors | 0, 20, 40, 60, 80, and 95 ms in a full tile |
| Transient refinement | Up to four extra anchors on the 5 ms grid |
| Fast spectral model | Six DCT-II coefficients per object |
| Active power range | 90 dB |
| Power state grid | 0.5 dB |
| Candidate power quantizers | 0.5, 1, 2, and 4 dB |
| Base quantizer | Twice the selected power quantizer step |
| Correlation pairs | At most eight disjoint pairs per tile |
| Correlation magnitude | Steps of 1/16, capped at 15/16 |
| Correlation phase | 32 directions |
| Error-envelope quantizer | 2 dB |
| Regularization floor | 0.001 |
| Position precision | 12 bits per coordinate |
| Moving-trajectory knot spacing | At most 100 ms |
| Trajectory gain-error bound | 0.005 in L2 norm |
| Predictor snapshot | Once per second |
| Metadata RD weight | Fixed per encode; default beta = 0.000001 |

The 24 bands use these half-open positive-frequency bin boundaries:

~~~text
0, 2, 4, 7, 11, 15, 20, 27, 34, 43, 55, 68, 85,
105, 130, 160, 196, 240, 293, 358, 436, 532, 648, 789, 961
~~~

Bins are 25 Hz apart. The statistics and Hermitian solve use double precision.
L-BFGS retains six curvature pairs and runs for at most 16 iterations per
refinement pass.

## Core encoding

The core uses libopus 1.6.1 through the Rust opus wrapper. Each stream uses
restricted low delay, complexity 10, and 20 ms packets. DTX, FEC, and DRED are
disabled. The four full-range streams use CBR; LFE uses unconstrained VBR.

| Stream | Channels | Default target |
|---|---|---:|
| Front | L, R | 160 kbps |
| Surround | Ls, Rs | 128 kbps |
| Height | H_L, H_R | 160 kbps |
| Center | C | 112 kbps |
| LFE | LFE | 24 kbps |
| Total core target | Eight channels | 584 kbps |

Metadata, framing, and setup add to the core rate. LFE's actual VBR payload
also varies with the source. The encoder reports each byte category and
calculates total rate from the complete serialized stream.

Core targets and the metadata RD weight are independent settings. The encoder
has no target-rate feedback loop. The quality report records the manually
selected core targets used for its 768 kbps comparisons.

## Source and transport

The DAMF reader resolves inherited metadata and expands non-LFE bed channels
into fixed-position objects. Object order remains stable. Source gain and
activity are applied once to PCM. Reconstructed objects carry unity gain.

A prepass selects one headroom gain for the programme. The encoder applies
it to every core channel, including LFE. The object decoder restores it
after synthesis.

IAMF carries the five ordinary Opus substreams. Its 5.1.2 layer maps the
codec's height pair to the front-top pair. Enhancement data occupies padding
extensions in the center stream. The parser extracts this data before
Opus decoding.

The parameter coder uses constriction with 24-bit rANS probability precision.
It assigns integer frequencies by largest remainder, with ties resolved by
symbol index. Counts rescale by ceiling halving when their total reaches
4,096. Entropy contexts reset for each tile; parameter predictors persist
until a snapshot replaces them.

## Rust components

| Crate | Responsibility |
|---|---|
| salsa-spatial | IAMF framing, syntax, parameter prediction, rANS, snapshots |
| salsa-spatial-core-decoder | Five Opus states and aligned bed PCM |
| salsa-spatial-dsp | Panner, STFT, covariance, reconstruction, synthesis |
| salsa-spatial-encoder | Source preparation, core encoding, statistics, RD search |
| salsa-spatial-encode | Encoding CLI |
| gst-salsa-spatial | Parser and decoder adapters |

Codec libraries are independent of GStreamer. The DSP library owns
reconstruction state, reset, and drain behavior.

The encoder uses Rayon for independent peak measurements, spectral analysis,
and RD evaluations. Distortion and gradient terms are computed per analysis
hop, then summed in hop order. This keeps RD decisions independent of worker
scheduling. Opus state and metadata predictors advance in source order.
`RAYON_NUM_THREADS` sets the worker limit.

`encode_with_progress` reports the headroom scan and encoding pass through a
callback on the calling thread. Sample counts refer to the selected source
interval. The CLI displays progress on stderr and writes its JSON report to
stdout. `--no-progress` disables the display.

~~~mermaid
flowchart LR
    S["DAMF source"] --> E["salsa-spatial-encoder"]
    E --> I["IAMF stream"]
    I --> P["salsaspatialparse"]
    P --> D["salsaspatialdec"]
    D --> F["SpatialFrame and ObjectAudioMeta"]
    F --> A["salsaoar"]
    F --> H["salsahrtfrender"]
~~~

## Timing and pipeline behavior

The parser reassembles a complete tile before committing parameter state.
Missing or invalid enhancement selects the decoded bed. A valid snapshot
restores object reconstruction, with a 20 ms crossfade between modes.

All timing uses the 48 kHz source sample clock. Core lookahead is removed
once at programme start. Analysis keeps its window context across tiles.

The GStreamer plugin exposes salsaspatialparse and salsaspatialdec. Its output
uses the shared SpatialFrame representation: LFE followed by source objects.
ObjectAudioMeta carries positions and events. Each output buffer starts with
the resolved trajectory state and carries the remaining ramps. The speaker
and binaural renderers consume that state.

## Current gaps

| Area | Current behavior |
|---|---|
| Programme calibration | Manifest warp mode is not imported or transported. |
| Bed-only gain | IAMF mix gains remain unity; headroom restoration occurs in the object decoder. |
| Final core samples | The local analysis flush packet is not transported; up to 120 tail samples can be unavailable. |
| Snapshot rate cost | Snapshot bytes count in the file rate but are added after the tile search. |
| Endpoint RD cost | The decoder uses real DC/Nyquist matrices; the search still uses a complex-band proxy. |
| Seeking | Decoder reset and snapshots exist; the host must provide core and filterbank preroll. |
