# Salsa

**SALSA** — *SALSA Ain't Licensed Spatial Audio*

Spatial audio tools for Dolby Atmos object audio: E-AC-3 JOC and TrueHD Atmos decoding, E-AC-3 encoding, DAMF reading, speaker and binaural rendering, and Dolby OAMD interoperability.

## GST Plugins

Salsa provides the following GStreamer elements. The plugin crates live in
`crates/gst-plugins/`.

> [!TIP]
> Salsa is designed for spatial audio only. For non-object TrueHD or E-AC-3 streams, use standard GStreamer decoders.

### Elements Overview

| Plugin            | Element                 | Description                           | Input → Output                                                                                                                                                                                                                    |
| ----------------- | ----------------------- | ------------------------------------- | --------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------- |
| `gst-eac3joc`     | `salsaeac3jocparse`     | E-AC-3 JOC syncframe parser           | `audio/x-eac3` (unframed or framed) → `audio/x-eac3, framed=true, alignment=frame, rate=8000..48000, channels=1..8, atmos={false,true}` (one syncframe per buffer)                                                                    |
| `gst-eac3joc`     | `salsaeac3jocdec`       | E-AC-3 JOC object decoder             | `audio/x-eac3, framed=true, alignment=frame` → `audio/x-raw` F32LE interleaved, `channels=1..16` (LFE + JOC objects), `rate={32000,44100,48000}`, `channel-mask=0`, optional `meta:ObjectAudioMeta`                                  |
| `gst-truehd`      | `salsatruehdparse`      | TrueHD access-unit parser             | `audio/x-true-hd` or `audio/x-truehd` (unframed or framed) → `audio/x-true-hd, framed=true, alignment=frame, rate={44100,48000,88200,96000,176400,192000}, atmos={false,true}`                                                       |
| `gst-truehd`      | `salsatruehdatmosdec`   | TrueHD Atmos decoder (presentation 3) | `audio/x-true-hd` or `audio/x-truehd`, `framed=true, alignment=frame`, `rate=1..655350` → `audio/x-raw` F32LE interleaved, `channels=1..16`, `rate={44100,48000,88200,96000,176400,192000}`, `channel-mask=0`, optional `meta:ObjectAudioMeta` |
| `gst-damf`        | `salsadamfsrc`          | DAMF master source                    | (no sink pad) → `audio/x-raw` F32LE interleaved, `channels=1..1024` (LFE + objects), CAF rate, `channel-mask=0`, optional `meta:ObjectAudioMeta`                                                                                     |
| `gst-damf`        | `salsadamfsink`         | DAMF master sink                      | `audio/x-raw` F32LE/S24LE interleaved with `meta:ObjectAudioMeta`, `channels=1..1024` (LFE + objects), `rate=8000..192000`, `channel-mask=0` → `.atmos` manifest + `.atmos.audio` (24-bit CAF) + `.atmos.metadata`                  |
| `gst-objectaudio` | `salsaoar`              | Speaker layout renderer               | `audio/x-raw` F32LE interleaved with `meta:ObjectAudioMeta`, `rate=32000..48000`, `channels=1..1024`, `channel-mask=0` → `audio/x-raw` F32LE interleaved, one structure per layout (`2.0`–`9.1.6`), layout channel count and mask     |
| `gst-objectaudio` | `salsahrtfrender`       | Binaural HRTF renderer (Rosella)      | `audio/x-raw` F32LE interleaved with `meta:ObjectAudioMeta`, `rate=48000`, `channels=1..1024`, `channel-mask=0` → `audio/x-raw` F32LE interleaved stereo, 48 kHz, `channel-mask=0x3`                                               |
| `gst-objectaudio` | `salsaobjectaudioadapt` | Salsa ↔ Dolby OAMD metadata bridge    | `audio/x-raw` F32LE/S32LE interleaved, `rate=32000..48000`, `channels=1..32`, `channel-mask=0`, with `meta:ObjectAudioMeta` or `meta:DlbObjectAudioMeta` (± `memory:SystemMemory`) or no feature → same rate/channels; `to-dolby` → `meta:DlbObjectAudioMeta`, `to-salsa` → `meta:ObjectAudioMeta`; `float=true` also converts S32LE → F32LE |

### Quick Start

```sh
cargo build --release -p gst-eac3joc -p gst-objectaudio -p gst-truehd -p gst-damf
GST_PLUGIN_PATH=$PWD/target/release gst-inspect-1.0 salsaeac3jocdec
GST_PLUGIN_PATH=$PWD/target/release gst-inspect-1.0 salsadamfsink
```

**E-AC-3 JOC → 7.1.4 speaker layout:**

