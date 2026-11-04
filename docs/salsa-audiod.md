# Salsa audio service (`salsa-audiod`)

`salsa-audiod` receives an IEC 61937 carrier (E-AC-3 JOC or TrueHD in MAT),
recovers the codec payload, runs the existing Salsa parsers/decoders and
renderers, and hands PCM to a bounded output scheduler. It can read an offline
carrier capture or the HAL shared-memory ring.

The codec path is shared with the rest of Salsa. The service does not implement
a second decoder or metadata path, and it derives GStreamer timestamps from
carrier sample positions.

See [macOS packaging](macos-packaging.md) for installation, device identities,
player settings, diagnostics, and uninstallation.

## Signal path

```text
carrier file or HAL ring
    -> iec61937 (bursts, E-AC-3 syncframes, TrueHD MAT access units)
    -> appsrc
    -> salsaeac3jocparse / salsatruehdparse
    -> salsaeac3jocdec  / salsatruehdatmosdec      (SpatialFrame + ObjectAudioMeta)
    -> salsaoar         / salsahrtfrender
    -> bounded appsink
    -> WAV, null, memory, or osxaudiosink scheduler
```

## Usage

Build the service:

```sh
cargo build --release -p salsa-audiod
```

Render an offline capture to WAV:

```sh
salsa-audiod render \
  --carrier platform/macos/salsa-hal/fixtures/eac3_20s.spdif \
  --codec eac3 --layout 7.1.4 \
  --output /tmp/eac3.wav --json /tmp/eac3.json
```

Render binaural:

```sh
salsa-audiod render \
  --carrier platform/macos/salsa-hal/fixtures/truehd_20s.spdif \
  --codec truehd \
  --binaural-model /path/to/model.personalized_headphone \
  --filterbank 2 --output /tmp/truehd_binaural.wav
```

Compare recovered carrier rendering with direct GStreamer input:

```sh
salsa-audiod compare \
  --carrier platform/macos/salsa-hal/fixtures/eac3_20s.spdif \
  --direct  platform/macos/salsa-hal/fixtures/eac3_20s.ec3 \
  --codec eac3 --layout 7.1.4 \
  --json /tmp/compare.json
```

Choose the physical output and render settings once:

```sh
salsa-audiod configure
salsa-audiod configure --show
```

The interactive setup always asks for the output device (showing the current
one as the default; choose `0` to follow the macOS system output), then speaker
or binaural rendering and, for binaural, the `.personalized_headphone` model
path. Non-interactive:

```sh
salsa-audiod configure --device system --layout 7.1.4
salsa-audiod configure --device "MacBook Pro Speakers" --layout 7.1.4
salsa-audiod configure --device "MacBook Pro Speakers" \
  --render binaural --binaural-model /path/to/model.personalized_headphone
```

Run live from the HAL ring using the saved configuration:

```sh
salsa-audiod run
```

The configuration lives in
`~/Library/Application Support/Salsa/audiod.json` (or
`$SALSA_AUDIOD_CONFIG`). CLI options override it for one run, for example:

```sh
salsa-audiod run --device "MacBook Pro Speakers" --layout 7.1.4
```

`salsa-audiod devices` lists outputs; `--device` accepts a list number, device
name, unique ID, or `default`. The service refuses the Salsa virtual device so
it cannot create a loop. This setting chooses where the service sends audio; it
does not change the macOS system output. Use the Salsa device in mpv (or in
System Settings → Sound for other players).

### Live timing options

| Option | Default | Meaning |
|---|---:|---|
| `--scheduling-allowance-ms` | 50 | Fixed queue/scheduling allowance for the session. |
| `--physical-latency-ms` | 0 | Measured latency of the physical output. |
| `--declare-latency` | off | Publish the contract to the HAL control block. |
| `--deadline-timeout-ms` | 2000 | Count a live output stall after this delay while carrier data is arriving. |
| `--hard-stall-timeout-ms` | 10000 | Log a recovery warning after a stall; the output sink is rebuilt instead of stopping the session. |

## Carrier input

| Source | Use |
|---|---|
| `CarrierSource::File` | Captured `.spdif` bytes. |
| `CarrierSource::Shared` | `/salsa-hal-carrier` shared-memory ring. Mirrors `SalsaCarrierShared.h`; the service copies one slot at a time and advances `tail`. |
| `CarrierSource::Scripted` | Pre-built chunks for lifecycle tests and diagnostics. |

## Lifecycle and recovery

A HAL `StartIO` after a stop, or an output format change, increments the shared
session generation. A `DISCONTINUITY`/`DROPPED` slot flag also marks a break.
On either event the service:

1. resets the IEC 61937 depacketizer and the timestamp timeline;
2. stops and joins the PCM pull thread. Each puller keeps an immutable session
   epoch and carrier clock, including for samples already taken from `appsink`;
3. keeps the existing decode/render pipeline and performs a native GStreamer
   flushing seek to the start of the new session. The seek flushes queued
   output and resets the parser, decoder and renderer in place;
4. increments the session epoch and starts a new PCM pull thread;
5. flushes the physical output when the writer observes the new epoch. The
   writer discards PCM from other epochs and resets its timestamp anchor.

