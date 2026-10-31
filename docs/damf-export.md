# DAMF export

Salsa does not include an ADM/DAMF writer. DAMF export uses the external
`gst-plugin-oamd` tooling. Two decode chains are supported:

- the Dolby Reference Player chain (`atmos_tools.py --mode decode`);
- the salsa chain (`salsaeac3joc*` / `salsatruehd*` + `salsaobjectaudioadapt`),
  which reuses the same `gst-plugin-oamd` packaging elements.

The Reference Player export is lossless with respect to the Dolby decode. The
salsa chain is lossless with respect to the salsa decoded object stream; the
adapter's OAMD encoding is documented below.

## Toolchain

| Item | Value |
|---|---|
| Tool | `gst-plugin-oamd/atmos_tools.py` (`--mode decode`) |
| Plugin | `dlboamdmod` 0.1.0 (`oamdcapsfeature`, `oamdserialize`) |
| Runtime | Dolby Reference Player GStreamer |
| Reference decoder | `dlbac3dec` / `dlbtruehddec` |
| Salsa chain | `salsaeac3jocparse ! salsaeac3jocdec` / `salsatruehdparse ! salsatruehdatmosdec` + `salsaobjectaudioadapt` |
| Read-back renderer | Cavern (`aq render`, verification only) |

`gst-plugin-oamd` is treated as a third-party dependency and is not modified.

## Dolby Reference decode chain

The tool detects E-AC-3 or TrueHD from the raw header and runs the Reference
Player decoder:

```text
E-AC-3:
  filesrc ! dlbac3parse enable-metadata=true ! dlbac3dec out-ch-config=21
    ! tee
      ! queue ! oamdcapsfeature remove=true ! audioconvert
          ! audio/x-raw,format=S24LE,layout=interleaved ! filesink <stem>.pcm
      ! queue ! oamdserialize ! filesink <stem>.metadata

TrueHD:
  filesrc ! dlbtruehdparse align-major-sync=false
    ! dlbtruehddec out-ch-config=21 presentation=16
    ! (same tee / PCM / metadata split)
```

It then converts the 16-channel S24LE PCM to `<stem>.caf` with ffmpeg and writes
the `<stem>.atmos` manifest (version `0.5.1`, LFE bed ID 3, object IDs 10–24).

The Dolby chain above is the reference. The next section replaces only the
decoder and metadata encoder with salsa while keeping the same
`gst-plugin-oamd` packaging elements.

## Salsa decode chain (`salsaobjectaudioadapt`)

`atmos_tools.py --mode decode` hardcodes `dlbac3parse`/`dlbac3dec` and
`dlbtruehdparse`/`dlbtruehddec`, so it cannot drive the salsa decoders directly.
The equivalent salsa chain decodes to the salsa IR, converts it to Dolby OAMD
at the adapter boundary, and then uses the same `oamdcapsfeature` /
`oamdserialize` elements:

```text
E-AC-3:
  filesrc blocksize=1048576 ! salsaeac3jocparse ! salsaeac3jocdec
    ! salsaobjectaudioadapt direction=to-dolby
    ! tee
      ! queue ! oamdcapsfeature remove=true ! audioconvert
          ! audio/x-raw,format=S24LE,layout=interleaved ! filesink <stem>.pcm
      ! queue ! oamdserialize ! filesink <stem>.metadata

TrueHD:
  filesrc blocksize=1048576 ! salsatruehdparse ! salsatruehdatmosdec
    ! salsaobjectaudioadapt direction=to-dolby
    ! (same tee / PCM / metadata split)
```

### Packaging

The PCM and metadata streams are then packaged exactly like the Dolby chain:

1. Count the object IDs in `<stem>.metadata` (IDs `>= 10`) and set
   `channels = 1 + object_count` for the CAF header.
2. Convert the S24LE PCM to CAF:

   ```sh
   ffmpeg -y -v error -f s24le -ar 48000 -ac "$CHANNELS" \
     -i <stem>.pcm -c:a copy <stem>.caf
   ```

