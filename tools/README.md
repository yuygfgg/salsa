# tools

Helper scripts that are not part of the Rust workspace build.

## `x86-interop-env.sh`, `build-x86-plugins.sh`, `test-dolby-interop.sh`

Harness for the x86_64-only Dolby Reference Player runtime.
`source tools/x86-interop-env.sh` prepares a cached x86_64 GStreamer/FFmpeg
prefix, `tools/build-x86-plugins.sh` stages the salsa plugins, and
`tools/test-dolby-interop.sh [stream.ec3]` runs the bidirectional matrix. See
[`docs/gst-plugins/interop.md`](../docs/gst-plugins/interop.md).

## `check-oamd-source.sh`

Fails if the workspace still resolves a standalone `oamd` package (the upstream
parser is merged into `objectaudio-oamd`), or if a crate above
`objectaudio-oamd` imports `oamd` directly.

```sh
tools/check-oamd-source.sh
```

## `test-atmos-tools-adapter.sh`

Follows `gst-plugin-oamd/atmos_tools.py`'s decode/binaural file layout while
replacing the Dolby decoder with a salsa decoder + `salsaobjectaudioadapt`. It
then exercises `oamdcapsfeature`, `oamdserialize` and `dlbdapvr` on the salsa
stream, and compares metadata IDs/event counts, CAF properties and binaural
waveforms against the `atmos_tools.py` reference.

```sh
tools/test-atmos-tools-adapter.sh /path/to/stream.ec3
tools/test-atmos-tools-adapter.sh /path/to/master.mlp
```

Intermediate PCM/CAF/WAV files live under `$SALSA_X86_CACHE/atmos-tools-adapter`
and are removed on success. Set `SALSA_KEEP_INTEROP=1` to keep them.

## `test-damf-export.sh`

Verifies DAMF export through the external `gst-plugin-oamd` tooling. It runs
`atmos_tools.py --mode decode`, checks `.atmos`/`.metadata`/`.caf`/`.pcm`,
compares PCM and metadata with a direct Dolby decode, round-trips the CAF, and
optionally runs the binaural mode and a Cavern read-back.

```sh
tools/test-damf-export.sh /path/to/stream.ec3 --binaural --cavern
```

`gst-plugin-oamd` is not modified; see
[`docs/gst-plugins/damf-export.md`](../docs/gst-plugins/damf-export.md).