A seek during pipeline startup, before the first rendered buffer, rebuilds the
pipeline once because a native seek can race caps negotiation. Normal seeks
stay on the native path and do not reload the renderer. All queues remain
bounded (`appsrc` 4 MiB, `appsink` 8 buffers, output channel 8). A late buffer
is rebased onto the current host time and counted instead of aborting playback.
A live-output stall is counted after the deadline allowance while carrier data
is still arriving; the output writer bounds each `appsrc` push and rebuilds the
stalled sink. Pause without carrier data is not a deadline failure.

`RenderReport.lifecycle` reports input chunks, sessions, session resets,
carrier discontinuities, output discontinuities (initial start and each reset),
deadline failures, the longest output stall, scheduling re-anchors, the worst
lateness, output recoveries, HAL dropped carrier frames, and reader-skipped
slots.

## Render verification

The carrier and direct paths use the same parser/decoder/renderer. With the
fixtures generated by `platform/macos/salsa-hal/tools/make_fixtures.sh`, the
rendered PCM is identical:

| Codec | Renderer | Carrier frames | Direct frames | Compared | Max abs diff |
|---|---|---:|---:|---:|---:|
| E-AC-3 | 7.1.4 speaker | 960,000 | 960,000 | 960,000 | 0.0 |
| TrueHD | 7.1.4 speaker | 959,040 | 960,000 | 959,040 | 0.0 |
| E-AC-3 | binaural (bank 2) | 1,201,152 | 1,201,152 | 955,456 | 0.0 |
| TrueHD | binaural (bank 2) | 1,199,616 | 1,201,152 | 954,516 | 0.0 |

The TrueHD carrier fixture is one MAT frame shorter than the raw `.mlp` prefix
(999 MAT frames, 23,976 access units, 959,040 programme samples), so the
speaker comparison checks the common decoded prefix. The binaural comparison
excludes a guard region at the end (`filterbank latency + 4096` samples) because
the room tail starts at a different point when one input is 960 samples
shorter; the compared main-signal region is still bit-exact.

Run the opt-in checks:

```sh
SALSA_TEST_SPDIF_FIXTURES="$PWD/platform/macos/salsa-hal/fixtures" \
SALSA_ROSELLA_MODEL=/path/to/model.personalized_headphone \
  cargo test -p salsa-audiod --release --test carrier_render

SALSA_TEST_SPDIF_FIXTURES="$PWD/platform/macos/salsa-hal/fixtures" \
  cargo test -p salsa-audiod --release --test lifecycle
```

Without the environment variables the checks skip, so the default test matrix
stays hermetic. The two binaural render checks are ignored in debug builds:
unoptimized HRTF convolution makes them take many minutes. Run them with
`--release`.

Physical seek, pause/resume, and device-change checks require hardware. Store
captures in a temporary directory outside the repository.

## Output path

`OutputSink` is owned by one writer thread. The pipeline hands it `PcmChunk`s
through a bounded `sync_channel` (capacity 8).

| Backend | Purpose |
|---|---|
| `Wav` | IEEE-float WAV for offline inspection. |
| `Null` | Throughput and statistics without I/O. |
| `Memory` | Comparison and lifecycle tests. |
| `CoreAudio` | Maps carrier host time to the device clock and sends PCM through a bounded queue to `osxaudiosink`. |

`appsink` runs with `sync=false`; the physical scheduler decides when samples
play. The renderer emits F32LE interleaved PCM in its standard layout order.
PCM chunks retain the full negotiated caps, including the channel mask.
Both appsrc queues wait for capacity and check cancellation and pipeline errors.
The codec input queue is limited to 4 MiB; the physical PCM queue is limited to
512 KiB. A buffer larger than its queue limit returns an error.

For live playback, each session carries a host-time anchor from the HAL.
The physical output adds the ledger's scheduling allowance to that timeline.
The PCM already contains codec and renderer signal delay. The ledger adds those
signal delays and the measured physical device latency once when it declares
the HAL latency. A missed playback deadline drops the late gap and re-anchors
the scheduling timeline at the current host time; it no longer stops the
service. File playback uses the first PCM timestamp as its anchor.

## Delay ledger

The ledger keeps signal delay, scheduling allowance, and parser/renderer
lookahead separate:

| Term | E-AC-3 | TrueHD | Notes |
|---|---:|---:|---|
| Codec signal delay | 1,473 samples | 0 | `EAC3_DECODER_DELAY`; TrueHD has no parser-side delay. |
| Renderer signal delay | 0 | 0 | Speaker renderer is sample-aligned. |
| Binaural filterbank | 961 (bank 1) / 448 (bank 2) | same | Reported through the renderer and included as signal delay. |
| Speaker block | 31 samples | 31 samples | Processing, not signal delay. |
| Parser lookahead | 1,536 samples | 40 samples | Scheduling requirement, not signal delay. |
| Scheduling allowance | default 50 ms | default 50 ms | Fixed per output session. |
| Physical output | 0 for file/null | 0 for file/null | `--physical-latency-ms` supplies a measured CoreAudio value; automatic query is not wired into the declared contract. |

`DelayLedger::declared_carrier_frames()` converts the contract to the HAL's
192 kHz frame units. The service warns when the allowance is smaller than the
parser/renderer lookahead. `--declare-latency` writes the value into the shared
HAL control block.
