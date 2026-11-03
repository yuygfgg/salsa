# Rendering

Two renderers consume the object-only IR: a speaker renderer and a binaural
headphone renderer. Both read the salsa `SpatialPayload` from
`ObjectAudioMeta`. Dolby input must pass through `salsaobjectaudioadapt`.

## Speaker layouts

`objectaudio-render` ships eleven layouts with a fixed output channel order:

| Layout | Channels |
|---|---|
| `2.0` | L, R |
| `3.1` | L, R, C, LFE |
| `4.0` | L, R, Lrs, Rrs |
| `5.1` | L, R, C, LFE, Ls, Rs |
| `7.1` | L, R, C, LFE, Ls, Rs, Lrs, Rrs |
| `5.1.2` | L, R, C, LFE, Ls, Rs, Ltm, Rtm |
| `5.1.4` | L, R, C, LFE, Ls, Rs, Ltf, Rtf, Ltr, Rtr |
| `7.1.2` | L, R, C, LFE, Ls, Rs, Lrs, Rrs, Ltm, Rtm |
| `7.1.4` | L, R, C, LFE, Ls, Rs, Lrs, Rrs, Ltf, Rtf, Ltr, Rtr |
| `9.1.4` | L, R, C, LFE, Ls, Rs, Lrs, Rrs, Lw, Rw, Ltf, Rtf, Ltr, Rtr |
| `9.1.6` | L, R, C, LFE, Ls, Rs, Lrs, Rrs, Lw, Rw, Ltf, Rtf, Ltm, Rtm, Ltr, Rtr |

Each layout carries Q15 speaker points, axis groups, the attenuation table and
the mapping between internal and standard channel order.

## Programme speaker controls

OAMD trim elements travel through the IR as `SpatialPayload::calibration`
(`ProgramCalibration`): warp mode, the nine layout configurations and the
per-object bypass flags. The renderer selects the configuration for its target
layout, so a stream can switch between 5.1 and 7.1 without re-decoding.

- `warp 0`: floor Y unchanged; `warp 1`: `clamp(2Y, 0, 1)`. The height layer is
  never warped.
- `warp 2` / `warp 3` render 5.1/5.1.2/5.1.4 through the matching 7.1.x layout
  and fold the rear pair into the target surround pair with PLIIx or Lo/Ro
  coefficients. 4.0 adds a rear pair from the 7.1 geometry; layouts with both
  surround pairs, or none, render directly.
- Trim is automatic (layout position compensation), disabled (unity), or
  explicit (centre/surround/height levels). Balance is applied only with an
  explicit configuration. Per-object bypass keeps warp and folding but drops
  balance and trim.
- Only 2.0, 3.1, 4.0, 5.1, 7.1, 5.1.2 and 5.1.4 carry trim; wider layouts use
  disabled trim.
- A payload without a trim element keeps the previous controls. The first
  calibration is seeded at the scene start, matching the reference.
- `salsahrtfrender` ignores calibration; binaural rendering never applies these
  controls.

`SpeakerRenderer::process` takes the selected `SpeakerControls`; streaming uses
`controls_from_calibration` on the `SceneTimeline` calibration and callers with
no calibration use `SpeakerControls::default_for(layout)`.

## Speaker renderer

`SpeakerRenderer` accepts interleaved `f32` `[samples][LFE + objects]` and
produces interleaved `f64` `[samples][layout channels]`:

- per-region axis-group equal-power interpolation, including the height blend;
- gain ramps in 32-sample blocks, with state carried across calls;
- LFE copied to the layout's LFE channel;
- validation of block multiples, nondecreasing offsets, region index, finite
  gains and Q15 coordinate range.

`bed_channel_q15` and `layout_bed_anchor` describe output speakers only; they
are not an input bed path. Non-LFE beds are expanded by the decoder/source into
fixed-position objects before reaching the renderer. This includes
`salsadamfsrc`, which expands every non-LFE `bedInstances` channel before it
emits the IR.

## Binaural renderer

`RosellaRenderer::submit_frame` accepts planar `LFE + N` PCM plus an optional
`SpatialPayload` and its snapshot offset. The timeline tracks object positions
from the payload snapshot and its events; there is no OAMD frame argument and no
bed-position injection.

## `salsaoar`

| Property | Default | Meaning |
|---|---|---|
| `layout` | `5.1` | Target layout. A recognized downstream `channel-mask` wins, so a 5.1 mask with `layout=7.1.4` renders 5.1. Set it before the pipeline reaches PAUSED; the renderer is built in `set_caps`. |
| `metadata-offset` | `0` | Extra sample offset for spatial snapshots and events. Clamped to the input buffer length. |
| `limiter-enable` | `false` | Scale a buffer by `1/peak` when its peak exceeds 0 dBFS. This is a safety normalizer, not Dolby DRC. |
| `discard-latency` | `false` | Compatibility property with no effect. Output remains sample-aligned. |

