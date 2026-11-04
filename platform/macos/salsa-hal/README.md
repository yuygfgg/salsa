# Salsa HAL carrier device

The Salsa HAL plug-in receives native E-AC-3 and TrueHD carrier data from an
unmodified mpv and exposes it to `salsa-audiod` through a bounded shared-memory
ring. Byte-exact carrier delivery was verified on macOS 15.7.2 with mpv 0.41.0.
See [`docs/macos-packaging.md`](../../../docs/macos-packaging.md) for
installation and player settings.

A compressed packet must describe a complete burst. The tested ASBDs are:

| Mode | Format | Rate | Channels | Bytes/frame | Frames/packet | Bytes/packet |
|---|---|---:|---:|---:|---:|---:|
| E-AC-3 | `cac3` | 192000 | 2 | 4 | 6144 | 24576 |
| TrueHD MAT | `cac3` | 192000 | 8 | 16 | 3840 | 61440 |

mpv's compressed-format comparison can accept an old two-channel format before
an asynchronous change completes. Build a fixed-format device for the codec you
will play instead of relying on one device that switches between two and eight
channels.

## Device identities

| Build | Device UID | Name | Carrier |
|---|---|---|---|
| `MODE=eac3` | `salsa-atmos-eac3` | Salsa Atmos E-AC-3 | 2 ch, 6144 frames/packet |
| `MODE=truehd` | `salsa-atmos-truehd` | Salsa Atmos TrueHD | 8 ch, 3840 frames/packet |
| `MODE=native` (default) | `salsa-atmos` | Salsa Atmos | variable 2/8 ch, for comparison |
| `MODE=native8` | `salsa-atmos` | Salsa Atmos | fixed 8 ch, for comparison |
| `MODE=pcm` | `salsa-atmos` | Salsa Atmos | S16 PCM carrier, for comparison |
| `MODE=frame` | `salsa-atmos` | Salsa Atmos | one-frame negative control |

Install one variant at a time. Only the fixed `eac3` and `truehd` builds are
recommended for normal playback. The diagnostic `native`, `pcm`, and `frame`
builds remain available for capture tests.

## Build

Run commands from this directory:

```sh
MODE=eac3 ./build.sh
MODE=truehd ./build.sh
./build.sh
MODE=native8 ./build.sh
MODE=pcm ./build.sh
MODE=frame ./build.sh
```

Each build creates its own bundle directory and a matching
`salsa_carrier_capture` probe. Builds verify their ad-hoc signatures.
`LATENCY_FRAMES` sets the declared device latency; the default is zero.
`build-pcm-test.sh` is a wrapper for `MODE=pcm ./build.sh`.

The driver tests use AddressSanitizer and UndefinedBehaviorSanitizer. They check
deferred format changes, S16 word preservation, ring overflow, timestamp
advancement, and the diagnostic recording lifecycle:

```sh
sh tools/test_driver.sh
```

## Install and diagnose

```sh
sudo ./install.sh build-eac3/SalsaHAL.driver
./diagnose.sh
```

`install.sh` copies an explicitly selected, already-built bundle, fixes
permissions, verifies the signature, and restarts `coreaudiod`. It does not
rebuild a different variant.

## Fixtures

```sh
tools/make_fixtures.sh
```

The script creates 20-second extracts from the workspace masters and muxes both
through FFmpeg's SPDIF muxer. The fixtures contain 625 E-AC-3 bursts and 999
TrueHD MAT bursts. Use the same FFmpeg version as the tested mpv build. Fixtures
are generated locally and are not stored in Git.

## Regression checks

Run the Rust carrier and lifecycle tests from the repository root:

```sh
cargo test -p iec61937 -p salsa-audiod
```

The tests use in-memory buffers or temporary files. They do not require a
repository-local log directory. Fixture comparisons need the files generated
by `tools/make_fixtures.sh`. Binaural comparisons also need
`SALSA_ROSELLA_MODEL` to identify a local headphone model.

Run the marker-coordinate regression from this directory:

```sh
python3 -m unittest discover -s tools -p 'test_*.py'
```

Physical output and A/V synchronization require hardware measurements.

## Capture method

The diagnostic path records native `WriteMix` bytes inside the driver. The probe
arms a fixed-capacity buffer while I/O is stopped; the callback copies bytes
into it and counts overflow. After playback stops, the probe reads the buffer
through a custom `CFData` property and writes the file. Recording memory is
allocated and released outside the callback. The capacity is 32 seconds at the
maximum carrier rate and channel count.

```sh
./build-eac3/salsa_carrier_capture --probe
./build-eac3/salsa_carrier_capture --record-start
# Play one fixture to the Salsa device, then wait for mpv to exit.
CAPTURE_DIR=$(mktemp -d)
./build-eac3/salsa_carrier_capture --record-save "$CAPTURE_DIR/manual.raw"
```

The loopback input path remains available with `--out`, `--channels`, and
`--seconds`. It validates the actual S16 or Float32 input format. A separate
capture client can be suspended while mpv owns the device in exclusive mode, so
do not use that path as the acceptance recorder.

The memory recorder is diagnostic instrumentation. Normal playback uses the
shared-memory carrier ring read by `salsa-audiod`.

## Timing harness

`service/` contains the timing prototype used to validate delay and clock
feedback on the PCM carrier device:

| File | Purpose |
|---|---|
| `salsa_audiod.c` | Reads `/salsa-hal-carrier`, schedules clicks tied to IEC 61937 bursts, publishes the physical output clock, and renders to CoreAudio. |
| `loopback_capture.c` | Records a CoreAudio input device to raw Float32 with host/sample metadata. |
| `clock_probe.c` | Reads the HAL zero timestamp through a custom property and compares it with the shared clock mapping. |
| `build.sh` | Builds the timing tools into `build/`. |
| `summarize_clock.py` | Summarizes clock-probe CSVs. |

Build the tools with `./service/build.sh`. Select the capture and output
paths explicitly when running each tool. Keep captures and logs in a temporary
directory outside the repository. `tools/measure_markers.py` compares marker
host times with file-relative click positions.

These tools require an installed HAL and suitable loopback hardware. Unit
and fixture tests do not verify physical latency or A/V synchronization.

## Files and system state

| File | Purpose |
|---|---|
| `src/SalsaHAL.c` | Audio Server plug-in: formats, ring, loopback, diagnostic recording |
| `src/SalsaHALProbe.h` | Custom property identifiers and diagnostic counters |
| `tools/salsa_carrier_capture.c` | HAL probe, driver record reader, optional loopback capture |
| `tools/compare_carrier.py` | Complete fixture comparison; only zero padding is allowed |
| `tools/test_driver.c` | Direct driver contract tests with a mock host |
| `service/` | Timing service and measurement tools |

The probe is not a default system output device. Installation and removal
require administrator access and restart `coreaudiod`. Large fixtures, raw
captures, and compiled bundles are ignored by Git.
