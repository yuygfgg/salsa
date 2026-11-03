# OAMD field coverage

Salsa uses its fork of [`truehdd/oamd`](https://github.com/truehdd/oamd) 0.1.0
(Apache-2.0) for the bit-level payload parse, merged into `objectaudio-oamd` as
the internal `payload`/`bitstream` modules. `objectaudio-oamd` owns both the
parser and the writer and maps the parse into the salsa IR. Codec decoders and
`salsaobjectaudioadapt` call it; no crate above it imports `oamd`.

## Covered

| OAMD field / element | salsa mapping |
|---|---|
| `oamd_version` | `OamdFrame::version`; only version 0 is accepted by the parser |
| `object_count` | `OamdFrame::object_count`, default limit 159 |
| `program_assignment` bed/ISF/dynamic counts | `OamdProgram` |
| `bed_assignment` | `BedLayout` mask/channels/name |
| object element `md_update_info` | `sample_offset`, `ramp_samples`, per-block `ObjectEvent` offsets and ramps |
| object element blocks | one `OamdObject` per slot; later blocks become `ObjectEvent` |
| `object_gain` | `gain_db` and linear `gain` (`-inf` becomes `None` / `0.0`) |
| `object_priority` | `OamdObject::priority` |
| object position (6/6/4 bit and differential) | ADM normalized position in `OamdObject::position` / `ObjectState::position` |
| extended-precision position element | folded into the position by `ObjectAudioMetadataPayload::get_damf_pos` |
| `distance_factor`, `object_size`, `b_object_snap` | `OamdObject` |
| `b_object_in_bed_or_isf` | `OamdObject::in_bed_or_isf`; dynamic objects feed `object_states()` |
| trim element | `ProgramCalibration` on `SpatialPayload` / `SpatialFrame`: warp mode, nine records, per-object bypass rebased to IR indices |

## Gaps and deliberate differences

- **Content flags are inferred.** `oamd` does not expose `content_description` or
  the raw `b_dyn_object_only_program` bit. `OamdProgram::{content_bed,content_isf,
  content_dynamic,dynamic_object_only}` are reconstructed from the counts and the
  bed assignments.
- **Standard vs non-standard bed assignment is lost.** The upstream parser expands
  both into a 17-label boolean array. `BedLayout::complete` is therefore based on
  whether the expanded mask matches a named layout, not on the original
  `b_standard_chan_assign` bit.
- **Positions are floats, not Q15.** The adapter keeps the normalized `f32` from
  the parser. The renderers derive Q15 coordinates where they need them.
- **Headphone, object-description and bed-object elements are parsed but not
  mapped.** They are not in the streaming path. The trim element now maps to
  `ProgramCalibration`; the renderer selects the record for its target layout.
- **Trailing padding is not validated as zero.** The upstream parser skips element
  padding but does not enforce that the remaining payload bits are zero.
- **Duplicate object elements are rejected.** The upstream parser keeps the last
  one; the adapter returns `OamdError::MultipleObjectElements` so a decoder never
  silently loses an element.
- **Multiple position blocks are supported** as `ObjectEvent`s.
- **The object limit is adjustable.** The default is the library maximum (159);
  the JOC path can pass a smaller `OamdLimits`.

## Writer coverage

`objectaudio-oamd::encode` writes OAMD version 0 as a dynamic-object-only
program with one LFE channel and one object element:

- object count (5 bits, or 31 + 7 bits above 31);
- `b_dyn_object_only_program` + `b_lfe_present`;
- one object element with 1–8 blocks, 32-sample block offsets, and a ramp
  duration selected from the standard codes (otherwise 11-bit raw);
- object gain (integer dB, or -inf), priority, 6/6/4-bit absolute position,
  zone/elevation, uniform object size, snap, and active flag;
- a trim element when `SpatialPayload::calibration` is present, including the
  nine records and per-object bypass.

Non-LFE beds are not rebuilt: the adapter intentionally emits every object as
dynamic. The writer is the inverse of the subset the parser accepts; positions
quantize to 1/62 in x/y and 1/15 in z, and gains to integer dB values.

## Verification

The OAMD tests cover the upstream example payload, non-zero versions, empty and
truncated payloads, an explicit object limit, every prefix of the example payload,
arbitrary byte vectors (proptest), and `SpatialPayload -> encode -> parse`
round-trips (including events and inactive objects). They do not require
GStreamer.

The differences above are intentional for a codec-independent IR. A consumer that
needs a more constrained frame model can quantize positions and enforce a smaller
object limit before handing the frame to a renderer.