Caps:

- sink: `audio/x-raw(memory:SystemMemory, meta:ObjectAudioMeta)` and
  `audio/x-raw(meta:ObjectAudioMeta)`; `F32LE`, interleaved, 32/44.1/48 kHz,
  `channels=[1, 1024]`;
- src: standard `audio/x-raw`, `F32LE`, interleaved, one structure per layout
  with its GStreamer channel mask.

A buffer without metadata keeps the previous panning state.

The element accepts arbitrary PCM frame counts. It queues PCM and metadata on
the same timeline, then passes complete 32-sample blocks to `SpeakerRenderer`.
Up to 31 samples remain queued between input buffers. The element reports this
buffering latency through the `LATENCY` query. Output PTS, duration and sample
offsets describe the emitted PCM, including samples retained from earlier
buffers.

EOS, a new segment, or new caps drain the queued partial block first. Only the
internal renderer input is zero-padded; output contains the original sample
count. `FLUSH_STOP` and `DISCONT` discard the queue and reset panning and gain
ramps, so a seek cannot reuse PCM or metadata from the previous timeline.

## Binaural renderer

`objectaudio-hrtf` implements the Dolby `.personalized_headphone` (Rosella)
model:

- JSON parsing with header/checksum validation;
- 64-channel HQMF table A, 20 hybrid groups / 36 direction terms of table B,
  36×36 direction matrices, distance profiles and field lanes;
- a coefficient SHA-256 computed over the little-endian int32 lane image.

Supported models use table A (dimension 64, option 3) plus table B (20 groups,
36 terms), no table C, at 48 kHz. SOFA/HDF5 is not supported.

Two filterbanks are available:

| `filterbank` | Bank | Hybrid taps | High-band delay | Latency |
|---|---|---:|---:|---:|
| `1` | public bank | 13 | 6 slots | 961 samples |
| `2` | captured bank | 11 | 3 slots | 448 samples |

The default is `1` (public). Each bank keeps its own coefficients,
histories, high-band delay, startup skip and flush capacity.

The renderer applies the 36-term direction basis, per-profile geometry, ear path
lengths, phase groups, direct gains, the room send, the recursive room model
with overlap-add FIR, and the LFE low-pass. Sources are summed in reverse channel
order. `chunk_frames` controls internal 1536-sample batching only.

`OamdPositionTimeline` schedules OAMD Q15 positions on the 32-sample metadata
grid, including the object delay. Bed-only streams use the declared bed layout as
fixed positions. Queries are monotonic and resets preserve fixed bed positions.

## `salsahrtfrender`

| Property | Default | Meaning |
|---|---|---|
| `personalized-headphone` | — | Rosella model path (required) |
| `filterbank` | `1` | `1` = public, `2` = captured |
| `profile` | `mid` | `near`, `mid`, `far` |
| `object-delay` | `1473` | OAMD delay in samples |
| `tail-seconds` | `5.0` | Room/filterbank flush at EOS |
| `output-gain` | `1.0` | Linear stereo gain |
| `chunk-frames` | `1` | Internal 1536-sample frames per chunk |
| `room-impulse-slots` | `4096` | Room FIR length in 64-sample slots |

Caps:

- sink: `audio/x-raw(memory:SystemMemory, meta:ObjectAudioMeta)` or
  `audio/x-raw(meta:ObjectAudioMeta)`, F32LE, interleaved, 48 kHz,
  `channels=[1, 1024]`;
- src: `audio/x-raw`, F32LE, interleaved, stereo, 48 kHz, channel mask `0x3`.

The element emits the raw stream in input order and reports the selected bank's
latency through the `LATENCY` query. EOS drains the room tail as an extra buffer.

## Metadata handling

Both renderers read the salsa `ObjectAudioMeta` / `SpatialPayload`; They never 
register the meta type either: the meta crate may be linked into more than one 
plugin dylib, and a second registration is a fatal GType error. `get_spatial`
uses `gst_meta_get_info` and the C layout instead.

OAMD slots map directly to object channels. Bed-only streams use the target
layout's anchor for the first bed channels.

Both renderers reset their streaming state on `FLUSH_STOP` and on an input
buffer carrying `GST_BUFFER_FLAG_DISCONT` (non-flushing seek). The speaker
renderer is sample-aligned and restarts immediately; the binaural renderer
restarts from clean filter/room state and its filterbank latency/transient at
the seek point is accepted.
