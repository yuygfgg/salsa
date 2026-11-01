# gst-plugin-salsa

**SALSA** — *SALSA Ain't Licensed Spatial Audio*

GStreamer plugins for decoding E-AC-3 JOC and TrueHD Atmos into object-based audio, rendering to speaker layouts or binaural headphones, and bridging with Dolby OAMD pipelines.

> [!TIP]
> Salsa is designed for spatial audio only. For non-object TrueHD or E-AC-3 streams, use standard GStreamer decoders.

## Elements Overview

| Plugin            | Element                 | Description                           | Input → Output                                                                                                                                                                                                                    |
| ----------------- | ----------------------- | ------------------------------------- | --------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------- |
| `gst-eac3joc`     | `salsaeac3jocparse`     | E-AC-3 JOC syncframe parser           | `audio/x-eac3` (unframed or framed) → `audio/x-eac3, framed=true, alignment=frame, rate=8000..48000, channels=1..8, atmos={false,true}` (one syncframe per buffer)                                                                    |
| `gst-eac3joc`     | `salsaeac3jocdec`       | E-AC-3 JOC object decoder             | `audio/x-eac3, framed=true, alignment=frame` → `audio/x-raw` F32LE interleaved, `channels=1..16` (LFE + JOC objects), `rate={32000,44100,48000}`, `channel-mask=0`, optional `meta:ObjectAudioMeta`                                  |
| `gst-truehd`      | `salsatruehdparse`      | TrueHD access-unit parser             | `audio/x-true-hd` or `audio/x-truehd` (unframed or framed) → `audio/x-true-hd, framed=true, alignment=frame, rate={44100,48000,88200,96000,176400,192000}, atmos={false,true}`                                                       |
| `gst-truehd`      | `salsatruehdatmosdec`   | TrueHD Atmos decoder (presentation 3) | `audio/x-true-hd` or `audio/x-truehd`, `framed=true, alignment=frame`, `rate=1..655350` → `audio/x-raw` F32LE interleaved, `channels=1..16`, `rate={44100,48000,88200,96000,176400,192000}`, `channel-mask=0`, optional `meta:ObjectAudioMeta` |
| `gst-objectaudio` | `salsaoar`              | Speaker layout renderer               | `audio/x-raw` F32LE interleaved with `meta:ObjectAudioMeta`, `rate=32000..48000`, `channels=1..1024`, `channel-mask=0` → `audio/x-raw` F32LE interleaved, one structure per layout (`2.0`–`9.1.6`), layout channel count and mask     |
| `gst-objectaudio` | `salsahrtfrender`       | Binaural HRTF renderer (Rosella)      | `audio/x-raw` F32LE interleaved with `meta:ObjectAudioMeta`, `rate=48000`, `channels=1..1024`, `channel-mask=0` → `audio/x-raw` F32LE interleaved stereo, 48 kHz, `channel-mask=0x3`                                               |
| `gst-objectaudio` | `salsaobjectaudioadapt` | Salsa ↔ Dolby OAMD metadata bridge    | `audio/x-raw` F32LE/S32LE interleaved, `rate=32000..48000`, `channels=1..32`, `channel-mask=0`, with `meta:ObjectAudioMeta` or `meta:DlbObjectAudioMeta` (± `memory:SystemMemory`) or no feature → same rate/channels; `to-dolby` → `meta:DlbObjectAudioMeta`, `to-salsa` → `meta:ObjectAudioMeta`; `float=true` also converts S32LE → F32LE |

## Quick Start

