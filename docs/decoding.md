# Decoding

Both supported codecs produce the same `SpatialFrame` / `SpatialPayload` IR, so
the renderers do not need codec-specific paths or OAMD knowledge.

```text
E-AC-3 JOC:  bytes → Eac3JocParser → Eac3JocFrame → Eac3JocDecoder ─┐
                                                                    ├─► SpatialFrame
TrueHD:      bytes → TrueHdParser   → TrueHdFrame  → TrueHdDecoder  ─┘
```

## E-AC-3 JOC parse stage

`eac3joc` parses the transport stream into audio frames:

| Module | Role |
|---|---|
| `transport` | syncframe framing, AC-3/E-AC-3 headers, stream grouping |
| `emdf` | EMDF containers, ID11/ID14 payload extraction |
| `joc` | ID14 metadata, dense/sparse Huffman matrices, dequantization |
| `bed` | core lanes, dependent channel map, ADM bed slots |
| `frame` | groups syncframes by `(codec, strmtyp, substreamid)` |

`Eac3JocParser::push_bytes` returns every completed audio frame. A frame is
complete when the next syncframe repeats a stream key, so output lags the input
by one audio frame; `flush()` releases the last one. The parse stage never calls
libavcodec and never touches QMF or matrix state.

The ID11 payload is parsed by `objectaudio-oamd` into the object-only IR.
`Eac3JocFrame` keeps the raw bytes for diagnostics and the EMDF sample offset;
the GStreamer decoder attaches the resulting `SpatialPayload`, never OAMD
bytes.

## E-AC-3 decode stage

- `eac3joc-core-decoder` opens libavcodec's `eac3` decoder with `drc_scale=0`
  and `target_level=0`, sends each syncframe of an audio frame as a packet, and
  returns planar `f32` core PCM.
- `eac3joc-dsp::JocKernel` performs 64-band analysis, parameter-band matrix
  interpolation, synthesis, the 21-tap surround DC compensation and the
  1217-sample LFE delay. The transforms use `rustfft`.
- `Eac3JocDecoder` maps the core lanes through the bed plan, runs the kernel and
  returns an object-only `SpatialFrame` (`ch0` = LFE, following channels are the
  stream's objects).

The kernel state is heap-allocated so the streaming path fits GStreamer's
task-thread stack. A flushing seek resets the libavcodec core, the JOC kernel
and the parser, while keeping the negotiated JOC configuration so a frame
without an ID14 payload can still be mapped to the existing bed. The codec
delay (1473 samples) is not removed on seek; the first frames after a seek can
therefore carry the usual decoder latency/transient.

## `salsaeac3jocdec`

`salsaeac3jocdec` is a `GstAudioDecoder`:

- `set_format` creates the parser and decoder. The decoder is built on a
  dedicated large-stack worker thread and boxed.
- On the first decoded frame it sets `LFE + N` output caps, where `N` is the
  JOC-declared object count.
- When the downstream peer accepts `meta:ObjectAudioMeta`, each output buffer
  carries an `ObjectAudioMeta` with the versioned `SpatialPayload` (objects and
  in-frame events), the payload snapshot offset and `bpf = 4 * channels`.
  Otherwise plain `audio/x-raw` is negotiated without metadata.
- EOS flushes the parser, decodes the last frame and forwards the event.
  `flush()` and `stop()` reset the parser and kernel state.

## TrueHD

`truehd-atmos` wraps the pure-Rust `truehd` crate:

- `TrueHdParser` accumulates complete access units and extracts raw OAMD
  payloads (`evo_payload_id == 11`) with their sample offsets.
- `TrueHdDecoder` decodes presentation 3, maps the LFE and object channels into
  the IR, rebases objects to contiguous IDs and shifts events by the access
  unit's OAMD sample offset.
- Presentation bed channels are expanded directly into fixed-position objects
  while the frame is built; the IR has no bed field. It also exposes a
  `SpatialSnapshot` (`SpatialPayload` + offset) for `ObjectAudioMeta`.
- PCM is 24-bit signed and is converted with `sample / 2^23`.

DRC and dialnorm are not applied; the decoder returns the presentation samples.

`salsatruehdparse` wraps `truehd::Extractor` in a `GstBaseParse`. It emits one
access unit per buffer with `framed=true, alignment=frame`, sets `rate` and the
frame rate from the major sync, and sets `atmos=true` when an access unit
carries OAMD. It forwards the original access-unit bytes and never serializes
`truehd::AccessUnit`.

`salsatruehdatmosdec` consumes the framed `audio/x-true-hd` output of
`salsatruehdparse` (`framed=true, alignment=frame`, one access unit per buffer)
and rejects unframed streams. The element accumulates 40-sample access units
into the renderer's 32-sample grid and emits a buffer either at a 1216-sample
boundary or immediately after an OAMD update. It attaches the
`SpatialSnapshot` with events shifted to the output buffer timeline, so each
buffer carries at most one snapshot at its exact offset.

Both elements handle flushing seeks. `salsatruehdparse` detects a discontinuity
from the BaseParse stream offset, drops the extractor/access-unit parser state
and resynchronises on the next major sync. The decoder worker resets on
`FLUSH_STOP`/`DISCONT`; because a TrueHD major sync is only guaranteed every 32
(FBB) or 128 (FBA) access units, seek precision is bounded by that interval.
The first buffers after a seek may not carry OAMD, in which case the renderers
keep their default state until the next snapshot arrives.

## Dynamic object count

Object counts come from the negotiated caps or the decoded stream; no fixed
15/16-object limit remains.

- `Eac3JocParser` reports the JOC object count, and `salsaeac3jocdec` sets
  output caps from it on the first frame. A mid-stream count change is rejected.
- `salsaoar` and `salsahrtfrender` advertise
  `channels=(int)[1, 1024]`, `channel-mask=0`, and derive
  `object_count = channels - 1`.
- `salsatruehdatmosdec` trims to the OAMD-declared object count.
- `salsaobjectaudioadapt` matches the Dolby renderer's 32-channel ceiling and
  rejects wider streams during negotiation.

The speaker and binaural renderers allocate per-object state from the negotiated
count. `SpeakerMetadata` borrows position slices, and the position timeline
fills a caller-owned buffer, so large event counts do not allocate per query.