3. Write `<stem>.atmos` with version `0.5.1`, LFE bed ID `3`, object IDs
   `10..N`, and the same fields as the `atmos_tools.py` manifest. The manifest
   generation is implemented in
   [`../tools/test-atmos-tools-adapter.sh`](../tools/test-atmos-tools-adapter.sh).

### Binaural

The adapter output also feeds the Dolby binaural renderer directly.
`source tools/x86-interop-env.sh` provides `$SALSA_GST_LAUNCH` and the plugin
paths:

```sh
"$SALSA_GST_LAUNCH" -e filesrc location=<stream> blocksize=1048576 ! \
  <salsa-decoder-chain> ! salsaobjectaudioadapt direction=to-dolby ! \
  oamdcapsfeature float=true ! dlbdapvr ! wavenc ! \
  filesink location=<stem>.binaural.wav
```

### Differences from the Dolby chain

- `<stem>.pcm` is the salsa-decoded object PCM. It is not expected to be 
  byte-identical to the Dolby `.pcm`.
- `<stem>.metadata` is OAMD encoded by the adapter from `SpatialPayload`.
  OAMD quantization (6-bit x/y, 4-bit z, integer dB) and the dynamic-object-only
  representation are lossy relative to Dolby's own OAMD, so the metadata is
  not byte-identical to `oamdserialize` on the Dolby decode.
- The `.atmos` manifest and the CAF/PCM layout are identical.

## Verification

Dolby Reference chain:

```sh
tools/test-damf-export.sh /path/to/stream.ec3 --binaural --cavern
```

Salsa + adapter chain:

```sh
tools/test-atmos-tools-adapter.sh /path/to/stream.ec3
```

`test-atmos-tools-adapter.sh` builds the x86_64 salsa plugins, runs the salsa
decode + adapter pipeline above, and compares the result with
`atmos_tools.py --mode decode`: object ID sets, metadata event counts, CAF
channels, and binaural waveform correlation. See
[`interop.md`](interop.md#gst-plugin-oamd-validation) for measured results.

The Dolby script checks:

| Check | Result |
|---|---|
| `.atmos`, `.metadata`, `.caf`, `.pcm` produced | pass |
| manifest references the correct metadata/audio files | pass |
| `.pcm` vs direct `dlbac3dec` S24LE decode | byte-identical |
| `.metadata` vs direct `oamdserialize` | byte-identical |
| `.caf` decoded back to S24LE vs `.pcm` | byte-identical |
| binaural mode (`dlbdapvr`) | pass (2 ch, 48 kHz) |
| Cavern read-back | the DAMF parses and renders |

The same checks pass for a TrueHD master, including byte-identical PCM and
metadata. For the Dolby chain, the DAMF export is therefore a lossless
representation of the decoded object stream for both codecs. For the salsa
chain, the PCM/CAF layout and manifest match, and the metadata is the adapter's
OAMD encoding (see [Differences from the Dolby chain](#differences-from-the-dolby-chain)).

## Sidecar naming

`atmos_tools.py` writes and references `<stem>.metadata` and `<stem>.caf`:

```yaml
audio: stream.caf
metadata: stream.metadata
```

Cavern's DAMF reader resolves sidecars by the standard names
`<manifest>.atmos.audio` and `<manifest>.atmos.metadata` in the same directory
rather than through the manifest fields. The verification script creates
aliases:

```sh
ln -s stream.caf        stream.atmos.audio
ln -s stream.metadata   stream.atmos.metadata
```

This is a reader convention, not a corrupt manifest: the manifest is valid and
the aliases point at the same files. The script never modifies
`gst-plugin-oamd`.

Cavern is used only to confirm that a third-party tool can parse the DAMF. It is
not a bit-match renderer for this export; losslessness is established by the
PCM/metadata/CAF comparisons above.

## Supported scope

- E-AC-3 and TrueHD raw streams to DAMF (`.atmos`, `.metadata`, `.caf`, `.pcm`);
- Dolby Reference Player decode and salsa decode + `salsaobjectaudioadapt`;
- binaural mode through the external tool (`oamdcapsfeature float=true` +
  `dlbdapvr`);
- `oamdserialize` output, which is also embedded in the DAMF export.