```sh
cargo build --release -p gst-eac3joc -p gst-objectaudio -p gst-truehd
GST_PLUGIN_PATH=$PWD/target/release gst-inspect-1.0 salsaeac3jocdec
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

> [!NOTE]
> On macOS/Homebrew, if a fresh registry scan deadlocks in `gst-plugin-scanner`, prefix with `GST_REGISTRY_FORK=no`.

## Salsa Studio (GUI preview player)

`apps/salsa-gui/` is a small eframe/egui preview player built on the Salsa
elements.

Features:

- opens raw E-AC-3 JOC and TrueHD Atmos streams (`.ec3`, `.eac3`, `.mlp`, `.thd`, `.truehd`);
- Speaker virtualization (object-rendered 2.0–9.1.6 layouts) and Binaural
  virtualization through a Rosella `.personalized_headphone` model;
- flushing, accurate seek and keyboard transport (Space, `←`/`→`,
  `Shift+←`/`→`, Home/End);
- 3D object stage with height color, level-sized objects, motion trails,
  speaker anchors, an LFE icon and a binaural head icon;
- device/layout fallback warning with a one-click switch to Binaural;
- recent files, persisted output/model/volume/window settings, diagnostics;

Build and run:

```sh
cargo build --release -p gst-eac3joc -p gst-objectaudio -p gst-truehd
cd apps/salsa-gui
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
  apps/salsa-gui/scripts/bundle-macos.sh
