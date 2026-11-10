# salsa-spatial implementation

This document describes the current software and bitstream profile.
The [codec design](salsa-spatial.md) defines the algorithms and their
mathematical model. The [quality evaluation](salsa-spatial-quality.md)
contains the current measurements.

## Profile

| Property | Current value |
|---|---|
| Bitstream version | Configuration and parameter syntax version 4 |
| Sample rate | 48 kHz |
| Object limit | 128 non-LFE objects |
| Core | 5.1(side)-compatible fallback (FL, FR, FC, LFE, SL, SR) plus two auxiliary height observations |
| Vertical coding angle | Original curve by default; configurable ceiling horizontal gain |
| Transport | One Ogg Opus multistream track, mapping family 1 |
| Declared layout | Standard 7.1; BL and BR share one encoded silence channel |
| Opus streams | Three stereo and three mono streams; nine coded channels |
| Opus packet | 20 ms |
| STFT | 1,920 samples, 240-sample hop, square-root Hann window |
| Parameter bands | 24 |
| Parameter tile | 100 ms |
| Regular anchors | 0, 20, 40, 60, 80, and 95 ms in a full tile |
| Transient refinement | Up to four extra anchors on the 5 ms grid |
| Power model | Per-band temporal delta from the previous reconstructed anchor |
| Inactive powers | Exactly zero; no power symbols are emitted |
| Active power range | 90 dB |
| Power state grid | 0.5 dB |
| Candidate power quantizers | 0.5, 1, 2, and 4 dB |
| Correlation pairs | At most eight disjoint pairs per tile |
| Correlation magnitude | Steps of 1/16, capped at 15/16 |
| Correlation phase | 32 directions |
| Error-envelope quantizer | 2 dB |
| Error prediction | Per-band temporal logarithmic predictor |
| Numerical system floor | 1e-12 (dimensionless, diagonally normalized) |
| Position precision | 12 bits per coordinate |
| Moving-trajectory knot spacing | At most 100 ms |
| Trajectory gain-error bound | 0.005 in L2 norm |
| Independent checkpoint | Once per second |
| Metadata RD weight | Fixed per encode; default beta = 0.00000075 |
| Entropy context lifetime | Persists across tiles; resets at checkpoints |
| Tile metadata transport | One fragment attached to the tile's first Opus packet |
| Configuration resend | Descriptor tables only after the first full record |
| Correction payload | Entropy-coded support, factors, and scales |

The 24 bands use these half-open positive-frequency bin boundaries:

~~~text
0, 2, 4, 7, 11, 15, 20, 27, 34, 43, 55, 68, 85,
105, 130, 160, 196, 240, 293, 358, 436, 532, 648, 789, 961
~~~

Bins are 25 Hz apart. The statistics and Hermitian solve use double precision.
L-BFGS retains six curvature pairs and runs for at most 16 iterations per
refinement pass.

## Core encoding

The core uses libopus 1.6.1 through the Rust opus wrapper. Each substream uses
restricted low delay, complexity 10, and 20 ms packets. DTX, FEC, and DRED are
disabled. The front, surround, auxiliary, and center streams use CBR. LFE and
the shared rear-silence stream use unconstrained VBR.

| Substream | Type | Logical channels | Default target |
|---|---|---|---:|
| 0 | Stereo | FL, FR | 146 kbps |
| 1 | Stereo | SL, SR | 80 kbps |
| 2 | Stereo | H_L, H_R | 88 kbps |
| 3 | Mono | FC | 70 kbps |
| 4 | Mono | LFE | 24 kbps |
| 5 | Mono | Shared rear silence | 16 kbps target; ~1.6 kbps measured |

The five active substreams target 408 kbps by default. The silence value is an encoder setting, not a budget: exact-zero VBR produces three-byte packets, about 1.2 kbps payload (1.6 kbps with framing).

The public family-1 map is `[0, 6, 1, 2, 3, 8, 8, 7]`. The private
`logical_to_coded` permutation is `[0, 1, 6, 7, 2, 3, 4, 5, 8]`. Coded channels
4 and 5 carry the auxiliary height observations and never appear in the public
speaker map.

Metadata, framing, and setup add to the core rate. LFE's actual VBR payload
also varies with the source. The encoder reports each byte category and
calculates total rate from the complete serialized stream.

The reconstruction-matrix correction is enabled by default with rank 4,
support 16, three keyframes per tile, and a rate weight of 0.6.

Core targets and the metadata RD weight are independent settings. The encoder
has no target-rate feedback loop. The quality report records the manually
selected core targets used for its 768 kbps comparisons.

## Source and transport

The DAMF reader resolves inherited metadata and expands non-LFE bed channels
into fixed-position objects. Object order remains stable. Source gain and
activity are applied once to PCM. Reconstructed objects carry unity gain.

A prepass selects one headroom gain for the programme. The encoder applies
it to every transported signal, including LFE and the auxiliary rows. The
object decoder restores it after synthesis. The prepass uses the configured
panner, quantized trajectories, and fixed fold used for the transported audio.

