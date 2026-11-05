# Salsa ALSA carrier devices

The Linux back end is an ALSA PCM plug-in (`libasound_module_pcm_salsa.so`). It
accepts the opaque IEC 61937 words that mpv writes for S/PDIF passthrough and
publishes them to the same shared-memory ring as the macOS HAL. The ring layout
is defined in [`platform/common/salsa_carrier_shared.h`](../../common/salsa_carrier_shared.h).

| PCM | Carrier | Channels | Rate | Format |
|---|---|---:|---:|---|
| `salsa_atmos_eac3` | E-AC-3 JOC | 2 | 192000 | `S16_LE` |
| `salsa_atmos_truehd` | TrueHD MAT | 8 | 192000 | `S16_LE` |

The plug-in exposes an output PCM only. It does not mix, resample, or decode.
`mpv`'s ALSA S/PDIF path supplies S16 carrier words and appends `AES0`–`AES3`
parameters to the device name; the configuration defines those arguments and
the plug-in ignores their values.

## Build

```sh
./build.sh
```

The result is `build/libasound_module_pcm_salsa.so`.

## Install

System-wide:

```sh
sudo ./install.sh
```

Per user, without root:

```sh
./install.sh --user
```

The user install writes the plug-in to `$XDG_DATA_HOME/salsa` and adds a managed
block to `$XDG_CONFIG_HOME/alsa/asoundrc` (normally `~/.config/alsa/asoundrc`).
The block can be removed with `./uninstall.sh --user`.

## Test

The virtual PCM does not produce audio itself. Start a consumer first:

```sh
salsa-audiod render --carrier-shm /salsa-hal-carrier-eac3 \
  --codec eac3 --layout 7.1.4 --output /tmp/eac3.wav
```

Then play with mpv:

```sh
mpv --ao=alsa --audio-device=alsa/salsa_atmos_eac3 \
  --audio-spdif=eac3 movie.mkv
```

Use `salsa-audiod run` instead of `render` to send the decoded result to a
physical output. `salsa-audiod devices` lists ALSA outputs; `--device` accepts
`alsa:<name>`, `pulse:<sink>`, or `pipewire:<target>`.

## Files

| File | Purpose |
|---|---|
| `src/pcm_salsa.c` | ALSA ioplug PCM, carrier pacing and shared-ring writer |
| `salsa.conf.in` | ALSA PCM definitions and mpv AES argument declarations |
| `build.sh` | Builds the shared library |
| `install.sh` / `uninstall.sh` | System or per-user installation |
| `../../common/salsa_carrier_shared.h` | Carrier ABI shared with the macOS HAL |