```

This produces `apps/salsa-gui/target/release/Salsa Studio.app`, copies the
three Salsa plugin dylibs into `Contents/Resources/gstreamer-1.0`, optionally
bundles the model, and ad-hoc signs the app. It still uses the host GStreamer
and FFmpeg installations; it does not bundle a GStreamer runtime.

### GUI tests

The GUI is a separate workspace, so run its tests from `apps/salsa-gui`:

```sh
cd apps/salsa-gui
cargo test
```

Real-stream playback tests are opt-in and use a headless sink:

```sh
cd apps/salsa-gui
SALSA_TEST_EC3=/path/to/stream.ec3 \
SALSA_TEST_TRUEHD=/path/to/master.mlp \
SALSA_TEST_NONATMOS=/path/to/non-atmos.ec3 \
SALSA_TEST_MODEL=/path/to/model.personalized_headphone \
SALSA_SINK_OVERRIDE=fakesink SALSA_SINK_SYNC=true \
GST_PLUGIN_PATH=$PWD/../../target/debug \
cargo test --test playback
```

Without these variables the tests skip.

## Object Audio Format

All Salsa elements exchange a unified raw object stream:

- **PCM Format**: `audio/x-raw, format=F32LE, layout=interleaved, channel-mask=0` (the Dolby bridge additionally accepts `S32LE` and can convert it).
- **Channel Mapping**: Channel `0` is LFE; channels `1..N` are dynamic point objects (`channels - 1` = object count). Decoders emit `1 + object_count` channels at 32/44.1/48 kHz for E-AC-3 JOC and up to 192 kHz for TrueHD; the renderers accept up to 1024 channels. Non-LFE beds are expanded into fixed-position objects by the TrueHD decoder or by `salsaobjectaudioadapt`; the E-AC-3 JOC decoder emits its JOC objects only.
- **Metadata (`meta:ObjectAudioMeta`)**: Carries a versioned, postcard-serialized `SpatialPayload` with snapshot and intra-buffer event sample offsets. Decoders advertise (and attach) it when downstream negotiates the caps feature, falling back to plain raw otherwise; `salsaoar` and `salsahrtfrender` require it, and Dolby `meta:DlbObjectAudioMeta` must be converted first by `salsaobjectaudioadapt`.

## Element Reference

### Parsers (`salsaeac3jocparse`, `salsatruehdparse`)

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

### Decoders (`salsaeac3jocdec`, `salsatruehdatmosdec`)

Both decoders handle flushing seeks and reset their codec/DSP state on `FLUSH_STOP` (and on `DISCONT` for non-flushing seeks). They take no properties:

- **`salsaeac3jocdec`**: Uses `libavcodec` for the E-AC-3 core and the JOC kernel for object reconstruction.
  - Sink caps: `audio/x-eac3, framed=(boolean)true, alignment=(string)frame, rate=(int)[1,655350]` only. Plain AC-3 cannot be linked.
  - Src caps: `audio/x-raw`, F32LE, interleaved, `channels=1..16`, `rate={32000,44100,48000}`, `channel-mask=0`, optionally with `meta:ObjectAudioMeta`.
  - Output channel count is `1 + N` where `N` is the JOC object count; it is locked on the first frame, and a mid-stream change is rejected.
- **`salsatruehdatmosdec`**: Pure-Rust TrueHD Atmos decoder (`truehd` crate).
  - Sink caps: `audio/x-true-hd` or `audio/x-truehd`, `framed=(boolean)true`, `alignment=(string)frame`, `rate=(int)[1,655350]`.
  - Src caps: `audio/x-raw`, F32LE, interleaved, `channels=1..16`, `rate={44100,48000,88200,96000,176400,192000}`, `channel-mask=0`, optionally with `meta:ObjectAudioMeta`.
  - Consumes the access-unit-per-buffer output of `salsatruehdparse`; unframed streams are not accepted. It accumulates 40-sample access units into 32-sample blocks and emits a buffer per 1216 samples or per OAMD update. Presentation bed channels are expanded into fixed-position objects.

### `salsaoar` (Speaker Renderer)

Sample-accurate equal-power panning to standard speaker layouts (`2.0`, `3.1`, `4.0`, `5.1`, `7.1`, `5.1.2`, `5.1.4`, `7.1.2`, `7.1.4`, `9.1.4`, `9.1.6`), passing LFE straight through.

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

### `salsahrtfrender` (Binaural Renderer)

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

### `salsaobjectaudioadapt` (Dolby OAMD Bridge)

Converts between Salsa `ObjectAudioMeta` and Dolby `gst-home-audio` (Reference Player) `DlbObjectAudioMeta` (resolved dynamically at runtime from loaded Dolby plugins). Note that OAMD quantization (6-bit x/y, 4-bit z, integer dB) and bed-to-object expansion are lossy. This element is in-place and does not change channel count, rate, or layout.

- **Sink caps**: `audio/x-raw` F32LE or S32LE interleaved, `rate=32000..48000`, `channels=1..32`, `channel-mask=0`, with `meta:ObjectAudioMeta` or `meta:DlbObjectAudioMeta` (either may be combined with `memory:SystemMemory`), or no caps feature.
- **Src caps**: same format, rate, and channels. `direction=to-dolby` advertises `meta:DlbObjectAudioMeta` (or `meta:ObjectAudioMeta` when `remove=true`); `direction=to-salsa` advertises `meta:ObjectAudioMeta`; `auto` offers both and resolves from the sink caps (Dolby feature present → `to-salsa`, otherwise `to-dolby`).
- `float=true` makes the output `F32LE` and converts `S32LE` input samples in place.

| Property    | Default | Description                                                                                                                   |
| ----------- | ------- | ----------------------------------------------------------------------------------------------------------------------------- |
| `direction` | `auto`  | `auto` (from input caps), `to-dolby` (`SpatialPayload` → OAMD ID11), or `to-salsa` (OAMD → `SpatialPayload`).                 |
| `remove`    | `false` | In `to-dolby` mode, do not add `DlbObjectAudioMeta`; existing Salsa metadata is left untouched and the src advertises Salsa caps. |
| `float`     | `false` | Convert interleaved `S32LE` samples to `F32LE` in place.                                                                      |

## Build & Test

**Prerequisites:** Rust 1.88+ (edition 2024), GStreamer 1.24+ (`gstreamer-1.0`, `gstreamer-base-1.0`, `gstreamer-audio-1.0`), and FFmpeg `libavcodec` (`ffmpeg-next` 9.x).

```sh
# macOS
brew install pkg-config gstreamer ffmpeg

# Debian / Ubuntu
sudo apt-get install pkg-config libgstreamer1.0-dev libgstreamer-plugins-base1.0-dev \
  libavcodec-dev libavformat-dev libavutil-dev libswresample-dev
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
- [Dolby gst-home-audio](https://github.com/DolbyLaboratories/gst-home-audio) is the development reference, compatibility target and also the architectural inspiration for the Salsa plugin design.
- [truehdd](https://github.com/truehdd/truehdd) provides the original OAMD parser and truehd decoder.
- [gst-plugin-oamd](https://github.com/yuygfgg/gst-plugin-oamd) can be useful to export DAMF master files.
- [atmos_quality](https://github.com/yuygfgg/atmos_quality) is a test suite for verifying the fidelity of Atmos decoders and renderers.