The encoder forms the complete fallback mix `b = g + A h` for the five ground
rows and transports `h` as the auxiliary group. Version 1 fixes
`A = [[0.5,0],[0,0.5],[0,0],[0.5,0],[0,0.5]]`, so heights remain audible in
the stock-player fallback. The decoder reconstructs observations as
`D_base = D_ground + A D_height` and `D_aux = D_height`; it never treats the
folded base rows as the original ground rows.

`--ceiling-horizontal-gain RHO` compresses the vertical coding angle to
`z * acos(RHO)`, with finite `RHO` in `[0, 1)`. Zero is the default and retains
the original curve. At the ceiling, horizontal gain is `RHO` and height gain
is `sqrt(1 - RHO*RHO)`. The fixed fold, channel map, authored positions, and
render targets do not change. For example:

```sh
cargo run --release -p salsa-spatial-encode -- input.atmos \
  --out output.ogg --ceiling-horizontal-gain 0.25
```

A nonzero value requires core-panner model version 2. Configuration version
4 always carries the ceiling gain as one big-endian f64. Base-fold tags remain
zero for no fold or one for the fixed matrix. Ordinary Ogg Opus playback keeps
the same public channel map.

Audio generation, trajectory validation, local RD measurements, and the
decoder use the same configured panner. In particular, the RD reconstruction
matrix includes the fixed fold rather than the unfolded geometric rows.
Reports include the selected ceiling gain and programme headroom gain.
Compression remains opt-in. A three-span Sol Levante comparison at matched
total bitrates found small height and binaural gains at `RHO = 0.25`, with
ground-channel regressions and no gain across all render layouts. See
[the measurement report](salsa-spatial-coding-angle.md) for the rates,
signal errors, and perceptual-proxy results. The default remains zero.

The standard Ogg Opus track carries one multistream packet per 20 ms. The
mono center substream carries versioned Salsa records in an experimental Opus
padding extension. Each tile's complete record bundle is attached to the
tile's first Opus packet as one fragment. Records carry the configuration,
independent checkpoints, and parameter tiles. A tile carries no second object
or event list: downstream `ObjectState` values are generated from the decoded
parameter and trajectory state. The shared silence substream is zero input and
uses unconstrained VBR.

The first configuration record contains the initial objects and trajectories.
Later configuration resends clear those fields: every subsequent checkpoint
carries the complete current power, activity, error, pair, and trajectory
state. If a checkpoint has an empty trajectory, the writer keeps the full
initial scene as a fallback.

The parameter coder uses constriction with 24-bit rANS probability precision.
It assigns integer frequencies by largest remainder, with ties resolved by
symbol index. Counts rescale by ceiling halving when their total reaches
4,096. Entropy contexts persist across parameter tiles and reset at each
independent checkpoint; parameter predictors follow the same boundary. A
checkpoint is a separate length-delimited record with its own reset entropy
contexts, so it can be parsed without any earlier tile or the current tile's
parameter payload. It describes the state immediately before its tile.

The optional reconstruction-correction trailer is entropy coded as well. Its
adaptive models cover keyframe and band presence, rank, shared support, sorted
support gaps, complex factor nibbles, and f32 scales. The trailer remains
length-delimited and can be omitted without affecting the pair baseline.

## Rust components

| Crate | Responsibility |
|---|---|
| salsa-spatial | Descriptors, multistream framing, Ogg Opus, records, syntax, parameters, checkpoints |
| salsa-spatial-core-decoder | Per-substream Opus states and coded-to-logical routing |
| salsa-spatial-dsp | Panner, fold, STFT, covariance, model dispatch, synthesis |
| salsa-spatial-encoder | Source preparation, complete fallback mix, multistream encoding, RD search |
| salsa-spatial-encode | Encoding CLI |
| gst-salsa-spatial | Ogg tile framing and decoder adapters |

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
    E --> I["Ogg Opus multistream"]
    I --> P["salsaspatialparse"]
    P --> D["salsaspatialdec"]
    D --> F["SpatialFrame and ObjectAudioMeta"]
    F --> A["salsaoar"]
    F --> H["salsahrtfrender"]
~~~

## Timing and pipeline behavior

The parser reassembles a complete tile before committing parameter state.
Missing or invalid enhancement selects the decoded bed. A complete checkpoint
restores the predictor chain, and the following valid tile restores object
reconstruction with a 20 ms crossfade between modes. A checkpoint never resets
the continuous Opus or STFT history on its own.

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
| Final core samples | The extra tail packet is transported with Ogg end trimming; other containers are not yet sample-exact. |
| Checkpoint rate cost | Checkpoint bytes count in the file rate but are added after the tile search. |
| Endpoint RD cost | The decoder uses real DC/Nyquist matrices; the search still uses a complex-band proxy. |
| Seeking | `salsaspatialparse` indexes checkpoint byte offsets, snaps flushing time seeks to the preceding checkpoint, restores the segment target, and lets `salsaspatialdec` preroll the Opus/filterbank path before clipped output starts. |
