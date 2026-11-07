# Architecture

Salsa is split into library crates and thin GStreamer plugin crates. Codec,
rendering, and scene-model libraries are independent of GStreamer. Shared
GStreamer adapters live under `crates/libs/gst`; plugin crates expose the
elements.

The experimental salsa-spatial object codec follows the E-AC-3 JOC crate
split: codec libraries, an encoding CLI, and a GStreamer parser/decoder plugin.
See the [codec design](salsa-spatial.md),
[implementation guide](salsa-spatial-implementation.md), and
[quality evaluation](salsa-spatial-quality.md).

```text
E-AC-3 JOC / TrueHD
        │
        ▼
  transport + parse          eac3joc, truehd-atmos
  DAMF manifest + CAF        objectaudio-damf
        │
        ▼
  SpatialFrame (IR)          objectaudio-ir
        │
        ├──► speaker renderer      objectaudio-render ──► salsaoar
        └──► binaural renderer     objectaudio-hrtf   ──► salsahrtfrender

DAMF master
        │
        ▼
  eac3joc-encoder  (downmix + LFE filter + JOC + reac3e core)
        │
        ▼
  E-AC-3 JOC  (.ec3)
```

## Crates

| Crate | Role | GStreamer |
|---|---|---|
| `objectaudio-ir` | `SceneTimeline`, `SpatialFrame`, `SpatialPayload`, object state/events, LFE+object channel map, planar PCM, parser/decoder traits | no |
| `objectaudio-oamd` | single OAMD parser/writer; bed types live here, not in the IR | no |
| `eac3joc` | E-AC-3 framing, EMDF containers, OAMD/ID11 and JOC/ID14 parsing | no |
| `eac3joc-core-decoder` | E-AC-3 core PCM through libavcodec with DRC disabled | no |
| `eac3joc-dsp` | QMF analysis, object matrix, synthesis and LFE handling | no |
| `truehd-atmos` | TrueHD Atmos parse/decode adapter producing the same IR | no |
| `iec61937` | IEC 61937 carrier framing, E-AC-3 syncframes, TrueHD MAT extraction and relative timing | no |
| `objectaudio-damf` | DAMF parser, inherited metadata resolver, shared channel layout, and CAF I/O | no |
| `objectaudio-meta` | `ObjectAudioMeta` GstMeta (`SpatialPayload`), caps feature, C ABI and Dolby compatibility | yes |
| `objectaudio-render` | speaker layout tables, speaker anchors and the object-to-speaker renderer | no |
| `objectaudio-hrtf` | personalized-headphone model, filterbanks, room model, binaural renderer | no |
| `gst-eac3joc` | `salsaeac3jocparse`, `salsaeac3jocdec` | yes |
| `gst-truehd` | `salsatruehdparse`, `salsatruehdatmosdec` | yes |
| `gst-objectaudio` | `salsaoar`, `salsahrtfrender`, `salsaobjectaudioadapt` | yes |
| `gst-damf` | `salsadamfsrc`, `salsadamfsink` | yes |
| `eac3joc-encoder` | DAMF → LFE-filtered 5.1 downmix → reac3e core → OAMD/JOC/EMDF → E-AC-3 JOC (library) | no |
| `salsa-eac3joc-encode` | CLI front-end for `eac3joc-encoder` | no |
| `salsa-audiod` | virtual IEC 61937 service: carrier recovery, decode/render pipeline, bounded PCM output | yes |

## Intermediate representation

`SpatialFrame` is deliberately small and codec independent:

- planar `f32` PCM at a declared sample rate;
- channel 0 is the LFE and channels 1..N are point objects;
- current object states and in-frame events.

There is no bed concept in the IR. A decoder or source that receives a speaker
bed expands every non-LFE bed channel into a fixed-position object before
building the frame. The renderer only sees `LFE + N` objects.

Decoder latency stays with the decoder and is expressed through buffer PTS and
event offsets. The frame itself always describes a contiguous block of audio.

`SpatialPayload` version 3 carries metadata without PCM:

- `objects` declares channel-ordered object states or targets.
- `snapshot` distinguishes an evaluated `State`, a transition `Target`, and
  `Continue` with no new state update.
- `snapshot_offset` and event offsets are signed samples relative to the same
  PCM buffer start. A snapshot offset is never added to event offsets.
- Each event has one object identifier, in its target state. `Transition`
  selects a step, a position ramp, or a ramp of all continuous IR properties.
  Discrete properties change at the event start.