```sh
GST_PLUGIN_PATH=$PWD/target/release \
  gst-launch-1.0 -q filesrc location=stream.ec3 ! salsaeac3jocparse ! salsaeac3jocdec \
  ! salsaoar layout=7.1.4 ! wavenc ! filesink location=out.wav
```

**E-AC-3 JOC → Binaural stereo (Rosella model):**

```sh
GST_PLUGIN_PATH=$PWD/target/release \
  gst-launch-1.0 -q filesrc location=stream.ec3 ! salsaeac3jocparse ! salsaeac3jocdec \
  ! salsahrtfrender personalized-headphone=model.personalized_headphone filterbank=2 \
  ! wavenc ! filesink location=binaural.wav
```

**TrueHD Atmos → 7.1.4 speaker layout:**

```sh
GST_PLUGIN_PATH=$PWD/target/release \
  gst-launch-1.0 -q filesrc location=stream.mlp blocksize=1048576 \
  ! salsatruehdparse ! salsatruehdatmosdec \
  ! salsaoar layout=7.1.4 ! wavenc ! filesink location=out.wav
```

`salsatruehdparse` is required: `salsatruehdatmosdec` expects one access unit per buffer.

**DAMF master → 7.1.4 speaker layout:**

```sh
GST_PLUGIN_PATH=$PWD/target/release \
  gst-launch-1.0 -q salsadamfsrc location=master.atmos \
  ! salsaoar layout=7.1.4 ! wavenc ! filesink location=out.wav
```

`salsadamfsrc` resolves the manifest's `.atmos.audio` / `.atmos.metadata` sidecars
and expands non-LFE bed channels into fixed-position objects. It reads CAF PCM
directly; no FFmpeg or external demuxer is required.

**E-AC-3 JOC → DAMF master:**

```sh
GST_PLUGIN_PATH=$PWD/target/release \
  gst-launch-1.0 -q filesrc location=stream.ec3 ! salsaeac3jocparse ! salsaeac3jocdec \
  ! salsadamfsink location=master.atmos
```

**TrueHD Atmos → DAMF master:**

```sh
GST_PLUGIN_PATH=$PWD/target/release \
  gst-launch-1.0 -q filesrc location=stream.mlp blocksize=1048576 \
  ! salsatruehdparse ! salsatruehdatmosdec \
  ! salsadamfsink location=master.atmos
```

> [!NOTE]
> On macOS/Homebrew, if a fresh registry scan deadlocks in `gst-plugin-scanner`, prefix with `GST_REGISTRY_FORK=no`.

### Object Audio Format

All Salsa elements exchange a unified raw object stream:

- **PCM Format**: `audio/x-raw, format=F32LE, layout=interleaved, channel-mask=0` (the Dolby bridge additionally accepts `S32LE` and can convert it).
- **Channel Mapping**: Channel `0` is LFE; channels `1..N` are dynamic point objects (`channels - 1` = object count). Decoders emit `1 + object_count` channels at 32/44.1/48 kHz for E-AC-3 JOC and up to 192 kHz for TrueHD; the renderers accept up to 1024 channels. Non-LFE beds are expanded into fixed-position objects by the TrueHD decoder or by `salsaobjectaudioadapt`; the E-AC-3 JOC decoder emits its JOC objects only.
- **Metadata (`meta:ObjectAudioMeta`)**: Carries a versioned, postcard-serialized `SpatialPayload` with signed sample offsets for snapshots, object events, and calibration events. Target snapshots specify the transition type and duration. Version 4 adds calibration events so slicing and reblocking preserve changes inside a buffer. Rebuild producers and consumers together; older payload versions are rejected. Decoders and DAMF sources attach metadata when downstream negotiates the caps feature. `salsaoar` and `salsahrtfrender` require it. Use `salsaobjectaudioadapt` to convert Dolby `meta:DlbObjectAudioMeta` first.

### Element Reference

#### `salsadamfsrc` (DAMF Source)

Reads a Dolby Atmos Master Format presentation without going through an
encoder/decoder. The `location` property points at the `.atmos` manifest; the
source resolves `presentations[0].audio` / `.metadata`, then the standard
`<manifest>.atmos.audio` / `<manifest>.atmos.metadata` names, then Cavern's
`<stem>.caf` fallback.

- **Src caps**: `audio/x-raw` F32LE interleaved, `channels=1..1024` (LFE +
  objects), CAF sample rate, `channel-mask=0`, with `meta:ObjectAudioMeta` when
  downstream accepts it, plus a plain-raw fallback.
- **PCM**: uncompressed CAF `lpcm`, including 16/24/32-bit integer and 32-bit
  float; 24-bit big-endian is the normal DAMF layout. Samples are reordered to
  `ch0 = LFE`, `ch1..N = objects`.
