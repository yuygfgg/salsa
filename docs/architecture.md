# Architecture

Salsa is split into small library crates and thin GStreamer plugin crates. The
libraries decode, render, and model object audio without depending on
GStreamer; only `objectaudio-meta` and the plugin crates link GStreamer.

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
```

## Crates

| Crate | Role | GStreamer |
|---|---|---|
| `objectaudio-ir` | `SpatialFrame`, `SpatialPayload`, object state/events, LFE+object channel map, planar PCM, parser/decoder traits | no |
| `objectaudio-oamd` | single OAMD parser/writer; bed types live here, not in the IR | no |
| `eac3joc` | E-AC-3 framing, EMDF containers, OAMD/ID11 and JOC/ID14 parsing | no |
| `eac3joc-core-decoder` | E-AC-3 core PCM through libavcodec with DRC disabled | no |
| `eac3joc-dsp` | QMF analysis, object matrix, synthesis and LFE handling | no |
| `truehd-atmos` | TrueHD Atmos parse/decode adapter producing the same IR | no |
| `objectaudio-damf` | DAMF `.atmos`/CAF/metadata reader; bed expansion and keyframe-to-IR mapping | no |
| `objectaudio-meta` | `ObjectAudioMeta` GstMeta (`SpatialPayload`), caps feature, C ABI and Dolby compatibility | yes |
| `objectaudio-render` | speaker layout tables, speaker anchors and the object-to-speaker renderer | no |
| `objectaudio-hrtf` | personalized-headphone model, filterbanks, room model, binaural renderer | no |
| `gst-eac3joc` | `salsaeac3jocparse`, `salsaeac3jocdec` | yes |
| `gst-truehd` | `salsatruehdparse`, `salsatruehdatmosdec` | yes |
| `gst-objectaudio` | `salsaoar`, `salsahrtfrender`, `salsaobjectaudioadapt` | yes |
| `gst-damf` | `salsadamfsrc` | yes |

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

`SpatialPayload` is the metadata-only snapshot carried by `ObjectAudioMeta`:

- `version` (currently 1), `sample_rate`;
- `objects`: the state at the payload's sample offset;
- `events`: changes whose offsets are relative to the buffer start;
- `snapshot_ramp_samples`: optional ramp for the snapshot transition, used by
  OAMD's first object-info block.

Serialization is `postcard`; unknown versions are rejected. PCM stays in the
GStreamer buffer.

## Metadata

`ObjectAudioMeta` carries an encoded `SpatialPayload`:

- `offset` is the sample position of the object snapshot;
- `bpf` is bytes per frame (`bytes_per_sample * channels`);
- payloads are copied on add and freed with the meta;
- a buffer may carry several instances;
- `GST_META_TRANSFORM_COPY` recomputes `offset` and shifts/clips events to the
  copied region.

The safe API is `add_spatial` / `add_spatial_bytes` / `get_spatial` / `iter_spatial`
plus the caps helpers. `objectaudio-meta` never registers the Dolby meta.
`DlbObjectAudioMeta` is read and written only by `salsaobjectaudioadapt`; the
core decoders and renderers never consume it.

## OAMD boundary

`objectaudio-oamd` is the single OAMD implementation: parser and writer share
the bed tables, object limits and field semantics. Codec decoders use it on
their input wire format; `salsaobjectaudioadapt` uses it in both directions.
No library above `objectaudio-oamd` imports the upstream `oamd` crate directly.

## Elements

| Element | Base class | Purpose |
|---|---|---|
| `salsadamfsrc` | `GstBaseSrc` | read a DAMF presentation and emit `LFE + N` object PCM |
| `salsaeac3jocparse` | `GstBaseParse` | frame E-AC-3 JOC and expose stream caps |
| `salsaeac3jocdec` | `GstAudioDecoder` | decode E-AC-3 JOC to `LFE + N` object PCM |
| `salsatruehdparse` | `GstBaseParse` | frame TrueHD access units and expose rate/Atmos caps |
| `salsatruehdatmosdec` | `GstAudioDecoder` | decode TrueHD Atmos to `LFE + N` object PCM |
| `salsaoar` | `GstBaseTransform` | object audio to a standard speaker layout |
| `salsahrtfrender` | `GstBaseTransform` | object audio to binaural stereo |
| `salsaobjectaudioadapt` | `GstBaseTransform` | bridge salsa `SpatialPayload` and Dolby OAMD |
