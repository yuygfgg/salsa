# Dolby interoperability

Salsa exchanges object audio with the Dolby Reference Player through
`salsaobjectaudioadapt`. The core decoders, sources and renderers use only the
salsa `SpatialPayload`; the adapter is the single OAMD boundary.

```text
salsa -> Dolby:  salsaeac3jocdec ! salsaobjectaudioadapt direction=to-dolby ! dlboar
Dolby -> salsa:  dlbac3dec ! salsaobjectaudioadapt direction=to-salsa ! salsaoar
```

`direction=auto` (the default) selects `to-salsa` when the negotiated sink caps
carry `meta:DlbObjectAudioMeta`, and `to-dolby` otherwise.

## Caps features

Salsa and Dolby spell the same object-audio caps differently:

```text
salsa:  audio/x-raw(memory:SystemMemory, meta:ObjectAudioMeta)
Dolby:  audio/x-raw(meta:DlbObjectAudioMeta)
```

Core renderer sinks advertise only the salsa feature. A Dolby stream must be
converted by the adapter first:

```sh
# correct
filesrc location=stream.ec3 ! dlbac3parse enable-metadata=true ! \
  dlbac3dec out-ch-config=21 ! salsaobjectaudioadapt ! salsaoar layout=5.1 ! ...

# rejected: no adapter
filesrc location=stream.ec3 ! dlbac3parse enable-metadata=true ! \
  dlbac3dec out-ch-config=21 ! salsaoar layout=5.1 ! ...
```

The second pipeline fails caps negotiation. Insert `salsaobjectaudioadapt`;
the renderer will then receive `ObjectAudioMeta` / `SpatialPayload`.

## `salsaobjectaudioadapt`

`crates/gst-plugins/gst-objectaudio/src/adapt.rs` is an always-in-place
`GstBaseTransform`.

| Property | Default | Meaning |
|---|---|---|
| `direction` | `auto` | `auto`, `to-dolby`, or `to-salsa` |
| `remove` | `false` | In to-dolby mode, do not attach `DlbObjectAudioMeta` |
| `float` | `false` | Convert interleaved S32LE samples to F32LE |

### to-dolby

- Reads every `SpatialPayload` from `ObjectAudioMeta`.
- Encodes it as OAMD ID11 with `objectaudio-oamd::encode`. Every non-LFE object
  is written as a dynamic object; beds are not rebuilt.
- Attaches `DlbObjectAudioMeta` with the same sample offset and `bpf`.
- Positions are quantized to 6-bit x/y and 4-bit z; gains to integer dB.

### to-salsa

- Reads every `DlbObjectAudioMeta` OAMD payload.
- Parses it with `objectaudio-oamd`, rebases dynamic objects to contiguous IDs,
  and appends non-LFE bed channels as fixed-position objects.
- Replaces the salsa metadata with a fresh `ObjectAudioMeta` / `SpatialPayload`.

The Dolby meta type is looked up at runtime; this crate never registers it. If
no Dolby plugin is loaded, to-dolby returns a negotiation error instead of
creating a conflicting GType.

## Verification

`tools/test-dolby-interop.sh` runs the matrix on the x86_64 Reference Player
build. The OAMD dump is produced through the adapter, so `oamdserialize` sees
the OAMD the adapter actually emits. Because the adapter encodes the IR as a
dynamic-object-only payload, byte equality with Dolby's own OAMD is not
expected; the script reports the difference for inspection. Round-trip and
quantization limits are covered by `objectaudio-oamd` unit tests.

## `gst-plugin-oamd` validation

`tools/test-atmos-tools-adapter.sh` follows the decode/binaural layout of
`gst-plugin-oamd/atmos_tools.py`, replacing the Dolby decoder with a salsa
decoder plus `salsaobjectaudioadapt`. It verifies that the external
`oamdcapsfeature`, `oamdserialize` and `dlbdapvr` elements consume the salsa
OAMD bridge correctly.

Measured on this machine (x86_64 Reference Player runtime):

| Stream | Metadata IDs | Events (ours / Dolby) | `.atmos` | Binaural corr | Gain-normalized SNR |
|---|---|---:|---|---:|---:|
| E-AC-3 (`mlp_14_...ec3`, 14 objects) | identical (LFE 3 + 10-23) | 8565 / 8565 | identical | 0.99991 | 37.5 dB |
| TrueHD (`16ch.mlp`, 15 objects) | identical (LFE 3 + 10-24) | 6719 / 6735 | identical | 0.99973 | 32.7 dB |

The TrueHD gain difference is the known -11 dB truehd/dialnorm level
difference; the E-AC-3 gain difference is the Dolby binaural renderer's
level/trim, not a waveform mismatch. Run the script with the stream path to
reproduce.