- **Bed expansion**: non-LFE bed channels become fixed-position objects. 
  LFE stays channel 0. A DAMF without an LFE bed gets a silent LFE channel.
- **Metadata**: `samplePos` is inherited by events that omit it (the group rule
  used by Dolby's writer). Object positions follow the encoder's `PosTimeline`
  semantics: hold the previous position until the event sample, then ramp to the
  new target over `rampLength` samples. `gain`, `active` and `size` are held
  between keyframes. The source samples that path at block boundaries and emits
  one shared ramp per block, matching the encoder's OAMD path and keeping the
  renderer's full-snapshot event model consistent.
- **Buffers**: 1536 samples per buffer, matching the decoders and Rosella's
  `FRAME_SAMPLES`; the final buffer may be shorter. PTS, duration, offset and
  offset-end follow the master timeline.
- **Seeking**: supports flushing seeks (forward and backward) in time format.
  The source sets the base-src format to time and repositions the CAF reader;
  rate must be 1.0. The object timeline is stateless, so no decoder reset is
  needed.
- **Ignored fields**: `importance`, `zones`, `dialog`, `music`, `screenFactor`,
  `depthFactor`, `snap`, `elevation`, `headTrackMode` and `binauralRenderMode`
  have no salsa IR representation yet.

#### `salsadamfsink` (DAMF Sink)

Writes a DAMF presentation from salsa object audio. The `location` property
is the `.atmos` manifest path; the sink writes `<location>.audio` (24-bit CAF)
and `<location>.metadata` next to it and references those names from the manifest.

- **Sink caps**: `audio/x-raw` F32LE or S24LE interleaved with
  `meta:ObjectAudioMeta` (required), `channels=1..1024` (LFE + objects),
  `rate=8000..192000`, `channel-mask=0`.
- **PCM**: `ch0` is written as the LFE bed; `ch1..N` become dynamic DAMF
  objects with IDs `object-id-base + (n - 1)` (default `10..`).
- **Metadata**: snapshots and events are written as DAMF keyframes at their
  sample positions with `gain` in dB and `rampLength` from the transition.
  `location`, `fps`, `offset`, `lfe-id`, `object-id-base`,
  `creation-tool`, `creation-tool-version` and `little-endian` are configurable.
- **Finalisation**: the CAF data chunk size is patched on EOS/stop.

#### Parsers (`salsaeac3jocparse`, `salsatruehdparse`)

Both parsers support flushing seeks (forward and backward) when the upstream source is seekable, and take no properties. TrueHD seeks land on the next major sync, so the granularity is bounded by the major-sync interval (≤32 access units for FBB, ≤128 for FBA):

- **`salsaeac3jocparse`**: E-AC-3 JOC syncframe framing.
  - Sink caps: `audio/x-eac3` only, with no `framed` requirement. AC-3 is not supported.
  - Src caps: `audio/x-eac3, framed=true, alignment=frame, rate=8000..48000, channels=1..8, atmos={false,true}`. `atmos` is `true` when the syncframe carries a JOC EMDF payload.
  - Emits one syncframe per buffer and sets the frame rate (1536 samples).
- **`salsatruehdparse`**: TrueHD access-unit framing around `truehd::Extractor`.
  - Sink caps: `audio/x-true-hd` or `audio/x-truehd`, with no `framed` requirement.
  - Src caps: `audio/x-true-hd, framed=true, alignment=frame, rate={44100,48000,88200,96000,176400,192000}, atmos={false,true}`.
  - Emits one access unit per buffer, skips leading junk by resynchronising on major sync, and derives `rate`/frame rate from the major sync. `atmos` becomes `true` when an access unit carries OAMD.
  - The parser does not serialize `truehd::AccessUnit`; it forwards the original access-unit bytes.

#### Decoders (`salsaeac3jocdec`, `salsatruehdatmosdec`)

Both decoders handle flushing seeks and reset their codec/DSP state on `FLUSH_STOP` (and on `DISCONT` for non-flushing seeks). They take no properties:

- **`salsaeac3jocdec`**: Uses `libavcodec` for the E-AC-3 core and the JOC kernel for object reconstruction.
  - Sink caps: `audio/x-eac3, framed=(boolean)true, alignment=(string)frame, rate=(int)[1,655350]` only. Plain AC-3 cannot be linked.
  - Src caps: `audio/x-raw`, F32LE, interleaved, `channels=1..16`, `rate={32000,44100,48000}`, `channel-mask=0`, optionally with `meta:ObjectAudioMeta`.
  - Output channel count is `1 + N` where `N` is the JOC object count; it is locked on the first frame, and a mid-stream change is rejected.
- **`salsatruehdatmosdec`**: Pure-Rust TrueHD Atmos decoder (`truehd` crate).
  - Sink caps: `audio/x-true-hd` or `audio/x-truehd`, `framed=(boolean)true`, `alignment=(string)frame`, `rate=(int)[1,655350]`.
  - Src caps: `audio/x-raw`, F32LE, interleaved, `channels=1..16`, `rate={44100,48000,88200,96000,176400,192000}`, `channel-mask=0`, optionally with `meta:ObjectAudioMeta`.
  - Consumes the access-unit-per-buffer output of `salsatruehdparse`; unframed streams are not accepted. It accumulates 40-sample access units into 32-sample blocks and emits a buffer per 1216 samples or per OAMD update. Presentation bed channels are expanded into fixed-position objects.

#### `salsaoar` (Speaker Renderer)

Sample-accurate equal-power panning to standard speaker layouts (`2.0`, `3.1`, `4.0`, `5.1`, `7.1`, `5.1.2`, `5.1.4`, `7.1.2`, `7.1.4`, `9.1.4`, `9.1.6`), passing LFE straight through. Programme OAMD trim, warp 0/1 and warp 2/3 surround folding follow the source calibration; wider layouts use disabled trim.

- **Sink caps**: `audio/x-raw` F32LE interleaved, `rate=32000..48000`, `channels=1..1024`, `channel-mask=0`, with `meta:ObjectAudioMeta` (also accepted with `memory:SystemMemory`). Plain raw and `meta:DlbObjectAudioMeta` are not advertised; Dolby streams must pass through `salsaobjectaudioadapt`.
- **Src caps**: `audio/x-raw` F32LE interleaved, one structure per supported layout with the layout's channel count and GStreamer channel mask, same rate as the sink.
- **Layout selection**: a recognized downstream `channel-mask` overrides `layout`; otherwise `layout` is used, falling back to inference by output channel count.

| Property          | Default | Description                                                                                          |
| ----------------- | ------- | ---------------------------------------------------------------------------------------------------- |
| `layout`          | `5.1`   | Target speaker layout name, or `auto` to infer from downstream caps (set before `PAUSED`).           |
| `metadata-offset` | `0`     | Extra sample offset applied to spatial snapshots/events (`0–65535`, clamped to buffer length).        |
| `limiter-enable`  | `false` | Normalize buffers exceeding 0 dBFS by `1/peak` (safety limiter, not Dolby DRC).                      |
| `discard-latency` | `false` | No-op property kept for Dolby `oar` CLI compatibility; output remains sample-aligned.               |

The E-AC-3 decoder already folds the 1473-sample codec delay into its metadata offsets, so the renderer default stays at zero. A buffer without metadata keeps the previous panning state.

Input buffers can contain any number of complete PCM frames, including a partial
block after an accurate seek. The element queues PCM and spatial events together
and renders complete 32-sample blocks. It retains at most 31 samples between
buffers and reports this buffering latency. Output timestamps and sample offsets
stay on the input timeline. EOS drains the final partial block without adding
samples. `FLUSH_STOP` and `DISCONT` discard queued PCM and metadata.

#### `salsahrtfrender` (Binaural Renderer)

Renders 48 kHz object audio to stereo via a Dolby `.personalized_headphone` (Rosella JSON) model, including direct HRTF, early reflections, recursive room model, and LFE low-pass. Requires table A (dim 64, option 3) + table B (20 groups, 36 terms), no table C.

- **Sink caps**: `audio/x-raw` F32LE interleaved, `rate=48000`, `channels=1..1024`, `channel-mask=0`, with `meta:ObjectAudioMeta` (also accepted with `memory:SystemMemory`). Plain raw and Dolby metadata are rejected.
- **Src caps**: `audio/x-raw` F32LE interleaved stereo, 48 kHz, `channel-mask=0x3`.

| Property                 | Default | Description                                                                             |
| ------------------------ | ------- | --------------------------------------------------------------------------------------- |
| `personalized-headphone` | —       | Path to the Rosella JSON model (**required**; checked in `set_caps`).                   |
| `filterbank`             | `1`     | `1` = public (13-tap, 961-sample latency); `2` = captured (11-tap, 448-sample latency). |
| `profile`                | `mid`   | Distance profile: `near`, `mid`, or `far`.                                              |
| `object-delay`           | `0`     | Extra sample delay applied to spatial events (`0–65535`).                               |
| `tail-seconds`           | `5.0`   | Room/filterbank flush tail drained at EOS (`0–60` s).                                   |
| `output-gain`            | `1.0`   | Linear output gain.                                                                     |
| `chunk-frames`           | `1`     | Internal 1536-sample frames per render chunk (`1–1024`).                                |
| `room-impulse-slots`     | `4096`  | Room FIR length in 64-sample slots (`1–65536`).                                         |

The decoder-provided `SpatialPayload` already includes the codec delay, so `object-delay` defaults to zero.

#### `salsaobjectaudioadapt` (Dolby OAMD Bridge)

Converts between Salsa `ObjectAudioMeta` and Dolby `gst-home-audio` (Reference Player) `DlbObjectAudioMeta` (resolved dynamically at runtime from loaded Dolby plugins). Note that OAMD quantization (6-bit x/y, 4-bit z, integer dB) and bed-to-object expansion are lossy. This element is in-place and does not change channel count, rate, or layout.

- **Sink caps**: `audio/x-raw` F32LE or S32LE interleaved, `rate=32000..48000`, `channels=1..32`, `channel-mask=0`, with `meta:ObjectAudioMeta` or `meta:DlbObjectAudioMeta` (either may be combined with `memory:SystemMemory`), or no caps feature.
- **Src caps**: same format, rate, and channels. `direction=to-dolby` advertises `meta:DlbObjectAudioMeta` (or `meta:ObjectAudioMeta` when `remove=true`); `direction=to-salsa` advertises `meta:ObjectAudioMeta`; `auto` offers both and resolves from the sink caps (Dolby feature present → `to-salsa`, otherwise `to-dolby`).
- `float=true` makes the output `F32LE` and converts `S32LE` input samples in place.

| Property    | Default | Description                                                                                                                   |
| ----------- | ------- | ----------------------------------------------------------------------------------------------------------------------------- |
| `direction` | `auto`  | `auto` (from input caps), `to-dolby` (`SpatialPayload` → OAMD ID11), or `to-salsa` (OAMD → `SpatialPayload`).                 |
| `remove`    | `false` | In `to-dolby` mode, do not add `DlbObjectAudioMeta`; existing Salsa metadata is left untouched and the src advertises Salsa caps. |
| `float`     | `false` | Convert interleaved `S32LE` samples to `F32LE` in place.                                                                      |

### DAMF → E-AC-3 JOC encoder (`salsa-eac3joc-encoder`)

`crates/libs/encoder/eac3joc-encoder/` is the encoder library and
`crates/apps/salsa-eac3joc-encode/` is its CLI. It converts a Dolby Atmos Master
(`.atmos` + `.atmos.audio` + `.atmos.metadata`) to E-AC-3 JOC:

```sh
cargo build --release -p salsa-eac3joc-encode
salsa-eac3joc-encode master.atmos -o output.ec3 \
  --core-bitrate 692950 \
  --emdf-key @/path/to/emdf_keys.bin
```

- Without correct `--emdf-key` the stream carries the public-CRC placeholder and
  will not pass Dolby validation.
- `reac3e` is LGPL-2.1-or-later; the rest of the tool follows salsa's MPL-2.0.

#### DAMF input requirements

This encoder does **not** cluster, remap, or otherwise process the input DAMF.
It expects:

- exactly one LFE bed channel;
- at most **15 dynamic objects**.

Masters outside this shape, for example **more than 15 objects**, a non-LFE bed,
or more than one bed channel, will fail validation or fail to produce a valid
JOC stream. Use `salsa-cluster` to cluster the master into at most 15 objects
and produce a compatible DAMF first.

The encoder writes one OAMD calibration per 1536-sample frame. It reads
`trimBypass` at the frame start and position targets at the frame end. Bypass
changes must fall on frame boundaries. The encoder rejects changes inside a
frame before it creates the output file.

#### reac3e core parameters

| Flag | Default | Meaning |
|---|---|---|
| `--core-bitrate BPS` | required | CBR core rate; must select 6-block frames (≤ 1,024,000). |
| `--dialnorm DB` | `-21` | Dialogue normalization, `-31..=-1`. |
| `--coupling auto\|on\|off` | `off` | E-AC-3 channel coupling. |
| `--coupling-start 0..15` | — | Override the coupling start band. |
| `--no-stereo-rematrixing` | rematrix on | Disable stereo rematrixing. |
| `--no-block-switch` | block switch on | Disable the transient detector / short blocks. |
| `--aht` | off | Enable experimental adaptive hybrid transform. |
| `--cutoff-hz HZ` | bitrate table | Override the coded bandwidth cutoff. |
| `--service-type main\|effects\|visually-impaired\|hearing-impaired\|commentary\|emergency\|voice-over\|karaoke` | `main` | BSI `bsmod`. |
| `--copyright BOOL` / `--original BOOL` | not indicated | BSI informational bits. |
| `--mixing-level 80..=111` | — | Audio production mixing level (dB). |
| `--room-type not-indicated\|large\|small` | — | Audio production room type. |
| `--ad-converter-type standard\|hdcd` | — | A/D converter type. |
| `--dolby-surround-mode not-indicated\|off\|on` | — | Stereo Dolby Surround mode. |
| `--dolby-headphone-mode not-indicated\|off\|on` | — | Stereo headphone mode. |
| `--dolby-surround-ex-mode not-indicated\|off\|on\|dplii-z` | — | Dolby Surround EX mode. |
| `--dmix-mode none\|ltrt\|loro\|dplii` | `loro` | Preferred stereo downmix; `none` omits the mixing metadata block. |
| `--ltrt-center-mix-level F` | `0.70710677` | Lt/Rt centre mix level. |
| `--ltrt-surround-mix-level F` | `0.70710677` | Lt/Rt surround mix level. |
| `--loro-center-mix-level F` | `0.70710677` | Lo/Ro centre mix level. |
| `--loro-surround-mix-level F` | `0.70710677` | Lo/Ro surround mix level. |

#### JOC / OAMD / EMDF parameters

| Flag | Default | Meaning |
|---|---|---|
| `--joc-lambda F` | `1e-3` | Huffman-aware RD weight; `0` disables RD refinement. |
| `--joc-ridge F` | `1e-3` | Diagonal loading relative to the per-band core covariance trace. |
| `--emdf-budget N` | `511` | Soft skip-field target; the hard limit is 511 bytes. |
| `--joc-select knapsack\|greedy` | `knapsack` | Per-object fine→coarse fallback. |
| `--joc-mode auto\|per-band\|broadband` | `auto` | Try per-band, then broadband only if needed. |
| `--joc-quant auto\|fine\|coarse` | `auto` | `fine` forces fine matrices (may fail if over 511 B); `coarse` forces coarse. |
| `--joc-refine-rounds N` | `2` | Quantization-aware coordinate-descent rounds. |
| `--joc-rd-passes N` | `1` | Huffman-aware RD passes. |
| `--joc-smooth F` | `0.9` | Causal covariance smoothing, `0..=1`. |
| `--joc-power-gate F` | `1e-8` | Per-band power gate relative to core energy. |
| `--emdf-target-block 0..5` | `2` | Audio block carrying the EMDF skip field. |
| `--addbsi-complexity 1..16` | object count (capped) | Atmos `complexity_index_type_a`. |
| `--drc-dynrng-profile 0..7` | `4` | Programme-information dynamic-range profile. |
| `--drc-compr-profile 0..7` | `4` | Programme-information compression profile. |

#### Downmix / signing

| Flag | Default | Meaning |
|---|---|---|
| `--pan hard71\|hard51\|vbap` | `hard71` | Downmix panning law. |
| `--gain-db DB` | `0` | Makeup gain on the five main channels (not LFE). |
| `--no-surround-phase-shift` | phase shift on | Write `dmx_config=0` and skip the +90° Ls/Rs all-pass. |
| `--emdf-key KEY` | — | Raw hex, `@hexfile`, or `@ddpmeta-key-set`. |
| `--emdf-key-id 0..7` | `0` | Key-set entry used for signing. |

### DAMF object clustering (`salsa-cluster`)

`salsa-cluster` reduces a DAMF master to at most N dynamic objects while
preserving the rendered scene.

```sh
cargo build --release -p salsa-cluster

# write a .atmos manifest
salsa-cluster master.atmos --out clustered.atmos --max-objects 15

# write the manifest and sidecars into a directory
salsa-cluster master.atmos --out out_dir --stem clustered --max-objects 15
```

| Flag | Default | Meaning |
|---|---|---|
| `--max-objects N` | `15` | Target dynamic object count. |
| `--centroid-score loudness\|reconstruction` | `loudness` | Rank centroids by loudness or by marginal rendered-scene error. |
| `--assign nearest\|pan2\|pan3\|auto\|audio` | `audio` | Source-to-cluster gain assignment. |
| `--audio-source-weighting none\|power\|importance` | `none` | Per-source weighting for the audio assignment objective. |
| `--beds-as-objects` | off | Let non-LFE beds compete as fixed-position inputs. |
| `--analysis-hop N` | `1536` | Samples between centroid updates. |
| `--metadata-interval-frames N` | `2` | Minimum keyframe interval. |
| `--max-seconds S` | `0` | Analyse only the first S seconds (`0` = whole master). |
| `--limit peak\|none` | `peak` | Peak-limit the output. |
| `--vad` | off | Enable the Silero VAD speech content prior. |
| `--vad-gain-db DB` | `6` | Maximum VAD excitation gain. |
| `--vad-threshold P` | `0.1` | VAD speech threshold. |
| `--vad-smooth-ms MS` | `100` | VAD probability smoothing time. |

The default is loudness-centered selection with audio-reconstruction gains
and unweighted source covariance. `--centroid-score reconstruction`
minimises rendered-scene error instead of ranking by loudness; it is slower and
can trade some perceptual accuracy for a different objective. `--help` lists
the remaining tuning parameters.

VAD uses the embedded Silero V6 model through `tract-onnx`; the master is
resampled to 16 kHz internally, so any DAMF sample rate works. It adds one
inference per source per frame, so it is opt-in.

### Virtual IEC 61937 audio device (macOS)

This lets an unmodified mpv play E-AC-3 JOC or TrueHD Atmos through Salsa. The
HAL driver and the Salsa service are installed separately. Install one codec
mode at a time.

Requirements: macOS 12 or newer, Xcode command-line tools, Rust 1.91 or newer,
GStreamer and FFmpeg (for example from Homebrew), and mpv 0.41 or newer.

#### 1. Build

```sh
cargo build --release -p salsa-audiod
```

Build the driver for the codec you will play:

```sh
# E-AC-3
MODE=eac3 ./platform/macos/salsa-hal/build.sh

# or TrueHD
MODE=truehd ./platform/macos/salsa-hal/build.sh
```

#### 2. Install the virtual audio device

E-AC-3:

```sh
sudo ./platform/macos/salsa-hal/install.sh \
  platform/macos/salsa-hal/build-eac3/SalsaHAL.driver
```

TrueHD:

```sh
sudo ./platform/macos/salsa-hal/install.sh \
  platform/macos/salsa-hal/build-truehd/SalsaHAL.driver
```

#### 3. Install the Salsa service

```sh
./platform/macos/salsa-audiod/install.sh
```

#### 4. Choose where sound comes out

```sh
salsa-audiod configure
```

The command lists your physical outputs and prompts for the output device,
render mode (speaker or binaural), and the speaker layout or binaural model
path. Choose `0` to follow the macOS system output; then you can switch outputs
in System Settings and start playback normally. This chooses where Salsa sends
audio; it does not change the macOS system output.

To follow the macOS system output:

```sh
salsa-audiod configure --device system --layout 7.1.4
```

For a fixed speaker layout without prompts:

```sh
salsa-audiod configure --device "MacBook Pro Speakers" --layout 7.1.4
```

For binaural output without prompts:

```sh
salsa-audiod configure \
  --device "Airpods" \
  --render binaural \
  --binaural-model /path/to/model.personalized_headphone
```

#### 5. Start the service

```sh
launchctl bootstrap gui/$(id -u) \
  "$HOME/Library/LaunchAgents/dev.salsa.audio.audiod.plist"
```

The service starts at login and is restarted automatically if it exits with an
error. After changing settings later, apply them with:

```sh
salsa-audiod configure --restart
```

#### 6. Play with mpv

E-AC-3:

```sh
mpv --ao=coreaudio_exclusive \
    --audio-device=coreaudio_exclusive/salsa-atmos-eac3 \
    --audio-spdif=eac3 \
    movie.mkv
```

TrueHD:

```sh
mpv --ao=coreaudio_exclusive \
    --audio-device=coreaudio_exclusive/salsa-atmos-truehd \
    --audio-spdif=truehd \
    movie.mkv
```

#### Change settings, stop, or uninstall

Change the output or renderer and apply it:

```sh
salsa-audiod configure --restart
```

Stop the service:

```sh
launchctl bootout gui/$(id -u)/dev.salsa.audio.audiod
```

Uninstall:

```sh
./platform/macos/salsa-audiod/uninstall.sh
sudo ./platform/macos/salsa-hal/uninstall.sh
```

To switch between E-AC-3 and TrueHD, uninstall the current driver, build and
install the other variant, then use the matching mpv command above.

## Salsa Studio (GUI preview player)

`crates/apps/salsa-gui/` is a small eframe/egui preview player built on the Salsa
elements.

Features:

- opens raw E-AC-3 JOC and TrueHD Atmos streams (`.ec3`, `.eac3`, `.mlp`, `.thd`, `.truehd`) and DAMF masters (`.atmos`);
- Speaker virtualization (object-rendered 2.0–9.1.6 layouts) and Binaural
  virtualization through a Rosella `.personalized_headphone` model;
- flushing, accurate seek and keyboard transport (Space, `←`/`→`,
  `Shift+←`/`→`, Home/End);
- 3D object stage with height color, level-sized objects, motion trails,
  speaker anchors, an LFE icon and a binaural head icon;
- a Settings switch that shows or hides the static 3D labels (object IDs,
  speaker names, LFE and listener); hovering an object still shows its detail;
- device/layout fallback warning with a one-click switch to Binaural;
- recent files, persisted output/model/volume/window settings, diagnostics;

Build and run:

```sh
cargo build --release -p gst-eac3joc -p gst-objectaudio -p gst-truehd -p gst-damf
cd crates/apps/salsa-gui
cargo run --release -- /path/to/stream.ec3
```

With no path argument, use `Cmd+O`, the Open button, or drag and drop. Playback
starts paused. The app auto-detects the repo's `target/release` or
`target/debug` plugin directory; otherwise set `GST_PLUGIN_PATH` before
launching. On macOS it sets `GST_REGISTRY_FORK=no` automatically.

Binaural playback needs a Rosella model; choose one in the Output panel. Raw
TrueHD has no container duration, so the app scans the parser output in the
background and shows `Scanning…` until the duration is known.

### macOS app bundle

```sh
SALSA_MODEL=/path/to/model.personalized_headphone \
  crates/apps/salsa-gui/scripts/bundle-macos.sh
```

This produces `crates/apps/salsa-gui/target/release/Salsa Studio.app`, copies the
four Salsa plugin dylibs into `Contents/Resources/gstreamer-1.0`, optionally
bundles the model, and ad-hoc signs the app. It still uses the host GStreamer
and FFmpeg installations; it does not bundle a GStreamer runtime.

### GUI tests

The GUI is a separate workspace, so run its tests from `crates/apps/salsa-gui`:

```sh
cd crates/apps/salsa-gui
cargo test
```

Real-stream playback tests are opt-in and use a headless sink:

```sh
cd crates/apps/salsa-gui
SALSA_TEST_EC3=/path/to/stream.ec3 \
SALSA_TEST_TRUEHD=/path/to/master.mlp \
SALSA_TEST_DAMF=/path/to/master.atmos \
SALSA_TEST_NONATMOS=/path/to/non-atmos.ec3 \
SALSA_TEST_MODEL=/path/to/model.personalized_headphone \
SALSA_SINK_OVERRIDE=fakesink SALSA_SINK_SYNC=true \
GST_PLUGIN_PATH=$PWD/../../target/debug \
cargo test --test playback
```

Without these variables the tests skip.

## Build & Test

**Prerequisites:** Rust 1.91+ (edition 2024), GStreamer 1.24+ (`gstreamer-1.0`, `gstreamer-base-1.0`, `gstreamer-audio-1.0`), and FFmpeg `libavcodec` (`ffmpeg-next` 9.x).

```sh
# macOS
brew install pkg-config gstreamer ffmpeg

# Debian / Ubuntu
sudo apt-get install pkg-config libgstreamer1.0-dev libgstreamer-plugins-base1.0-dev \
  libavcodec-dev libavformat-dev libavutil-dev libswresample-dev \
  libavfilter-dev libswscale-dev
```

```sh
cargo build --workspace
cargo test --workspace
cargo fmt --all --check
cargo clippy --workspace --all-targets -- -D warnings
```

**Opt-in real-stream integration tests:**

```sh
SALSA_TEST_EC3=/path/to/stream.ec3 cargo test -p gst-eac3joc --test pipeline
SALSA_TEST_EC3=/path/to/stream.ec3 SALSA_TEST_TRUEHD=/path/to/master.mlp \
  cargo test -p gst-objectaudio --test pipeline
SALSA_TEST_EC3=/path/to/stream.ec3 SALSA_REFERENCE_F32=/path/to/ref.f32le \
  cargo test -p eac3joc-dsp --test decode
SALSA_TEST_EC3=/path/to/stream.ec3 SALSA_ROSELLA_MODEL=/path/to/model.personalized_headphone \
  cargo test -p gst-objectaudio --test hrtf -- --nocapture
SALSA_ROSELLA_MODEL=/path/to/model.personalized_headphone \
  cargo test -p objectaudio-hrtf --test renderer
SALSA_TEST_TRUEHD=/path/to/master.mlp \
  cargo test -p truehd-atmos -p gst-truehd -- --nocapture
```

## License

Mozilla Public License 2.0 (MPL-2.0).

## References

- [JustOneCacophony](https://github.com/TheM14/JustOneCacophony) contributes main algorithms for E-AC-3 JOC decoding, object audio rendering and Rosella HRTF.
- [Dolby gst-home-audio](https://github.com/DolbyLaboratories/gst-home-audio) is the development reference, compatibility target and also the architectural inspiration for the Salsa gst plugin design.
- [truehdd](https://github.com/truehdd/truehdd) provides the original OAMD parser and truehd decoder.
- [atmos_quality](https://github.com/yuygfgg/atmos_quality) is a test suite for verifying the fidelity of Atmos decoders and renderers.
- [dolby-atmos-encoder](https://github.com/raress96/dolby-atmos-encoder) is a reference implementation for encoding E-AC-3 JOC.