- `calibration` optionally carries the programme speaker controls (warp, the
  nine trim configurations and per-object bypass). It is payload-level, not
  per-object; a payload without it keeps the previous controls.

`SceneTimeline` owns time interpretation. It resolves ramp origins, overlapping
updates, and repeated timestamps. HRTF, GUI, speaker rendering, and buffer
adapters use this timeline. Sources export actual event boundaries and unfinished
ramps instead of replacing each input block with one ramp.

DAMF patches are resolved once by `DamfScene`. The resolver owns inheritance,
defaults, bed expansion, and the source channel map. Encoding and clustering
consume resolved objects. `ConversionReport` records unsupported properties at
format and consumer boundaries. Complete field destructuring makes a new
metadata property an explicit decision at these boundaries.

The speaker adapter samples positions on its 32-sample DSP grid and interpolates
speaker gains within each block. HRTF uses its own control grid and filterbank.
Both apply source gain and activity before mixing. These renderer approximations
are separate from authored timeline semantics. Clustering emits evaluated states
at its analysis hop; it does not add another ramp to those states.

## Metadata and buffer operations

`ObjectAudioMeta` carries a postcard-encoded `SpatialPayload`. Version 1 payloads
are rejected. Rebuild the workspace plugins and GUI together after this change.
PCM stays in the GStreamer buffer.

- `bpf` is bytes per audio frame.
- The encoded payload owns timestamps. The C ABI `offset` field mirrors the
  nonnegative snapshot offset; C callers must supply a matching value.
- A buffer can carry several metadata attachments. Consumers submit all of them.
  The GUI queues them as one buffer update and smooths PCM levels once.
- `SpatialPayload::calibration` carries programme speaker controls (OAMD warp,
  trim configurations and per-object bypass). The renderer selects the
  configuration for its target layout; binaural rendering ignores it.
- `SpatialPayload::rebase` moves both timestamp kinds to another buffer origin.
- `SpatialPayload::slice` retains the causal prefix at negative offsets. It
  does not shorten ramps or change their targets. Target snapshots still need
  stream history; a slice cannot recover history absent from its input.
- `SceneTimeline::payload` exports an evaluated state and the remaining events
  for a range. Seek and streaming adapters use the same operation.

Audio reblocking keeps an absolute sample counter. Metadata is submitted at the
input buffer's sample position before PCM is consumed. Renderer reset, flush,
and discontinuity boundaries also reset this clock and its scene history.

The safe attachment API takes the payload and `bpf`; there is no second snapshot
offset argument. `objectaudio-meta` does not register the Dolby meta.

## OAMD boundary

`objectaudio-oamd` is the single OAMD implementation: parser and writer share
the bed tables, object limits and field semantics. Codec decoders use it on
their input wire format; `salsaobjectaudioadapt` uses it in both directions.
The E-AC-3 encoder also uses this writer, with a profile that adds its trim
metadata; a payload with `ProgramCalibration` writes that element back. DAMF
carries only the per-object `trimBypass` flag. Writers reject excess blocks, out-of-range event times, and conflicting
ramp lengths instead of silently truncating or merging them. Position and gain
quantization remain part of the OAMD format.

Verification focuses on semantic regressions: hold/ramp timing, amplitude,
buffer slicing, and parseable output. It does not require bit-exact renderer or
encoder output.

## Elements

| Element | Base class | Purpose |
|---|---|---|
| `salsadamfsrc` | `GstBaseSrc` | read a DAMF presentation and emit `LFE + N` object PCM |
| `salsadamfsink` | `GstBaseSink` | write `LFE + N` object PCM and `ObjectAudioMeta` as a DAMF presentation |
| `salsaeac3jocparse` | `GstBaseParse` | frame E-AC-3 JOC and expose stream caps |
| `salsaeac3jocdec` | `GstAudioDecoder` | decode E-AC-3 JOC to `LFE + N` object PCM |
| `salsatruehdparse` | `GstBaseParse` | frame TrueHD access units and expose rate/Atmos caps |
| `salsatruehdatmosdec` | `GstAudioDecoder` | decode TrueHD Atmos to `LFE + N` object PCM |
| `salsaoar` | `GstBaseTransform` | object audio to a standard speaker layout |
| `salsahrtfrender` | `GstBaseTransform` | object audio to binaural stereo |
| `salsaobjectaudioadapt` | `GstBaseTransform` | bridge salsa `SpatialPayload` and Dolby OAMD |
